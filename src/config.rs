use hashbrown::HashSet;
use std::path::PathBuf;
use std::{fs, time::Duration};

use toml_spanner::Toml;
use toml_spanner::{Arena, Item};

use crate::{
    audio::{
        BufferRequest, DenoiseConfig, DenoiseSuppression, DenoiseTypingSuppression,
        LiveAudioPacketLossProfile, LiveAudioTuning, NotificationSound,
    },
    bindings::BindingRuntime,
    client_net::ClientConfig,
};
use rpc::{control::DEFAULT_FILE_SIZE_LIMIT_BYTES, ids::RoomId};

pub const DEFAULT_CONFIG: &str = include_str!("../chatt.toml");
pub const DEFAULT_MAX_AMPLIFICATION: f32 = crate::audio::DEFAULT_LIVE_MAX_AMPLIFICATION;
pub const DEFAULT_DENOISE_SUPPRESSION: f32 = crate::audio::DEFAULT_DENOISE_SUPPRESSION;
pub const DEFAULT_DENOISE_RELEASE: f32 = crate::audio::DEFAULT_DENOISE_RELEASE;
pub const DEFAULT_DENOISE_TYPING_SUPPRESSION: bool =
    crate::audio::DEFAULT_DENOISE_TYPING_SUPPRESSION;
pub const DEFAULT_DENOISE_TYPING_VAD_ENTER: f32 = crate::audio::DEFAULT_DENOISE_TYPING_VAD_ENTER;
pub const DEFAULT_DENOISE_TYPING_VAD_RELEASE: f32 =
    crate::audio::DEFAULT_DENOISE_TYPING_VAD_RELEASE;
pub const DEFAULT_DENOISE_TYPING_RELEASE_MS: u64 = crate::audio::DEFAULT_DENOISE_TYPING_RELEASE_MS;
pub const MIN_USER_VOLUME_DB: f32 = -24.0;
pub const MAX_USER_VOLUME_DB: f32 = 12.0;
pub const USER_VOLUME_DB_STEP: f32 = 0.5;
pub const MIN_NOTIFICATION_VOLUME_DB: f32 = -24.0;
pub const MAX_NOTIFICATION_VOLUME_DB: f32 = 12.0;
pub const NOTIFICATION_VOLUME_DB_STEP: f32 = 0.5;

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct ServerEntry {
    pub alias: String,
    pub tcp_addr: String,
    #[toml(default, ToToml skip_if = String::is_empty)]
    pub udp_addr: String,
    #[toml(default)]
    pub udp_probe_addr: Option<String>,
    /// Deprecated. Older configs stored an admin-chosen `user` identifier here.
    /// The server now identifies clients by token, so this value is accepted for
    /// backward compatibility, ignored, and dropped on the next config save.
    #[toml(default, rename = "user", ToToml skip)]
    #[allow(dead_code)]
    pub legacy_user: String,
    pub display_name: String,
    pub token: String,
    #[toml(default)]
    pub server_public_key: String,
    pub room_id: u32,
}

impl Default for ServerEntry {
    fn default() -> Self {
        Self {
            alias: default_server_alias(),
            tcp_addr: "127.0.0.1:41000".to_string(),
            udp_addr: String::new(),
            udp_probe_addr: None,
            legacy_user: String::new(),
            display_name: "Alice".to_string(),
            token: "alice-dev-token".to_string(),
            server_public_key: String::new(),
            room_id: 1,
        }
    }
}

impl ServerEntry {
    pub fn client_config(&self, files: &FileConfig, p2p: &P2pConfig) -> ClientConfig {
        ClientConfig {
            tcp_addr: self.tcp_addr.clone(),
            udp_addr: self.effective_udp_addr(),
            udp_probe_addr: self.udp_probe_addr.clone(),
            display_name: self.effective_display_name(),
            token: self.token.clone(),
            server_public_key: non_empty_string(&self.server_public_key),
            room_id: RoomId(self.room_id),
            file_receive_dir: files.receive_dir_path(),
            max_upload_bytes: files.max_upload_bytes,
            max_receive_bytes: files.max_receive_bytes,
            candidate_privacy: p2p.candidate_privacy,
            prefer_ipv6: p2p.prefer_ipv6,
        }
    }

