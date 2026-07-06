//! Port of WebRTC neteq `TimeStretch`/`Accelerate`/`PreemptiveExpand`: the
//! pitch-synchronous time-scaling operations. Mono only (Chatt is single
//! channel), so the `AudioMultiVector` collapses to a `Vec<i16>` and
//! `CrossFade` operates on it directly.
//!
//! Fixed at 48 kHz (`fs_mult = 6`). Arithmetic mirrors the reference exactly,
//! including the x86 shift-masking the VAD relies on (`wrapping_shl`/`shr`).

use super::background_noise::BackgroundNoise;
use super::dsp_helper;
use super::spl;

const FS_MULT: usize = 6;
const CORRELATION_LEN: usize = 50;
const MIN_LAG: usize = 10;
const MAX_LAG: usize = 60;
const DOWNSAMPLED_LEN: usize = CORRELATION_LEN + MAX_LAG; // 110
const CORRELATION_THRESHOLD: i32 = 14746; // 0.9 in Q14.
const FS_MULT_120: usize = FS_MULT * 120; // 15 ms = 720 samples.

/// `TimeStretch::ReturnCodes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReturnCode {
    Success = 0,
    SuccessLowEnergy = 1,
    NoStretch = 2,
    Error = -1,
}

/// Outcome of a stretch: the return code and the number of samples the length
/// changed by. The produced samples land in the caller's output scratch.
#[derive(Debug)]
pub(crate) struct StretchResult {
    pub(crate) return_code: ReturnCode,
    pub(crate) length_change_samples: usize,
}

struct Analysis {
    peak_index: usize,
    best_correlation: i16,
    active_speech: bool,
}

/// `TimeStretch::Process` up to (but not including) `CheckCriteriaAndStretch`.
/// `best_correlation` is the active-speech value; passive handling is left to
/// the caller, matching `SetParametersForPassiveSpeech`.
fn analyze(signal: &[i16], background_noise: &BackgroundNoise) -> Analysis {
    let signal_len = signal.len();
    let max_input_value = spl::max_abs_value_w16(signal) as i32;

    let mut downsampled = [0i16; DOWNSAMPLED_LEN];
    dsp_helper::downsample_to_4khz(signal, signal_len, DOWNSAMPLED_LEN, true, &mut downsampled);

    // AutoCorrelation: lags kMinLag..kMaxLag in the 4 kHz domain.
    let mut auto_corr = [0i32; CORRELATION_LEN];
    spl::cross_correlation_with_auto_shift(
        &downsampled[MAX_LAG..],
        &downsampled,
        (MAX_LAG - MIN_LAG) as isize,
        CORRELATION_LEN,
        MAX_LAG - MIN_LAG,
        -1,
        &mut auto_corr,
    );
    let max_corr = spl::max_abs_value_w32(&auto_corr);
    let corr_scaling = (17 - spl::norm_w32(max_corr) as i32).max(0);
    let mut auto_correlation = [0i16; CORRELATION_LEN + 1];
    spl::vector_bit_shift_w32_to_w16(
        &mut auto_correlation,
        CORRELATION_LEN,
        &auto_corr,
        corr_scaling,
    );

    // Strongest correlation peak.
    let mut peak_index_arr = [0usize; 1];
    let mut peak_value_arr = [0i16; 1];
    dsp_helper::peak_detection(
        &mut auto_correlation,
        CORRELATION_LEN,
        1,
        FS_MULT,
        &mut peak_index_arr,
        &mut peak_value_arr,
    );
    // Compensate for the down-sampled starting position.
    let peak_index = peak_index_arr[0] + MIN_LAG * FS_MULT * 2;

    let scaling = (31
        - spl::norm_w32(max_input_value.wrapping_mul(max_input_value)) as i32
        - spl::norm_w32(peak_index as i32) as i32)
        .max(0);

    let vec1 = &signal[FS_MULT_120 - peak_index..];
    let vec2 = &signal[FS_MULT_120..];
    let vec1_energy = spl::dot_product_with_scale(vec1, vec1, peak_index, scaling);
    let vec2_energy = spl::dot_product_with_scale(vec2, vec2, peak_index, scaling);
    let mut cross_corr = spl::dot_product_with_scale(vec1, vec2, peak_index, scaling);

    let active_speech = speech_detection(
        vec1_energy,
        vec2_energy,
        peak_index,
        scaling,
        background_noise,
    );

    let mut best_correlation = 0i16;
    if active_speech {
        let mut energy1_scale = (16 - spl::norm_w32(vec1_energy) as i32).max(0);
        let energy2_scale = (16 - spl::norm_w32(vec2_energy) as i32).max(0);
        // Keep the total scaling even so the post-sqrt scale factor is exact.
        if (energy1_scale + energy2_scale) & 1 == 1 {
            energy1_scale += 1;
        }
        let vec1_energy_int16 = (vec1_energy >> energy1_scale) as i16;
        let vec2_energy_int16 = (vec2_energy >> energy2_scale) as i16;
        let sqrt_energy_prod =
            spl::sqrt_floor((vec1_energy_int16 as i32) * (vec2_energy_int16 as i32)) as i16;
        let temp_scale = 14 - (energy1_scale + energy2_scale) / 2;
        cross_corr = spl::shift_w32(cross_corr, temp_scale);
        cross_corr = cross_corr.max(0);
        best_correlation = (spl::div_w32w16(cross_corr, sqrt_energy_prod) as i16).min(16384);
    }

    Analysis {
        peak_index,
        best_correlation,
        active_speech,
    }
}

