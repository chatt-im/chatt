//! A port of WebRTC's `modules/audio_coding/neteq/packet.h`.
//!
//! A [`Packet`] holds one decodable unit before it reaches the decoder, keyed by
//! media timestamp and ranked by [`Priority`]. The redundancy levels mirror
//! WebRTC's Opus mapping: a primary payload is `{0, 0}`, in-band FEC is `{1, _}`,
//! and DRED recovered chunks are `{2, _}`. The buffer keeps packets ordered so
//! the next unit to decode is always at the front.
//!
//! Chatt differs from WebRTC in two deliberate ways: the sequence number is a
//! `u32` (Chatt's wire sequence) rather than a 16-bit RTP value, and the
//! payloads share the originating datagram bytes through an [`Rc`] so derived
//! FEC/DRED entries do not copy the encoded data.

use std::rc::Rc;

use super::tick_timer::Stopwatch;

/// Redundancy ranking. Sorted low-to-high: a lower value is a *higher* priority
/// payload that should be preferred for the same timestamp. `{0, 0}` is the
/// primary; FEC is `{1, _}`; DRED is `{2, _}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Priority {
    pub codec_level: i32,
    pub red_level: i32,
}

impl Priority {
    pub(crate) const PRIMARY: Priority = Priority {
        codec_level: 0,
        red_level: 0,
    };

    pub(crate) fn new(codec_level: i32, red_level: i32) -> Self {
        debug_assert!(codec_level >= 0 && red_level >= 0);
        Self {
            codec_level,
            red_level,
        }
    }
}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Mirrors Packet::Priority::operator<: compare codec_level first, then
        // red_level, both ascending (lower = higher priority).
        self.codec_level
            .cmp(&other.codec_level)
            .then(self.red_level.cmp(&other.red_level))
    }
}

/// The decodable content of a [`Packet`]. FEC and DRED reference the bounding
/// datagram's raw bytes so the actual Opus/DRED decode can run later in the
/// `GetAudio` loop without re-buffering the encoded data.
#[derive(Clone, Debug)]
pub(crate) enum PacketPayload {
    /// Primary Opus payload.
    Opus(Rc<Vec<u8>>),
    /// In-band FEC (LBRR): decode these bytes in FEC mode to recover the frame
    /// one step before the carrying packet.
    OpusFec(Rc<Vec<u8>>),
    /// A DRED recovered chunk: the bounding datagram bytes plus the DRED offset
    /// (in samples before the bounding packet's primary audio) to decode at.
    Dred { source: Rc<Vec<u8>>, offset: i32 },
}

/// One decodable unit before the decoder. Comparison establishes ordering by
/// (1) timestamp, (2) sequence number and (3) priority, all wrap-around aware.
#[derive(Clone, Debug)]
pub(crate) struct Packet {
    pub timestamp: u32,
    pub sequence_number: u32,
    pub priority: Priority,
    /// Decoded duration this packet spans, in 48 kHz samples (20 ms primary,
    /// 10 ms DRED chunk). Used for packet-buffer span accounting.
    pub duration_samples: usize,
    pub payload: PacketPayload,
    /// Set by the buffer on insert; measures how long the packet has waited.
    pub(super) waiting_time: Option<Stopwatch>,
}

impl Packet {
    pub(crate) fn new(
        timestamp: u32,
        sequence_number: u32,
        priority: Priority,
        duration_samples: usize,
        payload: PacketPayload,
    ) -> Self {
        Self {
            timestamp,
            sequence_number,
            priority,
            duration_samples,
            payload,
            waiting_time: None,
        }
    }

    /// True if `self` orders strictly before `rhs` — WebRTC `operator<`.
    pub(crate) fn is_earlier_than(&self, rhs: &Packet) -> bool {
        if self.timestamp == rhs.timestamp {
            if self.sequence_number == rhs.sequence_number {
                // Identical timestamp and sequence: higher priority sorts first.
                return self.priority < rhs.priority;
            }
            return rhs.sequence_number.wrapping_sub(self.sequence_number) < 0xFFFF_FFFF / 2;
        }
        rhs.timestamp.wrapping_sub(self.timestamp) < 0xFFFF_FFFF / 2
    }

    /// True if `self` and `rhs` denote the same (timestamp, sequence, priority).
    pub(crate) fn same_slot(&self, rhs: &Packet) -> bool {
        self.timestamp == rhs.timestamp
            && self.sequence_number == rhs.sequence_number
            && self.priority == rhs.priority
    }
}

/// True if `timestamp` is older than `timestamp_limit` but less than
/// `horizon_samples` behind it. `horizon_samples == 0` means half the 32-bit
/// range. Port of `PacketBuffer::IsObsoleteTimestamp`.
pub(crate) fn is_obsolete_timestamp(
    timestamp: u32,
    timestamp_limit: u32,
    horizon_samples: u32,
) -> bool {
    is_newer_timestamp(timestamp_limit, timestamp)
        && (horizon_samples == 0
            || is_newer_timestamp(timestamp, timestamp_limit.wrapping_sub(horizon_samples)))
}

/// Wrap-around aware "is `timestamp` newer than `prev_timestamp`". Port of
/// `IsNewerTimestamp` from `module_common_types_public.h`.
pub(crate) fn is_newer_timestamp(timestamp: u32, prev_timestamp: u32) -> bool {
    timestamp != prev_timestamp
        && timestamp.wrapping_sub(prev_timestamp) < 0x8000_0000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus_packet(timestamp: u32, sequence: u32, priority: Priority) -> Packet {
        Packet::new(
            timestamp,
            sequence,
            priority,
            960,
            PacketPayload::Opus(Rc::new(vec![0u8; 4])),
        )
    }

    #[test]
    fn priority_orders_primary_before_fec_before_dred() {
        assert!(Priority::PRIMARY < Priority::new(1, 0));
        assert!(Priority::new(1, 0) < Priority::new(2, 0));
        assert!(Priority::new(0, 0) < Priority::new(0, 1));
    }

    #[test]
    fn earlier_timestamp_sorts_first() {
        let a = opus_packet(1000, 10, Priority::PRIMARY);
        let b = opus_packet(1960, 11, Priority::PRIMARY);
        assert!(a.is_earlier_than(&b));
        assert!(!b.is_earlier_than(&a));
    }

    #[test]
    fn same_timestamp_higher_priority_sorts_first() {
        let primary = opus_packet(1000, 10, Priority::PRIMARY);
        let dred = opus_packet(1000, 10, Priority::new(2, 0));
        assert!(primary.is_earlier_than(&dred));
        assert!(!dred.is_earlier_than(&primary));
    }

    #[test]
    fn timestamp_comparison_handles_wraparound() {
        let late = opus_packet(u32::MAX - 480, 5, Priority::PRIMARY);
        let wrapped = opus_packet(480, 6, Priority::PRIMARY);
        assert!(late.is_earlier_than(&wrapped));
        assert!(!wrapped.is_earlier_than(&late));
    }

    #[test]
    fn obsolete_timestamp_matches_horizon_window() {
        assert!(is_obsolete_timestamp(90, 100, 20));
        assert!(!is_obsolete_timestamp(85, 100, 10));
        assert!(!is_obsolete_timestamp(100, 100, 10));
        assert!(is_obsolete_timestamp(50, 100, 0));
    }
}