    pub fn effective_display_name(&self) -> String {
        self.display_name.trim().to_string()
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

/// The cpal device period to request for one audio stream, in samples.
///
/// `Default` resolves to a usage-appropriate fixed size (see
/// [`DEFAULT_INPUT_BUFFER_SAMPLES`]/[`DEFAULT_OUTPUT_BUFFER_SAMPLES`]) rather
/// than the host default; `Samples(n)` requests exactly `n` frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum BufferSize {
    #[default]
    Default,
    Samples(u32),
}

/// Sample count `input-buffer.samples = "default"` resolves to: one 20 ms Opus
/// frame at 48 kHz, which keeps capture wakeups aligned to the encoder frame.
pub const DEFAULT_INPUT_BUFFER_SAMPLES: u32 = 960;
/// Sample count `output-buffer.samples = "default"` resolves to: one 10 ms
/// playback quantum at 48 kHz, lowering latency on hosts that honor fixed
/// output periods.
pub const DEFAULT_OUTPUT_BUFFER_SAMPLES: u32 = 480;

impl BufferSize {
    /// Resolves the configured size into a [`BufferRequest`], substituting
    /// `default_samples` for [`BufferSize::Default`].
    pub fn to_request(self, default_samples: u32) -> BufferRequest {
        match self {
            BufferSize::Default => BufferRequest::Fixed(default_samples),
            BufferSize::Samples(samples) => BufferRequest::Fixed(samples),
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
    #[toml(default)]
    pub denoise: DenoiseConfig,
    #[toml(default = false)]
    pub echo_cancellation: bool,
    #[toml(default = DEFAULT_MAX_AMPLIFICATION)]
    pub max_amplification: f32,
    #[toml(default = DEFAULT_DENOISE_SUPPRESSION)]
    pub denoise_suppression: f32,
    #[toml(default = DEFAULT_DENOISE_RELEASE)]
    pub denoise_release: f32,
    #[toml(default = DEFAULT_DENOISE_TYPING_SUPPRESSION)]
    pub denoise_typing_suppression: bool,
    #[toml(default = DEFAULT_DENOISE_TYPING_VAD_ENTER)]
    pub denoise_typing_vad_enter: f32,
    #[toml(default = DEFAULT_DENOISE_TYPING_VAD_RELEASE)]
    pub denoise_typing_vad_release: f32,
    #[toml(default, style = Dotted)]
    pub input_buffer: BufferSize,
    #[toml(default, style = Dotted)]
    pub output_buffer: BufferSize,
    #[toml(default, style = Header)]
    pub latency: AudioLatencyConfig,
}

impl AudioConfig {
    /// Bundles the two flat RNNoise tuning fields into a [`DenoiseSuppression`].
    pub fn suppression(&self) -> DenoiseSuppression {
        DenoiseSuppression {
            strength: self.denoise_suppression,
            release: self.denoise_release,
        }
    }

    pub fn typing_suppression(&self) -> DenoiseTypingSuppression {
        DenoiseTypingSuppression {
            enabled: self.denoise_typing_suppression,
            vad_enter: self.denoise_typing_vad_enter,
            vad_release: self.denoise_typing_vad_release,
            release_confirm: Duration::from_millis(DEFAULT_DENOISE_TYPING_RELEASE_MS),
        }
        .normalized()
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device_id: None,
            output_device_id: None,
            bitrate_bps: 48_000,
            denoise: DenoiseConfig::RnnNoise,
            echo_cancellation: false,
            max_amplification: DEFAULT_MAX_AMPLIFICATION,
            denoise_suppression: DEFAULT_DENOISE_SUPPRESSION,
            denoise_release: DEFAULT_DENOISE_RELEASE,
            denoise_typing_suppression: DEFAULT_DENOISE_TYPING_SUPPRESSION,
            denoise_typing_vad_enter: DEFAULT_DENOISE_TYPING_VAD_ENTER,
            denoise_typing_vad_release: DEFAULT_DENOISE_TYPING_VAD_RELEASE,
            input_buffer: BufferSize::Default,
            output_buffer: BufferSize::Default,
            latency: AudioLatencyConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct AudioLatencyConfig {
    #[toml(default = true)]
    pub capture_silence_gate: bool,
    #[toml(default = 60)]
    pub target_queue_ms: u64,
    #[toml(default = 20)]
    pub dynamic_target_floor_ms: u64,
    #[toml(default = 0)]
    pub base_minimum_target_ms: u64,
    #[toml(default = 1_000)]
    pub max_target_ms: u64,
    #[toml(default = 1_500)]
    pub hard_queue_bound_ms: u64,
    #[toml(default = 40)]
    pub initial_buffer_ms: u64,
    #[toml(default = 60)]
    pub max_reorder_delay_ms: u64,
    #[toml(default = 20)]
    pub device_period_margin_ms: u64,
    #[toml(default = 64)]
    pub silence_vad_max: u8,
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
            capture_silence_gate: tuning.capture_silence_gate,
            target_queue_ms: duration_ms(tuning.target_queue),
            dynamic_target_floor_ms: duration_ms(tuning.dynamic_target_floor),
            base_minimum_target_ms: duration_ms(tuning.base_minimum_target),
            max_target_ms: duration_ms(tuning.max_target),
            hard_queue_bound_ms: duration_ms(tuning.hard_queue_bound),
            initial_buffer_ms: duration_ms(tuning.initial_buffer),
            max_reorder_delay_ms: duration_ms(tuning.max_reorder_delay),
            device_period_margin_ms: duration_ms(tuning.device_period_margin),
            silence_vad_max: tuning.silence_vad_max,
            capture_long_silence_stop_ms: duration_ms(tuning.capture_long_silence_stop),
            capture_silence_preroll_ms: duration_ms(tuning.capture_silence_preroll),
            capture_silence_ramp_ms: duration_ms(tuning.capture_silence_ramp),
        }
    }
}

impl AudioLatencyConfig {
    pub fn to_tuning(&self) -> LiveAudioTuning {
        LiveAudioTuning {
            capture_silence_gate: self.capture_silence_gate,
            target_queue: Duration::from_millis(self.target_queue_ms),
            dynamic_target_floor: Duration::from_millis(self.dynamic_target_floor_ms),
            base_minimum_target: Duration::from_millis(self.base_minimum_target_ms),
            max_target: Duration::from_millis(self.max_target_ms),
            hard_queue_bound: Duration::from_millis(self.hard_queue_bound_ms),
            initial_buffer: Duration::from_millis(self.initial_buffer_ms),
            max_reorder_delay: Duration::from_millis(self.max_reorder_delay_ms),
            device_period_margin: Duration::from_millis(self.device_period_margin_ms),
            silence_vad_max: self.silence_vad_max,
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
    #[toml(default)]
    pub form_bindings: FormBindings,
    #[toml(default)]
    pub theme: ThemeChoice,
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
            form_bindings: FormBindings::Vim,
            theme: ThemeChoice::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum FormBindings {
    Standard,
    #[default]
    Vim,
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

/// How local host candidates are exposed to peers, mirroring RFC 8828.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum CandidatePrivacy {
    /// Publish host candidates as random `.local` names resolved via mDNS.
    #[default]
    Mdns,
    /// Publish host candidates as literal IP addresses.
    Disabled,
    /// Suppress host candidates entirely, relying on reflexive and relay.
    NoHost,
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct P2pConfig {
    #[toml(default)]
    pub candidate_privacy: CandidatePrivacy,
    /// Prefer native IPv6 over IPv4 at equal candidate type (RFC 8421). Set
    /// `false` to revert to IPv4-first for diagnostics.
    #[toml(default = true)]
    pub prefer_ipv6: bool,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            candidate_privacy: CandidatePrivacy::default(),
            prefer_ipv6: true,
        }
    }
}

impl P2pConfig {
    pub fn is_default(&self) -> bool {
        *self == P2pConfig::default()
    }
}

/// The optional browser chat-log view served over `darkhttp`.
#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct WebConfig {
    /// Whether to start the web server when the client runs.
    #[toml(default)]
    pub enabled: bool,
    /// The loopback address the web server binds.
    #[toml(default = default_web_bind())]
    pub bind: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_web_bind(),
        }
    }
}

impl WebConfig {
    pub fn is_default(&self) -> bool {
        *self == WebConfig::default()
    }
}

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct NotificationConfig {
    #[toml(default)]
    pub message_volume_db: f32,
    #[toml(default)]
    pub peer_join_volume_db: f32,
    #[toml(default)]
    pub peer_leave_volume_db: f32,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            message_volume_db: 0.0,
            peer_join_volume_db: 0.0,
            peer_leave_volume_db: 0.0,
        }
    }
}

