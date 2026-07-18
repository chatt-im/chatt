//! Server-managed user registry, persisted separately from the operator
//! config.
//!
//! The operator's config file is read-only at runtime. Every user record the
//! server mutates for invited users (pairing token hashes and usernames) lives
//! in `users.toml` under the storage data dir and is rewritten atomically on
//! each change.

use hashbrown::HashSet;
use std::{fs, path::PathBuf};

use rpc::ids::UserId;
use toml_spanner::Toml;

use crate::config::{
    FIRST_DYNAMIC_USER_ID, UserConfig, atomic_write_toml, toml_quote_value, valid_username,
    validate_secret_hash,
};

const USERS_FILE: &str = "users.toml";

/// Durable registry of invited/explicit paired users.
///
/// A store without a path (test configs with no data dir) keeps the registry
/// in memory only.
#[derive(Debug)]
pub struct UserStore {
    pub(crate) path: Option<PathBuf>,
    pub users: Vec<UserConfig>,
}

#[derive(Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct UsersFile {
    #[toml(default)]
    users: Vec<UserConfig>,
}

impl UserStore {
    /// An empty registry that is never persisted, for configs with no data dir.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            users: Vec::new(),
        }
    }

    /// Opens the registry at `<data_dir>/users.toml`, starting empty when the
    /// file does not exist yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be read, parsed, or
    /// validated. A corrupt registry refuses to load rather than starting
    /// fresh, because silently dropping records would lock every paired user
    /// out.
    pub fn open(data_dir: Option<PathBuf>) -> Result<Self, String> {
        let Some(dir) = data_dir else {
            return Ok(Self::in_memory());
        };
        let path = dir.join(USERS_FILE);
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: Some(path),
                    users: Vec::new(),
                });
            }
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let source = path.display().to_string();
        let arena = toml_spanner::Arena::new();
        let mut doc = toml_spanner::parse(&content, &arena)
            .map_err(|err| format!("failed to parse {source}: {err}"))?;
        let file: UsersFile = doc.to().map_err(|err| {
            let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
            format!("failed to deserialize {source}: {}", errors.join(", "))
        })?;
        let mut store = Self {
            path: Some(path),
            users: file.users,
        };
        store.normalize();
        store.validate(&source)?;
        Ok(store)
    }

    /// Records a completed pairing: sets the invited user's token hash and
    /// display name, creating the user record on its first pairing, and
    /// persists the registry.
    ///
    /// # Errors
    ///
    /// Returns an error when the explicit user-id range is exhausted or the
    /// registry write fails.
    pub fn mark_user_paired(
        &mut self,
        internal_reference: &str,
        username: String,
        token_hash: String,
    ) -> Result<UserConfig, String> {
        let (user, users) =
            self.prepare_mark_user_paired(internal_reference, username, token_hash)?;
        self.save_state(&users)?;
        self.users = users;
        Ok(user)
    }

    pub(crate) fn prepare_mark_user_paired(
        &self,
        internal_reference: &str,
        username: String,
        token_hash: String,
    ) -> Result<(UserConfig, Vec<UserConfig>), String> {
        if !valid_username(&username) {
            return Err("username must be 1-64 bytes with no control characters".to_string());
        }
        let username = username.trim().to_string();
        let mut users = self.users.clone();
        if let Some(user) = users
            .iter_mut()
            .find(|user| user.internal_reference == internal_reference)
        {
            user.username = username;
            user.token_hash = token_hash;
            let user = user.clone();
            return Ok((user, users));
        }

        let id = users
            .iter()
            .map(|user| user.id.0)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .map(UserId)
            .ok_or_else(|| "no user ids are available".to_string())?;
        if id.0 >= FIRST_DYNAMIC_USER_ID {
            return Err("no explicit user ids are available".to_string());
        }
        let user = UserConfig {
            id,
            internal_reference: internal_reference.to_string(),
            username,
            token_hash,
        };
        users.push(user.clone());
        Ok((user, users))
    }

    /// Updates a user's username and persists the registry.
    ///
    /// # Errors
    ///
    /// Returns an error when no user has `user_id` or the registry write fails.
    pub fn set_user_username(
        &mut self,
        user_id: UserId,
        username: String,
    ) -> Result<UserConfig, String> {
        let (user, users) = self.prepare_set_user_username(user_id, username)?;
        self.save_state(&users)?;
        self.users = users;
        Ok(user)
    }

    pub(crate) fn prepare_set_user_username(
        &self,
        user_id: UserId,
        username: String,
    ) -> Result<(UserConfig, Vec<UserConfig>), String> {
        if !valid_username(&username) {
            return Err("username must be 1-64 bytes with no control characters".to_string());
        }
        let username = username.trim().to_string();
        let mut users = self.users.clone();
        let Some(user) = users.iter_mut().find(|user| user.id == user_id) else {
            return Err(format!("no user with id {user_id}"));
        };
        user.username = username;
        let user = user.clone();
        Ok((user, users))
    }

    pub(crate) fn persistence_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    pub(crate) fn install_users(&mut self, users: Vec<UserConfig>) {
        self.users = users;
    }

    fn save_state(&self, users: &[UserConfig]) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        atomic_write_toml(path, &Self::snapshot(users))
    }

    pub(crate) fn snapshot(users: &[UserConfig]) -> String {
        let mut out = String::new();
        out.push_str("# chatt server user registry. Managed by the server; do not edit.\n\n");
        for user in users {
            out.push_str("\n[[users]]\n");
            out.push_str(&format!("id = {}\n", user.id.0));
            out.push_str(&format!(
                "name = \"{}\"\n",
                toml_quote_value(&user.internal_reference)
            ));
            out.push_str(&format!(
                "display-name = \"{}\"\n",
                toml_quote_value(&user.username)
            ));
            out.push_str(&format!(
                "token-hash = \"{}\"\n",
                toml_quote_value(&user.token_hash)
            ));
        }
        out
    }

    fn normalize(&mut self) {
        for user in &mut self.users {
            user.internal_reference = user.internal_reference.trim().to_string();
            user.username = user.username.trim().to_string();
            if user.username.is_empty() {
                user.username = user.internal_reference.clone();
            }
        }
    }

    fn validate(&self, source: &str) -> Result<(), String> {
        let mut user_ids = HashSet::new();
        let mut internal_reference_names = HashSet::new();
        let mut usernames = HashSet::new();
        for user in &self.users {
            if user.id == UserId(0) {
                return Err(format!("{source}: user id must be non-zero"));
            }
            if user.id.0 >= FIRST_DYNAMIC_USER_ID {
                return Err(format!(
                    "{source}: user {} id must be below {FIRST_DYNAMIC_USER_ID}; higher ids are reserved for dynamic users",
                    user.internal_reference
                ));
            }
            if user.internal_reference.is_empty() {
                return Err(format!("{source}: user name must not be empty"));
            }
            if !valid_username(&user.username) {
                return Err(format!(
                    "{source}: user {} username must be 1-64 bytes with no control characters",
                    user.internal_reference
                ));
            }
            if !user_ids.insert(user.id) {
                return Err(format!("{source}: duplicate user id {}", user.id));
            }
            if !internal_reference_names.insert(user.internal_reference.as_str()) {
                return Err(format!(
                    "{source}: duplicate user name {}",
                    user.internal_reference
                ));
            }
            // Usernames must be unique server-wide, compared case-insensitively,
            // to match the runtime uniqueness the registry enforces.
            if !user.username.is_empty() && !usernames.insert(user.username.trim().to_lowercase()) {
                return Err(format!("{source}: duplicate username {}", user.username));
            }
            if !user.token_hash.trim().is_empty() {
                validate_secret_hash(
                    source,
                    &format!("user {} token-hash", user.internal_reference),
                    &user.token_hash,
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::hash_secret;

    fn temp_data_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chatt-user-store-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn store_with_users() -> UserStore {
        let mut store = UserStore::in_memory();
        store.users = vec![
            UserConfig {
                id: UserId(1),
                internal_reference: "alice".to_string(),
                username: "Alice".to_string(),
                token_hash: hash_secret("alice-client-generated-token-with-at-least-32-bytes"),
            },
            UserConfig {
                id: UserId(2),
                internal_reference: "bob".to_string(),
                username: "Bob".to_string(),
                token_hash: hash_secret("bob-client-generated-token-with-at-least-32-bytes"),
            },
        ];
        store
    }

    #[test]
    fn mark_user_paired_persists_and_reloads() {
        let dir = temp_data_dir("pair");
        let mut store = UserStore::open(Some(dir.clone())).unwrap();

        let token_hash = hash_secret("client-generated-token-with-at-least-32-bytes");
        let user = store
            .mark_user_paired("alice", "Alice Example".to_string(), token_hash.clone())
            .unwrap();
        assert_eq!(user.id, UserId(1));
        assert_eq!(user.username, "Alice Example");
        assert_eq!(user.token_hash, token_hash);

        let reloaded = UserStore::open(Some(dir.clone())).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(reloaded.users.len(), 1);
        assert_eq!(reloaded.users[0].internal_reference, "alice");
        assert_eq!(reloaded.users[0].username, "Alice Example");
        assert_eq!(reloaded.users[0].token_hash, token_hash);
    }

    #[test]
    fn control_character_username_is_rejected() {
        let dir = temp_data_dir("control");
        let mut store = UserStore::open(Some(dir.clone())).unwrap();

        let username = "x\u{1}y\u{7f}z";
        let internal_reference = "dana\u{2}";
        let error = store
            .mark_user_paired(
                internal_reference,
                username.to_string(),
                hash_secret("dana-client-generated-token-with-at-least-32-bytes"),
            )
            .unwrap_err();
        let _ = fs::remove_dir_all(&dir);
        assert!(error.contains("control characters"), "{error}");
        assert!(store.users.is_empty());
    }

    #[test]
    fn mark_user_paired_updates_the_existing_record_in_place() {
        let mut store = store_with_users();

        let token_hash = hash_secret("fresh-client-generated-token-with-at-least-32-bytes");
        let user = store
            .mark_user_paired("alice", "Alice Two".to_string(), token_hash.clone())
            .unwrap();

        assert_eq!(user.id, UserId(1));
        assert_eq!(store.users.len(), 2);
        assert_eq!(store.users[0].username, "Alice Two");
        assert_eq!(store.users[0].token_hash, token_hash);
    }

    #[test]
    fn mark_user_paired_creates_new_user_with_next_id() {
        let mut store = store_with_users();

        let user = store
            .mark_user_paired(
                "carol",
                "Carol".to_string(),
                hash_secret("carol-client-generated-token-with-at-least-32-bytes"),
            )
            .unwrap();

        assert_eq!(user.id, UserId(3));
        assert_eq!(user.internal_reference, "carol");
        assert_eq!(store.users.len(), 3);
    }

    #[test]
    fn mark_user_paired_rejects_exhausted_explicit_id_range() {
        let mut store = UserStore::in_memory();
        store.users = vec![UserConfig {
            id: UserId(FIRST_DYNAMIC_USER_ID - 1),
            internal_reference: "last".to_string(),
            username: "Last".to_string(),
            token_hash: hash_secret("last-client-generated-token-with-at-least-32-bytes"),
        }];

        let error = store
            .mark_user_paired(
                "next",
                "Next".to_string(),
                hash_secret("next-client-generated-token-with-at-least-32-bytes"),
            )
            .unwrap_err();

        assert!(error.contains("explicit user ids"));
    }

    #[test]
    fn set_user_username_updates_and_persists() {
        let dir = temp_data_dir("rename");
        let mut store = UserStore::open(Some(dir.clone())).unwrap();
        let token_hash = hash_secret("alice-client-generated-token-with-at-least-32-bytes");
        let alice = store
            .mark_user_paired("alice", "Alice".to_string(), token_hash.clone())
            .unwrap();

        let user = store
            .set_user_username(alice.id, "Alice Renamed".to_string())
            .unwrap();
        assert_eq!(user.username, "Alice Renamed");
        assert_eq!(user.token_hash, token_hash);

        let reloaded = UserStore::open(Some(dir.clone())).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(reloaded.users[0].username, "Alice Renamed");
    }

    #[test]
    fn set_user_username_rejects_unknown_user() {
        let mut store = UserStore::in_memory();
        assert!(
            store
                .set_user_username(UserId(999), "Ghost".to_string())
                .is_err()
        );
    }

    #[test]
    fn failed_username_save_leaves_the_in_memory_user_unchanged() {
        let dir = temp_data_dir("rename-failure");
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, "block").unwrap();
        let mut store = store_with_users();
        store.path = Some(blocker.join(USERS_FILE));

        assert!(
            store
                .set_user_username(UserId(1), "Renamed".to_string())
                .is_err()
        );
        assert_eq!(store.users[0].username, "Alice");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_pairing_save_leaves_the_in_memory_user_unchanged() {
        let dir = temp_data_dir("pair-failure");
        fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-directory");
        fs::write(&blocker, "block").unwrap();
        let mut store = store_with_users();
        let old_hash = store.users[0].token_hash.clone();
        store.path = Some(blocker.join(USERS_FILE));

        assert!(
            store
                .mark_user_paired(
                    "alice",
                    "Renamed".to_string(),
                    hash_secret("replacement-client-generated-token-with-at-least-32-bytes"),
                )
                .is_err()
        );
        assert_eq!(store.users[0].username, "Alice");
        assert_eq!(store.users[0].token_hash, old_hash);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_rejects_corrupt_registry() {
        let dir = temp_data_dir("corrupt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(USERS_FILE), "this is not valid toml = [").unwrap();

        let error = UserStore::open(Some(dir.clone())).unwrap_err();
        let _ = fs::remove_dir_all(&dir);

        assert!(error.contains("failed to parse"));
    }

    #[test]
    fn open_rejects_explicit_user_id_in_dynamic_range() {
        let dir = temp_data_dir("id-range");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(USERS_FILE),
            format!("[[users]]\nid = {FIRST_DYNAMIC_USER_ID}\nname = \"alice\"\n"),
        )
        .unwrap();

        let error = UserStore::open(Some(dir.clone())).unwrap_err();
        let _ = fs::remove_dir_all(&dir);

        assert!(error.contains("reserved for dynamic users"));
    }

    #[test]
    fn open_rejects_duplicate_user_names() {
        let dir = temp_data_dir("dup-name");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(USERS_FILE),
            "[[users]]\nid = 1\nname = \"alice\"\n\n[[users]]\nid = 2\nname = \"alice\"\n",
        )
        .unwrap();

        let error = UserStore::open(Some(dir.clone())).unwrap_err();
        let _ = fs::remove_dir_all(&dir);

        assert!(error.contains("duplicate user name"));
    }

    #[test]
    fn open_rejects_duplicate_usernames_case_insensitively() {
        let dir = temp_data_dir("dup-username");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(USERS_FILE),
            "[[users]]\nid = 1\nname = \"a\"\ndisplay-name = \"Alice\"\n\n\
             [[users]]\nid = 2\nname = \"b\"\ndisplay-name = \"alice\"\n",
        )
        .unwrap();

        let error = UserStore::open(Some(dir.clone())).unwrap_err();
        let _ = fs::remove_dir_all(&dir);

        assert!(error.contains("duplicate username"), "{error}");
    }

    #[test]
    fn save_leaves_no_temp_residue() {
        let dir = temp_data_dir("residue");
        let mut store = UserStore::open(Some(dir.clone())).unwrap();
        store
            .mark_user_paired(
                "alice",
                "Alice".to_string(),
                hash_secret("alice-client-generated-token-with-at-least-32-bytes"),
            )
            .unwrap();

        let residue: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != USERS_FILE)
            .collect();
        let _ = fs::remove_dir_all(&dir);

        assert!(residue.is_empty(), "unexpected files: {residue:?}");
    }
}
