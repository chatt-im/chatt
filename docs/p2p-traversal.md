# P2P traversal specification

This document specifies the chatt peer-to-peer media traversal mechanism in
enough detail to build an interoperable implementation. It covers the signaling
protocol, the candidate and NAT model, the connectivity state machine, the media
wire format, and the dynamic relay/direct switch.

Chatt keeps the server UDP media relay as a permanent candidate and adds a direct
UDP path beside it. The relay is usable immediately. A validated direct path then
takes over, and the client stops relaying once the direct path is proven. The
server is trusted and decrypts media to route it. Direct media is encrypted under
per-pair keys the server mints, so this is not end-to-end encryption.

## 1. Components

- `crates/p2p` (`chatt-p2p`): the sans-IO traversal core. It owns no socket and no
  timer. The application feeds it inbound STUN and authenticated-packet
  observations plus a monotonic clock, and sends the `Action` packets it returns.
  Modules: `agent` (the state machine), `candidate` (candidate model, priority,
  pairing, port guessing), `nat` (reflexive-based NAT classification), `stun`
  (STUN subset), `restart` (rebind port quarantine), `interfaces`, `socket`.
- `crates/rpc`: the shared protocol. `control` carries the encrypted-TCP signaling
  messages, `media` defines the UDP media framing, `crypto` defines the AEAD and
  anti-replay. `PROTOCOL_VERSION` is `2`.
- `crates/server`: the signaling coordinator and the media relay. It pairs room
  members, mints per-pair connection IDs and keys, observes reflexive addresses,
  and relays UDP media.
- Client integration: `src/client_net.rs` runs the network worker that owns the
  TCP and UDP sockets, drives one `TraversalAgent` per peer, and implements the
  relay/direct switch.

The agent is deterministic. Identical inputs (clock values, inbound bytes,
candidate sets) produce identical `Action` sequences, which is what the simulator
tests exploit.

## 2. Roles and terminology

- Candidate: a transport address a peer might be reachable at, with a kind and a
  priority.
- Candidate pair: one local candidate and one remote candidate of the same
  address family, both direct.
- ICE role: `Controlling` or `Controlled`. The controlling agent's nomination
  wins ties. The server seeds the initial role from session-id ordering, and the
  agent resolves runtime conflicts (glare) from the STUN role attribute and a
  64-bit tie-breaker.
- Relay candidate: the server UDP media endpoint. Always present, never removed.
- Connection ID: a 64-bit per-pair identifier the server mints. It is carried
  inside encrypted direct media so a receiver can migrate a peer to a new
  IP/port without rebinding cryptographic state.

## 3. Signaling protocol (encrypted TCP control channel)

All control messages serialize with `jsony` `#[jsony(Binary, version)]` and travel
on the encrypted control channel (`CHANNEL_CONTROL = 1`). Maximum control payload
is 65536 bytes. Candidate lists are capped at 64 entries (`MAX_P2P_CANDIDATES`).

### 3.1 Client to server

`ClientControl::PublishP2p` publishes the local candidate set and NAT state for a
room. The client re-sends it whenever its reflexive address, NAT classification,
or interface set changes, bumping `generation` on a restart.

| Field | Type | Meaning |
| --- | --- | --- |
| `room_id` | `RoomId` (u32) | room the candidates apply to |
| `generation` | `u64` | restart counter, starts at 1, `wrapping_add(1).max(1)` |
| `nat` | `P2pNatKind` | `Unknown` \| `Cone` \| `Symmetric` |
| `tie_breaker` | `u64` | random per session, for glare resolution |
| `candidates` | `Vec<P2pCandidate>` | local candidates, max 64 |

`P2pCandidate`:

| Field | Type | Meaning |
| --- | --- | --- |
| `id` | `u32` | candidate id, unique within the publisher |
| `socket_id` | `u32` | originating socket/component, `1` |
| `generation` | `u64` | matches the publish generation |
| `kind` | `P2pCandidateKind` | `Host` \| `ServerReflexive` \| `PeerReflexive` \| `PortMapped` \| `Relay` |
| `addr` | `String` | `ip:port` |
| `priority` | `u32` | see section 4 |
| `foundation` | `String` | `"{kind}-{udp4\|udp6}"`, e.g. `host-udp4` |
| `verified` | `bool` | true for host candidates and locally observed addresses |

### 3.2 Server to client

