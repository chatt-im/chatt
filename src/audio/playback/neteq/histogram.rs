//! A port of WebRTC's `modules/audio_coding/neteq/histogram.cc`.
//!
//! A fixed-point probability histogram (buckets in Q30 summing to 1) with a
//! forgetting factor (Q15) that decays older observations. Used by the underrun
//! and reorder optimizers to estimate the delay distribution. The forgetting
//! factor ramps from 0 toward its steady-state value for fast initial
//! convergence; an optional `start_forget_weight` changes that ramp.

/// Fixed-point probability histogram. Buckets are Q30 and sum to `1 << 30`.
#[derive(Debug)]
pub(crate) struct Histogram {
    buckets: Vec<i64>,
    forget_factor: i32,      // Q15
    base_forget_factor: i32, // Q15
    add_count: i32,
    start_forget_weight: Option<f64>,
}

impl Histogram {
    pub(crate) fn new(
        num_buckets: usize,
        forget_factor: i32,
        start_forget_weight: Option<f64>,
    ) -> Self {
        debug_assert!(forget_factor < (1 << 15));
        let mut histogram = Self {
            buckets: vec![0; num_buckets],
            forget_factor: 0,
            base_forget_factor: forget_factor,
            add_count: 0,
            start_forget_weight,
        };
        histogram.reset();
        histogram
    }

    pub(crate) fn num_buckets(&self) -> usize {
        self.buckets.len()
    }

    pub(crate) fn buckets(&self) -> &[i64] {
        &self.buckets
    }

    /// Adds an observation in bucket `value`, decaying all buckets by the
    /// forgetting factor and renormalizing to sum to 1 (Q30). Port of
    /// `Histogram::Add`.
    pub(crate) fn add(&mut self, value: usize) {
        debug_assert!(value < self.buckets.len());
        let mut vector_sum: i64 = 0;
        for bucket in &mut self.buckets {
            *bucket = (*bucket * self.forget_factor as i64) >> 15;
            vector_sum += *bucket;
        }

        let increment = (32768 - self.forget_factor as i64) << 15;
        self.buckets[value] += increment;
        vector_sum += increment;

        // Compensate for fixed-point rounding so the buckets sum to 1 (Q30).
        vector_sum -= 1 << 30;
        if vector_sum != 0 {
            let flip_sign = if vector_sum > 0 { -1 } else { 1 };
            for bucket in &mut self.buckets {
                let correction = flip_sign * vector_sum.abs().min(*bucket >> 4);
                *bucket += correction;
                vector_sum += correction;
                if vector_sum.abs() == 0 {
                    break;
                }
            }
        }

        self.add_count += 1;

        // Ramp the forgetting factor toward its steady-state value.
        if self.start_forget_weight.is_some() {
            if self.forget_factor != self.base_forget_factor {
                let weight = self.start_forget_weight.unwrap();
                let forget_factor =
                    ((1 << 15) as f64 * (1.0 - weight / (self.add_count as f64 + 1.0))) as i32;
                self.forget_factor = forget_factor.clamp(0, self.base_forget_factor);
            }
        } else {
            self.forget_factor += (self.base_forget_factor - self.forget_factor + 3) >> 2;
        }
    }

    /// Bucket index for the given reverse-cumulative `probability` (Q30). Port
    /// of `Histogram::Quantile`.
    pub(crate) fn quantile(&self, probability: i32) -> usize {
        let inverse_probability = (1i64 << 30) - probability as i64;
        let mut index = 0;
        let mut sum: i64 = 1 << 30;
        sum -= self.buckets[index];
        while sum > inverse_probability && index < self.buckets.len() - 1 {
            index += 1;
            sum -= self.buckets[index];
        }
        index
    }

    /// Resets to an exponentially-decaying distribution `0.5^(i+1)`. Port of
    /// `Histogram::Reset`.
    pub(crate) fn reset(&mut self) {
        let mut temp_prob: u16 = 0x4002;
        for bucket in &mut self.buckets {
            temp_prob >>= 1;
            *bucket = (temp_prob as i64) << 16;
        }
        self.forget_factor = 0;
        self.add_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_distribution_sums_to_one_q30() {
        let histogram = Histogram::new(100, 32745, None);
        let sum: i64 = histogram.buckets().iter().sum();
        // Geometric series of 0.5^(i+1) sums to just under 1 (Q30).
        assert!((sum - (1 << 30)).abs() < (1 << 16), "sum={sum}");
    }

    #[test]
    fn add_keeps_sum_at_one_q30() {
        let mut histogram = Histogram::new(100, 32745, Some(2.0));
        for value in [0, 3, 3, 5, 1, 3, 10] {
            histogram.add(value);
            let sum: i64 = histogram.buckets().iter().sum();
            assert_eq!(sum, 1 << 30, "value={value}");
        }
    }

    #[test]
    fn quantile_tracks_observed_bucket() {
        let mut histogram = Histogram::new(100, 32745, Some(2.0));
        for _ in 0..200 {
            histogram.add(4);
        }
        // With nearly all mass at bucket 4, the 0.95 quantile is around 4.
        let q = histogram.quantile(((1i64 << 30) as f64 * 0.95) as i32);
        assert!((3..=5).contains(&q), "q={q}");
    }
}
