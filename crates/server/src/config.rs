use hashbrown::HashSet;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use ring::{digest, rand::SecureRandom, signature::KeyPair};
use rpc::{
    control::{DEFAULT_FILE_SIZE_LIMIT_BYTES, MAX_AUTH_FIELD_BYTES},
    crypto::{encode_hex, server_key_pair_from_seed_hex},
    ids::{RoomId, UserId},
};
use toml_spanner::{Item, Toml};

const SECRET_HASH_PREFIX: &str = "sha256:";
const SHA256_HEX_LEN: usize = 64;
/// First user id handed out to a dynamic (open-paired) user. Ids below this are
/// reserved for explicit `[[users]]` entries.
pub const FIRST_DYNAMIC_USER_ID: u64 = u32::MAX as u64 + 1;
/// First room id handed out to a runtime-created (DM) room. Ids below this are
/// reserved for explicit `[[rooms]]` entries.
pub const FIRST_DYNAMIC_ROOM_ID: u32 = 0x8000_0000;
/// Ring size used for `persistence = "memory"` rooms without an explicit
/// `memory-limit`.
pub const DEFAULT_MEMORY_HISTORY_LIMIT: usize = 512;

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct NetworkConfig {
    #[toml(FromToml with = toml_spanner::helper::parse_string)]
    pub tcp_addr: SocketAddr,
    #[toml(default = default_shared_addr(), FromToml with = toml_spanner::helper::parse_string)]
    pub udp_addr: SocketAddr,
    #[toml(FromToml with = toml_spanner::helper::parse_string)]
    pub udp_probe_addr: Option<SocketAddr>,
    #[toml(default)]
    pub public_tcp_addr: String,
    #[toml(default)]
    pub public_udp_addr: String,
    #[toml(default)]
    pub public_udp_probe_addr: Option<String>,
    #[toml(default = true)]
    pub p2p_enabled: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            tcp_addr: "127.0.0.1:41000".parse().expect("valid default TCP addr"),
            udp_addr: default_shared_addr(),
            udp_probe_addr: None,
            public_tcp_addr: String::new(),
            public_udp_addr: String::new(),
            public_udp_probe_addr: None,
            p2p_enabled: true,
        }
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct SecurityConfig {
    pub server_identity_seed: String,
    #[toml(default = true)]
    pub encryption: bool,
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_BYTES)]
    pub max_file_size_bytes: u64,
    /// Directory where `/report-bug` bundles are saved. Bug reports are rejected
    /// when unset.
    #[toml(default)]
    pub bug_report_dir: Option<String>,
    /// Whether users may self-join via `chatt pair <addr>` without an admin invite.
    #[toml(default)]
    pub public: bool,
    /// Shared secret required for open pairing. `None`/empty means no password.
    #[toml(default)]
    pub password: Option<String>,
    /// Current password epoch. Dynamic tokens embed the epoch they were issued
    /// under. Bumping this invalidates existing tokens.
    #[toml(default)]
    pub password_epoch: u32,
    /// Next id to hand out to a dynamic user. Persisted so ids never repeat.
    #[toml(default = FIRST_DYNAMIC_USER_ID)]
    pub next_dynamic_user_id: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            server_identity_seed: generate_identity_seed_hex()
                .expect("system random is available for default test config"),
            encryption: true,
            max_file_size_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
            bug_report_dir: None,
            public: false,
            password: None,
            password_epoch: 0,
            next_dynamic_user_id: FIRST_DYNAMIC_USER_ID,
        }
    }
}

/// Server-side retention for a room's messages.
///
/// `None` relays without retaining, `Memory` keeps a bounded in-memory ring
/// that is lost on restart, and `Durable` appends to an on-disk log under the
/// storage data dir.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum RoomPersistenceConfig {
    #[default]
    None,
    Memory,
    Durable,
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct RoomConfig {
    pub id: u32,
    pub name: String,
    /// `None` means public: every user on the server can access the room.
    /// `Some` restricts access to the listed `[[users]]` names.
    #[toml(default)]
    pub members: Option<Vec<String>>,
    #[toml(default)]
    pub persistence: RoomPersistenceConfig,
    /// Ring size for `persistence = "memory"`; rejected for other settings.
    #[toml(default)]
    pub memory_limit: Option<u64>,
    /// Marks the room clients drop into on connect. At most one room; when
    /// absent the lowest-id public room is the default.
    #[toml(default, rename = "default")]
    pub is_default: bool,
}

impl RoomConfig {
    pub fn room_id(&self) -> RoomId {
        RoomId(self.id)
    }

    pub fn is_public(&self) -> bool {
        self.members.is_none()
    }

    /// In-memory ring size backing this room until durable storage takes over:
    /// the configured `memory-limit` (or the default) for memory rooms, zero
    /// otherwise.
    pub fn memory_history_limit(&self) -> usize {
        if self.persistence != RoomPersistenceConfig::Memory {
            return 0;
        }
        let Some(limit) = self.memory_limit else {
            return DEFAULT_MEMORY_HISTORY_LIMIT;
        };
        usize::try_from(limit).unwrap_or(usize::MAX)
    }
}

#[derive(Clone, Debug, Default, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct StorageConfig {
    /// Directory for server-side room data (durable logs, DM registry).
    /// Defaults to `<config stem>-data` beside the config file.
    #[toml(default)]
    pub data_dir: Option<String>,
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct UserConfig {
    pub id: UserId,
    pub name: String,
    #[toml(default)]
    pub display_name: String,
    #[toml(default)]
    pub token_hash: String,
}

impl UserConfig {
    pub fn user_id(&self) -> UserId {
        self.id
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, recoverable, warn_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[toml(default)]
    pub network: NetworkConfig,
    #[toml(default)]
    pub security: SecurityConfig,
    #[toml(default)]
    pub storage: StorageConfig,
    #[toml(default = default_rooms())]
    pub rooms: Vec<RoomConfig>,
    #[toml(default)]
    pub users: Vec<UserConfig>,
    #[toml(skip)]
    pub config_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let mut config = Self {
            network: NetworkConfig::default(),
            security: SecurityConfig::default(),
            storage: StorageConfig::default(),
            rooms: default_rooms(),
            users: Vec::new(),
            config_path: None,
        };
        config.apply_inferred_addresses(false, false);
        config.normalize();
        config
    }
}

