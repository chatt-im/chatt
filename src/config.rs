use std::path::PathBuf;
use std::{fs, time::Duration};

use toml_spanner::Toml;
use toml_spanner::{Arena, Array, ArrayStyle, Item, Key, Table};

use crate::{
    audio::{BufferRequest, LiveAudioTuning},
    bindings::BindingRuntime,
    client_net::ClientConfig,
};
use rpc::{control::DEFAULT_FILE_SIZE_LIMIT_BYTES, ids::RoomId};

pub const DEFAULT_CONFIG: &str = include_str!("../chatt.toml");
pub const DEFAULT_MAX_AMPLIFICATION: f32 = crate::audio::DEFAULT_LIVE_MAX_AMPLIFICATION;
pub const MIN_USER_VOLUME_DB: f32 = -24.0;
pub const MAX_USER_VOLUME_DB: f32 = 12.0;
pub const USER_VOLUME_DB_STEP: f32 = 0.5;

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
    pub output_device_id: Option<String>,
    #[toml(default = 48_000)]
    pub bitrate_bps: i32,
    #[toml(default = true)]
    pub denoise: bool,
    #[toml(default = false)]
    pub echo_cancellation: bool,
    #[toml(default = DEFAULT_MAX_AMPLIFICATION)]
    pub max_amplification: f32,
    #[toml(default)]
    pub buffer: BufferChoice,
    #[toml(default)]
    pub latency: AudioLatencyConfig,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device_id: None,
            output_device_id: None,
            bitrate_bps: 48_000,
            denoise: true,
            echo_cancellation: false,
            max_amplification: DEFAULT_MAX_AMPLIFICATION,
            buffer: BufferChoice::Default,
            latency: AudioLatencyConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct AudioLatencyConfig {
    #[toml(default = true)]
    pub adaptive_catch_up: bool,
    #[toml(default = true)]
    pub playback_silence_skip: bool,
    #[toml(default = true)]
    pub capture_silence_gate: bool,
    #[toml(default = 60)]
    pub target_queue_ms: u64,
    #[toml(default = 320)]
    pub moderate_loss_queue_ms: u64,
    #[toml(default = 1_000)]
    pub dred_horizon_ms: u64,
    #[toml(default = 1_500)]
    pub hard_queue_bound_ms: u64,
    #[toml(default = 5_000)]
    pub loss_window_ms: u64,
    #[toml(default = 5_000)]
    pub loss_hold_ms: u64,
    #[toml(default = 10_000)]
    pub severe_loss_hold_ms: u64,
    #[toml(default = 40)]
    pub initial_buffer_ms: u64,
    #[toml(default = 60)]
    pub max_reorder_delay_ms: u64,
    #[toml(default = 0.15)]
    pub max_speed_up: f64,
    #[toml(default = 64)]
    pub silence_vad_max: u8,
    #[toml(default = 250)]
    pub silence_min_gap_ms: u64,
    #[toml(default = 40)]
    pub silence_guard_ms: u64,
    #[toml(default = 10)]
    pub silence_ramp_ms: u64,
    #[toml(default = 200)]
    pub silence_max_skip_ms: u64,
    #[toml(default = 20)]
    pub silence_min_skip_ms: u64,
    #[toml(default = 2_000)]
    pub capture_long_silence_stop_ms: u64,
    #[toml(default = 30)]
    pub capture_silence_preroll_ms: u64,
    #[toml(default = 10)]
    pub capture_silence_ramp_ms: u64,
}

