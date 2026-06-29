use crate::audio::shared::{
    LIVE_OPUS_FRAME_SAMPLES, SAMPLE_RATE, TIME_SCALE_MAX_LAG_48K, TIME_SCALE_MIN_LAG_48K,
};

const FS_MULT: usize = SAMPLE_RATE as usize / 8_000;
const OVERLAP_LENGTH: usize = 5 * FS_MULT;
const SIGNAL_LENGTH: usize = 256 * FS_MULT;
const DISTORTION_LENGTH: usize = 20 * FS_MULT;
const LPC_ANALYSIS_LENGTH: usize = 160 * FS_MULT;
const EXPAND_RANDOM_EXTRA: usize = 30;
const BACKGROUND_LPC_ORDER: usize = 8;
const UNVOICED_LPC_ORDER: usize = 6;
const BACKGROUND_VEC_LEN: usize = 256;
const BACKGROUND_RESIDUAL_LEN: usize = 64;
const MAX_CONSECUTIVE_EXPANDS: usize = 200;
const MERGE_REQUIRED_LENGTH: usize = (120 + 80 + 2) * FS_MULT;
const MERGE_CORRELATION_LENGTH: usize = 60 * FS_MULT;
const NORMAL_CROSSFADE_LENGTH: usize = SAMPLE_RATE as usize / 1000;
const SAMPLE_SCALE: f32 = i16::MAX as f32;
const MIN_ENERGY: f32 = 1.0 / (SAMPLE_SCALE * SAMPLE_SCALE);
const MUTE_WINDOW_START_48K: i32 = 31_711;
const MUTE_WINDOW_INCREMENT_48K: i32 = -1_057;
const UNMUTE_WINDOW_START_48K: i32 = 1_057;
const UNMUTE_WINDOW_INCREMENT_48K: i32 = 1_057;
const OVERLAP_WINDOW_SCALE: f32 = 32_768.0;

#[derive(Debug)]
pub(crate) struct ConcealmentChunk {
    pub(crate) overlap: Vec<f32>,
    pub(crate) samples: Vec<f32>,
}

pub(crate) fn blend_expand_overlap_sample(old: f32, new: f32, index: usize) -> f32 {
    let mute = (MUTE_WINDOW_START_48K + MUTE_WINDOW_INCREMENT_48K * index as i32) as f32
        / OVERLAP_WINDOW_SCALE;
    let unmute = (UNMUTE_WINDOW_START_48K + UNMUTE_WINDOW_INCREMENT_48K * index as i32) as f32
        / OVERLAP_WINDOW_SCALE;
    old * mute + new * unmute
}

fn blend_expand_overlap_tail(signal: &mut [f32], overlap: &[f32]) {
    let len = signal.len().min(overlap.len());
    if len == 0 {
        return;
    }
    let signal_start = signal.len() - len;
    let overlap_start = overlap.len() - len;
    for index in 0..len {
        let window_index = overlap_start + index;
        signal[signal_start + index] = blend_expand_overlap_sample(
            signal[signal_start + index],
            overlap[overlap_start + index],
            window_index,
        );
    }
}

#[derive(Debug)]
pub(crate) struct NetEqConcealment {
    background_noise: BackgroundNoise,
    random_vector: RandomVector,
    expand: Expand,
}

impl NetEqConcealment {
    pub(crate) fn new() -> Self {
        Self {
            background_noise: BackgroundNoise::new(),
            random_vector: RandomVector::new(),
            expand: Expand::new(),
        }
    }

    pub(crate) fn reset(&mut self) {
        self.background_noise.reset();
        self.random_vector.reset();
        self.expand.reset();
    }

    pub(crate) fn update_background_noise(&mut self, decoded: &[f32]) {
        self.background_noise.update(decoded);
    }

    pub(crate) fn expand(&mut self, history: &[f32]) -> Vec<f32> {
        self.expand_chunk(history).samples
    }

    pub(crate) fn expand_chunk(&mut self, history: &[f32]) -> ConcealmentChunk {
        self.expand
            .process(history, &mut self.background_noise, &mut self.random_vector)
    }

    pub(crate) fn expand_to_len(&mut self, history: &[f32], len: usize) -> Vec<f32> {
        self.expand_to_len_chunk(history, len).samples
    }

