mod app;
mod attach;
pub mod audio;
#[cfg(test)]
mod bench_upload;
#[cfg(test)]
mod e2e_test;
mod bindings;
mod chat_buffer;
pub mod cli;
mod client_channel;
mod client_net;
mod clipboard;
mod clipboard_paste;
mod config;
mod config_diagnostics;
mod e2e;
mod e2e_identity;
mod e2e_store;
mod device_link;
mod file_compression;
mod fuzzy;
mod highlight;
mod link;
mod local_control;
mod markdown;
pub mod mdns;
pub mod network;
pub mod packet_log;
mod paths;
mod receive_store;
mod room_catalog;
mod room_history;
mod runtime;
mod self_log;
mod settings;
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
