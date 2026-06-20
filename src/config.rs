use std::{fs, net::SocketAddr, path::PathBuf};

use toml_spanner::Toml;
use toml_spanner::{Arena, Item, Key, Table};

use crate::{audio::BufferRequest, bindings::BindingRuntime, client_net::ClientConfig};
use rpc::{control::DEFAULT_FILE_SIZE_LIMIT_BYTES, ids::RoomId};

pub const DEFAULT_CONFIG: &str = include_str!("../tomchat.toml");

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct NetworkConfig {
    #[toml(FromToml with = toml_spanner::helper::parse_string, ToToml with = toml_spanner::helper::display)]
    pub tcp_addr: SocketAddr,
    #[toml(FromToml with = toml_spanner::helper::parse_string, ToToml with = toml_spanner::helper::display)]
    pub udp_addr: SocketAddr,
    #[toml(default = default_udp_probe_addr(), FromToml with = toml_spanner::helper::parse_string, ToToml with = toml_spanner::helper::display)]
    pub udp_probe_addr: SocketAddr,
    pub user: String,
    pub token: String,
    pub room_id: u32,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            tcp_addr: "127.0.0.1:41000".parse().expect("valid default TCP addr"),
            udp_addr: "127.0.0.1:41001".parse().expect("valid default UDP addr"),
            udp_probe_addr: default_udp_probe_addr(),
            user: "alice".to_string(),
            token: "alice-dev-token".to_string(),
            room_id: 1,
        }
    }
}

impl NetworkConfig {
    pub fn client_config(&self, files: &FileConfig) -> ClientConfig {
        ClientConfig {
            tcp_addr: self.tcp_addr,
            udp_addr: self.udp_addr,
            udp_probe_addr: self.udp_probe_addr,
            user: self.user.clone(),
            token: self.token.clone(),
            room_id: RoomId(self.room_id),
            file_receive_dir: files.receive_dir_path(),
            max_upload_bytes: files.max_upload_bytes,
            max_receive_bytes: files.max_receive_bytes,
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
    #[toml(default)]
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
    #[toml(default)]
    pub network: NetworkConfig,
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
        doc.to().expect("embedded tomchat config must deserialize")
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
        let mut config: Config = doc.to().map_err(|err| {
            let errors: Vec<String> = err.errors.iter().map(ToString::to_string).collect();
            format!("failed to deserialize {source}: {}", errors.join(", "))
        })?;
        config.config_path = config_path;
        config.apply_env_and_cli_overrides();
        Ok(config)
    }

    fn apply_env_and_cli_overrides(&mut self) {
        let args = std::env::args().collect::<Vec<_>>();
        if let Some(user) =
            value_arg(&args, "--user").or_else(|| std::env::var("TOMCHAT_USER").ok())
        {
            self.network.user = user;
        }
        if let Some(token) =
            value_arg(&args, "--token").or_else(|| std::env::var("TOMCHAT_TOKEN").ok())
        {
            self.network.token = token;
        }
        if let Some(addr) = value_arg(&args, "--tcp").or_else(|| std::env::var("TOMCHAT_TCP").ok())
        {
            if let Ok(addr) = addr.parse() {
                self.network.tcp_addr = addr;
            }
        }
        if let Some(addr) = value_arg(&args, "--udp").or_else(|| std::env::var("TOMCHAT_UDP").ok())
        {
            if let Ok(addr) = addr.parse() {
                self.network.udp_addr = addr;
            }
        }
        if let Some(addr) =
            value_arg(&args, "--udp-probe").or_else(|| std::env::var("TOMCHAT_UDP_PROBE").ok())
        {
            if let Ok(addr) = addr.parse() {
                self.network.udp_probe_addr = addr;
            }
        }
        if let Some(receive_dir) =
            value_arg(&args, "--receive-dir").or_else(|| std::env::var("TOMCHAT_RECEIVE_DIR").ok())
        {
            self.files.receive_dir = receive_dir;
        }
        if let Some(limit) = value_arg(&args, "--max-upload-bytes")
            .or_else(|| std::env::var("TOMCHAT_MAX_UPLOAD_BYTES").ok())
            .and_then(|value| value.parse().ok())
        {
            self.files.max_upload_bytes = limit;
        }
        if let Some(limit) = value_arg(&args, "--max-receive-bytes")
            .or_else(|| std::env::var("TOMCHAT_MAX_RECEIVE_BYTES").ok())
            .and_then(|value| value.parse().ok())
        {
            self.files.max_receive_bytes = limit;
        }
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
        let output = toml_spanner::Formatting::preserved_from(&doc)
            .with_span_projection_identity()
            .format_table_to_bytes(table, &arena);
        fs::write(&path, output)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        Ok(path)
    }
}

fn reject_deprecated_config_keys(
    doc: &toml_spanner::Document<'_>,
    source: &str,
) -> Result<(), String> {
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

pub fn default_token(user: &str) -> &'static str {
    match user {
        "bob" => "bob-dev-token",
        "carol" => "carol-dev-token",
        _ => "alice-dev-token",
    }
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

fn default_udp_probe_addr() -> SocketAddr {
    "127.0.0.1:41002"
        .parse()
        .expect("valid default UDP probe addr")
}

fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/tomchat.toml"))
}

fn write_runtime_config<'de>(root: &mut Table<'de>, config: &Config, arena: &'de Arena) {
    {
        let network = ensure_table(root, "network", arena);
        insert_str(network, "user", &config.network.user, arena);
        insert_str(network, "token", &config.network.token, arena);
        insert_str(
            network,
            "tcp-addr",
            &config.network.tcp_addr.to_string(),
            arena,
        );
        insert_str(
            network,
            "udp-addr",
            &config.network.udp_addr.to_string(),
            arena,
        );
        network.insert(
            Key::new("room-id"),
            Item::from(config.network.room_id as i64),
            arena,
        );
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
}
