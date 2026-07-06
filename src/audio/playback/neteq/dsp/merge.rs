//! Port of WebRTC neteq `Merge`: stitches the first decoded frame after a
//! concealment run onto the extrapolated signal. It generates expand data to
//! borrow against, finds the best overlap offset by downsampled correlation,
//! energy-matches and unmutes the new frame, and cross-fades the seam.
//!
//! Mono, 48 kHz. The `sync_buffer` is a flat `&mut [i16]` with an explicit
//! `next_index` (its `FutureLength` is `len - next_index`). Bit-identical to the
//! reference.

use super::background_noise::BackgroundNoise;
use super::dsp_helper;
use super::expand::Expand;
use super::random_vector::RandomVector;
use super::scratch::DspScratch;
use super::spl;

const FS_MULT: usize = 6;
const TIMESTAMPS_PER_CALL: usize = 480; // fs_hz / 100
const OVERLAP_LENGTH: usize = 30;
const MAX_CORRELATION_LENGTH: usize = 60;
const EXPAND_DOWNSAMP_LENGTH: usize = 100;
const INPUT_DOWNSAMP_LENGTH: usize = 40;
const REQUIRED_LENGTH: usize = (120 + 80 + 2) * FS_MULT; // 1212
const DOWNSAMPLE_48KHZ_TBL: [i16; 7] = [1019, 390, 427, 440, 427, 390, 1019];

/// `Merge::Process` (mono). Modifies `sync_buffer` in place (the borrowed prefix
/// is written back at `next_index`) and writes the merged result past that
/// prefix into `scratch.op_out`. Returns `added`, the samples produced beyond
/// what was borrowed from the sync buffer.
pub(crate) fn process(
    input: &[i16],
    sync_buffer: &mut [i16],
    next_index: usize,
    expand: &mut Expand,
    background_noise: &mut BackgroundNoise,
    random_vector: &mut RandomVector,
    scratch: &mut DspScratch,
) -> usize {
    let DspScratch {
        expand: expand_scratch,
        expand_out: expanded_temp,
        merge: scratch,
        op_out: output,
        ..
    } = scratch;
    output.clear();
    let input_length = input.len();
    if input_length == 0 {
        return 0;
    }

    // --- GetExpandedSignal ---
    let old_length = sync_buffer.len() - next_index;
    expand.set_parameters_for_merge_after_expand();
    expand.process(
        sync_buffer,
        background_noise,
        random_vector,
        expand_scratch,
        expanded_temp,
    );

    // Only the first `REQUIRED_LENGTH` samples are ever read, so a longer
    // sync-buffer future is truncated up front to keep the scratch bounded.
    let future = &sync_buffer[next_index..];
    let take = future.len().min(REQUIRED_LENGTH);
    scratch.expanded.clear();
    scratch.expanded.extend_from_slice(&future[..take]);
    let expanded = &mut scratch.expanded;
    if expanded.len() < REQUIRED_LENGTH {
        while expanded.len() < REQUIRED_LENGTH {
            expanded.extend_from_slice(expanded_temp);
        }
        expanded.truncate(REQUIRED_LENGTH);
    }
    let expanded_length = REQUIRED_LENGTH;

    // --- per channel (mono) ---
    let new_mute_factor = signal_scaling(input, input_length, expanded).min(16384);

    let mut expanded_downsampled = [0i16; EXPAND_DOWNSAMP_LENGTH];
    let mut input_downsampled = [0i16; INPUT_DOWNSAMP_LENGTH];
    downsample(
        input,
        input_length,
        expanded,
        expanded_length,
        &mut expanded_downsampled,
        &mut input_downsampled,
    );
    let best_correlation_index = correlate_and_peak_search(
        old_length,
        input_length,
        expand,
        &input_downsampled,
        &expanded_downsampled,
        &mut scratch.correlation16,
    );

    scratch.temp_data.clear();
    scratch
        .temp_data
        .resize(input_length + best_correlation_index, 0);
    let temp_data = &mut scratch.temp_data;

    let mut interpolation_length =
        (MAX_CORRELATION_LENGTH * FS_MULT).min(expanded_length - best_correlation_index);
    interpolation_length = interpolation_length.min(input_length);

    let mute_factor = expand.mute_factor().max(new_mute_factor);

    // Ramp/unmute the new decoded frame (operates on a working copy of input).
    scratch.input_channel.clear();
    scratch.input_channel.extend_from_slice(input);
    let input_channel = &mut scratch.input_channel;
    if mute_factor < 16384 {
        let back_to_fullscale_inc = ((16384 - mute_factor as i32) << 6) / input_length as i32;
        let increment = (4194 / FS_MULT as i32).max(back_to_fullscale_inc);
        let mut mf = dsp_helper::ramp_signal_in_place(
            input_channel,
            interpolation_length,
            mute_factor as i32,
            increment,
        ) as i16;
        // UnmuteSignal writes the post-interpolation tail directly to output.
        dsp_helper::unmute_signal(
            &input[interpolation_length..],
            input_length - interpolation_length,
            &mut mf,
            increment,
            &mut temp_data[best_correlation_index + interpolation_length..],
        );
    } else {
        temp_data[best_correlation_index + interpolation_length..]
            .copy_from_slice(&input[interpolation_length..]);
    }

    // Overlap-and-mix linearly.
    let increment = (16384 / (interpolation_length + 1)) as i16;
    let local_mute_factor = 16384 - increment;
    temp_data[..best_correlation_index].copy_from_slice(&expanded[..best_correlation_index]);
    scratch.faded.clear();
    scratch.faded.resize(interpolation_length, 0);
    dsp_helper::cross_fade(
        &expanded[best_correlation_index..],
        input_channel,
        interpolation_length,
        local_mute_factor,
        increment,
        &mut scratch.faded,
    );
    temp_data[best_correlation_index..best_correlation_index + interpolation_length]
        .copy_from_slice(&scratch.faded);

    let output_length = best_correlation_index + input_length;

    // Write the borrowed prefix back to the sync buffer, return the rest.
    sync_buffer[next_index..next_index + old_length].copy_from_slice(&temp_data[..old_length]);
    output.extend_from_slice(&temp_data[old_length..output_length]);
    output_length - old_length
}

