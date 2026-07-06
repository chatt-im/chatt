//! A faithful port of WebRTC's NetEQ control plane for the live playback path.
//!
//! NetEQ drives playback from a timestamp-keyed packet buffer and a sync buffer
//! of decoded PCM: [`decision_logic`](self) selects an operation
//! (normal/expand/merge/accelerate/preemptive-expand) from a status snapshot,
//! and the `GetAudio` loop executes it. This replaces Chatt's earlier
//! queue-depth/arrival-delay heuristics.
//!
//! Modules are introduced bottom-up. Built so far:
//! - [`tick_timer`] — the 10 ms tick counter behind stopwatches/countdowns.
//! - [`packet`] — the priority-ranked, wrap-aware packet type.
//! - [`packet_buffer`] — the sorted buffer the decision logic reads from.
//! - [`redundancy`] — DRED/FEC expansion at insertion (priority 0/1/2).

mod buffer_level_filter;
mod core;
mod decision_logic;
mod delay_constraints;
mod delay_manager;
mod delay_optimizer;
mod dsp;
mod histogram;
mod neteq_status;
mod operation;
mod packet;
mod packet_arrival_history;
mod packet_buffer;
mod redundancy;
#[cfg(test)]
mod replay_repro;
mod sync_buffer;
mod tick_timer;

pub(crate) use core::{AudioResult, NetEqCore, NetEqDiagnostics, NetEqPreparedPacket};
#[cfg(test)]
pub(crate) use operation::Mode;
pub(crate) use packet::Packet;
pub(crate) use packet_buffer::PACKET_TRASH_CAPACITY;