    pub(crate) fn expand_to_len_chunk(&mut self, history: &[f32], len: usize) -> ConcealmentChunk {
        let mut local_history = Vec::with_capacity(history.len().saturating_add(len));
        local_history.extend_from_slice(history);
        let mut first_overlap = Vec::new();
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let generated = self.expand.process(
                &local_history,
                &mut self.background_noise,
                &mut self.random_vector,
            );
            if generated.samples.is_empty() {
                break;
            }
            if first_overlap.is_empty() {
                first_overlap.extend_from_slice(&generated.overlap);
            }
            blend_expand_overlap_tail(&mut local_history, &generated.overlap);
            blend_expand_overlap_tail(&mut out, &generated.overlap);
            let take = (len - out.len()).min(generated.samples.len());
            out.extend_from_slice(&generated.samples[..take]);
            local_history.extend_from_slice(&generated.samples);
        }
        ConcealmentChunk {
            overlap: first_overlap,
            samples: out,
        }
    }

    pub(crate) fn normal_after_expand(&mut self, history: &[f32], decoded: &mut [f32]) {
        if decoded.is_empty() {
            return;
        }
        self.expand.set_parameters_for_normal_after_expand();
        let expanded = self.expand_to_len(history, decoded.len().max(LIVE_OPUS_FRAME_SAMPLES));
        let mute_factor = self.expand.mute_factor();
        let local_mute_factor = self
            .background_noise
            .normal_mute_factor(decoded, 64 * FS_MULT);
        let mut gain = mute_factor.max(local_mute_factor).min(1.0);
        let gain_inc = ((1.0 - gain) / decoded.len().max(1) as f32).max(0.004 / FS_MULT as f32);
        for sample in decoded.iter_mut() {
            *sample *= gain;
            gain = (gain + gain_inc).min(1.0);
        }
        crossfade_prefix(&expanded, decoded, NORMAL_CROSSFADE_LENGTH);
        self.expand.reset();
    }

    pub(crate) fn merge_after_expand(&mut self, history: &[f32], decoded: &mut Vec<f32>) {
        if decoded.is_empty() {
            return;
        }
        self.expand.set_parameters_for_merge_after_expand();
        let mut expanded = self.expand_to_len(history, MERGE_REQUIRED_LENGTH);
        if expanded.len() < OVERLAP_LENGTH {
            self.normal_after_expand(history, decoded);
            return;
        }
        if expanded.len() < MERGE_REQUIRED_LENGTH {
            let last = expanded.last().copied().unwrap_or(0.0);
            expanded.resize(MERGE_REQUIRED_LENGTH, last);
        }

        let offset = merge_correlation_offset(
            decoded,
            &expanded,
            self.expand.max_lag().max(TIME_SCALE_MIN_LAG_48K),
        );
        let interpolation_len = MERGE_CORRELATION_LENGTH
            .min(decoded.len())
            .min(expanded.len().saturating_sub(offset))
            .max(1);
        let mut gain = self
            .expand
            .mute_factor()
            .max(signal_scaling(decoded, &expanded[offset..]))
            .min(1.0);
        let gain_inc = ((1.0 - gain) / decoded.len().max(1) as f32).max(0.004 / FS_MULT as f32);
        for sample in decoded.iter_mut() {
            *sample *= gain;
            gain = (gain + gain_inc).min(1.0);
        }

        let mut merged = Vec::with_capacity(offset + decoded.len());
        merged.extend_from_slice(&expanded[..offset]);
        for index in 0..interpolation_len {
            let t = (index + 1) as f32 / (interpolation_len + 1) as f32;
            merged.push((1.0 - t) * expanded[offset + index] + t * decoded[index]);
        }
        merged.extend_from_slice(&decoded[interpolation_len..]);
        *decoded = merged;
        self.expand.reset();
    }

    pub(crate) fn muted(&self) -> bool {
        self.expand.muted()
    }
}

#[derive(Debug)]
struct Expand {
    first_expand: bool,
    consecutive_expands: usize,
    max_lag: usize,
    expand_lags: [usize; 3],
    lag_index_direction: isize,
    current_lag_index: isize,
    stop_muting: bool,
    expand_duration_samples: usize,
    params: ExpandParameters,
}

impl Expand {
    fn new() -> Self {
        Self {
            first_expand: true,
            consecutive_expands: 0,
            max_lag: TIME_SCALE_MIN_LAG_48K,
            expand_lags: [TIME_SCALE_MIN_LAG_48K; 3],
            lag_index_direction: 0,
            current_lag_index: 0,
            stop_muting: false,
            expand_duration_samples: 0,
            params: ExpandParameters::new(),
        }
    }

    fn reset(&mut self) {
        self.first_expand = true;
        self.consecutive_expands = 0;
        self.max_lag = TIME_SCALE_MIN_LAG_48K;
        self.expand_lags = [TIME_SCALE_MIN_LAG_48K; 3];
        self.lag_index_direction = 0;
        self.current_lag_index = 0;
        self.stop_muting = false;
        self.expand_duration_samples = 0;
        self.params.expand_vector0.clear();
        self.params.expand_vector1.clear();
    }

