use std::fs;
use std::path::PathBuf;

use toml_spanner::Toml;
use toml_spanner::{Arena, Item, Key, Table};

use crate::{audio::BufferRequest, bindings::BindingRuntime, client_net::ClientConfig};
use rpc::{control::DEFAULT_FILE_SIZE_LIMIT_BYTES, ids::RoomId};

pub const DEFAULT_CONFIG: &str = include_str!("../tomchat.toml");

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
pub struct ServerEntry {
    pub alias: String,
    pub tcp_addr: String,
    #[toml(default)]
    pub udp_addr: String,
    #[toml(default)]
    pub udp_probe_addr: Option<String>,
    pub user: String,
    #[toml(default)]
    pub display_name: String,
    pub token: String,
    #[toml(default)]
    pub server_public_key: String,
    pub room_id: u32,
}

impl Default for ServerEntry {
    fn default() -> Self {
        Self {
            alias: default_active_server(),
            tcp_addr: "127.0.0.1:41000".to_string(),
            udp_addr: String::new(),
            udp_probe_addr: None,
            user: "alice".to_string(),
            display_name: "Alice".to_string(),
            token: "alice-dev-token".to_string(),
            server_public_key: String::new(),
            room_id: 1,
        }
    }
}

impl ServerEntry {
    pub fn client_config(&self, files: &FileConfig, pairing_code: Option<String>) -> ClientConfig {
        ClientConfig {
            tcp_addr: self.tcp_addr.clone(),
            udp_addr: self.effective_udp_addr(),
            udp_probe_addr: self.udp_probe_addr.clone(),
            user: self.user.clone(),
            display_name: self.effective_display_name(),
            token: self.token.clone(),
            pairing_code,
            server_public_key: non_empty_string(&self.server_public_key),
            room_id: RoomId(self.room_id),
            file_receive_dir: files.receive_dir_path(),
            max_upload_bytes: files.max_upload_bytes,
            max_receive_bytes: files.max_receive_bytes,
        }
    }

    pub fn effective_display_name(&self) -> String {
        let display_name = self.display_name.trim();
        if display_name.is_empty() {
            self.user.clone()
        } else {
            display_name.to_string()
        }
    }

    pub fn effective_udp_addr(&self) -> String {
        let udp_addr = self.udp_addr.trim();
        if udp_addr.is_empty() {
            self.tcp_addr.clone()
        } else {
            udp_addr.to_string()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum BufferChoice {
    Default,
    #[toml(rename = "fixed-240")]
    Fixed240,
    #[toml(rename = "fixed-480")]
    Fixed480,
    #[toml(rename = "fixed-960")]
    Fixed960,
}

impl Default for BufferChoice {
    fn default() -> Self {
        BufferChoice::Default
    }
}

impl BufferChoice {
    pub fn to_request(self) -> BufferRequest {
        match self {
            BufferChoice::Default => BufferRequest::Default,
            BufferChoice::Fixed240 => BufferRequest::Fixed(240),
            BufferChoice::Fixed480 => BufferRequest::Fixed(480),
            BufferChoice::Fixed960 => BufferRequest::Fixed(960),
        }
    }

    pub fn from_request(request: BufferRequest) -> Self {
        match request {
            BufferRequest::Default => BufferChoice::Default,
            BufferRequest::Fixed(240) => BufferChoice::Fixed240,
            BufferRequest::Fixed(480) => BufferChoice::Fixed480,
            BufferRequest::Fixed(960) => BufferChoice::Fixed960,
            BufferRequest::Fixed(_) => BufferChoice::Default,
        }
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct AudioConfig {
    pub input_device_id: Option<String>,
    #[toml(default = 24_000)]
    pub bitrate_bps: i32,
    #[toml(default = true)]
    pub denoise: bool,
    #[toml(default)]
    pub buffer: BufferChoice,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device_id: None,
            bitrate_bps: 24_000,
            denoise: true,
            buffer: BufferChoice::Default,
        }
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct UiConfig {
    #[toml(default = 4)]
    pub room_height: u16,
    #[toml(default = 6)]
    pub max_composer_height: u16,
    #[toml(default = default_placeholder())]
    pub placeholder: String,
    #[toml(default = 50_000)]
    pub max_messages: u32,
    #[toml(default = 24)]
    pub overscan: u32,
}

fn default_placeholder() -> String {
    "Message #lobby".to_string()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            room_height: 4,
            max_composer_height: 6,
            placeholder: default_placeholder(),
            max_messages: 50_000,
            overscan: 24,
        }
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct FileConfig {
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_BYTES)]
    pub max_upload_bytes: u64,
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_BYTES)]
    pub max_receive_bytes: u64,
    #[toml(default)]
    pub receive_dir: String,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
            max_receive_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
            receive_dir: String::new(),
        }
    }
}

