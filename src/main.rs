mod app;
#[cfg(test)]
mod bench_upload;
mod bindings;
mod chat_buffer;
mod cli;
#[allow(dead_code)]
mod client_net;
mod clipboard;
mod clipboard_paste;
mod config;
mod config_diagnostics;
mod file_compression;
mod fuzzy;
mod highlight;
mod link;
mod local_control;
mod markdown;
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

pub(crate) use chatt::audio;
pub(crate) use chatt::mdns;

#[cfg(all(feature = "mimalloc-allocator", not(feature = "dhat-heap")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    // Held for the whole process. Dropping it on exit writes `dhat-heap.json`
    // (override the path with the `DHAT_HEAP_FILE` env var) and prints a
    // summary to stderr.
    #[cfg(feature = "dhat-heap")]
    let _profiler = {
        let mut builder = dhat::Profiler::builder();
        if let Ok(path) = std::env::var("DHAT_HEAP_FILE") {
            builder = builder.file_name(path);
        }
        builder.build()
    };
    if let Err(error) = cli::run() {
        // Print via Display, not the Debug formatting Rust applies to a
        // `main` that returns `Result`, so messages read cleanly.
        eprintln!("error: {error}");
        // Drop the profiler before exiting so its report still flushes.
        #[cfg(feature = "dhat-heap")]
        drop(_profiler);
        std::process::exit(1);
    }
}
