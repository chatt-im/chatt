//! Server-wide username uniqueness index.
//!
//! Every user on a server owns a unique username (compared case-insensitively).
//! This registry is the single authority for "who owns which username" across
//! the two user populations:
//!
//! - **Explicit users** live in `users.toml` (see [`crate::user_store::UserStore`]).
//!   Their usernames are only mirrored into the in-memory index here; the TOML
//!   registry stays their backing store.
//! - **Dynamic users** (open-paired public users, id `>= FIRST_DYNAMIC_USER_ID`)
//!   have no TOML record, so their chosen usernames are persisted in an
//!   append-only log `usernames.log.bin` under the storage data dir.
//!
//! The log frames each record as `user_id: u64 LE | len: u8 | username[len]`.
//! Records are append-only and the newest record for a given id wins on replay,
//! so a rename simply appends. Each ownership change is synced before it becomes
//! visible in memory. A torn trailing record after an unclean shutdown is dropped
//! and the log is truncated to the last whole record.

use hashbrown::HashMap;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

use rpc::ids::UserId;

use crate::config::{FIRST_DYNAMIC_USER_ID, UserConfig, valid_username};

const USERNAMES_LOG_FILE: &str = "usernames.log.bin";

/// Case-insensitive fold used for the uniqueness comparison. The original
/// casing is preserved in [`UsernameRegistry::by_user`]; only the lookup key is
/// folded.
fn fold(name: &str) -> String {
    name.trim().to_lowercase()
}

/// In-memory username -> owner index, backed on disk (for dynamic users only)
/// by the append-only `usernames.log.bin`.
#[derive(Debug)]
pub struct UsernameRegistry {
    /// Append handle + path for the dynamic-user log; `None` for in-memory
    /// registries (test configs with no data dir).
    log: Option<(PathBuf, File)>,
    /// Folded username -> the user id that owns it.
    by_folded: HashMap<String, UserId>,
    /// User id -> its current username (original casing).
    by_user: HashMap<UserId, String>,
}

impl UsernameRegistry {
    /// A registry with no persistence, seeded from `explicit_users`, for configs
    /// with no data dir.
    pub fn in_memory(explicit_users: &[UserConfig]) -> Self {
        let mut registry = Self {
            log: None,
            by_folded: HashMap::new(),
            by_user: HashMap::new(),
        };
        registry
            .seed_explicit(explicit_users)
            .expect("in-memory explicit usernames are valid and unique");
        registry
    }