impl FileConfig {
    pub fn receive_dir_path(&self) -> Option<PathBuf> {
        let trimmed = self.receive_dir.trim();
        (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
    }
}

#[derive(Toml)]
#[toml(FromToml, recoverable, warn_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    #[toml(default = default_active_server())]
    pub active_server: String,
    #[toml(default = default_servers())]
    pub servers: Vec<ServerEntry>,
    #[toml(default)]
    pub audio: AudioConfig,
    #[toml(default)]
    pub ui: UiConfig,
    #[toml(default)]
    pub files: FileConfig,
    #[toml(default)]
    pub bindings: BindingRuntime,
    #[toml(skip)]
    pub config_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(DEFAULT_CONFIG, &arena)
            .expect("embedded tomchat config must parse");
        let mut config: Self = doc.to().expect("embedded tomchat config must deserialize");
        config.apply_inferred_addresses(false, false);
        config.normalize();
        config
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self, String> {
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("TOMCHAT_CONFIG").map(PathBuf::from))
            .or_else(default_config_path);

        let (content, source) = if let Some(path) = &config_path {
            let content = fs::read_to_string(path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            (content, path.display().to_string())
        } else {
            (DEFAULT_CONFIG.to_string(), "<embedded default>".to_string())
        };

        let arena = Arena::new();
        let mut doc = toml_spanner::parse(&content, &arena)
            .map_err(|err| format!("failed to parse {source}: {err}"))?;
        reject_deprecated_config_keys(&doc, &source)?;
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let mut config: Config = doc.to().map_err(|err| {
            let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
            format!("failed to deserialize {source}: {}", errors.join(", "))
        })?;
        config.config_path = config_path;
        config.apply_inferred_addresses_from_doc(&udp_addr_configured);
        config.normalize();
        config.validate(&source)?;
        Ok(config)
    }

    fn apply_inferred_addresses(&mut self, udp_addr_configured: bool, udp_addr_overridden: bool) {
        if !udp_addr_configured && !udp_addr_overridden {
            for server in &mut self.servers {
                server.udp_addr = server.tcp_addr.clone();
            }
        }
    }

    fn apply_inferred_addresses_from_doc(&mut self, udp_addr_configured: &[bool]) {
        for (index, server) in self.servers.iter_mut().enumerate() {
            if !udp_addr_configured.get(index).copied().unwrap_or(false) {
                server.udp_addr = server.tcp_addr.clone();
            }
        }
    }

    fn normalize(&mut self) {
        for server in &mut self.servers {
            server.alias = server.alias.trim().to_string();
            server.user = server.user.trim().to_string();
            server.display_name = server.effective_display_name();
        }
        if self.active_server.trim().is_empty()
            && let Some(server) = self.servers.first()
        {
            self.active_server = server.alias.clone();
        }
    }

    fn validate(&self, source: &str) -> Result<(), String> {
        if self.servers.is_empty() {
            return Err(format!(
                "{source}: at least one [[servers]] entry is required"
            ));
        }
        let mut aliases = std::collections::HashSet::new();
        for server in &self.servers {
            validate_server_alias(&server.alias)
                .map_err(|error| format!("{source}: server {}: {error}", server.alias))?;
            validate_non_empty(&server.user, "user")
                .map_err(|error| format!("{source}: server {}: {error}", server.alias))?;
            validate_non_empty(&server.token, "token")
                .map_err(|error| format!("{source}: server {}: {error}", server.alias))?;
            validate_non_empty(&server.display_name, "display-name")
                .map_err(|error| format!("{source}: server {}: {error}", server.alias))?;
            validate_endpoint(&server.tcp_addr, "tcp-addr")
                .map_err(|error| format!("{source}: server {}: {error}", server.alias))?;
            validate_endpoint(&server.effective_udp_addr(), "udp-addr")
                .map_err(|error| format!("{source}: server {}: {error}", server.alias))?;
            if let Some(addr) = &server.udp_probe_addr {
                validate_endpoint(addr, "udp-probe-addr")
                    .map_err(|error| format!("{source}: server {}: {error}", server.alias))?;
            }
            if server.room_id == 0 {
                return Err(format!(
                    "{source}: server {}: room-id must be non-zero",
                    server.alias
                ));
            }
            if !aliases.insert(server.alias.as_str()) {
                return Err(format!("{source}: duplicate server alias {}", server.alias));
            }
        }
        self.active_server().map(|_| ())
    }