impl NotificationConfig {
    pub fn volume_db(&self, sound: NotificationSound) -> f32 {
        match sound {
            NotificationSound::MessageReceived => self.message_volume_db,
            NotificationSound::PeerJoin => self.peer_join_volume_db,
            NotificationSound::PeerLeave => self.peer_leave_volume_db,
        }
    }

    pub fn is_default(&self) -> bool {
        *self == NotificationConfig::default()
    }
}

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct UserAudioPreference {
    pub server_alias: String,
    pub user_id: u32,
    pub volume_db: f32,
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct SoundboardClip {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct SoundboardConfig {
    #[toml(default)]
    pub enabled: bool,
    #[toml(default = default_soundboard_loss())]
    pub loss: String,
    #[toml(default = default_soundboard_seed())]
    pub seed: u64,
    #[toml(default, style = Header)]
    pub clips: Vec<SoundboardClip>,
}

impl Default for SoundboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            loss: default_soundboard_loss(),
            seed: default_soundboard_seed(),
            clips: Vec::new(),
        }
    }
}

impl SoundboardConfig {
    pub fn packet_loss(&self) -> Option<LiveAudioPacketLossProfile> {
        LiveAudioPacketLossProfile::from_name(self.loss.trim())
    }

    /// Whether this matches a freshly defaulted soundboard, in which case saves
    /// omit the `[soundboard]` section entirely.
    pub fn is_default(&self) -> bool {
        *self == SoundboardConfig::default()
    }
}

