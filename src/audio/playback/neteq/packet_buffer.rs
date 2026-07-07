//! A port of WebRTC's `modules/audio_coding/neteq/packet_buffer.cc`.
//!
//! The buffer is a list kept sorted at all times so the next packet to decode is
//! at the front. Insertion is wrap-around aware and resolves same-timestamp
//! collisions by [`Priority`](super::packet::Priority): a higher-priority payload
//! (primary over FEC over DRED) wins, a lower-priority duplicate is discarded.
//!
//! The smart-flushing field trial and the `DecoderDatabase`/`StatisticsCalculator`
//! collaborators are not ported; Chatt is a single Opus stream, so overflow uses
//! the plain full-flush path and discard counts live on the buffer itself.

use std::collections::VecDeque;

use super::packet::{Packet, is_obsolete_timestamp};
use super::tick_timer::TickTimer;

/// Sample rate of the live audio path, in samples per millisecond.
const SAMPLES_PER_MS: u64 = 48;

/// Outcome of [`PacketBuffer::insert_packet`], mirroring the WebRTC return codes
/// the live path acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertOutcome {
    Ok,
    Flushed,
}

/// Retired packets awaiting destruction off the audio callback.
///
/// Dropping a [`Packet`] can free its payload `Arc<Vec<u8>>`, which must not
/// happen on the realtime thread. The buffer parks retired packets here and the
/// decode worker swaps them out under the stream mutex it already holds, so the
/// frees run on the worker. When full, the push drops inline and counts the
/// overflow instead of blocking or growing.
#[derive(Debug)]
pub(crate) struct PacketTrash {
    packets: Vec<Packet>,
    #[cfg(test)]
    overflow: u64,
}

/// Retired-packet capacity: the 200-packet production buffer plus a margin for
/// packets extracted between worker drains.
pub(crate) const PACKET_TRASH_CAPACITY: usize = 256;

impl PacketTrash {
    fn new() -> Self {
        Self {
            packets: Vec::with_capacity(PACKET_TRASH_CAPACITY),
            #[cfg(test)]
            overflow: 0,
        }
    }

    fn push(&mut self, packet: Packet) {
        if self.packets.len() == self.packets.capacity() {
            #[cfg(test)]
            {
                self.overflow = self.overflow.saturating_add(1);
            }
            return;
        }
        self.packets.push(packet);
    }
}

/// Holds packets before decoding, ordered by (timestamp, sequence, priority).
#[derive(Debug)]
pub(crate) struct PacketBuffer {
    buffer: VecDeque<Packet>,
    max_number_of_packets: usize,
    packets_discarded: u64,
    secondary_packets_discarded: u64,
    trash: PacketTrash,
}

