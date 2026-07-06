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
    Dtmf,
    Undefined,
}

impl Operation {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Operation::Normal => "normal",
            Operation::Merge => "merge",
            Operation::Expand => "expand",
            Operation::Accelerate => "accelerate",
            Operation::FastAccelerate => "fast_accelerate",
            Operation::PreemptiveExpand => "preemptive_expand",
            Operation::Rfc3389Cng => "rfc3389_cng",
            Operation::Rfc3389CngNoPacket => "rfc3389_cng_no_packet",
            Operation::Dtmf => "dtmf",
            Operation::Undefined => "undefined",
        }
    }
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
    Error,
}

impl Mode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Expand => "expand",
            Mode::Merge => "merge",
            Mode::AccelerateSuccess => "accelerate_success",
            Mode::AccelerateLowEnergy => "accelerate_low_energy",
            Mode::AccelerateFail => "accelerate_fail",
            Mode::PreemptiveExpandSuccess => "preemptive_expand_success",
            Mode::PreemptiveExpandLowEnergy => "preemptive_expand_low_energy",
            Mode::PreemptiveExpandFail => "preemptive_expand_fail",
            Mode::Rfc3389Cng => "rfc3389_cng",
            Mode::Error => "error",
        }
    }

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
        matches!(self, Mode::Rfc3389Cng)
    }

    /// Whether the mode is concealment (expand or codec PLC). Port of `IsExpand`.
    pub(crate) fn is_expand(self) -> bool {
        matches!(self, Mode::Expand)
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
