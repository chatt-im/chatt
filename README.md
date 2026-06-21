# Chatt

Chatt is a Rust terminal chat client and local development server with
server-selected TCP/UDP transport encryption, file relay, and P2P media
candidate exchange.

The server is trusted in the current design. It decrypts traffic to route chat,
files, voice, and P2P setup messages, but the default server configuration does
not retain chat history.

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

Inspect audio input devices:

```sh
cargo run -p chatt -- debug-audio-inputs
```

## Common Commands

Build and validate the workspace:

```sh
cargo check --workspace
cargo fmt --all
cargo test --workspace
```

Useful in-app slash commands:

- `/upload path/to/file.ext`: relay a file to users in the room who accept files.
- `/mute` and `/unmute`: control microphone send.
- `/deafen` and `/undeafen`: stop or resume receive/playback and microphone send.
- `/audio`: show receive queue, adaptive catch-up, DRED/PLC, trim, and underrun diagnostics.
- `/users`: show known or current room users.
- `/whoami`: show the current authenticated user.
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
user = "alice"
display-name = "Alice"
token = "alice-dev-token"
server-public-key = ""
tcp-addr = "127.0.0.1:41000"
room-id = 1
```

`active-server` selects one `[[servers]]` entry. `alias` is the local name for
that server. `user` is the server's internal user identifier, and
`display-name` is the name shown in chat. `server-public-key` is pinned from the
server invite; if it is empty, the client falls back to the compiled
development server key.

UDP media shares `tcp-addr` by default. Set `udp-addr` only when the server uses
a separate UDP media address.

Voice receive keeps a low-latency 60 ms playback target under good conditions.
When loss or DRED recovery is observed, playback temporarily permits a larger
queue so Opus DRED can recover missing frames; adaptive resampling then catches
up instead of letting latency grow for the rest of the call. Use `/audio` or
`--logfile` to inspect queue growth, DRED recovery, PLC fallback, hard trims,
and underruns.

Latency controls live under `[audio.latency]` in `chatt.toml`. The defaults
enable adaptive catch-up, playback silence skipping, and capture silence gating;
set `adaptive-catch-up`, `playback-silence-skip`, or `capture-silence-gate` to
`false` to isolate those behaviors during testing or profiling.

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
using a server outside local testing. `name` is the internal user identifier
used by invites. `display-name` is updated when a user successfully joins. Room
id `1` is required as the default lobby by the current client flow.

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

The join TUI asks for a server alias and display username, shows the server
address and key, then pairs over the normal encrypted control channel. On
successful pairing, the client writes a named `[[servers]]` entry with the new
token, and the server writes `token-hash` plus the chosen `display-name`.
Invites are only held in server memory and are removed when replaced, expired,
or successfully used.

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
