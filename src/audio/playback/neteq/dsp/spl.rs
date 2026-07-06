//! Fixed-point signal-processing primitives ported from WebRTC's
//! `common_audio/signal_processing` (and the neteq `cross_correlation`).
//!
//! Every routine reproduces the reference integer arithmetic exactly, including
//! Q-format scaling, per-sample right shifts, rounding adds, and the 2's
//! complement overflow the C code relies on (`wrapping_*` where the original is
//! marked `RTC_NO_SANITIZE("signed-integer-overflow")` or otherwise overflows
//! by design). Outputs are bit-identical to the C build.

/// `WebRtcSpl_CountLeadingZeros32`: 32 for zero, else the leading-zero count.
fn count_leading_zeros32(n: u32) -> i32 {
    n.leading_zeros() as i32
}

/// `WebRtcSpl_NormW32`: left shifts available before `a` overflows int32.
pub(crate) fn norm_w32(a: i32) -> i16 {
    if a == 0 {
        return 0;
    }
    let masked = if a < 0 { !a } else { a } as u32;
    (count_leading_zeros32(masked) - 1) as i16
}

/// `WebRtcSpl_GetSizeInBits`: index of the most significant set bit plus one.
pub(crate) fn get_size_in_bits(n: u32) -> i16 {
    (32 - count_leading_zeros32(n)) as i16
}

/// `WEBRTC_SPL_ABS_W32`. Matches the C overflow at `i32::MIN` (wraps to itself).
fn abs_w32(a: i32) -> i32 {
    if a >= 0 { a } else { a.wrapping_neg() }
}

/// `WebRtcSpl_MaxAbsValueW16C`: max `|x|`, clamped to `i16::MAX`.
pub(crate) fn max_abs_value_w16(vector: &[i16]) -> i16 {
    let mut maximum: i32 = 0;
    for &v in vector {
        let absolute = (v as i32).abs();
        if absolute > maximum {
            maximum = absolute;
        }
    }
    if maximum > i16::MAX as i32 {
        maximum = i16::MAX as i32;
    }
    maximum as i16
}

/// `WebRtcSpl_MaxAbsValueW32C`: max `|x|`, clamped to `i32::MAX`.
pub(crate) fn max_abs_value_w32(vector: &[i32]) -> i32 {
    let mut maximum: u32 = 0;
    for &v in vector {
        let absolute = if v != i32::MIN {
            (v as i64).unsigned_abs() as u32
        } else {
            i32::MAX as u32 + 1
        };
        if absolute > maximum {
            maximum = absolute;
        }
    }
    if maximum > i32::MAX as u32 {
        maximum = i32::MAX as u32;
    }
    maximum as i32
}

/// `WebRtcSpl_MinMaxW16`: signed minimum and maximum of the vector.
fn min_max_w16(vector: &[i16]) -> (i16, i16) {
    let mut minimum = i16::MAX;
    let mut maximum = i16::MIN;
    for &v in vector {
        if v < minimum {
            minimum = v;
        }
        if v > maximum {
            maximum = v;
        }
    }
    (minimum, maximum)
}

/// `WebRtcSpl_MaxAbsElementW16`: the signed element with the largest magnitude.
pub(crate) fn max_abs_element_w16(vector: &[i16]) -> i16 {
    let (min_val, max_val) = min_max_w16(vector);
    if min_val == max_val || (min_val as i32) < -(max_val as i32) {
        min_val
    } else {
        max_val
    }
}

/// `WebRtcSpl_DivW32W16`: truncating division, `i32::MAX` on divide-by-zero.
pub(crate) fn div_w32w16(num: i32, den: i16) -> i32 {
    if den != 0 {
        num / (den as i32)
    } else {
        i32::MAX
    }
}

