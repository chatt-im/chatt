//! Port of WebRTC neteq `Expand`: the packet-loss concealment generator. It
//! analyzes the decoded history once per concealment run to estimate pitch, an
//! LPC residual model, and a voiced/unvoiced mix, then extrapolates 10 ms at a
//! time with progressive muting and comfort-noise fill.
//!
//! Mono only (Chatt is single channel). The `sync_buffer` is a flat `&mut [i16]`
//! the operation both reads history from and overlap-adds into, exactly as the
//! reference `SyncBuffer`. Arithmetic matches the reference bit for bit.

use super::background_noise::BackgroundNoise;
use super::dsp_helper;
use super::random_vector::{RandomVector, RANDOM_TABLE};
use super::spl;

const FS_HZ: i32 = 48000;
const FS_MULT: usize = 6;
const OVERLAP_LENGTH: usize = 5 * FS_MULT; // 30
const UNVOICED_LPC_ORDER: usize = 6;
const NOISE_LPC_ORDER: usize = 8; // BackgroundNoise::kMaxLpcOrder
const NUM_CORRELATION_CANDIDATES: usize = 3;
const DISTORTION_LENGTH: usize = 20;
const LPC_ANALYSIS_LENGTH: usize = 160;
const MAX_CONSECUTIVE_EXPANDS: i32 = 200;
const NUM_LAGS: i32 = 3;
const SIGNAL_LENGTH: usize = 256 * FS_MULT; // 1536

/// Per-channel `Expand` state.
#[derive(Debug)]
struct ChannelParameters {
    expand_vector0: Vec<i16>,
    expand_vector1: Vec<i16>,
    ar_filter: [i16; UNVOICED_LPC_ORDER + 1],
    ar_filter_state: [i16; UNVOICED_LPC_ORDER],
    mute_factor: i16,
    ar_gain: i16,
    ar_gain_scale: i16,
    voice_mix_factor: i16,
    current_voice_mix_factor: i16,
    onset: bool,
    mute_slope: i32,
}

impl ChannelParameters {
    fn new() -> Self {
        Self {
            expand_vector0: Vec::new(),
            expand_vector1: Vec::new(),
            ar_filter: [0; UNVOICED_LPC_ORDER + 1],
            ar_filter_state: [0; UNVOICED_LPC_ORDER],
            mute_factor: 16384,
            ar_gain: 0,
            ar_gain_scale: 0,
            voice_mix_factor: 0,
            current_voice_mix_factor: 0,
            onset: false,
            mute_slope: 0,
        }
    }
}

/// `Expand` for a single channel at 48 kHz.
#[derive(Debug)]
pub(crate) struct Expand {
    first_expand: bool,
    consecutive_expands: i32,
    max_lag: usize,
    expand_lags: [usize; 3],
    lag_index_direction: i32,
    current_lag_index: i32,
    stop_muting: bool,
    expand_duration_samples: usize,
    params: ChannelParameters,
}

impl Expand {
    pub(crate) fn new() -> Self {
        let mut e = Self {
            first_expand: true,
            consecutive_expands: 0,
            max_lag: 0,
            expand_lags: [0; 3],
            lag_index_direction: 0,
            current_lag_index: 0,
            stop_muting: false,
            expand_duration_samples: 0,
            params: ChannelParameters::new(),
        };
        e.reset();
        e
    }

    pub(crate) fn reset(&mut self) {
        self.first_expand = true;
        self.consecutive_expands = 0;
        self.max_lag = 0;
        self.params.expand_vector0.clear();
        self.params.expand_vector1.clear();
    }

    pub(crate) fn set_parameters_for_normal_after_expand(&mut self) {
        self.current_lag_index = 0;
        self.lag_index_direction = 0;
        self.stop_muting = true;
    }

    pub(crate) fn set_parameters_for_merge_after_expand(&mut self) {
        self.current_lag_index = -1;
        self.lag_index_direction = 1;
        self.stop_muting = true;
    }

    pub(crate) fn muted(&self) -> bool {
        if self.first_expand || self.stop_muting {
            return false;
        }
        self.params.mute_factor == 0
    }

