//! A port of WebRTC's `modules/audio_coding/neteq/delay_constraints.cc`.
//!
//! Clamps the target delay to the configured minimum/maximum and to 75 % of the
//! packet-buffer capacity. The effective minimum delay is the larger of the
//! externally set minimum and the (capacity-bounded) base minimum.

const MIN_BASE_MINIMUM_DELAY_MS: i32 = 0;
const MAX_BASE_MINIMUM_DELAY_MS: i32 = 10_000;

/// Min/max delay bounds for the target level.
#[derive(Debug)]
pub(crate) struct DelayConstraints {
    max_packets_in_buffer: i32,
    base_minimum_delay_ms: i32,
    effective_minimum_delay_ms: i32,
    minimum_delay_ms: i32,
    maximum_delay_ms: i32,
    packet_len_ms: i32,
}

impl DelayConstraints {
    pub(crate) fn new(max_packets_in_buffer: i32, base_minimum_delay_ms: i32) -> Self {
        Self {
            max_packets_in_buffer,
            base_minimum_delay_ms,
            effective_minimum_delay_ms: base_minimum_delay_ms,
            minimum_delay_ms: 0,
            maximum_delay_ms: 0,
            packet_len_ms: 0,
        }
    }

    /// Clamps `delay_ms` into the valid range. Port of `DelayConstraints::Clamp`.
    pub(crate) fn clamp(&self, delay_ms: i32) -> i32 {
        let mut delay_ms = delay_ms.max(self.effective_minimum_delay_ms);
        if self.maximum_delay_ms > 0 {
            delay_ms = delay_ms.min(self.maximum_delay_ms);
        }
        if self.packet_len_ms > 0 {
            delay_ms = delay_ms.min(3 * self.max_packets_in_buffer * self.packet_len_ms / 4);
        }
        delay_ms
    }

    pub(crate) fn set_packet_audio_length(&mut self, length_ms: i32) -> bool {
        if length_ms <= 0 {
            return false;
        }
        self.packet_len_ms = length_ms;
        true
    }

    pub(crate) fn set_minimum_delay(&mut self, delay_ms: i32) -> bool {
        if !self.is_valid_minimum_delay(delay_ms) {
            return false;
        }
        self.minimum_delay_ms = delay_ms;
        self.update_effective_minimum_delay();
        true
    }

    pub(crate) fn set_maximum_delay(&mut self, delay_ms: i32) -> bool {
        if delay_ms != 0 && delay_ms < self.minimum_delay_ms {
            return false;
        }
        self.maximum_delay_ms = delay_ms;
        self.update_effective_minimum_delay();
        true
    }

    pub(crate) fn set_base_minimum_delay(&mut self, delay_ms: i32) -> bool {
        if !self.is_valid_base_minimum_delay(delay_ms) {
            return false;
        }
        self.base_minimum_delay_ms = delay_ms;
        self.update_effective_minimum_delay();
        true
    }

    pub(crate) fn base_minimum_delay(&self) -> i32 {
        self.base_minimum_delay_ms
    }

    fn is_valid_minimum_delay(&self, delay_ms: i32) -> bool {
        (0..=self.minimum_delay_upper_bound()).contains(&delay_ms)
    }

    fn is_valid_base_minimum_delay(&self, delay_ms: i32) -> bool {
        (MIN_BASE_MINIMUM_DELAY_MS..=MAX_BASE_MINIMUM_DELAY_MS).contains(&delay_ms)
    }

    fn update_effective_minimum_delay(&mut self) {
        let base = self
            .base_minimum_delay_ms
            .clamp(0, self.minimum_delay_upper_bound());
        self.effective_minimum_delay_ms = self.minimum_delay_ms.max(base);
    }

    fn minimum_delay_upper_bound(&self) -> i32 {
        let q75 = self.max_packets_in_buffer * self.packet_len_ms * 3 / 4;
        let q75 = if q75 > 0 {
            q75
        } else {
            MAX_BASE_MINIMUM_DELAY_MS
        };
        let maximum = if self.maximum_delay_ms > 0 {
            self.maximum_delay_ms
        } else {
            MAX_BASE_MINIMUM_DELAY_MS
        };
        maximum.min(q75)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_respects_minimum_and_maximum() {
        let mut constraints = DelayConstraints::new(200, 0);
        assert!(constraints.set_packet_audio_length(20));
        assert!(constraints.set_minimum_delay(40));
        assert!(constraints.set_maximum_delay(200));
        assert_eq!(constraints.clamp(10), 40);
        assert_eq!(constraints.clamp(120), 120);
        assert_eq!(constraints.clamp(500), 200);
    }

    #[test]
    fn clamp_limited_to_three_quarters_of_capacity() {
        let mut constraints = DelayConstraints::new(10, 0);
        assert!(constraints.set_packet_audio_length(20));
        // 75% of 10 packets * 20 ms = 150 ms.
        assert_eq!(constraints.clamp(1000), 150);
    }
}
