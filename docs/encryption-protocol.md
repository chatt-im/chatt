# Chatt Encryption, Pairing, and Server Configuration

This document describes the current security design and the implementation map
for human audit. Function names are listed without line numbers so the map stays
useful as the code moves.

## Security Status

- By default, application control, chat, file relay, and server-relayed media
  traffic is encrypted after the TCP handshake completes. The server can select
  authenticated plaintext transport with `security.encryption = false`.
- The handshake itself carries only public key agreement material, nonces, and a
  server signature. No user token, pairing code, chat body, file content, or
  media payload is sent before the server-selected transport mode is active.
- In plaintext mode, the server's no-encryption decision is signed, but user
  tokens, pairing codes, chat, files, and server-relayed media do not have
  transport confidentiality.
- The server is trusted in this version. It decrypts messages and media in order
  to route them, so this is not end-to-end encryption.
- The default server config sets `security.chat-history-limit = 0`, so the
  server does not retain chat bodies for future room joins. It still keeps
  transient session, room membership, active upload, and P2P routing state in
  memory while those operations are active.
- Server logs avoid chat bodies, file contents, and file names. They still
  contain operational metadata such as user names, session ids, addresses,
  payload sizes, and transfer ids.
- Current key agreement and signatures are classical X25519 and Ed25519. This is
  not quantum-resistant. A post-quantum upgrade should use a hybrid exchange
  that combines X25519 with ML-KEM from NIST FIPS 203, and should evaluate
  post-quantum server authentication such as ML-DSA from NIST FIPS 204.

## Server Configuration

The server now loads TOML configuration from `./chatt-server.toml` by default,
or from `--config` / `CHATT_SERVER_CONFIG`.

Important fields:

- `network.tcp-addr`: TCP control listener. UDP media shares this address by
  default.
- `network.udp-addr`: optional UDP media listener override.
- `network.udp-probe-addr`: optional second UDP endpoint for P2P NAT
  classification.
- `network.public-tcp-addr`: TCP endpoint embedded in invites. This may be a
  DNS name or NAT/reverse-proxy address and port.
- `network.public-udp-addr`: UDP media endpoint embedded in invites.
- `network.public-udp-probe-addr`: optional public P2P NAT probe endpoint
  embedded in invites.
- `security.server-identity-seed`: 32-byte Ed25519 seed encoded as hex. Replace
  the development value before non-local use.
- `security.encryption`: whether TCP control and server-relayed UDP media use
  negotiated transport encryption. Defaults to `true`.
- `security.chat-history-limit`: number of messages to keep in memory for new
  joins. Use `0` for no retained message bodies.
- `security.max-file-size-bytes`: server-side file relay limit, capped by the
  RPC protocol maximum.
- `[[rooms]]`: configured rooms. The current client flow expects room id `1` as
  the default lobby.
- `[[users]]`: configured users. Token hashes are stored as
  `sha256:<64 hex chars>`. Invite secrets are never stored in TOML.

The server prints its public key at startup. Clients should copy that value into
the active `[[servers]].server-public-key` in `chatt.toml` for
non-development deployments. If the client field is empty, it falls back to the
compiled development key.

## Pairing Procedure

Pairing is used to bootstrap or rotate a user's long random token without
storing the token or invite secret in plaintext config.

1. While the server is running, the admin runs `chatt-server invite USER`.
   The command connects to the server's Unix admin socket and asks the running
   process to create an in-memory invite for that internal user identifier. The
   user does not need to exist in TOML yet. A new invite for the same user
   replaces the previous one. Invites expire after 24 hours.
2. The server returns a `tcj1_...` join string containing the server addresses,
   server public key, default room, and one-time invite secret. It does not
   carry the internal user identifier. The addresses come from
   `network.public-*`, not from the local bind addresses, so deployments behind
   NAT or DNS use their externally reachable connection details.
3. The user runs `chatt join JOIN_STRING`. The client derives a local server
   alias from the address, seeds the display name from the operating system
   account name, and generates a long client token.
4. The client performs the normal server-authenticated handshake. Inside the
   server-selected control channel, it sends `ClientControl::Pair` containing
   the display name, invite secret, and new token. No user identifier is sent.
5. The server matches the invite by its secret, which selects the user the
   invite was issued for, removes the invite, rejects the token if its hash
   already belongs to another user, hashes the new token, creates or updates
   the `[[users]]` entry with `token-hash` and `display-name`, and authenticates
   the current session.
6. After successful authentication, the client writes a named `[[servers]]`
   entry with the generated token. The invite secret is never written to client
   config. Future logins use `ClientControl::Authenticate` with the token.

## Transport Protocol

TCP control handshake:

1. `ClientHello` contains protocol version, a 32-byte random nonce, and an
   ephemeral X25519 public key.
2. `ServerHello` contains protocol version, a transport encryption decision, a
   32-byte random nonce, optional ephemeral X25519 public key material, and an
   Ed25519 signature over the handshake transcript.