    pub(crate) fn overlap_length(&self) -> usize {
        OVERLAP_LENGTH
    }

    pub(crate) fn mute_factor(&self) -> i16 {
        self.params.mute_factor
    }

    pub(crate) fn max_lag(&self) -> usize {
        self.max_lag
    }

    fn too_many_expands(&self) -> bool {
        self.consecutive_expands >= MAX_CONSECUTIVE_EXPANDS
    }

    fn update_lag_index(&mut self) {
        self.current_lag_index += self.lag_index_direction;
        if self.current_lag_index <= 0 {
            self.lag_index_direction = 1;
        }
        if self.current_lag_index >= NUM_LAGS - 1 {
            self.lag_index_direction = -1;
        }
    }

    fn initialize_for_an_expand_period(
        &mut self,
        background_noise: &mut BackgroundNoise,
        random_vector: &mut RandomVector,
    ) {
        self.lag_index_direction = 1;
        self.current_lag_index = -1;
        self.stop_muting = false;
        random_vector.set_seed_increment(1);
        self.consecutive_expands = 0;
        self.params.current_voice_mix_factor = 16384;
        self.params.mute_factor = 16384;
        background_noise.set_mute_factor(0);
    }

    /// `Expand::GenerateRandomVector`.
    fn generate_random_vector(
        random_vector: &mut RandomVector,
        seed_increment: i16,
        length: usize,
        out: &mut [i16],
    ) {
        let mut samples_generated = 0;
        let max_rand = RANDOM_TABLE.len();
        while samples_generated < length {
            let rand_length = (length - samples_generated).min(max_rand);
            random_vector.increase_seed_increment(seed_increment);
            random_vector.generate(&mut out[samples_generated..samples_generated + rand_length]);
            samples_generated += rand_length;
        }
    }

