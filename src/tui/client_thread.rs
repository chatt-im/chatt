//! Per-client render thread: drives extui on a client's terminal file
//! descriptors, owning that client's [`crate::tui::view::ClientView`].
//!
//! Populated in the daemon-core phase.