impl PacketBuffer {
    pub(crate) fn new(max_number_of_packets: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(max_number_of_packets),
            max_number_of_packets,
            packets_discarded: 0,
            secondary_packets_discarded: 0,
            trash: PacketTrash::new(),
        }
    }

    /// Parks a consumed packet for the worker to destroy off the callback.
    pub(crate) fn retire(&mut self, packet: Packet) {
        self.trash.push(packet);
    }

    /// Swaps the retired packets into `into` (an empty, equally sized vec) so
    /// the worker can drop them on its own thread.
    pub(crate) fn swap_trash(&mut self, into: &mut Vec<Packet>) {
        debug_assert!(into.is_empty());
        std::mem::swap(&mut self.trash.packets, into);
    }

    /// Retired packets dropped inline because the trash was full.
    #[cfg(test)]
    pub(crate) fn trash_overflow(&self) -> u64 {
        self.trash.overflow
    }

    /// Deletes all packets in the buffer.
    pub(crate) fn flush(&mut self) {
        while let Some(packet) = self.buffer.pop_front() {
            log_discard(
                &packet,
                &mut self.packets_discarded,
                &mut self.secondary_packets_discarded,
            );
            self.trash.push(packet);
        }
    }

    pub(crate) fn num_packets(&self) -> usize {
        self.buffer.len()
    }

    /// Inserts `packet`, keeping the list sorted. Faithful translation of
    /// `PacketBuffer::InsertPacket`: on overflow the whole buffer is flushed
    /// first; same-timestamp collisions keep the higher-priority payload.
    pub(crate) fn insert_packet(
        &mut self,
        mut packet: Packet,
        tick_timer: &TickTimer,
    ) -> InsertOutcome {
        packet.waiting_time = Some(tick_timer.new_stopwatch());

        let mut outcome = InsertOutcome::Ok;
        if self.buffer.len() >= self.max_number_of_packets {
            self.flush();
            outcome = InsertOutcome::Flushed;
        }

        // Scan from the back for the last existing packet that the new packet is
        // not earlier than (WebRTC `NewTimestampIsLarger`, i.e. `new >= p`).
        let mut found_r = None;
        for idx in (0..self.buffer.len()).rev() {
            if !packet.is_earlier_than(&self.buffer[idx]) {
                found_r = Some(idx);
                break;
            }
        }

        let insert_idx = match found_r {
            Some(r) => {
                // `r` has higher-or-equal priority at the same timestamp: drop the
                // new (lower or duplicate) payload.
                if packet.timestamp == self.buffer[r].timestamp {
                    log_discard(
                        &packet,
                        &mut self.packets_discarded,
                        &mut self.secondary_packets_discarded,
                    );
                    self.trash.push(packet);
                    return outcome;
                }
                r + 1
            }
            None => 0,
        };

        // The element now at `insert_idx` is strictly later than the new packet.
        // If it shares the timestamp it is a lower-priority payload — replace it.
        if insert_idx < self.buffer.len() && packet.timestamp == self.buffer[insert_idx].timestamp {
            let replaced = self.buffer.remove(insert_idx).expect("index in range");
            log_discard(
                &replaced,
                &mut self.packets_discarded,
                &mut self.secondary_packets_discarded,
            );
            self.trash.push(replaced);
        }

        self.buffer.insert(insert_idx, packet);
        outcome
    }

    /// Timestamp of the first packet, if any.
    pub(crate) fn next_timestamp(&self) -> Option<u32> {
        self.buffer.front().map(|p| p.timestamp)
    }

    /// Highest timestamp strictly below `timestamp`, used to size the DRED gap on
    /// insertion (the end of the most recent buffered audio before `timestamp`).
    pub(crate) fn next_lower_timestamp(&self, timestamp: u32) -> Option<u32> {
        // `insert_packet` keeps the buffer in ascending timestamp order, so the
        // timestamps strictly below `timestamp` form a prefix and the highest is
        // the last of them. Scanning from the back and returning the first
        // strictly-lower timestamp yields that maximum in O(1) for the common
        // in-order insert, versus scanning the whole buffer.
        self.buffer
            .iter()
            .rev()
            .map(|p| p.timestamp)
            .find(|&ts| ts < timestamp)
    }

    pub(crate) fn peek_next_packet(&self) -> Option<&Packet> {
        self.buffer.front()
    }

    /// True when a DRED packet is close enough to the front that the next few
    /// `GetAudio` pulls may have to materialize it.
    pub(crate) fn dred_within_front_span(&self, span_samples: usize) -> bool {
        let mut preceding_samples = 0usize;
        for packet in &self.buffer {
            if packet.priority.codec_level == 2 {
                return preceding_samples <= span_samples;
            }
            preceding_samples = preceding_samples.saturating_add(packet.duration_samples);
            if preceding_samples > span_samples {
                return false;
            }
        }
        false
    }

    pub(crate) fn get_next_packet(&mut self) -> Option<Packet> {
        self.buffer.pop_front()
    }

    /// Discards front packets strictly older than `timestamp_limit` but within
    /// `horizon_samples`. Port of `DiscardOldPackets`, which stops at the first
    /// non-obsolete packet: the buffer is sorted, so obsolete packets form a
    /// front prefix except for entries older than the horizon window.
    pub(crate) fn discard_old_packets(&mut self, timestamp_limit: u32, horizon_samples: u32) {
        while let Some(front) = self.buffer.front() {
            if front.timestamp == timestamp_limit
                || !is_obsolete_timestamp(front.timestamp, timestamp_limit, horizon_samples)
            {
                break;
            }
            let packet = self.buffer.pop_front().expect("front exists");
            log_discard(
                &packet,
                &mut self.packets_discarded,
                &mut self.secondary_packets_discarded,
            );
            self.trash.push(packet);
        }
    }

    /// Total duration in samples the buffered packets span across. Port of
    /// `GetSpanSamples`: timestamp range plus the trailing duration (or the
    /// back packet's waiting time when `count_waiting_time`).
    pub(crate) fn span_samples(&self, count_waiting_time: bool, tick_timer: &TickTimer) -> usize {
        let (Some(front), Some(back)) = (self.buffer.front(), self.buffer.back()) else {
            return 0;
        };
        let mut span = back.timestamp.wrapping_sub(front.timestamp) as usize;
        if count_waiting_time {
            let waiting_ms = back
                .waiting_time
                .map_or(0, |watch| watch.elapsed_ms(tick_timer));
            span += (waiting_ms * SAMPLES_PER_MS) as usize;
        } else {
            span += back.duration_samples;
        }
        span
    }

    pub(crate) fn packets_discarded(&self) -> u64 {
        self.packets_discarded
    }

    pub(crate) fn secondary_packets_discarded(&self) -> u64 {
        self.secondary_packets_discarded
    }

    #[cfg(test)]
    pub(crate) fn timestamps(&self) -> Vec<u32> {
        self.buffer.iter().map(|p| p.timestamp).collect()
    }
}

