use crate::audio::{
    playback::MonoSampleQueue,
    shared::{
        TIME_SCALE_CORRELATION_LEN, TIME_SCALE_CORRELATION_THRESHOLD, TIME_SCALE_DECIMATION,
        TIME_SCALE_DOWNSAMPLED_LEN, TIME_SCALE_FAST_CORRELATION_THRESHOLD, TIME_SCALE_MAX_LAG_4K,
        TIME_SCALE_MAX_LAG_48K, TIME_SCALE_MIN_LAG_4K, TIME_SCALE_MIN_LAG_48K,
        TIME_SCALE_NOISE_FLOOR_MS, TIME_SCALE_REF_OFFSET, TIME_SCALE_VAD_RATIO, TIME_SCALE_WINDOW,
    },
};

const DOWNSAMPLE_48KHZ_Q12: [f32; 7] = [
    1019.0 / 4096.0,
    390.0 / 4096.0,
    427.0 / 4096.0,
    440.0 / 4096.0,
    427.0 / 4096.0,
    390.0 / 4096.0,
    1019.0 / 4096.0,
];

#[derive(Debug)]
pub(crate) struct TimeScaler {
    downsampled: Vec<f32>,
    auto_correlation: Vec<f32>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PitchAnalysis {
    pub(crate) peak_index: usize,
    pub(crate) best_correlation: f32,
    pub(crate) active_speech: bool,
}

impl TimeScaler {
    pub(crate) fn new() -> Self {
        Self {
            downsampled: vec![0.0; TIME_SCALE_DOWNSAMPLED_LEN],
            auto_correlation: vec![0.0; TIME_SCALE_CORRELATION_LEN],
        }
    }

    pub(crate) fn analyze(&mut self, window: &[f32]) -> PitchAnalysis {
        debug_assert!(window.len() >= TIME_SCALE_WINDOW);
        self.downsample_to_4khz(window);
        self.auto_correlate();
        let peak_index = self.refined_peak_index_48k();
        let (best_correlation, active_speech) = normalized_correlation_and_vad(window, peak_index);
        PitchAnalysis {
            peak_index,
            best_correlation,
            active_speech,
        }
    }

    fn downsample_to_4khz(&mut self, window: &[f32]) {
        // Ported from neteq/dsp_helper.cc:47 and :355, using the 48 kHz Q12
        // FIR table with decimate-by-12. The exact anti-alias shape is not a
        // timing actuator; it only stabilizes the pitch-period search.
        for out in 0..TIME_SCALE_DOWNSAMPLED_LEN {
            let base = out * TIME_SCALE_DECIMATION;
            let mut sum = 0.0;
            for (tap, coeff) in DOWNSAMPLE_48KHZ_Q12.iter().enumerate() {
                sum += window.get(base + tap).copied().unwrap_or(0.0) * coeff;
            }
            self.downsampled[out] = sum;
        }
    }

    fn auto_correlate(&mut self) {
        // Direct f32 port of neteq/time_stretch.cc:166-178 without the Q14
        // normalization. This computes lags 10..59 in the 4 kHz domain.
        for lag_offset in 0..TIME_SCALE_CORRELATION_LEN {
            let delayed = TIME_SCALE_MAX_LAG_4K - TIME_SCALE_MIN_LAG_4K - lag_offset;
            let mut sum = 0.0;
            for n in 0..TIME_SCALE_CORRELATION_LEN {
                sum += self.downsampled[TIME_SCALE_MAX_LAG_4K + n] * self.downsampled[delayed + n];
            }
            self.auto_correlation[lag_offset] = sum;
        }
    }

    fn refined_peak_index_48k(&self) -> usize {
        let mut peak = self
            .auto_correlation
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(index, _)| index)
            .unwrap_or(0);
        let peak_value = self.auto_correlation[peak];
        let lag = TIME_SCALE_MIN_LAG_4K + peak;
        let half_lag = lag / 2;
        if half_lag >= TIME_SCALE_MIN_LAG_4K {
            let half_index = half_lag - TIME_SCALE_MIN_LAG_4K;
            if half_index < self.auto_correlation.len()
                && self.auto_correlation[half_index] >= peak_value * 0.85
            {
                peak = half_index;
            }
        }

