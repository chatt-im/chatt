//! Port of WebRTC neteq `DspHelper`: the shared DSP utilities for the
//! time-stretch and concealment operations (downsampling, peak picking,
//! parabolic interpolation, and cross-fading). All arithmetic is fixed-point
//! and matches the reference bit for bit.

use super::spl;

/// `DspHelper::kDownsample48kHzTbl`: Q12 anti-alias FIR for 48 kHz -> 4 kHz.
const DOWNSAMPLE_48KHZ_TBL: [i16; 7] = [1019, 390, 427, 440, 427, 390, 1019];

/// `DspHelper::kParabolaCoefficients[17][3]`.
const PARABOLA_COEFFICIENTS: [[i16; 3]; 17] = [
    [120, 32, 64],
    [140, 44, 75],
    [150, 50, 80],
    [160, 57, 85],
    [180, 72, 96],
    [200, 89, 107],
    [210, 98, 112],
    [220, 108, 117],
    [240, 128, 128],
    [260, 150, 139],
    [270, 162, 144],
    [280, 174, 149],
    [300, 200, 160],
    [320, 228, 171],
    [330, 242, 176],
    [340, 257, 181],
    [360, 288, 192],
];

/// `DspHelper::DownsampleTo4kHz`. Only the 48 kHz input rate is wired up, since
/// that is the playback rate. Returns -1 if the input is too short, else 0.
pub(crate) fn downsample_to_4khz(
    input: &[i16],
    input_length: usize,
    output_length: usize,
    compensate_delay: bool,
    output: &mut [i16],
) -> i32 {
    // 48 kHz parameters.
    let filter_length = 7usize;
    let factor = 12usize;
    let filter_delay = if compensate_delay { 3 + 1 } else { 0 };
    spl::downsample_fast(
        input,
        (filter_length - 1) as isize,
        input_length - filter_length + 1,
        output,
        output_length,
        &DOWNSAMPLE_48KHZ_TBL,
        factor,
        filter_delay,
    )
}

/// Q15 tapering-window constants for Expand's overlap-add at 48 kHz.
pub(crate) const MUTE_FACTOR_START_48KHZ: i32 = 31711;
pub(crate) const MUTE_FACTOR_INCREMENT_48KHZ: i32 = -1057;
pub(crate) const UNMUTE_FACTOR_START_48KHZ: i32 = 1057;
pub(crate) const UNMUTE_FACTOR_INCREMENT_48KHZ: i32 = 1057;

/// `DspHelper::MinDistortion`: lag (`min_lag..=max_lag`) minimizing the sum of
/// absolute differences between `signal` and a delayed copy. `signal` is read at
/// negative offsets (`signal[j - i]`), so the caller must provide that history.
/// Returns the lag and writes the minimum distortion to `distortion_value`.
pub(crate) fn min_distortion(
    signal: &[i16],
    signal_base: isize,
    min_lag: usize,
    max_lag: usize,
    length: usize,
) -> (usize, i32) {
    let mut best_index = 0;
    let mut min_distortion = i32::MAX;
    for i in min_lag..=max_lag {
        let mut sum_diff: i32 = 0;
        for j in 0..length {
            let d1 = signal[(signal_base + j as isize) as usize] as i32;
            let d2 = signal[(signal_base + j as isize - i as isize) as usize] as i32;
            sum_diff = sum_diff.wrapping_add((d1 - d2).abs());
        }
        if sum_diff < min_distortion {
            min_distortion = sum_diff;
            best_index = i;
        }
    }
    (best_index, min_distortion)
}

/// `DspHelper::MuteSignal`: fade `signal` toward zero, reducing the Q14 gain by
/// `mute_slope` (Q20) per sample.
pub(crate) fn mute_signal(signal: &mut [i16], mute_slope: i32, length: usize) {
    let mut factor: i32 = (16384 << 6) + 32;
    for i in 0..length {
        signal[i] = (((factor >> 6).wrapping_mul(signal[i] as i32) + 8192) >> 14) as i16;
        factor -= mute_slope;
    }
}

