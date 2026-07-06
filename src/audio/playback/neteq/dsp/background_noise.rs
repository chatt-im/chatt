//! Port of WebRTC neteq `BackgroundNoise`: estimates a comfort-noise model
//! (energy, an LPC filter, and a gain) from low-energy stretches of decoded
//! audio, and synthesizes noise from it during long concealment runs.
//!
//! Chatt is mono, so this models a single channel. The arithmetic mirrors the
//! reference exactly, including the leading zero-padding the self-correlation
//! and MA-filter rely on for their negative-index reads.

use super::spl;

const MAX_LPC_ORDER: usize = 8;
const VEC_LEN: usize = 256;
const LOG_VEC_LEN: i32 = 8;
const RESIDUAL_LENGTH: usize = 64;
const LOG_RESIDUAL_LENGTH: i16 = 6;
const THRESHOLD_INCREMENT: i32 = 229; // 0.0035 in Q16.

/// Per-channel comfort-noise state (`BackgroundNoise::ChannelParameters`).
#[derive(Debug)]
struct ChannelParameters {
    energy: i32,
    max_energy: i32,
    energy_update_threshold: i32,
    low_energy_update_threshold: i32,
    filter_state: [i16; MAX_LPC_ORDER],
    filter: [i16; MAX_LPC_ORDER + 1],
    mute_factor: i16,
    scale: i16,
    scale_shift: i16,
}

impl ChannelParameters {
    fn new() -> Self {
        let mut p = Self {
            energy: 0,
            max_energy: 0,
            energy_update_threshold: 0,
            low_energy_update_threshold: 0,
            filter_state: [0; MAX_LPC_ORDER],
            filter: [0; MAX_LPC_ORDER + 1],
            mute_factor: 0,
            scale: 0,
            scale_shift: 0,
        };
        p.reset();
        p
    }

    fn reset(&mut self) {
        self.energy = 2500;
        self.max_energy = 0;
        self.energy_update_threshold = 500000;
        self.low_energy_update_threshold = 0;
        self.filter_state = [0; MAX_LPC_ORDER];
        self.filter = [0; MAX_LPC_ORDER + 1];
        self.filter[0] = 4096;
        self.mute_factor = 0;
        self.scale = 20000;
        self.scale_shift = 24;
    }
}

/// `BackgroundNoise` for a single channel.
#[derive(Debug)]
pub(crate) struct BackgroundNoise {
    params: ChannelParameters,
    initialized: bool,
}

