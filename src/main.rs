mod app;
mod bindings;
mod chat_buffer;
mod cli;
#[allow(dead_code)]
mod client_net;
mod clipboard;
mod config;
mod fuzzy;
mod local_control;
mod runtime;
mod settings;
mod theme;
mod tui;
mod ui;

pub(crate) use chatt::audio;
pub(crate) use chatt::mdns;

#[cfg(all(feature = "mimalloc-allocator", not(feature = "dhat-heap")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    cli::run()
}
