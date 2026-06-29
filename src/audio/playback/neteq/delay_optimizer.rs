//! Ports of WebRTC's `underrun_optimizer.cc` and `reorder_optimizer.cc`.
//!
//! Both build a delay [`Histogram`](super::histogram::Histogram) over 20 ms
//! buckets. The underrun optimizer reports the delay whose underrun probability
//! is below `1 - quantile`; the reorder optimizer minimizes a delay-vs-loss cost
//! so reordered packets are not dropped. The [`DelayManager`](super::delay_manager)
//! combines them.

use super::histogram::Histogram;
use super::tick_timer::{Stopwatch, TickTimer};

const DELAY_BUCKETS: usize = 100;
const BUCKET_SIZE_MS: i32 = 20;

/// Estimates the delay that keeps the buffer-underrun probability below
/// `1 - quantile`. Port of `UnderrunOptimizer`.
#[derive(Debug)]
pub(crate) struct UnderrunOptimizer {
    histogram: Histogram,
    histogram_quantile: i32, // Q30
    resample_interval_ms: Option<i32>,
    resample_stopwatch: Option<Stopwatch>,
    max_delay_in_interval_ms: i32,
    optimal_delay_ms: Option<i32>,
}

impl UnderrunOptimizer {
    pub(crate) fn new(
        histogram_quantile: i32,
        forget_factor: i32,
        start_forget_weight: Option<f64>,
        resample_interval_ms: Option<i32>,
    ) -> Self {
        Self {
            histogram: Histogram::new(DELAY_BUCKETS, forget_factor, start_forget_weight),
            histogram_quantile,
            resample_interval_ms,
            resample_stopwatch: None,
            max_delay_in_interval_ms: 0,
            optimal_delay_ms: None,
        }
    }

    pub(crate) fn update(&mut self, relative_delay_ms: i32, tick_timer: &TickTimer) {
        let mut histogram_update = None;
        if let Some(interval) = self.resample_interval_ms {
            let watch = self
                .resample_stopwatch
                .get_or_insert_with(|| tick_timer.new_stopwatch());
            if watch.elapsed_ms(tick_timer) as i32 > interval {
                histogram_update = Some(self.max_delay_in_interval_ms);
                self.resample_stopwatch = Some(tick_timer.new_stopwatch());
                self.max_delay_in_interval_ms = 0;
            }
            self.max_delay_in_interval_ms = self.max_delay_in_interval_ms.max(relative_delay_ms);
        } else {
            histogram_update = Some(relative_delay_ms);
        }
        let Some(update) = histogram_update else {
            return;
        };

        let index = (update / BUCKET_SIZE_MS) as usize;
        if index < self.histogram.num_buckets() {
            self.histogram.add(index);
        }
        let bucket_index = self.histogram.quantile(self.histogram_quantile);
        self.optimal_delay_ms = Some((1 + bucket_index as i32) * BUCKET_SIZE_MS);
    }

    pub(crate) fn optimal_delay_ms(&self) -> Option<i32> {
        self.optimal_delay_ms
    }

    pub(crate) fn reset(&mut self) {
        self.histogram.reset();
        self.resample_stopwatch = None;
        self.max_delay_in_interval_ms = 0;
        self.optimal_delay_ms = None;
    }
}

/// Minimizes a delay-vs-reordered-loss cost. Port of `ReorderOptimizer`.
#[derive(Debug)]
pub(crate) struct ReorderOptimizer {
    histogram: Histogram,
    ms_per_loss_percent: i32,
    optimal_delay_ms: Option<i32>,
}

impl ReorderOptimizer {
    pub(crate) fn new(
        forget_factor: i32,
        ms_per_loss_percent: i32,
        start_forget_weight: Option<f64>,
    ) -> Self {
        Self {
            histogram: Histogram::new(DELAY_BUCKETS, forget_factor, start_forget_weight),
            ms_per_loss_percent,
            optimal_delay_ms: None,
        }
    }

    pub(crate) fn update(&mut self, relative_delay_ms: i32, reordered: bool, base_delay_ms: i32) {
        let index = if reordered {
            (relative_delay_ms / BUCKET_SIZE_MS) as usize
        } else {
            0
        };
        if index < self.histogram.num_buckets() {
            self.histogram.add(index);
        }
        let bucket_index = self.minimize_cost_function(base_delay_ms);
        self.optimal_delay_ms = Some((1 + bucket_index) * BUCKET_SIZE_MS);
    }

    pub(crate) fn optimal_delay_ms(&self) -> Option<i32> {
        self.optimal_delay_ms
    }

    pub(crate) fn reset(&mut self) {
        self.histogram.reset();
        self.optimal_delay_ms = None;
    }

    fn minimize_cost_function(&self, base_delay_ms: i32) -> i32 {
        let buckets = self.histogram.buckets();
        let mut loss_probability: i64 = 1 << 30; // Q30
        let mut min_cost = i64::MAX;
        let mut min_bucket = 0;
        for (i, bucket) in buckets.iter().enumerate() {
            loss_probability -= *bucket;
            let delay_ms = ((i as i32 * BUCKET_SIZE_MS - base_delay_ms).max(0) as i64) << 30;
            let cost = delay_ms + 100 * self.ms_per_loss_percent as i64 * loss_probability;
            if cost < min_cost {
                min_cost = cost;
                min_bucket = i as i32;
            }
            if loss_probability == 0 {
                break;
            }
        }
        min_bucket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn underrun_optimal_delay_tracks_observed_delay() {
        let mut timer = TickTimer::new();
        // No resampling so each update feeds the histogram directly.
        let mut opt = UnderrunOptimizer::new(
            ((1i64 << 30) as f64 * 0.95) as i32,
            (32768.0 * 0.983) as i32,
            Some(2.0),
            None,
        );
        for _ in 0..300 {
            opt.update(60, &timer);
            timer.increment();
        }
        let delay = opt.optimal_delay_ms.unwrap();
        // 60 ms lands in bucket 3 → optimal around (1+3)*20 = 80 ms.
        assert!((60..=100).contains(&delay), "delay={delay}");
    }

    #[test]
    fn reorder_optimizer_increases_delay_for_reordering() {
        let mut opt = ReorderOptimizer::new((32768.0 * 0.9993) as i32, 20, Some(2.0));
        for _ in 0..400 {
            opt.update(80, true, 0);
        }
        assert!(opt.optimal_delay_ms.unwrap() > 0);
    }
}
