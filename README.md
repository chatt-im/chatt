# Chatt

Chatt is a minimalist, self-hosted system for low-latency voice, video, text,
and file communication.

It consists of two applications:

- `chatt`, a terminal client with an optional local web view
- `chatt-server`, a small server for hosting rooms and connecting clients

> [!WARNING]
> Chatt is experimental and unreleased. Expect incomplete features, breaking
> configuration and protocol changes, and occasional data loss. Its security
> design and implementation have not received an independent audit.

## Features

- MLS end-to-end encryption for room and direct-message events
- End-to-end encrypted file transfer
- Low-latency VoIP with packet-loss recovery and adaptive playout
- A keyboard-driven terminal interface with Vim-style navigation
- Live video and screen streaming
- A local web view for live media playback, attachments, and code view
- A single server process with generated configuration and invite-based pairing
- Low CPU, memory, and network overhead

The web view is built for media that does not fit in a terminal. Its
production bundle tiny and shipped in the repo so building Chatt does not require a
JavaScript toolchain.

## Audio

Providing the lowest-latency highest quality voice calls is where Chatt started and remains the focus.
The audio pipeline uses the current development versions of [Opus](https://opus-codec.org/) and
[RNNoise](https://gitlab.xiph.org/xiph/rnnoise):

- Opus with Deep Redundancy (DRED) from the upstream main branch
- RNNoise from the upstream main branch
- Full-band processing without WebRTC's band splitting and 32 kHz downsampling

The pipeline combines ideas from Mumble and WebRTC while keeping processing and
buffering streamlined optimized globally from end to end.

## Deliberate non-features

- Federation
- Typing indicators
- Read receipts

Chatt takes inspiration from Mumble's voice communication, Matrix's room model,
and Vim's keyboard interface.

## Install

Chatt currently supports macOS and Linux. There are no binary releases yet, so
installation is from source:

```sh
git clone https://gitlab.com/chatt-im/chatt.git
cd chatt
cargo build --release -p chatt -p server
```

This produces `target/release/chatt` and `target/release/chatt-server`. See the
[build guide](BUILDING.md) for prerequisites and first-run instructions.
