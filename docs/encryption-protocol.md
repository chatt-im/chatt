# Chatt Encryption, Pairing, and Server Configuration

This document describes the current security design and the implementation map
for human audit. Function names are listed without line numbers so the map stays
useful as the code moves.

## Security Status

- By default (`transport-mode = "native-encrypted"`), application control, chat,
  file relay, video, and server-relayed media traffic is encrypted after the TCP
  handshake completes. The server can instead select
  `transport-mode = "external-secure-link"`, which defers wire security to an
  outer tunnel and sends those payloads in the clear after the handshake.
- The signed X25519 handshake runs and derives full session material in **both**
  modes; only the selected mode differs, and the server's signature covers it. No
  user token, pairing code, chat body, file content, or media payload is sent
  before the transport is active.
- In `external-secure-link` mode, control, media, video, and file payloads have
  no chatt-provided confidentiality (the outer link supplies it), but UDP address
  claims still carry a proof of possession under a session-derived bind key, so a
  spoofed datagram cannot hijack a session's media address. P2P is disabled in
  this mode because it would bypass the outer link.
- The server is trusted to route. Public/private room chat, voice, video, and
  files are decrypted by the server, so they are not end-to-end encrypted.
- Direct messages are end-to-end encrypted (text, edits, deletions, and file
  transfers).
  Each user holds a long-term X25519 identity seed; the per-DM root is a
  static-static X25519 agreement HKDF-bound to both user ids and public keys,
  with mirrored directional keys. Every message derives a one-shot
  ChaCha20-Poly1305 key from a fresh 32-byte salt (no counters, so restarts and
  concurrent sessions can never reuse a nonce), with AAD binding the envelope
  version, content class, room id, and sender. Chat plaintexts are zero-padded
  to 160-byte multiples; sealed file streams are Padmé-padded and their chunks
  are AEAD frames under a random per-transfer content key carried inside the
  sealed metadata envelope. Peer keys are server-distributed and become usable
  immediately under trust on first use (TOFU). Durable pins in `client.toml`
  bind the DM room id, user id, and X25519 public key, retain the latest display
  name, and carry an `Accepted` or `Verified` trust level. `Accepted` means the
  key has only TOFU continuity; `Verified` means that exact key was compared
  through an independent channel using its 24
  identity words or a matching Chatt verification card. A server presentation
  matching a stored pin is only a session-level pin match and never upgrades
  `Accepted` to `Verified`. The local identity seed is likewise bound to its
  authenticated user id. First-use and replacement keys become active in memory
  immediately; an atomic `0600` config replacement preserves continuity across
  restart. A failed write does not hide plaintext or disable the current session.
  `/identity` and `/identity user` only open exact-key independent verification;
  there is no blind-accept action. The durable room-id
  binding remains encryption-required while reconnect room state is rebuilt;
  reclassifying a pinned DM as public/private fails the connection instead of
  enabling plaintext fallback. A changed key remains usable but produces a
  persistent red continuity warning; a display-name change alone does not change
  trust. Former keys remain receive-only for retained history unless the server
  later presents one as current again. DM chat and file sender labels use current
  room-directory display state (or the local configured name for an own echo),
  never the outer per-message display-name field; names are not trust material.
  Edit and deletion targets remain visible for server-side ownership checks,
  but are duplicated inside the authenticated plaintext; clients reject a
  mutation if the server-visible target differs from the sealed target.
  Each opened remote message records the exact peer key that authenticated it.
  The TUI and browser show `(Unverified)` unless that key is independently
  verified; verification relabels retained messages from the same key without
  relabeling messages from other keys. Before the server's key response, records
  use a bounded in-memory ordering queue (2 MiB/1024 controls per room, 16
  MiB/8192 globally). It is not persisted, and overflow fails the connection
  instead of silently discarding data. File payloads that race key discovery are
  declined rather than buffered. Forbidden plaintext/envelope forms still fail closed. Deliberately no
  ratchet: keys are static so server-fetched DM history stays decryptable, and
  seed compromise exposes all of that user's DM traffic. The server still sees
  DM routing metadata (participants, timing, size classes, edit/delete
  targets), can replay an exact sender envelope within the same
  room/sender/content class, and DM voice/video stay transport-encrypted only.
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

## DM Identity Presentation and Verification

The canonical identity formatter in `src/e2e_identity.rs` validates a 32-byte
X25519 public key and exposes two losslessly related public presentations:

- Lowercase hexadecimal is the complete raw key as 64 unseparated digits.
- `Chatt public-key identity words` append the first eight bits of
  `SHA-256(public_key)` to the raw 256 key bits, read the 264 bits
  most-significant-bit first as 24 11-bit indices, and map those indices through
  the 2048 unique lowercase words in `assets/english.txt`.

The hash byte is only a transcription checksum. The words are not a wallet
seed, recovery phrase, secret, or separate SHA-256 identity fingerprint. The
first 256 encoded bits are exactly the public key Chatt will pin.

A public verification card is one canonical line:

```text
chatt-e2e:v1:<server-ed25519-key-base32>:<user-id-decimal>:<x25519-key-base32>:<checksum-base32>
```

The checksum is the first eight bytes of SHA-256 over the preceding canonical
ASCII fields. The binary key and checksum fields use Chatt's lowercase,
unpadded Crockford base32 encoding; this shortens the card without replacing or
truncating either public key. The checksum detects damaged pastes; it is not a
signature or proof. The full server Ed25519 key prevents a card from being
accidentally applied to another configured server, and the user id prevents
applying it to another account. Import marks a key `Verified` only when server,
expected peer user id, and exact presented/accepted X25519 key all match.
Wrong-server, wrong-user, self-card, malformed, checksum, and stale-key cases
are distinct errors. A key mismatch has no verification action in the dialog.

