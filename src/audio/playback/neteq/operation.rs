//! NetEQ operation and mode enums — a port of the `NetEq::Operation` and
//! `NetEq::Mode` enums in `/tmp/webrtc/api/neteq/neteq.h`.
//!
//! `Operation` is what the decision logic selects for the next output block;
//! `Mode` is what the `GetAudio` loop reports it actually did, fed back as
//! `last_mode` on the next decision. Chatt has no DTMF or RFC 3389 comfort-noise
//! path, but the variants are kept for a faithful decision tree; they simply
//! never get selected.

/// The action the decision logic selects for the next output block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Normal,
    Merge,
    Expand,
    Accelerate,
    FastAccelerate,
    PreemptiveExpand,
    Rfc3389Cng,
    Rfc3389CngNoPacket,
    CodecInternalCng,
    Dtmf,
    Undefined,
}

/// What the `GetAudio` loop actually did, reported back as `last_mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Normal,
    Expand,
    Merge,
    AccelerateSuccess,
    AccelerateLowEnergy,
    AccelerateFail,
    PreemptiveExpandSuccess,
    PreemptiveExpandLowEnergy,
    PreemptiveExpandFail,
    Rfc3389Cng,
    CodecInternalCng,
    CodecPlc,
    Dtmf,
    Error,
    Undefined,
}

impl Mode {
    /// Whether the mode is a successful time-stretch (accelerate or preemptive
    /// expand), used to arm the timescale refractory. Port of `IsTimestretch`.
    pub(crate) fn is_timestretch(self) -> bool {
        matches!(
            self,
            Mode::AccelerateSuccess
                | Mode::AccelerateLowEnergy
                | Mode::PreemptiveExpandSuccess
                | Mode::PreemptiveExpandLowEnergy
        )
    }

    /// Whether the mode is comfort noise (RFC 3389 or codec-internal). Port of
    /// `IsCng`.
    pub(crate) fn is_cng(self) -> bool {
        matches!(self, Mode::Rfc3389Cng | Mode::CodecInternalCng)
    }

    /// Whether the mode is concealment (expand or codec PLC). Port of `IsExpand`.
    pub(crate) fn is_expand(self) -> bool {
        matches!(self, Mode::Expand | Mode::CodecPlc)
    }

    /// Whether the mode is an accelerate outcome (success or low-energy).
    pub(crate) fn is_accelerate(self) -> bool {
        matches!(self, Mode::AccelerateSuccess | Mode::AccelerateLowEnergy)
    }

    /// Whether the mode is a preemptive-expand outcome (success or low-energy).
    pub(crate) fn is_preemptive_expand(self) -> bool {
        matches!(
            self,
            Mode::PreemptiveExpandSuccess | Mode::PreemptiveExpandLowEnergy
        )
    }
}
