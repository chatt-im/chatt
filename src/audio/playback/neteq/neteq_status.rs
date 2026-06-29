//! The decision inputs — a port of the `NetEqController::NetEqStatus` family in
//! `/tmp/webrtc/api/neteq/neteq_controller.h`.
//!
//! `NetEqStatus` is the snapshot the [`DecisionLogic`](super::decision_logic)
//! consumes each output block: the playout point (`target_timestamp`), the next
//! buffered packet, the previous mode, generated-noise accounting, and the
//! packet-buffer span. `PacketArrivedInfo` is the per-packet update fed to the
//! delay manager.

use super::operation::Mode;

/// Minimal description of the next buffered packet for the decision.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PacketInfo {
    pub timestamp: u32,
    pub is_dtx: bool,
    pub is_cng: bool,
}

/// Aggregate packet-buffer state for the decision.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PacketBufferInfo {
    pub dtx_or_cng: bool,
    pub num_samples: usize,
    pub span_samples: usize,
    pub span_samples_wait_time: usize,
    pub num_packets: usize,
}

/// Full status snapshot consumed by `GetDecision`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NetEqStatus {
    /// The timestamp of the next sample to play out (`sync_buffer.end_timestamp`).
    pub target_timestamp: u32,
    /// Expand mute factor in Q14 (16384 == 1.0).
    pub expand_mutefactor: i16,
    pub last_packet_samples: usize,
    pub next_packet: Option<PacketInfo>,
    pub last_mode: Mode,
    pub play_dtmf: bool,
    pub generated_noise_samples: usize,
    pub packet_buffer_info: PacketBufferInfo,
    /// Future samples already decoded into the sync buffer.
    pub sync_buffer_samples: usize,
}

/// Per-packet update for the delay manager. Port of `PacketArrivedInfo`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PacketArrivedInfo {
    pub packet_length_samples: usize,
    pub main_timestamp: u32,
    pub main_sequence_number: u16,
    pub is_cng_or_dtmf: bool,
    pub is_dtx: bool,
    pub buffer_flush: bool,
}

/// Controller configuration. Port of `NetEqController::Config`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ControllerConfig {
    pub allow_time_stretching: bool,
    pub max_packets_in_buffer: i32,
    pub base_min_delay_ms: i32,
}