    /// Opens the registry, seeding the index from the explicit `users.toml`
    /// records and then replaying `usernames.log.bin` over the dynamic users.
    ///
    /// # Errors
    ///
    /// Returns an error when the log exists but its append handle cannot be
    /// opened. A torn trailing record is not an error: it is dropped and the log
    /// truncated to the last whole record.
    pub fn open(data_dir: Option<PathBuf>, explicit_users: &[UserConfig]) -> Result<Self, String> {
        let Some(dir) = data_dir else {
            return Ok(Self::in_memory(explicit_users));
        };
        let path = dir.join(USERNAMES_LOG_FILE);
        let mut registry = Self {
            log: None,
            by_folded: HashMap::new(),
            by_user: HashMap::new(),
        };
        registry.seed_explicit(explicit_users)?;
        registry.replay_log(&path)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|err| format!("failed to open {}: {err}", path.display()))?;
        registry.log = Some((path, file));
        Ok(registry)
    }

    /// The user id currently owning `name`, if any.
    pub fn owner_of(&self, name: &str) -> Option<UserId> {
        self.by_folded.get(&fold(name)).copied()
    }

    /// Whether `user_id` already has a registered username.
    pub fn contains_user(&self, user_id: UserId) -> bool {
        self.by_user.contains_key(&user_id)
    }

    /// Whether `name` is free, or already owned by `claimant`.
    pub fn is_available(&self, name: &str, claimant: Option<UserId>) -> bool {
        match self.owner_of(name) {
            None => true,
            Some(owner) => Some(owner) == claimant,
        }
    }

    /// Records a dynamic user's username, appending to the log when it changed.
    ///
    /// # Errors
    ///
    /// Returns an error when `name` is owned by a different user, or the log
    /// append fails.
    pub fn claim_dynamic(&mut self, user_id: UserId, name: &str) -> Result<(), String> {
        if user_id.0 < FIRST_DYNAMIC_USER_ID {
            return Err(format!("user {user_id} is not a dynamic user"));
        }
        if !valid_username(name) {
            return Err("username must be 1-64 bytes with no control characters".to_string());
        }
        let name = name.trim();
        if !self.is_available(name, Some(user_id)) {
            return Err(format!("username '{name}' is already in use"));
        }
        // No-op when this id already holds exactly this username (same casing).
        if self.by_user.get(&user_id).map(String::as_str) == Some(name) {
            return Ok(());
        }
        if let Some((path, file)) = self.log.as_mut() {
            write_record(file, user_id, name)
                .map_err(|err| format!("failed to append {}: {err}", path.display()))?;
        }
        self.set_in_memory(user_id, name);
        Ok(())
    }

    /// Mirrors an explicit user's username into the index. Explicit users are
    /// persisted in `users.toml`, so this touches memory only.
    pub fn set_explicit(&mut self, user_id: UserId, name: &str) {
        debug_assert!(user_id.0 < FIRST_DYNAMIC_USER_ID);
        debug_assert!(valid_username(name));
        debug_assert!(self.is_available(name, Some(user_id)));
        self.set_in_memory(user_id, name);
    }

    fn seed_explicit(&mut self, explicit_users: &[UserConfig]) -> Result<(), String> {
        for user in explicit_users {
            self.claim_in_memory(user.id, &user.username)?;
        }
        Ok(())
    }

    /// Updates both maps for `user_id`, dropping the fold key of its previous
    /// username when that key still points at this id.
    fn set_in_memory(&mut self, user_id: UserId, name: &str) {
        if let Some(previous) = self.by_user.get(&user_id) {
            let previous_key = fold(previous);
            if previous_key != fold(name) && self.by_folded.get(&previous_key) == Some(&user_id) {
                self.by_folded.remove(&previous_key);
            }
        }
        self.by_folded.insert(fold(name), user_id);
        self.by_user.insert(user_id, name.to_string());
    }

    fn claim_in_memory(&mut self, user_id: UserId, name: &str) -> Result<(), String> {
        if !valid_username(name) {
            return Err(format!(
                "username registry contains invalid username for user {user_id}"
            ));
        }
        if !self.is_available(name, Some(user_id)) {
            return Err(format!(
                "username registry contains duplicate username '{name}'"
            ));
        }
        self.set_in_memory(user_id, name);
        Ok(())
    }

    /// Replays every whole record in `path`, overlaying dynamic-user names on the
    /// explicit seed. A torn trailing record is dropped and the log truncated.
    fn replay_log(&mut self, path: &PathBuf) -> Result<(), String> {
        let Ok(bytes) = fs::read(path) else {
            return Ok(());
        };
        // Collapse the dynamic log independently before merging it with the
        // current explicit-user snapshot. An explicit user may legitimately own
        // a name that a dynamic user held earlier in the log and later freed.
        let mut dynamic = Self {
            log: None,
            by_folded: HashMap::new(),
            by_user: HashMap::new(),
        };
        let mut offset = 0usize;
        while let Some((user_id, name, next)) = read_record(&bytes, offset) {
            if user_id.0 < FIRST_DYNAMIC_USER_ID {
                return Err(format!(
                    "{}: username log contains non-dynamic user id {user_id}",
                    path.display()
                ));
            }
            dynamic
                .claim_in_memory(user_id, name)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            offset = next;
        }
        if offset < bytes.len() {
            kvlog::warn!(
                "username log tail corrupt; truncating",
                path = path.display().to_string().as_str(),
                valid_bytes = offset,
                total_bytes = bytes.len()
            );
            if let Err(error) = truncate(path, offset as u64) {
                kvlog::error!(
                    "username log tail truncation failed",
                    path = path.display().to_string().as_str(),
                    error = error.to_string().as_str()
                );
            }
        }
        for (user_id, name) in dynamic.by_user {
            self.claim_in_memory(user_id, &name)
                .map_err(|error| format!("{}: {error}", path.display()))?;
        }
        Ok(())
    }
}

/// Appends one `user_id | len:u8 | username` record. Usernames are capped at 64
/// bytes upstream (`valid_username`); the `u8` length guard is a belt-and-braces
/// check so an oversized name never writes a frame the loader cannot read back.
fn write_record(file: &mut File, user_id: UserId, name: &str) -> std::io::Result<()> {
    let name = name.as_bytes();
    if name.len() > u8::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "username exceeds the log record cap",
        ));
    }
    let original_len = file.metadata()?.len();
    let mut record = Vec::with_capacity(9 + name.len());
    record.extend_from_slice(&user_id.0.to_le_bytes());
    record.push(name.len() as u8);
    record.extend_from_slice(name);
    if let Err(error) = file.write_all(&record).and_then(|()| file.sync_data()) {
        file.set_len(original_len)?;
        file.sync_data()?;
        return Err(error);
    }
    Ok(())
}

