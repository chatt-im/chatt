#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertOutcome {
    Accepted,
    Late,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EncoderNetworkProfile {
    pub dred_duration_10ms: i32,
    pub bitrate_bps: i32,
    pub packet_loss_percent: i32,
}

impl EncoderNetworkProfile {
    pub(crate) const EXCELLENT: Self = Self {
        dred_duration_10ms: 0,
        bitrate_bps: 32_000,
        packet_loss_percent: 0,
    };
    // DRED needs spare bits beyond the VOIP core. Below ~40 kbps the core
    // consumes the whole budget and DRED reach collapses to under one frame,
    // making recovery impossible. The lossy profiles run at 48-64 kbps so each
    // packet carries multiple frames of usable redundancy.
    pub(crate) const DEGRADED: Self = Self {
        dred_duration_10ms: 100,
        bitrate_bps: 48_000,
        packet_loss_percent: 3,
    };
    pub(crate) const SEVERE: Self = Self {
        dred_duration_10ms: 100,
        bitrate_bps: 64_000,
        packet_loss_percent: 10,
    };
    pub(crate) const CRITICAL: Self = Self {
        dred_duration_10ms: 100,
        bitrate_bps: 64_000,
        packet_loss_percent: 20,
    };
}

pub(crate) trait EncoderNetworkTuning {
    type Error;

    fn apply_network_profile(&mut self, profile: EncoderNetworkProfile) -> Result<(), Self::Error>;
}
