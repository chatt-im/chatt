//! A port of WebRTC's `modules/audio_coding/neteq/packet_arrival_history.cc`.
//!
//! Records when packets arrived (in media-sample units of a caller-supplied
//! wall clock) keyed by unwrapped RTP timestamp, within a fixed window. The
//! delay of a timestamp is measured against the packet in history that
//! maximizes it, via monotonic min/max deques. The decision logic uses this for
//! the playout-delay estimate and `GetMaxDelayMs`.
//!
//! WebRTC derives arrival times from its tick timer because the audio device
//! calls `GetAudio` at an unwavering 10 ms cadence, making ticks a wall clock.
//! Chatt's playback pulls are ring-scheduled — a muted-suppression drain or
//! refill bunches several `get_audio` calls into one instant — so tick-derived
//! arrival times wobble around wall time and read as phantom jitter. The
//! caller passes real wall-clock sample counts instead.

use std::collections::VecDeque;

/// A forward packet after a long receive-side idle period can belong to a new
/// talkspurt or source instance even if the sender failed to advance RTP time
/// across that idle. Do not let that stale baseline read as queued playout.
const TIMING_DISCONTINUITY_MIN_GAP_MS: i64 = 1_000;
const TIMING_DISCONTINUITY_MIN_EXCESS_MS: i64 = 250;

/// Unwraps wrap-around `u32` RTP timestamps into a monotonic `i64`. Port of
/// `RtpTimestampUnwrapper` (signed-delta form).
#[derive(Debug, Default)]
struct TimestampUnwrapper {
    last_unwrapped: Option<i64>,
}

impl TimestampUnwrapper {
    fn peek_unwrap(&self, value: u32) -> i64 {
        match self.last_unwrapped {
            None => value as i64,
            Some(last) => {
                let delta = value.wrapping_sub(last as u32) as i32;
                last + delta as i64
            }
        }
    }

    fn unwrap(&mut self, value: u32) -> i64 {
        let unwrapped = self.peek_unwrap(value);
        self.last_unwrapped = Some(unwrapped);
        unwrapped
    }

