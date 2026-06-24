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

use mimalloc::MiMalloc;

pub(crate) use chatt::audio;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    cli::run()
}