fn log_discard(packet: &Packet, primary: &mut u64, secondary: &mut u64) {
    if packet.priority.codec_level > 0 {
        *secondary += 1;
    } else {
        *primary += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::audio::playback::neteq::packet::{Packet, PacketPayload, Priority};

    fn opus(timestamp: u32, sequence: u32, priority: Priority) -> Packet {
        Packet::new(
            timestamp,
            sequence,
            priority,
            960,
            PacketPayload::Opus(Arc::new(vec![1, 2, 3, 4])),
        )
    }

    #[test]
    fn insert_keeps_buffer_sorted_by_timestamp() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(1920, 12, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 11, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(2880, 13, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(0, 10, Priority::PRIMARY), &timer);
        assert_eq!(buffer.timestamps(), vec![0, 960, 1920, 2880]);
    }

    #[test]
    fn higher_priority_replaces_lower_at_same_timestamp() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        // A DRED chunk arrives first, then the primary for the same timestamp.
        buffer.insert_packet(opus(960, 11, Priority::new(2, 0)), &timer);
        buffer.insert_packet(opus(960, 11, Priority::PRIMARY), &timer);
        assert_eq!(buffer.num_packets(), 1);
        assert_eq!(
            buffer.peek_next_packet().unwrap().priority,
            Priority::PRIMARY
        );
        assert_eq!(buffer.secondary_packets_discarded(), 1);
    }

    #[test]
    fn lower_priority_at_existing_timestamp_is_discarded() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(960, 11, Priority::PRIMARY), &timer);
        // A late DRED chunk for an already-present primary is dropped.
        buffer.insert_packet(opus(960, 11, Priority::new(2, 0)), &timer);
        assert_eq!(buffer.num_packets(), 1);
        assert_eq!(
            buffer.peek_next_packet().unwrap().priority,
            Priority::PRIMARY
        );
        assert_eq!(buffer.secondary_packets_discarded(), 1);
    }

    #[test]
    fn overflow_flushes_whole_buffer() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(2);
        buffer.insert_packet(opus(0, 0, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 1, Priority::PRIMARY), &timer);
        let outcome = buffer.insert_packet(opus(1920, 2, Priority::PRIMARY), &timer);
        assert_eq!(outcome, InsertOutcome::Flushed);
        assert_eq!(buffer.timestamps(), vec![1920]);
    }

    #[test]
    fn span_samples_covers_range_plus_trailing_duration() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(0, 0, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 1, Priority::PRIMARY), &timer);
        // range 960 + trailing 20 ms frame (960) = 1920.
        assert_eq!(buffer.span_samples(false, &timer), 1920);
    }

    #[test]
    fn discard_old_packets_drops_obsolete_front() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(0, 0, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 1, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(1920, 2, Priority::PRIMARY), &timer);
        buffer.discard_old_packets(1920, 0);
        assert_eq!(buffer.timestamps(), vec![1920]);
        assert_eq!(buffer.packets_discarded(), 2);
    }

    #[test]
    fn discarded_packets_park_in_trash_until_swapped() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(0, 0, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 1, Priority::PRIMARY), &timer);
        buffer.discard_old_packets(1920, 0);
        buffer.flush();

        let mut swapped = Vec::with_capacity(PACKET_TRASH_CAPACITY);
        buffer.swap_trash(&mut swapped);
        assert_eq!(swapped.len(), 2);
        assert_eq!(buffer.trash_overflow(), 0);

        let mut empty = Vec::with_capacity(PACKET_TRASH_CAPACITY);
        buffer.swap_trash(&mut empty);
        assert!(empty.is_empty(), "trash drained by the previous swap");
    }

    #[test]
    fn full_trash_drops_inline_and_counts_overflow() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(PACKET_TRASH_CAPACITY + 8);
        for index in 0..PACKET_TRASH_CAPACITY as u32 + 2 {
            buffer.insert_packet(opus(index * 960, index, Priority::PRIMARY), &timer);
        }
        buffer.flush();
        assert_eq!(buffer.trash_overflow(), 2);
    }

    #[test]
    fn next_lower_timestamp_finds_preceding_audio_end() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(0, 0, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 1, Priority::PRIMARY), &timer);
        assert_eq!(buffer.next_lower_timestamp(2880), Some(960));
        assert_eq!(buffer.next_lower_timestamp(0), None);
    }

    #[test]
    fn dred_within_front_span_finds_near_recovery_packet() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(0, 0, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 1, Priority::new(2, 0)), &timer);
        assert!(buffer.dred_within_front_span(960));
        assert!(!buffer.dred_within_front_span(480));
    }
}
