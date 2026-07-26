use hashbrown::HashSet;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use aws_lc_rs::{digest, rand::SecureRandom, signature::KeyPair};
use rpc::{
    control::DEFAULT_FILE_SIZE_LIMIT_BYTES,
    crypto::{encode_hex, server_key_pair_from_seed_hex},
    ids::{RoomId, UserId},
};
use toml_spanner::{Context, Failed, FromToml, Item, Toml};

use crate::config_diagnostics::{self, Diag};

const SECRET_HASH_PREFIX: &str = "sha256:";
const SHA256_HEX_LEN: usize = 64;
const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:41000";
pub const SERVER_CONFIG_FILE_NAME: &str = "chatt-server.toml";
const MIB: u64 = 1024 * 1024;
const DEFAULT_FILE_SIZE_LIMIT_MB: u64 = DEFAULT_FILE_SIZE_LIMIT_BYTES / MIB;
/// First user id handed out to a dynamic (open-paired) user. Ids below this are
/// reserved for explicit user-registry entries.
pub const FIRST_DYNAMIC_USER_ID: u64 = u32::MAX as u64 + 1;
/// First room id handed out to a runtime-created (DM) room. Ids below this are
/// reserved for explicit `[[rooms]]` entries.
pub const FIRST_DYNAMIC_ROOM_ID: u32 = 0x8000_0000;
/// Ring size used for `persistence = "memory"` rooms without an explicit
/// `memory-limit`.
pub const DEFAULT_MEMORY_HISTORY_LIMIT: usize = 512;

#[derive(Clone, Copy, Debug)]
pub struct Binds {
    pub tcp: SocketAddr,
    pub udp: SocketAddr,
}

impl Default for Binds {
    fn default() -> Self {
        let addr = default_listen_addr();
        Self {
            tcp: addr,
            udp: addr,
        }
    }
}

impl<'de> FromToml<'de> for Binds {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        if item.as_str().is_some() {
            let addr = toml_spanner::helper::parse_string::from_toml(ctx, item)?;
            return Ok(Self {
                tcp: addr,
                udp: addr,
            });
        }

        let mut table = item.table_helper(ctx)?;
        let tcp = table.required_mapped("tcp", Item::parse::<SocketAddr>)?;
        let udp = table.required_mapped("udp", Item::parse::<SocketAddr>)?;
        table.require_empty()?;
        Ok(Self { tcp, udp })
    }
}

#[derive(Clone, Debug, Default)]
pub struct PublicAddrs {
    pub tcp: String,
    pub udp: String,
}

impl<'de> FromToml<'de> for PublicAddrs {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        if let Some(addr) = item.as_str() {
            let addr = addr.to_string();
            return Ok(Self {
                tcp: addr.clone(),
                udp: addr,
            });
        }

        let mut table = item.table_helper(ctx)?;
        let tcp = table.required("tcp")?;
        let udp = table.required("udp")?;
        table.require_empty()?;
        Ok(Self { tcp, udp })
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct NetworkConfig {
    #[toml(default)]
    pub bind: Binds,
    #[toml(FromToml with = toml_spanner::helper::parse_string)]
    pub udp_probe_addr: Option<SocketAddr>,
    #[toml(default)]
    pub public_addr: PublicAddrs,
    #[toml(default)]
    pub public_udp_probe_addr: Option<String>,
    #[toml(skip)]
    public_udp_probe_addr_overridden: bool,
    #[toml(default = true)]
    pub p2p: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            bind: Binds::default(),
            udp_probe_addr: None,
            public_addr: PublicAddrs::default(),
            public_udp_probe_addr: None,
            public_udp_probe_addr_overridden: false,
            p2p: true,
        }
    }
}

fn default_listen_addr() -> SocketAddr {
    DEFAULT_LISTEN_ADDR.parse().expect("valid default TCP addr")
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct SecurityConfig {
    pub server_identity_seed: String,
    /// Whether Chatt encrypts control, media, video, and file transport payloads.
    #[toml(default = true)]
    pub transport_encryption: bool,
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_MB)]
    pub max_file_size_mb: u64,
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
            transport_encryption: true,
            max_file_size_mb: DEFAULT_FILE_SIZE_LIMIT_MB,
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
    /// Maximum offline MLS replay period for this room. When absent the
    /// global storage default is resolved when the encrypted room is created.
    #[toml(default)]
    pub mls_retention_days: Option<u16>,
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

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct StorageConfig {
    /// Directory for server-side room data (durable logs, DM registry, user
    /// registry). Defaults to `<config stem>-data` beside the config file.
    #[toml(default)]
    pub data_dir: Option<String>,
    #[toml(default = 90)]
    pub mls_retention_days: u16,
    #[toml(default = 15)]
    pub mls_cleanup_interval_minutes: u64,
    #[toml(default = 4096)]
    pub mls_cleanup_batch_events: usize,
    #[toml(default = 24)]
    pub mls_compaction_min_interval_hours: u64,
    #[toml(default = 256)]
    pub mls_compaction_min_fragmented_mib: u64,
    #[toml(default = 25)]
    pub mls_compaction_min_fragmented_percent: u8,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: None,
            mls_retention_days: 90,
            mls_cleanup_interval_minutes: 15,
            mls_cleanup_batch_events: 4096,
            mls_compaction_min_interval_hours: 24,
            mls_compaction_min_fragmented_mib: 256,
            mls_compaction_min_fragmented_percent: 25,
        }
    }
}

