use hashbrown::HashSet;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use ring::{digest, rand::SecureRandom, signature::KeyPair};
use rpc::{
    control::DEFAULT_FILE_SIZE_LIMIT_BYTES,
    crypto::{encode_hex, server_key_pair_from_seed_hex},
    ids::{RoomId, UserId},
};
use toml_spanner::{Item, Toml};

const SECRET_HASH_PREFIX: &str = "sha256:";
const SHA256_HEX_LEN: usize = 64;
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:41000";
/// First user id handed out to a dynamic (open-paired) user. Ids below this are
/// reserved for explicit user-registry entries.
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
    /// UDP media bind address. `None` inherits `tcp-addr`; read through
    /// [`NetworkConfig::udp_addr`].
    #[toml(FromToml with = toml_spanner::helper::parse_string)]
    pub udp_addr: Option<SocketAddr>,
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

impl NetworkConfig {
    /// The UDP media bind address: `udp-addr` when configured, otherwise
    /// `tcp-addr`.
    pub fn udp_addr(&self) -> SocketAddr {
        self.udp_addr.unwrap_or(self.tcp_addr)
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            tcp_addr: DEFAULT_LISTEN_ADDR.parse().expect("valid default TCP addr"),
            udp_addr: None,
            udp_probe_addr: None,
            public_tcp_addr: String::new(),
            public_udp_addr: String::new(),
            public_udp_probe_addr: None,
            p2p_enabled: true,
        }
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
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
    /// SHA-256 hash (`sha256:<hex>`) of the shared secret required for open
    /// pairing. `None`/empty means no password.
    #[toml(default)]
    pub password_hash: Option<String>,
    /// Current password epoch. Dynamic tokens embed the epoch they were issued
    /// under. Bumping this invalidates existing tokens.
    #[toml(default)]
    pub password_epoch: u32,
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
            password_hash: None,
            password_epoch: 0,
        }
    }
}

/// Server-side retention for a room's messages.
///
/// `None` relays without retaining, `Memory` keeps a bounded in-memory ring
/// that is lost on restart, and `Durable` appends to an on-disk log under the
/// storage data dir.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub enum RoomPersistenceConfig {
    #[default]
    None,
    Memory,
    Durable,
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct RoomConfig {
    pub id: u32,
    pub name: String,
    /// `None` means public: every user on the server can access the room.
    /// `Some` restricts access to the listed user-registry names.
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
    /// Directory for server-side room data (durable logs, DM registry, user
    /// registry). Defaults to `<config stem>-data` beside the config file.
    #[toml(default)]
    pub data_dir: Option<String>,
}

/// One record in the server-managed user registry (see
/// [`crate::user_store::UserStore`]).
#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
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

/// The operator's server configuration.
///
/// Parsed once at startup and never rewritten by the server. Everything the
/// server mutates at runtime (user records, dynamic-id counters, the DM
/// registry) lives in state files under [`Config::data_dir`].
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
            config_path: None,
        };
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
#
# The server never rewrites this file. Runtime state (the user registry, the
# DM room registry, message logs) lives under storage.data-dir.

[network]
# Bind addresses on this host.
tcp-addr = "{listen_addr}"
# udp-addr defaults to tcp-addr when omitted.
# udp-addr = "{listen_addr}"
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
# SHA-256 hash of the shared secret gating public open pairing. Omit for no
# password. Generate with: printf %s 'secret' | sha256sum
# password-hash = "sha256:2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
# Bump this to invalidate existing dynamic tokens.
password-epoch = 0

# Server-side runtime state (durable message logs, the DM room registry, the
# user registry) lives in storage.data-dir. Defaults to "<config stem>-data"
# beside this file.
# [storage]
# data-dir = "chatt-server-data"