/// `TimeStretch::SpeechDetection`: simple energy-ratio VAD.
fn speech_detection(
    vec1_energy: i32,
    vec2_energy: i32,
    peak_index: usize,
    scaling: i32,
    background_noise: &BackgroundNoise,
) -> bool {
    let mut left_side = ((vec1_energy as i64 + vec2_energy as i64) / 16)
        .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    let mut right_side = if background_noise.initialized() {
        background_noise.energy()
    } else {
        75000
    };
    let right_scale = (16 - spl::norm_w32(right_side) as i32).max(0);
    left_side >>= right_scale;
    right_side = (peak_index as i32).wrapping_mul(right_side >> right_scale);

    if (spl::norm_w32(left_side) as i32) < 2 * scaling {
        let temp_scale = spl::norm_w32(left_side) as i32;
        left_side = left_side.wrapping_shl(temp_scale as u32);
        right_side = right_side.wrapping_shr((2 * scaling - temp_scale) as u32);
    } else {
        left_side = left_side.wrapping_shl((2 * scaling) as u32);
    }
    left_side > right_side
}

/// `AudioVector::CrossFade` for a mono buffer: Q14 cross-fade of the last
/// `fade_length` samples of `out` with the first `fade_length` of `append`,
/// then append the remainder of `append`.
fn cross_fade(out: &mut Vec<i16>, append: &[i16], fade_length: usize) {
    let fade_length = fade_length.min(out.len()).min(append.len());
    let position = out.len() - fade_length;
    let alpha_step = 16384 / (fade_length as i32 + 1);
    let mut alpha = 16384;
    for i in 0..fade_length {
        alpha -= alpha_step;
        let idx = position + i;
        out[idx] =
            ((alpha * out[idx] as i32 + (16384 - alpha) * append[i] as i32 + 8192) >> 14) as i16;
    }
    out.extend_from_slice(&append[fade_length..]);
}