/// One record in the server-managed user registry (see
/// [`crate::user_store::UserStore`]).
#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct UserConfig {
    pub id: UserId,
    /// Should ONLY be used in configs, never appear on the RPC boundary and never be used
    /// for identity during runtime accept, during config loading where it's converted to the
    /// the UserId. Persisted under the `name` key.
    #[toml(rename = "name")]
    pub internal_reference: String,

    /// User chosen username, must be unique in the server. For display purposes only, true identifer is still UserID.
    /// Persisted under the `display-name` key (the wire/storage rename is deferred).
    #[toml(default, rename = "display-name")]
    pub username: String,

    #[toml(default)]
    pub token_hash: String,
}

impl UserConfig {
    pub fn user_id(&self) -> UserId {
        self.id
    }
}

/// Whether a user-chosen username is valid on the server: non-empty after
/// trimming, at most 64 bytes, and free of control characters.
pub(crate) fn valid_username(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty() && name.len() <= 64 && !name.chars().any(char::is_control)
}

/// The operator's server configuration.
///
/// Parsed once at startup and never rewritten by the server. Everything the
/// server mutates at runtime (user records, dynamic usernames, the DM registry)
/// lives in state files under [`Config::data_dir`].
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConfigOverride {
    name: &'static str,
    value: String,
}

impl ConfigOverride {
    pub(crate) fn new(name: &str, value: String) -> Result<Self, String> {
        let spec = config_option_spec(name)
            .ok_or_else(|| format!("unknown server configuration option --{name}"))?;
        Ok(Self {
            name: spec.name,
            value,
        })
    }

    pub(crate) fn name(&self) -> &'static str {
        self.name
    }
}

pub(crate) struct ConfigOptionSpec {
    pub name: &'static str,
    pub value_name: &'static str,
    pub description: &'static str,
    apply: fn(&mut Config, &str) -> Result<(), String>,
}

macro_rules! scalar_parse_setter {
    ($function:ident, $section:ident.$field:ident, $type:ty, $name:literal) => {
        fn $function(config: &mut Config, value: &str) -> Result<(), String> {
            let parsed = value
                .parse::<$type>()
                .map_err(|error| format!("invalid value for --{}: {error}", $name))?;
            config.$section.$field = parsed;
            Ok(())
        }
    };
}

macro_rules! optional_parse_setter {
    ($function:ident, $section:ident.$field:ident, $type:ty, $name:literal) => {
        fn $function(config: &mut Config, value: &str) -> Result<(), String> {
            let parsed = if value.is_empty() {
                None
            } else {
                Some(
                    value
                        .parse::<$type>()
                        .map_err(|error| format!("invalid value for --{}: {error}", $name))?,
                )
            };
            config.$section.$field = parsed;
            Ok(())
        }
    };
}

macro_rules! optional_string_setter {
    ($function:ident, $section:ident.$field:ident) => {
        fn $function(config: &mut Config, value: &str) -> Result<(), String> {
            config.$section.$field = (!value.is_empty()).then(|| value.to_string());
            Ok(())
        }
    };
}

fn set_network_bind(config: &mut Config, value: &str) -> Result<(), String> {
    let addr = value
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid value for --network.bind: {error}"))?;
    config.network.bind = Binds {
        tcp: addr,
        udp: addr,
    };
    Ok(())
}
fn set_network_bind_tcp(config: &mut Config, value: &str) -> Result<(), String> {
    config.network.bind.tcp = value
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid value for --network.bind.tcp: {error}"))?;
    Ok(())
}
fn set_network_bind_udp(config: &mut Config, value: &str) -> Result<(), String> {
    config.network.bind.udp = value
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid value for --network.bind.udp: {error}"))?;
    Ok(())
}
optional_parse_setter!(
    set_network_udp_probe_addr,
    network.udp_probe_addr,
    SocketAddr,
    "network.udp-probe-addr"
);
fn set_network_public_addr(config: &mut Config, value: &str) -> Result<(), String> {
    config.network.public_addr = PublicAddrs {
        tcp: value.to_string(),
        udp: value.to_string(),
    };
    Ok(())
}
fn set_network_public_addr_tcp(config: &mut Config, value: &str) -> Result<(), String> {
    config.network.public_addr.tcp = value.to_string();
    Ok(())
}
fn set_network_public_addr_udp(config: &mut Config, value: &str) -> Result<(), String> {
    config.network.public_addr.udp = value.to_string();
    Ok(())
}
fn set_network_public_udp_probe_addr(config: &mut Config, value: &str) -> Result<(), String> {
    config.network.public_udp_probe_addr = (!value.is_empty()).then(|| value.to_string());
    config.network.public_udp_probe_addr_overridden = true;
    Ok(())
}
scalar_parse_setter!(set_network_p2p_enabled, network.p2p, bool, "network.p2p");

scalar_parse_setter!(
    set_security_transport_encryption,
    security.transport_encryption,
    bool,
    "security.transport-encryption"
);
scalar_parse_setter!(
    set_security_max_file_size_mb,
    security.max_file_size_mb,
    u64,
    "security.max-file-size-mb"
);
optional_string_setter!(set_security_bug_report_dir, security.bug_report_dir);
scalar_parse_setter!(
    set_security_public,
    security.public,
    bool,
    "security.public"
);
optional_string_setter!(set_security_password_hash, security.password_hash);
scalar_parse_setter!(
    set_security_password_epoch,
    security.password_epoch,
    u32,
    "security.password-epoch"
);