3. The client verifies the server signature against the pinned Ed25519 public
   key before accepting the transport mode.
4. In encrypted mode, both sides compute the X25519 shared secret and derive
   four traffic keys with HKDF-SHA256: client control, server control, client
   media, and server media. The transcript hash is the HKDF salt.
5. In encrypted mode, TCP control frames are length-prefixed and then encrypted
   with ChaCha20-Poly1305. The AEAD associated data includes the channel id, key
   id, and counter. TCP control requires strict monotonically increasing
   counters. In plaintext mode, length-prefixed TCP frames carry encoded control
   payloads directly.

UDP media:

1. UDP packets carry version, media kind, key id, and counter in a clear header.
2. In encrypted mode, the media payload is encrypted with ChaCha20-Poly1305
   using the negotiated media key. The key id and counter are authenticated as
   associated data.
3. In plaintext mode, key id `0` identifies an unencrypted payload encoded in
   the same media payload format.
4. UDP receive paths use a sliding anti-replay window before accepting a packet.

P2P media:

1. Peers publish candidates through the server-selected control channel.
2. The trusted server creates random directional P2P media keys and sends them
   to both peers through control messages.
3. Direct peer media packets use the same media AEAD and anti-replay machinery.

## Implementation Map

Shared protocol:

- `crates/rpc/src/control.rs`: `ClientHello`, `ServerHello`,
  `ClientControl::Authenticate`, `ClientControl::Pair`, `ServerControl`, and
  validation in `validate_client_control`; `InviteTicket` join-string
  encoding/decoding.
- `crates/rpc/src/crypto.rs`: handshake generation and verification in
  `generate_client_hello`, `respond_to_client_hello`,
  `respond_to_client_hello_plaintext`, and
  `complete_client_transport_handshake`; key derivation in
  `derive_session_secrets`; control transport selection in `ControlTransport`;
  AEAD framing in `TransportCipher`, `seal_with_key`, and `open_with_key`;
  replay tracking in `AntiReplay`; configured-key helpers in
  `server_key_pair_from_seed_hex`, `ed25519_public_key_from_hex`, and
  `encode_hex`.
- `crates/rpc/src/media.rs`: UDP encryption/plaintext and anti-replay entry
  points in `seal_media`, `open_media`, `seal_plaintext_media`,
  `open_plaintext_media`, and `parse_header`.

Server:

- `crates/server/src/config.rs`: TOML loading in `Config::load`; Ed25519
  identity loading in `Config::server_key_pair`; pairing persistence in
  `Config::mark_user_paired`; token helpers in `hash_secret` and
  `verify_secret_hash`.
- `crates/server/src/main.rs`: server startup in `main` and `Server::bind`;
  invite creation in `Server::create_invite`;
  TCP handshake in `Server::process_frame`; auth dispatch in
  `Server::handle_control`; token auth in `Server::authenticate_client`;
  first-time pairing in `Server::pair_client`; session creation in
  `Server::establish_session`; control send in
  `Server::send_control_to_token`; UDP receive/send in
  `Server::handle_udp_packet` and `Server::send_udp_payload`; message retention
  behavior in `Server::send_chat` and `Server::start_file_upload`.

Client:

- `src/config.rs`: client TOML fields in `ServerEntry` and runtime persistence
  in `write_runtime_config`.
- `src/main.rs`: join-string setup UI in `run_join_setup` and named-server
  persistence after successful pairing.
- `src/client_net.rs`: server connection and handshake in
  `connect_and_handshake`; public key pin selection in
  `pinned_server_public_key`; first auth or pairing message in
  `run_worker_inner`; control send in `WorkerState::queue_control`;
  server message handling in `WorkerState::handle_control`; UDP media send and
  receive in `WorkerState::send_media`, `WorkerState::handle_udp_packet`, and
  `WorkerState::handle_p2p_media`.

## Open Security Work

- Add hybrid post-quantum key agreement. The target shape is X25519 plus
  ML-KEM, with both shared secrets mixed into HKDF and the full KEM transcript
  authenticated by the server signature.
- Add post-quantum or hybrid server authentication when a production-quality
  Rust implementation is chosen.
- Implement session rekeying. Constants exist for rekey timing, but the current
  transport does not perform an in-band rekey.
- If the server must become untrusted, add end-to-end room/message encryption,
  client identity keys, device verification, and key transparency. The current
  trusted-server design intentionally does not provide those properties.

References:

- NIST FIPS 203, Module-Lattice-Based Key-Encapsulation Mechanism Standard:
  https://csrc.nist.gov/pubs/fips/203/final
- NIST FIPS 204, Module-Lattice-Based Digital Signature Standard:
  https://csrc.nist.gov/pubs/fips/204/final
- NIST SP 800-56C Rev. 2, Recommendation for Key-Derivation Methods in
  Key-Establishment Schemes:
  https://csrc.nist.gov/pubs/sp/800/56/c/r2/final
