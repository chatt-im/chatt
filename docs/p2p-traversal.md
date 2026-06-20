# P2P traversal design

Tomchat keeps the existing server UDP media path as a relay candidate and adds a
direct UDP path beside it. The relay remains usable immediately, then direct
candidate checks can replace it when a validated peer path is available.

The traversal core lives in `crates/p2p`. It is a deterministic ICE-like state
machine driven by the client event loop:

- Gather host candidates from active non-virtual interfaces.
- Discover server-reflexive candidates with encrypted NAT probes sent from the
  same UDP socket used for media.
- Exchange host, server-reflexive, and relay candidates over encrypted TCP
  signaling.
- Pace STUN Binding checks at 25 ms minimum spacing.
- Retransmit checks with 100, 200, 400, and 800 ms exponential backoff.
- Keep the relay path active for immediate fallback.
- Nominate direct paths only after a STUN response or authenticated packet is
  observed from the actual source address.

## Required failure handling

Symmetric-symmetric peers do not wait for doomed checks. They use the relay
immediately because static UDP punching cannot solve two destination-dependent
port mappings.

Symmetric-to-cone peers are asymmetric. The symmetric peer sends first. The cone
peer treats the inbound STUN request source as a peer-reflexive candidate and
replies there, so it does not need to guess the symmetric peer's new external
port.

Endpoint-independent mapping deviations are handled by limited, sequential port
guesses after ordinary checks fail. Guesses are paced by the same 25 ms limiter
to avoid port-scan signatures.

Host, server-reflexive, and relay candidates are all present in the checklist.
Host-host checks cover LAN peers and NATs without hairpin support. Relay covers
IPv4/IPv6 mismatches and all hard NAT/firewall failures.

Liveness uses STUN keepalives every 10 seconds. Five seconds without inbound
traffic requests an ICE restart. Fifteen seconds without inbound traffic tears
the direct path down and leaves media on the relay.

## Runtime integration

The server is the signaling coordinator, not the media bottleneck for direct
paths. After UDP bind, each client publishes:

- host candidates from active non-virtual interfaces;
- the server-reflexive address observed by the server from that same UDP socket;
- the existing server UDP endpoint as the relay candidate.

For every pair of room members that publish candidates, the server distributes a
pair-specific connection ID and two symmetric media keys. Each side gets one key
for sending and the opposite key for receiving. Direct voice packets use the
`PeerVoice` media payload, which carries the connection ID inside the encrypted
payload so the client can migrate to a new source IP/port after Wi-Fi/LTE
roaming without binding cryptographic state to the old socket address.

The client always keeps sending through the server relay. Once a direct path is
selected, it also sends peer-encrypted packets directly. Receivers feed both
paths into the existing jitter buffer; duplicate sequence numbers are dropped,
so the relay remains a seamless fallback while the direct packet usually wins on
latency.

NAT classification uses two server UDP endpoints. Stable observed mappings are
classified as cone; destination-dependent mappings are classified as symmetric.
Set `TOMCHAT_P2P_NAT=symmetric` or `TOMCHAT_P2P_NAT=cone` to override local NAT
classification when testing known topologies.

The client polls local interfaces every 2 seconds. Interface/IP changes trigger
a P2P restart, clear stale reflexive state, rebind the UDP socket to a fresh
ephemeral port, and re-publish candidates without stopping the relay path.

## Validation

`cargo test --workspace` runs deterministic simulator coverage for the 23 NAT,
topology, firewall, socket, lifecycle, and timing edge cases. The Linux network
namespace smoke test is opt-in:

```sh
sudo TOMCHAT_NETNS_TESTS=1 cargo test -p tomchat-p2p --test netns
```
