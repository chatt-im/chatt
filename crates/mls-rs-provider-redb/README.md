# mls-rs-provider-redb

Persistent `redb` storage implementations for the `mls-rs-core` group state,
key package, and pre-shared key interfaces, plus an application key-value store.

```rust
use mls_rs_provider_redb::RedbDataStorageEngine;

let engine = RedbDataStorageEngine::open("mls.redb")?;
let client = mls_rs::Client::builder()
    .key_package_repo(engine.key_package_storage())
    .psk_store(engine.pre_shared_key_storage())
    .group_state_storage(engine.group_state_storage());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Security

`redb` does not encrypt database contents. MLS snapshots, epoch records, key
packages, and PSKs contain secrets, so production deployments must use encrypted
filesystem/device storage or add authenticated record encryption with external
key management.

`redb` is copy-on-write. Deleting a key package or other record does not guarantee
physical erasure of old database pages. Crypto-shredding requires destroying an
encryption key that is managed outside this provider.

On Unix, `RedbDataStorageEngine::open` creates the database with mode `0600` and
repairs broader permissions on reopen. `RedbDataStorageEngine::new` cannot do
this because an already-open `redb::Database` does not expose its path, so its
caller is responsible for securing the backing file.

After a database write or commit I/O error, discard every storage adapter and
engine clone and reopen the database. A redb commit error can require recovery;
the existing in-process database handle must not be reused.