pub fn write_generated_template(path: &Path) -> Result<(), String> {
    let content = generated_template_config()?;
    parse_config_content(
        &content,
        &path.display().to_string(),
        Some(path.to_path_buf()),
    )?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let mut file = open_new_config_file(path)?;
    file.write_all(content.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to sync {}: {err}", path.display()))?;
    drop(file);
    sync_parent_dir(path);
    Ok(())
}

pub fn generated_template_config() -> Result<String, String> {
    let seed = generate_identity_seed_hex()?;
    Ok(format!(
        r#"# chatt server configuration
#
# Generated by `chatt-server init-config`. Keep this file private: it contains
# the server identity seed that authenticates handshakes and dynamic tokens.

[network]
# Bind addresses on this host.
tcp-addr = "127.0.0.1:41000"
# udp-addr defaults to tcp-addr when omitted.
# udp-addr = "127.0.0.1:41000"
# Optional UDP socket used for P2P path probes.
# udp-probe-addr = "127.0.0.1:41001"

# Public endpoints embedded in invites and returned during open pairing. Set
# these when clients need a DNS name, reverse proxy port, or forwarded NAT port.
# public-tcp-addr = "chat.example.com:41000"
# public-udp-addr = "chat.example.com:41000"
# public-udp-probe-addr = "chat.example.com:41001"
p2p-enabled = true

[security]
server-identity-seed = "{seed}"
encryption = true
max-file-size-bytes = {max_file_size_bytes}
# Directory where `/report-bug` bundles are saved. Bug reports are rejected when
# unset.
# bug-report-dir = "/tmp/chatt-bugs"

# Public mode lets users self-join with `chatt pair <host:port>`.
public = false
# Set a shared secret to gate public open pairing. Omit or leave empty for no
# password.
# password = "change-me"
# Bump this to invalidate existing dynamic tokens.
password-epoch = 0
# Dynamic users are issued ids from this counter. Explicit users must stay below
# this range.
next-dynamic-user-id = {first_dynamic_user_id}

# Server-side room data (durable message logs, the DM room registry) lives in
# storage.data-dir. Defaults to "<config stem>-data" beside this file.
# [storage]
# data-dir = "chatt-server-data"

[[rooms]]
id = 1
name = "lobby"
# Rooms are public unless they list members; members are [[users]] names.
# members = ["alice", "bob"]
# Server-side retention: "none" (relay only), "memory" (ring, lost on
# restart), or "durable" (on-disk log under storage.data-dir).
persistence = "none"
# Ring size for persistence = "memory".
# memory-limit = 512
# Clients drop into the default room on connect. At most one room; when
# omitted the lowest-id public room is the default.
default = true

# Create invite users with:
#   chatt-server invite USER
# The server writes [[users]] entries as invites are accepted.
"#,
        seed = seed,
        max_file_size_bytes = DEFAULT_FILE_SIZE_LIMIT_BYTES,
        first_dynamic_user_id = FIRST_DYNAMIC_USER_ID,
    ))
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let source = path.display().to_string();
        parse_config_content(&content, &source, Some(path.to_path_buf()))
    }

    pub fn server_key_pair(&self) -> Result<ring::signature::Ed25519KeyPair, String> {
        server_key_pair_from_seed_hex(&self.security.server_identity_seed)
            .map_err(|error| format!("invalid security.server-identity-seed: {error}"))
    }

    pub fn server_public_key_hex(&self) -> Result<String, String> {
        let key_pair = self.server_key_pair()?;
        Ok(encode_hex(key_pair.public_key().as_ref()))
    }

    pub fn mark_user_paired(
        &mut self,
        user_name: &str,
        display_name: String,
        token_hash: String,
    ) -> Result<UserConfig, String> {
        if let Some(index) = self.users.iter().position(|user| user.name == user_name) {
            let old_token_hash = self.users[index].token_hash.clone();
            let old_display_name = self.users[index].display_name.clone();
            self.users[index].display_name = display_name;
            self.users[index].token_hash = token_hash;
            if let Err(error) = self.save_runtime() {
                self.users[index].token_hash = old_token_hash;
                self.users[index].display_name = old_display_name;
                return Err(error);
            }
            return Ok(self.users[index].clone());
        }

        let id = self
            .users
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
            name: user_name.to_string(),
            display_name,
            token_hash,
        };
        self.users.push(user);
        if let Err(error) = self.save_runtime() {
            self.users.pop();
            return Err(error);
        }
        Ok(self.users.last().expect("user was just inserted").clone())
    }

    pub fn is_public(&self) -> bool {
        self.security.public
    }

    pub fn password(&self) -> Option<&str> {
        self.security
            .password
            .as_deref()
            .filter(|password| !password.is_empty())
    }

    pub fn password_epoch(&self) -> u32 {
        self.security.password_epoch
    }

    /// The room clients drop into on connect: the room marked `default = true`,
    /// or the lowest-id public room.
    pub fn default_room_id(&self) -> RoomId {
        if let Some(room) = self.rooms.iter().find(|room| room.is_default) {
            return room.room_id();
        }
        self.rooms
            .iter()
            .filter(|room| room.is_public())
            .map(|room| room.id)
            .min()
            .map(RoomId)
            .unwrap_or(RoomId(1))
    }

    /// Directory for server-side room data: `storage.data-dir` when set,
    /// otherwise `<config stem>-data` beside the config file. `None` only for
    /// in-memory test configs with no config path.
    pub fn data_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.storage.data_dir {
            return Some(PathBuf::from(dir));
        }
        let path = self.config_path.as_ref()?;
        let stem = path.file_stem()?.to_string_lossy();
        Some(path.with_file_name(format!("{stem}-data")))
    }

    /// Reserves the next dynamic user id, persisting the advanced counter so ids
    /// never repeat across restarts. Rolls back the in-memory counter if the
    /// config write fails.
    pub fn allocate_dynamic_user_id(&mut self) -> Result<UserId, String> {
        let id = self.security.next_dynamic_user_id;
        if id < FIRST_DYNAMIC_USER_ID {
            return Err(format!(
                "next dynamic user id must be at least {FIRST_DYNAMIC_USER_ID}"
            ));
        }
        let next = id
            .checked_add(1)
            .ok_or_else(|| "no dynamic user ids are available".to_string())?;
        self.security.next_dynamic_user_id = next;
        if let Err(error) = self.save_runtime() {
            self.security.next_dynamic_user_id = id;
            return Err(error);
        }
        Ok(UserId(id))
    }

    pub fn set_user_display_name(
        &mut self,
        user_name: &str,
        display_name: String,
    ) -> Result<UserConfig, String> {
        let Some(index) = self.users.iter().position(|user| user.name == user_name) else {
            return Err(format!("no user named '{user_name}'"));
        };
        let old_display_name = self.users[index].display_name.clone();
        self.users[index].display_name = display_name;
        if let Err(error) = self.save_runtime() {
            self.users[index].display_name = old_display_name;
            return Err(error);
        }
        Ok(self.users[index].clone())
    }

    fn apply_inferred_addresses(&mut self, udp_addr_configured: bool, udp_addr_overridden: bool) {
        if !udp_addr_configured && !udp_addr_overridden {
            self.network.udp_addr = self.network.tcp_addr;
        }
    }

    fn normalize(&mut self) {
        self.network.public_tcp_addr = self.network.public_tcp_addr.trim().to_string();
        if self.network.public_tcp_addr.is_empty() {
            self.network.public_tcp_addr = self.network.tcp_addr.to_string();
        }
        self.network.public_udp_addr = self.network.public_udp_addr.trim().to_string();
        if self.network.public_udp_addr.is_empty() {
            self.network.public_udp_addr = self.network.udp_addr.to_string();
        }
        self.network.public_udp_probe_addr = self
            .network
            .public_udp_probe_addr
            .as_deref()
            .map(str::trim)
            .filter(|addr| !addr.is_empty())
            .map(str::to_string)
            .or_else(|| self.network.udp_probe_addr.map(|addr| addr.to_string()));
        for room in &mut self.rooms {
            room.name = room.name.trim().to_string();
            if let Some(members) = &mut room.members {
                for member in members {
                    *member = member.trim().to_string();
                }
            }
        }
        for user in &mut self.users {
            user.name = user.name.trim().to_string();
            user.display_name = user.display_name.trim().to_string();
            if user.display_name.is_empty() {
                user.display_name = user.name.clone();
            }
        }
    }

    fn validate(&self, source: &str) -> Result<(), String> {
        if self.rooms.is_empty() {
            return Err(format!("{source}: at least one room is required"));
        }
        server_key_pair_from_seed_hex(&self.security.server_identity_seed)
            .map_err(|error| format!("{source}: invalid security.server-identity-seed: {error}"))?;
        if let Some(password) = &self.security.password
            && password.len() > MAX_AUTH_FIELD_BYTES
        {
            return Err(format!(
                "{source}: security.password exceeds {MAX_AUTH_FIELD_BYTES} bytes"
            ));
        }
        validate_endpoint(
            source,
            "network.public-tcp-addr",
            &self.network.public_tcp_addr,
        )?;
        validate_endpoint(
            source,
            "network.public-udp-addr",
            &self.network.public_udp_addr,
        )?;
        if let Some(addr) = &self.network.public_udp_probe_addr {
            validate_endpoint(source, "network.public-udp-probe-addr", addr)?;
        }
        if self.security.next_dynamic_user_id < FIRST_DYNAMIC_USER_ID {
            return Err(format!(
                "{source}: security.next-dynamic-user-id must be at least {FIRST_DYNAMIC_USER_ID}"
            ));
        }

        let mut user_ids = HashSet::new();
        let mut user_names = HashSet::new();
        for user in &self.users {
            if user.id == UserId(0) {
                return Err(format!("{source}: user id must be non-zero"));
            }
            if user.id.0 >= FIRST_DYNAMIC_USER_ID {
                return Err(format!(
                    "{source}: user {} id must be below {FIRST_DYNAMIC_USER_ID}; higher ids are reserved for dynamic users",
                    user.name
                ));
            }
            if user.name.trim().is_empty() {
                return Err(format!("{source}: user name must not be empty"));
            }
            if user.display_name.trim().is_empty() {
                return Err(format!(
                    "{source}: user {} display-name must not be empty",
                    user.name
                ));
            }
            if user.display_name.len() > 64 {
                return Err(format!(
                    "{source}: user {} display-name exceeds 64 bytes",
                    user.name
                ));
            }
            if !user_ids.insert(user.id) {
                return Err(format!("{source}: duplicate user id {}", user.id));
            }
            if !user_names.insert(user.name.as_str()) {
                return Err(format!("{source}: duplicate user name {}", user.name));
            }
            validate_optional_secret_hash(
                source,
                &format!("user {} token-hash", user.name),
                &user.token_hash,
            )?;
        }

        let mut room_ids = HashSet::new();
        let mut room_names = HashSet::new();
        let mut default_room = None;
        let mut has_public_room = false;
        for room in &self.rooms {
            if room.id == 0 {
                return Err(format!("{source}: room id must be non-zero"));
            }
            if room.id >= FIRST_DYNAMIC_ROOM_ID {
                return Err(format!(
                    "{source}: room {} id must be below {FIRST_DYNAMIC_ROOM_ID}; higher ids are reserved for runtime-created rooms",
                    room.name
                ));
            }
            if room.name.is_empty() {
                return Err(format!("{source}: room name must not be empty"));
            }
            if !room_ids.insert(room.id) {
                return Err(format!("{source}: duplicate room id {}", room.id));
            }
            if !room_names.insert(room.name.as_str()) {
                return Err(format!("{source}: duplicate room name {}", room.name));
            }
            match &room.members {
                None => has_public_room = true,
                Some(members) => {
                    if members.is_empty() {
                        return Err(format!(
                            "{source}: private room {} must list at least one member",
                            room.name
                        ));
                    }
                    for member in members {
                        if !user_names.contains(member.as_str()) {
                            return Err(format!(
                                "{source}: room {} member {member} does not match any [[users]] name",
                                room.name
                            ));
                        }
                    }
                }
            }
            if room.memory_limit.is_some() && room.persistence != RoomPersistenceConfig::Memory {
                return Err(format!(
                    "{source}: room {} memory-limit requires persistence = \"memory\"",
                    room.name
                ));
            }
            if room.memory_limit == Some(0) {
                return Err(format!(
                    "{source}: room {} memory-limit must be non-zero",
                    room.name
                ));
            }
            if room.is_default {
                if let Some(previous) = default_room {
                    return Err(format!(
                        "{source}: rooms {previous} and {} both set default = true",
                        room.name
                    ));
                }
                if !room.is_public() {
                    return Err(format!(
                        "{source}: default room {} must be public",
                        room.name
                    ));
                }
                default_room = Some(room.name.as_str());
            }
        }
        if !has_public_room {
            return Err(format!("{source}: at least one public room is required"));
        }
        Ok(())
    }

    fn save_runtime(&self) -> Result<(), String> {
        let path = self
            .config_path
            .as_ref()
            .ok_or_else(|| "pairing requires a writable server config path".to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        atomic_write_config(path, &self.to_toml_string())
    }

    fn to_toml_string(&self) -> String {
        let mut out = String::new();
        out.push_str("# chatt server configuration\n\n");
        out.push_str("[network]\n");
        out.push_str(&format!("tcp-addr = \"{}\"\n", self.network.tcp_addr));
        if self.network.udp_addr != self.network.tcp_addr {
            out.push_str(&format!("udp-addr = \"{}\"\n", self.network.udp_addr));
        }
        if let Some(udp_probe_addr) = self.network.udp_probe_addr {
            out.push_str(&format!("udp-probe-addr = \"{}\"\n", udp_probe_addr));
        }
        if self.network.public_tcp_addr != self.network.tcp_addr.to_string() {
            out.push_str(&format!(
                "public-tcp-addr = \"{}\"\n",
                toml_quote_value(&self.network.public_tcp_addr)
            ));
        }
        if self.network.public_udp_addr != self.network.udp_addr.to_string() {
            out.push_str(&format!(
                "public-udp-addr = \"{}\"\n",
                toml_quote_value(&self.network.public_udp_addr)
            ));
        }
        if self.network.public_udp_probe_addr
            != self.network.udp_probe_addr.map(|addr| addr.to_string())
            && let Some(addr) = &self.network.public_udp_probe_addr
        {
            out.push_str(&format!(
                "public-udp-probe-addr = \"{}\"\n",
                toml_quote_value(addr)
            ));
        }
        out.push_str(&format!("p2p-enabled = {}\n\n", self.network.p2p_enabled));
        out.push_str("[security]\n");
        out.push_str(&format!(
            "server-identity-seed = \"{}\"\n",
            toml_quote_value(&self.security.server_identity_seed)
        ));
        out.push_str(&format!("encryption = {}\n", self.security.encryption));
        out.push_str(&format!(
            "max-file-size-bytes = {}\n",
            self.security.max_file_size_bytes
        ));
        if let Some(dir) = &self.security.bug_report_dir {
            out.push_str(&format!("bug-report-dir = \"{}\"\n", toml_quote_value(dir)));
        }
        out.push_str(&format!("public = {}\n", self.security.public));
        if let Some(password) = &self.security.password {
            out.push_str(&format!("password = \"{}\"\n", toml_quote_value(password)));
        }
        out.push_str(&format!(
            "password-epoch = {}\n",
            self.security.password_epoch
        ));
        out.push_str(&format!(
            "next-dynamic-user-id = {}\n",
            self.security.next_dynamic_user_id
        ));
        out.push('\n');
        if let Some(data_dir) = &self.storage.data_dir {
            out.push_str("[storage]\n");
            out.push_str(&format!(
                "data-dir = \"{}\"\n\n",
                toml_quote_value(data_dir)
            ));
        }
        for room in &self.rooms {
            out.push_str("[[rooms]]\n");
            out.push_str(&format!("id = {}\n", room.id));
            out.push_str(&format!("name = \"{}\"\n", toml_quote_value(&room.name)));
            if let Some(members) = &room.members {
                let members: Vec<String> = members
                    .iter()
                    .map(|member| format!("\"{}\"", toml_quote_value(member)))
                    .collect();
                out.push_str(&format!("members = [{}]\n", members.join(", ")));
            }
            let persistence = match room.persistence {
                RoomPersistenceConfig::None => None,
                RoomPersistenceConfig::Memory => Some("memory"),
                RoomPersistenceConfig::Durable => Some("durable"),
            };
            if let Some(persistence) = persistence {
                out.push_str(&format!("persistence = \"{persistence}\"\n"));
            }
            if let Some(memory_limit) = room.memory_limit {
                out.push_str(&format!("memory-limit = {memory_limit}\n"));
            }
            if room.is_default {
                out.push_str("default = true\n");
            }
            out.push('\n');
        }
        for user in &self.users {
            out.push_str("[[users]]\n");
            out.push_str(&format!("id = {}\n", user.id));
            out.push_str(&format!("name = \"{}\"\n", toml_quote_value(&user.name)));
            out.push_str(&format!(
                "display-name = \"{}\"\n",
                toml_quote_value(&user.display_name)
            ));
            out.push_str(&format!(
                "token-hash = \"{}\"\n",
                toml_quote_value(&user.token_hash)
            ));
            out.push('\n');
        }
        out
    }
}