| Message | Fields | When |
| --- | --- | --- |
| `UdpBound` | none | after the server sees the client's first `Bind` |
| `UdpReflexive` | `addr: String` | the source address the server observed for the client's media socket |
| `P2pNatProbe` | `probe_id: u8`, `addr: String` | the source address observed for a `NatProbe`, per probe endpoint |
| `P2pPeer` | `peer: P2pPeerInfo` | a room peer's candidates and keys are available |
| `P2pPeerGone` | `session_id: SessionId`, `user_id: UserId` | a peer left the room or disconnected |

`P2pPeerInfo`:

| Field | Type | Meaning |
| --- | --- | --- |
| `room_id` | `RoomId` | room |
| `session_id` | `SessionId` (u64) | the peer's session |
| `user_id` | `UserId` (u32) | the peer's user |
| `generation` | `u64` | the peer's published generation |
| `role` | `P2pRole` | `Controlling` \| `Controlled`, this recipient's seeded role |
| `nat` | `P2pNatKind` | the peer's published NAT kind |
| `tie_breaker` | `u64` | the peer's published tie-breaker |
| `candidates` | `Vec<P2pCandidate>` | the peer's candidates |
| `send_key` | `P2pKey` | key this recipient uses to seal media to the peer |
| `recv_key` | `P2pKey` | key this recipient uses to open media from the peer |
| `connection_id` | `u64` | shared per-pair connection id |

`P2pKey` is `{ id: u32, bytes: Vec<u8> }` where `bytes` is 32 bytes and `id` is the
nonzero key id used in the media header.

## 4. Candidate model

A candidate has a kind, an address, a base, a priority, and a foundation.

Type preferences (higher is preferred):

| Kind | Type preference |
| --- | --- |
| `Host` | 126 |
| `PeerReflexive` | 110 |
| `PortMapped` | 105 |
| `ServerReflexive` | 100 |
| `Relay` | 0 |

Candidate priority is

```
priority = (type_preference << 24) | (local_preference << 8) | (256 - component)
```

with `component = 1` (so the low byte is 255), and `local_preference = 65535` for
IPv4, `65534` for IPv6. IPv4 is preferred over IPv6 at equal type.

Foundation is `"{kind_name}-{family}"` with kind names `host`, `srflx`, `prflx`,
`map`, `relay` and family `udp4` or `udp6`.

A pair forms only when both candidates share an address family, both are direct
(non-relay), and neither address is unspecified. The relay candidate never pairs.
It is the fallback target, not a check target.

Pair priority follows RFC 5245 form. With `G` the controlling side's candidate
priority and `D` the controlled side's:

```
pair_priority = (2^32 - 1) * min(G, D) + 2 * max(G, D) + (G > D ? 1 : 0)
```

The agent recomputes pair priorities whenever its role changes.

## 5. NAT classification

The client classifies its own NAT from server-reflexive observations. Each
observation is a `(server_addr, mapped_addr)` pair: the server endpoint a probe
was sent to, and the mapped address the server reported back.

- Fewer than two observations from distinct server endpoints: `Unknown`.
- Two or more observations with identical mapped addresses: `Cone`.
- Mapped addresses differ across server endpoints: `Symmetric`.

The primary reflexive address is the mapped address observed from the
lowest-sorted server endpoint.

Distinguishing cone from symmetric requires a second server UDP endpoint,
configured as `udp-probe-addr`. The client sends `NatProbe` to both endpoints
(`probe_id` 0 for the main media endpoint, 1 for the probe endpoint). Without a
probe endpoint the client still publishes a server-reflexive candidate but leaves
NAT kind `Unknown`. `CHATT_P2P_NAT=cone` or `CHATT_P2P_NAT=symmetric` overrides
classification for testing.

## 6. Server coordination

The server holds, per session, the last published `{generation, nat, tie_breaker,
candidates}` and the UDP source address it last saw for that session's media
socket. It refreshes the UDP address on every inbound media datagram and never
expires it.

### 6.1 Pairing

When a member publishes, the server pairs it with every other room member that has
also published. Pairs are keyed by the ordered session-id tuple. For each new pair
the server mints a `PeerLink`:

- `connection_id`: a `u64` from a counter, `wrapping_add(1).max(1)`, never 0.
- two 32-byte keys from a CSPRNG, `low_to_high` and `high_to_low`, each with a
  nonzero random `u32` id.

The server sends each side a `P2pPeer`. Key assignment is by session-id order: the
lower session id sends with `low_to_high` and receives with `high_to_low`, the
higher session id is the mirror. The seeded role is also by order: the lower
session id is `Controlling`, the higher is `Controlled`.

