//! A port of WebRTC's `modules/audio_coding/neteq/packet_arrival_history.cc`.
//!
//! Records when packets arrived (in media-sample units derived from the tick
//! timer) keyed by unwrapped RTP timestamp, within a fixed window. The delay of
//! a timestamp is measured against the packet in history that maximizes it, via
//! monotonic min/max deques. The decision logic uses this for the playout-delay
//! estimate and `GetMaxDelayMs`.

use std::collections::{BTreeMap, VecDeque};

use super::tick_timer::TickTimer;

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

/// Fixed-window record of packet arrivals for delay estimation.
#[derive(Debug)]
pub(crate) struct PacketArrivalHistory {
    window_size_ms: i64,
    sample_rate_khz: i64,
    unwrapper: TimestampUnwrapper,
    history: BTreeMap<i64, PacketArrival>,
    min_arrivals: VecDeque<PacketArrival>,
    max_arrivals: VecDeque<PacketArrival>,
}

impl PacketArrivalHistory {
    pub(crate) fn new(window_size_ms: i32) -> Self {
        Self {
            window_size_ms: window_size_ms as i64,
            sample_rate_khz: 0,
            unwrapper: TimestampUnwrapper::default(),
            history: BTreeMap::new(),
            min_arrivals: VecDeque::new(),
            max_arrivals: VecDeque::new(),
        }
    }

    pub(crate) fn set_sample_rate(&mut self, sample_rate: i32) {
        self.sample_rate_khz = (sample_rate / 1000) as i64;
    }

    pub(crate) fn size(&self) -> usize {
        self.history.len()
    }

    /// Records a packet arrival. Returns false if it is too old or a duplicate.
    /// Port of `PacketArrivalHistory::Insert`.
    pub(crate) fn insert(
        &mut self,
        rtp_timestamp: u32,
        packet_length_samples: i32,
        tick_timer: &TickTimer,
    ) -> bool {
        let arrival_timestamp = self.now_samples(tick_timer);
        let packet = PacketArrival {
            rtp_timestamp: self.unwrapper.unwrap(rtp_timestamp),
            arrival_timestamp,
            length_samples: packet_length_samples as i64,
        };
        if self.is_obsolete(&packet) || self.contains(&packet) {
            return false;
        }
        self.history.insert(packet.rtp_timestamp, packet);
        let newest = *self.history.values().next_back().expect("just inserted");
        if packet != newest {
            // Reordered packet: kept in history but excluded from min/max.
            return true;
        }
        while let Some((&oldest_key, &oldest)) = self.history.iter().next() {
            if !self.is_obsolete(&oldest) {
                break;
            }
            if self.min_arrivals.front() == Some(&oldest) {
                self.min_arrivals.pop_front();
            }
            if self.max_arrivals.front() == Some(&oldest) {
                self.max_arrivals.pop_front();
            }
            self.history.remove(&oldest_key);
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
    pub(crate) fn delay_ms(&self, rtp_timestamp: u32, tick_timer: &TickTimer) -> i32 {
        let packet = PacketArrival {
            rtp_timestamp: self.unwrapper.peek_unwrap(rtp_timestamp),
            arrival_timestamp: self.now_samples(tick_timer),
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
        match self.history.values().next_back() {
            None => true,
            Some(newest) => self.unwrapper.peek_unwrap(rtp_timestamp) == newest.rtp_timestamp,
        }
    }

    fn now_samples(&self, tick_timer: &TickTimer) -> i64 {
        tick_timer.ticks() as i64 * tick_timer.ms_per_tick() as i64 * self.sample_rate_khz
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
        match self.history.values().next_back() {
            None => false,
            Some(newest) => {
                packet.rtp_timestamp + self.window_size_ms * self.sample_rate_khz
                    < newest.rtp_timestamp
            }
        }
    }

    fn contains(&self, packet: &PacketArrival) -> bool {
        match self.history.range(..=packet.rtp_timestamp).next_back() {
            None => false,
            Some((_, prev)) => prev.contains(packet),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_arrivals_have_zero_delay() {
        let mut timer = TickTimer::new();
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        // One 20 ms packet every 20 ms (2 ticks): perfectly on time.
        for seq in 0..50u32 {
            history.insert(seq * 960, 960, &timer);
            timer.increment_by(2);
        }
        assert!(history.max_delay_ms() <= 1, "{}", history.max_delay_ms());
    }

    #[test]
    fn late_packet_registers_positive_delay() {
        let mut timer = TickTimer::new();
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        history.insert(0, 960, &timer);
        timer.increment_by(2);
        history.insert(960, 960, &timer);
        // Third packet arrives 40 ms late (extra 4 ticks).
        timer.increment_by(6);
        history.insert(1920, 960, &timer);
        assert!(history.max_delay_ms() >= 30, "{}", history.max_delay_ms());
    }

    #[test]
    fn duplicate_timestamp_is_rejected() {
        let timer = TickTimer::new();
        let mut history = PacketArrivalHistory::new(2000);
        history.set_sample_rate(48000);
        assert!(history.insert(0, 960, &timer));
        assert!(!history.insert(0, 960, &timer));
    }
}