/// `Merge::SignalScaling`.
fn signal_scaling(input: &[i16], input_length: usize, expanded_signal: &[i16]) -> i16 {
    let mod_input_length = (64 * FS_MULT).min(input_length);
    let expanded_max = spl::max_abs_value_w16(&expanded_signal[..mod_input_length]);
    let mut factor =
        (expanded_max as i32 * expanded_max as i32) / (i32::MAX / mod_input_length as i32);
    let expanded_shift = if factor == 0 {
        0
    } else {
        31 - spl::norm_w32(factor) as i32
    };
    let mut energy_expanded = spl::dot_product_with_scale(
        expanded_signal,
        expanded_signal,
        mod_input_length,
        expanded_shift,
    );

    let input_max = spl::max_abs_value_w16(&input[..mod_input_length]);
    factor = (input_max as i32 * input_max as i32) / (i32::MAX / mod_input_length as i32);
    let input_shift = if factor == 0 {
        0
    } else {
        31 - spl::norm_w32(factor) as i32
    };
    let mut energy_input = spl::dot_product_with_scale(input, input, mod_input_length, input_shift);

    if input_shift > expanded_shift {
        energy_expanded >>= input_shift - expanded_shift;
    } else {
        energy_input >>= expanded_shift - input_shift;
    }

    if energy_input > energy_expanded {
        let temp_shift = spl::norm_w32(energy_input) as i32 - 17;
        energy_input = spl::shift_w32(energy_input, temp_shift);
        energy_expanded = spl::shift_w32(energy_expanded, temp_shift + 14);
        spl::sqrt_floor((energy_expanded / energy_input) << 14) as i16
    } else {
        16384
    }
}