    fn process(
        &mut self,
        history: &[f32],
        background_noise: &mut BackgroundNoise,
        random_vector: &mut RandomVector,
    ) -> ConcealmentChunk {
        if self.first_expand {
            self.analyze_signal(history, background_noise, random_vector);
            self.first_expand = false;
            self.expand_duration_samples = 0;
        } else {
            random_vector.increase_seed_increment(2);
        }

        self.update_lag_index();
        let lag = self.expand_lags[self.current_lag_index as usize]
            .clamp(TIME_SCALE_MIN_LAG_48K, TIME_SCALE_MAX_LAG_48K);
        let voiced_storage = self.voiced_vector_with_overlap(lag);
        let voiced = &voiced_storage[OVERLAP_LENGTH..];
        let overlap = self.expand_overlap(&voiced_storage);
        let unvoiced = self.unvoiced_vector(lag, random_vector);
        let mut output = self.mix_voice_and_noise(voiced, &unvoiced);
        self.apply_progressive_mute(&mut output);
        let bgn = background_noise.generate(random_vector, self.params.mute_slope, lag);
        for (sample, noise) in output.iter_mut().zip(bgn) {
            *sample = (*sample + noise).clamp(-1.0, 1.0);
        }
        self.consecutive_expands = self
            .consecutive_expands
            .saturating_add(1)
            .min(MAX_CONSECUTIVE_EXPANDS);
        self.expand_duration_samples = self
            .expand_duration_samples
            .saturating_add(output.len())
            .min(SAMPLE_RATE as usize * 2);
        ConcealmentChunk {
            overlap,
            samples: output,
        }
    }

    fn analyze_signal(
        &mut self,
        history: &[f32],
        background_noise: &mut BackgroundNoise,
        random_vector: &mut RandomVector,
    ) {
        self.initialize_for_expand_period(background_noise);
        let signal = tail_padded(history, SIGNAL_LENGTH);
        let pitch = pitch_analysis(&signal);
        self.max_lag = pitch.distortion_lag.max(pitch.correlation_lag);
        self.expand_lags = if pitch.distortion_lag == pitch.correlation_lag {
            [pitch.distortion_lag; 3]
        } else {
            [
                pitch.distortion_lag,
                (pitch.distortion_lag + pitch.correlation_lag) / 2,
                if pitch.distortion_lag > pitch.correlation_lag {
                    (pitch.distortion_lag + pitch.correlation_lag - 1) / 2
                } else {
                    (pitch.distortion_lag + pitch.correlation_lag + 1) / 2
                },
            ]
        };

        let expansion_length = self.max_lag + OVERLAP_LENGTH;
        self.params.expand_vector0 = tail_padded(&signal, expansion_length);
        self.params.expand_vector1 =
            lagged_tail_padded(&signal, expansion_length, pitch.distortion_lag);
        let energy0 = dot(&self.params.expand_vector0, &self.params.expand_vector0);
        let energy1 = dot(&self.params.expand_vector1, &self.params.expand_vector1);
        let amplitude_ratio = if energy0 / 4.0 < energy1 && energy0 > energy1 / 4.0 && energy1 > 0.0
        {
            let ratio = (energy0 / energy1).sqrt().clamp(0.5, 2.0);
            for sample in &mut self.params.expand_vector1 {
                *sample *= ratio;
            }
            ratio
        } else {
            self.params
                .expand_vector1
                .clone_from(&self.params.expand_vector0);
            if energy0 / 4.0 < energy1 || energy1 == 0.0 {
                0.5
            } else {
                2.0
            }
        };

        self.params.ar_filter = lpc_filter(
            &signal[SIGNAL_LENGTH - LPC_ANALYSIS_LENGTH..],
            UNVOICED_LPC_ORDER,
        )
        .unwrap_or_else(|| identity_filter(UNVOICED_LPC_ORDER));
        self.params.ar_state = tail_padded(&signal, UNVOICED_LPC_ORDER);
        let residual = residual_signal(&signal[SIGNAL_LENGTH - 128..], &self.params.ar_filter);
        self.params.ar_gain = rms(&residual).max(background_noise.residual_rms());
        self.params.voice_mix_factor = voice_mix_factor(pitch.correlation);
        self.params.current_voice_mix_factor = 1.0;
        self.params
            .configure_mute_slope(amplitude_ratio, pitch.distortion_lag.max(1));

        let noise_len = if pitch.distortion_lag < 40 {
            2 * pitch.distortion_lag + EXPAND_RANDOM_EXTRA
        } else {
            pitch.distortion_lag + EXPAND_RANDOM_EXTRA
        };
        random_vector.seed_from_table(noise_len);
    }

    fn initialize_for_expand_period(&mut self, background_noise: &mut BackgroundNoise) {
        self.lag_index_direction = 1;
        self.current_lag_index = -1;
        self.stop_muting = false;
        self.consecutive_expands = 0;
        self.params.current_voice_mix_factor = 1.0;
        self.params.mute_factor = 1.0;
        background_noise.set_mute_factor(0.0);
    }

    fn set_parameters_for_normal_after_expand(&mut self) {
        self.current_lag_index = 0;
        self.lag_index_direction = 0;
        self.stop_muting = true;
    }

    fn set_parameters_for_merge_after_expand(&mut self) {
        self.current_lag_index = -1;
        self.lag_index_direction = 1;
        self.stop_muting = true;
    }

