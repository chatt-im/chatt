//! Port of WebRTC neteq `Normal`, the operation that resumes decoded playback.
//! Only the post-Expand path is ported (Chatt's `normal_after_expand`): it ramps
//! the first decoded frame up from the Expand mute level and cross-fades a short
//! Expand-generated tail over the seam so resumption does not click.
//!
//! Mono, 48 kHz. Bit-identical to the reference.

use super::background_noise::BackgroundNoise;
use super::expand::Expand;
use super::random_vector::RandomVector;
use super::spl;

const FS_HZ: i32 = 48000;
const FS_MULT: i32 = 6;
const SAMPLES_PER_MS: usize = (FS_HZ / 1000) as usize; // 48

/// `Normal::Process` for `last_mode == kExpand`. `input` is the freshly decoded
/// frame; `output` receives the unmuted, cross-faded result. `expand` must hold
/// the state from the concealment run that just ended.
pub(crate) fn process_after_expand(
    input: &[i16],
    sync_buffer: &mut [i16],
    expand: &mut Expand,
    background_noise: &mut BackgroundNoise,
    random_vector: &mut RandomVector,
    output: &mut Vec<i16>,
) {
    output.clear();
    output.extend_from_slice(input);
    if input.is_empty() {
        return;
    }

    let fs_shift = 30 - spl::norm_w32(FS_MULT) as i32;

    // Generate the interpolation tail from Expand.
    expand.set_parameters_for_normal_after_expand();
    let mut expanded = Vec::new();
    expand.process(sync_buffer, background_noise, random_vector, &mut expanded);
    let mut mute_factor = expand.mute_factor();
    expand.reset();

    let length_per_channel = input.len();

    let decoded_max = spl::max_abs_value_w16(input);
    let energy_length = ((FS_MULT * 64) as usize).min(length_per_channel);
    let mut scaling = (6 + fs_shift - spl::norm_w32(decoded_max as i32 * decoded_max as i32) as i32)
        .max(0);
    let mut energy = spl::dot_product_with_scale(input, input, energy_length, scaling);
    let scaled_energy_length = (energy_length >> scaling) as i32;
    energy = if scaled_energy_length > 0 {
        energy / scaled_energy_length
    } else {
        0
    };

    let mut local_mute_factor = 16384;
    if energy != 0 && energy > background_noise.energy() {
        scaling = spl::norm_w32(energy) as i32 - 16;
        let bgn_energy = spl::shift_w32(background_noise.energy(), scaling + 14);
        let energy_scaled = spl::shift_w32(energy, scaling) as i16;
        let ratio = spl::div_w32w16(bgn_energy, energy_scaled);
        local_mute_factor = local_mute_factor.min(spl::sqrt_floor(ratio << 14));
    }
    let mut mute_factor_i32 = (mute_factor as i32).max(local_mute_factor);
    mute_factor = mute_factor_i32 as i16;

    let back_to_fullscale_inc = (16384 - mute_factor as i32) / length_per_channel as i32;
    let increment = (64 / FS_MULT).max(back_to_fullscale_inc);
    for sample in output.iter_mut() {
        let scaled_signal = *sample as i32 * mute_factor as i32;
        *sample = ((scaled_signal + 8192) >> 14) as i16;
        mute_factor_i32 = (mute_factor as i32 + increment).min(16384);
        mute_factor = mute_factor_i32 as i16;
    }

    // Cross-fade the expanded tail into the unmuted output.
    crossfade(&expanded, output, SAMPLES_PER_MS);
}

/// `Normal`'s file-local `Crossfade`: fade `from` out and `to` in over
/// `win_length` samples (Q14).
fn crossfade(from: &[i16], to: &mut [i16], win_length: usize) {
    let win_length_clamped = win_length.min(from.len()).min(to.len());
    if win_length_clamped == 0 {
        return;
    }
    let win_slope_q14 = (1 << 14) / win_length_clamped as i16;
    let mut win_up_q14 = 0i16;
    for i in 0..win_length_clamped {
        win_up_q14 += win_slope_q14;
        to[i] = ((win_up_q14 as i32 * to[i] as i32
            + ((1 << 14) - win_up_q14 as i32) * from[i] as i32
            + (1 << 13))
            >> 14) as i16;
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
    fn process_after_expand_matches_oracle() {
        let mut sync = as_i16(load("normal_sync_in"));
        let input = as_i16(load("normal_in"));
        let mut bg = BackgroundNoise::new();
        let mut rv = RandomVector::new();
        let mut expand = Expand::new();
        for _ in 0..3 {
            let mut o = Vec::new();
            expand.process(&mut sync, &mut bg, &mut rv, &mut o);
        }

        let mut output = Vec::new();
        process_after_expand(&input, &mut sync, &mut expand, &mut bg, &mut rv, &mut output);
        let mut got = vec![output.len() as i64];
        got.extend(output.iter().map(|&x| x as i64));
        assert_eq!(got, load("normal_out"));
    }
}