/// `Merge::Downsample` (48 kHz, mono).
fn downsample(
    input: &[i16],
    input_length: usize,
    expanded_signal: &[i16],
    expanded_length: usize,
    expanded_downsampled: &mut [i16],
    input_downsampled: &mut [i16],
) {
    let decimation_factor = 12usize;
    let num_coefficients = 7usize;
    let length_limit = 480usize;
    let signal_offset = num_coefficients - 1; // 6

    spl::downsample_fast(
        expanded_signal,
        signal_offset as isize,
        expanded_length - signal_offset,
        expanded_downsampled,
        EXPAND_DOWNSAMP_LENGTH,
        &DOWNSAMPLE_48KHZ_TBL,
        decimation_factor,
        0,
    );

    if input_length <= length_limit {
        let temp_len = if input_length > signal_offset {
            input_length - signal_offset
        } else {
            0
        };
        let downsamp_temp_len = temp_len / decimation_factor;
        if downsamp_temp_len > 0 {
            spl::downsample_fast(
                input,
                signal_offset as isize,
                temp_len,
                input_downsampled,
                downsamp_temp_len,
                &DOWNSAMPLE_48KHZ_TBL,
                decimation_factor,
                0,
            );
        }
        for s in &mut input_downsampled[downsamp_temp_len..INPUT_DOWNSAMP_LENGTH] {
            *s = 0;
        }
    } else {
        spl::downsample_fast(
            input,
            signal_offset as isize,
            input_length - signal_offset,
            input_downsampled,
            INPUT_DOWNSAMP_LENGTH,
            &DOWNSAMPLE_48KHZ_TBL,
            decimation_factor,
            0,
        );
    }
}

/// `Merge::CorrelateAndPeakSearch`.
fn correlate_and_peak_search(
    start_position: usize,
    input_length: usize,
    expand: &Expand,
    input_downsampled: &[i16],
    expanded_downsampled: &[i16],
    correlation16: &mut Vec<i16>,
) -> usize {
    let stop_position_downsamp = MAX_CORRELATION_LENGTH.min(expand.max_lag() / (FS_MULT * 2) + 1);

    let mut correlation = [0i32; MAX_CORRELATION_LENGTH];
    spl::cross_correlation_with_auto_shift(
        input_downsampled,
        expanded_downsampled,
        0,
        INPUT_DOWNSAMP_LENGTH,
        stop_position_downsamp,
        1,
        &mut correlation,
    );

    let pad_length = OVERLAP_LENGTH - 1; // 29
    let correlation_buffer_size = 2 * pad_length + MAX_CORRELATION_LENGTH + 1;
    correlation16.clear();
    correlation16.resize(correlation_buffer_size, 0);
    let max_correlation = spl::max_abs_value_w32(&correlation[..stop_position_downsamp]);
    let norm_shift = (17 - spl::norm_w32(max_correlation) as i32).max(0);
    spl::vector_bit_shift_w32_to_w16(
        &mut correlation16[pad_length..pad_length + stop_position_downsamp],
        stop_position_downsamp,
        &correlation,
        norm_shift,
    );

    let mut start_index = TIMESTAMPS_PER_CALL + OVERLAP_LENGTH;
    start_index = start_position.max(start_index);
    start_index = if input_length > start_index {
        0
    } else {
        start_index - input_length
    };
    let start_index_downsamp = start_index / (FS_MULT * 2);
    let modified_stop_pos =
        stop_position_downsamp.min(MAX_CORRELATION_LENGTH + pad_length - start_index_downsamp);

    let mut peak_index = [0usize; 1];
    let mut peak_value = [0i16; 1];
    dsp_helper::peak_detection(
        &mut correlation16[pad_length + start_index_downsamp..],
        modified_stop_pos,
        1,
        FS_MULT,
        &mut peak_index,
        &mut peak_value,
    );
    peak_index[0] + start_index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::neteq::dsp::test_vectors::load;

    fn as_i16(v: Vec<i64>) -> Vec<i16> {
        v.into_iter().map(|x| x as i16).collect()
    }

    #[test]
    fn process_matches_oracle() {
        let mut sync = as_i16(load("merge_sync_in"));
        let input = as_i16(load("merge_in"));
        let next_index = 1536;
        let mut bg = BackgroundNoise::new();
        let mut rv = RandomVector::new();
        let mut expand = Expand::new();
        let mut scratch = DspScratch::new();
        {
            let mut o = Vec::new();
            expand.process(&mut sync, &mut bg, &mut rv, &mut scratch.expand, &mut o);
        }

        let added = process(
            &input,
            &mut sync,
            next_index,
            &mut expand,
            &mut bg,
            &mut rv,
            &mut scratch,
        );
        let output = &scratch.op_out;
        let mut got = vec![added as i64, output.len() as i64];
        got.extend(output.iter().map(|&x| x as i64));
        assert_eq!(got, load("merge_out"));

        let after: Vec<i64> = sync.iter().map(|&x| x as i64).collect();
        assert_eq!(after, load("merge_sync_after"));
    }
}