    fn reset(&mut self) {
        self.last_unwrapped = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PacketArrival {
    rtp_timestamp: i64,
    arrival_timestamp: i64,
    length_samples: i64,
}

impl PacketArrival {
    fn relative(&self) -> i64 {
        self.arrival_timestamp - self.rtp_timestamp
    }

    fn contains(&self, other: &PacketArrival) -> bool {
        self.rtp_timestamp <= other.rtp_timestamp
            && self.rtp_timestamp + self.length_samples
                >= other.rtp_timestamp + other.length_samples
    }
}

/// Entry bound for the history and the monotonic deques: the 2 s window at
/// one 10 ms redundancy unit per entry, with slack for reordered stragglers.
/// `PacketArrival` is `Copy`, so preallocated deques make every operation —
/// including the callback-side `reset` — allocation- and free-free.
const HISTORY_CAPACITY: usize = 512;

/// Fixed-window record of packet arrivals for delay estimation.
#[derive(Debug)]
pub(crate) struct PacketArrivalHistory {
    window_size_ms: i64,
    sample_rate_khz: i64,
    unwrapper: TimestampUnwrapper,
    /// Arrivals sorted by unwrapped RTP timestamp (unique), oldest at the
    /// front. Replaces WebRTC's map with a bounded deque so the callback can
    /// reset it without touching the allocator.
    history: VecDeque<PacketArrival>,
    min_arrivals: VecDeque<PacketArrival>,
    max_arrivals: VecDeque<PacketArrival>,
}

impl PacketArrivalHistory {
    pub(crate) fn new(window_size_ms: i32) -> Self {
        Self {
            window_size_ms: window_size_ms as i64,
            sample_rate_khz: 0,
            unwrapper: TimestampUnwrapper::default(),
            history: VecDeque::with_capacity(HISTORY_CAPACITY),
            min_arrivals: VecDeque::with_capacity(HISTORY_CAPACITY),
            max_arrivals: VecDeque::with_capacity(HISTORY_CAPACITY),
        }
    }

    pub(crate) fn set_sample_rate(&mut self, sample_rate: i32) {
        self.sample_rate_khz = (sample_rate / 1000) as i64;
    }

    pub(crate) fn size(&self) -> usize {
        self.history.len()
    }

    /// Drops the current arrival baseline if the next forward packet follows a
    /// large wall-clock receive gap that the media timestamp did not also cover.
    ///
    /// This catches sender implementations that restart a source or miss a
    /// silence marker without advancing RTP time across the quiet period. Genuine
    /// silence-aware streams, where RTP time advances with wall time, keep the
    /// existing history and continue through the usual obsolete-window pruning.
    pub(crate) fn reset_for_timing_discontinuity(
        &mut self,
        rtp_timestamp: u32,
        now_samples: i64,
    ) -> bool {
        if self.sample_rate_khz <= 0 {
            return false;
        }
        let Some(newest) = self.history.back().copied() else {
            return false;
        };
        let unwrapped = self.unwrapper.peek_unwrap(rtp_timestamp);
        if unwrapped <= newest.rtp_timestamp {
            return false;
        }

        let arrival_gap_ms = (now_samples - newest.arrival_timestamp) / self.sample_rate_khz;
        let media_gap_ms = (unwrapped - newest.rtp_timestamp) / self.sample_rate_khz;
        if arrival_gap_ms >= TIMING_DISCONTINUITY_MIN_GAP_MS
            && arrival_gap_ms.saturating_sub(media_gap_ms) >= TIMING_DISCONTINUITY_MIN_EXCESS_MS
        {
            self.reset();
            return true;
        }
        false
    }

    /// Records a packet arrival. Returns false if it is too old or a duplicate.
    /// Port of `PacketArrivalHistory::Insert`.
    pub(crate) fn insert(
        &mut self,
        rtp_timestamp: u32,
        packet_length_samples: i32,
        arrival_timestamp: i64,
    ) -> bool {
        let packet = PacketArrival {
            rtp_timestamp: self.unwrapper.unwrap(rtp_timestamp),
            arrival_timestamp,
            length_samples: packet_length_samples as i64,
        };
        if self.is_obsolete(&packet) || self.contains(&packet) {
            return false;
        }
        // Sorted insert, scanning from the back (arrivals are mostly in
        // order). A same-timestamp entry was already rejected by `contains`
        // only when the earlier packet spans this one, so replace on equality.
        let position = self
            .history
            .iter()
            .rposition(|entry| entry.rtp_timestamp <= packet.rtp_timestamp);
        match position {
            Some(index) if self.history[index].rtp_timestamp == packet.rtp_timestamp => {
                self.history[index] = packet;
            }
            Some(index) => {
                if self.history.len() == HISTORY_CAPACITY {
                    let evicted = self.history.pop_front().expect("history full");
                    if self.min_arrivals.front() == Some(&evicted) {
                        self.min_arrivals.pop_front();
                    }
                    if self.max_arrivals.front() == Some(&evicted) {
                        self.max_arrivals.pop_front();
                    }
                    self.history.insert(index, packet);
                } else {
                    self.history.insert(index + 1, packet);
                }
            }
            None => {
                if self.history.len() < HISTORY_CAPACITY {
                    self.history.push_front(packet);
                }
            }
        }
        let newest = *self.history.back().expect("just inserted");
        if packet != newest {
            // Reordered packet: kept in history but excluded from min/max.
            return true;
        }
        while let Some(&oldest) = self.history.front() {
            if !self.is_obsolete(&oldest) {
                break;
            }
            if self.min_arrivals.front() == Some(&oldest) {
                self.min_arrivals.pop_front();
            }
            if self.max_arrivals.front() == Some(&oldest) {
                self.max_arrivals.pop_front();
            }
            self.history.pop_front();
        }
        while self
            .min_arrivals
            .back()
            .is_some_and(|back| packet.relative() <= back.relative())
        {
            self.min_arrivals.pop_back();
        }
        while self
            .max_arrivals
            .back()
            .is_some_and(|back| packet.relative() >= back.relative())
        {
            self.max_arrivals.pop_back();
        }
        self.min_arrivals.push_back(packet);
        self.max_arrivals.push_back(packet);
        true
    }

    pub(crate) fn reset(&mut self) {
        self.history.clear();
        self.min_arrivals.clear();
        self.max_arrivals.clear();
        self.unwrapper.reset();
    }

    /// Delay (ms) of `rtp_timestamp` measured against the min-delay packet at the
    /// current time. Port of `GetDelayMs`.
    pub(crate) fn delay_ms(&self, rtp_timestamp: u32, now_samples: i64) -> i32 {
        let packet = PacketArrival {
            rtp_timestamp: self.unwrapper.peek_unwrap(rtp_timestamp),
            arrival_timestamp: now_samples,
            length_samples: 0,
        };
        self.packet_arrival_delay_ms(&packet)
    }

    /// Maximum packet arrival delay observed in the window. Port of
    /// `GetMaxDelayMs`.
    pub(crate) fn max_delay_ms(&self) -> i32 {
        match self.max_arrivals.front() {
            None => 0,
            Some(&front) => self.packet_arrival_delay_ms(&front),
        }
    }

    pub(crate) fn is_newest_rtp_timestamp(&self, rtp_timestamp: u32) -> bool {
        match self.history.back() {
            None => true,
            Some(newest) => self.unwrapper.peek_unwrap(rtp_timestamp) == newest.rtp_timestamp,
        }
    }

    fn packet_arrival_delay_ms(&self, packet: &PacketArrival) -> i32 {
        let Some(front) = self.min_arrivals.front() else {
            return 0;
        };
        debug_assert_ne!(self.sample_rate_khz, 0);
        let khz = self.sample_rate_khz;
        let delay = (packet.arrival_timestamp / khz - front.arrival_timestamp / khz)
            - (packet.rtp_timestamp / khz - front.rtp_timestamp / khz);
        delay.max(0) as i32
    }

    fn is_obsolete(&self, packet: &PacketArrival) -> bool {
        match self.history.back() {
            None => false,
            Some(newest) => {
                packet.rtp_timestamp + self.window_size_ms * self.sample_rate_khz
                    < newest.rtp_timestamp
            }
        }
    }

    fn contains(&self, packet: &PacketArrival) -> bool {
        let previous = self
            .history
            .iter()
            .rev()
            .find(|entry| entry.rtp_timestamp <= packet.rtp_timestamp);
        match previous {
            None => false,
            Some(prev) => prev.contains(packet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 ms of wall clock in 48 kHz sample units.
    const TICK: i64 = 480;

    #[test]
    fn steady_arrivals_have_zero_delay() {
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        // One 20 ms packet every 20 ms: perfectly on time.
        for seq in 0..50u32 {
            history.insert(seq * 960, 960, seq as i64 * 2 * TICK);
        }
        assert!(history.max_delay_ms() <= 1, "{}", history.max_delay_ms());
    }

    #[test]
    fn late_packet_registers_positive_delay() {
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        history.insert(0, 960, 0);
        history.insert(960, 960, 2 * TICK);
        // Third packet arrives 40 ms late.
        history.insert(1920, 960, 8 * TICK);
        assert!(history.max_delay_ms() >= 30, "{}", history.max_delay_ms());
    }

    #[test]
    fn reordered_packets_keep_their_older_rtp_timestamp() {
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        let first = 10_000;
        let reordered = first - 8 * TICK as u32;

        assert!(history.insert(first, 960, 0));
        assert!(history.is_newest_rtp_timestamp(first));

        assert!(history.insert(reordered, 960, 0));
        assert!(!history.is_newest_rtp_timestamp(reordered));
        assert_eq!(history.delay_ms(reordered, 0), 80);

        let next = first + 960;
        assert!(history.insert(next, 960, 8 * TICK));
        assert!(history.is_newest_rtp_timestamp(next));
        assert_eq!(history.delay_ms(next, 8 * TICK), 60);
        assert_eq!(history.max_delay_ms(), 60);
    }

    #[test]
    fn duplicate_timestamp_is_rejected() {
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        assert!(history.insert(0, 960, 0));
        assert!(!history.insert(0, 960, 0));
    }

    #[test]
    fn long_wall_gap_without_matching_media_gap_resets_baseline() {
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        assert!(history.insert(0, 960, 0));
        assert!(history.insert(960, 960, 2 * TICK));

        // The next packet is only one media frame later, but arrives 30 seconds
        // later. Treat this as a source/talkspurt timing discontinuity instead
        // of reporting a 30 s playout delay.
        let mut now = 3002 * TICK;
        assert!(history.reset_for_timing_discontinuity(1920, now));
        assert!(history.insert(1920, 960, now));
        assert_eq!(history.delay_ms(1920, now), 0);

        now += 2 * TICK;
        assert!(!history.reset_for_timing_discontinuity(2880, now));
        assert!(history.insert(2880, 960, now));
        assert!(history.max_delay_ms() <= 1, "{}", history.max_delay_ms());
    }

    #[test]
    fn long_gap_with_matching_media_gap_keeps_baseline() {
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        assert!(history.insert(0, 960, 0));
        assert!(history.insert(960, 960, 2 * TICK));

        let media_advanced = 960 + 3000 * 10 * 48;
        assert!(!history.reset_for_timing_discontinuity(media_advanced, 3002 * TICK));
    }
}
