use std::{collections::HashSet, fs, net::SocketAddr, path::PathBuf};

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
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            server_identity_seed:
                "546f6d636861742064657620736572766572206b657920763100000000000001".to_string(),
            encryption: true,
            chat_history_limit: 0,
            max_file_size_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
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

        let arena = toml_spanner::Arena::new();
        let mut doc = toml_spanner::parse(&content, &arena)
            .map_err(|err| format!("failed to parse {source}: {err}"))?;
        reject_deprecated_config_keys(&doc, &source)?;
        let udp_addr_configured = network_contains_key(&doc, "udp-addr");
        let mut config: Config = doc.to().map_err(|err| {
            let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
            format!("failed to deserialize {source}: {}", errors.join(", "))
        })?;
        config.config_path = config_path;
        config.apply_inferred_addresses(udp_addr_configured, false);
        config.normalize();
        config.validate(&source)?;
        Ok(config)
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
        fs::write(path, self.to_toml_string())
            .map_err(|err| format!("failed to write {}: {err}", path.display()))
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
            "max-file-size-bytes = {}\n\n",
            self.security.max_file_size_bytes
        ));
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
}
