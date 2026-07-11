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
cargo run -p server -- init-config chatt-server.toml
cargo run -p server -- serve chatt-server.toml
```

The server requires an explicit config path. `init-config` writes a commented
template with a generated Ed25519 identity seed; `serve` loads that path and
prints the server public key for client pinning.

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

Pair from the generated string:

```sh
cargo run -p chatt -- pair tcj1_...
```

Connect to an already-configured server by label or address:

```sh
cargo run -p chatt -- join my-server-label
cargo run -p chatt -- join 192.168.0.1:4000
```

`join` connects directly when the specifier matches one configured server, opens
the picker filtered to the matches when it is ambiguous, and falls back to open
pairing when nothing matches but the address is a public `host:port`.

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

Override the capture with your own command, passing the verbatim argv after
`start`. Everything after `start` is the command, run directly with no shell, and
it must write NUT (preferred) or raw Annex-B to stdout; the format is detected
automatically. Prefer NUT: its framing lets each frame stream out the moment it
is encoded, while raw Annex-B holds every frame back until the next one starts —
with damage-driven capture (kmsgrab, wl-screenrec) that delays the latest frame
until the screen changes again. Add `--hevc` (before the command) when it emits
H.265:

```sh
cargo run -p chatt -- screencast start ffmpeg -f x11grab -i :0 -f nut pipe:1
cargo run -p chatt -- screencast start wl-screenrec -o HDMI-A-1 --ffmpeg-muxer nut -f -
```

The command need not be ffmpeg. `wl-screenrec` (Wayland) or any capture tool that
emits a NUT-muxed or Annex-B H.264 stream on stdout works. When the capture command fails to
start a stream or exits early, the client shows the reason (the tail of the
command's stderr). The full stderr is in the client logfile (`--logfile`).

A room member sees a play button in their web view (`[web] enabled = true`) and
the live desktop renders to a canvas. The sharer can watch their own outgoing
stream the same way: it appears in their web view as a self-view, fed locally
without a server round-trip. The web view lists every active share, including
ones that started before the browser tab connected. The default capture needs
`ffmpeg` on `PATH` and an X11 session. A custom command needs only its own
program on `PATH`.

The web view can autoplay newly received video attachments and move previews
from the side panel into separate browser tabs:

```toml
[web]
# Optional browser WebSocket allowlist. A non-empty list replaces the origins
# derived from the configured bind address.
allowed-origins = ["https://chat.example.test"]
# false (default), true (muted), or "with-audio"
autoplay = "with-audio"
# "panel" (default) keeps the side panel; "tab" opens one preview per browser tab
viewer = "panel"
```

The web server permits native WebSocket clients without an `Origin` header.
Browser clients must send an origin from the allowlist; when the list is empty,
chatt derives the bound `http://` origin and adds `localhost` for loopback or
unspecified binds. This is browser hardening rather than authentication, so
keep the web server bound to loopback unless the surrounding network is trusted.

Unmuted autoplay remains subject to the browser's media policy. A standalone
preview tab contains only the selected preview and its controls, without the
side panel's internal preview-history tabs.

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
- `/upload-rate 200K|off`: throttle upload speed in bytes per second (accepts a
  `K`/`M`/`G` suffix, or `off` for unlimited). Applies to the current session.
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

The settings screen is split into four tabs (Audio, Interface, Data, Extra);
`Alt-l` and `Alt-h` cycle between them (even while a text field is focused),
and the tab bar is clickable. Tabs with low-level rows end with a
`Show Advanced` toggle that reveals them for the session. The detail panel
notes each setting's default value.

The default key bindings are in `chatt.toml` under `[bindings.*]`. A bindings
table can set `inherit = ["name", ...]` to pull in another table's bindings
before its own keys, so its own keys always win. A table whose name is not a
layer (like the default `list`) is a reusable template: it only takes effect
where it is inherited, and a table that is neither a layer nor inherited
anywhere is reported as an unknown-key warning.

In the chat log, `j`/`k` move a line cursor, `{`/`}` jump between sender
blocks, and `v` (or a mouse drag) starts a visual-line selection. While a
selection is active the `chat-visual` layer overlays `workspace`: `y` yanks
the selection and `Esc` clears it. Without a selection `y` opens a yank
chord — `y y` copies the cursor's line, `y m` the whole message.

