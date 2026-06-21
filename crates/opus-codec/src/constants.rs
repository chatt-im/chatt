//! Crate-wide constants and small helpers

use crate::types::SampleRate;

/// Maximum samples per channel in a single Opus frame at 48 kHz.
///
/// 120 ms at 48 kHz = 0.120 * 48000 = 5760 samples.
pub const MAX_FRAME_SAMPLES_48KHZ: usize = 5760;

/// Maximum packet duration in milliseconds.
pub const MAX_PACKET_DURATION_MS: usize = 120;

/// Compute the maximum samples per channel for a frame at the given `sample_rate`.
#[must_use]
pub const fn max_frame_samples_for(sample_rate: SampleRate) -> usize {
    // Scale linearly from the 48 kHz base.
    // sample_rate.as_i32() is always positive given valid SampleRate enum values
    (MAX_FRAME_SAMPLES_48KHZ * (sample_rate as usize)) / 48_000
}

/// Number of samples per channel in a 2.5 ms frame at the given `sample_rate`.
///
/// libopus requires PLC/FEC and DRED frame sizes to be multiples of this value.
#[must_use]
pub const fn samples_per_2_5ms(sample_rate: SampleRate) -> usize {
    (sample_rate as usize) / 400
}

/// Returns `true` when `frame_size` is a multiple of 2.5 ms at `sample_rate`.
#[must_use]
pub const fn is_frame_size_2_5ms_aligned(frame_size: usize, sample_rate: SampleRate) -> bool {
    let quant = samples_per_2_5ms(sample_rate);
    quant > 0 && frame_size.is_multiple_of(quant)
}