impl Default for AudioLatencyConfig {
    fn default() -> Self {
        let tuning = LiveAudioTuning::default();
        Self {
            adaptive_catch_up: tuning.adaptive_catch_up,
            playback_silence_skip: tuning.playback_silence_skip,
            capture_silence_gate: tuning.capture_silence_gate,
            target_queue_ms: duration_ms(tuning.target_queue),
            moderate_loss_queue_ms: duration_ms(tuning.moderate_loss_queue),
            dred_horizon_ms: duration_ms(tuning.dred_horizon),
            hard_queue_bound_ms: duration_ms(tuning.hard_queue_bound),
            loss_window_ms: duration_ms(tuning.loss_window),
            loss_hold_ms: duration_ms(tuning.loss_hold),
            severe_loss_hold_ms: duration_ms(tuning.severe_loss_hold),
            initial_buffer_ms: duration_ms(tuning.initial_buffer),
            max_reorder_delay_ms: duration_ms(tuning.max_reorder_delay),
            max_speed_up: tuning.max_speed_up,
            silence_vad_max: tuning.silence_vad_max,
            silence_min_gap_ms: duration_ms(tuning.silence_min_gap),
            silence_guard_ms: duration_ms(tuning.silence_guard),
            silence_ramp_ms: duration_ms(tuning.silence_ramp),
            silence_max_skip_ms: duration_ms(tuning.silence_max_skip),
            silence_min_skip_ms: duration_ms(tuning.silence_min_skip),
            capture_long_silence_stop_ms: duration_ms(tuning.capture_long_silence_stop),
            capture_silence_preroll_ms: duration_ms(tuning.capture_silence_preroll),
            capture_silence_ramp_ms: duration_ms(tuning.capture_silence_ramp),
        }
    }
}