[[rooms]]
id = 1
name = "lobby"
# Rooms are public unless they list members; members are user-registry names.
# A member that has not paired yet is ignored until it exists.
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
# Accepted invites are recorded in the user registry under storage.data-dir.
"#,
        listen_addr = DEFAULT_LISTEN_ADDR,
        seed = seed,
        max_file_size_bytes = DEFAULT_FILE_SIZE_LIMIT_BYTES,
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

    pub fn is_public(&self) -> bool {
        self.security.public
    }

    /// The stored `sha256:` hash gating open pairing, `None` when open pairing
    /// is unpassworded.
    pub fn password_hash(&self) -> Option<&str> {
        self.security
            .password_hash
            .as_deref()
            .filter(|hash| !hash.is_empty())
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

    /// Directory for server-side runtime state: `storage.data-dir` when set,
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

    fn normalize(&mut self) {
        self.network.public_tcp_addr = self.network.public_tcp_addr.trim().to_string();
        if self.network.public_tcp_addr.is_empty() {
            self.network.public_tcp_addr = self.network.tcp_addr.to_string();
        }
        self.network.public_udp_addr = self.network.public_udp_addr.trim().to_string();
        if self.network.public_udp_addr.is_empty() {
            self.network.public_udp_addr = self.network.udp_addr().to_string();
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
    }

    fn validate(&self, source: &str) -> Result<(), String> {
        if self.rooms.is_empty() {
            return Err(format!("{source}: at least one room is required"));
        }
        server_key_pair_from_seed_hex(&self.security.server_identity_seed)
            .map_err(|error| format!("{source}: invalid security.server-identity-seed: {error}"))?;
        if let Some(hash) = self.password_hash() {
            validate_secret_hash(source, "security.password-hash", hash)?;
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
}

fn parse_config_content(
    content: &str,
    source: &str,
    config_path: Option<PathBuf>,
) -> Result<Config, String> {
    let arena = toml_spanner::Arena::new();
    let mut doc = toml_spanner::parse(content, &arena)
        .map_err(|err| format!("failed to parse {source}: {err}"))?;
    if !section_contains_key(&doc, "security", "server-identity-seed") {
        return Err(format!(
            "{source}: security.server-identity-seed is required; run `chatt-server init-config PATH` to generate a private server config"
        ));
    }
    let mut config: Config = doc.to().map_err(|err| {
        let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
        format!("failed to deserialize {source}: {}", errors.join(", "))
    })?;
    config.config_path = config_path;
    config.normalize();
    config.validate(source)?;
    Ok(config)
}

/// Writes `content` to a sibling temp file, fsyncs it, then atomically renames
/// it over `path`. The rename is atomic, so a reader never sees a partial or
/// missing file even if the process dies mid-write. The temp file is created
/// with `create_new` and owner-only mode (0600) because the content carries
/// secrets, and the rename makes its mode the destination's mode; a stale or
/// planted file at the predictable temp path is removed and the exclusive
/// create retried rather than opened through.
pub(crate) fn atomic_write_toml(path: &Path, content: &str) -> Result<(), String> {
    let tmp = temp_config_path(path);
    let mut file = match create_temp_file(&tmp) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(&tmp)
                .map_err(|err| format!("failed to remove stale {}: {err}", tmp.display()))?;
            create_temp_file(&tmp)
                .map_err(|err| format!("failed to create {}: {err}", tmp.display()))?
        }
        Err(err) => return Err(format!("failed to create {}: {err}", tmp.display())),
    };
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

fn create_temp_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn open_new_config_file(path: &Path) -> Result<File, String> {
    create_temp_file(path).map_err(|err| format!("failed to create {}: {err}", path.display()))
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

/// Whether `secret` hashes to `stored_hash`. The comparison runs over
/// fixed-length digests in constant time, so neither the secret's content nor
/// its length leaks through timing.
pub fn verify_secret_hash(stored_hash: &str, secret: &str) -> bool {
    let Some(expected) = parse_secret_hash(stored_hash) else {
        return false;
    };
    let digest = digest::digest(&digest::SHA256, secret.as_bytes());
    let mut diff = 0u8;
    for (&left, &right) in expected.iter().zip(digest.as_ref()) {
        diff |= left ^ right;
    }
    diff == 0
}

pub(crate) fn validate_secret_hash(source: &str, name: &str, hash: &str) -> Result<(), String> {
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

fn section_contains_key(doc: &toml_spanner::Document<'_>, section: &str, key: &str) -> bool {
    doc.table()
        .get(section)
        .and_then(Item::as_table)
        .is_some_and(|table| table.contains_key(key))
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

/// Escapes `value` for a TOML basic string. Control characters become
/// `\uXXXX`, matching how toml-spanner renders them, so client-supplied names
/// can never produce a state file the parser rejects on reload.
pub(crate) fn toml_quote_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04X}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::crypto::dev_server_seed_hex;

    fn parse(content: &str) -> Result<Config, String> {
        parse_config_content(content, "<test>", Some(PathBuf::from("chatt-server.toml")))
    }

    /// Minimal valid config content with `extra` appended after the required
    /// sections; rooms default to the lobby when `extra` declares none.
    fn config_content(extra: &str) -> String {
        format!(
            "[network]\ntcp-addr = \"127.0.0.1:41000\"\n\n[security]\nserver-identity-seed = \"{}\"\n\n{extra}",
            dev_server_seed_hex()
        )
    }

    #[test]
    fn default_config_parses_and_validates() {
        let config = Config::default();
        config.validate("<test>").unwrap();
        assert_eq!(config.network.tcp_addr.to_string(), "127.0.0.1:41000");
        assert_eq!(config.network.udp_addr(), config.network.tcp_addr);
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

        let config = parse(&content).unwrap();

        assert_ne!(config.security.server_identity_seed, dev_server_seed_hex());
        assert_eq!(config.network.udp_addr(), config.network.tcp_addr);
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
        let content = "[network]\ntcp-addr = \"127.0.0.1:41000\"\n";

        let error = parse(content).unwrap_err();

        assert!(error.contains("server-identity-seed is required"));
    }

    #[test]
    fn config_rejects_malformed_password_hash() {
        let content =
            config_content("").replace("[security]", "[security]\npassword-hash = \"hunter2\"");

        let error = parse(&content).unwrap_err();

        assert!(error.contains("security.password-hash"));
    }

    #[test]
    fn config_accepts_hashed_password() {
        let content = config_content("").replace(
            "[security]",
            &format!("[security]\npassword-hash = \"{}\"", hash_secret("hunter2")),
        );

        let config = parse(&content).unwrap();

        assert!(verify_secret_hash(
            config.password_hash().unwrap(),
            "hunter2"
        ));
    }

    #[test]
    fn config_parses_p2p_disabled() {
        let content = config_content("").replace("[network]", "[network]\np2p-enabled = false");

        let config = parse(&content).unwrap();

        assert!(!config.network.p2p_enabled);
    }

    #[test]
    fn udp_addr_inherits_tcp_addr_when_omitted() {
        let config = parse(&config_content("")).unwrap();

        assert_eq!(config.network.udp_addr, None);
        assert_eq!(config.network.udp_addr(), config.network.tcp_addr);
        assert_eq!(config.network.udp_probe_addr, None);
    }

    #[test]
    fn parses_explicit_udp_and_probe_addrs() {
        let content = config_content("").replace(
            "tcp-addr = \"127.0.0.1:41000\"",
            "tcp-addr = \"127.0.0.1:42000\"\nudp-addr = \"127.0.0.1:42001\"\nudp-probe-addr = \"127.0.0.1:42002\"",
        );

        let config = parse(&content).unwrap();

        assert_eq!(config.network.udp_addr().to_string(), "127.0.0.1:42001");
        assert_eq!(
            config.network.udp_probe_addr.map(|addr| addr.to_string()),
            Some("127.0.0.1:42002".to_string())
        );
    }

    #[test]
    fn public_endpoints_can_differ_from_bind_addresses() {
        let content = config_content("").replace(
            "tcp-addr = \"127.0.0.1:41000\"",
            "tcp-addr = \"0.0.0.0:41000\"\npublic-tcp-addr = \"chat.example.com:443\"\npublic-udp-addr = \"198.51.100.20:54100\"",
        );

        let config = parse(&content).unwrap();

        assert_eq!(config.network.tcp_addr.to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.udp_addr().to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.public_tcp_addr, "chat.example.com:443");
        assert_eq!(config.network.public_udp_addr, "198.51.100.20:54100");
    }

    #[test]
    fn secret_hash_verification_is_exact() {
        let hash = hash_secret("pair-alice-please-change");
        assert!(verify_secret_hash(&hash, "pair-alice-please-change"));
        assert!(!verify_secret_hash(&hash, "pair-bob-please-change"));
        assert!(!verify_secret_hash(&hash, "short"));
        assert!(!verify_secret_hash(
            &hash,
            "a-much-longer-candidate-secret-than-the-stored-one"
        ));
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
        let corrupt_exists = extension_path(&path, "corrupt").exists();
        let _ = std::fs::remove_file(&path);

        assert!(error.contains("failed to parse"));
        assert_eq!(content, "this is not valid toml = [");
        assert!(!corrupt_exists);
    }

    #[test]
    fn bug_report_dir_parses() {
        let content = config_content("").replace(
            "[security]",
            "[security]\nbug-report-dir = \"/tmp/chatt-bugs\"",
        );

        let config = parse(&content).unwrap();

        assert_eq!(
            config.security.bug_report_dir.as_deref(),
            Some("/tmp/chatt-bugs")
        );
    }

    #[test]
    fn room_config_parses_members_persistence_and_default() {
        let config = parse(&config_content(
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
        let config = parse(&config_content(
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
        let error = parse(&config_content(
            "[[rooms]]\nid = 1\nname = \"lobby\"\n\n[[rooms]]\nid = 2\nname = \"lobby\"\n",
        ))
        .unwrap_err();

        assert!(error.contains("duplicate room name"));
    }

    #[test]
    fn config_rejects_empty_private_members() {
        let error = parse(&config_content(
            "[[rooms]]\nid = 1\nname = \"lobby\"\n\n\
             [[rooms]]\nid = 2\nname = \"secret\"\nmembers = []\n",
        ))
        .unwrap_err();

        assert!(error.contains("at least one member"));
    }

    #[test]
    fn config_rejects_multiple_default_rooms() {
        let error = parse(&config_content(
            "[[rooms]]\nid = 1\nname = \"lobby\"\ndefault = true\n\n\
             [[rooms]]\nid = 2\nname = \"dev\"\ndefault = true\n",
        ))
        .unwrap_err();

        assert!(error.contains("both set default"));
    }

    #[test]
    fn config_rejects_private_default_room() {
        let error = parse(&config_content(
            "[[rooms]]\nid = 1\nname = \"lobby\"\n\n\
             [[rooms]]\nid = 2\nname = \"secret\"\nmembers = [\"alice\"]\ndefault = true\n",
        ))
        .unwrap_err();

        assert!(error.contains("default room secret must be public"));
    }

    #[test]
    fn config_requires_a_public_room() {
        let error = parse(&config_content(
            "[[rooms]]\nid = 1\nname = \"secret\"\nmembers = [\"alice\"]\n",
        ))
        .unwrap_err();

        assert!(error.contains("at least one public room"));
    }

    #[test]
    fn config_rejects_room_id_in_dynamic_range() {
        let error = parse(&config_content(&format!(
            "[[rooms]]\nid = {FIRST_DYNAMIC_ROOM_ID}\nname = \"lobby\"\n"
        )))
        .unwrap_err();

        assert!(error.contains("reserved for runtime-created rooms"));
    }

    #[test]
    fn config_rejects_memory_limit_without_memory_persistence() {
        let error = parse(&config_content(
            "[[rooms]]\nid = 1\nname = \"lobby\"\npersistence = \"durable\"\nmemory-limit = 10\n",
        ))
        .unwrap_err();

        assert!(error.contains("memory-limit requires"));
    }

    #[test]
    fn default_room_falls_back_to_lowest_public_id() {
        let config = parse(&config_content(
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
    fn toml_quote_value_escapes_control_characters() {
        assert_eq!(toml_quote_value("x\u{1}y"), "x\\u0001y");
        assert_eq!(toml_quote_value("bell\u{7}"), "bell\\u0007");
        assert_eq!(toml_quote_value("del\u{7f}"), "del\\u007F");
        assert_eq!(toml_quote_value("tab\tquote\""), "tab\\tquote\\\"");
    }

    #[test]
    fn atomic_write_toml_keeps_secrets_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "chatt-atomic-mode-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_file(&path);

        atomic_write_toml(&path, "key = 1\n").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "state rewrite must stay owner-only, got {mode:o}"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn atomic_write_toml_replaces_a_planted_temp_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!(
            "chatt-atomic-planted-{}-{:?}.toml",
            std::process::id(),
            std::thread::current().id()
        ));
        let tmp = temp_config_path(&path);
        let _ = fs::remove_file(&path);
        fs::write(&tmp, "planted").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write_toml(&path, "key = 1\n").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        let tmp_exists = tmp.exists();
        let _ = fs::remove_file(&path);

        assert_eq!(content, "key = 1\n");
        assert_eq!(
            mode & 0o077,
            0,
            "planted mode must not survive, got {mode:o}"
        );
        assert!(!tmp_exists);
    }
}