/// `WebRtcSpl_DivW32HiLow`: `num / den` in Q31 where `den` is split hi/low.
pub(crate) fn div_w32_hi_low(num: i32, den_hi: i16, den_low: i16) -> i32 {
    let approx = div_w32w16(0x1FFF_FFFF, den_hi) as i16;

    // den * approx (Q30).
    let mut tmp = ((den_hi as i32).wrapping_mul(approx as i32) << 1)
        .wrapping_add(((den_low as i32).wrapping_mul(approx as i32) >> 15) << 1);
    // 2.0 - den * approx in Q30.
    tmp = (0x7FFF_FFFF_i64 - tmp as i64) as i32;

    let mut tmp_hi = (tmp >> 16) as i16;
    let mut tmp_low = ((tmp - ((tmp_hi as i32) << 16)) >> 1) as i16;

    // 1/den in Q29.
    tmp = ((tmp_hi as i32).wrapping_mul(approx as i32)
        + ((tmp_low as i32).wrapping_mul(approx as i32) >> 15))
        << 1;

    tmp_hi = (tmp >> 16) as i16;
    tmp_low = ((tmp - ((tmp_hi as i32) << 16)) >> 1) as i16;

    let num_hi = (num >> 16) as i16;
    let num_low = ((num - ((num_hi as i32) << 16)) >> 1) as i16;

    // num * (1/den), Q28.
    tmp = (num_hi as i32).wrapping_mul(tmp_hi as i32)
        + ((num_hi as i32).wrapping_mul(tmp_low as i32) >> 15)
        + ((num_low as i32).wrapping_mul(tmp_hi as i32) >> 15);

    // Convert Q28 -> Q31.
    tmp << 3
}

/// `WebRtcSpl_SqrtFloor`: integer floor of the square root (0 for negatives).
pub(crate) fn sqrt_floor(value: i32) -> i32 {
    let mut root: i32 = 0;
    let mut value = value;
    // WEBRTC_SPL_SQRT_ITER unrolled from bit 15 down to 0.
    let mut n = 15i32;
    loop {
        let try1 = root + (1 << n);
        if value >= (try1 << n) {
            value -= try1 << n;
            root |= 2 << n;
        }
        if n == 0 {
            break;
        }
        n -= 1;
    }
    root >> 1
}

