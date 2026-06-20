# Tomchat

Tomchat is a Rust terminal chat client and local development server with
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

The server loads `./tomchat-server.toml` by default and prints its Ed25519 public
key at startup. Keep that public key for client pinning outside local
development.

Run the client:

```sh
cargo run -p tomchat -- --config tomchat.toml
```

Run as a different configured development user:

```sh
cargo run -p tomchat -- --config tomchat.toml --user bob --token bob-dev-token
```

Upload a file into an already running client session:

```sh
cargo run -p tomchat -- upload ./path/to/file.ext
```

Inspect audio input devices:

```sh
cargo run -p tomchat -- debug-audio-inputs
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
- `/users`: show known or current room users.
- `/whoami`: show the current authenticated user.
- `/settings` or `/config`: open settings.
- `/clear`: clear the local chat view.

The default key bindings are in `tomchat.toml` under `[bindings.*]`.

## Client Configuration

Client config is loaded from `~/.config/tomchat.toml` when present, or from
`--config` / `TOMCHAT_CONFIG`. The repository `tomchat.toml` is a development
sample.

Important client fields:

```toml
[network]
user = "alice"
token = "alice-dev-token"
pairing-code = ""
server-public-key = ""
tcp-addr = "127.0.0.1:41000"
udp-addr = "127.0.0.1:41001"
udp-probe-addr = "127.0.0.1:41002"
room-id = 1
```

`server-public-key` should be set to the public key printed by the server for
non-development use. If it is empty, the client falls back to the compiled
development server key.

Useful client overrides:

- `--user`, `TOMCHAT_USER`
- `--token`, `TOMCHAT_TOKEN`
- `--pairing-code`, `TOMCHAT_PAIRING_CODE`
- `--server-public-key`, `TOMCHAT_SERVER_PUBLIC_KEY`
- `--tcp`, `TOMCHAT_TCP`
- `--udp`, `TOMCHAT_UDP`
- `--udp-probe`, `TOMCHAT_UDP_PROBE`
- `--receive-dir`, `TOMCHAT_RECEIVE_DIR`
- `--max-upload-bytes`, `TOMCHAT_MAX_UPLOAD_BYTES`
- `--max-receive-bytes`, `TOMCHAT_MAX_RECEIVE_BYTES`

## Server Configuration

Server config is loaded from `./tomchat-server.toml` when present, or from
`--config` / `TOMCHAT_SERVER_CONFIG`.

Important server fields:

```toml
[network]
tcp-addr = "127.0.0.1:41000"
udp-addr = "127.0.0.1:41001"
udp-probe-addr = "127.0.0.1:41002"

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
token-hash = "sha256:..."
pairing-code-hash = "sha256:..."
```

Replace the development `server-identity-seed`, user token hashes, and pairing
code hashes before using a server outside local testing. Room id `1` is required
as the default lobby by the current client flow.

`encryption = true` makes the server require encrypted TCP control and
server-relayed UDP media transport. Set it to `false` only for trusted local
networks or debugging; the server still signs that plaintext decision, but user
tokens, pairing codes, chat, files, and server-relayed media are sent without
confidentiality.

`chat-history-limit = 0` means the server relays chat without retaining message
bodies for future room joins. Raising the value keeps that many messages in
server memory.

Useful server overrides:

- `--config`, `TOMCHAT_SERVER_CONFIG`
- `--tcp`, `TOMCHAT_SERVER_TCP`
- `--udp`, `TOMCHAT_SERVER_UDP`
- `--udp-probe`, `TOMCHAT_SERVER_UDP_PROBE`
- `--encryption true|false`, `--no-encryption`, `TOMCHAT_SERVER_ENCRYPTION`
- `--chat-history-limit`, `TOMCHAT_SERVER_CHAT_HISTORY_LIMIT`

## Pairing Procedure

Pairing bootstraps or rotates a user's long-lived client token without storing
that token in plaintext on the server. The one-time pairing code is verified
inside the server-selected control channel after the server-authenticated
handshake. Keep `encryption = true` when pairing outside a trusted local
environment.

1. Generate a client token and a one-time pairing code:

```sh
openssl rand -hex 32
openssl rand -base64 24
```

2. Hash the one-time pairing code for the server config:

```sh
printf '%s' 'PAIRING_CODE_HERE' | sha256sum | awk '{print "sha256:" $1}'
```

3. Add or update the server user entry:

```toml
[[users]]
id = 4
name = "dana"
token-hash = ""
pairing-code-hash = "sha256:<pairing-code-sha256-hex>"
```

4. Start the server with a writable config path:

```sh
cargo run -p server -- --config tomchat-server.toml
```

5. Configure the client with the generated token, one-time pairing code, and
   server public key:

```toml
[network]
user = "dana"
token = "<generated-client-token>"
pairing-code = "<one-time-pairing-code>"
server-public-key = "<server-public-key-printed-at-startup>"
tcp-addr = "127.0.0.1:41000"
udp-addr = "127.0.0.1:41001"
udp-probe-addr = "127.0.0.1:41002"
room-id = 1
```

6. Start the client:

```sh
cargo run -p tomchat -- --config tomchat.toml
```

On successful pairing, the server rewrites its config: `token-hash` is set to a
hash of the client token and `pairing-code-hash` is cleared. Remove
`pairing-code` from the client config after the first successful login. Future
logins use the regular `Authenticate` flow with `user` and `token`.

Pairing requires a writable server config path because the one-time code must be
consumed durably.

## Security Notes

See [docs/encryption-protocol.md](docs/encryption-protocol.md) for the protocol
audit map.

Current status:

- The server decides whether TCP control and server-relayed UDP media are
  encrypted after the handshake. Encryption is enabled by default.
- UDP media uses an anti-replay window. Encrypted TCP control uses strict
  counters.
- User tokens and pairing codes are stored on the server as `sha256:` hashes.
- The server is trusted and not end-to-end encrypted.
- The current handshake uses X25519 and Ed25519. It is not yet
  quantum-resistant; the documented next step is a hybrid X25519 + ML-KEM
  handshake and post-quantum or hybrid server authentication.
