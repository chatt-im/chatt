//! Registry of user DM identity public keys, persisted under the data dir.
//!
//! Keys are published by clients over the control channel and are public
//! material; the server never holds a private key. The registry is keyed by
//! [`UserId`] so explicit and dynamic users are covered uniformly — dynamic
//! users have no `users.toml` row, which is why this is its own file.

use hashbrown::HashSet;
use std::{fs, path::PathBuf};

use rpc::crypto::{decode_hex, encode_hex};
use rpc::e2e::E2E_PUBLIC_KEY_LEN;
use rpc::ids::UserId;
use toml_spanner::Toml;

use crate::config::atomic_write_toml;

const KEYS_FILE: &str = "e2e-keys.toml";

/// Durable registry of published DM identity keys.
///
/// A store without a path (test configs with no data dir) keeps the registry
/// in memory only.
#[derive(Debug)]
pub struct E2eKeyStore {
    path: Option<PathBuf>,
    keys: Vec<E2eKeyEntry>,
}

/// What [`E2eKeyStore::publish`] did with the presented key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPublish {
    Unchanged,
    Updated,
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct E2eKeyEntry {
    user_id: u64,
    public_key: String,
}

#[derive(Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct KeysFile {
    #[toml(default)]
    keys: Vec<E2eKeyEntry>,
}

impl E2eKeyStore {
    /// An empty registry that is never persisted, for configs with no data dir.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            keys: Vec::new(),
        }
    }

    /// Opens the registry at `<data_dir>/e2e-keys.toml`, starting empty when
    /// the file does not exist yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read, parsed, or
    /// validated. A corrupt registry refuses to load rather than starting
    /// fresh, because silently dropping keys would make every peer's pinned
    /// key look changed.
    pub fn open(data_dir: Option<PathBuf>) -> Result<Self, String> {
        let Some(dir) = data_dir else {
            return Ok(Self::in_memory());
        };
        let path = dir.join(KEYS_FILE);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: Some(path),
                    keys: Vec::new(),
                });
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let source = path.display().to_string();
        let arena = toml_spanner::Arena::new();
        let mut doc = toml_spanner::parse(&content, &arena)
            .map_err(|err| format!("failed to parse {source}: {err}"))?;
        let file: KeysFile = doc.to().map_err(|err| {
            let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
            format!("failed to deserialize {source}: {}", errors.join(", "))
        })?;
        let store = Self {
            path: Some(path),
            keys: file.keys,
        };
        store.validate(&source)?;
        Ok(store)
    }

    /// The published key for `user_id`, decoded to raw bytes.
    pub fn key_for(&self, user_id: UserId) -> Option<Vec<u8>> {
        let entry = self.keys.iter().find(|entry| entry.user_id == user_id.0)?;
        decode_hex(&entry.public_key).ok()
    }

    /// Records `public_key` as the user's identity key, persisting the
    /// registry when it is new or changed. Last publish wins: a reinstalled
    /// client must be able to re-key, and peers' pinned keys raise the alarm.
    ///
    /// # Errors
    ///
    /// Returns an error when the registry write fails; the in-memory registry
    /// is left unchanged then.
    pub fn publish(
        &mut self,
        user_id: UserId,
        public_key: &[u8; E2E_PUBLIC_KEY_LEN],
    ) -> Result<KeyPublish, String> {
        let public_key = encode_hex(public_key);
        let mut keys = self.keys.clone();
        match keys.iter_mut().find(|entry| entry.user_id == user_id.0) {
            Some(entry) if entry.public_key == public_key => return Ok(KeyPublish::Unchanged),
            Some(entry) => entry.public_key = public_key,
            None => keys.push(E2eKeyEntry {
                user_id: user_id.0,
                public_key,
            }),
        }
        self.save_state(&keys)?;
        self.keys = keys;
        Ok(KeyPublish::Updated)
    }

    fn save_state(&self, keys: &[E2eKeyEntry]) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        atomic_write_toml(path, &Self::snapshot(keys))
    }

    fn snapshot(keys: &[E2eKeyEntry]) -> String {
        let mut out = String::new();
        out.push_str(
            "# chatt server DM identity key registry. Managed by the server; do not edit.\n",
        );
        for entry in keys {
            out.push_str("\n[[keys]]\n");
            out.push_str(&format!("user-id = {}\n", entry.user_id));
            out.push_str(&format!("public-key = \"{}\"\n", entry.public_key));
        }
        out
    }

    fn validate(&self, source: &str) -> Result<(), String> {
        let mut user_ids = HashSet::new();
        for entry in &self.keys {
            if !user_ids.insert(entry.user_id) {
                return Err(format!(
                    "{source}: duplicate key for user {}",
                    entry.user_id
                ));
            }
            let decoded = decode_hex(&entry.public_key)
                .map_err(|_| format!("{source}: key for user {} is not hex", entry.user_id))?;
            if decoded.len() != E2E_PUBLIC_KEY_LEN {
                return Err(format!(
                    "{source}: key for user {} has the wrong length",
                    entry.user_id
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chatt-key-store-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn publish_persists_and_reloads() {
        let dir = temp_data_dir("publish");
        let mut store = E2eKeyStore::open(Some(dir.clone())).unwrap();

        assert_eq!(
            store
                .publish(UserId(7), &[3u8; E2E_PUBLIC_KEY_LEN])
                .unwrap(),
            KeyPublish::Updated
        );
        assert_eq!(
            store
                .publish(UserId(7), &[3u8; E2E_PUBLIC_KEY_LEN])
                .unwrap(),
            KeyPublish::Unchanged
        );
        assert_eq!(
            store
                .publish(UserId(7), &[4u8; E2E_PUBLIC_KEY_LEN])
                .unwrap(),
            KeyPublish::Updated
        );

        let reloaded = E2eKeyStore::open(Some(dir.clone())).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(reloaded.key_for(UserId(7)), Some(vec![4u8; 32]));
        assert_eq!(reloaded.key_for(UserId(8)), None);
    }

    #[test]
    fn open_rejects_corrupt_registry() {
        let dir = temp_data_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(KEYS_FILE), "this is not valid toml = [").unwrap();
        let error = E2eKeyStore::open(Some(dir.clone())).unwrap_err();
        let _ = fs::remove_dir_all(&dir);
        assert!(error.contains("failed to parse"));

        let dir = temp_data_dir("badkey");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(KEYS_FILE),
            "[[keys]]\nuser-id = 7\npublic-key = \"abcd\"\n",
        )
        .unwrap();
        let error = E2eKeyStore::open(Some(dir.clone())).unwrap_err();
        let _ = fs::remove_dir_all(&dir);
        assert!(error.contains("wrong length"), "{error}");
    }

    #[test]
    fn failed_save_leaves_the_in_memory_registry_unchanged() {
        let dir = temp_data_dir("save-failure");
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, "block").unwrap();
        let mut store = E2eKeyStore::in_memory();
        store
            .publish(UserId(7), &[3u8; E2E_PUBLIC_KEY_LEN])
            .unwrap();
        store.path = Some(blocker.join(KEYS_FILE));

        assert!(
            store
                .publish(UserId(7), &[4u8; E2E_PUBLIC_KEY_LEN])
                .is_err()
        );
        assert_eq!(store.key_for(UserId(7)), Some(vec![3u8; 32]));
        let _ = fs::remove_dir_all(&dir);
    }
}