    fn update_lag_index(&mut self) {
        self.current_lag_index += self.lag_index_direction;
        if self.current_lag_index <= 0 {
            self.current_lag_index = 0;
            self.lag_index_direction = 1;
        }
        if self.current_lag_index >= 2 {
            self.current_lag_index = 2;
            self.lag_index_direction = -1;
        }
    }

    fn voiced_vector_with_overlap(&self, lag: usize) -> Vec<f32> {
        let expansion_vector_length = self.max_lag + OVERLAP_LENGTH;
        let position = expansion_vector_length.saturating_sub(lag + OVERLAP_LENGTH);
        let len = lag + OVERLAP_LENGTH;
        let mut out = Vec::with_capacity(len);
        for index in 0..len {
            let source_index = position + index;
            let v0 = sample_at_wrapped(&self.params.expand_vector0, source_index);
            let sample = match self.current_lag_index {
                0 => v0,
                1 => {
                    0.75 * v0 + 0.25 * sample_at_wrapped(&self.params.expand_vector1, source_index)
                }
                _ => 0.5 * v0 + 0.5 * sample_at_wrapped(&self.params.expand_vector1, source_index),
            };
            out.push(sample);
        }
        out
    }

    fn unvoiced_vector(&mut self, len: usize, random_vector: &mut RandomVector) -> Vec<f32> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            let excitation = random_vector.next_sample() * self.params.ar_gain;
            let mut sample = excitation;
            for order in 1..self.params.ar_filter.len() {
                sample -= self.params.ar_filter[order] * self.params.ar_state[order - 1];
            }
            shift_filter_state(&mut self.params.ar_state, sample);
            out.push(sample);
        }
        out
    }

    fn mix_voice_and_noise(&mut self, voiced: &[f32], unvoiced: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(voiced.len().min(unvoiced.len()));
        let temp_shift = bit_width(self.max_lag).saturating_sub(5);
        let mut mix_increment = (256 >> temp_shift.min(8)) as f32 / 16_384.0;
        if self.stop_muting {
            mix_increment = 0.0;
        }
        for (&voice, &noise) in voiced.iter().zip(unvoiced) {
            let mix = self.params.current_voice_mix_factor;
            out.push(mix * voice + (1.0 - mix) * noise);
            if mix_increment > 0.0 {
                self.params.current_voice_mix_factor = (self.params.current_voice_mix_factor
                    - mix_increment)
                    .max(self.params.voice_mix_factor);
            }
        }
        out
    }

    fn expand_overlap(&self, voiced_storage: &[f32]) -> Vec<f32> {
        if self.params.mute_factor <= 0.05 || self.params.current_voice_mix_factor <= 0.5 {
            return Vec::new();
        };
        voiced_storage
            .iter()
            .take(OVERLAP_LENGTH)
            .map(|sample| self.params.mute_factor * *sample)
            .collect()
    }

    fn apply_progressive_mute(&mut self, output: &mut [f32]) {
        if self.consecutive_expands == 3 {
            self.params.mute_slope = self.params.mute_slope.max(0.0010 / FS_MULT as f32);
        }
        if self.consecutive_expands == 7 {
            self.params.mute_slope = self.params.mute_slope.max(0.0020 / FS_MULT as f32);
        }
        if self.consecutive_expands == 0 && self.params.onset {
            return;
        }
        let mut factor = self.params.mute_factor;
        for sample in output.iter_mut() {
            *sample *= factor;
            if !self.stop_muting {
                factor = (factor - self.params.mute_slope).max(0.0);
            }
        }
        if !self.stop_muting {
            if self.consecutive_expands > 3 && factor >= self.params.mute_factor {
                self.params.mute_factor = 0.0;
            } else {
                self.params.mute_factor = factor;
            }
        }
    }

    fn muted(&self) -> bool {
        !self.first_expand && !self.stop_muting && self.params.mute_factor <= 0.0
    }

    fn mute_factor(&self) -> f32 {
        self.params.mute_factor
    }

    fn max_lag(&self) -> usize {
        self.max_lag
    }
}

#[derive(Debug)]
struct ExpandParameters {
    mute_factor: f32,
    ar_filter: Vec<f32>,
    ar_state: Vec<f32>,
    ar_gain: f32,
    voice_mix_factor: f32,
    current_voice_mix_factor: f32,
    expand_vector0: Vec<f32>,
    expand_vector1: Vec<f32>,
    onset: bool,
    mute_slope: f32,
}

impl ExpandParameters {
    fn new() -> Self {
        Self {
            mute_factor: 1.0,
            ar_filter: identity_filter(UNVOICED_LPC_ORDER),
            ar_state: vec![0.0; UNVOICED_LPC_ORDER],
            ar_gain: 0.0,
            voice_mix_factor: 0.0,
            current_voice_mix_factor: 1.0,
            expand_vector0: Vec::new(),
            expand_vector1: Vec::new(),
            onset: false,
            mute_slope: 0.0,
        }
    }