impl BackgroundNoise {
    pub(crate) fn new() -> Self {
        Self {
            params: ChannelParameters::new(),
            initialized: false,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.initialized = false;
        self.params.reset();
    }

    pub(crate) fn initialized(&self) -> bool {
        self.initialized
    }

    pub(crate) fn energy(&self) -> i32 {
        self.params.energy
    }

    #[cfg(test)]
    pub(crate) fn mute_factor(&self) -> i16 {
        self.params.mute_factor
    }

    pub(crate) fn set_mute_factor(&mut self, value: i16) {
        self.params.mute_factor = value;
    }

    #[cfg(test)]
    pub(crate) fn filter(&self) -> &[i16] {
        &self.params.filter
    }

    #[cfg(test)]
    pub(crate) fn filter_state(&self) -> &[i16] {
        &self.params.filter_state
    }

    #[cfg(test)]
    pub(crate) fn scale(&self) -> i16 {
        self.params.scale
    }

    #[cfg(test)]
    pub(crate) fn scale_shift(&self) -> i16 {
        self.params.scale_shift
    }

    fn set_filter_state(&mut self, input: &[i16]) {
        let length = input.len().min(MAX_LPC_ORDER);
        self.params.filter_state[..length].copy_from_slice(&input[..length]);
    }

    /// `BackgroundNoise::Update`: re-estimate from the tail of `sync_buffer`.
    /// Returns true if the filter parameters were saved this call.
    pub(crate) fn update(&mut self, sync_buffer: &[i16]) -> bool {
        let mut auto_correlation = [0i32; MAX_LPC_ORDER + 1];
        let mut filter_output = [0i16; RESIDUAL_LENGTH];
        let mut reflection = [0i16; MAX_LPC_ORDER];
        let mut lpc = [0i16; MAX_LPC_ORDER + 1];

        // temp_signal_array[kMaxLpcOrder] = signal start; the first kMaxLpcOrder
        // entries stay zero so the negative-index reads land in the pad.
        let mut temp = [0i16; VEC_LEN + MAX_LPC_ORDER];
        let tail = &sync_buffer[sync_buffer.len() - VEC_LEN..];
        temp[MAX_LPC_ORDER..].copy_from_slice(tail);

        let sample_energy = self.calculate_auto_correlation(&temp, &mut auto_correlation);

        if sample_energy >= self.params.energy_update_threshold {
            self.increment_energy_threshold(sample_energy);
            return false;
        }

        if auto_correlation[0] <= 0 {
            return false;
        }
        self.params.energy_update_threshold = sample_energy.max(1);
        self.params.low_energy_update_threshold = 0;

        if spl::levinson_durbin(&auto_correlation, &mut lpc, &mut reflection, MAX_LPC_ORDER) != 1 {
            return false;
        }

        // Residual via the MA (inverse) filter over the last kResidualLength.
        let ma_base = (MAX_LPC_ORDER + VEC_LEN - RESIDUAL_LENGTH) as isize;
        spl::filter_ma_fast_q12(&temp, ma_base, &mut filter_output, &lpc, RESIDUAL_LENGTH);
        let residual_energy =
            spl::dot_product_with_scale(&filter_output, &filter_output, RESIDUAL_LENGTH, 0);

        // Spectral flatness: 5*residual >= 16*sample, and non-zero energy.
        if sample_energy > 0 && 5i64 * residual_energy as i64 >= 16i64 * sample_energy as i64 {
            let state_start = MAX_LPC_ORDER + VEC_LEN - MAX_LPC_ORDER;
            let filter_state: [i16; MAX_LPC_ORDER] = temp[state_start..state_start + MAX_LPC_ORDER]
                .try_into()
                .unwrap();
            self.save_parameters(&lpc, &filter_state, sample_energy, residual_energy);
            true
        } else {
            false
        }
    }

    /// `BackgroundNoise::GenerateBackgroundNoise`: synthesize `num_noise_samples`
    /// into `buffer[kMaxLpcOrder..]`. `buffer[..kMaxLpcOrder]` is used as the AR
    /// filter state. `random_vector` must be at least `num_noise_samples` long.
    /// `scaled_random` and `noise_copy` are caller scratch (see
    /// [`ExpandScratch`](super::scratch::ExpandScratch)); their contents are
    /// overwritten.
    pub(crate) fn generate_background_noise(
        &mut self,
        random_vector: &[i16],
        num_noise_samples: usize,
        buffer: &mut [i16],
        scaled_random: &mut Vec<i16>,
        noise_copy: &mut Vec<i16>,
    ) {
        const ORDER: usize = MAX_LPC_ORDER;
        // The synthesized noise is scaled by `mute_factor` (a `< 16384` affine)
        // before it leaves this function, so a zero mute factor makes every
        // output sample zero regardless of the AR synthesis. Chatt never raises
        // the BGN mute factor above the `0` that `Expand` seeds each expand
        // period, so this fast path is always taken today; skipping the affine
        // scale and `filter_ar_fast_q12` here is bit-exact. If the unmute ramp is
        // ever wired up, a non-zero `mute_factor` re-engages the full synthesis.
        if !self.initialized || self.params.mute_factor == 0 {
            buffer[ORDER..ORDER + num_noise_samples].fill(0);
            return;
        }

        scaled_random.clear();
        scaled_random.resize(num_noise_samples, 0);
        buffer[..ORDER].copy_from_slice(&self.params.filter_state);

        let dc_offset = if self.params.scale_shift > 1 {
            1 << (self.params.scale_shift - 1)
        } else {
            0
        };
        spl::affine_transform_vector(
            scaled_random,
            random_vector,
            self.params.scale,
            dc_offset,
            self.params.scale_shift,
            num_noise_samples,
        );

        let filter = self.params.filter;
        spl::filter_ar_fast_q12(scaled_random, buffer, &filter, num_noise_samples);

        let new_state: [i16; ORDER] = buffer[num_noise_samples..num_noise_samples + ORDER]
            .try_into()
            .unwrap();
        self.set_filter_state(&new_state);

        let bgn_mute_factor = self.params.mute_factor;
        if bgn_mute_factor < 16384 {
            // In place: noise_samples = buffer[ORDER..].
            let noise = &mut buffer[ORDER..ORDER + num_noise_samples];
            noise_copy.clear();
            noise_copy.extend_from_slice(noise);
            spl::affine_transform_vector(
                noise,
                noise_copy,
                bgn_mute_factor,
                8192,
                14,
                num_noise_samples,
            );
        }
        self.params.mute_factor = bgn_mute_factor;
    }

    /// `BackgroundNoise::CalculateAutoCorrelation`: scaled self-correlation,
    /// returning the energy-per-sample. `temp` is the zero-padded buffer with
    /// the signal at offset `kMaxLpcOrder`.
    fn calculate_auto_correlation(&self, temp: &[i16], auto_correlation: &mut [i32]) -> i32 {
        let correlation_scale = spl::cross_correlation_with_auto_shift(
            &temp[MAX_LPC_ORDER..],
            temp,
            MAX_LPC_ORDER as isize,
            VEC_LEN,
            MAX_LPC_ORDER + 1,
            -1,
            auto_correlation,
        );
        let energy_sample_shift = LOG_VEC_LEN - correlation_scale;
        auto_correlation[0] >> energy_sample_shift
    }

    /// `BackgroundNoise::IncrementEnergyThreshold`.
    fn increment_energy_threshold(&mut self, sample_energy: i32) {
        let p = &mut self.params;
        let mut temp_energy = (THRESHOLD_INCREMENT * p.low_energy_update_threshold) >> 16;
        temp_energy += THRESHOLD_INCREMENT * (p.energy_update_threshold & 0xFF);
        temp_energy += (THRESHOLD_INCREMENT * ((p.energy_update_threshold >> 8) & 0xFF)) << 8;
        p.low_energy_update_threshold += temp_energy;

        p.energy_update_threshold += THRESHOLD_INCREMENT * (p.energy_update_threshold >> 16);
        p.energy_update_threshold += p.low_energy_update_threshold >> 16;
        p.low_energy_update_threshold &= 0x0FFFF;

        p.max_energy -= p.max_energy >> 10;
        if sample_energy > p.max_energy {
            p.max_energy = sample_energy;
        }

        let energy_update_threshold = (p.max_energy + 524288) >> 20;
        if energy_update_threshold > p.energy_update_threshold {
            p.energy_update_threshold = energy_update_threshold;
        }
    }

    /// `BackgroundNoise::SaveParameters`.
    fn save_parameters(
        &mut self,
        lpc_coefficients: &[i16],
        filter_state: &[i16; MAX_LPC_ORDER],
        sample_energy: i32,
        residual_energy: i32,
    ) {
        let p = &mut self.params;
        p.filter
            .copy_from_slice(&lpc_coefficients[..MAX_LPC_ORDER + 1]);
        p.filter_state.copy_from_slice(filter_state);
        p.energy = sample_energy.max(1);
        p.energy_update_threshold = p.energy;
        p.low_energy_update_threshold = 0;

        let mut norm_shift = spl::norm_w32(residual_energy) - 1;
        if norm_shift & 0x1 != 0 {
            norm_shift -= 1;
        }
        let residual_energy = spl::shift_w32(residual_energy, norm_shift as i32);
        p.scale = spl::sqrt_floor(residual_energy) as i16;
        p.scale_shift = 13 + ((LOG_RESIDUAL_LENGTH + norm_shift) / 2);
        self.initialized = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::neteq::dsp::random_vector::RandomVector;
    use crate::audio::playback::neteq::dsp::test_vectors::load;

    fn as_i16(v: Vec<i64>) -> Vec<i16> {
        v.into_iter().map(|x| x as i16).collect()
    }

    #[test]
    fn update_and_generate_match_oracle() {
        let signal = as_i16(load("background_noise_in"));
        let mut bg = BackgroundNoise::new();
        let updated = bg.update(&signal);

        let mut params = vec![
            updated as i64,
            bg.initialized() as i64,
            bg.energy() as i64,
            bg.mute_factor() as i64,
            bg.scale() as i64,
            bg.scale_shift() as i64,
        ];
        params.extend(bg.filter().iter().map(|&x| x as i64));
        params.extend(bg.filter_state().iter().map(|&x| x as i64));
        assert_eq!(params, load("background_noise_params"));

        let num_noise = 80;
        let mut rv = RandomVector::new();
        let mut random_vector = vec![0i16; num_noise];
        rv.generate(&mut random_vector);
        let mut buffer = vec![0i16; MAX_LPC_ORDER + num_noise];
        let mut scaled_random = Vec::new();
        let mut noise_copy = Vec::new();
        bg.generate_background_noise(
            &random_vector,
            num_noise,
            &mut buffer,
            &mut scaled_random,
            &mut noise_copy,
        );
        let noise: Vec<i64> = buffer[MAX_LPC_ORDER..].iter().map(|&x| x as i64).collect();
        assert_eq!(noise, load("background_noise_generate"));
    }
}