/// `WebRtcSpl_DotProductWithScale`: sum of `(v1*v2) >> scaling`, saturated to
/// int32. The accumulator is 64-bit.
pub(crate) fn dot_product_with_scale(
    vector1: &[i16],
    vector2: &[i16],
    length: usize,
    scaling: i32,
) -> i32 {
    let mut sum: i64 = 0;
    for i in 0..length {
        sum += (((vector1[i] as i32) * (vector2[i] as i32)) >> scaling) as i64;
    }
    sum.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

/// `WebRtcSpl_CrossCorrelationC` with an explicit `seq2` base index so callers
/// can use a negative `step` into a shared buffer (as `time_stretch` does).
/// `seq1[j]` is read for `j in 0..dim_seq`; `seq2` is read at
/// `seq2_base + i*step + j`.
pub(crate) fn cross_correlation_at(
    cross_correlation: &mut [i32],
    seq1: &[i16],
    seq2: &[i16],
    seq2_base: isize,
    dim_seq: usize,
    dim_cross_correlation: usize,
    right_shifts: i32,
    step_seq2: isize,
) {
    for i in 0..dim_cross_correlation {
        let mut corr: i32 = 0;
        let lag_base = seq2_base + (i as isize) * step_seq2;
        for j in 0..dim_seq {
            let idx = (lag_base + j as isize) as usize;
            corr = corr.wrapping_add(((seq1[j] as i32) * (seq2[idx] as i32)) >> right_shifts);
        }
        cross_correlation[i] = corr;
    }
}

/// `WebRtcSpl_CrossCorrelation` for the contiguous, forward-stepping case.
#[cfg(test)]
pub(crate) fn cross_correlation(
    cross_correlation: &mut [i32],
    seq1: &[i16],
    seq2: &[i16],
    dim_seq: usize,
    dim_cross_correlation: usize,
    right_shifts: i32,
    step_seq2: isize,
) {
    cross_correlation_at(
        cross_correlation,
        seq1,
        seq2,
        0,
        dim_seq,
        dim_cross_correlation,
        right_shifts,
        step_seq2,
    );
}

/// `CrossCorrelationWithAutoShift` (neteq): picks an overflow-safe scaling from
/// the input magnitudes, runs the correlation, and returns the scaling applied.
/// `seq2_base` is the index in `seq2` of the C `sequence_2` pointer.
pub(crate) fn cross_correlation_with_auto_shift(
    seq1: &[i16],
    seq2: &[i16],
    seq2_base: isize,
    sequence_1_length: usize,
    cross_correlation_length: usize,
    cross_correlation_step: isize,
    cross_correlation: &mut [i32],
) -> i32 {
    let max_1 = max_abs_element_w16(&seq1[..sequence_1_length]);
    let sequence_2_shift = cross_correlation_step * (cross_correlation_length as isize - 1);
    let sequence_2_start = if sequence_2_shift >= 0 {
        seq2_base
    } else {
        seq2_base + sequence_2_shift
    };
    let sequence_2_length = sequence_1_length + sequence_2_shift.unsigned_abs();
    let seq2_region =
        &seq2[sequence_2_start as usize..(sequence_2_start as usize + sequence_2_length)];
    let max_2 = max_abs_element_w16(seq2_region);

    let max_value = (max_1 as i32 * max_2 as i32).unsigned_abs() as i64 * sequence_1_length as i64;
    let factor = (max_value >> 31) as i32;
    let scaling = if factor == 0 {
        0
    } else {
        31 - norm_w32(factor) as i32
    };

    cross_correlation_at(
        cross_correlation,
        seq1,
        seq2,
        seq2_base,
        sequence_1_length,
        cross_correlation_length,
        scaling,
        cross_correlation_step,
    );
    scaling
}

/// `WebRtcSpl_AutoCorrelation`: scaled autocorrelation for lags `0..=order`.
/// Returns the per-sample right shift via `scale`.
#[cfg(test)]
pub(crate) fn auto_correlation(
    in_vector: &[i16],
    order: usize,
    result: &mut [i32],
    scale: &mut i32,
) -> usize {
    let in_vector_length = in_vector.len();
    let smax = max_abs_value_w16(in_vector);
    let scaling = if smax == 0 {
        0
    } else {
        let nbits = get_size_in_bits(in_vector_length as u32) as i32;
        let t = norm_w32((smax as i32) * (smax as i32)) as i32;
        if t > nbits { 0 } else { nbits - t }
    };

    for i in 0..order + 1 {
        let mut sum: i32 = 0;
        for j in 0..in_vector_length - i {
            sum = sum.wrapping_add(((in_vector[j] as i32) * (in_vector[i + j] as i32)) >> scaling);
        }
        result[i] = sum;
    }
    *scale = scaling;
    order + 1
}

/// `WebRtcSpl_LevinsonDurbin`: solve for Q12 LPC coefficients `a` and Q15
/// reflection coefficients `k`. Returns 1 if the filter is stable, else 0.
/// Reproduces the hi/low fixed-point arithmetic and its intentional overflow.
pub(crate) fn levinson_durbin(r: &[i32], a: &mut [i16], k: &mut [i16], order: usize) -> i16 {
    const MAXORDER: usize = 20;
    let mut r_hi = [0i16; MAXORDER + 1];
    let mut r_low = [0i16; MAXORDER + 1];
    let mut a_hi = [0i16; MAXORDER + 1];
    let mut a_low = [0i16; MAXORDER + 1];
    let mut a_upd_hi = [0i16; MAXORDER + 1];
    let mut a_upd_low = [0i16; MAXORDER + 1];

    let norm = norm_w32(r[0]);

    for i in 0..=order {
        let temp1 = (r[i] as i32).wrapping_mul(1 << norm);
        r_hi[i] = (temp1 >> 16) as i16;
        r_low[i] = ((temp1 - ((r_hi[i] as i32) * 65536)) >> 1) as i16;
    }

    // K = A[1] = -R[1] / R[0].
    let temp2 = (r[1] as i32).wrapping_mul(1 << norm);
    let temp3 = abs_w32(temp2);
    let mut temp1 = div_w32_hi_low(temp3, r_hi[0], r_low[0]);
    if temp2 > 0 {
        temp1 = -temp1;
    }

    let mut k_hi = (temp1 >> 16) as i16;
    let mut k_low = ((temp1 - ((k_hi as i32) * 65536)) >> 1) as i16;
    k[0] = k_hi;

    temp1 >>= 4; // A[1] in Q27.
    a_hi[1] = (temp1 >> 16) as i16;
    a_low[1] = ((temp1 - ((a_hi[1] as i32) * 65536)) >> 1) as i16;

    // Alpha = R[0] * (1 - K^2).
    let mut temp1 = (((k_hi as i32).wrapping_mul(k_low as i32) >> 14)
        + (k_hi as i32).wrapping_mul(k_hi as i32))
    .wrapping_mul(2);
    temp1 = abs_w32(temp1);
    temp1 = 0x7FFF_FFFF - temp1;

    let mut tmp_hi = (temp1 >> 16) as i16;
    let mut tmp_low = ((temp1 - ((tmp_hi as i32) << 16)) >> 1) as i16;

    let mut temp1 = ((r_hi[0] as i32).wrapping_mul(tmp_hi as i32)
        + ((r_hi[0] as i32).wrapping_mul(tmp_low as i32) >> 15)
        + ((r_low[0] as i32).wrapping_mul(tmp_hi as i32) >> 15))
        << 1;

    let mut alpha_exp = norm_w32(temp1);
    temp1 = temp1.wrapping_shl(alpha_exp as u32);
    let mut alpha_hi = (temp1 >> 16) as i16;
    let mut alpha_low = ((temp1 - ((alpha_hi as i32) << 16)) >> 1) as i16;

    for i in 2..=order {
        let mut temp1: i32 = 0;
        for j in 1..i {
            temp1 = temp1.wrapping_add(
                (r_hi[j] as i32)
                    .wrapping_mul(a_hi[i - j] as i32)
                    .wrapping_mul(2)
                    + (((r_hi[j] as i32).wrapping_mul(a_low[i - j] as i32) >> 15)
                        + ((r_low[j] as i32).wrapping_mul(a_hi[i - j] as i32) >> 15))
                        .wrapping_mul(2),
            );
        }

        temp1 = temp1.wrapping_mul(16);
        temp1 = temp1.wrapping_add(((r_hi[i] as i32) * 65536).wrapping_add((r_low[i] as i32) << 1));

        let temp2 = abs_w32(temp1);
        let mut temp3 = div_w32_hi_low(temp2, alpha_hi, alpha_low);
        if temp1 > 0 {
            temp3 = -temp3;
        }

        let norm = norm_w32(temp3);
        if alpha_exp <= norm || temp3 == 0 {
            temp3 = temp3.wrapping_mul(1 << alpha_exp);
        } else if temp3 > 0 {
            temp3 = 0x7FFF_FFFF;
        } else {
            temp3 = i32::MIN;
        }

        k_hi = (temp3 >> 16) as i16;
        k_low = ((temp3 - ((k_hi as i32) * 65536)) >> 1) as i16;
        k[i - 1] = k_hi;

        if (k_hi as i32).abs() > 32750 {
            return 0; // Unstable filter.
        }

        for j in 1..i {
            let mut t = ((a_hi[j] as i32) * 65536).wrapping_add((a_low[j] as i32) << 1);
            t = t.wrapping_add(
                ((k_hi as i32).wrapping_mul(a_hi[i - j] as i32)
                    + ((k_hi as i32).wrapping_mul(a_low[i - j] as i32) >> 15)
                    + ((k_low as i32).wrapping_mul(a_hi[i - j] as i32) >> 15))
                    .wrapping_mul(2),
            );
            a_upd_hi[j] = (t >> 16) as i16;
            a_upd_low[j] = ((t - ((a_upd_hi[j] as i32) * 65536)) >> 1) as i16;
        }

        temp3 >>= 4; // K in Q27.
        a_upd_hi[i] = (temp3 >> 16) as i16;
        a_upd_low[i] = ((temp3 - ((a_upd_hi[i] as i32) * 65536)) >> 1) as i16;

        // Alpha = Alpha * (1 - K^2).
        let mut temp1 = (((k_hi as i32).wrapping_mul(k_low as i32) >> 14)
            + (k_hi as i32).wrapping_mul(k_hi as i32))
        .wrapping_mul(2);
        temp1 = abs_w32(temp1);
        temp1 = 0x7FFF_FFFF - temp1;

        tmp_hi = (temp1 >> 16) as i16;
        tmp_low = ((temp1 - ((tmp_hi as i32) << 16)) >> 1) as i16;

        temp1 = ((alpha_hi as i32).wrapping_mul(tmp_hi as i32)
            + ((alpha_hi as i32).wrapping_mul(tmp_low as i32) >> 15)
            + ((alpha_low as i32).wrapping_mul(tmp_hi as i32) >> 15))
            << 1;

        let norm = norm_w32(temp1);
        temp1 = temp1.wrapping_shl(norm as u32);
        alpha_hi = (temp1 >> 16) as i16;
        alpha_low = ((temp1 - ((alpha_hi as i32) << 16)) >> 1) as i16;
        alpha_exp += norm;

        for j in 1..=i {
            a_hi[j] = a_upd_hi[j];
            a_low[j] = a_upd_low[j];
        }
    }

    a[0] = 4096;
    for i in 1..=order {
        let temp1 = ((a_hi[i] as i32) * 65536).wrapping_add((a_low[i] as i32) << 1);
        a[i] = ((temp1.wrapping_mul(2) + 32768) >> 16) as i16;
    }
    1
}

/// `WEBRTC_SPL_SHIFT_W32`: left shift for non-negative `c`, else right shift.
pub(crate) fn shift_w32(x: i32, c: i32) -> i32 {
    if c >= 0 {
        x.wrapping_mul(1 << c)
    } else {
        x >> -c
    }
}

/// `WebRtcSpl_AffineTransformVector`: `out[i] = (in[i]*gain + add) >> shifts`.
pub(crate) fn affine_transform_vector(
    out: &mut [i16],
    in_vector: &[i16],
    gain: i16,
    add_constant: i32,
    right_shifts: i16,
    vector_length: usize,
) {
    for i in 0..vector_length {
        out[i] = (((in_vector[i] as i32).wrapping_mul(gain as i32)).wrapping_add(add_constant)
            >> right_shifts) as i16;
    }
}

/// `WebRtcSpl_ScaleAndAddVectorsWithRoundC`: rounded weighted sum of two vectors.
pub(crate) fn scale_and_add_vectors_with_round(
    in_vector1: &[i16],
    in_vector1_scale: i16,
    in_vector2: &[i16],
    in_vector2_scale: i16,
    right_shifts: i32,
    out_vector: &mut [i16],
    length: usize,
) {
    let round_value = (1 << right_shifts) >> 1;
    for i in 0..length {
        out_vector[i] = (((in_vector1[i] as i32).wrapping_mul(in_vector1_scale as i32)
            + (in_vector2[i] as i32).wrapping_mul(in_vector2_scale as i32)
            + round_value)
            >> right_shifts) as i16;
    }
}

/// `WebRtcSpl_FilterMAFastQ12`: FIR filter with Q12 `b` coefficients. `in_buf` is
/// read at `in_base + i - j` to keep the C negative-index state in bounds.
pub(crate) fn filter_ma_fast_q12(
    in_buf: &[i16],
    in_base: isize,
    out: &mut [i16],
    b: &[i16],
    length: usize,
) {
    let b_length = b.len();
    for i in 0..length {
        let mut o: i32 = 0;
        for j in 0..b_length {
            let idx = (in_base + i as isize - j as isize) as usize;
            o = o.wrapping_add((b[j] as i32).wrapping_mul(in_buf[idx] as i32));
        }
        o = o.clamp(-134_217_728, 134_215_679);
        out[i] = ((o + 2048) >> 12) as i16;
    }
}

/// `WebRtcSpl_SatW32ToW16`: clamp an int32 into the int16 range.
pub(crate) fn sat_w32_to_w16(value32: i32) -> i16 {
    value32.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}

/// `WebRtcSpl_VectorBitShiftW32ToW16`: shift each int32 and saturate to int16.
pub(crate) fn vector_bit_shift_w32_to_w16(
    out: &mut [i16],
    length: usize,
    in_vector: &[i32],
    right_shifts: i32,
) {
    if right_shifts >= 0 {
        for i in 0..length {
            out[i] = sat_w32_to_w16(in_vector[i] >> right_shifts);
        }
    } else {
        let left = -right_shifts;
        for i in 0..length {
            out[i] = sat_w32_to_w16(in_vector[i].wrapping_shl(left as u32));
        }
    }
}

/// `WebRtcSpl_MaxIndexW32`: index of the (first) signed maximum.
pub(crate) fn max_index_w32(vector: &[i32]) -> usize {
    let mut maximum = i32::MIN;
    let mut index = 0;
    for (i, &v) in vector.iter().enumerate() {
        if v > maximum {
            maximum = v;
            index = i;
        }
    }
    index
}

/// `WebRtcSpl_VectorBitShiftW16`: shift each int16 (arithmetic for shifts > 0,
/// left otherwise). Operates in place when `out` and `in_vector` alias via the
/// caller passing the same data through a temporary.
pub(crate) fn vector_bit_shift_w16(
    out: &mut [i16],
    length: usize,
    in_vector: &[i16],
    right_shifts: i16,
) {
    if right_shifts > 0 {
        // C promotes the int16 operand to int before shifting, so a shift of 16
        // (silence -> zero norm) is well defined there; mirror that in i32.
        for i in 0..length {
            out[i] = ((in_vector[i] as i32) >> right_shifts) as i16;
        }
    } else {
        for i in 0..length {
            out[i] = (in_vector[i] as i32).wrapping_mul(1 << (-right_shifts)) as i16;
        }
    }
}

/// `WebRtcSpl_MaxIndexW16`: index of the (first) signed maximum.
pub(crate) fn max_index_w16(vector: &[i16]) -> usize {
    let mut maximum = i16::MIN;
    let mut index = 0;
    for (i, &v) in vector.iter().enumerate() {
        if v > maximum {
            maximum = v;
            index = i;
        }
    }
    index
}

/// `WebRtcSpl_DownsampleFastC`: decimating FIR. `data_in` is read at
/// `in_base + i - j` so the C negative-index filter state stays in bounds.
/// Returns -1 if the input is too short, else 0.
pub(crate) fn downsample_fast(
    data_in: &[i16],
    in_base: isize,
    data_in_length: usize,
    data_out: &mut [i16],
    data_out_length: usize,
    coefficients: &[i16],
    factor: usize,
    delay: usize,
) -> i32 {
    let coefficients_length = coefficients.len();
    let endpos = delay + factor * (data_out_length - 1) + 1;
    if data_out_length == 0 || coefficients_length == 0 || data_in_length < endpos {
        return -1;
    }
    let mut out_index = 0;
    let mut i = delay;
    while i < endpos {
        let mut out_s32: i32 = 2048; // Round value, 0.5 in Q12.
        for j in 0..coefficients_length {
            let idx = (in_base + i as isize - j as isize) as usize;
            out_s32 =
                out_s32.wrapping_add((coefficients[j] as i32).wrapping_mul(data_in[idx] as i32));
        }
        out_s32 >>= 12; // Q0.
        data_out[out_index] = sat_w32_to_w16(out_s32);
        out_index += 1;
        i += factor;
    }
    0
}

/// `WebRtcSpl_FilterARFastQ12`: all-pole filter with Q12 coefficients.
/// `state_and_out[0..order]` holds the filter state; output for sample `i` is
/// written to `state_and_out[order + i]`, matching the C code's negative-index
/// state layout (`coefficients.len() == order + 1`).
pub(crate) fn filter_ar_fast_q12(
    data_in: &[i16],
    state_and_out: &mut [i16],
    coefficients: &[i16],
    data_length: usize,
) {
    let order = coefficients.len() - 1;
    for i in 0..data_length {
        let mut sum: i64 = 0;
        for j in (1..=order).rev() {
            sum += (coefficients[j] as i64) * (state_and_out[order + i - j] as i64);
        }
        let mut output: i64 = (coefficients[0] as i64) * (data_in[i] as i64);
        output -= sum;
        // WEBRTC_SPL_SAT(134215679, output, -134217728).
        output = output.clamp(-134_217_728, 134_215_679);
        state_and_out[order + i] = ((output + 2048) >> 12) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::neteq::dsp::test_vectors::load;

    fn as_i16(v: Vec<i64>) -> Vec<i16> {
        v.into_iter().map(|x| x as i16).collect()
    }
    fn as_i32(v: Vec<i64>) -> Vec<i32> {
        v.into_iter().map(|x| x as i32).collect()
    }

    #[test]
    fn cross_correlation_matches_oracle() {
        let a = as_i16(load("spl_cross_correlation_in_a"));
        let b = as_i16(load("spl_cross_correlation_in_b"));
        for shift in [0, 3, 6] {
            let mut cc = vec![0i32; 40];
            cross_correlation(&mut cc, &a, &b, 160, 40, shift, 1);
            let expected = as_i32(load(&format!("spl_cross_correlation_shift{shift}")));
            assert_eq!(cc, expected, "shift {shift}");
        }
    }

    #[test]
    fn cross_correlation_auto_shift_matches_oracle() {
        let buf = as_i16(load("spl_cross_correlation_auto_shift_in"));
        let s1_off = 100usize;
        let s2_off = 60usize;
        let mut cc = vec![0i32; 40];
        let scaling = cross_correlation_with_auto_shift(
            &buf[s1_off..],
            &buf,
            s2_off as isize,
            50,
            40,
            -1,
            &mut cc,
        );
        let mut got = vec![scaling];
        got.extend_from_slice(&cc);
        let expected = as_i32(load("spl_cross_correlation_auto_shift"));
        assert_eq!(got, expected);
    }

    #[test]
    fn auto_correlation_matches_oracle() {
        let input = as_i16(load("spl_auto_correlation_in"));
        let order = 8;
        let mut result = vec![0i32; order + 1];
        let mut scale = 0i32;
        auto_correlation(&input, order, &mut result, &mut scale);
        let mut got = vec![scale];
        got.extend_from_slice(&result);
        let expected = as_i32(load("spl_auto_correlation"));
        assert_eq!(got, expected);
    }

    #[test]
    fn levinson_durbin_matches_oracle() {
        let r = as_i32(load("spl_levinson_durbin_in"));
        let order = 6;
        let mut a = vec![0i16; order + 1];
        let mut k = vec![0i16; order];
        let stable = levinson_durbin(&r, &mut a, &mut k, order);
        let mut got = vec![stable as i32];
        got.extend(a.iter().map(|&x| x as i32));
        got.extend(k.iter().map(|&x| x as i32));
        let expected = as_i32(load("spl_levinson_durbin"));
        assert_eq!(got, expected);
    }

    #[test]
    fn filter_ar_fast_q12_matches_oracle() {
        let data_in = as_i16(load("spl_filter_ar_fast_q12_in"));
        let coefs: Vec<i16> = vec![4096, -2048, 1024, -512, 256, -128, 64];
        let order = coefs.len() - 1;
        let len = 160;
        let mut scratch = vec![0i16; order + len];
        for i in 0..order {
            scratch[i] = (100 * i as i32 - 250) as i16;
        }
        filter_ar_fast_q12(&data_in, &mut scratch, &coefs, len);
        let got: Vec<i32> = scratch[order..].iter().map(|&x| x as i32).collect();
        let expected = as_i32(load("spl_filter_ar_fast_q12"));
        assert_eq!(got, expected);
    }

    #[test]
    fn sqrt_floor_matches_oracle() {
        let inputs: [i32; 11] = [
            0, 1, 2, 3, 4, 100, 65535, 65536, 1000000, 16777216, 2000000000,
        ];
        let got: Vec<i32> = inputs.iter().map(|&v| sqrt_floor(v)).collect();
        let expected = as_i32(load("spl_sqrt_floor"));
        assert_eq!(got, expected);
    }

    #[test]
    fn div_w32w16_matches_oracle() {
        let pairs: [(i32, i16); 6] = [
            (1000000, 7),
            (-1000000, 7),
            (123456789, 1234),
            (2147483647, 2),
            (5, 32767),
            (-5, 3),
        ];
        let got: Vec<i32> = pairs.iter().map(|&(n, d)| div_w32w16(n, d)).collect();
        let expected = as_i32(load("spl_div_w32w16"));
        assert_eq!(got, expected);
    }
}