fn parse_config_content(
    content: &str,
    source: &str,
    config_path: Option<PathBuf>,
) -> Result<Config, String> {
    let arena = toml_spanner::Arena::new();
    let mut doc = toml_spanner::parse(content, &arena)
        .map_err(|err| format!("failed to parse {source}: {err}"))?;
    reject_deprecated_config_keys(&doc, source)?;
    if !section_contains_key(&doc, "security", "server-identity-seed") {
        return Err(format!(
            "{source}: security.server-identity-seed is required; run `chatt-server init-config PATH` to generate a private server config"
        ));
    }
    let udp_addr_configured = network_contains_key(&doc, "udp-addr");
    let mut config: Config = doc.to().map_err(|err| {
        let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
        format!("failed to deserialize {source}: {}", errors.join(", "))
    })?;
    config.config_path = config_path;
    config.apply_inferred_addresses(udp_addr_configured, false);
    config.normalize();
    config.validate(source)?;
    Ok(config)
}

/// Writes `content` to a sibling temp file, fsyncs it, then atomically renames
/// it over `path`. The rename is atomic, so a reader never sees a partial or
/// missing config even if the process dies mid-write.
fn atomic_write_config(path: &Path, content: &str) -> Result<(), String> {
    let tmp = temp_config_path(path);

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|err| format!("failed to create {}: {err}", tmp.display()))?;
    file.write_all(content.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", tmp.display()))?;
    file.sync_all()
        .map_err(|err| format!("failed to sync {}: {err}", tmp.display()))?;
    drop(file);

    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!(
            "failed to replace {} with {}: {err}",
            path.display(),
            tmp.display()
        )
    })?;
    sync_parent_dir(path);
    Ok(())
}

