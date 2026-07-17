# Chatt Encryption, Pairing, and Server Configuration

This document describes the current security design and the implementation map
for human audit. Function names are listed without line numbers so the map stays
useful as the code moves.

## Security Status

### Cryptographic implementation

AWS-LC is Chatt's sole cryptographic implementation. The transport, media,
identity, token, and shared protocol paths call `aws-lc-rs`; the MLS paths use
the in-workspace `mls-rs-crypto-awslc` provider backed by the same
`aws-lc-rs`/`aws-lc-sys` versions. Chatt has no `ring` cryptographic backend.
Some AWS-LC Rust APIs intentionally resemble `ring` APIs, so the type and method
names alone do not identify the provider.

The non-MLS protocol uses AWS-LC's X25519, Ed25519, ChaCha20-Poly1305,
HKDF-SHA256, HMAC-SHA256, SHA-256, and secure random generation. Chatt's only
enabled MLS cipher suite is `CURVE25519_AES128`, comprising X25519 and Ed25519
with AES-128-GCM, HKDF-SHA256, HMAC-SHA256, and SHA-256.

The workspace sets `AWS_LC_SYS_CFLAGS=-DOPENSSL_SMALL` in
`.cargo/config.toml`. This changes the native AWS-LC library used by all of the
paths above; it is not scoped to the MLS crate. AWS-LC's small build replaces
the approximately 30 KiB Curve25519 fixed-base precomputation table with an
approximately 1 KiB table and a more computationally expensive fixed-base
scalar multiplication. Performance evaluation should consequently include
Ed25519 key generation and signing in addition to verification, X25519/HPKE,
AEAD, hashes, and KDFs, and should use distinct target directories for builds
with and without the flag to prevent Cargo from reusing the wrong
`aws-lc-sys` artifact.

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
  Each account has a stable Ed25519 authority and an authority-signed,
  append-only device ledger. Every installation has independent Ed25519 event
  signing and X25519 delivery keys, a random 128-bit device id, and monotonic
  key epochs. Ledger actions add devices, rotate a device key with an old-key
  co-signature, retire a key with final sender-chain heads, revoke a device,
  rotate the account authority with a new-authority co-signature, and update
  the recovery-bundle hash. Retired and revoked material is terminal: it can
  validate history at the roster checkpoint that originally authorized it but
  can never become current sending or delivery authorization again.

  A sender creates one random content key per event and wraps it independently
  to every active device on both the sender and recipient accounts using an
  ephemeral X25519 agreement. The author device signs the complete envelope:
  random sender event id, room, sender/account/device/key epoch, content class,
  authenticated creation time, per-room/device sequence and predecessor,
  both signed roster checkpoints, the exact recipient wrap set, and ciphertext.
  The server requires a session-bound proof from an active device before it
  accepts a DM event and rejects events whose author or roster head differs
  from that binding. A ledger advance invalidates every account session binding
  until each still-active device validates the new chain and binds again.

  Clients persist peer and local ledger heads and accept only exact append-only
  extensions. A full response that drops or changes a persisted prefix is a
  rollback/fork error, including after server data restoration. Before any
  event effect, clients durably journal its sender event id and ciphertext
  digest. Exact duplicates, same-id forks, stale live sequences, and broken
  immediate predecessor chains are discarded. This prevents a relay from
  replaying an old text, file announcement, edit, or deletion under a fresh
  server message id. Displayed DM timestamps come from authenticated plaintext;
  server ids/timestamps remain routing and pagination metadata only.

  File announcements are ordinary signed sender events. Their event id is also
  included in every file-chunk AEAD transcript, alongside room, sender, and
  chunk counter, so chunks cannot be transplanted between transfers. Chat
  plaintexts retain length padding; sealed file streams retain Padmé padding.

  A new account identity is stored in an owner-only, atomic local identity file
  scoped by server public key and authenticated user id. `/devices link`
  creates a short-lived one-time link and encrypted authority-transfer bundle;
  `/device-pair` on the new installation opens it locally, signs an `AddDevice`
  transition, and retains independent device keys. Each new link replaces the
  prior outstanding link, while closing its dialog does not cancel it. The
  server never receives the authority seed. `/devices` lists device ids and
  states, and `/devices revoke <device-id-hex>` creates a terminal signed
  revocation. Offline recovery codes are intentionally not supported. A
  confirmed `/devices reset` is the destructive recovery boundary: the server
  accepts it only from a still-registered bearer bound to an active device,
  advances the account generation, rebinds that bearer to the replacement
  genesis device, removes other credentials and outstanding links, and clears
  verification sync. The client stages the replacement identity before the
  request and archives the old file only after confirmation.

  `/identity` verification now compares the stable account id, so legitimate
  device addition or rotation does not change the human-verified fingerprint.
  Display names are never trust material. The server still sees DM participants,
  delivery timing, padded size classes, and server-visible ownership targets;
  DM voice/video remain transport-encrypted rather than end-to-end encrypted.
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