### Pasting from the clipboard

In a room, `Ctrl+V` or `Ctrl+Alt+V` pastes from the system clipboard, from any
focus. In the composer's normal (vim) mode `p` does the same. Text is inserted
into the composer. An image or file opens a small dialog to confirm the upload
name before it goes through the normal upload pipeline. `Enter` uploads, `Esc`
cancels.

Clipboard reads use the same command-line helpers as copying: `wl-paste` on
Wayland, `xclip`/`xsel` on X11, and `pbpaste` (plus `pngpaste` for images) on
macOS. A missing helper reports a status message rather than failing.

Because `Ctrl+V` is paste, the compose-mode microphone toggle is `Alt+M`
(`M-m`). Mute is still `m` in the room's normal and workspace layers.

## Client Configuration

Client config is loaded from `~/.config/chatt.toml` when present, or from
`--config` / `CHATT_CONFIG`. The repository `chatt.toml` is a development
sample.

Important client fields:

```toml
active-server = "local"

[[servers]]
label = "local"
username = "Alice"
token = "alice-dev-token"
server-public-key = ""
tcp-addr = "127.0.0.1:41000"
room-id = 1
```

`active-server` selects one `[[servers]]` entry. `label` is the local name for
that server. `token` is the secret that identifies the client to the server, so
there is no separate user field. `username` is the name shown in chat.
`server-public-key` is pinned from the server invite; if it is empty, the client
falls back to the compiled development server key.

UDP media shares `tcp-addr` by default. Set `udp-addr` only when the server uses
a separate UDP media address.

Under `[ui]`, `composer-padding = true` (the default) insets the composer by one
column and adds half-block borders above and below it. Set it to `false` for the
compact, unframed composer.

The `theme` key under `[ui]` selects the color theme. It may name a builtin —
`"tomorrow-night"` (the default true-color dark palette), `"base16-dark"`, or
`"base16-light"` — or a custom theme defined under `[ui.themes.<name>]`. The
base16 themes draw foreground roles from the 16 terminal ANSI colors and keep
the terminal's own background, so they follow the terminal's color scheme. Use
`base16-light` on a light terminal and `base16-dark` on a dark one. The Theme row
in the settings page (`F2`) cycles through the builtins plus every custom theme
and applies the change immediately; Save writes the selection to `chatt.toml`.

### Custom themes

Define a custom theme under `[ui.themes.<name>]`. Each starts from a builtin
`base`, may define a theme-local `palette`, and overrides individual style
slots; unset slots inherit the base:

```toml
[ui.themes.midnight]
base = "tomorrow-night"
palette.surface = "#101018"
palette.sky = "#88ccff"
palette.violet = "#c792ea"
background.bg = "surface"
accent.fg = "sky"
status-fill = { fg = "#cccccc", bg = "#202030" }
scrollbar = { fg = "#cccccc", bg = "#202030" }

[ui.themes.midnight.syntax]
keyword = "violet"

[ui]
theme = "midnight"
```

