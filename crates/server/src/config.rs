use std::{collections::HashSet, fs, net::SocketAddr, path::PathBuf};

use ring::{digest, signature::KeyPair};
use rpc::{
    control::DEFAULT_FILE_SIZE_LIMIT_BYTES,
    crypto::{encode_hex, server_key_pair_from_seed_hex},
    ids::{RoomId, UserId},
};
use toml_spanner::Toml;

pub const DEFAULT_SERVER_CONFIG: &str = include_str!("../../../tomchat-server.toml");
const SECRET_HASH_PREFIX: &str = "sha256:";
const SHA256_HEX_LEN: usize = 64;

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct NetworkConfig {
    #[toml(FromToml with = toml_spanner::helper::parse_string, ToToml with = toml_spanner::helper::display)]
    pub tcp_addr: SocketAddr,
    #[toml(FromToml with = toml_spanner::helper::parse_string, ToToml with = toml_spanner::helper::display)]
    pub udp_addr: SocketAddr,
    #[toml(default = default_udp_probe_addr(), FromToml with = toml_spanner::helper::parse_string, ToToml with = toml_spanner::helper::display)]
    pub udp_probe_addr: SocketAddr,
    #[toml(default = true)]
    pub p2p_enabled: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            tcp_addr: "127.0.0.1:41000".parse().expect("valid default TCP addr"),
            udp_addr: "127.0.0.1:41001".parse().expect("valid default UDP addr"),
            udp_probe_addr: default_udp_probe_addr(),
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
    pub token_hash: String,
    #[toml(default)]
    pub pairing_code_hash: String,
}

impl UserConfig {
    pub fn user_id(&self) -> UserId {
        UserId(self.id)
    }

