#![cfg_attr(all(test, feature = "nightly-test-spans"), feature(thread_spawn_hook))]
// Windows compiles out the daemon/attach half (see `runtime.rs`), and that
// leaves the whole local-RPC projection surface without a caller: the settings,
// identity and appearance hubs, and the `App` methods that feed them. None of
// it is dead in the ordinary sense, and none of it sits behind a single
// boundary that could be gated instead: the projections are fields and methods
// of the renderer-independent core, deliberately carrying no `cfg` so the core
// reads the same on both platforms. Linux builds every one of them and is
// warning-free, so code that is genuinely unreachable still gets caught.
#![cfg_attr(not(unix), allow(dead_code))]

mod app;
mod attach;
pub mod audio;
#[cfg(test)]
mod bench_upload;
mod bindings;
mod chat_buffer;
pub mod cli;
mod client_channel;
mod client_net;
mod clipboard;
mod clipboard_paste;
#[cfg(windows)]
mod clipboard_windows;
mod config;
mod config_diagnostics;
mod device_link;
mod e2e;
mod e2e_identity;
#[cfg(feature = "embedded-client")]
pub mod embedded;
mod file_compression;
mod fuzzy;
mod local_control;
#[cfg(renderer_clients)]
mod local_stream;
pub mod mdns;
#[cfg(test)]
mod mls_e2e_test;
#[cfg(test)]
mod mls_scenario_test;
pub mod network;
pub mod packet_log;
#[cfg(test)]
mod pairing_test;
mod paths;
mod receive_store;
mod room_catalog;
mod room_history;
mod runtime;
mod self_log;
pub mod diagnostic_logs {
    pub use crate::self_log::{
        ExternalLogSink, decode_to_colored, decode_to_plain, external_log_sink,
    };
}
mod settings;
#[cfg(test)]
mod test_temp;
mod theme;
mod tui;
mod ui;
mod url_open;
mod video;
mod web_server;
mod web_wire;

#[cfg(test)]
pub(crate) mod test_alloc;

#[cfg(test)]
#[global_allocator]
static TEST_ALLOC: test_alloc::CountingAllocator = test_alloc::CountingAllocator;