### 6.2 Generations and restart

A republish with a new `generation` replaces the stored candidate set and re-sends
`P2pPeer` to the pair with the new candidates and generation. The existing
`PeerLink` is reused, so the connection id and both keys are stable across ICE
restarts. Roles do not change. Key rotation is not part of restart.

### 6.3 Departure

On room leave or TCP disconnect the server sends `P2pPeerGone { session_id,
user_id }` to the remaining room members and drops every `PeerLink` that contains
the departed session.

## 7. Media transport

### 7.1 UDP datagram framing

Every media datagram begins with a 14-byte plaintext header
(`UDP_HEADER_LEN = 14`):

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | `version`, `UDP_VERSION = 3` |
| 1 | 1 | `kind` (see below) |
| 2 | 4 | `key_id`, u32 little-endian, `0` means plaintext |
| 6 | 8 | `counter`, u64 little-endian |

Kinds:

| Value | Kind | Path |
| --- | --- | --- |
| 1 | `Bind` | client to server |
| 2 | `Voice` | relay |
| 3 | `Ping` | keepalive |
| 4 | `Pong` | keepalive |
| 5 | `PeerVoice` | direct |
| 6 | `NatProbe` | client to server |
| 7 | `VoiceFeedback` | relay |
| 8 | `PeerVoiceFeedback` | direct |

The body after the header is the payload encoding. For encrypted packets the body
is the ChaCha20-Poly1305 ciphertext of the payload encoding plus a 16-byte tag. For
plaintext packets (`key_id = 0`, used for `Bind` and `NatProbe` before media keys
exist) the body is the payload encoding directly.

AEAD parameters for media:

- key: the 32-byte `KeyMaterial.bytes`, identified by `key_id`.
- nonce: 12 bytes, `[0,0,0,0]` followed by `counter` little-endian (8 bytes).
- associated data: `[CHANNEL_MEDIA = 2]` followed by the 12-byte transport header
  `key_id` (4, LE) and `counter` (8, LE). The 14-byte UDP header's version and
  kind bytes are not in the AAD, only key_id and counter are.

### 7.2 Payload encoding

The payload encoding starts with a repeated 1-byte kind, then fields, all integers
little-endian:

- `Bind`: `session_id` u64.
- `NatProbe`: `session_id` u64, `probe_id` u8.
- `Voice`: `stream_id` u32, `sequence` u32, `flags` u8, voice payload.
- `PeerVoice`: `connection_id` u64, `stream_id` u32, `sequence` u32, `flags` u8,
  voice payload.
- `VoiceFeedback`: `stream_id` u32, feedback block (20 bytes).
- `PeerVoiceFeedback`: `connection_id` u64, `stream_id` u32, feedback block.
- `Ping` / `Pong`: `nonce` u64.

Voice payload: 1 type byte (`0` Opus, `1` Silence), then a u16 little-endian
length, then the bytes. Opus length is 1..=1024. Silence carries length 0.

Feedback block (20 bytes): `highest_contiguous_sequence` u32, then eight u16
fields `expected_packets`, `lost_packets`, `late_packets`, `duplicate_packets`,
`reordered_packets`, `window_ms`, `max_queue_ms`, `max_interarrival_jitter_ms`.

The encoded payload must not exceed `SAFE_UDP_PAYLOAD_BYTES = 1200`.

### 7.3 Anti-replay

Each receiving key tracks a sliding-window anti-replay state over the `counter`.
The window is 2048 bits. A counter above the current high water mark advances the
window. A counter inside the window is accepted once and then marked seen. A
counter below the window or already seen is rejected. Direct media and relayed
media each have their own counter space and replay state.

### 7.4 Dual path and de-duplication

A speaking client sends each frame on the relay as `Voice` and, when a direct path
is selected, on the direct path as `PeerVoice`. A receiver feeds both into one
jitter buffer and de-duplicates by `(stream_id, sequence)` across a 512-entry
window, so whichever copy arrives first wins and the other is dropped. The relay
and direct copies use different sequence-number spaces only at the transport
header level. The voice `sequence` is shared, which is what makes de-duplication
work.

The server relays `Voice` to every room member except the sender, sealing each
copy under that recipient's server media key. It relays exactly what it receives.
There is no server-side relay suppression. A sender that stops emitting `Voice`
stops being relayed.

## 8. STUN subset

