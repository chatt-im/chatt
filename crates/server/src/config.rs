use hashbrown::HashSet;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use ring::{digest, signature::KeyPair};
use rpc::{
    control::DEFAULT_FILE_SIZE_LIMIT_BYTES,
    crypto::{encode_hex, server_key_pair_from_seed_hex},
    ids::{RoomId, UserId},
};
use toml_spanner::{Item, Toml};

pub const DEFAULT_SERVER_CONFIG: &str = include_str!("../../../chatt-server.toml");
const SECRET_HASH_PREFIX: &str = "sha256:";
const SHA256_HEX_LEN: usize = 64;

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
    #[toml(default)]
    pub chat_history_limit: u64,
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_BYTES)]
    pub max_file_size_bytes: u64,
    /// Directory where `/report-bug` bundles are saved. Bug reports are rejected
    /// when unset.
    #[toml(default)]
    pub bug_report_dir: Option<String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            server_identity_seed:
                "546f6d636861742064657620736572766572206b657920763100000000000001".to_string(),
            encryption: true,
            chat_history_limit: 0,
            max_file_size_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
            bug_report_dir: None,
        }
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct RoomConfig {
    pub id: u32,
    pub name: String,
}

impl RoomConfig {
    pub fn room_id(&self) -> RoomId {
        RoomId(self.id)
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct UserConfig {
    pub id: u32,
    pub name: String,
    #[toml(default)]
    pub display_name: String,
    #[toml(default)]
    pub token_hash: String,
}

impl UserConfig {
    pub fn user_id(&self) -> UserId {
        UserId(self.id)
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, recoverable, warn_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[toml(default)]
    pub network: NetworkConfig,
    #[toml(default)]
    pub security: SecurityConfig,
    #[toml(default = default_rooms())]
    pub rooms: Vec<RoomConfig>,
    #[toml(default = default_users())]
    pub users: Vec<UserConfig>,
    #[toml(skip)]
    pub config_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let arena = toml_spanner::Arena::new();
        let mut doc = toml_spanner::parse(DEFAULT_SERVER_CONFIG, &arena)
            .expect("embedded chatt server config must parse");
        let mut config: Self = doc
            .to()
            .expect("embedded chatt server config must deserialize");
        config.apply_inferred_addresses(false, false);
        config.normalize();
        config
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self, String> {
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CHATT_SERVER_CONFIG").map(PathBuf::from))
            .or_else(default_config_path);

        let (content, source) = if let Some(path) = &config_path {
            let content = fs::read_to_string(path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            (content, path.display().to_string())
        } else {
            (
                DEFAULT_SERVER_CONFIG.to_string(),
                "<embedded default>".to_string(),
            )
        };

        match parse_config_content(&content, &source, config_path.clone()) {
            Ok(config) => Ok(config),
            Err(error) => {
                let Some(path) = config_path.as_deref() else {
                    return Err(error);
                };
                recover_config_from_backup(path, &content, &error)
            }
        }
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
            .map(|user| user.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "no user ids are available".to_string())?;
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
        if self.users.is_empty() {
            return Err(format!("{source}: at least one user is required"));
        }
        if self.security.max_file_size_bytes > DEFAULT_FILE_SIZE_LIMIT_BYTES {
            return Err(format!(
                "{source}: security.max-file-size-bytes exceeds protocol maximum"
            ));
        }
        server_key_pair_from_seed_hex(&self.security.server_identity_seed)
            .map_err(|error| format!("{source}: invalid security.server-identity-seed: {error}"))?;
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
        for room in &self.rooms {
            if room.id == 0 {
                return Err(format!("{source}: room id must be non-zero"));
            }
            if room.name.trim().is_empty() {
                return Err(format!("{source}: room name must not be empty"));
            }
            if !room_ids.insert(room.id) {
                return Err(format!("{source}: duplicate room id {}", room.id));
            }
        }
        if !room_ids.contains(&1) {
            return Err(format!(
                "{source}: room id 1 is required as the default lobby"
            ));
        }

        let mut user_ids = HashSet::new();
        let mut user_names = HashSet::new();
        for user in &self.users {
            if user.id == 0 {
                return Err(format!("{source}: user id must be non-zero"));
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
        atomic_write_config(path, &self.to_toml_string(), true)
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
            "chat-history-limit = {}\n",
            self.security.chat_history_limit
        ));
        out.push_str(&format!(
            "max-file-size-bytes = {}\n",
            self.security.max_file_size_bytes
        ));
        if let Some(dir) = &self.security.bug_report_dir {
            out.push_str(&format!("bug-report-dir = \"{}\"\n", toml_quote_value(dir)));
        }
        out.push('\n');
        for room in &self.rooms {
            out.push_str("[[rooms]]\n");
            out.push_str(&format!("id = {}\n", room.id));
            out.push_str(&format!("name = \"{}\"\n\n", toml_quote_value(&room.name)));
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
    if config_path.is_some() && !doc.table().contains_key("users") {
        return Err(format!(
            "{source}: persisted server config must contain a users section"
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

fn atomic_write_config(path: &Path, content: &str, backup_existing: bool) -> Result<(), String> {
    let tmp = temp_config_path(path);
    let bak = backup_config_path(path);
    if backup_existing && path.exists() {
        fs::copy(path, &bak).map_err(|err| {
            format!(
                "failed to back up {} to {}: {err}",
                path.display(),
                bak.display()
            )
        })?;
    }

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

fn recover_config_from_backup(
    path: &Path,
    corrupt_content: &str,
    load_error: &str,
) -> Result<Config, String> {
    let corrupt_path = preserve_corrupt_config(path, corrupt_content)?;
    let bak = backup_config_path(path);
    let backup_content = fs::read_to_string(&bak).map_err(|err| {
        format!(
            "{load_error}; preserved corrupt config as {}; no usable backup at {}: {err}",
            corrupt_path.display(),
            bak.display()
        )
    })?;
    let config = parse_config_content(
        &backup_content,
        &bak.display().to_string(),
        Some(path.to_path_buf()),
    )
    .map_err(|backup_error| {
        format!(
            "{load_error}; preserved corrupt config as {}; backup {} is not usable: {backup_error}",
            corrupt_path.display(),
            bak.display()
        )
    })?;
    atomic_write_config(path, &backup_content, false)?;
    kvlog::warn!(
        "server config restored from backup",
        path = %path.display(),
        backup = %bak.display(),
        corrupt = %corrupt_path.display(),
        error = load_error
    );
    Ok(config)
}

fn preserve_corrupt_config(path: &Path, content: &str) -> Result<PathBuf, String> {
    let corrupt_path = next_corrupt_config_path(path);
    fs::write(&corrupt_path, content).map_err(|err| {
        format!(
            "failed to preserve corrupt config {} as {}: {err}",
            path.display(),
            corrupt_path.display()
        )
    })?;
    Ok(corrupt_path)
}

fn temp_config_path(path: &Path) -> PathBuf {
    extension_path(path, "tmp")
}

fn backup_config_path(path: &Path) -> PathBuf {
    extension_path(path, "bak")
}

fn next_corrupt_config_path(path: &Path) -> PathBuf {
    let first = extension_path(path, "corrupt");
    if !first.exists() {
        return first;
    }
    for index in 1.. {
        let candidate = extension_path(path, &format!("corrupt.{index}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("unbounded corrupt config suffix search must return")
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

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
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

fn default_config_path() -> Option<PathBuf> {
    let explicit = std::env::var_os("CHATT_SERVER_CONFIG").map(PathBuf::from);
    if explicit.is_some() {
        return explicit;
    }
    let local = PathBuf::from("chatt-server.toml");
    local.exists().then_some(local)
}

fn default_shared_addr() -> SocketAddr {
    "127.0.0.1:41000"
        .parse()
        .expect("valid default network addr")
}

fn network_contains_key(doc: &toml_spanner::Document<'_>, key: &str) -> bool {
    doc.table()
        .get("network")
        .and_then(Item::as_table)
        .is_some_and(|network| network.contains_key(key))
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
    }]
}

fn default_users() -> Vec<UserConfig> {
    vec![
        UserConfig {
            id: 1,
            name: "alice".to_string(),
            display_name: "Alice".to_string(),
            token_hash: "sha256:8989ee243a8829e3364b6eb9cae74f1edfa53e327e5718fe337d23d6ab8b4625"
                .to_string(),
        },
        UserConfig {
            id: 2,
            name: "bob".to_string(),
            display_name: "Bob".to_string(),
            token_hash: "sha256:fd57e39ef28a47298ae51762fd3c907253ac7d4b7507fdf28fcf6d5e4bbaeb8a"
                .to_string(),
        },
        UserConfig {
            id: 3,
            name: "carol".to_string(),
            display_name: "Carol".to_string(),
            token_hash: "sha256:f93be296b384d5f0f8760bdb53cf3ea52a5e41e6f7a532645458784ece411f62"
                .to_string(),
        },
    ]
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
        assert_eq!(config.security.chat_history_limit, 0);
        assert_eq!(config.rooms[0].room_id(), RoomId(1));
    }

    #[test]
    fn config_parses_p2p_disabled() {
        let arena = toml_spanner::Arena::new();
        let content = DEFAULT_SERVER_CONFIG.replace("p2p-enabled = true", "p2p-enabled = false");
        let mut doc = toml_spanner::parse(&content, &arena).unwrap();
        let config: Config = doc.to().unwrap();

        assert!(!config.network.p2p_enabled);
    }

    #[test]
    fn udp_addr_inherits_tcp_addr_when_omitted() {
        let arena = toml_spanner::Arena::new();
        let mut doc = toml_spanner::parse(DEFAULT_SERVER_CONFIG, &arena).unwrap();
        let udp_addr_configured = network_contains_key(&doc, "udp-addr");
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses(udp_addr_configured, false);

        assert_eq!(config.network.udp_addr, config.network.tcp_addr);
        assert_eq!(config.network.udp_probe_addr, None);
    }

    #[test]
    fn parses_explicit_udp_and_probe_addrs() {
        let arena = toml_spanner::Arena::new();
        let content = DEFAULT_SERVER_CONFIG.replace(
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
        let content = DEFAULT_SERVER_CONFIG.replace(
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
        let mut config = Config::default();
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
        let mut config = Config::default();
        config.config_path = Some(path.clone());

        let token_hash = hash_secret("client-generated-token-with-at-least-32-bytes");
        let user = config
            .mark_user_paired("billy", "Billy".to_string(), token_hash.clone())
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(user.id, 4);
        assert_eq!(user.name, "billy");
        assert_eq!(user.display_name, "Billy");
        assert_eq!(user.token_hash, token_hash);
        assert!(content.contains("name = \"billy\""));
        assert!(content.contains("display-name = \"Billy\""));
        assert!(content.contains(&format!("token-hash = \"{token_hash}\"")));
    }

    #[test]
    fn save_runtime_keeps_backup_of_previous_config() {
        let path = std::env::temp_dir().join(format!(
            "chatt-server-backup-save-test-{}.toml",
            std::process::id()
        ));
        let bak = backup_config_path(&path);
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
        let backup = std::fs::read_to_string(&bak).unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&bak);
        let _ = std::fs::remove_file(&tmp);

        assert!(current.contains("display-name = \"Alice Two\""));
        assert!(current.contains(&format!("token-hash = \"{second_hash}\"")));
        assert!(backup.contains("display-name = \"Alice One\""));
        assert!(backup.contains(&format!("token-hash = \"{first_hash}\"")));
        assert!(!tmp.exists());
    }

    #[test]
    fn corrupt_config_restores_from_backup_without_demo_user_fallback() {
        let path = std::env::temp_dir().join(format!(
            "chatt-server-corrupt-restore-test-{}.toml",
            std::process::id()
        ));
        let bak = backup_config_path(&path);
        let corrupt = extension_path(&path, "corrupt");
        let mut config = Config::default();
        config.config_path = Some(path.clone());
        config.users = vec![UserConfig {
            id: 9,
            name: "paired-user".to_string(),
            display_name: "Paired User".to_string(),
            token_hash: hash_secret("paired-client-generated-token-with-at-least-32-bytes"),
        }];
        let backup_content = config.to_toml_string();
        std::fs::write(&bak, &backup_content).unwrap();
        std::fs::write(&path, "this is not valid toml = [").unwrap();

        let restored = Config::load(Some(path.to_str().unwrap())).unwrap();
        let restored_content = std::fs::read_to_string(&path).unwrap();
        let corrupt_content = std::fs::read_to_string(&corrupt).unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&bak);
        let _ = std::fs::remove_file(&corrupt);

        assert_eq!(restored.users.len(), 1);
        assert_eq!(restored.users[0].name, "paired-user");
        assert!(restored_content.contains("name = \"paired-user\""));
        assert!(corrupt_content.contains("this is not valid toml"));
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

    #[test]
    fn persisted_config_without_users_is_rejected() {
        let content = Config::default().to_toml_string();
        let content = content
            .split("[[users]]")
            .next()
            .expect("default config has users")
            .to_string();
        let error = parse_config_content(
            &content,
            "<persisted>",
            Some(PathBuf::from("chatt-server.toml")),
        )
        .unwrap_err();

        assert!(error.contains("users section"));
    }
}