    /// `Expand::Process`: produce the next concealment segment into `output`,
    /// overlap-adding the leading region into the `sync_buffer` tail.
    pub(crate) fn process(
        &mut self,
        sync_buffer: &mut [i16],
        background_noise: &mut BackgroundNoise,
        random_vector: &mut RandomVector,
        output: &mut Vec<i16>,
    ) {
        let mut rng_buffer = [0i16; 48000 / 8000 * 120 + 30]; // 750
        let mut scaled_random_vector = [0i16; 48000 / 8000 * 125]; // 750
        let mut temp_data = [0i16; 3600];
        let mut unvoiced_array_memory = [0i16; NOISE_LPC_ORDER + 48000 / 8000 * 125];

        if self.first_expand {
            self.analyze_signal(sync_buffer, background_noise, random_vector, &mut rng_buffer);
            self.first_expand = false;
            self.expand_duration_samples = 0;
        } else {
            let rand_length = self.max_lag;
            Self::generate_random_vector(random_vector, 2, rand_length, &mut rng_buffer);
        }

        self.update_lag_index();

        let expansion_vector_length = self.max_lag + OVERLAP_LENGTH;
        let current_lag = self.expand_lags[self.current_lag_index as usize];
        let expansion_vector_position = expansion_vector_length - current_lag - OVERLAP_LENGTH;
        let expansion_temp_length = current_lag + OVERLAP_LENGTH;

        // Voiced part: weighted combination of the two expansion vectors.
        let mut voiced_vector_storage = vec![0i16; expansion_temp_length];
        match self.current_lag_index {
            0 => {
                voiced_vector_storage.copy_from_slice(
                    &self.params.expand_vector0
                        [expansion_vector_position..expansion_vector_position + expansion_temp_length],
                );
            }
            1 => {
                let temp_0 = &self.params.expand_vector0
                    [expansion_vector_position..expansion_vector_position + expansion_temp_length];
                let temp_1 = &self.params.expand_vector1
                    [expansion_vector_position..expansion_vector_position + expansion_temp_length];
                spl::scale_and_add_vectors_with_round(
                    temp_0,
                    3,
                    temp_1,
                    1,
                    2,
                    &mut voiced_vector_storage,
                    expansion_temp_length,
                );
            }
            _ => {
                let temp_0 = &self.params.expand_vector0
                    [expansion_vector_position..expansion_vector_position + expansion_temp_length];
                let temp_1 = &self.params.expand_vector1
                    [expansion_vector_position..expansion_vector_position + expansion_temp_length];
                spl::scale_and_add_vectors_with_round(
                    temp_0,
                    1,
                    temp_1,
                    1,
                    1,
                    &mut voiced_vector_storage,
                    expansion_temp_length,
                );
            }
        }
        // voiced_vector points past the overlap region.
        // (Indices into voiced_vector_storage are offset by OVERLAP_LENGTH.)

        // Overlap-add the leading region into the sync buffer tail (Q15 window).
        if self.params.mute_factor > 819 && self.params.current_voice_mix_factor > 8192 {
            let mut muting_window = dsp_helper::MUTE_FACTOR_START_48KHZ;
            let mut unmuting_window = dsp_helper::UNMUTE_FACTOR_START_48KHZ;
            let start_ix = sync_buffer.len() - OVERLAP_LENGTH;
            for i in 0..OVERLAP_LENGTH {
                let sb = sync_buffer[start_ix + i] as i32;
                let mixed = (self.params.mute_factor as i32 * voiced_vector_storage[i] as i32) >> 14;
                sync_buffer[start_ix + i] = ((sb * muting_window
                    + mixed * unmuting_window
                    + 16384)
                    >> 15) as i16;
                muting_window += dsp_helper::MUTE_FACTOR_INCREMENT_48KHZ;
                unmuting_window += dsp_helper::UNMUTE_FACTOR_INCREMENT_48KHZ;
            }
        }

        // Unvoiced part: AR-filter the scaled random vector.
        unvoiced_array_memory[NOISE_LPC_ORDER - UNVOICED_LPC_ORDER..NOISE_LPC_ORDER]
            .copy_from_slice(&self.params.ar_filter_state);
        let add_constant = if self.params.ar_gain_scale > 0 {
            1 << (self.params.ar_gain_scale - 1)
        } else {
            0
        };
        spl::affine_transform_vector(
            &mut scaled_random_vector,
            &rng_buffer,
            self.params.ar_gain,
            add_constant,
            self.params.ar_gain_scale,
            current_lag,
        );
        // filter_ar state/output layout: state at [NOISE_LPC_ORDER-UNVOICED..],
        // output at unvoiced_vector = base + UNVOICED_LPC_ORDER. Use a window of
        // unvoiced_array_memory starting at NOISE_LPC_ORDER - UNVOICED_LPC_ORDER.
        let uv_base = NOISE_LPC_ORDER - UNVOICED_LPC_ORDER;
        spl::filter_ar_fast_q12(
            &scaled_random_vector,
            &mut unvoiced_array_memory[uv_base..],
            &self.params.ar_filter,
            current_lag,
        );
        // Save the AR filter state for next time.
        let uv_out = NOISE_LPC_ORDER; // unvoiced_vector index base
        self.params.ar_filter_state.copy_from_slice(
            &unvoiced_array_memory[uv_out + current_lag - UNVOICED_LPC_ORDER..uv_out + current_lag],
        );

        // Combine voiced and unvoiced.
        let temp_shift0 = (31 - spl::norm_w32(self.max_lag as i32) as i32) - 5;
        let mut mix_factor_increment = (256 >> temp_shift0) as i16;
        if self.stop_muting {
            mix_factor_increment = 0;
        }
        let temp_shift = 8 - temp_shift0;
        let mut temp_length = ((self.params.current_voice_mix_factor as i32
            - self.params.voice_mix_factor as i32)
            >> temp_shift) as usize;
        temp_length = temp_length.min(current_lag);

        // CrossFade(voiced_vector, unvoiced_vector, temp_length, ...).
        {
            let voiced = &voiced_vector_storage[OVERLAP_LENGTH..];
            let unvoiced = &unvoiced_array_memory[uv_out..];
            let mut factor = self.params.current_voice_mix_factor;
            let mut complement = 16384 - factor;
            for i in 0..temp_length {
                temp_data[i] = ((factor as i32 * voiced[i] as i32
                    + complement as i32 * unvoiced[i] as i32
                    + 8192)
                    >> 14) as i16;
                factor -= mix_factor_increment;
                complement += mix_factor_increment;
            }
            self.params.current_voice_mix_factor = factor;
        }

        if temp_length < current_lag {
            if mix_factor_increment != 0 {
                self.params.current_voice_mix_factor = self.params.voice_mix_factor;
            }
            let temp_scale = 16384 - self.params.current_voice_mix_factor;
            let voiced = &voiced_vector_storage[OVERLAP_LENGTH..];
            let unvoiced = &unvoiced_array_memory[uv_out..];
            let mut tail = vec![0i16; current_lag - temp_length];
            spl::scale_and_add_vectors_with_round(
                &voiced[temp_length..],
                self.params.current_voice_mix_factor,
                &unvoiced[temp_length..],
                temp_scale,
                14,
                &mut tail,
                current_lag - temp_length,
            );
            temp_data[temp_length..current_lag].copy_from_slice(&tail);
        }

        // Progressive muting.
        if self.consecutive_expands == 3 {
            self.params.mute_slope = self.params.mute_slope.max(1049 / FS_MULT as i32);
        }
        if self.consecutive_expands == 7 {
            self.params.mute_slope = self.params.mute_slope.max(2097 / FS_MULT as i32);
        }
        if self.consecutive_expands != 0 || !self.params.onset {
            let src = temp_data; // [i16; N] is Copy.
            spl::affine_transform_vector(
                &mut temp_data,
                &src,
                self.params.mute_factor,
                8192,
                14,
                current_lag,
            );
            if !self.stop_muting {
                dsp_helper::mute_signal(&mut temp_data, self.params.mute_slope, current_lag);
                let mut gain = (16384
                    - (((current_lag as i32 * self.params.mute_slope) + 8192) >> 6))
                    as i16;
                gain = (((gain as i32 * self.params.mute_factor as i32) + 8192) >> 14) as i16;
                if self.consecutive_expands > 3 && gain >= self.params.mute_factor {
                    self.params.mute_factor = 0;
                } else {
                    self.params.mute_factor = gain;
                }
            }
        }

        // Background noise.
        background_noise.generate_background_noise(
            &rng_buffer,
            current_lag,
            &mut unvoiced_array_memory,
        );
        let noise = &unvoiced_array_memory[NOISE_LPC_ORDER..];
        for i in 0..current_lag {
            temp_data[i] = (temp_data[i] as i32 + noise[i] as i32) as i16;
        }

        output.clear();
        output.extend_from_slice(&temp_data[..current_lag]);

        self.consecutive_expands = if self.consecutive_expands >= MAX_CONSECUTIVE_EXPANDS {
            MAX_CONSECUTIVE_EXPANDS
        } else {
            self.consecutive_expands + 1
        };
        self.expand_duration_samples =
            (self.expand_duration_samples + output.len()).min((FS_HZ * 2) as usize);
    }