Connectivity checks use a STUN Binding subset over the media UDP socket. STUN and
media share the socket. A datagram is STUN when it is at least 20 bytes, its first
two bits are zero, and bytes 4..8 equal the magic cookie `0x2112A442`.

Header (20 bytes): 2-byte type, 2-byte attribute length, 4-byte magic cookie,
12-byte transaction id. The transaction id is `b"tchp"` followed by an 8-byte
big-endian counter. Message types used are Binding Request `0x0001` and Binding
Success `0x0101`.

Attributes (4-byte type, 2-byte length, value padded to 4 bytes):

| Attribute | Type | Use |
| --- | --- | --- |
| `USERNAME` | `0x0006` | `"chatt-p2p:{connection_id}"`, connection id in decimal |
| `XOR-MAPPED-ADDRESS` | `0x0020` | reflexive address in a success response |
| `PRIORITY` | `0x0024` | the check's local candidate priority |
| `USE-CANDIDATE` | `0x0025` | nomination flag, controlling side |
| `SOFTWARE` | `0x8022` | `"chatt-p2p"` |
| `ICE-CONTROLLED` | `0x8029` | 8-byte tie-breaker, controlled side |
| `ICE-CONTROLLING` | `0x802a` | 8-byte tie-breaker, controlling side |

`XOR-MAPPED-ADDRESS` follows RFC 5389: port xored with the high 16 bits of the
cookie, IPv4 address xored with the cookie, IPv6 address xored with the cookie
concatenated with the transaction id.