fn open_new_config_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|err| format!("failed to create {}: {err}", path.display()))
}

fn temp_config_path(path: &Path) -> PathBuf {
    extension_path(path, "tmp")
}

fn extension_path(path: &Path, suffix: &str) -> PathBuf {
    let extension = path
        .extension()
        .map(|extension| format!("{}.{}", extension.to_string_lossy(), suffix))
        .unwrap_or_else(|| suffix.to_string());
    path.with_extension(extension)
}

fn sync_parent_dir(path: &Path) {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return;
    };
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

pub fn value_arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find_map(|window| (window[0] == key).then(|| window[1].clone()))
}

pub fn hash_secret(secret: &str) -> String {
    let digest = digest::digest(&digest::SHA256, secret.as_bytes());
    format!("{SECRET_HASH_PREFIX}{}", encode_hex(digest.as_ref()))
}

pub fn verify_secret_hash(stored_hash: &str, secret: &str) -> bool {
    let Some(expected) = parse_secret_hash(stored_hash) else {
        return false;
    };
    let digest = digest::digest(&digest::SHA256, secret.as_bytes());
    constant_time_eq(&expected, digest.as_ref())
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&left, &right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn validate_optional_secret_hash(source: &str, name: &str, hash: &str) -> Result<(), String> {
    if hash.trim().is_empty() {
        return Ok(());
    }
    parse_secret_hash(hash)
        .map(|_| ())
        .ok_or_else(|| format!("{source}: invalid {name}; expected sha256:<64 hex chars>"))
}

fn validate_endpoint(source: &str, name: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{source}: {name} must not be empty"));
    }
    if value.parse::<SocketAddr>().is_ok() {
        return Ok(());
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(format!("{source}: {name} must include a port"));
    };
    if host.trim().is_empty() || port.trim().is_empty() {
        return Err(format!("{source}: {name} must include a host and port"));
    }
    port.parse::<u16>()
        .map(|_| ())
        .map_err(|_| format!("{source}: {name} port is invalid"))
}