impl AudioLatencyConfig {
    pub fn to_tuning(&self) -> LiveAudioTuning {
        LiveAudioTuning {
            adaptive_catch_up: self.adaptive_catch_up,
            playback_silence_skip: self.playback_silence_skip,
            capture_silence_gate: self.capture_silence_gate,
            target_queue: Duration::from_millis(self.target_queue_ms),
            moderate_loss_queue: Duration::from_millis(self.moderate_loss_queue_ms),
            dred_horizon: Duration::from_millis(self.dred_horizon_ms),
            hard_queue_bound: Duration::from_millis(self.hard_queue_bound_ms),
            loss_window: Duration::from_millis(self.loss_window_ms),
            loss_hold: Duration::from_millis(self.loss_hold_ms),
            severe_loss_hold: Duration::from_millis(self.severe_loss_hold_ms),
            initial_buffer: Duration::from_millis(self.initial_buffer_ms),
            max_reorder_delay: Duration::from_millis(self.max_reorder_delay_ms),
            max_speed_up: self.max_speed_up,
            silence_vad_max: self.silence_vad_max,
            silence_min_gap: Duration::from_millis(self.silence_min_gap_ms),
            silence_guard: Duration::from_millis(self.silence_guard_ms),
            silence_ramp: Duration::from_millis(self.silence_ramp_ms),
            silence_max_skip: Duration::from_millis(self.silence_max_skip_ms),
            silence_min_skip: Duration::from_millis(self.silence_min_skip_ms),
            capture_long_silence_stop: Duration::from_millis(self.capture_long_silence_stop_ms),
            capture_silence_preroll: Duration::from_millis(self.capture_silence_preroll_ms),
            capture_silence_ramp: Duration::from_millis(self.capture_silence_ramp_ms),
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.to_tuning().validate()
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
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

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct UserAudioPreference {
    pub server_alias: String,
    pub user_id: u32,
    pub volume_db: f32,
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
    pub user_audio: Vec<UserAudioPreference>,
    #[toml(default)]
    pub bindings: BindingRuntime,
    #[toml(skip)]
    pub config_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let arena = Arena::new();
        let mut doc =
            toml_spanner::parse(DEFAULT_CONFIG, &arena).expect("embedded chatt config must parse");
        let mut config: Self = doc.to().expect("embedded chatt config must deserialize");
        config.apply_inferred_addresses(false, false);
        config.normalize();
        config
    }
}

impl Config {
    pub fn load(path: Option<&str>) -> Result<Self, String> {
        let config_path = path
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("CHATT_CONFIG").map(PathBuf::from))
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
        for preference in &mut self.user_audio {
            preference.server_alias = preference.server_alias.trim().to_string();
            preference.volume_db = snap_user_volume_db(preference.volume_db);
        }
        self.user_audio
            .retain(|preference| preference.volume_db != 0.0);
        self.sort_user_audio();
    }

    fn validate(&self, source: &str) -> Result<(), String> {
        self.audio
            .latency
            .validate()
            .map_err(|error| format!("{source}: audio latency: {error}"))?;
        if !(8_000..=96_000).contains(&self.audio.bitrate_bps) {
            return Err(format!(
                "{source}: audio bitrate-bps must be between 8000 and 96000"
            ));
        }
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
        let mut user_audio_keys = std::collections::HashSet::new();
        for preference in &self.user_audio {
            validate_server_alias(&preference.server_alias).map_err(|error| {
                format!(
                    "{source}: user-audio {}:{}: {error}",
                    preference.server_alias, preference.user_id
                )
            })?;
            if preference.user_id == 0 {
                return Err(format!(
                    "{source}: user-audio {}: user-id must be non-zero",
                    preference.server_alias
                ));
            }
            if !(MIN_USER_VOLUME_DB..=MAX_USER_VOLUME_DB).contains(&preference.volume_db) {
                return Err(format!(
                    "{source}: user-audio {}:{} volume-db must be between {:.1} and {:.1}",
                    preference.server_alias,
                    preference.user_id,
                    MIN_USER_VOLUME_DB,
                    MAX_USER_VOLUME_DB
                ));
            }
            if !user_audio_keys.insert((preference.server_alias.as_str(), preference.user_id)) {
                return Err(format!(
                    "{source}: duplicate user-audio entry for {}:{}",
                    preference.server_alias, preference.user_id
                ));
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

    pub fn user_volume_db(&self, server_alias: &str, user_id: u32) -> f32 {
        self.user_audio
            .iter()
            .find(|preference| {
                preference.server_alias == server_alias && preference.user_id == user_id
            })
            .map_or(0.0, |preference| preference.volume_db)
    }

    pub fn set_user_volume_db(&mut self, server_alias: &str, user_id: u32, volume_db: f32) {
        let volume_db = snap_user_volume_db(volume_db);
        if volume_db == 0.0 {
            self.user_audio.retain(|preference| {
                !(preference.server_alias == server_alias && preference.user_id == user_id)
            });
            return;
        }

        if let Some(preference) = self.user_audio.iter_mut().find(|preference| {
            preference.server_alias == server_alias && preference.user_id == user_id
        }) {
            preference.volume_db = volume_db;
        } else {
            self.user_audio.push(UserAudioPreference {
                server_alias: server_alias.to_string(),
                user_id,
                volume_db,
            });
        }
        self.sort_user_audio();
    }

    fn sort_user_audio(&mut self) {
        self.user_audio.sort_by(|a, b| {
            a.server_alias
                .cmp(&b.server_alias)
                .then_with(|| a.user_id.cmp(&b.user_id))
        });
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

pub fn snap_user_volume_db(volume_db: f32) -> f32 {
    if !volume_db.is_finite() {
        return 0.0;
    }
    let snapped = (volume_db / USER_VOLUME_DB_STEP).round() * USER_VOLUME_DB_STEP;
    let clamped = snapped.clamp(MIN_USER_VOLUME_DB, MAX_USER_VOLUME_DB);
    if clamped.abs() < f32::EPSILON {
        0.0
    } else {
        clamped
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
            "failed to deserialize {source}: servers.pairing-code is not supported; use `chatt join JOIN_STRING`"
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
    let explicit = std::env::var_os("CHATT_CONFIG").map(PathBuf::from);
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
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/chatt.toml"))
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
        match config.audio.output_device_id.as_deref() {
            Some(id) => insert_str(audio, "output-device-id", id, arena),
            None => {
                audio.remove_entry("output-device-id");
            }
        }
        audio.insert(
            Key::new("bitrate-bps"),
            Item::from(config.audio.bitrate_bps),
            arena,
        );
        audio.insert(Key::new("denoise"), Item::from(config.audio.denoise), arena);
        audio.insert(
            Key::new("max-amplification"),
            Item::from(config.audio.max_amplification as f64),
            arena,
        );
        insert_str(
            audio,
            "buffer",
            buffer_choice_name(config.audio.buffer),
            arena,
        );

        let latency = ensure_table(audio, "latency", arena);
        latency.insert(
            Key::new("adaptive-catch-up"),
            Item::from(config.audio.latency.adaptive_catch_up),
            arena,
        );
        latency.insert(
            Key::new("playback-silence-skip"),
            Item::from(config.audio.latency.playback_silence_skip),
            arena,
        );
        latency.insert(
            Key::new("capture-silence-gate"),
            Item::from(config.audio.latency.capture_silence_gate),
            arena,
        );
        latency.insert(
            Key::new("target-queue-ms"),
            Item::from(config.audio.latency.target_queue_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("moderate-loss-queue-ms"),
            Item::from(config.audio.latency.moderate_loss_queue_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("dred-horizon-ms"),
            Item::from(config.audio.latency.dred_horizon_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("hard-queue-bound-ms"),
            Item::from(config.audio.latency.hard_queue_bound_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("loss-window-ms"),
            Item::from(config.audio.latency.loss_window_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("loss-hold-ms"),
            Item::from(config.audio.latency.loss_hold_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("severe-loss-hold-ms"),
            Item::from(config.audio.latency.severe_loss_hold_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("initial-buffer-ms"),
            Item::from(config.audio.latency.initial_buffer_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("max-reorder-delay-ms"),
            Item::from(config.audio.latency.max_reorder_delay_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("max-speed-up"),
            Item::from(config.audio.latency.max_speed_up),
            arena,
        );
        latency.insert(
            Key::new("silence-vad-max"),
            Item::from(config.audio.latency.silence_vad_max as i64),
            arena,
        );
        latency.insert(
            Key::new("silence-min-gap-ms"),
            Item::from(config.audio.latency.silence_min_gap_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("silence-guard-ms"),
            Item::from(config.audio.latency.silence_guard_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("silence-ramp-ms"),
            Item::from(config.audio.latency.silence_ramp_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("silence-max-skip-ms"),
            Item::from(config.audio.latency.silence_max_skip_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("silence-min-skip-ms"),
            Item::from(config.audio.latency.silence_min_skip_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("capture-long-silence-stop-ms"),
            Item::from(config.audio.latency.capture_long_silence_stop_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("capture-silence-preroll-ms"),
            Item::from(config.audio.latency.capture_silence_preroll_ms as i64),
            arena,
        );
        latency.insert(
            Key::new("capture-silence-ramp-ms"),
            Item::from(config.audio.latency.capture_silence_ramp_ms as i64),
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

    root.remove_entry("user-audio");
    if !config.user_audio.is_empty() {
        let mut array = Array::try_with_capacity(config.user_audio.len(), arena)
            .expect("user audio preferences length must fit in TOML array");
        array.set_style(ArrayStyle::Header);
        for preference in &config.user_audio {
            let mut table = Table::new();
            insert_str(&mut table, "server-alias", &preference.server_alias, arena);
            table.insert(
                Key::new("user-id"),
                Item::from(preference.user_id as i64),
                arena,
            );
            table.insert(
                Key::new("volume-db"),
                Item::from(preference.volume_db as f64),
                arena,
            );
            array.push(table.into_item(), arena);
        }
        root.insert(Key::new("user-audio"), array.into_item(), arena);
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
            "chatt-config-legacy-input-device-index-{}.toml",
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

    #[test]
    fn runtime_config_writes_audio_amplification() {
        let config = Config::default();
        let arena = Arena::new();
        let doc = toml_spanner::parse(DEFAULT_CONFIG, &arena).unwrap();
        let mut table = doc.table().clone_in(&arena);

        write_runtime_config(&mut table, &config, &arena);
        let content = String::from_utf8(
            toml_spanner::Formatting::preserved_from(&doc)
                .with_span_projection_identity()
                .format_table_to_bytes(table, &arena),
        )
        .unwrap();

        assert!(content.contains("max-amplification = 2.0"));
    }

    #[test]
    fn runtime_config_writes_audio_device_ids() {
        let mut config = Config::default();
        config.audio.input_device_id = Some("usb mic".to_string());
        config.audio.output_device_id = Some("usb speakers".to_string());
        let arena = Arena::new();
        let doc = toml_spanner::parse(DEFAULT_CONFIG, &arena).unwrap();
        let mut table = doc.table().clone_in(&arena);

        write_runtime_config(&mut table, &config, &arena);
        let content = String::from_utf8(
            toml_spanner::Formatting::preserved_from(&doc)
                .with_span_projection_identity()
                .format_table_to_bytes(table, &arena),
        )
        .unwrap();

        assert!(content.contains("input-device-id = \"usb mic\""));
        assert!(content.contains("output-device-id = \"usb speakers\""));
    }

    #[test]
    fn audio_latency_config_defaults_match_live_tuning() {
        let config = AudioLatencyConfig::default();
        let tuning = config.to_tuning();

        assert_eq!(tuning, LiveAudioTuning::default());
        assert!(tuning.validate().is_ok());
        assert_eq!(Config::default().audio.bitrate_bps, 48_000);
    }

    #[test]
    fn runtime_config_writes_audio_latency_knobs() {
        let mut config = Config::default();
        config.audio.latency.playback_silence_skip = false;
        config.audio.latency.target_queue_ms = 80;
        let arena = Arena::new();
        let doc = toml_spanner::parse(DEFAULT_CONFIG, &arena).unwrap();
        let mut table = doc.table().clone_in(&arena);

        write_runtime_config(&mut table, &config, &arena);
        let content = String::from_utf8(
            toml_spanner::Formatting::preserved_from(&doc)
                .with_span_projection_identity()
                .format_table_to_bytes(table, &arena),
        )
        .unwrap();

        assert!(content.contains("[audio.latency]"));
        assert!(content.contains("playback-silence-skip = false"));
        assert!(content.contains("target-queue-ms = 80"));
    }

    #[test]
    fn rejects_invalid_audio_latency_config() {
        let mut config = Config::default();
        config.audio.latency.hard_queue_bound_ms = 40;
        config.audio.latency.dred_horizon_ms = 1_000;

        let error = config.validate("<test>").unwrap_err();

        assert!(error.contains("audio latency"));
        assert!(error.contains("hard-queue-bound-ms"));
    }

    #[test]
    fn rejects_invalid_audio_bitrate() {
        let mut config = Config::default();
        config.audio.bitrate_bps = 128_000;

        let error = config.validate("<test>").unwrap_err();

        assert!(error.contains("bitrate-bps"));
    }

    #[test]
    fn user_volume_preferences_snap_sort_and_remove_zero() {
        let mut config = Config::default();

        config.set_user_volume_db("local", 3, -5.4);
        config.set_user_volume_db("local", 2, 7.6);

        assert_eq!(config.user_volume_db("local", 3), -5.5);
        assert_eq!(config.user_volume_db("local", 2), 7.5);
        assert_eq!(config.user_audio[0].user_id, 2);
        assert_eq!(config.user_audio[1].user_id, 3);

        config.set_user_volume_db("local", 2, 0.0);

        assert_eq!(config.user_volume_db("local", 2), 0.0);
        assert_eq!(config.user_audio.len(), 1);
    }

    #[test]
    fn runtime_config_writes_user_audio_as_array_of_tables() {
        let mut config = Config::default();
        config.set_user_volume_db("local", 2, -5.5);
        let arena = Arena::new();
        let doc = toml_spanner::parse(DEFAULT_CONFIG, &arena).unwrap();
        let mut table = doc.table().clone_in(&arena);

        write_runtime_config(&mut table, &config, &arena);
        let content = String::from_utf8(
            toml_spanner::Formatting::preserved_from(&doc)
                .with_span_projection_identity()
                .format_table_to_bytes(table, &arena),
        )
        .unwrap();

        assert!(content.contains("[[user-audio]]"));
        assert!(content.contains("server-alias = \"local\""));
        assert!(content.contains("user-id = 2"));
        assert!(content.contains("volume-db = -5.5"));
    }
}
