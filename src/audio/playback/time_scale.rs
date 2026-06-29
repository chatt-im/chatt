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
    background_noise: VadBackgroundNoiseEstimate,
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
            background_noise: VadBackgroundNoiseEstimate::new(),
        }
    }

    pub(crate) fn analyze(&mut self, window: &[f32]) -> PitchAnalysis {
        debug_assert!(window.len() >= TIME_SCALE_WINDOW);
        self.downsample_to_4khz(window);
        self.auto_correlate();
        let peak_index = self.refined_peak_index_48k();
        let (best_correlation, active_speech, mean_square) =
            normalized_correlation_and_vad(window, peak_index, self.background_noise.energy());
        self.background_noise.update(mean_square);
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
        // Octave correction: the 4 kHz autocorrelation frequently maxes at a
        // higher harmonic of the true pitch period. When the half-lag bin holds
        // nearly as much energy, it is the fundamental, so prefer it. This keeps
        // each accelerate/expand operation on a single fundamental period rather
        // than a multiple of it.
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
) -> usize {
    let before = q.frames();
    let Some(start) = (lead + TIME_SCALE_REF_OFFSET).checked_sub(peak_index) else {
        return 0;
    };
    q.overlap_add_compress(start, peak_index);
    before.saturating_sub(q.frames())
}

pub(crate) fn expand_one_period(q: &mut MonoSampleQueue, lead: usize, peak_index: usize) -> usize {
    let before = q.frames();
    q.overlap_add_expand(lead + TIME_SCALE_REF_OFFSET, peak_index);
    q.frames().saturating_sub(before)
}

pub(crate) fn threshold_for_mode(fast: bool) -> f32 {
    if fast {
        TIME_SCALE_FAST_CORRELATION_THRESHOLD
    } else {
        TIME_SCALE_CORRELATION_THRESHOLD
    }
}

fn normalized_correlation_and_vad(
    window: &[f32],
    peak_index: usize,
    background_noise_energy: f32,
) -> (f32, bool, f32) {
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

    // Floating-point form of neteq/time_stretch.cc:180-221 SpeechDetection.
    // The fixed fallback background-noise energy is 75000 in int16 units,
    // equivalent to TIME_SCALE_NOISE_FLOOR_MS after normalizing by 32768^2.
    let mean_square = (e1 + e2) / (2 * peak_index).max(1) as f32;
    let active_speech = mean_square > TIME_SCALE_VAD_RATIO * background_noise_energy;
    (best_correlation, active_speech, mean_square)
}

#[derive(Debug)]
struct VadBackgroundNoiseEstimate {
    energy: f32,
    initialized: bool,
    update_threshold: f32,
    low_energy_update_threshold: f32,
    max_energy: f32,
}

impl VadBackgroundNoiseEstimate {
    fn new() -> Self {
        Self {
            energy: TIME_SCALE_NOISE_FLOOR_MS,
            initialized: false,
            update_threshold: 500_000.0 / (i16::MAX as f32 * i16::MAX as f32),
            low_energy_update_threshold: 0.0,
            max_energy: 0.0,
        }
    }

    fn energy(&self) -> f32 {
        if self.initialized {
            self.energy
        } else {
            TIME_SCALE_NOISE_FLOOR_MS
        }
    }

    fn update(&mut self, sample_energy: f32) {
        if sample_energy < self.update_threshold {
            self.energy = sample_energy.max(1.0 / (i16::MAX as f32 * i16::MAX as f32));
            self.update_threshold = self.energy;
            self.low_energy_update_threshold = 0.0;
            self.initialized = true;
            return;
        }

        // Floating-point form of BackgroundNoise::IncrementEnergyThreshold.
        // The WebRTC comment states that the fixed-point body is essentially
        // threshold += increment * threshold, with the 60 dB max-energy floor.
        const THRESHOLD_INCREMENT: f32 = 229.0 / 65_536.0;
        self.low_energy_update_threshold += THRESHOLD_INCREMENT * self.update_threshold;
        self.update_threshold += THRESHOLD_INCREMENT * self.update_threshold;
        self.max_energy -= self.max_energy / 1024.0;
        self.max_energy = self.max_energy.max(sample_energy);
        self.update_threshold = self.update_threshold.max(self.max_energy / 1_048_576.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::shared::SAMPLE_RATE;

    fn queue_samples(q: &MonoSampleQueue) -> Vec<f32> {
        let mut samples = Vec::new();
        q.copy_window(0, q.frames(), &mut samples);
        samples
    }

    fn assert_near(left: f32, right: f32) {
        assert!(
            (left - right).abs() <= 1.0e-6,
            "left={left:.8} right={right:.8}"
        );
    }

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

    #[test]
    fn accelerate_one_period_matches_webrtc_crossfade_window() {
        // Ported from NetEq Accelerate::CheckCriteriaAndStretch and
        // AudioVector::CrossFade: copy 0..15 ms, crossfade the following pitch
        // period onto the tail of that output, then skip the copied period.
        let segment = TIME_SCALE_MIN_LAG_48K;
        let ref_offset = TIME_SCALE_REF_OFFSET;
        let mut input = vec![0.0; ref_offset + segment + 32];
        input[ref_offset..ref_offset + segment].fill(1.0);
        input[ref_offset + segment..].fill(0.25);

        let mut q = MonoSampleQueue::new();
        q.push_back(&input);

        assert_eq!(accelerate_one_period(&mut q, 0, segment), segment);
        let output = queue_samples(&q);
        assert_eq!(output.len(), input.len() - segment);

        for sample in output.iter().take(ref_offset - segment) {
            assert_near(*sample, 0.0);
        }
        for index in 0..segment {
            let expected = (index + 1) as f32 / (segment + 1) as f32;
            assert_near(output[ref_offset - segment + index], expected);
        }
        for sample in output.iter().skip(ref_offset) {
            assert_near(*sample, 0.25);
        }
    }

    #[test]
    fn expand_one_period_matches_webrtc_preemptive_expand_crossfade_window() {
        // Ported from NetEq PreemptiveExpand::CheckCriteriaAndStretch: copy
        // through the following pitch period, crossfade the period before the
        // reference onto the output tail, then append the original future audio.
        let segment = TIME_SCALE_MIN_LAG_48K;
        let ref_offset = TIME_SCALE_REF_OFFSET;
        let mut input = vec![0.0; ref_offset + segment + 32];
        input[ref_offset..ref_offset + segment].fill(1.0);
        input[ref_offset + segment..].fill(0.25);

        let mut q = MonoSampleQueue::new();
        q.push_back(&input);

        assert_eq!(expand_one_period(&mut q, 0, segment), segment);
        let output = queue_samples(&q);
        assert_eq!(output.len(), input.len() + segment);

        for sample in output.iter().take(ref_offset) {
            assert_near(*sample, 0.0);
        }
        for index in 0..segment {
            let fade_to_previous = (index + 1) as f32 / (segment + 1) as f32;
            let expected = 1.0 - fade_to_previous;
            assert_near(output[ref_offset + index], expected);
        }
        for sample in &output[ref_offset + segment..ref_offset + 2 * segment] {
            assert_near(*sample, 1.0);
        }
        for sample in output.iter().skip(ref_offset + 2 * segment) {
            assert_near(*sample, 0.25);
        }
    }
}