/// `DspHelper::ParabolicFit`: three-point parabola interpolation. `signal_points`
/// is the slice starting at `data[peak - 1]`; `peak_index` enters as the integer
/// peak and is rewritten to the interpolated full-rate index.
fn parabolic_fit(
    signal_points: &[i16],
    fs_mult: usize,
    peak_index: &mut usize,
    peak_value: &mut i16,
) {
    let mut fit_index = [0u16; 13];
    match fs_mult {
        1 => {
            fit_index[0] = 0;
            fit_index[1] = 8;
            fit_index[2] = 16;
        }
        2 => {
            fit_index[0] = 0;
            fit_index[1] = 4;
            fit_index[2] = 8;
            fit_index[3] = 12;
            fit_index[4] = 16;
        }
        4 => {
            for (i, v) in [0, 2, 4, 6, 8, 10, 12, 14, 16].into_iter().enumerate() {
                fit_index[i] = v;
            }
        }
        _ => {
            for (i, v) in [0, 1, 3, 4, 5, 7, 8, 9, 11, 12, 13, 15, 16]
                .into_iter()
                .enumerate()
            {
                fit_index[i] = v;
            }
        }
    }

    let p0 = signal_points[0] as i32;
    let p1 = signal_points[1] as i32;
    let p2 = signal_points[2] as i32;
    let num = (p0 * -3) + (p1 * 4) - p2;
    let den = p0 + (p1 * -2) + p2;
    let temp = num * 120;
    let mut flag = 1i32;
    let stp = PARABOLA_COEFFICIENTS[fit_index[fs_mult] as usize][0]
        - PARABOLA_COEFFICIENTS[fit_index[fs_mult - 1] as usize][0];
    let strt = (PARABOLA_COEFFICIENTS[fit_index[fs_mult] as usize][0]
        + PARABOLA_COEFFICIENTS[fit_index[fs_mult - 1] as usize][0])
        / 2;

    if temp < -den * (strt as i32) {
        let mut lmt = strt - stp;
        loop {
            if flag == fs_mult as i32 || temp > -den * (lmt as i32) {
                let idx = fit_index[(fs_mult as i32 - flag) as usize] as usize;
                *peak_value = ((den * PARABOLA_COEFFICIENTS[idx][1] as i32
                    + num * PARABOLA_COEFFICIENTS[idx][2] as i32
                    + p0 * 256)
                    / 256) as i16;
                *peak_index = (*peak_index)
                    .wrapping_mul(2)
                    .wrapping_mul(fs_mult)
                    .wrapping_sub(flag as usize);
                break;
            }
            flag += 1;
            lmt -= stp;
        }
    } else if temp > -den * ((strt + stp) as i32) {
        let mut lmt = strt + 2 * stp;
        loop {
            if flag == fs_mult as i32 || temp < -den * (lmt as i32) {
                let idx = fit_index[(fs_mult as i32 + flag) as usize] as usize;
                let temp_term_1 = den * PARABOLA_COEFFICIENTS[idx][1] as i32;
                let temp_term_2 = num * PARABOLA_COEFFICIENTS[idx][2] as i32;
                let temp_term_3 = p0 * 256;
                *peak_value = ((temp_term_1 + temp_term_2 + temp_term_3) / 256) as i16;
                *peak_index = (*peak_index)
                    .wrapping_mul(2)
                    .wrapping_mul(fs_mult)
                    .wrapping_add(flag as usize);
                break;
            }
            flag += 1;
            lmt += stp;
        }
    } else {
        *peak_value = signal_points[1];
        *peak_index = (*peak_index).wrapping_mul(2).wrapping_mul(fs_mult);
    }
}

/// `DspHelper::PeakDetection`: find `num_peaks` parabola-refined maxima in `data`,
/// zeroing a window around each found peak before the next search. `data` must
/// have one extra readable element past `data_length` when `num_peaks == 1`.
pub(crate) fn peak_detection(
    data: &mut [i16],
    data_length: usize,
    num_peaks: usize,
    fs_mult: usize,
    peak_index: &mut [usize],
    peak_value: &mut [i16],
) {
    let mut data_length = data_length;
    for i in 0..num_peaks {
        if num_peaks == 1 {
            data_length += 1;
        }

        peak_index[i] = spl::max_index_w16(&data[..data_length - 1]);

        let (min_index, max_index) = if i != num_peaks - 1 {
            (
                if peak_index[i] > 2 { peak_index[i] - 2 } else { 0 },
                (data_length - 1).min(peak_index[i] + 2),
            )
        } else {
            (0, 0)
        };

        if peak_index[i] != 0 && peak_index[i] != data_length - 2 {
            let base = peak_index[i] - 1;
            parabolic_fit(&data[base..], fs_mult, &mut peak_index[i], &mut peak_value[i]);
        } else if peak_index[i] == data_length - 2 {
            if data[peak_index[i]] > data[peak_index[i] + 1] {
                let base = peak_index[i] - 1;
                parabolic_fit(&data[base..], fs_mult, &mut peak_index[i], &mut peak_value[i]);
            } else {
                // Linear approximation.
                peak_value[i] = (data[peak_index[i]] + data[peak_index[i] + 1]) >> 1;
                peak_index[i] = (peak_index[i] * 2 + 1) * fs_mult;
            }
        } else {
            peak_value[i] = data[peak_index[i]];
            peak_index[i] = peak_index[i] * 2 * fs_mult;
        }

        if i != num_peaks - 1 {
            for sample in &mut data[min_index..=max_index] {
                *sample = 0;
            }
        }
    }
}