fn default_soundboard_loss() -> String {
    "congested_wifi".to_string()
}

fn default_soundboard_seed() -> u64 {
    0x746f_6d63_6861_7405
}

/// Selects which builtin color theme the UI renders with. A future custom mode
/// can add a variant (or a sibling `[theme]` table) without breaking configs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum ThemeChoice {
    /// The original dark pastel palette, in 24-bit RGB.
    #[default]
    TomorrowNight,
    /// A 16-color base16 palette for dark terminals.
    Base16Dark,
    /// A 16-color base16 palette for light terminals.
    Base16Light,
}

impl ThemeChoice {
    /// The builtin themes in cycle order.
    pub const ALL: [ThemeChoice; 3] = [
        ThemeChoice::TomorrowNight,
        ThemeChoice::Base16Dark,
        ThemeChoice::Base16Light,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::TomorrowNight => "Tomorrow Night",
            ThemeChoice::Base16Dark => "Base16 Dark",
            ThemeChoice::Base16Light => "Base16 Light",
        }
    }
}

#[derive(Toml)]
#[toml(
    FromToml,
    ToToml,
    recoverable,
    warn_unknown_fields,
    rename_all = "kebab-case"
)]
pub struct Config {
    #[allow(dead_code)]
    #[toml(default, rename = "active-server", ToToml skip)]
    legacy_active_server: Option<String>,
    #[toml(default = default_servers(), style = Header)]
    pub servers: Vec<ServerEntry>,
    #[toml(default, style = Header)]
    pub audio: AudioConfig,
    #[toml(default, style = Header)]
    pub ui: UiConfig,
    #[toml(default, style = Header)]
    pub files: FileConfig,
    #[toml(default, style = Header, ToToml skip_if = P2pConfig::is_default)]
    pub p2p: P2pConfig,
    #[toml(default, style = Header, ToToml skip_if = WebConfig::is_default)]
    pub web: WebConfig,
    #[toml(default, style = Header, ToToml skip_if = NotificationConfig::is_default)]
    pub notifications: NotificationConfig,
    #[toml(default, style = Header, ToToml skip_if = Vec::is_empty)]
    pub user_audio: Vec<UserAudioPreference>,
    // Serialized so a save never drops a configured soundboard; an unconfigured
    // (default) soundboard is omitted so saves don't inject an empty section.
    #[toml(default, style = Header, ToToml skip_if = SoundboardConfig::is_default)]
    pub soundboard: SoundboardConfig,
    // `BindingRuntime` re-emits the `[bindings]` table it was parsed from, so a
    // save preserves custom keybindings verbatim. See its `ToToml` impl.
    #[toml(default, style = Implicit)]
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
            server.display_name = server.effective_display_name();
        }
        for preference in &mut self.user_audio {
            preference.server_alias = preference.server_alias.trim().to_string();
            preference.volume_db = snap_user_volume_db(preference.volume_db);
        }
        self.user_audio
            .retain(|preference| preference.volume_db != 0.0);
        self.sort_user_audio();
        self.web.bind = self.web.bind.trim().to_string();
        self.notifications.message_volume_db =
            snap_notification_volume_db(self.notifications.message_volume_db);
        self.notifications.peer_join_volume_db =
            snap_notification_volume_db(self.notifications.peer_join_volume_db);
        self.notifications.peer_leave_volume_db =
            snap_notification_volume_db(self.notifications.peer_leave_volume_db);
        self.soundboard.loss = self.soundboard.loss.trim().to_string();
        for clip in &mut self.soundboard.clips {
            clip.name = clip.name.trim().to_string();
            clip.path = clip.path.trim().to_string();
        }
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
        self.web
            .bind
            .parse::<std::net::SocketAddr>()
            .map_err(|error| format!("{source}: web bind must be a socket address: {error}"))?;
        for (name, volume_db) in [
            ("message-volume-db", self.notifications.message_volume_db),
            (
                "peer-join-volume-db",
                self.notifications.peer_join_volume_db,
            ),
            (
                "peer-leave-volume-db",
                self.notifications.peer_leave_volume_db,
            ),
        ] {
            if !(MIN_NOTIFICATION_VOLUME_DB..=MAX_NOTIFICATION_VOLUME_DB).contains(&volume_db) {
                return Err(format!(
                    "{source}: notifications {name} must be between {:.1} and {:.1}",
                    MIN_NOTIFICATION_VOLUME_DB, MAX_NOTIFICATION_VOLUME_DB
                ));
            }
        }
        let mut aliases = HashSet::new();
        for server in &self.servers {
            validate_server_alias(&server.alias)
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
        let mut user_audio_keys = HashSet::new();
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
        if self.soundboard.enabled {
            if self.soundboard.packet_loss().is_none() {
                return Err(format!(
                    "{source}: soundboard loss must be one of: {}",
                    LiveAudioPacketLossProfile::NAMES.join(", ")
                ));
            }
            if self.soundboard.clips.is_empty() {
                return Err(format!(
                    "{source}: soundboard enabled but no [[soundboard.clips]] entries are configured"
                ));
            }
        }
        for (index, clip) in self.soundboard.clips.iter().enumerate() {
            if clip.name.is_empty() {
                return Err(format!(
                    "{source}: soundboard clip {}: name must not be empty",
                    index + 1
                ));
            }
            if clip.path.is_empty() {
                return Err(format!(
                    "{source}: soundboard clip {}: path must not be empty",
                    index + 1
                ));
            }
        }
        Ok(())
    }

    pub fn server(&self, alias: &str) -> Result<&ServerEntry, String> {
        self.servers
            .iter()
            .find(|server| server.alias == alias)
            .ok_or_else(|| format!("server {alias} is not configured"))
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

        let output = self
            .runtime_toml(&content)
            .map_err(|err| format!("failed to serialize {}: {err}", path.display()))?;
        fs::write(&path, output)
            .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        Ok(path)
    }

    /// Serializes the runtime config, reusing the comments, key order, and
    /// value styling of `existing` for the keys they share.
    ///
    /// Content comes entirely from `self`: every section the file should keep
    /// (including `[soundboard]` and `[bindings]`) is serialized by `Config`'s
    /// [`ToToml`] impl, so format preservation only restyles, never re-adds,
    /// keys.
    fn runtime_toml(&self, existing: &str) -> Result<String, String> {
        let arena = Arena::new();
        let doc = toml_spanner::parse(existing, &arena)
            .map_err(|err| format!("failed to parse existing config: {err}"))?;
        toml_spanner::Formatting::preserved_from(&doc)
            .format(self)
            .map_err(|err| err.to_string())
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

pub fn snap_notification_volume_db(volume_db: f32) -> f32 {
    if !volume_db.is_finite() {
        return 0.0;
    }
    let snapped = (volume_db / NOTIFICATION_VOLUME_DB_STEP).round() * NOTIFICATION_VOLUME_DB_STEP;
    let clamped = snapped.clamp(MIN_NOTIFICATION_VOLUME_DB, MAX_NOTIFICATION_VOLUME_DB);
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
            "failed to deserialize {source}: [network] is not supported; use [[servers]]"
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
    const REMOVED_AUDIO_LATENCY_KEYS: &[&str] = &[
        "playback-silence-skip",
        "dynamic-target-margin-ms",
        "dynamic-jitter-gain",
        "dynamic-peak-weight",
        "moderate-loss-queue-ms",
        "dred-horizon-ms",
        "loss-window-ms",
        "loss-hold-ms",
        "severe-loss-hold-ms",
        "max-speed-up",
        "catch-up-start-excess-ms",
        "silence-min-gap-ms",
        "silence-ramp-ms",
        "silence-max-skip-ms",
        "silence-min-skip-ms",
    ];
    if let Some(key) = doc
        .table()
        .get("audio")
        .and_then(Item::as_table)
        .and_then(|audio| audio.get("latency"))
        .and_then(Item::as_table)
        .and_then(|latency| {
            REMOVED_AUDIO_LATENCY_KEYS
                .iter()
                .find(|key| latency.contains_key(**key))
        })
    {
        return Err(format!(
            "failed to deserialize {source}: audio.latency.{key} was removed by the WSOLA playback refactor"
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

fn default_server_alias() -> String {
    "local".to_string()
}

fn default_web_bind() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_servers() -> Vec<ServerEntry> {
    Vec::new()
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

pub fn validate_server_entry(server: &ServerEntry) -> Result<(), String> {
    validate_server_alias(&server.alias)?;
    validate_non_empty(&server.token, "token")?;
    validate_display_name(&server.display_name)?;
    validate_endpoint(&server.tcp_addr, "tcp-addr")?;
    validate_endpoint(&server.effective_udp_addr(), "udp-addr")?;
    if let Some(addr) = &server.udp_probe_addr {
        validate_endpoint(addr, "udp-probe-addr")?;
    }
    if server.room_id == 0 {
        return Err("room-id must be non-zero".to_string());
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
    fn server_ignores_legacy_user_key() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
alias = "lab"
user = "stale-internal-name"
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
        config.normalize();

        config.validate("<test>").unwrap();
        assert_eq!(config.servers[0].display_name, "Alice");
    }

    #[test]
    fn server_parses_explicit_udp_and_probe_addrs() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
alias = "lab"
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

        let server = config.server("prod").unwrap();
        assert_eq!(server.tcp_addr, "chat.example.com:443");
        assert_eq!(server.udp_addr, "media.example.com:54100");
        assert_eq!(
            server.udp_probe_addr.as_deref(),
            Some("probe.example.com:54101")
        );
    }

    #[test]
    fn soundboard_config_parses_clip_and_loss_profile() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
alias = "lab"
display-name = "Carol"
token = "carol-dev-token"
tcp-addr = "127.0.0.1:42000"
server-public-key = ""
room-id = 1

[soundboard]
enabled = true
loss = "random_60"
seed = 123

[[soundboard.clips]]
name = "sample"
path = "assets/sample-001.opus"
"#,
            &arena,
        )
        .unwrap();
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses_from_doc(&udp_addr_configured);
        config.normalize();

        config.validate("test").unwrap();
        assert!(config.soundboard.enabled);
        assert_eq!(
            config.soundboard.packet_loss(),
            Some(LiveAudioPacketLossProfile::Random60)
        );
        assert_eq!(config.soundboard.clips[0].name, "sample");
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

    fn render_runtime(config: &Config) -> String {
        config.runtime_toml(DEFAULT_CONFIG).unwrap()
    }

    #[test]
    fn runtime_config_writes_servers_without_legacy_keys() {
        let mut config = Config::default();
        config.servers.push(ServerEntry::default());
        let content = render_runtime(&config);

        assert!(content.contains("[[servers]]"));
        assert!(!content.contains("active-server"));
        assert!(!content.contains("pairing-code"));
    }

    #[test]
    fn theme_choice_parses_each_builtin() {
        let arena = Arena::new();
        for (text, expected) in [
            (
                "[ui]\ntheme = \"tomorrow-night\"\n",
                ThemeChoice::TomorrowNight,
            ),
            ("[ui]\ntheme = \"base16-dark\"\n", ThemeChoice::Base16Dark),
            ("[ui]\ntheme = \"base16-light\"\n", ThemeChoice::Base16Light),
        ] {
            let config: Config = toml_spanner::parse(text, &arena).unwrap().to().unwrap();
            assert_eq!(config.ui.theme, expected);
        }
    }

    #[test]
    fn theme_choice_defaults_to_tomorrow_night() {
        let arena = Arena::new();
        let config: Config = toml_spanner::parse("", &arena).unwrap().to().unwrap();
        assert_eq!(config.ui.theme, ThemeChoice::TomorrowNight);
    }

    #[test]
    fn runtime_config_round_trips_theme() {
        let mut config = Config::default();
        config.ui.theme = ThemeChoice::Base16Dark;
        let content = render_runtime(&config);
        assert!(content.contains("theme = \"base16-dark\""));

        let arena = Arena::new();
        let parsed: Config = toml_spanner::parse(&content, &arena).unwrap().to().unwrap();
        assert_eq!(parsed.ui.theme, ThemeChoice::Base16Dark);
    }

    #[test]
    fn runtime_config_writes_audio_amplification() {
        let content = render_runtime(&Config::default());

        assert!(content.contains("max-amplification = 12.0"));
    }

    #[test]
    fn runtime_config_writes_audio_device_ids() {
        let mut config = Config::default();
        config.audio.input_device_id = Some("usb mic".to_string());
        config.audio.output_device_id = Some("usb speakers".to_string());
        let content = render_runtime(&config);

        assert!(content.contains("input-device-id = \"usb mic\""));
        assert!(content.contains("output-device-id = \"usb speakers\""));
    }

    #[test]
    fn runtime_config_omits_unset_audio_device_ids() {
        let content = render_runtime(&Config::default());

        assert!(!content.contains("input-device-id"));
        assert!(!content.contains("output-device-id"));
    }

    #[test]
    fn runtime_config_writes_audio_echo_cancellation() {
        let mut config = Config::default();
        config.audio.echo_cancellation = true;
        let content = render_runtime(&config);

        assert!(content.contains("echo-cancellation = true"));
    }

    #[test]
    fn runtime_config_writes_split_buffer_sizes() {
        let mut config = Config::default();
        config.audio.input_buffer = BufferSize::Samples(1024);
        config.audio.output_buffer = BufferSize::Default;
        let content = render_runtime(&config);

        assert!(content.contains("input-buffer.samples = 1024"));
        assert!(content.contains("output-buffer = \"default\""));
    }

    #[test]
    fn runtime_config_preserves_bindings_and_comments() {
        let existing = concat!(
            "[audio]\n",
            "input-buffer = \"default\" # keep me\n",
            "\n",
            "[bindings.compose]\n",
            "\"C-x\" = \"Quit\"\n",
        );
        let arena = Arena::new();
        let config: Config = toml_spanner::parse(existing, &arena).unwrap().to().unwrap();
        let content = config.runtime_toml(existing).unwrap();

        assert!(content.contains("# keep me"));
        assert!(content.contains("[bindings.compose]"));
        assert!(content.contains("\"C-x\" = \"Quit\""));
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
    fn typing_suppression_clamps_release_threshold_above_enter() {
        let audio = AudioConfig {
            denoise_typing_suppression: true,
            denoise_typing_vad_enter: 0.85,
            denoise_typing_vad_release: 0.70,
            ..AudioConfig::default()
        };

        let gate = audio.typing_suppression();

        assert!(gate.enabled);
        assert_eq!(gate.vad_enter, 0.85);
        assert_eq!(gate.vad_release, 0.85);
    }

    #[test]
    fn runtime_config_writes_audio_latency_knobs() {
        let mut config = Config::default();
        config.audio.latency.target_queue_ms = 80;
        config.audio.latency.dynamic_target_floor_ms = 25;
        config.audio.latency.base_minimum_target_ms = 90;
        config.audio.latency.max_target_ms = 1_200;
        let content = render_runtime(&config);

        assert!(content.contains("[audio.latency]"));
        assert!(content.contains("target-queue-ms = 80"));
        assert!(content.contains("dynamic-target-floor-ms = 25"));
        assert!(content.contains("base-minimum-target-ms = 90"));
        assert!(content.contains("max-target-ms = 1200"));
    }

    #[test]
    fn rejects_invalid_audio_latency_config() {
        let mut config = Config::default();
        config.audio.latency.hard_queue_bound_ms = 40;
        config.audio.latency.max_target_ms = 1_000;

        let error = config.validate("<test>").unwrap_err();

        assert!(error.contains("audio latency"));
        assert!(error.contains("hard-queue-bound-ms"));
    }

    #[test]
    fn audio_latency_max_target_must_cover_target_queue() {
        let mut tuning = LiveAudioTuning::default();
        tuning.max_target = tuning.target_queue;
        assert!(tuning.validate().is_ok());

        tuning.max_target = Duration::from_millis(20);
        tuning.target_queue = Duration::from_millis(60);
        let error = tuning.validate().unwrap_err();
        assert!(error.contains("max-target-ms"));
    }

    #[test]
    fn audio_latency_base_minimum_must_fit_constraints() {
        let mut tuning = LiveAudioTuning::default();
        tuning.base_minimum_target = Duration::from_millis(80);
        assert!(tuning.validate().is_ok());

        tuning.base_minimum_target = Duration::from_millis(1_200);
        let error = tuning.validate().unwrap_err();
        assert!(error.contains("base-minimum-target-ms"));
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
        let content = render_runtime(&config);

        assert!(content.contains("[[user-audio]]"));
        assert!(content.contains("server-alias = \"local\""));
        assert!(content.contains("user-id = 2"));
        assert!(content.contains("volume-db = -5.5"));
    }
}
