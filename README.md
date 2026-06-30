# Chatt

Chatt is a Rust terminal chat client and local development server with
server-selected TCP/UDP transport encryption, file relay, and P2P media
candidate exchange.

The server is trusted in the current design. It decrypts traffic to route chat,
files, voice, and P2P setup messages, but the default server configuration does
not retain chat history.

## Native Dependencies

Chatt needs Rust 1.87 or newer. Install that with `rustup` or another current
Rust toolchain source before building. The repository builds the bundled Opus
codec by default, including DRED support, so Linux builds also need a C
toolchain, CMake, ALSA development headers, and the tools used by the Opus
build script.

If you use `rustup`, select a current stable toolchain:

```sh
rustup update stable
rustup default stable
```

Install build dependencies on Debian/Ubuntu:

```sh
sudo apt update
sudo apt install -y build-essential pkg-config libasound2-dev cmake wget tar ca-certificates git
```

Install build dependencies on Arch Linux:

```sh
sudo pacman -Syu --needed base-devel rustup pkgconf alsa-lib cmake wget tar ca-certificates git
```

Dependency notes:

- `libasound2-dev` on Debian/Ubuntu, or `alsa-lib` on Arch, provides the
  `alsa.pc` metadata required by `alsa-sys`, which is pulled in by the `cpal`
  audio backend used by the client.
- Realtime audio callback scheduling is enabled by the default
  `audio-realtime` Cargo feature. On Linux/BSD, CPAL can only acquire RT
  priority when the user has `rtprio` limits or equivalent privileges; without
  that, chatt logs the refusal and keeps running at normal scheduling priority.
  Desktop systems with rtkit can use `--features audio-realtime-dbus` if D-Bus
  development files are installed.
- PipeWire support is available as a non-default Cargo feature. Enable it with
  `cargo run -p chatt --features pipewire -- --config chatt.toml`; this path
  needs PipeWire development files discoverable by `pkg-config`.
- `cmake` and the C toolchain build the bundled `crates/opus-codec` copy of
  libopus. The default `chatt` build enables the Opus `dred` feature. The DRED
  model weights it needs are vendored in `crates/opus-codec/opus/dnn`, so the
  build pulls nothing from the network.
- The optional RNNoise V2 denoiser uses an external `weights_blob.bin` at build
  time. The blob is intentionally not checked into this repository. Generating
  it from upstream RNNoise requires the usual autotools stack in addition to the
  base C toolchain.
- `ffmpeg` is not needed for `cargo check`, but is needed for full audio tests
  and benchmarks. Install it with `sudo apt install -y ffmpeg` on
  Debian/Ubuntu or `sudo pacman -S --needed ffmpeg` on Arch.
- The opt-in P2P network namespace test needs the `ip` command from
  `iproute2`, plus root or `CAP_NET_ADMIN`, when `CHATT_NETNS_TESTS=1` is set.
  Install it with `sudo apt install -y iproute2` on Debian/Ubuntu or
  `sudo pacman -S --needed iproute2` on Arch.
- Regenerating Opus bindings, rather than using the checked-in
  `crates/opus-codec/src/bindings.rs`, needs libclang. Install
  `clang libclang-dev` on Debian/Ubuntu or `clang` on Arch for that workflow.
- The non-default `opus-codec/system-lib` feature links to a system libopus
  instead of the bundled copy. That path needs `libopus-dev` on Debian/Ubuntu
  or `opus` on Arch, and the system libopus must include any features you rely
  on, such as DRED.

## Quick Start

Run the development server:

```sh
cargo run -p server
```

The server loads `./chatt-server.toml` by default and prints its Ed25519 public
key at startup. Keep that public key for client pinning outside local
development.

Run the client:

```sh
cargo run -p chatt -- --config chatt.toml
```

### Optimized build (x86-64-v3)

`cargo run --release` produces a portable binary: baseline x86-64 codegen with
Opus's own runtime AVX2 dispatch, so it runs on any x86-64 CPU. For lower CPU
use on modern hardware, build with the `v3` alias:

```sh
cargo v3                 # builds target/release-v3/chatt
./target/release-v3/chatt --config chatt.toml
```

It compiles the Rust audio code for `x86-64-v3` (AVX2 + FMA) and enables the
`avx2` feature, which presumes AVX2 in bundled C audio backends that support it
and drops their per-call CPU dispatch. The result needs an Intel Haswell / AMD
Excavator or newer CPU (2013+) and measures roughly 20% fewer cycles on the
live call audio pipeline. The portable `release` build is unaffected.

### RNNoise denoiser

The capture path denoises with the vendored RNNoise model in
`crates/nnnoiseless`. The trained weights are checked in at
`crates/nnnoiseless/rnnoise_weights.bin` and embedded at build time, so an
ordinary `cargo build` just works with no extra features or environment
variables.

The `avx2` feature (and the `cargo v3` alias) compiles the bundled RNNoise C
objects directly with AVX2/FMA and skips runtime CPU dispatch; the default build
stays portable and selects SSE4.1/AVX2 kernels at runtime.

The checked-in blob is the upstream "little" model. To regenerate it (for
maintainers updating the model):

```sh
git clone https://github.com/xiph/rnnoise /tmp/rnnoise
cd /tmp/rnnoise
./autogen.sh
./configure
./download_model.sh
cp src/rnnoise_data_little.c src/rnnoise_data.c   # omit for the larger model
make dump_weights_blob
./dump_weights_blob
cp weights_blob.bin /code/chatt/crates/nnnoiseless/rnnoise_weights.bin
```

Capture client diagnostics while running:

```sh
cargo run -p chatt -- --config chatt.toml --logfile /tmp/chatt.log
```

Invite a configured user from a running server:

```sh
cargo run -p server -- invite alice
```

Join from the generated string:

```sh
cargo run -p chatt -- join tcj1_...
```

Upload a file into an already running client session:

```sh
cargo run -p chatt -- upload ./path/to/file.ext
```

Share your screen to other room members, who watch it live in their browser web
view. `start` captures the X11 desktop with a built-in ffmpeg command:

```sh
cargo run -p chatt -- screencast start
cargo run -p chatt -- screencast stop
```

Pass `--hevc` to capture H.265/HEVC instead of H.264. HEVC compresses better but
browser HEVC decode is platform-gated (absent in many Firefox builds), so H.264
is the default and the reliable cross-browser path:

```sh
cargo run -p chatt -- screencast start --hevc
```

Override the capture command with `--ffmpeg`, passing the verbatim argv that
writes Annex-B to stdout (`pipe:1`). Everything after `--ffmpeg` is the command,
run directly with no shell. Add `--hevc` (before `--ffmpeg`) when the custom
command emits H.265:

```sh
cargo run -p chatt -- screencast start --ffmpeg ffmpeg -f x11grab -i :0 -f h264 pipe:1
```

A room member sees a play button in their web view (`[web] enabled = true`) and
the live desktop renders to a canvas. The sharer can watch their own outgoing
stream the same way: it appears in their web view as a self-view, fed locally
without a server round-trip. The web view lists every active share, including
ones that started before the browser tab connected. Screen share needs `ffmpeg`
on `PATH` and, for the default capture, an X11 session.

Inspect audio input devices:

```sh
cargo run -p chatt -- debug-audio-inputs
```

Play an audio file through the real live playback path while applying the same
loss and delivery-delay profiles used by the latency simulations:

```sh
cargo run -p chatt -- --config chatt.toml test-audio-playback assets/sample-001.opus --loss congested_wifi
```

Use `--loss none` to isolate output-device behavior without synthetic network
loss, or profiles such as `random_60` and `mobile_handoff` to stress DRED/PLC
recovery and jitter handling.

For live receiver testing, use the dev soundboard client. It joins the room
without opening a microphone and sends prerecorded clips over the normal voice
path when triggered:

```sh
devsm start server
devsm client-alice
devsm client-bob
devsm client-soundboard
```

Run each interactive client in its own terminal. In the soundboard client,
press `1` or type `/sound 1` to send `assets/sample-001.opus` with the
configured `[soundboard]` loss profile.

## Common Commands

Build and validate the workspace:

```sh
cargo check --workspace
cargo fmt --all
cargo test --workspace
```

Useful in-app slash commands:

- `/help`: show the in-app command list. Type a prefix and press Tab to
  complete a slash command; press Tab again to cycle other matches.
- `/upload path/to/file.ext`: relay a file to users in the room who accept files.
- `/report-bug what went wrong`: send recent logs and diagnostics to the server.
- `screencast start` / `screencast stop` (CLI subcommands): share your screen to room members' web views.
- `/mute` and `/unmute`: control microphone send.
- `/deafen` and `/undeafen`: stop or resume receive/playback and microphone send.
- `/audio`: show receive queue, adaptive catch-up, DRED/PLC, trim, and underrun diagnostics.
- `/users`: show known or current room users.
- `/whoami`: show the current authenticated user.
- `/soundboard` and `/sound N|name`: list or trigger configured soundboard clips.
- `/settings` or `/config`: open settings.
- `/clear`: clear the local chat view.

The default key bindings are in `chatt.toml` under `[bindings.*]`.

## Client Configuration

Client config is loaded from `~/.config/chatt.toml` when present, or from
`--config` / `CHATT_CONFIG`. The repository `chatt.toml` is a development
sample.

Important client fields:

```toml
active-server = "local"

[[servers]]
alias = "local"
display-name = "Alice"
token = "alice-dev-token"
server-public-key = ""
tcp-addr = "127.0.0.1:41000"
room-id = 1
```

`active-server` selects one `[[servers]]` entry. `alias` is the local name for
that server. `token` is the secret that identifies the client to the server, so
there is no separate user field. `display-name` is the name shown in chat.
`server-public-key` is pinned from the server invite; if it is empty, the client
falls back to the compiled development server key.

Older configs may still contain a `user` key. It is now ignored and removed the
next time the client saves its config.

UDP media shares `tcp-addr` by default. Set `udp-addr` only when the server uses
a separate UDP media address.

The `theme` key under `[ui]` selects the color theme. Valid values are
`"tomorrow-night"` (the default true-color dark palette), `"base16-dark"`, and
`"base16-light"`. The base16 themes draw foreground roles from the 16 terminal
ANSI colors and keep the terminal's own background, so they follow the
terminal's color scheme. Use `base16-light` on a light terminal and
`base16-dark` on a dark one. The Theme row in the settings page (`F2`) cycles
through the themes and applies the change immediately; Save writes it to
`chatt.toml`.

Voice receive uses the NetEQ delay manager as the jitter buffer controller. It
starts at `neteq-start-delay-ms`, clamps the delay target between
`neteq-min-delay-ms` and `neteq-max-delay-ms`, and raises the target when packet
arrival jitter or reordering requires a wider horizon. Use `/audio` or
`--logfile` to inspect NetEQ target/playout delay, packet-buffer wait/span, the
current decision, DRED horizon misses, PLC fallback, hard trims, and output-ring
underruns.

Latency controls live under `[audio.latency]` in `chatt.toml`. The defaults
enable capture silence gating and a 60 ms NetEQ start delay. Set
`capture-silence-gate = false` to isolate sender-side silence suppression during
testing, and tune `neteq-min-delay-ms`, `neteq-base-minimum-delay-ms`, or
`neteq-max-delay-ms` when profiling receive latency.

Set `echo-cancellation = true` under `[audio]` to remove far-end speaker audio
from the microphone before encoding. It uses the speaker mix as the echo
reference and runs the WebRTC AEC3 canceller as the first capture DSP step. It
defaults to `false` and is most useful when playing through speakers rather than
a headset.

The client accepts `--config` / `CHATT_CONFIG` for config path selection.

## Server Configuration