    /// `Expand::Correlation`: down-sample to 4 kHz and correlate, writing 54
    /// normalized int16 lags into `output`.
    fn correlation(input: &[i16], input_length: usize, output: &mut [i16]) {
        const START_LAG: usize = 10;
        const NUM_LAGS_C: usize = 54;
        const CORR_LEN: usize = 60;
        const DOWNSAMPLED_LEN: usize = START_LAG + NUM_LAGS_C + CORR_LEN; // 124
        let factor = 12usize;
        let coef: [i16; 7] = [1019, 390, 427, 440, 427, 390, 1019];

        let mut downsampled = [0i16; DOWNSAMPLED_LEN];
        let in_base = (input_length - DOWNSAMPLED_LEN * factor) as isize;
        spl::downsample_fast(
            input,
            in_base,
            DOWNSAMPLED_LEN * factor,
            &mut downsampled,
            DOWNSAMPLED_LEN,
            &coef,
            factor,
            0,
        );

        let max_value = spl::max_abs_value_w16(&downsampled);
        let norm_shift = 16 - spl::norm_w32(max_value as i32);
        let tmp = downsampled;
        spl::vector_bit_shift_w16(&mut downsampled, DOWNSAMPLED_LEN, &tmp, norm_shift);

        let mut correlation = [0i32; NUM_LAGS_C];
        spl::cross_correlation_with_auto_shift(
            &downsampled[DOWNSAMPLED_LEN - CORR_LEN..],
            &downsampled,
            (DOWNSAMPLED_LEN - CORR_LEN - START_LAG) as isize,
            CORR_LEN,
            NUM_LAGS_C,
            -1,
            &mut correlation,
        );

        let max_correlation = spl::max_abs_value_w32(&correlation);
        let norm_shift2 = (18 - spl::norm_w32(max_correlation) as i32).max(0);
        spl::vector_bit_shift_w32_to_w16(output, NUM_LAGS_C, &correlation, norm_shift2);
    }