optional_string_setter!(set_storage_data_dir, storage.data_dir);
scalar_parse_setter!(
    set_storage_mls_retention_days,
    storage.mls_retention_days,
    u16,
    "storage.mls-retention-days"
);
scalar_parse_setter!(
    set_storage_mls_cleanup_interval_minutes,
    storage.mls_cleanup_interval_minutes,
    u64,
    "storage.mls-cleanup-interval-minutes"
);
scalar_parse_setter!(
    set_storage_mls_cleanup_batch_events,
    storage.mls_cleanup_batch_events,
    usize,
    "storage.mls-cleanup-batch-events"
);
scalar_parse_setter!(
    set_storage_mls_compaction_min_interval_hours,
    storage.mls_compaction_min_interval_hours,
    u64,
    "storage.mls-compaction-min-interval-hours"
);
scalar_parse_setter!(
    set_storage_mls_compaction_min_fragmented_mib,
    storage.mls_compaction_min_fragmented_mib,
    u64,
    "storage.mls-compaction-min-fragmented-mib"
);
scalar_parse_setter!(
    set_storage_mls_compaction_min_fragmented_percent,
    storage.mls_compaction_min_fragmented_percent,
    u8,
    "storage.mls-compaction-min-fragmented-percent"
);

pub(crate) const CONFIG_OPTION_SPECS: &[ConfigOptionSpec] = &[
    ConfigOptionSpec {
        name: "network.bind",
        value_name: "ADDR",
        description: "TCP and UDP bind address",
        apply: set_network_bind,
    },
    ConfigOptionSpec {
        name: "network.bind.tcp",
        value_name: "ADDR",
        description: "TCP bind address",
        apply: set_network_bind_tcp,
    },
    ConfigOptionSpec {
        name: "network.bind.udp",
        value_name: "ADDR",
        description: "UDP media bind address",
        apply: set_network_bind_udp,
    },
    ConfigOptionSpec {
        name: "network.udp-probe-addr",
        value_name: "ADDR",
        description: "UDP P2P probe bind address; empty disables it",
        apply: set_network_udp_probe_addr,
    },
    ConfigOptionSpec {
        name: "network.public-addr",
        value_name: "ENDPOINT",
        description: "TCP and UDP endpoints advertised to clients",
        apply: set_network_public_addr,
    },
    ConfigOptionSpec {
        name: "network.public-addr.tcp",
        value_name: "ENDPOINT",
        description: "TCP endpoint advertised to clients",
        apply: set_network_public_addr_tcp,
    },
    ConfigOptionSpec {
        name: "network.public-addr.udp",
        value_name: "ENDPOINT",
        description: "UDP endpoint advertised to clients",
        apply: set_network_public_addr_udp,
    },
    ConfigOptionSpec {
        name: "network.public-udp-probe-addr",
        value_name: "ENDPOINT",
        description: "UDP probe endpoint advertised to clients; empty disables it",
        apply: set_network_public_udp_probe_addr,
    },
    ConfigOptionSpec {
        name: "network.p2p",
        value_name: "BOOL",
        description: "enable direct peer-to-peer media",
        apply: set_network_p2p_enabled,
    },
    ConfigOptionSpec {
        name: "security.transport-encryption",
        value_name: "BOOL",
        description: "encrypt transport payloads",
        apply: set_security_transport_encryption,
    },
    ConfigOptionSpec {
        name: "security.max-file-size-mb",
        value_name: "MIB",
        description: "maximum relayed file size",
        apply: set_security_max_file_size_mb,
    },
    ConfigOptionSpec {
        name: "security.bug-report-dir",
        value_name: "PATH",
        description: "bug-report output directory; empty disables reports",
        apply: set_security_bug_report_dir,
    },
    ConfigOptionSpec {
        name: "security.public",
        value_name: "BOOL",
        description: "allow users to pair without an invite",
        apply: set_security_public,
    },
    ConfigOptionSpec {
        name: "security.password-hash",
        value_name: "HASH",
        description: "public-pairing password hash; empty clears it",
        apply: set_security_password_hash,
    },
    ConfigOptionSpec {
        name: "security.password-epoch",
        value_name: "NUMBER",
        description: "dynamic-token password epoch",
        apply: set_security_password_epoch,
    },
    ConfigOptionSpec {
        name: "storage.data-dir",
        value_name: "PATH",
        description: "runtime state directory; empty uses the config default",
        apply: set_storage_data_dir,
    },
    ConfigOptionSpec {
        name: "storage.mls-retention-days",
        value_name: "DAYS",
        description: "default MLS replay retention",
        apply: set_storage_mls_retention_days,
    },
    ConfigOptionSpec {
        name: "storage.mls-cleanup-interval-minutes",
        value_name: "MINUTES",
        description: "MLS cleanup interval",
        apply: set_storage_mls_cleanup_interval_minutes,
    },
    ConfigOptionSpec {
        name: "storage.mls-cleanup-batch-events",
        value_name: "COUNT",
        description: "maximum events removed per MLS cleanup batch",
        apply: set_storage_mls_cleanup_batch_events,
    },
    ConfigOptionSpec {
        name: "storage.mls-compaction-min-interval-hours",
        value_name: "HOURS",
        description: "minimum interval between MLS compactions",
        apply: set_storage_mls_compaction_min_interval_hours,
    },
    ConfigOptionSpec {
        name: "storage.mls-compaction-min-fragmented-mib",
        value_name: "MIB",
        description: "minimum fragmented space before MLS compaction",
        apply: set_storage_mls_compaction_min_fragmented_mib,
    },
    ConfigOptionSpec {
        name: "storage.mls-compaction-min-fragmented-percent",
        value_name: "PERCENT",
        description: "minimum fragmentation percentage before MLS compaction",
        apply: set_storage_mls_compaction_min_fragmented_percent,
    },
];