A Binding Request is accepted only when its `USERNAME` matches the agent's
configured username (the pair's `chatt-p2p:{connection_id}`). Success responses are
matched by transaction id, not username.

## 9. Traversal state machine

One `TraversalAgent` runs per peer. The application drives it through three entry
points and sends the returned `Action`s.

### 9.1 Inputs

- `poll(now) -> Vec<Action>`: time-driven. Drives pacing, retransmission,
  fallback, keepalive, and idle timeouts.
- `handle_inbound(now, src, bytes) -> Result<Vec<Action>, StunError>`: an inbound
  STUN datagram from `src`.
- `observe_authenticated_packet(now, src) -> Option<Action>`: a decrypted direct
  media datagram arrived from `src`. This both confirms liveness and drives
  migration.

`selected() -> Option<SelectedPair>` exposes the current direct target.
`SelectedPair` carries the chosen candidate ids, the `remote_addr` to send to, and
a `peer_reflexive` flag.

### 9.2 Actions

| Action | Meaning |
| --- | --- |
| `UseRelay { relay, reason }` | use the relay, with a `FallbackReason` |
| `SendStun { to, bytes, transaction_id, pair, retransmit }` | send a connectivity check |
| `SendStunResponse { to, bytes, transaction_id }` | reply to an inbound request |
| `SendKeepalive { to, bytes, transaction_id }` | send a keepalive request |
| `DirectReady { selected }` | a direct path is selected |
| `Migrated { selected }` | the selected path moved to a new address |
| `IceRestart { reason }` | the path is stale, restart ICE |
| `Disconnected` | the path is dead, tear it down |

`FallbackReason` is `SymmetricSymmetric`, `NoCommonAddressFamily`,
`RelayCandidateAvailable`, or `DirectChecksFailed`.

### 9.3 Pair states

A pair is `Waiting`, `InProgress`, `Succeeded`, or `Failed`. On creation a pair is
`Waiting` with the configured initial RTO. Sending a check moves it to
`InProgress`. A matching success response moves it to `Succeeded` and selects it.
Exhausting retransmissions past the deadline moves it to `Failed`.

### 9.4 poll() decision order

1. Mark expired pairs. After `handshake_min_duration`, any `InProgress` pair that
   has used all `max_check_attempts` and whose first send is older than
   `check_deadline` becomes `Failed`.
2. If a pair is selected, run the selected-path branch (section 9.7) and return.
3. If fallback has not been announced yet:
   - both sides symmetric: announce `UseRelay(SymmetricSymmetric)` and return.
     Two destination-dependent mappings cannot be punched with static guesses.
   - no pairs at all: announce `UseRelay(NoCommonAddressFamily)` and return.
   - otherwise, if a relay candidate exists, announce
     `UseRelay(RelayCandidateAvailable)` and continue. The relay is usable
     immediately while checks proceed.
4. If all pairs have failed and guesses were not added yet, add port guesses
   (section 9.6) and rebuild pairs.
5. If all pairs have failed and `handshake_min_duration` has elapsed, announce
   `UseRelay(DirectChecksFailed)` and return.
6. If `now` is before the next allowed check time, return. Checks are paced at
   `min_check_interval`.
7. Otherwise pick the next check. A due retransmission with attempts left and the
   highest priority wins, else the highest-priority `Waiting` pair that is allowed.
   Send it and schedule the next check `min_check_interval` later.

### 9.5 Selecting a path

`handle_inbound` first updates the last-receive time and clears the restart and
disconnect latches, then resolves role conflicts (section 9.8).

- Binding Request: marks peer-reflexive seen, selects the request's source as a
  peer-reflexive candidate (adding a remote candidate if the source is unknown),
  replies with a Binding Success carrying `XOR-MAPPED-ADDRESS = src`, and emits
  `SendStunResponse` then `DirectReady`. This is how the cone side of a
  symmetric-to-cone pair selects: it replies to the actual source instead of
  guessing the symmetric peer's mapped port.
- Binding Success: matches the transaction, marks the pair `Succeeded`, and
  selects `src`. The pair is flagged peer-reflexive when `src` differs from the
  candidate address the check targeted. The agent nominates only after a response,
  or an authenticated packet, from the real source address.

`observe_authenticated_packet` selects or migrates from real media:

- no selection yet: select `src` peer-reflexive, emit `DirectReady`.
- selection at the same address: nothing.
- selection at a different address: move the selection to `src` peer-reflexive and
  emit `Migrated`. This handles Wi-Fi/LTE roaming without a renegotiation, because
  the connection id, not the address, binds the cryptographic state.

### 9.6 Limited port guessing

Once every ordinary pair has failed, the agent adds peer-reflexive guess
candidates derived from each IPv4 server-reflexive remote candidate, one time.
Guesses walk outward by port delta
`+1, -1, +2, -2, ...` up to `port_guess_max_delta`, capped at `port_guess_limit`,
skipping invalid ports. Guesses are paced by the same `min_check_interval` limiter
so the pattern does not look like a port scan.

The check-allowance rule also encodes the asymmetry: when the local NAT is cone,
the remote NAT is symmetric, and no peer-reflexive request has been seen yet, the
agent does not send checks to the remote's reflexive candidates. The symmetric
side sends first, and the cone side answers the inbound request.

### 9.7 Liveness, restart, and disconnect

Once a path is selected the agent sends a STUN keepalive every
`keepalive_interval` and watches the idle time since the last inbound STUN or
authenticated packet:

- idle past `restart_after_idle`: emit `IceRestart(Idle)` once.
- idle past `disconnect_after_idle`: emit `Disconnected` once.

Any inbound STUN or authenticated packet refreshes the last-receive time and
clears both latches, so a recovered path cancels a pending restart.

### 9.8 Role conflict resolution

The seeded role can collide (glare). On an inbound request the agent compares its
role and tie-breaker to the peer's STUN role attribute:

- it is controlling and the peer is controlling with a larger tie-breaker: it
  becomes controlled.
- it is controlled and the peer is controlled with a smaller tie-breaker: it
  becomes controlling.

Either change recomputes pair priorities.

## 10. Timers and configuration

`AgentConfig` defaults:

| Parameter | Default | Role |
| --- | --- | --- |
| `min_check_interval` | 25 ms | minimum spacing between checks |
| `handshake_min_duration` | 3 s | floor before declaring direct failure or expiring pairs |
| `check_deadline` | 5 s | max age of an in-progress pair before failure |
| `initial_rto` | 100 ms | first retransmit timeout |
| `max_rto` | 800 ms | retransmit ceiling, doubling 100, 200, 400, 800 |
| `keepalive_interval` | 10 s | keepalive spacing on a selected path |
| `restart_after_idle` | 5 s | idle before `IceRestart` |
| `disconnect_after_idle` | 15 s | idle before `Disconnected` |
| `port_guess_limit` | 8 | max guesses per reflexive source |
| `port_guess_max_delta` | 8 | max port delta when guessing |
| `max_check_attempts` | 5 | sends before a pair can fail |

The client overrides `keepalive_interval` to 1 s per peer
(`P2P_KEEPALIVE_INTERVAL`). The tighter keepalive keeps the selected path's
liveness fresh through ordinary speech silence, which is what makes the relay
switch in section 11 detect failure in about a second and a half rather than five.

## 11. Dynamic relay/direct switch

Relaying every frame in parallel with the direct path is pure redundancy once the
direct path works. The client drops the relay once a direct path to every other
participant has been confirmed, and resumes it the moment a path degrades. This is
a client-only behavior. The server still relays whatever it receives.

### 11.1 Per-peer stability

The client tracks, per peer, the last inbound direct packet time and a
`direct_stable_since` instant. On every `poll` it reconciles stability from
liveness: a path is healthy when the agent has a selection and an inbound direct
packet arrived within `DIRECT_FAILOVER_IDLE`. A healthy path with no
`direct_stable_since` arms it to now. An unhealthy path clears it. This single
rule arms the confirmation clock, runs the failover watchdog, and re-arms after
recovery without special-casing agent actions.

### 11.2 Suppression decision

The client suppresses the relay when there is at least one other online
participant and every one of them has a peer whose `direct_stable_since` is at
least `DIRECT_CONFIRM_WINDOW` old. The membership set comes from the room roster
and presence updates. Because the relay is a single broadcast to all members, the
decision is all-or-nothing: a newcomer or any peer without a confirmed direct path
keeps the relay alive for everyone, so no participant is ever cut off.

While suppressed, a speaking client sends only `PeerVoice` and skips `Voice`. It
applies the same gate to `VoiceFeedback`: feedback for a stream whose owner is a
confirmed direct peer goes only on `PeerVoiceFeedback`.

### 11.3 Keeping the relay warm

While suppressed the client sends a `Bind` to the server every
`RELAY_KEEPALIVE_INTERVAL` so the on-path NAT binding and the server's stored UDP
address stay fresh. Resuming the relay is then immediate, with no rebind stall.

### 11.4 Resume on degradation

Resume is automatic. The stability reconcile clears `direct_stable_since` as soon
as the selection is lost or inbound direct traffic stalls past
`DIRECT_FAILOVER_IDLE`, which un-suppresses the relay on the next frame. The
1-second keepalive guarantees inbound traffic on a healthy path, so a dead path is
detected in about `DIRECT_FAILOVER_IDLE`. The agent's own `IceRestart` at 5 s and
`Disconnected` at 15 s remain as deeper recovery, the former triggering an ICE
restart and socket rebind.

### 11.5 Symmetry

Each client controls only its own uplink. A client suppresses its relay only after
`DIRECT_CONFIRM_WINDOW` of answered 1-second keepalives, which confirms the path
works in both directions. Each side's decision is independent, so no extra
coordination message is needed.

### 11.6 Constants

| Constant | Value | Role |
| --- | --- | --- |
| `DIRECT_CONFIRM_WINDOW` | 3 s | required healthy duration before dropping the relay |
| `DIRECT_FAILOVER_IDLE` | 1500 ms | inbound-direct idle that marks a path degraded |
| `RELAY_KEEPALIVE_INTERVAL` | 5 s | server keepalive cadence while suppressed |
| `P2P_KEEPALIVE_INTERVAL` | 1 s | STUN keepalive spacing on a selected path |

## 12. Restart and socket rebind lifecycle

The client polls interfaces every 2 seconds. An interface or address change, or an
`IceRestart` action, triggers a restart: bump `generation`, clear the reflexive
address, clear published candidates, reset the NAT classifier, and request a UDP
rebind. The rebind deregisters the socket, records the old port in a quarantine of
the last 16 ports for 120 seconds, binds a fresh ephemeral port (retrying up to 8
times and rejecting recently used ports), re-registers, re-sends `Bind` plus NAT
probes, and republishes candidates. The relay path is not interrupted during a
rebind.

## 13. Failure handling summary

- Symmetric to symmetric: relay immediately. Static punching cannot solve two
  destination-dependent mappings.
- Symmetric to cone: the symmetric side sends checks first. The cone side treats
  the inbound request source as a peer-reflexive candidate and answers there.
- Endpoint-independent mapping quirks: limited, paced sequential port guesses
  after ordinary checks fail.
- IPv4/IPv6 mismatch and hard NAT or firewall failures: relay. Host-host checks
  still cover LAN peers and NATs without hairpin support.
- Roaming: authenticated-packet migration moves the selected address under the
  stable connection id.

## 14. Validation

`cargo test --workspace` runs the deterministic simulator coverage for the NAT,
topology, firewall, socket, lifecycle, and timing edge cases, the media and STUN
wire round-trips, and the relay-switch unit tests. The Linux network namespace
smoke test is opt-in:

```sh
sudo CHATT_NETNS_TESTS=1 cargo test -p chatt-p2p --test netns
```