    /// `Expand::AnalyzeSignal`: estimate pitch lags, expansion vectors, the LPC
    /// model, gain, and the voiced mix from the decoded history.
    fn analyze_signal(
        &mut self,
        sync_buffer: &mut [i16],
        background_noise: &mut BackgroundNoise,
        random_vector: &mut RandomVector,
        rng_buffer: &mut [i16],
    ) {
        let fs_mult = FS_MULT;
        let fs_mult_4 = fs_mult * 4;
        let fs_mult_20 = fs_mult * 20;
        let fs_mult_120 = fs_mult * 120;
        let fs_mult_dist_len = fs_mult * DISTORTION_LENGTH; // 120
        let fs_mult_lpc_analysis_len = fs_mult * LPC_ANALYSIS_LENGTH; // 960

        let audio_history_position = sync_buffer.len() - SIGNAL_LENGTH;
        let audio_history: Vec<i16> =
            sync_buffer[audio_history_position..audio_history_position + SIGNAL_LENGTH].to_vec();

        self.initialize_for_an_expand_period(background_noise, random_vector);

        let mut correlation_length = 51usize;
        let mut correlation_vector = [0i16; 54];
        Self::correlation(&audio_history, SIGNAL_LENGTH, &mut correlation_vector);

        let mut best_correlation_index = [0usize; NUM_CORRELATION_CANDIDATES];
        let mut best_correlation = [0i16; NUM_CORRELATION_CANDIDATES];
        dsp_helper::peak_detection(
            &mut correlation_vector,
            correlation_length,
            NUM_CORRELATION_CANDIDATES,
            fs_mult,
            &mut best_correlation_index,
            &mut best_correlation,
        );
        for v in &mut best_correlation_index {
            *v += fs_mult_20;
        }

        let mut best_distortion_index = [0usize; NUM_CORRELATION_CANDIDATES];
        let mut best_distortion_w32 = [0i32; NUM_CORRELATION_CANDIDATES];
        let mut distortion_scale = 0i32;
        let dist_base = (SIGNAL_LENGTH - fs_mult_dist_len) as isize;
        for i in 0..NUM_CORRELATION_CANDIDATES {
            let min_index = fs_mult_20.max(best_correlation_index[i] - fs_mult_4);
            let max_index = (fs_mult_120 - 1).min(best_correlation_index[i] + fs_mult_4);
            let (idx, dist) = dsp_helper::min_distortion(
                &audio_history,
                dist_base,
                min_index,
                max_index,
                fs_mult_dist_len,
            );
            best_distortion_index[i] = idx;
            best_distortion_w32[i] = dist;
            distortion_scale = (16 - spl::norm_w32(best_distortion_w32[i]) as i32).max(distortion_scale);
        }
        let mut best_distortion = [0i16; NUM_CORRELATION_CANDIDATES];
        spl::vector_bit_shift_w32_to_w16(
            &mut best_distortion,
            NUM_CORRELATION_CANDIDATES,
            &best_distortion_w32,
            distortion_scale,
        );

        let mut best_ratio = i32::MIN;
        let mut best_index = usize::MAX;
        for i in 0..NUM_CORRELATION_CANDIDATES {
            let ratio = if best_distortion[i] > 0 {
                (best_correlation[i] as i32 * (1 << 16)) / best_distortion[i] as i32
            } else if best_correlation[i] == 0 {
                0
            } else {
                i32::MAX
            };
            if ratio > best_ratio {
                best_index = i;
                best_ratio = ratio;
            }
        }

        let distortion_lag = best_distortion_index[best_index];
        let correlation_lag = best_correlation_index[best_index];
        self.max_lag = distortion_lag.max(correlation_lag);

        correlation_length = (distortion_lag + 10).min(fs_mult_120).max(60 * fs_mult);
        let start_index = distortion_lag.min(correlation_lag);
        let correlation_lags =
            ((distortion_lag as i32 - correlation_lag as i32).unsigned_abs() as usize) + 1;

        // Single channel.
        let parameters = &mut self.params;

        let signal_max = spl::max_abs_value_w16(
            &audio_history[SIGNAL_LENGTH - correlation_length - start_index - correlation_lags
                ..SIGNAL_LENGTH - correlation_length - start_index - correlation_lags
                    + (correlation_length + start_index + correlation_lags - 1)],
        );
        let correlation_scale = ((31 - spl::norm_w32(signal_max as i32 * signal_max as i32) as i32)
            + (31 - spl::norm_w32(correlation_length as i32) as i32)
            - 31)
            .max(0);

        let mut correlation_vector2 = vec![0i32; correlation_lags];
        spl::cross_correlation_at(
            &mut correlation_vector2,
            &audio_history[SIGNAL_LENGTH - correlation_length..],
            &audio_history,
            (SIGNAL_LENGTH - correlation_length - start_index) as isize,
            correlation_length,
            correlation_lags,
            correlation_scale,
            -1,
        );

        let mut best_index = spl::max_index_w32(&correlation_vector2);
        let mut max_correlation = correlation_vector2[best_index];
        best_index += start_index;

        let mut energy1 = spl::dot_product_with_scale(
            &audio_history[SIGNAL_LENGTH - correlation_length..],
            &audio_history[SIGNAL_LENGTH - correlation_length..],
            correlation_length,
            correlation_scale,
        );
        let mut energy2 = spl::dot_product_with_scale(
            &audio_history[SIGNAL_LENGTH - correlation_length - best_index..],
            &audio_history[SIGNAL_LENGTH - correlation_length - best_index..],
            correlation_length,
            correlation_scale,
        );

        let corr_coefficient: i32;
        if energy1 > 0 && energy2 > 0 {
            let mut energy1_scale = (16 - spl::norm_w32(energy1) as i32).max(0);
            let energy2_scale = (16 - spl::norm_w32(energy2) as i32).max(0);
            if (energy1_scale + energy2_scale) & 1 == 1 {
                energy1_scale += 1;
            }
            let scaled_energy1 = energy1 >> energy1_scale;
            let scaled_energy2 = energy2 >> energy2_scale;
            let sqrt_energy_product = spl::sqrt_floor(scaled_energy1 * scaled_energy2) as i16;
            let cc_shift = 14 - (energy1_scale + energy2_scale) / 2;
            max_correlation = spl::shift_w32(max_correlation, cc_shift);
            corr_coefficient =
                spl::div_w32w16(max_correlation, sqrt_energy_product).min(16384);
        } else {
            corr_coefficient = 0;
        }

        // Extract expand_vector0 and expand_vector1.
        let expansion_length = self.max_lag + OVERLAP_LENGTH;
        let v1_start = SIGNAL_LENGTH - expansion_length;
        let vector1 = &audio_history[v1_start..v1_start + expansion_length];
        let v2_start = v1_start - distortion_lag;
        let vector2 = &audio_history[v2_start..v2_start + expansion_length];
        energy1 =
            spl::dot_product_with_scale(vector1, vector1, expansion_length, correlation_scale);
        energy2 =
            spl::dot_product_with_scale(vector2, vector2, expansion_length, correlation_scale);

        let amplitude_ratio: i16;
        if energy1 / 4 < energy2 && energy1 > energy2 / 4 {
            let scaled_energy2_b = (16 - spl::norm_w32(energy2) as i32).max(0);
            let scaled_energy1_b = scaled_energy2_b - 13;
            let energy_ratio = spl::div_w32w16(
                spl::shift_w32(energy1, -scaled_energy1_b),
                (energy2 >> scaled_energy2_b) as i16,
            );
            amplitude_ratio = spl::sqrt_floor(energy_ratio << 13) as i16;
            parameters.expand_vector0.clear();
            parameters.expand_vector0.extend_from_slice(vector1);
            parameters.expand_vector1.clear();
            parameters.expand_vector1.resize(expansion_length, 0);
            let mut temp_1 = vec![0i16; expansion_length];
            spl::affine_transform_vector(
                &mut temp_1,
                vector2,
                amplitude_ratio,
                4096,
                13,
                expansion_length,
            );
            parameters.expand_vector1.copy_from_slice(&temp_1);
        } else {
            parameters.expand_vector0.clear();
            parameters.expand_vector0.extend_from_slice(vector1);
            parameters.expand_vector1 = parameters.expand_vector0.clone();
            amplitude_ratio = if energy1 / 4 < energy2 || energy2 == 0 {
                4096
            } else {
                16384
            };
        }

        // Set the three lag values.
        if distortion_lag == correlation_lag {
            self.expand_lags = [distortion_lag; 3];
        } else {
            self.expand_lags[0] = distortion_lag;
            self.expand_lags[1] = (distortion_lag + correlation_lag) / 2;
            self.expand_lags[2] = if distortion_lag > correlation_lag {
                (distortion_lag + correlation_lag - 1) / 2
            } else {
                (distortion_lag + correlation_lag + 1) / 2
            };
        }

        // LPC analysis on a zero-padded copy of the signal tail.
        let temp_index = SIGNAL_LENGTH - fs_mult_lpc_analysis_len - UNVOICED_LPC_ORDER;
        let mut temp_signal = vec![0i16; fs_mult_lpc_analysis_len + UNVOICED_LPC_ORDER];
        temp_signal[UNVOICED_LPC_ORDER..].copy_from_slice(
            &audio_history[temp_index + UNVOICED_LPC_ORDER
                ..temp_index + UNVOICED_LPC_ORDER + fs_mult_lpc_analysis_len],
        );
        let mut auto_correlation = [0i32; UNVOICED_LPC_ORDER + 1];
        spl::cross_correlation_with_auto_shift(
            &temp_signal[UNVOICED_LPC_ORDER..],
            &temp_signal,
            UNVOICED_LPC_ORDER as isize,
            fs_mult_lpc_analysis_len,
            UNVOICED_LPC_ORDER + 1,
            -1,
            &mut auto_correlation,
        );

        if auto_correlation[0] > 0 {
            let mut reflection = [0i16; UNVOICED_LPC_ORDER];
            let stability = spl::levinson_durbin(
                &auto_correlation,
                &mut parameters.ar_filter,
                &mut reflection,
                UNVOICED_LPC_ORDER,
            );
            if stability != 1 {
                parameters.ar_filter[0] = 4096;
                for c in &mut parameters.ar_filter[1..] {
                    *c = 0;
                }
            }
        }

        // Noise segment for channel 0.
        let noise_length = if distortion_lag < 40 {
            2 * distortion_lag + 30
        } else {
            distortion_lag + 30
        };
        if noise_length <= RANDOM_TABLE.len() {
            rng_buffer[..noise_length].copy_from_slice(&RANDOM_TABLE[..noise_length]);
        } else {
            rng_buffer[..RANDOM_TABLE.len()].copy_from_slice(&RANDOM_TABLE);
            random_vector.increase_seed_increment(2);
            random_vector.generate(&mut rng_buffer[RANDOM_TABLE.len()..noise_length]);
        }

        // Unvoiced filter state and gain.
        parameters
            .ar_filter_state
            .copy_from_slice(&audio_history[SIGNAL_LENGTH - UNVOICED_LPC_ORDER..]);
        let mut unvoiced_buf = vec![0i16; UNVOICED_LPC_ORDER + 128];
        unvoiced_buf[..UNVOICED_LPC_ORDER]
            .copy_from_slice(&audio_history[SIGNAL_LENGTH - 128 - UNVOICED_LPC_ORDER..SIGNAL_LENGTH - 128]);
        spl::filter_ma_fast_q12(
            &audio_history,
            (SIGNAL_LENGTH - 128) as isize,
            &mut unvoiced_buf[UNVOICED_LPC_ORDER..],
            &parameters.ar_filter,
            128,
        );
        let unvoiced_vector = &unvoiced_buf[UNVOICED_LPC_ORDER..];
        let max_abs = spl::max_abs_value_w16(&unvoiced_vector[..128]);
        let unvoiced_max_abs = if max_abs == i16::MAX { max_abs as i32 + 1 } else { max_abs as i32 };
        let unvoiced_prescale = (2 * spl::get_size_in_bits(unvoiced_max_abs as u32) as i32 - 24).max(0);
        let mut unvoiced_energy = spl::dot_product_with_scale(
            unvoiced_vector,
            unvoiced_vector,
            128,
            unvoiced_prescale,
        );
        let mut unvoiced_scale = spl::norm_w32(unvoiced_energy) - 3;
        unvoiced_scale += (unvoiced_scale & 0x1) ^ 0x1;
        unvoiced_energy = spl::shift_w32(unvoiced_energy, unvoiced_scale as i32);
        let unvoiced_gain = spl::sqrt_floor(unvoiced_energy) as i16;
        parameters.ar_gain_scale = 13 + (unvoiced_scale as i32 + 7 - unvoiced_prescale) as i16 / 2;
        parameters.ar_gain = unvoiced_gain;

        // Voice mix factor from corr_coefficient (cubic polynomial).
        if corr_coefficient > 7875 {
            let x1 = corr_coefficient as i16;
            let x2 = ((x1 as i32 * x1 as i32) >> 14) as i16;
            let x3 = ((x1 as i32 * x2 as i32) >> 14) as i16;
            let coeffs = [-5179i32, 19931, -16422, 5776];
            let mut temp_sum = coeffs[0] * 16384;
            temp_sum += coeffs[1] * x1 as i32;
            temp_sum += coeffs[2] * x2 as i32;
            temp_sum += coeffs[3] * x3 as i32;
            parameters.voice_mix_factor = (temp_sum / 4096).min(16384) as i16;
            parameters.voice_mix_factor = parameters.voice_mix_factor.max(0);
        } else {
            parameters.voice_mix_factor = 0;
        }

        // Muting slope from amplitude_ratio (Q13).
        let slope = amplitude_ratio;
        if slope > 12288 {
            let denom = spl::sat_w32_to_w16((distortion_lag as i32 * slope as i32) >> 8);
            let temp_ratio = spl::div_w32w16((slope as i32 - 8192) << 12, denom);
            if slope > 14746 {
                parameters.mute_slope = (temp_ratio + 1) / 2;
            } else {
                parameters.mute_slope = (temp_ratio + 4) / 8;
            }
            parameters.onset = true;
        } else {
            parameters.mute_slope =
                spl::div_w32w16((8192 - slope as i32) * 128, distortion_lag as i16);
            if parameters.voice_mix_factor <= 13107 {
                parameters.mute_slope = (5243 / fs_mult as i32).max(parameters.mute_slope);
            } else if slope > 8028 {
                parameters.mute_slope = 0;
            }
            parameters.onset = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::neteq::dsp::test_vectors::load;

    fn as_i16(v: Vec<i64>) -> Vec<i16> {
        v.into_iter().map(|x| x as i16).collect()
    }

    #[test]
    fn process_sequence_matches_oracle() {
        let mut sync = as_i16(load("expand_in"));
        let mut bg = BackgroundNoise::new();
        let mut rv = RandomVector::new();
        let mut expand = Expand::new();

        for call in 0..4 {
            let mut output = Vec::new();
            expand.process(&mut sync, &mut bg, &mut rv, &mut output);
            let mut got = vec![output.len() as i64];
            got.extend(output.iter().map(|&x| x as i64));
            assert_eq!(got, load(&format!("expand_out_{call}")), "call {call}");
        }

        let tail: Vec<i64> = sync.iter().map(|&x| x as i64).collect();
        assert_eq!(tail, load("expand_sync_tail"));
    }
}
