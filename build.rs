//! Emits `cfg(renderer_clients)`: the core serves renderer clients.
//!
//! A renderer client is a frontend that consumes the core's `local-rpc`
//! projection: a native renderer attached over the Unix control socket, or the
//! in-process frontend of `embedded-client`. The socket transport is Unix-only,
//! but the embedded frontend is not, so the client model in `runtime.rs` is
//! compiled wherever either exists. Spelled once here rather than as
//! `any(unix, feature = "embedded-client")` on every item it covers.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(renderer_clients)");
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();
    let embedded = std::env::var_os("CARGO_FEATURE_EMBEDDED_CLIENT").is_some();
    if unix || embedded {
        println!("cargo:rustc-cfg=renderer_clients");
    }
}