The Chat status bar and browser warning are projections of the authoritative
room trust state: `FetchingKey`, `KeyUnavailable`, `Accepted`, or `Verified`.
Only key discovery and key unavailability temporarily prevent sending.
`Accepted` remains usable while showing either the yellow first-use warning or a
red continuity-change warning. Background authentication never opens modal
stacks; only explicit `/identity` requests open the verification screen.

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
- `security.transport-mode`: `"native-encrypted"` (default) has chatt secure the
  wire with session keys; `"external-secure-link"` defers wire security to an
  outer tunnel, sending payloads clear after the signed handshake and disabling
  P2P.
- `security.chat-history-limit`: number of messages to keep in memory for new
  joins. Use `0` for no retained message bodies.
- `security.max-file-size-bytes`: server-side file relay limit, capped by the
  RPC protocol maximum.
- `[[rooms]]`: configured rooms. The current client flow expects room id `1` as
  the default lobby.
- User records live in `users.toml` under the storage data dir, written by the
  server as invites are accepted. Token hashes are stored as
  `sha256:<64 hex chars>`. Invite secrets are never stored on disk.

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
   user does not need to exist yet. A new invite for the same user
   replaces the previous one. Invites expire after 24 hours.
2. The server returns a `tcj1_...` join string containing the server addresses,
   server public key, default room, and one-time invite secret. It does not
   carry the internal user identifier. The addresses come from
   `network.public-*`, not from the local bind addresses, so deployments behind
   NAT or DNS use their externally reachable connection details.
3. The user runs `chatt pair JOIN_STRING`. The client derives a local server
   label from the address, seeds the username from the operating system
   account name, and generates a long client token.
4. The client performs the normal server-authenticated handshake. Inside the
   server-selected control channel, it sends `ClientControl::Pair` containing
   the display name derived from the username, invite secret, and new token. No
   user identifier is sent.
5. The server matches the invite by its secret, which selects the user the
   invite was issued for, removes the invite, rejects the token if its hash
   already belongs to another user, hashes the new token, creates or updates
   the user-registry record with the token hash and display name, and
   authenticates the current session.
6. After successful authentication, the client writes a labeled `[[servers]]`
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
  `generate_client_hello`, `respond_to_client_hello`, and
  `complete_client_transport_handshake`; the negotiated `TransportMode` and the
  per-session `SessionTransport` (route id, bind key, video auth key); key
  derivation in `derive_session_transport`; record-lane selection in
  `RecordProtection`; AEAD framing in `TransportCipher`, `seal_with_key`, and
  `open_with_key`; truncated-HMAC setup proofs in `auth_proof`; replay tracking
  in `AntiReplay`.
- `crates/rpc/src/e2e.rs`: DM pair-key derivation, envelope sealing/opening,
  content-class and room/sender AAD, and plaintext padding.
- `crates/rpc/src/media.rs`: the per-session `MediaProtection` codec and its
  `seal_media`/`open_media` (returning `OpenedMedia` with an `AddressProof`),
  the raw-key peer codec `seal_peer_media`/`open_peer_media`, and `parse_header`.

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

- `src/config.rs`: client TOML fields in `ServerEntry`, durable DM identity
  tuples and trust levels, and runtime persistence in `write_runtime_config`.
- `src/e2e_identity.rs`: canonical X25519 hex/word presentation and
  context-bound verification-card parsing and checksums.
- `src/e2e.rs`: pinned-room classification, per-session DM identity state,
  snapshot-checked TOFU/verification transitions, authenticated sender binding,
  exact-key message provenance, and chat envelope policy.
- `src/app/room.rs`, `src/app/mod.rs`, and `src/tui/overlay.rs`: room-keyed
  trust projection, Chat-bar warnings, requester-scoped dialog routing, and the
  verification interface.
- `src/room_history.rs`, `src/chat_buffer.rs`, `src/web_wire.rs`, and
  `web/src/App.tsx`: exact-key provenance persistence and `(Unverified)`
  annotations in retained TUI and browser messages.
- `src/cli/mod.rs`: join-string pairing CLI and named-server persistence after
  successful pairing.
- `src/client_net.rs`: server connection and handshake in
  `connect_and_handshake`; public key pin selection in
  `pinned_server_public_key`; first auth or pairing message in
  `run_worker_inner`; control send in `WorkerState::queue_control`;
  server message handling and the transient pre-key ordering queue in
  `WorkerState::handle_control`, `defer_e2e`, and `drain_deferred_e2e_room`;
  UDP media send and receive in
  `WorkerState::send_media`, `WorkerState::handle_udp_packet`, and
  `WorkerState::handle_p2p_media`.

## Open Security Work

- Add hybrid post-quantum key agreement. The target shape is X25519 plus
  ML-KEM, with both shared secrets mixed into HKDF and the full KEM transcript
  authenticated by the server signature.
- Add post-quantum or hybrid server authentication when a production-quality
  Rust implementation is chosen.
- Implement session rekeying. Constants exist for rekey timing, but the current
  transport does not perform an in-band rekey.
- If public/private room content must survive server compromise, add group
  end-to-end encryption. Direct messages use TOFU client identity keys and
  support independent word/card verification; multi-device identity
  policy and key transparency remain future work.

References:

- NIST FIPS 203, Module-Lattice-Based Key-Encapsulation Mechanism Standard:
  https://csrc.nist.gov/pubs/fips/203/final
- NIST FIPS 204, Module-Lattice-Based Digital Signature Standard:
  https://csrc.nist.gov/pubs/fips/204/final
- NIST SP 800-56C Rev. 2, Recommendation for Key-Derivation Methods in
  Key-Establishment Schemes:
  https://csrc.nist.gov/pubs/sp/800/56/c/r2/final