/// `Accelerate::Process`. `input` is the mono 30 ms analysis window; the
/// stretched samples are written into `output`.
pub(crate) fn accelerate_process(
    input: &[i16],
    fast_mode: bool,
    background_noise: &BackgroundNoise,
    output: &mut Vec<i16>,
) -> StretchResult {
    output.clear();
    let analysis = analyze(input, background_noise);
    // Accelerate::SetParametersForPassiveSpeech sets correlation to zero.
    let best_correlation = if analysis.active_speech {
        analysis.best_correlation
    } else {
        0
    };
    let peak_index = analysis.peak_index;
    let active_speech = analysis.active_speech;

    let threshold = if fast_mode {
        8192
    } else {
        CORRELATION_THRESHOLD
    };
    if (best_correlation as i32) > threshold || !active_speech {
        // The reported length change is the original peak; fast mode only
        // rescales the local copy used to build the stretched output.
        let stretch_peak = if fast_mode {
            (FS_MULT_120 / peak_index) * peak_index
        } else {
            peak_index
        };
        output.extend_from_slice(&input[..FS_MULT_120]);
        let temp = &input[FS_MULT_120..FS_MULT_120 + stretch_peak];
        cross_fade(output, temp, stretch_peak);
        output.extend_from_slice(&input[FS_MULT_120 + stretch_peak..]);
        let return_code = if active_speech {
            ReturnCode::Success
        } else {
            ReturnCode::SuccessLowEnergy
        };
        StretchResult {
            return_code,
            length_change_samples: peak_index,
        }
    } else {
        output.extend_from_slice(input);
        StretchResult {
            return_code: ReturnCode::NoStretch,
            length_change_samples: 0,
        }
    }
}

/// `PreemptiveExpand::Process`. `old_data_length` and `overlap_samples` come
/// from the caller, as in the reference.
pub(crate) fn preemptive_expand_process(
    input: &[i16],
    old_data_length: usize,
    overlap_samples: usize,
    background_noise: &BackgroundNoise,
    output: &mut Vec<i16>,
) -> StretchResult {
    output.clear();
    // Length guard from PreemptiveExpand::Process (mono).
    let input_length = input.len();
    if input_length < (2 * 120 - 1) * FS_MULT || old_data_length >= input_length - overlap_samples {
        output.extend_from_slice(input);
        return StretchResult {
            return_code: ReturnCode::Error,
            length_change_samples: 0,
        };
    }

    let analysis = analyze(input, background_noise);
    let active_speech = analysis.active_speech;
    let (best_correlation, peak_index) = if active_speech {
        (analysis.best_correlation, analysis.peak_index)
    } else {
        // PreemptiveExpand::SetParametersForPassiveSpeech.
        (0, analysis.peak_index.min(input_length - old_data_length))
    };

    if ((best_correlation as i32) > CORRELATION_THRESHOLD && old_data_length <= FS_MULT_120)
        || !active_speech
    {
        let unmodified_length = old_data_length.max(FS_MULT_120);
        output.extend_from_slice(&input[..unmodified_length + peak_index]);
        let temp = &input[unmodified_length - peak_index..unmodified_length];
        cross_fade(output, temp, peak_index);
        output.extend_from_slice(&input[unmodified_length..]);
        let return_code = if active_speech {
            ReturnCode::Success
        } else {
            ReturnCode::SuccessLowEnergy
        };
        StretchResult {
            return_code,
            length_change_samples: peak_index,
        }
    } else {
        output.extend_from_slice(input);
        StretchResult {
            return_code: ReturnCode::NoStretch,
            length_change_samples: 0,
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

    fn check(result: &StretchResult, output: &[i16], name: &str) {
        let mut got = vec![
            result.return_code as i64,
            result.length_change_samples as i64,
            output.len() as i64,
        ];
        got.extend(output.iter().map(|&x| x as i64));
        assert_eq!(got, load(name));
    }

    #[test]
    fn accelerate_matches_oracle() {
        let input = as_i16(load("accelerate_in"));
        let bg = BackgroundNoise::new();
        let mut output = Vec::new();
        let result = accelerate_process(&input, false, &bg, &mut output);
        check(&result, &output, "accelerate_normal");
        let result = accelerate_process(&input, true, &bg, &mut output);
        check(&result, &output, "accelerate_fast");
    }

    #[test]
    fn preemptive_expand_matches_oracle() {
        let input = as_i16(load("preemptive_expand_in"));
        let bg = BackgroundNoise::new();
        let mut output = Vec::new();
        let result = preemptive_expand_process(&input, 480, 240, &bg, &mut output);
        check(&result, &output, "preemptive_expand_normal");
    }
}