fn parse_secret_hash(stored_hash: &str) -> Option<[u8; 32]> {
    let hex = stored_hash.trim().strip_prefix(SECRET_HASH_PREFIX)?;
    if hex.len() != SHA256_HEX_LEN {
        return None;
    }
    let decoded = rpc::crypto::decode_hex(hex).ok()?;
    decoded.try_into().ok()
}

fn default_shared_addr() -> SocketAddr {
    "127.0.0.1:41000"
        .parse()
        .expect("valid default network addr")
}

fn network_contains_key(doc: &toml_spanner::Document<'_>, key: &str) -> bool {
    section_contains_key(doc, "network", key)
}

fn section_contains_key(doc: &toml_spanner::Document<'_>, section: &str, key: &str) -> bool {
    doc.table()
        .get(section)
        .and_then(Item::as_table)
        .is_some_and(|table| table.contains_key(key))
}

fn reject_deprecated_config_keys(
    doc: &toml_spanner::Document<'_>,
    source: &str,
) -> Result<(), String> {
    if doc
        .table()
        .get("users")
        .and_then(Item::as_array)
        .is_some_and(|users| {
            users.iter().any(|user| {
                user.as_table()
                    .is_some_and(|user| user.contains_key("pairing-code-hash"))
            })
        })
    {
        return Err(format!(
            "failed to deserialize {source}: users.pairing-code-hash is not supported; use `chatt-server invite USER`"
        ));
    }
    Ok(())
}

fn default_rooms() -> Vec<RoomConfig> {
    vec![RoomConfig {
        id: 1,
        name: "lobby".to_string(),
        members: None,
        persistence: RoomPersistenceConfig::None,
        memory_limit: None,
        is_default: true,
    }]
}

fn generate_identity_seed_hex() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "failed to generate server identity seed".to_string())?;
    Ok(encode_hex(&bytes))
}