    pub fn active_server(&self) -> Result<&ServerEntry, String> {
        self.servers
            .iter()
            .find(|server| server.alias == self.active_server)
            .ok_or_else(|| format!("active server {} is not configured", self.active_server))
    }

    pub fn upsert_server(&mut self, server: ServerEntry) {
        if let Some(existing) = self
            .servers
            .iter_mut()
            .find(|existing| existing.alias == server.alias)
        {
            *existing = server;
        } else {
            self.servers.push(server);
        }
    }

    pub fn set_active_server(&mut self, alias: String) {
        self.active_server = alias;
    }

    pub fn save_runtime(&self) -> Result<PathBuf, String> {
        let path = self
            .config_path
            .clone()
            .or_else(user_config_path)
            .ok_or_else(|| "HOME is not set; cannot determine config path".to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }

        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.to_string(),
            Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
        };

        let arena = Arena::new();
        let doc = toml_spanner::parse(&content, &arena)
            .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
        let mut table = doc.table().clone_in(&arena);
        write_runtime_config(&mut table, self, &arena);
        let rest = toml_spanner::Formatting::preserved_from(&doc)
            .with_span_projection_identity()
            .format_table_to_bytes(table, &arena);
        let mut output = runtime_servers_toml(self).into_bytes();
        let rest = trim_leading_blank_lines(&rest);
        if !rest.is_empty() {
            output.extend_from_slice(rest);
        }
        fs::write(&path, output)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        Ok(path)
    }
}

fn reject_deprecated_config_keys(
    doc: &toml_spanner::Document<'_>,
    source: &str,
) -> Result<(), String> {
    if doc.table().contains_key("network") {
        return Err(format!(
            "failed to deserialize {source}: [network] is not supported; use active-server and [[servers]]"
        ));
    }
    if doc
        .table()
        .get("servers")
        .and_then(Item::as_array)
        .is_some_and(|servers| {
            servers.iter().any(|server| {
                server
                    .as_table()
                    .is_some_and(|server| server.contains_key("pairing-code"))
            })
        })
    {
        return Err(format!(
            "failed to deserialize {source}: servers.pairing-code is not supported; use `tomchat join JOIN_STRING`"
        ));
    }
    if doc
        .table()
        .get("audio")
        .and_then(Item::as_table)
        .is_some_and(|audio| audio.contains_key("input-device-index"))
    {
        return Err(format!(
            "failed to deserialize {source}: audio.input-device-index is not supported; use audio.input-device-id"
        ));
    }
    Ok(())
}

pub fn value_arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find_map(|window| (window[0] == key).then(|| window[1].clone()))
}

fn default_config_path() -> Option<PathBuf> {
    let explicit = std::env::var_os("TOMCHAT_CONFIG").map(PathBuf::from);
    if explicit.is_some() {
        return explicit;
    }
    let user = user_config_path()?;
    user.exists().then_some(user)
}

fn default_active_server() -> String {
    "local".to_string()
}

fn default_servers() -> Vec<ServerEntry> {
    vec![ServerEntry::default()]
}