Colors are `"#rrggbb"` / `"#rgb"` for true color, or `"ansi:N"` (equivalently a
bare integer `0`–`255`) for a 256-color palette index. Slot and syntax values
may also name a color from `[ui.themes.<name>.palette]`; palette values
themselves must be direct colors, not aliases. Each slot takes an inline
`{ fg = "…", bg = "…" }` table, or the equivalent dotted keys like
`text.fg = "sky"`; a bare string or integer is shorthand for the foreground.
Defining a slot replaces the whole base slot, so an omitted `fg` or `bg` resets
that component to the terminal default (transparent background). Surfaces such
as `background` normally set `background.bg = "…"`, `{ bg = "…" }`, or `{}` for
transparent.
Palette names are theme-local and use the same identifier rules as custom theme
names, except names that look like color literals (`12`, `ansi:12`, `#fff`) are
reserved. The overridable slot names match the theme's roles — surfaces
(`background`, `panel`, `panel-alt`, `detail-panel`, `dialog-panel`, `dialog-header`),
foreground roles (`text`, `muted`, `subtle`, `accent`, `good`, `warn`, `error`),
chat lines (`local-line`, `selected-line`, `chat-visual-line`,
`chat-cursor-line`, `room-selected`), the status bar
(`status-fill`, `status-section`), composer frame (`composer-border`), and
scrollbars (`scrollbar`, where `fg` colors the thumb and `bg` colors the
gutter), inputs (`join-input-active`,
`join-input-inactive`, `join-input-boundary-active`), form rows (`row-focused`,
`selected-focused`), mode badges (`mode-server-select`, `mode-server-edit`,
`mode-compose`, `mode-log`, `mode-settings`), editor selection
(`editor-selection-charwise`, `editor-selection-linewise`), and the VU meter
(`vu-track`, `vu-idle`, and the level zones `vu-low`, `vu-good`, `vu-warn`,
`vu-peak` — each taking `{ fg, bg }` where `fg` is the glyph/readout color and
`bg` is the fill). A nested
`[ui.themes.<name>.syntax]` table overrides syntax colors (`fg`, `type`,
`function`, `binding`, `namespace`, `keyword`, `string`, `number`, `comment`).
Custom theme tables are written back verbatim on Save, so their color spellings,
comments, and key order are preserved. A custom name may not collide with a
builtin theme name.

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
`neteq-max-delay-ms` when profiling receive latency. Set `render-assist = true`
(default `false`) only on devices too slow to render a NetEQ block within the
audio callback: it pre-renders playout blocks off the callback thread at the cost
of a deeper staged output ring, so capable hardware should leave it off.

Set `echo-cancellation = true` under `[audio]` to remove far-end speaker audio
from the microphone before encoding. It uses the speaker mix as the echo
reference and runs the WebRTC AEC3 canceller as the first capture DSP step. It
defaults to `false` and is most useful when playing through speakers rather than
a headset.

The client accepts `--config` / `CHATT_CONFIG` for config path selection.

## Server Configuration

Server config is never loaded implicitly. Generate a private template once, then
start the server with that path:

```sh
cargo run -p server -- init-config chatt-server.toml
cargo run -p server -- serve chatt-server.toml
```

Important server fields:

```toml
[network]
tcp-addr = "127.0.0.1:41000"
# public-tcp-addr = "chat.example.com:443"
# public-udp-addr = "198.51.100.20:41000"
p2p-enabled = true

[security]
server-identity-seed = "<generated 64 hex chars>"
transport-mode = "native-encrypted"
chat-history-limit = 0
# max upload size the server relays, any u64 (e.g. 68719476736 for 64 GiB); defaults to 50 MiB
max-file-size-bytes = 52428800

[[rooms]]
id = 1
name = "lobby"
```

`tcp-addr`, `udp-addr`, and `udp-probe-addr` are bind addresses on the server
host. `public-tcp-addr`, `public-udp-addr`, and `public-udp-probe-addr` are the
connection details embedded in invites and returned during open pairing. Set the
public fields when clients need a DNS name, public IP, reverse proxy port, or
NAT-forwarded port. When omitted, the public fields default to the corresponding
bind address.

The generated `server-identity-seed` must stay private. The server never
rewrites the config file; user records live in `users.toml` under
`storage.data-dir` (default `<config stem>-data` beside the config). Explicit
users are added by `chatt-server invite USER` when the invite is accepted;
`name` is the admin-chosen internal identifier, while `display-name` is updated
when a user successfully joins. Room id `1` is required as the default lobby by
the current client flow.

`transport-mode` selects the trust boundary for every client. `"native-encrypted"`
(the default) has chatt secure the wire: TCP control, server-relayed UDP media,
video, and file chunks are protected by session keys. `"external-secure-link"`
defers wire security to an outer tunnel (e.g. WireGuard or an SSH tunnel): after
the signed handshake, control, media, video, and file payloads travel in the
clear, but UDP address claims still carry a proof of possession so a spoofed
datagram cannot hijack a session's media address. P2P is disabled in
`external-secure-link` mode because it would bypass the outer link. The signed
X25519 handshake and session key derivation run in both modes; the mode only
decides whether chatt encrypts payloads. Use `"external-secure-link"` only on a
network that is already confidential end to end.