    fn configure_mute_slope(&mut self, amplitude_ratio: f32, distortion_lag: usize) {
        if amplitude_ratio > 1.5 {
            let ratio = (amplitude_ratio - 1.0) / (distortion_lag as f32 * amplitude_ratio);
            self.mute_slope = if amplitude_ratio > 1.8 {
                ratio / 2.0
            } else {
                ratio / 8.0
            };
            self.onset = true;
        } else {
            self.mute_slope = ((1.0 - amplitude_ratio).max(0.0) / distortion_lag as f32).max(0.0);
            if self.voice_mix_factor <= 0.8 {
                self.mute_slope = self.mute_slope.max(0.005 / FS_MULT as f32);
            } else if amplitude_ratio > 0.98 {
                self.mute_slope = 0.0;
            }
            self.onset = false;
        }
    }
}

#[derive(Debug)]
struct BackgroundNoise {
    initialized: bool,
    energy: f32,
    max_energy: f32,
    energy_update_threshold: f32,
    low_energy_update_threshold: f32,
    filter: Vec<f32>,
    filter_state: Vec<f32>,
    residual_rms: f32,
    mute_factor: f32,
}

impl BackgroundNoise {
    fn new() -> Self {
        Self {
            initialized: false,
            energy: 2_500.0 / (SAMPLE_SCALE * SAMPLE_SCALE),
            max_energy: 0.0,
            energy_update_threshold: 500_000.0 / (SAMPLE_SCALE * SAMPLE_SCALE),
            low_energy_update_threshold: 0.0,
            filter: identity_filter(BACKGROUND_LPC_ORDER),
            filter_state: vec![0.0; BACKGROUND_LPC_ORDER],
            residual_rms: 20_000.0 / SAMPLE_SCALE,
            mute_factor: 0.0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn update(&mut self, samples: &[f32]) -> bool {
        if samples.len() < BACKGROUND_VEC_LEN {
            return false;
        }
        let signal = &samples[samples.len() - BACKGROUND_VEC_LEN..];
        let sample_energy = mean_square(signal);
        if sample_energy >= self.energy_update_threshold {
            self.increment_energy_threshold(sample_energy);
            return false;
        }
        let Some(filter) = lpc_filter(signal, BACKGROUND_LPC_ORDER) else {
            return false;
        };
        let residual = residual_signal(&signal[signal.len() - BACKGROUND_RESIDUAL_LEN..], &filter);
        let residual_energy = dot(&residual, &residual) / BACKGROUND_RESIDUAL_LEN as f32;
        if sample_energy > 0.0 && 5.0 * residual_energy >= 16.0 * sample_energy {
            self.filter = filter;
            self.filter_state = tail_padded(signal, BACKGROUND_LPC_ORDER);
            self.energy = sample_energy.max(MIN_ENERGY);
            self.energy_update_threshold = self.energy;
            self.low_energy_update_threshold = 0.0;
            self.residual_rms = residual_energy.sqrt().max(1.0 / SAMPLE_SCALE);
            self.initialized = true;
            true
        } else {
            self.energy_update_threshold = sample_energy.max(MIN_ENERGY);
            self.low_energy_update_threshold = 0.0;
            false
        }
    }

    fn increment_energy_threshold(&mut self, sample_energy: f32) {
        const THRESHOLD_INCREMENT: f32 = 229.0 / 65_536.0;
        self.low_energy_update_threshold += THRESHOLD_INCREMENT * self.energy_update_threshold;
        self.energy_update_threshold += THRESHOLD_INCREMENT * self.energy_update_threshold;
        self.max_energy -= self.max_energy / 1024.0;
        self.max_energy = self.max_energy.max(sample_energy);
        self.energy_update_threshold = self
            .energy_update_threshold
            .max(self.max_energy / 1_048_576.0);
    }

    fn generate(
        &mut self,
        random_vector: &mut RandomVector,
        _mute_slope: f32,
        num_noise_samples: usize,
    ) -> Vec<f32> {
        if !self.initialized {
            return vec![0.0; num_noise_samples];
        }
        let mut out = Vec::with_capacity(num_noise_samples);
        for _ in 0..num_noise_samples {
            let excitation = random_vector.next_sample() * self.residual_rms;
            let mut sample = excitation;
            for order in 1..self.filter.len() {
                sample -= self.filter[order] * self.filter_state[order - 1];
            }
            shift_filter_state(&mut self.filter_state, sample);
            out.push(sample * self.mute_factor);
            self.mute_factor = (self.mute_factor + 0.004 / FS_MULT as f32).min(1.0);
        }
        out
    }

    fn normal_mute_factor(&self, decoded: &[f32], energy_len: usize) -> f32 {
        let energy_len = energy_len.min(decoded.len()).max(1);
        let energy = mean_square(&decoded[..energy_len]);
        if energy > self.energy && energy > 0.0 {
            (self.energy / energy).sqrt().clamp(0.0, 1.0)
        } else {
            1.0
        }
    }

    fn residual_rms(&self) -> f32 {
        if self.initialized {
            self.residual_rms
        } else {
            1.0 / SAMPLE_SCALE
        }
    }

    fn set_mute_factor(&mut self, factor: f32) {
        self.mute_factor = factor.clamp(0.0, 1.0);
    }
}

#[derive(Clone, Copy, Debug)]
struct PitchAnalysis {
    distortion_lag: usize,
    correlation_lag: usize,
    correlation: f32,
}

fn pitch_analysis(signal: &[f32]) -> PitchAnalysis {
    let max_lag = TIME_SCALE_MAX_LAG_48K.min(signal.len() / 2);
    let min_lag = TIME_SCALE_MIN_LAG_48K.min(max_lag);
    if max_lag < min_lag || min_lag == 0 {
        return PitchAnalysis {
            distortion_lag: TIME_SCALE_MIN_LAG_48K,
            correlation_lag: TIME_SCALE_MIN_LAG_48K,
            correlation: 0.0,
        };
    }
    let corr_len = (60 * FS_MULT).min(signal.len().saturating_sub(max_lag));
    let corr_ref_start = signal.len().saturating_sub(corr_len);
    let corr_ref = &signal[corr_ref_start..corr_ref_start + corr_len];
    let distortion_ref = &signal[signal.len().saturating_sub(DISTORTION_LENGTH)..];
    let mut best_corr = f32::MIN;
    let mut correlation_lag = min_lag;
    let mut best_distortion = f32::MAX;
    let mut distortion_lag = min_lag;
    for lag in min_lag..=max_lag {
        if corr_ref_start < lag {
            continue;
        }
        let corr_cmp = &signal[corr_ref_start - lag..corr_ref_start - lag + corr_len];
        let corr = normalized_correlation(corr_ref, corr_cmp);
        if corr > best_corr {
            best_corr = corr;
            correlation_lag = lag;
        }
        let dist_cmp_start = signal.len().saturating_sub(DISTORTION_LENGTH + lag);
        let dist_cmp = &signal[dist_cmp_start..dist_cmp_start + DISTORTION_LENGTH];
        let distortion = distortion_ref
            .iter()
            .zip(dist_cmp)
            .map(|(left, right)| (left - right).abs())
            .sum::<f32>();
        if distortion < best_distortion {
            best_distortion = distortion;
            distortion_lag = lag;
        }
    }
    let start = distortion_lag.min(correlation_lag);
    let end = distortion_lag.max(correlation_lag).min(max_lag);
    let mut exact_lag = correlation_lag;
    let mut exact_corr = best_corr.max(0.0);
    for lag in start..=end {
        if corr_ref_start < lag {
            continue;
        }
        let corr_cmp = &signal[corr_ref_start - lag..corr_ref_start - lag + corr_len];
        let corr = normalized_correlation(corr_ref, corr_cmp);
        if corr > exact_corr {
            exact_corr = corr;
            exact_lag = lag;
        }
    }
    PitchAnalysis {
        distortion_lag,
        correlation_lag: exact_lag,
        correlation: exact_corr.clamp(0.0, 1.0),
    }
}

fn merge_correlation_offset(input: &[f32], expanded: &[f32], max_lag: usize) -> usize {
    let length = (40 * FS_MULT).min(input.len()).min(expanded.len()).max(1);
    let stop = (max_lag / 2 + 1)
        .min(MERGE_CORRELATION_LENGTH)
        .min(expanded.len().saturating_sub(length));
    let mut best = 0usize;
    let mut best_corr = f32::MIN;
    for offset in 0..=stop {
        let corr = normalized_correlation(&input[..length], &expanded[offset..offset + length]);
        if corr > best_corr {
            best_corr = corr;
            best = offset;
        }
    }
    let min_start = LIVE_OPUS_FRAME_SAMPLES + OVERLAP_LENGTH;
    while best + input.len() < min_start {
        best = best.saturating_add(max_lag.max(1));
        if best >= expanded.len().saturating_sub(OVERLAP_LENGTH) {
            return 0;
        }
    }
    best.min(expanded.len().saturating_sub(OVERLAP_LENGTH))
}

fn signal_scaling(input: &[f32], expanded_signal: &[f32]) -> f32 {
    let len = (64 * FS_MULT)
        .min(input.len())
        .min(expanded_signal.len())
        .max(1);
    let input_energy = mean_square(&input[..len]);
    let expanded_energy = mean_square(&expanded_signal[..len]);
    if input_energy > expanded_energy && input_energy > 0.0 {
        (expanded_energy / input_energy).sqrt().clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn crossfade_prefix(from: &[f32], to: &mut [f32], win_length: usize) {
    let len = win_length.min(from.len()).min(to.len());
    if len == 0 {
        return;
    }
    for index in 0..len {
        let up = (index + 1) as f32 / len as f32;
        to[index] = up * to[index] + (1.0 - up) * from[index];
    }
}

fn voice_mix_factor(correlation: f32) -> f32 {
    if correlation <= 0.48 {
        return 0.0;
    }
    let x = correlation;
    ((-5179.0 + 19931.0 * x - 16422.0 * x * x + 5776.0 * x * x * x) / 4096.0).clamp(0.0, 1.0)
}

fn lpc_filter(signal: &[f32], order: usize) -> Option<Vec<f32>> {
    if signal.len() <= order {
        return None;
    }
    let mut autocorr = vec![0.0f32; order + 1];
    for lag in 0..=order {
        let mut sum = 0.0;
        for index in lag..signal.len() {
            sum += signal[index] * signal[index - lag];
        }
        autocorr[lag] = sum;
    }
    levinson_durbin(&autocorr, order)
}

fn levinson_durbin(autocorr: &[f32], order: usize) -> Option<Vec<f32>> {
    if autocorr.first().copied().unwrap_or(0.0) <= 0.0 {
        return None;
    }
    let mut coeff = vec![0.0f32; order + 1];
    let mut prev = vec![0.0f32; order + 1];
    coeff[0] = 1.0;
    let mut error = autocorr[0];
    for index in 1..=order {
        let mut acc = autocorr[index];
        for j in 1..index {
            acc += coeff[j] * autocorr[index - j];
        }
        let reflection = -acc / error.max(1.0e-12);
        if !reflection.is_finite() || reflection.abs() >= 1.0 {
            return None;
        }
        prev.copy_from_slice(&coeff);
        coeff[index] = reflection;
        for j in 1..index {
            coeff[j] = prev[j] + reflection * prev[index - j];
        }
        error *= 1.0 - reflection * reflection;
        if error <= 1.0e-12 {
            return None;
        }
    }
    Some(coeff)
}

fn residual_signal(signal: &[f32], filter: &[f32]) -> Vec<f32> {
    let mut out = Vec::with_capacity(signal.len());
    for index in 0..signal.len() {
        let mut sample = signal[index];
        for order in 1..filter.len() {
            if index >= order {
                sample += filter[order] * signal[index - order];
            }
        }
        out.push(sample);
    }
    out
}

fn normalized_correlation(left: &[f32], right: &[f32]) -> f32 {
    let len = left.len().min(right.len());
    if len == 0 {
        return 0.0;
    }
    let mut cross = 0.0;
    let mut left_energy = 0.0;
    let mut right_energy = 0.0;
    for index in 0..len {
        cross += left[index] * right[index];
        left_energy += left[index] * left[index];
        right_energy += right[index] * right[index];
    }
    (cross.max(0.0) / (left_energy * right_energy).sqrt().max(1.0e-12)).clamp(0.0, 1.0)
}

fn identity_filter(order: usize) -> Vec<f32> {
    let mut filter = vec![0.0; order + 1];
    filter[0] = 1.0;
    filter
}

fn tail_padded(signal: &[f32], len: usize) -> Vec<f32> {
    if signal.len() >= len {
        signal[signal.len() - len..].to_vec()
    } else {
        let mut out = vec![0.0; len - signal.len()];
        out.extend_from_slice(signal);
        out
    }
}

fn lagged_tail_padded(signal: &[f32], len: usize, lag: usize) -> Vec<f32> {
    if signal.len() >= len.saturating_add(lag) {
        signal[signal.len() - len - lag..signal.len() - lag].to_vec()
    } else {
        tail_padded(signal, len)
    }
}

fn sample_at_wrapped(signal: &[f32], index: usize) -> f32 {
    if signal.is_empty() {
        0.0
    } else {
        signal[index % signal.len()]
    }
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}

fn mean_square(signal: &[f32]) -> f32 {
    if signal.is_empty() {
        return 0.0;
    }
    dot(signal, signal) / signal.len() as f32
}

fn rms(signal: &[f32]) -> f32 {
    mean_square(signal).sqrt()
}

fn shift_filter_state(state: &mut [f32], sample: f32) {
    for index in (1..state.len()).rev() {
        state[index] = state[index - 1];
    }
    if let Some(first) = state.first_mut() {
        *first = sample;
    }
}

fn bit_width(value: usize) -> usize {
    usize::BITS as usize - value.max(1).leading_zeros() as usize
}

#[derive(Debug)]
struct RandomVector {
    seed: u32,
    seed_increment: u32,
    table_cursor: usize,
}

impl RandomVector {
    fn new() -> Self {
        Self {
            seed: 777,
            seed_increment: 1,
            table_cursor: 0,
        }
    }

    fn reset(&mut self) {
        self.seed = 777;
        self.seed_increment = 1;
        self.table_cursor = 0;
    }

    fn seed_from_table(&mut self, len: usize) {
        self.table_cursor = len.min(RANDOM_TABLE.len());
    }

    fn increase_seed_increment(&mut self, increase_by: u32) {
        self.seed_increment = (self.seed_increment + increase_by) & 255;
    }

    fn next_sample(&mut self) -> f32 {
        if self.table_cursor < RANDOM_TABLE.len() {
            let sample = RANDOM_TABLE[self.table_cursor] as f32 / SAMPLE_SCALE;
            self.table_cursor += 1;
            return sample;
        }
        self.seed = self.seed.wrapping_add(self.seed_increment);
        let position = (self.seed & 255) as usize;
        RANDOM_TABLE[position] as f32 / SAMPLE_SCALE
    }
}

const RANDOM_TABLE: [i16; 256] = [
    2680, 5532, 441, 5520, 16170, -5146, -1024, -8733, 3115, 9598, -10380, -4959, -1280, -21716,
    7133, -1522, 13458, -3902, 2789, -675, 3441, 5016, -13599, -4003, -2739, 3922, -7209, 13352,
    -11617, -7241, 12905, -2314, 5426, 10121, -9702, 11207, -13542, 1373, 816, -5934, -12504, 4798,
    1811, 4112, -613, 201, -10367, -2960, -2419, 3442, 4299, -6116, -6092, 1552, -1650, -480,
    -1237, 18720, -11858, -8303, -8212, 865, -2890, -16968, 12052, -5845, -5912, 9777, -5665,
    -6294, 5426, -4737, -6335, 1652, 761, 3832, 641, -8552, -9084, -5753, 8146, 12156, -4915,
    15086, -1231, -1869, 11749, -9319, -6403, 11407, 6232, -1683, 24340, -11166, 4017, -10448,
    3153, -2936, 6212, 2891, -866, -404, -4807, -2324, -1917, -2388, -6470, -3895, -10300, 5323,
    -5403, 2205, 4640, 7022, -21186, -6244, -882, -10031, -3395, -12885, 7155, -5339, 5079, -2645,
    -9515, 6622, 14651, 15852, 359, 122, 8246, -3502, -6696, -3679, -13535, -1409, -704, -7403,
    -4007, 1798, 279, -420, -12796, -14219, 1141, 3359, 11434, 7049, -6684, -7473, 14283, -4115,
    -9123, -8969, 4152, 4117, 13792, 5742, 16168, 8661, -1609, -6095, 1881, 14380, -5588, 6758,
    -6425, -22969, -7269, 7031, 1119, -1611, -5850, -11281, 3559, -8952, -10146, -4667, -16251,
    -1538, 2062, -1012, -13073, 227, -3142, -5265, 20, 5770, -7559, 4740, -4819, 992, -8208, -7130,
    -4652, 6725, 7369, -1036, 13144, -1588, -5304, -2344, -449, -5705, -8894, 5205, -17904, -11188,
    -1022, 4852, 10101, -5255, -4200, -752, 7941, -1543, 5959, 14719, 13346, 17045, -15605, -1678,
    -1600, -9230, 68, 23348, 1172, 7750, 11212, -18227, 9956, 4161, 883, 3947, 4341, 1014, -4889,
    -2603, 1246, -5630, -3596, -870, -1298, 2784, -3317, -6612, -20541, 4166, 4181, -8625, 3562,
    12890, 4761, 3205, -12259, -8579,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|index| {
                (2.0 * std::f32::consts::PI * freq * index as f32 / SAMPLE_RATE as f32).sin() * 0.4
            })
            .collect()
    }

    #[test]
    fn expand_produces_progressively_muted_output() {
        let mut concealment = NetEqConcealment::new();
        let mut history = sine(200.0, SIGNAL_LENGTH);
        let first = concealment.expand(&history);
        history.extend_from_slice(&first);
        let first_rms = rms(&first);
        let mut last_rms = first_rms;
        for _ in 0..20 {
            let out = concealment.expand(&history);
            history.extend_from_slice(&out);
            last_rms = rms(&out);
        }
        assert!(first.len() >= TIME_SCALE_MIN_LAG_48K);
        assert!(last_rms < first_rms, "first={first_rms} last={last_rms}");
    }

    #[test]
    fn normal_after_expand_crossfades_first_millisecond() {
        let mut concealment = NetEqConcealment::new();
        let history = sine(200.0, SIGNAL_LENGTH);
        let _ = concealment.expand(&history);
        let mut decoded = sine(200.0, LIVE_OPUS_FRAME_SAMPLES);
        let before = decoded[0];
        concealment.normal_after_expand(&history, &mut decoded);
        assert_ne!(decoded[0], before);
    }

    #[test]
    fn merge_after_expand_returns_recovered_audio_plus_overlap() {
        let mut concealment = NetEqConcealment::new();
        let history = sine(200.0, SIGNAL_LENGTH);
        let _ = concealment.expand(&history);
        let mut decoded = sine(200.0, LIVE_OPUS_FRAME_SAMPLES);
        concealment.merge_after_expand(&history, &mut decoded);
        assert!(decoded.len() >= LIVE_OPUS_FRAME_SAMPLES);
    }
}