fn config_option_spec(name: &str) -> Option<&'static ConfigOptionSpec> {
    CONFIG_OPTION_SPECS.iter().find(|spec| spec.name == name)
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

pub(crate) fn load_or_initialize_directory(
    dir: &Path,
    overrides: &[ConfigOverride],
) -> Result<(Config, bool), String> {
    ensure_private_server_dir(dir)?;
    let config_path = dir.join(SERVER_CONFIG_FILE_NAME);
    let initialized = if config_path
        .try_exists()
        .map_err(|error| format!("failed to inspect {}: {error}", config_path.display()))?
    {
        false
    } else {
        write_generated_template(&config_path)?;
        true
    };
    let config = Config::load_with_overrides(&config_path, overrides)?;
    Ok((config, initialized))
}

fn ensure_private_server_dir(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("server directory must not be empty".to_string());
    }
    fs::create_dir_all(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let dir = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)
            .map_err(|error| format!("failed to open directory {}: {error}", path.display()))?;
        let metadata = dir
            .metadata()
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid {
            return Err(format!("{} is not owned by uid {uid}", path.display()));
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        dir.set_permissions(permissions)
            .map_err(|error| format!("failed to chmod {}: {error}", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if !metadata.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
    }

    Ok(())
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
# Generated by `chatt-server init-config` or `chatt-server serve --dir`.
# Keep this file private: it contains the server identity seed that
# authenticates handshakes and dynamic tokens.
#
# The server never rewrites this file. Runtime state (the user registry, the
# DM room registry, message logs) lives under storage.data-dir.

[network]
# Bind addresses on this host.
bind = "{listen_addr}"
# To use different transport addresses, replace `bind` above with:
# bind.tcp = "127.0.0.1:41000"
# bind.udp = "127.0.0.1:41000"
# Optional UDP socket used for P2P path probes.
# udp-probe-addr = "127.0.0.1:41001"

# Public endpoints embedded in invites and returned during open pairing. Set
# these when clients need a DNS name, reverse proxy port, or forwarded NAT port.
# public-addr = "chat.example.com:41000"
# To advertise different transport endpoints, use:
# public-addr.tcp = "chat.example.com:41000"
# public-addr.udp = "media.example.com:41000"
# public-udp-probe-addr = "chat.example.com:41001"
p2p = true

[security]
server-identity-seed = "{seed}"
# Whether Chatt encrypts control, media, video, and file transport payloads.
# Disabling this sends those payloads in plaintext after the signed handshake
# and disables P2P.
transport-encryption = true
# Maximum relayed file size, in MiB.
max-file-size-mb = {max_file_size_mb}
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
# mls-retention-days = 90
# mls-cleanup-interval-minutes = 15
# mls-cleanup-batch-events = 4096
# mls-compaction-min-interval-hours = 24
# mls-compaction-min-fragmented-mib = 256
# mls-compaction-min-fragmented-percent = 25

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
# Optional MLS retention override for this room.
# mls-retention-days = 30
# Clients drop into the default room on connect. At most one room; when
# omitted the lowest-id public room is the default.
default = true

# Create invite users with:
#   chatt-server invite USER
# Accepted invites are recorded in the user registry under storage.data-dir.
"#,
        listen_addr = DEFAULT_LISTEN_ADDR,
        seed = seed,
        max_file_size_mb = DEFAULT_FILE_SIZE_LIMIT_MB,
    ))
}

impl SecurityConfig {
    pub fn max_file_size_bytes(&self) -> u64 {
        self.max_file_size_mb.checked_mul(MIB).unwrap_or(u64::MAX)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        Self::load_with_overrides(path, &[])
    }