/// Reads the record starting at `offset`, returning it plus the offset past it,
/// or `None` when fewer than a whole record's bytes remain (torn tail) or the
/// username is not valid UTF-8.
fn read_record(bytes: &[u8], offset: usize) -> Option<(UserId, &str, usize)> {
    let id_end = offset.checked_add(9)?;
    let header = bytes.get(offset..id_end)?;
    let user_id = u64::from_le_bytes(header[..8].try_into().expect("8 bytes"));
    let len = header[8] as usize;
    let end = id_end.checked_add(len)?;
    let name = std::str::from_utf8(bytes.get(id_end..end)?).ok()?;
    Some((UserId(user_id), name, end))
}

fn truncate(path: &PathBuf, valid_bytes: u64) -> std::io::Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(valid_bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chatt-username-registry-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn explicit(id: u64, username: &str) -> UserConfig {
        UserConfig {
            id: UserId(id),
            internal_reference: format!("ref-{id}"),
            username: username.to_string(),
            token_hash: String::new(),
        }
    }

    #[test]
    fn dynamic_claims_persist_and_reload() {
        let dir = temp_data_dir("persist");
        let mut registry = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
        registry
            .claim_dynamic(UserId(5_000_000_000), "Alice")
            .unwrap();

        let reloaded = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(reloaded.owner_of("alice"), Some(UserId(5_000_000_000)));
    }

    #[test]
    fn rename_frees_the_old_name_and_last_write_wins() {
        let dir = temp_data_dir("rename");
        let id = UserId(5_000_000_001);
        let mut registry = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
        registry.claim_dynamic(id, "First").unwrap();
        registry.claim_dynamic(id, "Second").unwrap();

        let reloaded = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(reloaded.owner_of("second"), Some(id));
        assert_eq!(reloaded.owner_of("first"), None);
    }

    #[test]
    fn is_available_is_case_insensitive_and_owner_aware() {
        let mut registry = UsernameRegistry::in_memory(&[explicit(1, "Alice")]);
        assert!(!registry.is_available("alice", None));
        assert!(!registry.is_available("ALICE", Some(UserId(2))));
        assert!(registry.is_available("alice", Some(UserId(1))));
        assert!(registry.is_available("bob", None));

        // A dynamic user cannot steal an explicit user's name.
        assert!(
            registry
                .claim_dynamic(UserId(5_000_000_000), "alice")
                .is_err()
        );
    }

    #[test]
    fn seeds_from_explicit_users() {
        let registry = UsernameRegistry::in_memory(&[explicit(1, "Alice"), explicit(2, "Bob")]);
        assert_eq!(registry.owner_of("alice"), Some(UserId(1)));
        assert_eq!(registry.owner_of("bob"), Some(UserId(2)));
    }

    #[test]
    fn startup_rejects_collision_between_explicit_and_dynamic_users() {
        let dir = temp_data_dir("cross-store-collision");
        {
            let mut registry = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
            registry
                .claim_dynamic(UserId(FIRST_DYNAMIC_USER_ID), "Alice")
                .unwrap();
        }

        let error = UsernameRegistry::open(Some(dir.clone()), &[explicit(1, "alice")]).unwrap_err();
        let _ = fs::remove_dir_all(&dir);
        assert!(error.contains("duplicate username"), "{error}");
    }

    #[test]
    fn startup_allows_explicit_user_to_take_a_name_freed_by_dynamic_user() {
        let dir = temp_data_dir("cross-store-freed");
        let dynamic_id = UserId(FIRST_DYNAMIC_USER_ID);
        {
            let mut registry = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
            registry.claim_dynamic(dynamic_id, "Alice").unwrap();
            registry.claim_dynamic(dynamic_id, "Bob").unwrap();
        }

        let registry = UsernameRegistry::open(Some(dir.clone()), &[explicit(1, "alice")]).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(registry.owner_of("alice"), Some(UserId(1)));
        assert_eq!(registry.owner_of("bob"), Some(dynamic_id));
    }

    #[test]
    fn torn_tail_record_is_dropped_and_truncated() {
        let dir = temp_data_dir("torn");
        let id = UserId(5_000_000_009);
        {
            let mut registry = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
            registry.claim_dynamic(id, "Whole").unwrap();
        }
        let path = dir.join(USERNAMES_LOG_FILE);
        let good_len = fs::metadata(&path).unwrap().len();
        // Append a half-written record: a header claiming 8 bytes but only 3.
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&6_000_000_000u64.to_le_bytes()).unwrap();
            file.write_all(&[8]).unwrap();
            file.write_all(b"abc").unwrap();
        }

        let reloaded = UsernameRegistry::open(Some(dir.clone()), &[]).unwrap();
        let truncated_len = fs::metadata(&path).unwrap().len();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(reloaded.owner_of("whole"), Some(id));
        assert_eq!(
            truncated_len, good_len,
            "torn tail should be truncated away"
        );
    }
}