The canonical identity formatter in `src/e2e_identity.rs` validates the
32-byte stable account id and exposes two losslessly related public
presentations:

- Lowercase hexadecimal is the complete raw account id as 64 unseparated digits.
- `Chatt account identity words` append the first eight bits of
  `SHA-256(account_id)` to the raw 256 identity bits, read the 264 bits
  most-significant-bit first as 24 11-bit indices, and map those indices through
  the 2048 unique lowercase words in `assets/english.txt`.

The hash byte is only a transcription checksum. The words are not a wallet
seed, recovery phrase, secret, or separate SHA-256 identity fingerprint. The
first 256 encoded bits are exactly the stable account id Chatt will pin.

A public verification card is one canonical line:

```text
chatt-e2e:v2:<server-ed25519-key-base32>:<user-id-decimal>:<account-id-base32>:<checksum-base32>
```

The checksum is the first eight bytes of SHA-256 over the preceding canonical
ASCII fields. The binary identity and checksum fields use Chatt's lowercase,
unpadded Crockford base32 encoding; this shortens the card without replacing or
truncating the server key or account id. The checksum detects damaged pastes; it is not a
signature or proof. The full server Ed25519 key prevents a card from being
accidentally applied to another configured server, and the user id prevents
applying it to another account. Import marks a key `Verified` only when server,
expected peer user id, and exact presented/accepted account id all match.
Wrong-server, wrong-user, self-card, malformed, checksum, and stale-account cases
are distinct errors. A key mismatch has no verification action in the dialog.

The Chat status bar and browser warning project account-level `Accepted` or
`Verified` state separately from device-ledger authorization. Background
authentication never opens modal stacks; only explicit `/identity` requests
open the verification screen.

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
- `crates/rpc/src/e2e.rs`: account/device ledger validation, signed rotations
  and revocations, session-binding proofs, multi-recipient event sealing/opening,
  sender chains, file-event chunk binding, and plaintext padding.
- `crates/rpc/src/media.rs`: the per-session `MediaProtection` codec and its
  `seal_media`/`open_media` (returning `OpenedMedia` with an `AddressProof`),
  the raw-key peer codec `seal_peer_media`/`open_peer_media`, and `parse_header`.

Server:

- `crates/server/src/config.rs`: TOML loading in `Config::load`; Ed25519
  identity loading in `Config::server_key_pair`; pairing persistence in
  `Config::mark_user_paired`; token helpers in `hash_secret` and
  `verify_secret_hash`.
- `crates/server/src/device_directory.rs`: durable authority-signed account
  ledgers, append compare-and-swap validation, current active-device lookup,
  and opaque recovery bundles.
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

- `src/config.rs`: client TOML fields in `ServerEntry`, optional recovery-link
  code, durable account verification pins, and runtime persistence.
- `src/e2e_identity.rs`: canonical account-id hex/word presentation and
  context-bound verification-card parsing and checksums.
- `src/e2e_store.rs`: owner-only device private keys, account and peer ledger
  checkpoints, recovery material, sender sequence reservations, and replay
  journal.
- `src/e2e.rs`: account recovery/device enrollment, roster-based recipient
  selection, current-versus-historical key authorization, authenticated sender
  events, replay observation, and account-level verification projection.
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
  account-ledger synchronization and device session binding; server message
  handling and the transient pre-ledger ordering queue in
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
- Add an externally auditable key-transparency log or witness gossip for account
  ledger heads. Signed client checkpoints already make rollback/forks locally
  detectable, but independent witnesses would make equivocation detectable
  across clients that never communicate.
- If public/private room content must survive server compromise, add group
  end-to-end encryption; the account/device primitives currently protect DMs.

References:

- NIST FIPS 203, Module-Lattice-Based Key-Encapsulation Mechanism Standard:
  https://csrc.nist.gov/pubs/fips/203/final
- NIST FIPS 204, Module-Lattice-Based Digital Signature Standard:
  https://csrc.nist.gov/pubs/fips/204/final
- NIST SP 800-56C Rev. 2, Recommendation for Key-Derivation Methods in
  Key-Establishment Schemes:
  https://csrc.nist.gov/pubs/sp/800/56/c/r2/final
