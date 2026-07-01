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

/// Holds packets before decoding, ordered by (timestamp, sequence, priority).
#[derive(Debug)]
pub(crate) struct PacketBuffer {
    buffer: VecDeque<Packet>,
    max_number_of_packets: usize,
    packets_discarded: u64,
    secondary_packets_discarded: u64,
}

impl PacketBuffer {
    pub(crate) fn new(max_number_of_packets: usize) -> Self {
        Self {
            buffer: VecDeque::new(),
            max_number_of_packets,
            packets_discarded: 0,
            secondary_packets_discarded: 0,
        }
    }

    /// Deletes all packets in the buffer.
    pub(crate) fn flush(&mut self) {
        for packet in &self.buffer {
            log_discard(
                packet,
                &mut self.packets_discarded,
                &mut self.secondary_packets_discarded,
            );
        }
        self.buffer.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buffer.is_empty()
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
        }

        self.buffer.insert(insert_idx, packet);
        outcome
    }

    /// Timestamp of the first packet, if any.
    pub(crate) fn next_timestamp(&self) -> Option<u32> {
        self.buffer.front().map(|p| p.timestamp)
    }

    /// First timestamp at or above `timestamp`. Port of `NextHigherTimestamp`.
    pub(crate) fn next_higher_timestamp(&self, timestamp: u32) -> Option<u32> {
        self.buffer
            .iter()
            .find(|p| p.timestamp >= timestamp)
            .map(|p| p.timestamp)
    }

    /// Highest timestamp strictly below `timestamp`, used to size the DRED gap on
    /// insertion (the end of the most recent buffered audio before `timestamp`).
    pub(crate) fn next_lower_timestamp(&self, timestamp: u32) -> Option<u32> {
        self.buffer
            .iter()
            .map(|p| p.timestamp)
            .filter(|&ts| ts < timestamp)
            .max()
    }

    pub(crate) fn peek_next_packet(&self) -> Option<&Packet> {
        self.buffer.front()
    }

    pub(crate) fn get_next_packet(&mut self) -> Option<Packet> {
        self.buffer.pop_front()
    }

    pub(crate) fn discard_next_packet(&mut self) {
        if let Some(packet) = self.buffer.pop_front() {
            log_discard(
                &packet,
                &mut self.packets_discarded,
                &mut self.secondary_packets_discarded,
            );
        }
    }

    /// Discards packets strictly older than `timestamp_limit` but within
    /// `horizon_samples`. Port of `DiscardOldPackets`.
    pub(crate) fn discard_old_packets(&mut self, timestamp_limit: u32, horizon_samples: u32) {
        let mut kept = VecDeque::with_capacity(self.buffer.len());
        while let Some(packet) = self.buffer.pop_front() {
            if packet.timestamp == timestamp_limit
                || !is_obsolete_timestamp(packet.timestamp, timestamp_limit, horizon_samples)
            {
                kept.push_back(packet);
            } else {
                log_discard(
                    &packet,
                    &mut self.packets_discarded,
                    &mut self.secondary_packets_discarded,
                );
            }
        }
        self.buffer = kept;
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

    /// Number of samples carried by all packets, counting only primaries' worth
    /// of duration (mirrors `NumSamplesInBuffer` for a single codec).
    pub(crate) fn num_samples(&self) -> usize {
        self.buffer.iter().map(|p| p.duration_samples).sum()
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
    use std::rc::Rc;

    use super::*;
    use crate::audio::playback::neteq::packet::{Packet, PacketPayload, Priority};

    fn opus(timestamp: u32, sequence: u32, priority: Priority) -> Packet {
        Packet::new(
            timestamp,
            sequence,
            priority,
            960,
            PacketPayload::Opus(Rc::new(vec![1, 2, 3, 4])),
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
    fn next_lower_timestamp_finds_preceding_audio_end() {
        let timer = TickTimer::new();
        let mut buffer = PacketBuffer::new(50);
        buffer.insert_packet(opus(0, 0, Priority::PRIMARY), &timer);
        buffer.insert_packet(opus(960, 1, Priority::PRIMARY), &timer);
        assert_eq!(buffer.next_lower_timestamp(2880), Some(960));
        assert_eq!(buffer.next_lower_timestamp(0), None);
    }
}
