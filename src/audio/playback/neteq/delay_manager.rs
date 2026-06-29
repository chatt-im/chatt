//! A port of WebRTC's `modules/audio_coding/neteq/delay_manager.cc`.
//!
//! The target buffer level is the underrun optimizer's estimate, raised by the
//! reorder optimizer so reordered packets are not lost. This is the sole
//! latency controller for the live path (replacing Chatt's adaptive-resampling
//! estimator); accelerate/preemptive-expand steer the actual buffer toward this
//! target. Field-trial config is fixed to WebRTC defaults.

use super::delay_optimizer::{ReorderOptimizer, UnderrunOptimizer};
use super::neteq_status::PacketArrivedInfo;
use super::tick_timer::TickTimer;

const START_DELAY_MS: i32 = 80;

// WebRTC `DelayManager::Config` defaults.
const QUANTILE: f64 = 0.95;
const FORGET_FACTOR: f64 = 0.983;
const START_FORGET_WEIGHT: f64 = 2.0;
const RESAMPLE_INTERVAL_MS: i32 = 500;
const REORDER_FORGET_FACTOR: f64 = 0.9993;
const MS_PER_LOSS_PERCENT: i32 = 20;

/// Tracks the preferred buffer level from arrival statistics.
#[derive(Debug)]
pub(crate) struct DelayManager {
    underrun_optimizer: UnderrunOptimizer,
    reorder_optimizer: ReorderOptimizer,
    target_level_ms: i32,
}

impl DelayManager {
    pub(crate) fn new() -> Self {
        let underrun_optimizer = UnderrunOptimizer::new(
            ((1i64 << 30) as f64 * QUANTILE) as i32,
            ((1 << 15) as f64 * FORGET_FACTOR) as i32,
            Some(START_FORGET_WEIGHT),
            Some(RESAMPLE_INTERVAL_MS),
        );
        let reorder_optimizer = ReorderOptimizer::new(
            ((1 << 15) as f64 * REORDER_FORGET_FACTOR) as i32,
            MS_PER_LOSS_PERCENT,
            Some(START_FORGET_WEIGHT),
        );
        let mut manager = Self {
            underrun_optimizer,
            reorder_optimizer,
            target_level_ms: START_DELAY_MS,
        };
        manager.reset();
        manager
    }

    /// Updates from a newly arrived packet's relative delay. Port of
    /// `DelayManager::Update`.
    pub(crate) fn update(
        &mut self,
        arrival_delay_ms: i32,
        reordered: bool,
        _info: &PacketArrivedInfo,
        tick_timer: &TickTimer,
    ) {
        if !reordered {
            self.underrun_optimizer.update(arrival_delay_ms, tick_timer);
        }
        self.target_level_ms = self
            .underrun_optimizer
            .optimal_delay_ms()
            .unwrap_or(START_DELAY_MS);
        self.reorder_optimizer
            .update(arrival_delay_ms, reordered, self.target_level_ms);
        self.target_level_ms = self
            .target_level_ms
            .max(self.reorder_optimizer.optimal_delay_ms().unwrap_or(0));
    }

    pub(crate) fn reset(&mut self) {
        self.underrun_optimizer.reset();
        self.reorder_optimizer.reset();
        self.target_level_ms = START_DELAY_MS;
    }

    /// The preferred buffer level in ms. Port of `TargetDelayMs`.
    pub(crate) fn target_delay_ms(&self) -> i32 {
        self.target_level_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> PacketArrivedInfo {
        PacketArrivedInfo {
            packet_length_samples: 960,
            main_timestamp: 0,
            main_sequence_number: 0,
            is_cng_or_dtmf: false,
            is_dtx: false,
            buffer_flush: false,
        }
    }

    #[test]
    fn starts_at_default_target() {
        let manager = DelayManager::new();
        assert_eq!(manager.target_delay_ms(), START_DELAY_MS);
    }

    #[test]
    fn target_rises_with_observed_delay() {
        let mut timer = TickTimer::new();
        let mut manager = DelayManager::new();
        // Feed a consistent 100 ms arrival delay; the target should climb.
        for _ in 0..2000 {
            manager.update(100, false, &info(), &timer);
            timer.increment();
        }
        assert!(
            manager.target_delay_ms() >= 80,
            "{}",
            manager.target_delay_ms()
        );
    }
}