/// `DspHelper::RampSignal`: scale `input` by `factor` (Q14), stepping the gain by
/// `increment` (Q20) per sample. Returns the final Q14 factor.
pub(crate) fn ramp_signal(input: &[i16], length: usize, factor: i32, increment: i32, output: &mut [i16]) -> i32 {
    let mut factor = factor;
    let mut factor_q20 = (factor << 6) + 32;
    for i in 0..length {
        output[i] = ((factor * input[i] as i32 + 8192) >> 14) as i16;
        factor_q20 += increment;
        factor_q20 = factor_q20.max(0);
        factor = (factor_q20 >> 6).min(16384);
    }
    factor
}

/// In-place `DspHelper::RampSignal(int16_t* signal, ...)`.
pub(crate) fn ramp_signal_in_place(signal: &mut [i16], length: usize, factor: i32, increment: i32) -> i32 {
    let mut factor = factor;
    let mut factor_q20 = (factor << 6) + 32;
    for i in 0..length {
        signal[i] = ((factor * signal[i] as i32 + 8192) >> 14) as i16;
        factor_q20 += increment;
        factor_q20 = factor_q20.max(0);
        factor = (factor_q20 >> 6).min(16384);
    }
    factor
}

/// `DspHelper::UnmuteSignal`: scale `input` by an increasing Q14 gain starting at
/// `factor`, stepping by `increment` (Q20). Writes the final factor back.
pub(crate) fn unmute_signal(input: &[i16], length: usize, factor: &mut i16, increment: i32, output: &mut [i16]) {
    let mut factor_16b = *factor as u16;
    let mut factor_32b = ((factor_16b as i32) << 6) + 32;
    for i in 0..length {
        output[i] = ((factor_16b as i32 * input[i] as i32 + 8192) >> 14) as i16;
        factor_32b = (factor_32b + increment).max(0);
        factor_16b = (factor_32b >> 6).min(16384) as u16;
    }
    *factor = factor_16b as i16;
}

/// `DspHelper::CrossFade`: mix `input1` (gain from `mix_factor`, Q14, decreasing
/// by `factor_decrement`) with `input2` (complement gain) into `output`. Returns
/// the final mix factor.
pub(crate) fn cross_fade(
    input1: &[i16],
    input2: &[i16],
    length: usize,
    mix_factor: i16,
    factor_decrement: i16,
    output: &mut [i16],
) -> i16 {
    let mut factor = mix_factor;
    let mut complement_factor = 16384 - factor;
    for i in 0..length {
        output[i] = ((factor as i32 * input1[i] as i32
            + complement_factor as i32 * input2[i] as i32
            + 8192)
            >> 14) as i16;
        factor -= factor_decrement;
        complement_factor += factor_decrement;
    }
    factor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::neteq::dsp::test_vectors::load;

    fn as_i16(v: Vec<i64>) -> Vec<i16> {
        v.into_iter().map(|x| x as i16).collect()
    }

    #[test]
    fn downsample_to_4khz_matches_oracle() {
        let input = as_i16(load("dsp_downsample_to_4khz_in"));
        let mut output = vec![0i16; 110];
        let r = downsample_to_4khz(&input, input.len(), 110, true, &mut output);
        let mut got = vec![r as i64];
        got.extend(output.iter().map(|&x| x as i64));
        assert_eq!(got, load("dsp_downsample_to_4khz"));
    }

    #[test]
    fn peak_detection_matches_oracle() {
        let mut data = as_i16(load("dsp_peak_detection_in"));
        data.push(0); // PeakDetection may read one past data_length.
        let mut peak_index = vec![0usize; 2];
        let mut peak_value = vec![0i16; 2];
        peak_detection(&mut data, 50, 2, 6, &mut peak_index, &mut peak_value);
        let mut got: Vec<i64> = peak_index.iter().map(|&x| x as i64).collect();
        got.extend(peak_value.iter().map(|&x| x as i64));
        assert_eq!(got, load("dsp_peak_detection"));
    }

    #[test]
    fn parabolic_fit_matches_oracle() {
        let triples: [[i16; 3]; 4] = [
            [1000, 2000, 1500],
            [2000, 2000, 2000],
            [500, 9000, 8000],
            [-100, 50, -200],
        ];
        let mut got = Vec::new();
        for t in triples {
            let mut idx = 10usize;
            let mut val = 0i16;
            parabolic_fit(&t, 6, &mut idx, &mut val);
            got.push(idx as i64);
            got.push(val as i64);
        }
        assert_eq!(got, load("dsp_parabolic_fit"));
    }

    #[test]
    fn cross_fade_matches_oracle() {
        let in1 = as_i16(load("dsp_cross_fade_in1"));
        let in2 = as_i16(load("dsp_cross_fade_in2"));
        let mut output = vec![0i16; 80];
        cross_fade(&in1, &in2, 80, 16384, 16384 / 80, &mut output);
        let got: Vec<i64> = output.iter().map(|&x| x as i64).collect();
        assert_eq!(got, load("dsp_cross_fade"));
    }
}