fn toml_quote_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::crypto::dev_server_seed_hex;

    fn config_with_users() -> Config {
        let mut config = Config::default();
        config.users = vec![
            UserConfig {
                id: UserId(1),
                name: "alice".to_string(),
                display_name: "Alice".to_string(),
                token_hash: hash_secret("alice-client-generated-token-with-at-least-32-bytes"),
            },
            UserConfig {
                id: UserId(2),
                name: "bob".to_string(),
                display_name: "Bob".to_string(),
                token_hash: hash_secret("bob-client-generated-token-with-at-least-32-bytes"),
            },
            UserConfig {
                id: UserId(3),
                name: "carol".to_string(),
                display_name: "Carol".to_string(),
                token_hash: hash_secret("carol-client-generated-token-with-at-least-32-bytes"),
            },
        ];
        config
    }

    #[test]
    fn default_config_parses_and_validates() {
        let mut config = Config::default();
        config.config_path = None;
        config.validate("<test>").unwrap();
        assert_eq!(config.network.tcp_addr.to_string(), "127.0.0.1:41000");
        assert_eq!(config.network.udp_addr, config.network.tcp_addr);
        assert_eq!(config.network.udp_probe_addr, None);
        assert_eq!(config.network.public_tcp_addr, "127.0.0.1:41000");
        assert_eq!(config.network.public_udp_addr, "127.0.0.1:41000");
        assert_eq!(config.network.public_udp_probe_addr, None);
        assert!(config.network.p2p_enabled);
        assert!(config.security.encryption);
        assert_ne!(config.security.server_identity_seed, dev_server_seed_hex());
        assert_eq!(config.rooms[0].room_id(), RoomId(1));
        assert!(config.rooms[0].is_public());
        assert_eq!(config.rooms[0].persistence, RoomPersistenceConfig::None);
        assert_eq!(config.default_room_id(), RoomId(1));
    }

    #[test]
    fn generated_template_parses_with_private_seed() {
        let content = generated_template_config().unwrap();

        let config = parse_config_content(
            &content,
            "<generated>",
            Some(PathBuf::from("chatt-server.toml")),
        )
        .unwrap();

        assert_ne!(config.security.server_identity_seed, dev_server_seed_hex());
        assert!(config.users.is_empty());
    }

    #[test]
    fn write_generated_template_refuses_to_overwrite() {
        let path = std::env::temp_dir().join(format!(
            "chatt-generated-template-{}.toml",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);

        write_generated_template(&path).unwrap();
        let error = write_generated_template(&path).unwrap_err();
        let content = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);

        assert!(error.contains("failed to create"));
        assert!(content.contains("server-identity-seed"));
    }

    #[test]
    fn config_rejects_missing_identity_seed() {
        let content = Config::default()
            .to_toml_string()
            .lines()
            .filter(|line| !line.starts_with("server-identity-seed = "))
            .collect::<Vec<_>>()
            .join("\n");

        let error = parse_config_content(&content, "<test>", Some(PathBuf::from("server.toml")))
            .unwrap_err();

        assert!(error.contains("server-identity-seed is required"));
    }

    #[test]
    fn config_rejects_public_password_longer_than_wire_limit() {
        let content = Config::default()
            .to_toml_string()
            .replace("public = false", "public = true")
            .replace(
                "password-epoch = 0",
                &format!(
                    "password = \"{}\"\npassword-epoch = 0",
                    "x".repeat(MAX_AUTH_FIELD_BYTES + 1)
                ),
            );

        let error = parse_config_content(&content, "<test>", Some(PathBuf::from("server.toml")))
            .unwrap_err();

        assert!(error.contains("security.password exceeds"));
    }

    #[test]
    fn config_with_zero_users_validates() {
        let mut config = Config::default();
        config.config_path = None;
        config.users.clear();
        config.validate("<test>").unwrap();
    }

    #[test]
    fn allocate_dynamic_user_id_starts_increments_and_persists() {
        let path = std::env::temp_dir().join(format!(
            "chatt-open-alloc-{}-{}.toml",
            std::process::id(),
            FIRST_DYNAMIC_USER_ID
        ));
        let _ = fs::remove_file(&path);
        let mut config = Config::default();
        config.config_path = Some(path.clone());

        assert_eq!(
            config.allocate_dynamic_user_id().unwrap(),
            UserId(FIRST_DYNAMIC_USER_ID)
        );
        assert_eq!(
            config.allocate_dynamic_user_id().unwrap(),
            UserId(FIRST_DYNAMIC_USER_ID + 1)
        );
        assert_eq!(
            config.security.next_dynamic_user_id,
            FIRST_DYNAMIC_USER_ID + 2
        );

        let content = fs::read_to_string(&path).unwrap();
        let reloaded = parse_config_content(&content, "<test>", Some(path.clone())).unwrap();
        assert_eq!(
            reloaded.security.next_dynamic_user_id,
            FIRST_DYNAMIC_USER_ID + 2
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn allocate_dynamic_user_id_rejects_explicit_range_counter() {
        let mut config = Config::default();
        config.security.next_dynamic_user_id = FIRST_DYNAMIC_USER_ID - 1;

        let error = config.allocate_dynamic_user_id().unwrap_err();

        assert!(error.contains("next dynamic user id"));
    }

    #[test]
    fn config_rejects_dynamic_counter_below_reserved_range() {
        let content = Config::default().to_toml_string().replace(
            "next-dynamic-user-id = 4294967296",
            "next-dynamic-user-id = 42",
        );

        let error = parse_config_content(&content, "<test>", Some(PathBuf::from("server.toml")))
            .unwrap_err();

        assert!(error.contains("next-dynamic-user-id"));
    }

    #[test]
    fn config_rejects_explicit_user_id_in_dynamic_range() {
        let content = config_with_users().to_toml_string().replace(
            "id = 1\nname = \"alice\"",
            &format!("id = {FIRST_DYNAMIC_USER_ID}\nname = \"alice\""),
        );

        let error = parse_config_content(&content, "<test>", Some(PathBuf::from("server.toml")))
            .unwrap_err();

        assert!(error.contains("reserved for dynamic users"));
    }

    #[test]
    fn config_parses_p2p_disabled() {
        let arena = toml_spanner::Arena::new();
        let content = Config::default()
            .to_toml_string()
            .replace("p2p-enabled = true", "p2p-enabled = false");
        let mut doc = toml_spanner::parse(&content, &arena).unwrap();
        let config: Config = doc.to().unwrap();

        assert!(!config.network.p2p_enabled);
    }

    #[test]
    fn udp_addr_inherits_tcp_addr_when_omitted() {
        let arena = toml_spanner::Arena::new();
        let content = Config::default().to_toml_string();
        let mut doc = toml_spanner::parse(&content, &arena).unwrap();
        let udp_addr_configured = network_contains_key(&doc, "udp-addr");
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses(udp_addr_configured, false);

        assert_eq!(config.network.udp_addr, config.network.tcp_addr);
        assert_eq!(config.network.udp_probe_addr, None);
    }

    #[test]
    fn parses_explicit_udp_and_probe_addrs() {
        let arena = toml_spanner::Arena::new();
        let content = Config::default().to_toml_string().replace(
            "tcp-addr = \"127.0.0.1:41000\"",
            "tcp-addr = \"127.0.0.1:42000\"\nudp-addr = \"127.0.0.1:42001\"\nudp-probe-addr = \"127.0.0.1:42002\"",
        );
        let mut doc = toml_spanner::parse(&content, &arena).unwrap();
        let udp_addr_configured = network_contains_key(&doc, "udp-addr");
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses(udp_addr_configured, false);

        assert_eq!(config.network.udp_addr.to_string(), "127.0.0.1:42001");
        assert_eq!(
            config.network.udp_probe_addr.map(|addr| addr.to_string()),
            Some("127.0.0.1:42002".to_string())
        );
    }

    #[test]
    fn public_endpoints_can_differ_from_bind_addresses() {
        let arena = toml_spanner::Arena::new();
        let content = Config::default().to_toml_string().replace(
            "tcp-addr = \"127.0.0.1:41000\"",
            "tcp-addr = \"0.0.0.0:41000\"\npublic-tcp-addr = \"chat.example.com:443\"\npublic-udp-addr = \"198.51.100.20:54100\"",
        );
        let mut doc = toml_spanner::parse(&content, &arena).unwrap();
        let udp_addr_configured = network_contains_key(&doc, "udp-addr");
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses(udp_addr_configured, false);
        config.normalize();
        config.validate("<test>").unwrap();

        assert_eq!(config.network.tcp_addr.to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.udp_addr.to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.public_tcp_addr, "chat.example.com:443");
        assert_eq!(config.network.public_udp_addr, "198.51.100.20:54100");
    }

    #[test]
    fn secret_hash_verification_is_exact() {
        let hash = hash_secret("pair-alice-please-change");
        assert!(verify_secret_hash(&hash, "pair-alice-please-change"));
        assert!(!verify_secret_hash(&hash, "pair-bob-please-change"));
    }

    #[test]
    fn mark_user_paired_persists_token_hash_and_display_name() {
        let path = std::env::temp_dir().join(format!(
            "chatt-server-pairing-test-{}.toml",
            std::process::id()
        ));
        let mut config = Config::default();
        config.config_path = Some(path.clone());

        let token_hash = hash_secret("client-generated-token-with-at-least-32-bytes");
        let user = config
            .mark_user_paired("alice", "Alice Example".to_string(), token_hash.clone())
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(user.token_hash, token_hash);
        assert_eq!(user.display_name, "Alice Example");
        assert!(content.contains("p2p-enabled = true"));
        assert!(!content.contains("udp-probe-addr"));
        assert!(content.contains("display-name = \"Alice Example\""));
        assert!(content.contains(&format!("token-hash = \"{token_hash}\"")));
        assert!(!content.contains("pairing-code-hash"));
    }

    #[test]
    fn set_user_display_name_updates_and_persists() {
        let path = std::env::temp_dir().join(format!(
            "chatt-server-display-name-test-{}.toml",
            std::process::id()
        ));
        let mut config = config_with_users();
        config.config_path = Some(path.clone());
        let token_hash = config.users[0].token_hash.clone();

        let user = config
            .set_user_display_name("alice", "Alice Renamed".to_string())
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(user.display_name, "Alice Renamed");
        assert_eq!(user.token_hash, token_hash);
        assert!(content.contains("display-name = \"Alice Renamed\""));
    }

    #[test]
    fn set_user_display_name_rejects_unknown_user() {
        let mut config = Config::default();
        assert!(
            config
                .set_user_display_name("nobody", "Ghost".to_string())
                .is_err()
        );
    }

    #[test]
    fn mark_user_paired_creates_new_user() {
        let path = std::env::temp_dir().join(format!(
            "chatt-server-new-user-pairing-test-{}.toml",
            std::process::id()
        ));
        let mut config = config_with_users();
        config.config_path = Some(path.clone());

        let token_hash = hash_secret("client-generated-token-with-at-least-32-bytes");
        let user = config
            .mark_user_paired("billy", "Billy".to_string(), token_hash.clone())
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(user.id, UserId(4));
        assert_eq!(user.name, "billy");
        assert_eq!(user.display_name, "Billy");
        assert_eq!(user.token_hash, token_hash);
        assert!(content.contains("name = \"billy\""));
        assert!(content.contains("display-name = \"Billy\""));
        assert!(content.contains(&format!("token-hash = \"{token_hash}\"")));
    }

    #[test]
    fn mark_user_paired_rejects_exhausted_explicit_id_range() {
        let mut config = Config::default();
        config.users = vec![UserConfig {
            id: UserId(FIRST_DYNAMIC_USER_ID - 1),
            name: "last".to_string(),
            display_name: "Last".to_string(),
            token_hash: hash_secret("last-client-generated-token-with-at-least-32-bytes"),
        }];

        let error = config
            .mark_user_paired(
                "next",
                "Next".to_string(),
                hash_secret("next-client-generated-token-with-at-least-32-bytes"),
            )
            .unwrap_err();

        assert!(error.contains("explicit user ids"));
    }

    #[test]
    fn save_runtime_writes_atomically_without_backup_or_temp_residue() {
        let path = std::env::temp_dir().join(format!(
            "chatt-server-atomic-save-test-{}.toml",
            std::process::id()
        ));
        let tmp = temp_config_path(&path);
        let mut config = Config::default();
        config.config_path = Some(path.clone());

        let first_hash = hash_secret("first-client-generated-token-with-at-least-32-bytes");
        config
            .mark_user_paired("alice", "Alice One".to_string(), first_hash.clone())
            .unwrap();
        let second_hash = hash_secret("second-client-generated-token-with-at-least-32-bytes");
        config
            .mark_user_paired("alice", "Alice Two".to_string(), second_hash.clone())
            .unwrap();

        let current = std::fs::read_to_string(&path).unwrap();
        let bak_exists = extension_path(&path, "bak").exists();
        let tmp_exists = tmp.exists();
        let _ = std::fs::remove_file(&path);

        assert!(current.contains("display-name = \"Alice Two\""));
        assert!(current.contains(&format!("token-hash = \"{second_hash}\"")));
        // The atomic rename leaves no backup or temp file behind.
        assert!(!bak_exists);
        assert!(!tmp_exists);
    }

    #[test]
    fn unparseable_config_errors_and_leaves_the_file_untouched() {
        let path = std::env::temp_dir().join(format!(
            "chatt-server-unparseable-test-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "this is not valid toml = [").unwrap();

        let error = Config::load(&path).unwrap_err();
        let content = std::fs::read_to_string(&path).unwrap();
        // The operator's file must survive verbatim, with no `.corrupt` sibling.
        let corrupt_exists = extension_path(&path, "corrupt").exists();
        let _ = std::fs::remove_file(&path);

        assert!(error.contains("failed to parse"));
        assert_eq!(content, "this is not valid toml = [");
        assert!(!corrupt_exists);
    }

    #[test]
    fn bug_report_dir_round_trips_through_persisted_config() {
        let mut config = Config::default();
        config.security.bug_report_dir = Some("/tmp/chatt-bugs".to_string());
        let content = config.to_toml_string();
        assert!(content.contains("bug-report-dir = \"/tmp/chatt-bugs\""));

        let restored = parse_config_content(
            &content,
            "<persisted>",
            Some(PathBuf::from("chatt-server.toml")),
        )
        .unwrap();
        assert_eq!(
            restored.security.bug_report_dir.as_deref(),
            Some("/tmp/chatt-bugs")
        );
    }

    #[test]
    fn default_config_omits_bug_report_dir() {
        let config = Config::default();
        assert_eq!(config.security.bug_report_dir, None);
        assert!(!config.to_toml_string().contains("bug-report-dir"));
    }

    fn parse(content: &str) -> Result<Config, String> {
        parse_config_content(content, "<test>", Some(PathBuf::from("chatt-server.toml")))
    }

    fn rooms_section(rooms: &str) -> String {
        let base = config_with_users().to_toml_string();
        let default_room = "[[rooms]]\nid = 1\nname = \"lobby\"\ndefault = true\n";
        assert!(base.contains(default_room), "unexpected default room block");
        base.replace(default_room, rooms)
    }

    #[test]
    fn room_config_parses_members_persistence_and_default() {
        let config = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\npersistence = \"durable\"\ndefault = true\n\n\
             [[rooms]]\nid = 2\nname = \"dev\"\npersistence = \"memory\"\nmemory-limit = 200\n\n\
             [[rooms]]\nid = 3\nname = \"secret\"\nmembers = [\"alice\", \"bob\"]\n",
        ))
        .unwrap();

        assert_eq!(config.rooms.len(), 3);
        assert_eq!(config.rooms[0].persistence, RoomPersistenceConfig::Durable);
        assert!(config.rooms[0].is_default);
        assert_eq!(config.rooms[1].persistence, RoomPersistenceConfig::Memory);
        assert_eq!(config.rooms[1].memory_history_limit(), 200);
        assert_eq!(
            config.rooms[2].members.as_deref(),
            Some(["alice".to_string(), "bob".to_string()].as_slice())
        );
        assert!(!config.rooms[2].is_public());
        assert_eq!(config.rooms[2].memory_history_limit(), 0);
        assert_eq!(config.default_room_id(), RoomId(1));
    }

    #[test]
    fn memory_room_without_limit_uses_the_default_ring_size() {
        let config = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\npersistence = \"memory\"\n",
        ))
        .unwrap();

        assert_eq!(
            config.rooms[0].memory_history_limit(),
            DEFAULT_MEMORY_HISTORY_LIMIT
        );
    }

    #[test]
    fn config_rejects_duplicate_room_names() {
        let error = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\n\n[[rooms]]\nid = 2\nname = \"lobby\"\n",
        ))
        .unwrap_err();

        assert!(error.contains("duplicate room name"));
    }

    #[test]
    fn config_rejects_unknown_private_member() {
        let error = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\n\n\
             [[rooms]]\nid = 2\nname = \"secret\"\nmembers = [\"mallory\"]\n",
        ))
        .unwrap_err();

        assert!(error.contains("member mallory does not match"));
    }

    #[test]
    fn config_rejects_empty_private_members() {
        let error = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\n\n\
             [[rooms]]\nid = 2\nname = \"secret\"\nmembers = []\n",
        ))
        .unwrap_err();

        assert!(error.contains("at least one member"));
    }

    #[test]
    fn config_rejects_multiple_default_rooms() {
        let error = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\ndefault = true\n\n\
             [[rooms]]\nid = 2\nname = \"dev\"\ndefault = true\n",
        ))
        .unwrap_err();

        assert!(error.contains("both set default"));
    }

    #[test]
    fn config_rejects_private_default_room() {
        let error = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\n\n\
             [[rooms]]\nid = 2\nname = \"secret\"\nmembers = [\"alice\"]\ndefault = true\n",
        ))
        .unwrap_err();

        assert!(error.contains("default room secret must be public"));
    }

    #[test]
    fn config_requires_a_public_room() {
        let error = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"secret\"\nmembers = [\"alice\"]\n",
        ))
        .unwrap_err();

        assert!(error.contains("at least one public room"));
    }

    #[test]
    fn config_rejects_room_id_in_dynamic_range() {
        let error = parse(&rooms_section(&format!(
            "[[rooms]]\nid = {FIRST_DYNAMIC_ROOM_ID}\nname = \"lobby\"\n"
        )))
        .unwrap_err();

        assert!(error.contains("reserved for runtime-created rooms"));
    }

    #[test]
    fn config_rejects_memory_limit_without_memory_persistence() {
        let error = parse(&rooms_section(
            "[[rooms]]\nid = 1\nname = \"lobby\"\npersistence = \"durable\"\nmemory-limit = 10\n",
        ))
        .unwrap_err();

        assert!(error.contains("memory-limit requires"));
    }

    #[test]
    fn default_room_falls_back_to_lowest_public_id() {
        let config = parse(&rooms_section(
            "[[rooms]]\nid = 7\nname = \"annex\"\n\n\
             [[rooms]]\nid = 2\nname = \"den\"\n\n\
             [[rooms]]\nid = 3\nname = \"secret\"\nmembers = [\"alice\"]\n",
        ))
        .unwrap();

        assert_eq!(config.default_room_id(), RoomId(2));
    }

    #[test]
    fn data_dir_defaults_beside_config_file() {
        let mut config = Config::default();
        config.config_path = Some(PathBuf::from("/srv/chatt/chatt-server.toml"));
        assert_eq!(
            config.data_dir(),
            Some(PathBuf::from("/srv/chatt/chatt-server-data"))
        );

        config.storage.data_dir = Some("/var/lib/chatt".to_string());
        assert_eq!(config.data_dir(), Some(PathBuf::from("/var/lib/chatt")));

        config.storage.data_dir = None;
        config.config_path = None;
        assert_eq!(config.data_dir(), None);
    }

    #[test]
    fn room_settings_round_trip_through_persisted_config() {
        let mut config = config_with_users();
        config.storage.data_dir = Some("room-data".to_string());
        config.rooms = vec![
            RoomConfig {
                id: 1,
                name: "lobby".to_string(),
                members: None,
                persistence: RoomPersistenceConfig::Durable,
                memory_limit: None,
                is_default: true,
            },
            RoomConfig {
                id: 2,
                name: "dev".to_string(),
                members: None,
                persistence: RoomPersistenceConfig::Memory,
                memory_limit: Some(200),
                is_default: false,
            },
            RoomConfig {
                id: 3,
                name: "secret".to_string(),
                members: Some(vec!["alice".to_string(), "bob".to_string()]),
                persistence: RoomPersistenceConfig::None,
                memory_limit: None,
                is_default: false,
            },
        ];

        let restored = parse(&config.to_toml_string()).unwrap();

        assert_eq!(restored.storage.data_dir.as_deref(), Some("room-data"));
        assert_eq!(restored.rooms.len(), 3);
        assert_eq!(
            restored.rooms[0].persistence,
            RoomPersistenceConfig::Durable
        );
        assert!(restored.rooms[0].is_default);
        assert_eq!(restored.rooms[1].memory_limit, Some(200));
        assert_eq!(
            restored.rooms[2].members.as_deref(),
            Some(["alice".to_string(), "bob".to_string()].as_slice())
        );
    }

    #[test]
    fn persisted_public_config_without_users_is_accepted() {
        let content = Config::default().to_toml_string();
        let content = content.replace("public = false", "public = true");
        let config = parse_config_content(
            &content,
            "<persisted>",
            Some(PathBuf::from("chatt-server.toml")),
        )
        .unwrap();

        assert!(config.security.public);
        assert!(config.users.is_empty());
    }
}