    pub(crate) fn load_with_overrides(
        path: &Path,
        overrides: &[ConfigOverride],
    ) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let source = path.display().to_string();
        let outcome = collect_config_content_with_overrides(
            &content,
            &source,
            Some(path.to_path_buf()),
            overrides,
        );
        config_diagnostics::render(&source, &content, &outcome.diagnostics);
        let errors = outcome
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.error)
            .count();
        match outcome.config {
            Some(config) if errors == 0 => Ok(config),
            _ => Err(format!(
                "invalid configuration: {errors} error(s) in {source}"
            )),
        }
    }

    pub fn server_key_pair(&self) -> Result<aws_lc_rs::signature::Ed25519KeyPair, String> {
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

    /// The wire mode selected by `security.transport-encryption`.
    pub fn transport_mode(&self) -> rpc::crypto::TransportMode {
        if self.security.transport_encryption {
            rpc::crypto::TransportMode::Encrypted
        } else {
            rpc::crypto::TransportMode::Plaintext
        }
    }

    pub(crate) fn normalize(&mut self) {
        // P2P transport is never available when transport encryption is off.
        if !self.security.transport_encryption {
            self.network.p2p = false;
        }
        self.network.public_addr.tcp = self.network.public_addr.tcp.trim().to_string();
        if self.network.public_addr.tcp.is_empty() {
            self.network.public_addr.tcp = self.network.bind.tcp.to_string();
        }
        self.network.public_addr.udp = self.network.public_addr.udp.trim().to_string();
        if self.network.public_addr.udp.is_empty() {
            self.network.public_addr.udp =
                public_endpoint_for_bind_addr(self.network.bind.udp, &self.network.public_addr.tcp);
        }
        let public_udp_probe_addr = self
            .network
            .public_udp_probe_addr
            .as_deref()
            .map(str::trim)
            .filter(|addr| !addr.is_empty())
            .map(str::to_string);
        self.network.public_udp_probe_addr = if self.network.public_udp_probe_addr_overridden {
            public_udp_probe_addr
        } else {
            public_udp_probe_addr.or_else(|| {
                self.network
                    .udp_probe_addr
                    .map(|addr| public_endpoint_for_bind_addr(addr, &self.network.public_addr.tcp))
            })
        };
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
        validate_public_endpoint(
            source,
            "network.public-addr.tcp",
            &self.network.public_addr.tcp,
        )?;
        validate_public_endpoint(
            source,
            "network.public-addr.udp",
            &self.network.public_addr.udp,
        )?;
        if let Some(addr) = &self.network.public_udp_probe_addr {
            validate_public_endpoint(source, "network.public-udp-probe-addr", addr)?;
        }
        if !(1..=3650).contains(&self.storage.mls_retention_days) {
            return Err(format!(
                "{source}: storage.mls-retention-days must be between 1 and 3650"
            ));
        }
        if self.storage.mls_cleanup_interval_minutes == 0
            || self.storage.mls_cleanup_batch_events == 0
            || self.storage.mls_compaction_min_interval_hours == 0
            || self.storage.mls_compaction_min_fragmented_percent > 100
        {
            return Err(format!("{source}: invalid MLS storage maintenance limits"));
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
            if room
                .mls_retention_days
                .is_some_and(|days| !(1..=3650).contains(&days))
            {
                return Err(format!(
                    "{source}: room {} mls-retention-days must be between 1 and 3650",
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
    let outcome = collect_config_content(content, source, config_path);
    let errors = outcome
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.error)
        .count();
    match outcome.config {
        Some(config) if errors == 0 => Ok(config),
        _ => Err(config_diagnostics::render_to_string(
            source,
            content,
            &outcome.diagnostics,
        )),
    }
}

struct LoadOutcome {
    config: Option<Config>,
    diagnostics: Vec<Diag>,
}

fn collect_config_content(
    content: &str,
    source: &str,
    config_path: Option<PathBuf>,
) -> LoadOutcome {
    collect_config_content_with_overrides(content, source, config_path, &[])
}

fn collect_config_content_with_overrides(
    content: &str,
    source: &str,
    config_path: Option<PathBuf>,
    overrides: &[ConfigOverride],
) -> LoadOutcome {
    let arena = toml_spanner::Arena::new();
    let mut doc = toml_spanner::parse_recoverable(content, &arena);
    let mut diagnostics = Vec::new();
    let identity_seed_configured = section_contains_key(&doc, "security", "server-identity-seed");
    let (config, from_toml) = match doc.to_allowing_errors::<Config>() {
        Ok((config, errors)) => (Some(config), errors),
        Err(errors) => (None, errors),
    };
    diagnostics.extend(
        from_toml
            .errors
            .iter()
            .map(|error| config_diagnostics::from_toml_error(error, content)),
    );
    if !identity_seed_configured && !diagnostics.iter().any(|diagnostic| diagnostic.error) {
        diagnostics.push(Diag::error(format!(
            "{source}: security.server-identity-seed is required; run `chatt-server init-config PATH` or use `chatt-server serve --dir DIR` to generate a private server config"
        )));
    }
    let config = config.map(|mut config| {
        for config_override in overrides {
            let spec = config_option_spec(config_override.name())
                .expect("ConfigOverride names come from CONFIG_OPTION_SPECS");
            if let Err(error) = (spec.apply)(&mut config, &config_override.value) {
                diagnostics.push(Diag::error(error));
            }
        }
        config.config_path = config_path;
        config.normalize();
        if let Err(error) = config.validate(source) {
            diagnostics.push(Diag::error(error));
        }
        config
    });
    LoadOutcome {
        config,
        diagnostics,
    }
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

fn validate_public_endpoint(source: &str, name: &str, value: &str) -> Result<(), String> {
    validate_endpoint(source, name, value)?;
    if let Ok(addr) = value.trim().parse::<SocketAddr>()
        && addr.ip().is_unspecified()
    {
        return Err(format!(
            "{source}: {name} must not use an unspecified address"
        ));
    }
    Ok(())
}

fn public_endpoint_for_bind_addr(bind_addr: SocketAddr, public_tcp_addr: &str) -> String {
    if bind_addr.ip().is_unspecified() {
        return endpoint_with_port(public_tcp_addr, bind_addr.port())
            .unwrap_or_else(|| bind_addr.to_string());
    }
    bind_addr.to_string()
}

fn endpoint_with_port(endpoint: &str, port: u16) -> Option<String> {
    let endpoint = endpoint.trim();
    if let Ok(addr) = endpoint.parse::<SocketAddr>() {
        return Some(SocketAddr::new(addr.ip(), port).to_string());
    }
    let (host, _) = endpoint.rsplit_once(':')?;
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    Some(format!("{host}:{port}"))
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
        mls_retention_days: None,
        is_default: true,
    }]
}

fn generate_identity_seed_hex() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    aws_lc_rs::rand::SystemRandom::new()
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
            "[network]\nbind = \"127.0.0.1:41000\"\n\n[security]\nserver-identity-seed = \"{}\"\n\n{extra}",
            dev_server_seed_hex()
        )
    }

    fn config_override(name: &str, value: impl Into<String>) -> ConfigOverride {
        ConfigOverride::new(name, value.into()).unwrap()
    }

    #[test]
    fn default_config_parses_and_validates() {
        let config = Config::default();
        config.validate("<test>").unwrap();
        assert_eq!(config.network.bind.tcp.to_string(), "127.0.0.1:41000");
        assert_eq!(config.network.bind.udp, config.network.bind.tcp);
        assert_eq!(config.network.udp_probe_addr, None);
        assert_eq!(config.network.public_addr.tcp, "127.0.0.1:41000");
        assert_eq!(config.network.public_addr.udp, "127.0.0.1:41000");
        assert_eq!(config.network.public_udp_probe_addr, None);
        assert!(config.network.p2p);
        assert!(config.security.transport_encryption);
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
        assert_eq!(config.network.bind.udp, config.network.bind.tcp);
    }

    #[test]
    fn option_registry_covers_every_cli_overridable_config_field() {
        assert_eq!(
            CONFIG_OPTION_SPECS
                .iter()
                .map(|spec| spec.name)
                .collect::<Vec<_>>(),
            [
                "network.bind",
                "network.bind.tcp",
                "network.bind.udp",
                "network.udp-probe-addr",
                "network.public-addr",
                "network.public-addr.tcp",
                "network.public-addr.udp",
                "network.public-udp-probe-addr",
                "network.p2p",
                "security.transport-encryption",
                "security.max-file-size-mb",
                "security.bug-report-dir",
                "security.public",
                "security.password-hash",
                "security.password-epoch",
                "storage.data-dir",
                "storage.mls-retention-days",
                "storage.mls-cleanup-interval-minutes",
                "storage.mls-cleanup-batch-events",
                "storage.mls-compaction-min-interval-hours",
                "storage.mls-compaction-min-fragmented-mib",
                "storage.mls-compaction-min-fragmented-percent",
            ]
        );
    }

    #[test]
    fn scalar_overrides_apply_to_every_registered_field() {
        let password_hash = hash_secret("override-password");
        let overrides = vec![
            config_override("network.bind", "127.0.0.1:41999"),
            config_override("network.bind.tcp", "127.0.0.1:42000"),
            config_override("network.bind.udp", "127.0.0.1:42001"),
            config_override("network.udp-probe-addr", "127.0.0.1:42002"),
            config_override("network.public-addr", "chat.example.com:442"),
            config_override("network.public-addr.tcp", "chat.example.com:443"),
            config_override("network.public-addr.udp", "chat.example.com:444"),
            config_override("network.public-udp-probe-addr", "chat.example.com:445"),
            config_override("network.p2p", "false"),
            config_override("security.transport-encryption", "true"),
            config_override("security.max-file-size-mb", "12"),
            config_override("security.bug-report-dir", "/tmp/chatt-bugs"),
            config_override("security.public", "true"),
            config_override("security.password-hash", &password_hash),
            config_override("security.password-epoch", "9"),
            config_override("storage.data-dir", "/tmp/chatt-data"),
            config_override("storage.mls-retention-days", "120"),
            config_override("storage.mls-cleanup-interval-minutes", "10"),
            config_override("storage.mls-cleanup-batch-events", "100"),
            config_override("storage.mls-compaction-min-interval-hours", "12"),
            config_override("storage.mls-compaction-min-fragmented-mib", "64"),
            config_override("storage.mls-compaction-min-fragmented-percent", "15"),
        ];

        let outcome = collect_config_content_with_overrides(
            &config_content(""),
            "<test>",
            Some(PathBuf::from("server.toml")),
            &overrides,
        );
        assert!(
            !outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.error),
            "{:?}",
            outcome
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
        );
        let config = outcome.config.unwrap();

        assert_eq!(config.network.bind.tcp.to_string(), "127.0.0.1:42000");
        assert_eq!(config.network.bind.udp.to_string(), "127.0.0.1:42001");
        assert_eq!(
            config.network.udp_probe_addr.map(|addr| addr.to_string()),
            Some("127.0.0.1:42002".to_string())
        );
        assert_eq!(config.network.public_addr.tcp, "chat.example.com:443");
        assert_eq!(config.network.public_addr.udp, "chat.example.com:444");
        assert_eq!(
            config.network.public_udp_probe_addr.as_deref(),
            Some("chat.example.com:445")
        );
        assert!(!config.network.p2p);
        assert_eq!(config.security.server_identity_seed, dev_server_seed_hex());
        assert!(config.security.transport_encryption);
        assert_eq!(config.security.max_file_size_mb, 12);
        assert_eq!(
            config.security.bug_report_dir.as_deref(),
            Some("/tmp/chatt-bugs")
        );
        assert!(config.security.public);
        assert_eq!(
            config.security.password_hash.as_deref(),
            Some(password_hash.as_str())
        );
        assert_eq!(config.security.password_epoch, 9);
        assert_eq!(config.storage.data_dir.as_deref(), Some("/tmp/chatt-data"));
        assert_eq!(config.storage.mls_retention_days, 120);
        assert_eq!(config.storage.mls_cleanup_interval_minutes, 10);
        assert_eq!(config.storage.mls_cleanup_batch_events, 100);
        assert_eq!(config.storage.mls_compaction_min_interval_hours, 12);
        assert_eq!(config.storage.mls_compaction_min_fragmented_mib, 64);
        assert_eq!(config.storage.mls_compaction_min_fragmented_percent, 15);
    }

    #[test]
    fn overrides_precede_normalization_and_last_value_wins() {
        let overrides = vec![
            config_override("network.bind", "127.0.0.1:41999"),
            config_override("network.bind", "127.0.0.1:42000"),
        ];

        let outcome = collect_config_content_with_overrides(
            &config_content(""),
            "<test>",
            Some(PathBuf::from("server.toml")),
            &overrides,
        );
        let config = outcome.config.unwrap();

        assert_eq!(config.network.bind.tcp.to_string(), "127.0.0.1:42000");
        assert_eq!(config.network.bind.udp.to_string(), "127.0.0.1:42000");
        assert_eq!(config.network.public_addr.tcp, "127.0.0.1:42000");
        assert_eq!(config.network.public_addr.udp, "127.0.0.1:42000");
    }

    #[test]
    fn override_supplies_bind_omitted_from_partial_network_section() {
        let content = format!(
            "[network]\np2p = false\n\n[security]\nserver-identity-seed = \"{}\"\n",
            dev_server_seed_hex()
        );
        let overrides = [config_override("network.bind", "127.0.0.1:42000")];

        let outcome = collect_config_content_with_overrides(
            &content,
            "<test>",
            Some(PathBuf::from("server.toml")),
            &overrides,
        );

        assert!(
            !outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.error),
            "{:?}",
            outcome
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
        );
        let config = outcome.config.unwrap();
        assert_eq!(config.network.bind.tcp.to_string(), "127.0.0.1:42000");
        assert_eq!(config.network.bind.udp.to_string(), "127.0.0.1:42000");
        assert!(!config.network.p2p);
    }

    #[test]
    fn transport_specific_override_changes_only_that_bind() {
        let overrides = [config_override("network.bind.udp", "127.0.0.1:42001")];

        let outcome = collect_config_content_with_overrides(
            &config_content(""),
            "<test>",
            Some(PathBuf::from("server.toml")),
            &overrides,
        );
        let config = outcome.config.unwrap();

        assert_eq!(config.network.bind.tcp.to_string(), "127.0.0.1:41000");
        assert_eq!(config.network.bind.udp.to_string(), "127.0.0.1:42001");
    }

    #[test]
    fn empty_public_udp_probe_override_remains_disabled_after_normalization() {
        let content = config_content("").replace(
            "[network]",
            "[network]\nudp-probe-addr = \"127.0.0.1:42002\"\npublic-udp-probe-addr = \"chat.example.com:445\"",
        );
        let overrides = [config_override("network.public-udp-probe-addr", "")];

        let outcome = collect_config_content_with_overrides(
            &content,
            "<test>",
            Some(PathBuf::from("server.toml")),
            &overrides,
        );
        let mut config = outcome.config.unwrap();

        assert_eq!(config.network.public_udp_probe_addr, None);
        config.normalize();
        assert_eq!(config.network.public_udp_probe_addr, None);
    }

    #[test]
    fn directory_flow_initializes_once_and_does_not_persist_overrides() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("instance");

        let (first, initialized) = load_or_initialize_directory(&dir, &[]).unwrap();
        assert!(initialized);
        let config_path = dir.join(SERVER_CONFIG_FILE_NAME);
        let original = fs::read_to_string(&config_path).unwrap();
        let seed = first.security.server_identity_seed.clone();
        assert_eq!(first.config_path.as_deref(), Some(config_path.as_path()));
        assert_eq!(first.data_dir(), Some(dir.join("chatt-server-data")));

        let overrides = [config_override("network.bind", "127.0.0.1:42000")];
        let (second, initialized) = load_or_initialize_directory(&dir, &overrides).unwrap();

        assert!(!initialized);
        assert_eq!(second.security.server_identity_seed, seed);
        assert_eq!(second.network.bind.tcp.to_string(), "127.0.0.1:42000");
        assert_eq!(second.network.bind.udp.to_string(), "127.0.0.1:42000");
        assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
    }

    #[test]
    fn directory_flow_does_not_replace_invalid_existing_config() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("instance");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SERVER_CONFIG_FILE_NAME);
        fs::write(&path, "not valid toml = [").unwrap();

        let error = load_or_initialize_directory(&dir, &[]).unwrap_err();

        assert!(error.contains("invalid configuration"));
        assert_eq!(fs::read_to_string(path).unwrap(), "not valid toml = [");
    }

    #[cfg(unix)]
    #[test]
    fn directory_flow_uses_private_modes() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("instance");

        load_or_initialize_directory(&dir, &[]).unwrap();

        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let config_mode = fs::metadata(dir.join(SERVER_CONFIG_FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(config_mode, 0o600);
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
        let content = "[network]\nbind = \"127.0.0.1:41000\"\n";

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
        let content = config_content("").replace("[network]", "[network]\np2p = false");

        let config = parse(&content).unwrap();

        assert!(!config.network.p2p);
    }

    #[test]
    fn disabled_transport_encryption_forces_p2p_off() {
        // Even with p2p = true, plaintext transport disables P2P.
        let content = config_content("")
            .replace("[network]", "[network]\np2p = true")
            .replace("[security]", "[security]\ntransport-encryption = false");

        let config = parse(&content).unwrap();

        assert!(!config.security.transport_encryption);
        assert!(!config.network.p2p);
    }

    #[test]
    fn scalar_bind_populates_both_transports() {
        let config = parse(&config_content("")).unwrap();

        assert_eq!(config.network.bind.tcp.to_string(), "127.0.0.1:41000");
        assert_eq!(config.network.bind.udp, config.network.bind.tcp);
        assert_eq!(config.network.udp_probe_addr, None);
    }

    #[test]
    fn parses_transport_specific_binds_and_probe_addr() {
        let content = config_content("").replace(
            "bind = \"127.0.0.1:41000\"",
            "bind.tcp = \"127.0.0.1:42000\"\nbind.udp = \"127.0.0.1:42001\"\nudp-probe-addr = \"127.0.0.1:42002\"",
        );

        let config = parse(&content).unwrap();

        assert_eq!(config.network.bind.tcp.to_string(), "127.0.0.1:42000");
        assert_eq!(config.network.bind.udp.to_string(), "127.0.0.1:42001");
        assert_eq!(
            config.network.udp_probe_addr.map(|addr| addr.to_string()),
            Some("127.0.0.1:42002".to_string())
        );
    }

    #[test]
    fn public_endpoints_can_differ_from_bind_addresses() {
        let content = config_content("").replace(
            "bind = \"127.0.0.1:41000\"",
            "bind = \"0.0.0.0:41000\"\npublic-addr.tcp = \"chat.example.com:443\"\npublic-addr.udp = \"198.51.100.20:54100\"",
        );

        let config = parse(&content).unwrap();

        assert_eq!(config.network.bind.tcp.to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.bind.udp.to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.public_addr.tcp, "chat.example.com:443");
        assert_eq!(config.network.public_addr.udp, "198.51.100.20:54100");
    }

    #[test]
    fn scalar_public_addr_populates_both_transports() {
        let content = config_content("").replace(
            "bind = \"127.0.0.1:41000\"",
            "bind = \"0.0.0.0:41000\"\npublic-addr = \"104.247.224.7:41000\"",
        );

        let config = parse(&content).unwrap();

        assert_eq!(config.network.bind.tcp.to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.bind.udp.to_string(), "0.0.0.0:41000");
        assert_eq!(config.network.public_addr.tcp, "104.247.224.7:41000");
        assert_eq!(config.network.public_addr.udp, "104.247.224.7:41000");
    }

    #[test]
    fn public_endpoints_reject_unspecified_addresses() {
        let content =
            config_content("").replace("bind = \"127.0.0.1:41000\"", "bind = \"0.0.0.0:41000\"");

        let error = parse(&content).unwrap_err();

        assert!(error.contains("network.public-addr.tcp"));
        assert!(error.contains("unspecified address"));
    }

    #[test]
    fn explicit_public_udp_addr_rejects_unspecified_address() {
        let content = config_content("").replace(
            "bind = \"127.0.0.1:41000\"",
            "bind = \"0.0.0.0:41000\"\npublic-addr.tcp = \"104.247.224.7:41000\"\npublic-addr.udp = \"0.0.0.0:41000\"",
        );

        let error = parse(&content).unwrap_err();

        assert!(error.contains("network.public-addr.udp"));
        assert!(error.contains("unspecified address"));
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

        assert!(error.contains("invalid configuration"));
        assert_eq!(content, "this is not valid toml = [");
        assert!(!corrupt_exists);
    }

    #[test]
    fn parse_errors_are_rendered_as_annotated_snippets() {
        let content = "[network]\nbind = 42\n";
        let outcome = collect_config_content(content, "server.toml", None);
        let rendered =
            config_diagnostics::render_to_string("server.toml", content, &outcome.diagnostics);

        assert!(rendered.contains("server.toml:2"), "{rendered}");
        assert!(rendered.contains("bind = 42"), "{rendered}");
        assert!(rendered.contains("error:"), "{rendered}");
    }

    #[test]
    fn unknown_keys_are_warnings_and_do_not_reject_the_config() {
        let content = config_content("unknown-setting = true\n");
        let outcome = collect_config_content(&content, "server.toml", None);

        assert!(outcome.config.is_some());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diagnostic| !diagnostic.error)
        );
        assert!(
            !outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.error)
        );
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
    fn mls_retention_defaults_and_room_override_parse() {
        let config = parse(&config_content(
            "[storage]\nmls-retention-days = 120\nmls-cleanup-batch-events = 17\n\n\
             [[rooms]]\nid = 1\nname = \"lobby\"\nmls-retention-days = 30\n",
        ))
        .unwrap();
        assert_eq!(config.storage.mls_retention_days, 120);
        assert_eq!(config.storage.mls_cleanup_batch_events, 17);
        assert_eq!(config.rooms[0].mls_retention_days, Some(30));

        let defaults = Config::default();
        assert_eq!(defaults.storage.mls_retention_days, 90);
        assert_eq!(defaults.storage.mls_cleanup_interval_minutes, 15);
    }

    #[test]
    fn zero_mls_retention_is_rejected_globally_and_per_room() {
        let global = parse(&config_content(
            "[storage]\nmls-retention-days = 0\n\n[[rooms]]\nid = 1\nname = \"lobby\"\n",
        ))
        .unwrap_err();
        assert!(global.contains("between 1 and 3650"));

        let room = parse(&config_content(
            "[[rooms]]\nid = 1\nname = \"lobby\"\nmls-retention-days = 0\n",
        ))
        .unwrap_err();
        assert!(room.contains("between 1 and 3650"));
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
