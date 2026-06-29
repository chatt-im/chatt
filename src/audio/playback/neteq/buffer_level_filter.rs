//! A port of WebRTC's `modules/audio_coding/neteq/buffer_level_filter.cc`.
//!
//! A first-order IIR over the packet-buffer span (in samples). The smoothing
//! factor is chosen from the target level; time-stretch operations are
//! subtracted directly so accelerate/preemptive-expand show up immediately in
//! the filtered level rather than being smoothed away. All arithmetic is the
//! original fixed-point (filter state in Q8).

/// First-order buffer-level smoother.
#[derive(Debug)]
pub(crate) struct BufferLevelFilter {
    level_factor: i64,           // Filter factor in Q8.
    filtered_current_level: i64, // Filtered level in Q8.
}

impl BufferLevelFilter {
    pub(crate) fn new() -> Self {
        let mut filter = Self {
            level_factor: 0,
            filtered_current_level: 0,
        };
        filter.reset();
        filter
    }

    pub(crate) fn reset(&mut self) {
        self.filtered_current_level = 0;
        self.level_factor = 253;
    }

    /// IIR update with the current buffer span, subtracting time-stretched
    /// samples (which bypass the filter). Port of `BufferLevelFilter::Update`.
    pub(crate) fn update(&mut self, buffer_size_samples: usize, time_stretched_samples: i32) {
        let filtered = (self.level_factor * self.filtered_current_level >> 8)
            + (256 - self.level_factor) * buffer_size_samples as i64;
        self.filtered_current_level =
            (filtered - (time_stretched_samples as i64) * (1 << 8)).max(0);
    }

    /// Directly sets the filtered level (used after buffer flushes / delay
    /// changes). Port of `SetFilteredBufferLevel`.
    pub(crate) fn set_filtered_buffer_level(&mut self, buffer_size_samples: i64) {
        self.filtered_current_level = buffer_size_samples * 256;
    }

    /// Selects the smoothing factor from the target level in ms. Port of
    /// `SetTargetBufferLevel`.
    pub(crate) fn set_target_buffer_level(&mut self, target_buffer_level_ms: i32) {
        self.level_factor = if target_buffer_level_ms <= 20 {
            251
        } else if target_buffer_level_ms <= 60 {
            252
        } else if target_buffer_level_ms <= 140 {
            253
        } else {
            254
        };
    }

    /// Filtered current level in samples, rounded to nearest. Port of
    /// `filtered_current_level`.
    pub(crate) fn filtered_current_level(&self) -> i32 {
        ((self.filtered_current_level + (1 << 7)) >> 8) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_toward_constant_input() {
        let mut filter = BufferLevelFilter::new();
        // The default factor (253) gives a ~85-sample time constant, so this
        // needs many iterations to reach the steady input level.
        for _ in 0..5000 {
            filter.update(960, 0);
        }
        assert!((filter.filtered_current_level() - 960).abs() <= 1);
    }

    #[test]
    fn time_stretch_is_subtracted_immediately() {
        let mut filter = BufferLevelFilter::new();
        for _ in 0..200 {
            filter.update(960, 0);
        }
        let before = filter.filtered_current_level();
        filter.update(960, 480);
        assert!(
            filter.filtered_current_level() <= before - 480 + 5,
            "accelerate delta should drop the filtered level immediately"
        );
    }

    #[test]
    fn set_filtered_level_is_exact_after_rounding() {
        let mut filter = BufferLevelFilter::new();
        filter.set_filtered_buffer_level(1234);
        assert_eq!(filter.filtered_current_level(), 1234);
    }
}
