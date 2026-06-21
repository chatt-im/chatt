//! Common types and constants used by the Opus codec

use crate::bindings::{
    OPUS_APPLICATION_AUDIO, OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_APPLICATION_VOIP, OPUS_AUTO,
    OPUS_BANDWIDTH_FULLBAND, OPUS_BANDWIDTH_MEDIUMBAND, OPUS_BANDWIDTH_NARROWBAND,
    OPUS_BANDWIDTH_SUPERWIDEBAND, OPUS_BANDWIDTH_WIDEBAND, OPUS_BITRATE_MAX, OPUS_FRAMESIZE_2_5_MS,
    OPUS_FRAMESIZE_5_MS, OPUS_FRAMESIZE_10_MS, OPUS_FRAMESIZE_20_MS, OPUS_FRAMESIZE_40_MS,
    OPUS_FRAMESIZE_60_MS, OPUS_FRAMESIZE_80_MS, OPUS_FRAMESIZE_100_MS, OPUS_FRAMESIZE_120_MS,
    OPUS_FRAMESIZE_ARG, OPUS_SIGNAL_MUSIC, OPUS_SIGNAL_VOICE,
};

/// Encoder application mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Application {
    /// Optimize for conversational speech.
    #[default]
    Voip = OPUS_APPLICATION_VOIP as isize,
    /// Optimize for general audio/music.
    Audio = OPUS_APPLICATION_AUDIO as isize,
    /// Low-delay mode, reduced algorithmic delay.
    RestrictedLowDelay = OPUS_APPLICATION_RESTRICTED_LOWDELAY as isize,
}

/// Audio channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    /// Single-channel audio.
    Mono = 1,
    /// Two-channel interleaved audio.
    Stereo = 2,
}

impl Channels {
    /// As `usize`.
    #[must_use]
    pub const fn as_usize(self) -> usize {
        self as usize
    }

    /// As `i32`.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Supported input/output sample rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SampleRate {
    /// 8 kHz.
    Hz8000 = 8000,
    /// 12 kHz.
    Hz12000 = 12000,
    /// 16 kHz.
    Hz16000 = 16000,
    /// 24 kHz.
    Hz24000 = 24000,
    /// 48 kHz.
    #[default]
    Hz48000 = 48000,
}

impl SampleRate {
    /// As `i32`.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Return true if the sample rate is valid for Opus.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        matches!(
            self,
            Self::Hz8000 | Self::Hz12000 | Self::Hz16000 | Self::Hz24000 | Self::Hz48000
        )
    }
}

/// Coded bandwidth classifications in packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bandwidth {
    /// 4 kHz bandpass.
    Narrowband = OPUS_BANDWIDTH_NARROWBAND as isize,
    /// 6 kHz bandpass.
    Mediumband = OPUS_BANDWIDTH_MEDIUMBAND as isize,
    /// 8 kHz bandpass.
    Wideband = OPUS_BANDWIDTH_WIDEBAND as isize,
    /// 12 kHz bandpass.
    SuperWideband = OPUS_BANDWIDTH_SUPERWIDEBAND as isize,
    /// 20 kHz bandpass.
    Fullband = OPUS_BANDWIDTH_FULLBAND as isize,
}

/// Convenience frame sizes in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSize {
    /// 2.5 ms.
    Ms2_5 = 25,
    /// 5 ms.
    Ms5 = 50,
    /// 10 ms.
    Ms10 = 100,
    /// 20 ms.
    Ms20 = 200,
    /// 40 ms.
    Ms40 = 400,
    /// 60 ms.
    Ms60 = 600,
}

impl FrameSize {
    /// Number of samples for this duration at `sample_rate`.
    #[must_use]
    pub const fn samples(self, sample_rate: SampleRate) -> usize {
        // FrameSize discriminants count 0.1 ms units, so divide by 10_000 to convert to seconds
        (self as usize * (sample_rate as usize)) / 10_000
    }
}

/// Hint the encoder about the type of content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Automatic selection (default).
    Auto = OPUS_AUTO as isize,
    /// Voice-optimized mode.
    Voice = OPUS_SIGNAL_VOICE as isize,
    /// Music/general audio optimized mode.
    Music = OPUS_SIGNAL_MUSIC as isize,
}

/// Expert frame duration settings for the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpertFrameDuration {
    /// Select frame size from the argument (default).
    Auto = OPUS_FRAMESIZE_ARG as isize,
    /// 2.5 ms.
    Ms2_5 = OPUS_FRAMESIZE_2_5_MS as isize,
    /// 5 ms.
    Ms5 = OPUS_FRAMESIZE_5_MS as isize,
    /// 10 ms.
    Ms10 = OPUS_FRAMESIZE_10_MS as isize,
    /// 20 ms.
    Ms20 = OPUS_FRAMESIZE_20_MS as isize,
    /// 40 ms.
    Ms40 = OPUS_FRAMESIZE_40_MS as isize,
    /// 60 ms.
    Ms60 = OPUS_FRAMESIZE_60_MS as isize,
    /// 80 ms.
    Ms80 = OPUS_FRAMESIZE_80_MS as isize,
    /// 100 ms.
    Ms100 = OPUS_FRAMESIZE_100_MS as isize,
    /// 120 ms.
    Ms120 = OPUS_FRAMESIZE_120_MS as isize,
}

/// Encoder complexity wrapper in the range 0..=10.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Complexity(u32);

impl Complexity {
    /// Create a new complexity value in range 0..=10.
    ///
    /// # Panics
    /// Panics when `complexity` is greater than 10.
    #[must_use]
    pub const fn new(complexity: u32) -> Self {
        assert!(complexity <= 10, "Complexity must be between 0 and 10");
        Self(complexity)
    }

    /// Raw complexity value.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl Default for Complexity {
    fn default() -> Self {
        Self::new(10)
    }
}

/// Bitrate control options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bitrate {
    /// Let the encoder choose.
    Auto,
    /// Maximum allowed.
    Max,
    /// Explicit bits-per-second.
    Custom(i32),
}

impl Bitrate {
    /// Convert to libopus `i32` value.
    #[must_use]
    pub const fn value(self) -> i32 {
        match self {
            Self::Auto => OPUS_AUTO,
            Self::Max => OPUS_BITRATE_MAX,
            Self::Custom(bps) => bps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_samples_are_correct() {
        assert_eq!(FrameSize::Ms20.samples(SampleRate::Hz48000), 960);
        assert_eq!(FrameSize::Ms5.samples(SampleRate::Hz16000), 80);
        assert_eq!(FrameSize::Ms2_5.samples(SampleRate::Hz8000), 20);
    }
}