        // Replaces neteq/dsp_helper.cc:68-164 ParabolicFit with the same
        // three-point parabola in floating point.
        let delta = if peak > 0 && peak + 1 < self.auto_correlation.len() {
            let left = self.auto_correlation[peak - 1];
            let center = self.auto_correlation[peak];
            let right = self.auto_correlation[peak + 1];
            let denom = left - 2.0 * center + right;
            if denom.abs() > f32::EPSILON {
                (0.5 * (left - right) / denom).clamp(-1.0, 1.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        let lag_4k = TIME_SCALE_MIN_LAG_4K as f32 + peak as f32 + delta;
        (lag_4k * TIME_SCALE_DECIMATION as f32)
            .round()
            .clamp(TIME_SCALE_MIN_LAG_48K as f32, TIME_SCALE_MAX_LAG_48K as f32) as usize
    }
}

pub(crate) fn accelerate_one_period(
    q: &mut MonoSampleQueue,
    lead: usize,
    peak_index: usize,
    overlap: usize,
) -> usize {
    let before = q.frames();
    let overlap = overlap.min(peak_index);
    q.overlap_add_compress(overlap, lead + TIME_SCALE_REF_OFFSET, peak_index);
    before.saturating_sub(q.frames())
}

pub(crate) fn expand_one_period(
    q: &mut MonoSampleQueue,
    lead: usize,
    peak_index: usize,
    overlap: usize,
) -> usize {
    let before = q.frames();
    let overlap = overlap.min(peak_index);
    q.overlap_add_expand(overlap, lead + TIME_SCALE_REF_OFFSET, peak_index);
    q.frames().saturating_sub(before)
}

pub(crate) fn threshold_for_mode(fast: bool) -> f32 {
    if fast {
        TIME_SCALE_FAST_CORRELATION_THRESHOLD
    } else {
        TIME_SCALE_CORRELATION_THRESHOLD
    }
}

fn normalized_correlation_and_vad(window: &[f32], peak_index: usize) -> (f32, bool) {
    // Direct f32 port of neteq/time_stretch.cc:92-145. vec1 starts at 15 ms
    // minus one pitch period; vec2 starts at 15 ms.
    let vec1_start = TIME_SCALE_REF_OFFSET.saturating_sub(peak_index);
    let vec2_start = TIME_SCALE_REF_OFFSET;
    let vec1 = &window[vec1_start..vec1_start + peak_index];
    let vec2 = &window[vec2_start..vec2_start + peak_index];
    let mut cross = 0.0;
    let mut e1 = 0.0;
    let mut e2 = 0.0;
    for (&left, &right) in vec1.iter().zip(vec2.iter()) {
        cross += left * right;
        e1 += left * left;
        e2 += right * right;
    }
    let best_correlation = (cross.max(0.0) / (e1 * e2).sqrt().max(1.0e-12)).clamp(0.0, 1.0);

    // Fixed-threshold form of neteq/time_stretch.cc:180-221 SpeechDetection.
    let mean_square = (e1 + e2) / (2 * peak_index).max(1) as f32;
    let active_speech = mean_square > TIME_SCALE_VAD_RATIO * TIME_SCALE_NOISE_FLOOR_MS;
    (best_correlation, active_speech)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::SAMPLE_RATE;

    #[test]
    fn time_scale_correlation_search_finds_known_pitch() {
        let mut scaler = TimeScaler::new();
        let window: Vec<f32> = (0..TIME_SCALE_WINDOW)
            .map(|n| {
                let phase = (n % 240) as f32 / 240.0;
                phase * 2.0 - 1.0
            })
            .collect();
        let analysis = scaler.analyze(&window);
        assert!(
            analysis.peak_index.abs_diff((SAMPLE_RATE / 200) as usize) <= 12,
            "{analysis:?}"
        );
        assert!(analysis.best_correlation > 0.9, "{analysis:?}");
        assert!(analysis.active_speech, "{analysis:?}");

        let quiet = vec![0.0; TIME_SCALE_WINDOW];
        let analysis = scaler.analyze(&quiet);
        assert!(!analysis.active_speech, "{analysis:?}");
    }
}