    pub fn has_pairing_code(&self) -> bool {
        !self.pairing_code_hash.trim().is_empty()
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(
    FromToml,
    ToToml,
    recoverable,
    warn_unknown_fields,
    rename_all = "kebab-case"
)]
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
            .expect("embedded tomchat server config must parse");
        doc.to()
            .expect("embedded tomchat server config must deserialize")
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self, String> {
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("TOMCHAT_SERVER_CONFIG").map(PathBuf::from))
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
        let mut config: Config = doc.to().map_err(|err| {
            let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
            format!("failed to deserialize {source}: {}", errors.join(", "))
        })?;
        config.config_path = config_path;
        config.apply_env_and_cli_overrides();
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
        token_hash: String,
    ) -> Result<UserConfig, String> {
        let index = self
            .users
            .iter()
            .position(|user| user.name == user_name)
            .ok_or_else(|| "unknown pairing user".to_string())?;
        let old_token_hash = self.users[index].token_hash.clone();
        let old_pairing_code_hash = self.users[index].pairing_code_hash.clone();
        self.users[index].token_hash = token_hash;
        self.users[index].pairing_code_hash.clear();
        if let Err(error) = self.save_runtime() {
            self.users[index].token_hash = old_token_hash;
            self.users[index].pairing_code_hash = old_pairing_code_hash;
            return Err(error);
        }
        Ok(self.users[index].clone())
    }

    fn apply_env_and_cli_overrides(&mut self) {
        let args = std::env::args().collect::<Vec<_>>();
        if let Some(addr) = value_arg(&args, "--tcp")
            .or_else(|| std::env::var("TOMCHAT_SERVER_TCP").ok())
            .or_else(|| std::env::var("TOMCHAT_TCP").ok())
        {
            if let Ok(addr) = addr.parse() {
                self.network.tcp_addr = addr;
            }
        }
        if let Some(addr) = value_arg(&args, "--udp")
            .or_else(|| std::env::var("TOMCHAT_SERVER_UDP").ok())
            .or_else(|| std::env::var("TOMCHAT_UDP").ok())
        {
            if let Ok(addr) = addr.parse() {
                self.network.udp_addr = addr;
            }
        }
        if let Some(addr) = value_arg(&args, "--udp-probe")
            .or_else(|| std::env::var("TOMCHAT_SERVER_UDP_PROBE").ok())
            .or_else(|| std::env::var("TOMCHAT_UDP_PROBE").ok())
        {
            if let Ok(addr) = addr.parse() {
                self.network.udp_probe_addr = addr;
            }
        }
        if let Some(limit) = value_arg(&args, "--chat-history-limit")
            .or_else(|| std::env::var("TOMCHAT_SERVER_CHAT_HISTORY_LIMIT").ok())
            .and_then(|value| value.parse().ok())
        {
            self.security.chat_history_limit = limit;
        }
        if let Some(encryption) = value_arg(&args, "--encryption")
            .or_else(|| std::env::var("TOMCHAT_SERVER_ENCRYPTION").ok())
            .and_then(|value| parse_bool(&value))
        {
            self.security.encryption = encryption;
        }
        if args.iter().any(|arg| arg == "--no-encryption") {
            self.security.encryption = false;
        }
        if let Some(p2p_enabled) = value_arg(&args, "--p2p")
            .or_else(|| value_arg(&args, "--p2p-enabled"))
            .or_else(|| std::env::var("TOMCHAT_SERVER_P2P_ENABLED").ok())
            .or_else(|| std::env::var("TOMCHAT_SERVER_P2P").ok())
            .and_then(|value| parse_bool(&value))
        {
            self.network.p2p_enabled = p2p_enabled;
        }
        if args.iter().any(|arg| arg == "--no-p2p") {
            self.network.p2p_enabled = false;
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
            if !user_ids.insert(user.id) {
                return Err(format!("{source}: duplicate user id {}", user.id));
            }
            if !user_names.insert(user.name.as_str()) {
                return Err(format!("{source}: duplicate user name {}", user.name));
            }
            if user.token_hash.trim().is_empty() && user.pairing_code_hash.trim().is_empty() {
                return Err(format!(
                    "{source}: user {} needs token-hash or pairing-code-hash",
                    user.name
                ));
            }
            validate_optional_secret_hash(
                source,
                &format!("user {} token-hash", user.name),
                &user.token_hash,
            )?;
            validate_optional_secret_hash(
                source,
                &format!("user {} pairing-code-hash", user.name),
                &user.pairing_code_hash,
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
        out.push_str("# tomchat server configuration\n\n");
        out.push_str("[network]\n");
        out.push_str(&format!("tcp-addr = \"{}\"\n", self.network.tcp_addr));
        out.push_str(&format!("udp-addr = \"{}\"\n", self.network.udp_addr));
        out.push_str(&format!(
            "udp-probe-addr = \"{}\"\n",
            self.network.udp_probe_addr
        ));
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
                "token-hash = \"{}\"\n",
                toml_quote_value(&user.token_hash)
            ));
            out.push_str(&format!(
                "pairing-code-hash = \"{}\"\n\n",
                toml_quote_value(&user.pairing_code_hash)
            ));
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

fn parse_secret_hash(stored_hash: &str) -> Option<[u8; 32]> {
    let hex = stored_hash.trim().strip_prefix(SECRET_HASH_PREFIX)?;
    if hex.len() != SHA256_HEX_LEN {
        return None;
    }
    let decoded = rpc::crypto::decode_hex(hex).ok()?;
    decoded.try_into().ok()
}

fn default_config_path() -> Option<PathBuf> {
    let explicit = std::env::var_os("TOMCHAT_SERVER_CONFIG").map(PathBuf::from);
    if explicit.is_some() {
        return explicit;
    }
    let local = PathBuf::from("tomchat-server.toml");
    local.exists().then_some(local)
}

fn default_udp_probe_addr() -> SocketAddr {
    "127.0.0.1:41002"
        .parse()
        .expect("valid default UDP probe addr")
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
            token_hash: "sha256:8989ee243a8829e3364b6eb9cae74f1edfa53e327e5718fe337d23d6ab8b4625"
                .to_string(),
            pairing_code_hash: String::new(),
        },
        UserConfig {
            id: 2,
            name: "bob".to_string(),
            token_hash: "sha256:fd57e39ef28a47298ae51762fd3c907253ac7d4b7507fdf28fcf6d5e4bbaeb8a"
                .to_string(),
            pairing_code_hash: String::new(),
        },
        UserConfig {
            id: 3,
            name: "carol".to_string(),
            token_hash: "sha256:f93be296b384d5f0f8760bdb53cf3ea52a5e41e6f7a532645458784ece411f62"
                .to_string(),
            pairing_code_hash: String::new(),
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

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
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
    fn secret_hash_verification_is_exact() {
        let hash = hash_secret("pair-alice-please-change");
        assert!(verify_secret_hash(&hash, "pair-alice-please-change"));
        assert!(!verify_secret_hash(&hash, "pair-bob-please-change"));
    }

    #[test]
    fn mark_user_paired_persists_token_hash_and_clears_pairing_code() {
        let path = std::env::temp_dir().join(format!(
            "tomchat-server-pairing-test-{}.toml",
            std::process::id()
        ));
        let mut config = Config::default();
        config.config_path = Some(path.clone());
        config.users[0].pairing_code_hash = hash_secret("temporary-pairing-code");

        let token_hash = hash_secret("client-generated-token-with-at-least-32-bytes");
        let user = config
            .mark_user_paired("alice", token_hash.clone())
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(user.token_hash, token_hash);
        assert!(user.pairing_code_hash.is_empty());
        assert!(content.contains("p2p-enabled = true"));
        assert!(content.contains(&format!("token-hash = \"{token_hash}\"")));
        assert!(content.contains("pairing-code-hash = \"\""));
    }
}
