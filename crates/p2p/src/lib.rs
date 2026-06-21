//! Deterministic UDP NAT traversal primitives for chatt.
//!
//! The crate intentionally keeps the ICE-like state machine independent from
//! sockets and timers. The application owns the UDP socket, feeds inbound STUN
//! packets into [`TraversalAgent`], and sends the returned [`Action`] packets.

pub mod agent;
pub mod candidate;
pub mod interfaces;
pub mod nat;
pub mod restart;
pub mod socket;
pub mod stun;

#[cfg(test)]
mod sim;

pub use agent::{
    Action, AgentConfig, FallbackReason, IceRole, RestartReason, SelectedPair, TraversalAgent,
};
pub use candidate::{Candidate, CandidateKind, CandidatePairId, NatKind, NetworkFamily};
pub use nat::{NatClassifier, ReflexiveObservation};
pub use restart::RestartPortPolicy;
pub use stun::{MessageClass, MessageKind, StunMessage, TransactionId};