fn server_udp_addr_configured(doc: &toml_spanner::Document<'_>) -> Vec<bool> {
    doc.table()
        .get("servers")
        .and_then(Item::as_array)
        .map(|servers| {
            servers
                .iter()
                .map(|server| {
                    server
                        .as_table()
                        .is_some_and(|table| table.contains_key("udp-addr"))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/tomchat.toml"))
}

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

pub fn validate_server_alias(alias: &str) -> Result<(), String> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Err("alias is empty".to_string());
    }
    if alias.len() > 64 {
        return Err("alias exceeds 64 bytes".to_string());
    }
    if !alias
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("alias must use ASCII letters, numbers, '-' or '_'".to_string());
    }
    Ok(())
}

pub fn validate_display_name(display_name: &str) -> Result<(), String> {
    let display_name = display_name.trim();
    validate_non_empty(display_name, "display-name")?;
    if display_name.len() > 64 {
        return Err("display-name exceeds 64 bytes".to_string());
    }
    Ok(())
}

fn validate_endpoint(value: &str, name: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if value.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(());
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(format!("{name} must include a port"));
    };
    if host.trim().is_empty() || port.trim().is_empty() {
        return Err(format!("{name} must include a host and port"));
    }
    port.parse::<u16>()
        .map(|_| ())
        .map_err(|_| format!("{name} port is invalid"))
}

fn validate_non_empty(value: &str, name: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{name} is empty"))
    } else {
        Ok(())
    }
}

fn write_runtime_config<'de>(root: &mut Table<'de>, config: &Config, arena: &'de Arena) {
    {
        root.remove_entry("network");
        root.remove_entry("active-server");
        root.remove_entry("servers");
    }

    {
        let audio = ensure_table(root, "audio", arena);
        audio.remove_entry("input-device-index");
        match config.audio.input_device_id.as_deref() {
            Some(id) => insert_str(audio, "input-device-id", id, arena),
            None => {
                audio.remove_entry("input-device-id");
            }
        }
        audio.insert(
            Key::new("bitrate-bps"),
            Item::from(config.audio.bitrate_bps),
            arena,
        );
        audio.insert(Key::new("denoise"), Item::from(config.audio.denoise), arena);
        insert_str(
            audio,
            "buffer",
            buffer_choice_name(config.audio.buffer),
            arena,
        );
    }

    {
        let ui = ensure_table(root, "ui", arena);
        ui.insert(
            Key::new("room-height"),
            Item::from(config.ui.room_height as i64),
            arena,
        );
        ui.insert(
            Key::new("max-composer-height"),
            Item::from(config.ui.max_composer_height as i64),
            arena,
        );
        insert_str(ui, "placeholder", &config.ui.placeholder, arena);
        ui.insert(
            Key::new("max-messages"),
            Item::from(config.ui.max_messages as i64),
            arena,
        );
        ui.insert(
            Key::new("overscan"),
            Item::from(config.ui.overscan as i64),
            arena,
        );
    }

    {
        let files = ensure_table(root, "files", arena);
        files.insert(
            Key::new("max-upload-bytes"),
            Item::from(config.files.max_upload_bytes as i64),
            arena,
        );
        files.insert(
            Key::new("max-receive-bytes"),
            Item::from(config.files.max_receive_bytes as i64),
            arena,
        );
        insert_str(files, "receive-dir", &config.files.receive_dir, arena);
    }
}

fn runtime_servers_toml(config: &Config) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "active-server = \"{}\"\n\n",
        toml_quote_value(&config.active_server)
    ));
    for server in &config.servers {
        out.push_str("[[servers]]\n");
        out.push_str(&format!(
            "alias = \"{}\"\n",
            toml_quote_value(&server.alias)
        ));
        out.push_str(&format!("user = \"{}\"\n", toml_quote_value(&server.user)));
        out.push_str(&format!(
            "display-name = \"{}\"\n",
            toml_quote_value(&server.display_name)
        ));
        out.push_str(&format!(
            "token = \"{}\"\n",
            toml_quote_value(&server.token)
        ));
        out.push_str(&format!(
            "server-public-key = \"{}\"\n",
            toml_quote_value(&server.server_public_key)
        ));
        out.push_str(&format!("tcp-addr = \"{}\"\n", server.tcp_addr));
        if server.effective_udp_addr() != server.tcp_addr {
            out.push_str(&format!("udp-addr = \"{}\"\n", server.udp_addr));
        }
        if let Some(addr) = &server.udp_probe_addr {
            out.push_str(&format!("udp-probe-addr = \"{}\"\n", addr));
        }
        out.push_str(&format!("room-id = {}\n\n", server.room_id));
    }
    out
}

