# Chatt MLS boundary

Chatt uses `mls-rs` directly instead of Wire CoreCrypto or OpenMLS. The exact
upstream revision is pinned in `Cargo.toml` and `Cargo.lock`:

```text
42131c9959efb1d3928428259bc89853027f730d
```

The dependency spike rejected CoreCrypto because its public Rust API could not
stage and inspect a received commit's complete leaf credential/signature-key
set before durable merge. It also pulled 342 normal dependencies in the
minimal native check. The selected `mls-rs` feature set built with 38 normal
dependencies and exposes the policy points Chatt needs:

- a custom `IdentityProvider` for authority-certified Basic credentials;
- `MlsRules` callbacks for both sent and received commits;
- explicit pending-commit apply/clear operations;
- explicit `write_to_storage`, allowing crash-safe cache/ratchet ordering;
- an `ExternalClient` for the server's public-group observer;
- out-of-order private-message handling;
- SQLCipher-backed group, KeyPackage, and application storage.

The initial cipher suite is
`MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`. Handshakes are public and
application messages are private. Chatt does not enable X.509, PSKs, draft/PQ
extensions, history-secret transfer, or a legacy encryption protocol here.

The `mls-rs` RustCrypto provider describes itself as experimental and is not a
separately audited Chatt cryptographic module. The implementation therefore
pins its full source revision, keeps the provider behind this crate boundary,
and treats a provider/runtime upgrade as a security-sensitive change requiring
the policy and interoperability tests in this crate to be rerun and reviewed.

`policy.rs` is the shared client/server authorization layer. `persistent.rs`
owns explicit MLS persistence and the encrypted event/outbox cache. `server.rs`
contains the secret-free public group validator. Protocol wire types remain in
`crates/rpc`.
