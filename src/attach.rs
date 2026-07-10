//! Thin-client shim: attaches to a running master over the control socket,
//! forwards the terminal file descriptors, then relays signals until detach.
//!
//! Populated in the multi-client attach phase.
