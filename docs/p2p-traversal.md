# P2P traversal design

Tomchat keeps the existing server UDP media path as a relay candidate and adds a
direct UDP path beside it. The relay remains usable immediately, then direct
candidate checks can replace it when a validated peer path is available.

The traversal core lives in `crates/p2p`. It is a deterministic ICE-like state
machine driven by the client event loop:

- Gather host candidates from active non-virtual interfaces.
- Discover server-reflexive candidates with the same UDP socket used for media.
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
