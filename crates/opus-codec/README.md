# opus-codec

[![Build Status](https://github.com/Deniskore/opus-codec/actions/workflows/ci.yml/badge.svg)](https://github.com/Deniskore/opus-codec/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/opus-codec.svg)](https://crates.io/crates/opus-codec)
[![API reference](https://docs.rs/opus-codec/badge.svg)](https://docs.rs/opus-codec)
[![MSRV](https://img.shields.io/badge/MSRV-1.87.0-blue)](https://doc.rust-lang.org/cargo/reference/manifest.html#the-rust-version-field)
[![License](https://img.shields.io/crates/l/opus-codec.svg)](https://crates.io/crates/opus-codec)

Safe Rust wrappers around libopus for encoding/decoding Opus audio, with tests that validate core functionality against ffmpeg.

## Features

- `presume-avx2`: Build the bundled libopus with `OPUS_X86_PRESUME_AVX2` on x86/x86_64 targets, assuming AVX/AVX2/FMA support. Ignored when linking against a system libopus.
- `dred`: Enable libopus DRED support (downloads the model when building the bundled library). The bundled DRED build currently assumes a Unix-like host with `sh`, `wget`, and `tar`, it is not supported on Windows.
- `system-lib`: Link against a system-provided libopus instead of the bundled sources.

## MSRV

Minimum Supported Rust Version (MSRV): **1.87.0**.

## License

This crate is licensed under either of

- [MIT license](https://opensource.org/licenses/MIT)
- [Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0)

at your option.

## Bundled libopus

The upstream libopus sources are vendored via `git subtree` at tag **v1.6.1** (split commit `22244de5a79bd1d6d623c32e72bf1954b56235be`).
You can verify the copy is pristine by diffing `opus/` against that upstream commit.

## Windows MSVC

Bundled builds follow Cargo's selected C runtime automatically. By default, `opus-codec` builds the vendored `libopus` with the dynamic MSVC runtime. If you build with `RUSTFLAGS="-C target-feature=+crt-static"`, the bundled `libopus` build switches to the static MSVC runtime as well.

If you enable the `system-lib` feature, `opus-codec` links against an already installed `libopus` instead of the vendored copy. In that case, the installed `libopus` must use the same CRT mode as the final binary.