Server config is loaded from `./chatt-server.toml` when present, or from
`--config` / `CHATT_SERVER_CONFIG`.

Important server fields:

```toml
[network]
tcp-addr = "127.0.0.1:41000"
# public-tcp-addr = "chat.example.com:443"
# public-udp-addr = "198.51.100.20:41000"
p2p-enabled = true

[security]
server-identity-seed = "546f6d636861742064657620736572766572206b657920763100000000000001"
encryption = true
chat-history-limit = 0
max-file-size-bytes = 52428800

[[rooms]]
id = 1
name = "lobby"

[[users]]
id = 1
name = "alice"
display-name = "Alice"
token-hash = "sha256:..."
```

`tcp-addr`, `udp-addr`, and `udp-probe-addr` are bind addresses on the server
host. `public-tcp-addr`, `public-udp-addr`, and `public-udp-probe-addr` are the
connection details embedded in invites. Set the public fields when clients need
a DNS name, public IP, reverse proxy port, or NAT-forwarded port. When omitted,
the public fields default to the corresponding bind address.

Replace the development `server-identity-seed` and user token hashes before
using a server outside local testing. `name` is the admin-chosen internal
identifier. It is used by `invite` and distinguishes users server-side, and it
is never sent by or shown to clients, which authenticate by token alone.
`display-name` is updated when a user successfully joins. Room id `1` is required
as the default lobby by the current client flow.

`encryption = true` makes the server require encrypted TCP control and
server-relayed UDP media transport. Set it to `false` only for trusted local
networks or debugging; the server still signs that plaintext decision, but user
tokens, pairing codes, chat, files, and server-relayed media are sent without
confidentiality.

`chat-history-limit = 0` means the server relays chat without retaining message
bodies for future room joins. Raising the value keeps that many messages in
server memory.

`p2p-enabled = false` disables P2P candidate exchange and NAT probing while
leaving server-relayed UDP media enabled.

UDP media binds to `tcp-addr` by default because TCP and UDP can listen on the
same numeric port. Set `udp-addr` only if deployment needs separate local
control and media sockets. `udp-probe-addr` is optional and only enables a
second UDP endpoint for P2P NAT classification; ordinary voice relay does not
need it.

The server accepts `--config` / `CHATT_SERVER_CONFIG` for config path
selection. Network, P2P, encryption, and history settings live in TOML.

## Pairing Procedure

Pairing bootstraps or rotates a user's long-lived client token without putting a
pairing code in either config file.

1. Start the server with a writable config path:

```sh
cargo run -p server -- --config chatt-server.toml
```

2. On the server host, create an in-memory 24-hour invite:

```sh
cargo run -p server -- invite dana
```

The `dana` value is the server's internal user identifier. It does not need to
exist in TOML yet; successful pairing creates or updates the `[[users]]` entry.

3. On the client, join with the printed string:

```sh
cargo run -p chatt -- join tcj1_...
```

The client derives a server alias from the address and seeds the display name
from the operating system account name, then pairs over the normal encrypted
control channel. On successful pairing, the client writes a named `[[servers]]`
entry with the new token, and the server writes `token-hash` plus the
`display-name`. The display name is editable afterward in settings. Invites are
only held in server memory and are removed when replaced, expired, or
successfully used.

## Security Notes

See [docs/encryption-protocol.md](docs/encryption-protocol.md) for the protocol
audit map.

Current status:

- The server decides whether TCP control and server-relayed UDP media are
  encrypted after the handshake. Encryption is enabled by default.
- UDP media uses an anti-replay window. Encrypted TCP control uses strict
  counters.
- User tokens are stored on the server as `sha256:` hashes. Invite codes are
  ephemeral server memory only and expire after 24 hours.
- The server is trusted and not end-to-end encrypted.
- The current handshake uses X25519 and Ed25519. It is not yet
  quantum-resistant; the documented next step is a hybrid X25519 + ML-KEM
  handshake and post-quantum or hybrid server authentication.