`public = true` opens the server to self-service joining: a client runs
`chatt pair <host:port>` with no admin invite. The server stores no row per
open user. It hands out ids from a persisted counter (starting at `4294967296`,
leaving ids below that for explicit users) and issues a sealed bearer token
that carries the user id and the `password-epoch`. Set `password-hash` to the
SHA-256 of a shared secret (`sha256:<hex>`, generate with
`printf %s 'secret' | sha256sum`) to gate open joining behind that secret. New
anonymous dynamic user allocations are rate-limited per source IP and globally
to protect the user-registry write path. Bump `password-epoch` to invalidate
every issued dynamic token. With a configured password, affected clients can
re-pair with the current password and keep their user id; without a password,
stale-token re-pair allocates a new dynamic id. Changing `password-hash`
without bumping the epoch leaves existing tokens valid. `public` defaults to
`false`; when false, dynamic tokens are rejected and only invite-based and
explicit users can authenticate.

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

Network, P2P, encryption, and history settings live in the explicit TOML path
passed to `chatt-server serve`.

## Pairing Procedure

Pairing bootstraps or rotates a user's long-lived client token without putting a
pairing code in either config file.

1. Start the server:

```sh
cargo run -p server -- serve chatt-server.toml
```

2. On the server host, create an in-memory 24-hour invite:

```sh
cargo run -p server -- invite dana
```

The `dana` value is the server's internal user identifier. It does not need to
exist yet; successful pairing creates or updates the user's record in the
registry under the data dir.

3. On the client, pair with the printed string:

```sh
cargo run -p chatt -- pair tcj1_...
```

The client derives a server label from the address and seeds the username
from the operating system account name, then pairs over the normal encrypted
control channel. On successful pairing, the client writes a labeled `[[servers]]`
entry with the new token, and the server records the token hash plus the
display name in its user registry. The username is editable afterward in the server editor. Invites are
only held in server memory and are removed when replaced, expired, or
successfully used.

### Open Pairing

When the server sets `public = true`, no invite is needed. Pair with a bare
address:

```sh
cargo run -p chatt -- pair 127.0.0.1:41000
```

The client trusts the server's public key on first use and pins it. If the
server has a `password-hash`, the client shows a prompt (input masked), pins the key
from the first response, and retries only against that key. The server allocates
a dynamic user id, issues a bearer token, returns its public UDP endpoints, and
the client stores a labeled `[[servers]]` entry exactly like invite pairing.
After the admin bumps `password-epoch`, the next connection is prompted to
re-enter the password and keeps its user id when public pairing has a password.

### Rejoining a Configured Server

Once a server is paired, `chatt join <specifier>` reaches it without the picker.
The specifier is a server label or a `host:port` address:

```sh
cargo run -p chatt -- join my-server-label
cargo run -p chatt -- join 192.168.0.1:4000
```

An exact match on a label or address connects directly. A specifier that could
mean several configured servers opens the picker filtered to them. When nothing
matches but the specifier is a public `host:port`, `join` starts open pairing
instead.

## Security Notes

See [docs/encryption-protocol.md](docs/encryption-protocol.md) for the protocol
audit map.

Current status:

- The server decides whether TCP control and server-relayed UDP media are
  encrypted after the handshake. Encryption is enabled by default.
- UDP media uses an anti-replay window. Encrypted TCP control uses strict
  counters.
- Explicit user tokens are stored on the server as `sha256:` hashes. Invite
  codes are ephemeral server memory only and expire after 24 hours.
- Open-pairing tokens are stateless: the server stores no per-user row, only a
  counter and the password epoch. A token is a ChaCha20-Poly1305 sealed blob
  keyed from `server-identity-seed`, so only the issuing server can mint or read
  one. Revocation is global, by disabling `public` or bumping `password-epoch`.
- The server is trusted and not end-to-end encrypted.
- The current handshake uses X25519 and Ed25519. It is not yet
  quantum-resistant; the documented next step is a hybrid X25519 + ML-KEM
  handshake and post-quantum or hybrid server authentication.