fn trim_leading_blank_lines(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() {
        match bytes[start] {
            b'\n' | b'\r' => start += 1,
            b' ' | b'\t' => {
                let line_start = start;
                while start < bytes.len() && matches!(bytes[start], b' ' | b'\t') {
                    start += 1;
                }
                if start < bytes.len() && matches!(bytes[start], b'\n' | b'\r') {
                    start += 1;
                } else {
                    start = line_start;
                    break;
                }
            }
            _ => break,
        }
    }
    &bytes[start..]
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

fn ensure_table<'de, 'b>(
    root: &'b mut Table<'de>,
    name: &'static str,
    arena: &'de Arena,
) -> &'b mut Table<'de> {
    let needs_insert = root.get(name).and_then(|item| item.as_table()).is_none();
    if needs_insert {
        root.insert(Key::new(name), Table::new().into_item(), arena);
    }
    root.get_mut(name)
        .and_then(|item| item.as_table_mut())
        .expect("section was just inserted as a table")
}

fn insert_str<'de>(table: &mut Table<'de>, key: &'static str, value: &str, arena: &'de Arena) {
    table.insert(Key::new(key), Item::string(arena.alloc_str(value)), arena);
}

fn buffer_choice_name(choice: BufferChoice) -> &'static str {
    match choice {
        BufferChoice::Default => "default",
        BufferChoice::Fixed240 => "fixed-240",
        BufferChoice::Fixed480 => "fixed-480",
        BufferChoice::Fixed960 => "fixed-960",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_udp_addr_inherits_tcp_addr_when_omitted() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
alias = "lab"
user = "alice"
display-name = "Alice"
token = "alice-dev-token"
tcp-addr = "127.0.0.1:42000"
server-public-key = ""
room-id = 1
"#,
            &arena,
        )
        .unwrap();
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses_from_doc(&udp_addr_configured);

        assert_eq!(config.servers[0].udp_addr, config.servers[0].tcp_addr);
        assert_eq!(config.servers[0].udp_probe_addr, None);
    }

    #[test]
    fn server_parses_explicit_udp_and_probe_addrs() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
alias = "lab"
user = "alice"
display-name = "Alice"
token = "alice-dev-token"
tcp-addr = "127.0.0.1:42000"
udp-addr = "127.0.0.1:42001"
udp-probe-addr = "127.0.0.1:42002"
server-public-key = ""
room-id = 1
"#,
            &arena,
        )
        .unwrap();
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses_from_doc(&udp_addr_configured);

        assert_eq!(config.servers[0].udp_addr, "127.0.0.1:42001");
        assert_eq!(
            config.servers[0].udp_probe_addr.as_deref(),
            Some("127.0.0.1:42002")
        );
    }

    #[test]
    fn server_endpoints_accept_domains() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "prod"

[[servers]]
alias = "prod"
user = "alice"
display-name = "Alice"
token = "client-generated-token-with-at-least-32-bytes"
tcp-addr = "chat.example.com:443"
udp-addr = "media.example.com:54100"
udp-probe-addr = "probe.example.com:54101"
server-public-key = ""
room-id = 1
"#,
            &arena,
        )
        .unwrap();
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses_from_doc(&udp_addr_configured);
        config.normalize();
        config.validate("<test>").unwrap();

        let server = config.active_server().unwrap();
        assert_eq!(server.tcp_addr, "chat.example.com:443");
        assert_eq!(server.udp_addr, "media.example.com:54100");
        assert_eq!(
            server.udp_probe_addr.as_deref(),
            Some("probe.example.com:54101")
        );
    }

    #[test]
    fn rejects_legacy_input_device_index() {
        let path = std::env::temp_dir().join(format!(
            "tomchat-config-legacy-input-device-index-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
[audio]
input-device-index = 20
"#,
        )
        .unwrap();

        let error = match Config::load(Some(path.to_str().unwrap())) {
            Ok(_) => panic!("legacy input-device-index should be rejected"),
            Err(error) => error,
        };
        let _ = std::fs::remove_file(path);

        assert!(error.contains("audio.input-device-index"));
        assert!(error.contains("audio.input-device-id"));
    }

    #[test]
    fn runtime_servers_toml_does_not_write_pairing_code() {
        let config = Config::default();
        let content = runtime_servers_toml(&config);

        assert!(content.contains("[[servers]]"));
        assert!(content.contains("active-server = \"local\""));
        assert!(!content.contains("pairing-code"));
    }
}
