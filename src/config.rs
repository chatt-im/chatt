use extui::{AnsiColor, Color, Rgb};
use hashbrown::HashSet;
use rpc::ids::{RoomId, UserId};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{fs, time::Duration};

use toml_spanner::Toml;
use toml_spanner::{
    Arena, Context, Failed, FromToml, Item, OwnedTable, Table, ToToml, ToTomlError,
};

use crate::{
    audio::{
        BufferRequest, DenoiseConfig, DenoiseSuppression, DenoiseTypingSuppression, DredConfig,
        LiveAudioPacketLossProfile, LiveAudioTuning, NotificationSound,
    },
    bindings::BindingRuntime,
    client_net::{ClientConfig, FilePolicy},
    config_diagnostics::{self, Diag},
    paths,
};
use rpc::control::DEFAULT_FILE_SIZE_LIMIT_BYTES;

pub const MIB: u64 = 1024 * 1024;
pub const DEFAULT_FILE_SIZE_LIMIT_MB: u64 = DEFAULT_FILE_SIZE_LIMIT_BYTES / MIB;
pub const DEFAULT_MAX_UPLOAD_MB: u64 = 4096;

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
pub const DEFAULT_OUTPUT_VOLUME_PERCENT: f32 = crate::audio::DEFAULT_OUTPUT_VOLUME_PERCENT;
pub const MAX_OUTPUT_VOLUME_PERCENT: f32 = crate::audio::MAX_OUTPUT_VOLUME_PERCENT;
pub const MIN_USER_VOLUME_DB: f32 = -24.0;
pub const MAX_USER_VOLUME_DB: f32 = 12.0;
pub const USER_VOLUME_DB_STEP: f32 = 0.5;
pub const MIN_NOTIFICATION_VOLUME_DB: f32 = -24.0;
pub const MAX_NOTIFICATION_VOLUME_DB: f32 = 12.0;
pub const NOTIFICATION_VOLUME_DB_STEP: f32 = 0.5;

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct ServerEntry {
    pub label: String,
    pub tcp_addr: String,
    #[toml(default, ToToml skip_if = String::is_empty)]
    pub udp_addr: String,
    #[toml(default)]
    pub udp_probe_addr: Option<String>,
    pub username: String,
    pub token: String,
    #[toml(default)]
    pub server_public_key: String,
    #[toml(default = true, ToToml skip_if = |value: &bool| *value)]
    pub require_native_encryption: bool,
    #[toml(default, style = Header, ToToml skip_if = FileOverrides::is_empty)]
    pub files: FileOverrides,
    #[toml(default, style = Header, ToToml skip_if = HistoryOverrides::is_empty)]
    pub history: HistoryOverrides,
    #[toml(default, style = Header, ToToml skip_if = Vec::is_empty)]
    pub rooms: Vec<RoomOverrides>,
}

impl Default for ServerEntry {
    fn default() -> Self {
        Self {
            label: default_server_alias(),
            tcp_addr: "127.0.0.1:41000".to_string(),
            udp_addr: String::new(),
            udp_probe_addr: None,
            username: "Alice".to_string(),
            token: "alice-dev-token".to_string(),
            server_public_key: String::new(),
            require_native_encryption: true,
            files: FileOverrides::default(),
            history: HistoryOverrides::default(),
            rooms: Vec::new(),
        }
    }
}

impl ServerEntry {
    pub fn client_config(
        &self,
        config: &Config,
        download_store: crate::receive_store::DownloadStore,
    ) -> ClientConfig {
        ClientConfig {
            tcp_addr: self.tcp_addr.clone(),
            udp_addr: self.effective_udp_addr(),
            udp_probe_addr: self.udp_probe_addr.clone(),
            display_name: self.effective_display_name(),
            token: self.token.clone(),
            server_public_key: non_empty_string(&self.server_public_key),
            require_native_encryption: self.require_native_encryption,
            file_policy: config.file_policy(self),
            download_store,
            max_upload_bytes: config.files.max_upload_bytes(),
            upload_rate_bytes: config.files.upload_rate_bytes,
            p2p_enabled: config.p2p.enabled,
            candidate_privacy: config.p2p.candidate_privacy,
            prefer_ipv6: config.p2p.prefer_ipv6,
        }
    }

    pub fn effective_display_name(&self) -> String {
        self.username.trim().to_string()
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
/// `Default` resolves to auto mode: a target latency (see
/// [`DEFAULT_INPUT_TARGET_LATENCY`]/[`DEFAULT_OUTPUT_TARGET_LATENCY`]) that the
/// cpal backend negotiates into a natively-supported device period, rather than
/// a forced fixed size. `Samples(n)` requests exactly `n` frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum BufferSize {
    #[default]
    Default,
    Samples(u32),
}

/// Target capture period `input-buffer.samples = "default"` resolves to: a 10 ms
/// frame, matching WebRTC's fixed capture frame. The device keeps its own native
/// period; the 20 ms Opus packet is assembled by the capture repacketizer.
pub const DEFAULT_INPUT_TARGET_LATENCY: Duration = Duration::from_millis(10);
/// Target playback period `output-buffer.samples = "default"` resolves to: a
/// ~10 ms period. cpal snaps playback to the nearest power of two (512 at
/// 48 kHz) so the period aligns to the sink/graph clock.
pub const DEFAULT_OUTPUT_TARGET_LATENCY: Duration = Duration::from_millis(10);

impl BufferSize {
    /// Resolves the configured size into a [`BufferRequest`], substituting
    /// `default_target` (as an auto-negotiated latency) for [`BufferSize::Default`].
    pub fn to_request(self, default_target: Duration) -> BufferRequest {
        match self {
            BufferSize::Default => BufferRequest::Auto {
                target: default_target,
            },
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
    #[toml(default)]
    pub dred: DredConfig,
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
    #[toml(
        default = DEFAULT_OUTPUT_VOLUME_PERCENT,
        ToToml skip_if = is_default_output_volume_percent
    )]
    pub output_volume: f32,
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
            dred: DredConfig::default(),
            echo_cancellation: false,
            max_amplification: DEFAULT_MAX_AMPLIFICATION,
            denoise_suppression: DEFAULT_DENOISE_SUPPRESSION,
            denoise_release: DEFAULT_DENOISE_RELEASE,
            denoise_typing_suppression: DEFAULT_DENOISE_TYPING_SUPPRESSION,
            denoise_typing_vad_enter: DEFAULT_DENOISE_TYPING_VAD_ENTER,
            denoise_typing_vad_release: DEFAULT_DENOISE_TYPING_VAD_RELEASE,
            input_buffer: BufferSize::Default,
            output_buffer: BufferSize::Default,
            output_volume: DEFAULT_OUTPUT_VOLUME_PERCENT,
            latency: AudioLatencyConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct AudioLatencyConfig {
    #[toml(default = true)]
    pub capture_silence_gate: bool,
    #[toml(default = false)]
    pub render_assist: bool,
    #[toml(default = 60)]
    pub neteq_start_delay_ms: u64,
    #[toml(default = 20)]
    pub neteq_min_delay_ms: u64,
    #[toml(default = 0)]
    pub neteq_base_minimum_delay_ms: u64,
    #[toml(default = 1_000)]
    pub neteq_max_delay_ms: u64,
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
            render_assist: tuning.render_assist,
            neteq_start_delay_ms: duration_ms(tuning.neteq_start_delay),
            neteq_min_delay_ms: duration_ms(tuning.neteq_min_delay),
            neteq_base_minimum_delay_ms: duration_ms(tuning.neteq_base_minimum_delay),
            neteq_max_delay_ms: duration_ms(tuning.neteq_max_delay),
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
            render_assist: self.render_assist,
            neteq_start_delay: Duration::from_millis(self.neteq_start_delay_ms),
            neteq_min_delay: Duration::from_millis(self.neteq_min_delay_ms),
            neteq_base_minimum_delay: Duration::from_millis(self.neteq_base_minimum_delay_ms),
            neteq_max_delay: Duration::from_millis(self.neteq_max_delay_ms),
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
    #[toml(default = "Message #lobby".to_string())]
    pub placeholder: String,
    #[toml(default = 50_000)]
    pub max_messages: u32,
    #[toml(default = 24)]
    pub overscan: u32,
    #[toml(default)]
    pub default_bindings: DefaultBindings,
    #[toml(default)]
    pub theme: ThemeSelection,
    #[toml(default, style = Header, ToToml skip_if = ThemesConfig::is_empty)]
    pub themes: ThemesConfig,
}

/// The platform default URL opener: `open` on macOS, `xdg-open` on Linux, and
/// nothing elsewhere (clicks are then inert until `url-open` is configured).
fn default_url_open() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        vec!["open".to_string()]
    }
    #[cfg(target_os = "linux")]
    {
        vec!["xdg-open".to_string()]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// Whether `value` equals the platform default, so a save omits the key.
fn is_default_url_open(value: &Vec<String>) -> bool {
    *value == default_url_open()
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            room_height: 4,
            max_composer_height: 6,
            placeholder: "Message #lobby".to_string(),
            max_messages: 50_000,
            overscan: 24,
            default_bindings: DefaultBindings::Standard,
            theme: ThemeSelection::default(),
            themes: ThemesConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum DefaultBindings {
    #[default]
    Standard,
    Vim,
}

pub use DefaultBindings as FormBindings;

/// Default upload pacing ceiling. `0` disables throttling: uploads stream at
/// full socket speed.
pub const DEFAULT_UPLOAD_RATE_BYTES: u64 = 0;

/// Default capacity of the in-memory download ring buffer, in MiB.
pub const DEFAULT_DOWNLOAD_MEMORY_MB: u64 = 512;

/// How received files are handled. The single downloads switch, unified across
/// the settings UI and the `download` config key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub enum DownloadMode {
    /// Reject incoming files.
    Off,
    /// Hold received files in an in-memory ring buffer, never touching disk.
    #[default]
    Memory,
    /// Write received files to a directory on disk.
    Persistent,
}

impl DownloadMode {
    pub const ALL: [DownloadMode; 3] = [
        DownloadMode::Off,
        DownloadMode::Memory,
        DownloadMode::Persistent,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DownloadMode::Off => "off",
            DownloadMode::Memory => "memory",
            DownloadMode::Persistent => "persistent",
        }
    }
}

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct FileConfig {
    /// How received files are handled: dropped, kept in an in-memory ring
    /// buffer, or written to disk.
    #[toml(default)]
    pub download: DownloadMode,
    /// Directory received files are written to in [`DownloadMode::Persistent`].
    /// Empty falls back to the platform default download directory.
    #[toml(default)]
    pub download_dir: String,
    /// Capacity of the in-memory download ring buffer used by
    /// [`DownloadMode::Memory`], in MiB. One shared store, so this is a single
    /// global ceiling rather than a per-server or per-room setting.
    #[toml(default = DEFAULT_DOWNLOAD_MEMORY_MB)]
    pub download_memory_mb: u64,
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_MB)]
    pub max_download_mb: u64,
    #[toml(default = DEFAULT_MAX_UPLOAD_MB)]
    pub max_upload_mb: u64,
    /// Upload pacing ceiling in bytes per second. `0` streams at full socket
    /// speed. Primarily a test lever to stretch a transfer so its progress is
    /// observable, and a mild bandwidth cap.
    #[toml(default = DEFAULT_UPLOAD_RATE_BYTES)]
    pub upload_rate_bytes: u64,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            download: DownloadMode::Memory,
            download_dir: String::new(),
            download_memory_mb: DEFAULT_DOWNLOAD_MEMORY_MB,
            max_download_mb: DEFAULT_FILE_SIZE_LIMIT_MB,
            max_upload_mb: DEFAULT_MAX_UPLOAD_MB,
            upload_rate_bytes: DEFAULT_UPLOAD_RATE_BYTES,
        }
    }
}

impl FileConfig {
    pub fn download_memory_bytes(&self) -> u64 {
        self.download_memory_mb.checked_mul(MIB).unwrap_or(u64::MAX)
    }

    pub fn max_upload_bytes(&self) -> u64 {
        self.max_upload_mb.checked_mul(MIB).unwrap_or(u64::MAX)
    }
}

/// The resolved download destination after room > server > global resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DownloadTarget {
    /// Downloads are disabled.
    #[default]
    Off,
    /// Received files are held in the shared in-memory ring buffer.
    Memory,
    /// Received files are written under this directory.
    Persistent(PathBuf),
}

impl DownloadTarget {
    /// Whether this level accepts downloads at all.
    pub fn is_active(&self) -> bool {
        !matches!(self, DownloadTarget::Off)
    }

    /// The download mode this target corresponds to, for display.
    pub fn mode(&self) -> DownloadMode {
        match self {
            DownloadTarget::Off => DownloadMode::Off,
            DownloadTarget::Memory => DownloadMode::Memory,
            DownloadTarget::Persistent(_) => DownloadMode::Persistent,
        }
    }
}

/// Download settings after room > server > global resolution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveFiles {
    pub target: DownloadTarget,
    pub max_download_bytes: u64,
}

/// Persistence settings after room > server > global resolution.
/// `location: None` means the platform data dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveHistory {
    pub enabled: bool,
    pub location: Option<PathBuf>,
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
    pub enabled: bool,
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
            enabled: false,
            candidate_privacy: CandidatePrivacy::default(),
            prefer_ipv6: true,
        }
    }
}

/// The optional browser chat-log view served over `darkhttp`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebAutoplay {
    #[default]
    Disabled,
    Muted,
    WithAudio,
}

impl WebAutoplay {
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Muted => "muted",
            Self::WithAudio => "with-audio",
        }
    }
}

impl<'de> FromToml<'de> for WebAutoplay {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        match (item.as_bool(), item.as_str()) {
            (Some(false), _) => Ok(Self::Disabled),
            (Some(true), _) => Ok(Self::Muted),
            (_, Some("with-audio")) => Ok(Self::WithAudio),
            _ => Err(ctx.report_custom_error("expected false, true, or \"with-audio\"", item)),
        }
    }
}

impl ToToml for WebAutoplay {
    fn to_toml<'a>(&'a self, _arena: &'a Arena) -> Result<Item<'a>, ToTomlError> {
        Ok(match self {
            Self::Disabled => Item::from(false),
            Self::Muted => Item::from(true),
            Self::WithAudio => Item::string("with-audio"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct WebConfig {
    /// Whether to start the web server when the client runs.
    #[toml(default)]
    pub enabled: bool,
    /// The loopback address the web server binds.
    #[toml(default = "127.0.0.1:8080".to_string())]
    pub bind: String,
    /// Whether the browser view is view-only. When `true` (the default) the page
    /// cannot send chat messages or files. Set `false` to enable the compose box.
    #[toml(default = true)]
    pub readonly: bool,
    /// Automatically play newly received videos. `true` uses muted playback so
    /// browsers can start it without interaction; `"with-audio"` also requests
    /// audio, which browsers may reject under their autoplay policy.
    #[toml(default)]
    pub autoplay: WebAutoplay,
    /// Open each file preview in its own browser tab instead of the side panel.
    /// The misspelling is part of the user-facing configuration key.
    #[toml(default)]
    pub viewer_in_seperate_browser_tab: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:8080".to_string(),
            readonly: true,
            autoplay: WebAutoplay::Disabled,
            viewer_in_seperate_browser_tab: false,
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct HistoryConfig {
    #[toml(default)]
    pub enabled: bool,
    /// Base directory for persisted history. `None` uses the platform data dir.
    pub location: Option<String>,
}

/// Per-server or per-room download overrides. `None` inherits the next level
/// up. The `download` mode carries off/memory/persistent; the in-memory ring
/// size stays a global-only setting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct FileOverrides {
    pub download: Option<DownloadMode>,
    pub download_dir: Option<String>,
    pub max_download_mb: Option<u64>,
}

impl FileOverrides {
    pub fn is_empty(&self) -> bool {
        *self == FileOverrides::default()
    }
}

/// Per-server or per-room persistence overrides. `None` inherits the next
/// level up.
#[derive(Clone, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct HistoryOverrides {
    pub enabled: Option<bool>,
    pub location: Option<String>,
}

impl HistoryOverrides {
    pub fn is_empty(&self) -> bool {
        *self == HistoryOverrides::default()
    }
}

/// Download and persistence overrides for one room of a server, keyed by the
/// server-scoped room id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct RoomOverrides {
    pub room_id: RoomId,
    #[toml(default, style = Header, ToToml skip_if = FileOverrides::is_empty)]
    pub files: FileOverrides,
    #[toml(default, style = Header, ToToml skip_if = HistoryOverrides::is_empty)]
    pub history: HistoryOverrides,
}

impl RoomOverrides {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.history.is_empty()
    }
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
    pub user_id: UserId,
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
    #[toml(default = "congested_wifi".to_string())]
    pub loss: String,
    #[toml(default = 0x746f_6d63_6861_7405)]
    pub seed: u64,
    #[toml(default, style = Header)]
    pub clips: Vec<SoundboardClip>,
}

impl Default for SoundboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            loss: "congested_wifi".to_string(),
            seed: 0x746f_6d63_6861_7405,
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

/// Selects which builtin color theme the UI renders with. Custom user themes
/// live in the sibling `[ui.themes.<name>]` registry ([`ThemesConfig`]) and are
/// referenced by name through [`ThemeSelection`].
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

    /// The kebab-case config name for this builtin, matching the `rename_all`
    /// mapping used when serializing `ui.theme` as a plain string.
    pub fn kebab(self) -> &'static str {
        match self {
            ThemeChoice::TomorrowNight => "tomorrow-night",
            ThemeChoice::Base16Dark => "base16-dark",
            ThemeChoice::Base16Light => "base16-light",
        }
    }

    /// Resolves a builtin from its kebab-case config name, or `None` when the
    /// name is not a builtin (so it may reference a custom theme).
    pub fn from_kebab(name: &str) -> Option<Self> {
        ThemeChoice::ALL
            .into_iter()
            .find(|choice| choice.kebab() == name)
    }
}

/// A literal color parsed from a custom theme table: `"#rgb"`/`"#rrggbb"` for
/// true color, or `"ansi:N"` / a bare integer `0..=255` for a 256-color palette
/// index. Wraps an [`extui::Color`]; only parses (the registry re-serializes
/// verbatim, see [`ThemesConfig`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemeColor(pub Color);

impl<'de> FromToml<'de> for ThemeColor {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        if let Some(text) = item.as_str() {
            return parse_theme_color(text)
                .map(ThemeColor)
                .ok_or_else(|| ctx.report_custom_error(THEME_COLOR_EXPECTED, item));
        }
        if let Some(index) = item.as_i64() {
            return u8::try_from(index)
                .map(|index| ThemeColor(Color::Ansi(AnsiColor(index))))
                .map_err(|_| ctx.report_custom_error(THEME_COLOR_EXPECTED, item));
        }
        Err(ctx.report_custom_error(THEME_COLOR_EXPECTED, item))
    }
}

const THEME_COLOR_EXPECTED: &str =
    "expected a color: \"#rrggbb\", \"#rgb\", \"ansi:N\", or an integer 0-255";

/// Parses one color string. Returns `None` on any malformed input so the caller
/// can attach a spanned diagnostic.
fn parse_theme_color(text: &str) -> Option<Color> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(index) = text.strip_prefix("ansi:") {
        return index
            .trim()
            .parse::<u8>()
            .ok()
            .map(|n| Color::Ansi(AnsiColor(n)));
    }
    // A bare integer string is also accepted as a palette index.
    text.parse::<u8>().ok().map(|n| Color::Ansi(AnsiColor(n)))
}

/// Parses the hex body after `#`: three nibbles (`rgb`) or six (`rrggbb`).
fn parse_hex_color(hex: &str) -> Option<Color> {
    let bytes = hex.as_bytes();
    match bytes.len() {
        3 => {
            let r = hex_nibble(bytes[0])?;
            let g = hex_nibble(bytes[1])?;
            let b = hex_nibble(bytes[2])?;
            Some(Color::Rgb(Rgb(r * 0x11, g * 0x11, b * 0x11)))
        }
        6 => {
            let r = hex_pair(bytes[0], bytes[1])?;
            let g = hex_pair(bytes[2], bytes[3])?;
            let b = hex_pair(bytes[4], bytes[5])?;
            Some(Color::Rgb(Rgb(r, g, b)))
        }
        _ => None,
    }
}

fn hex_nibble(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|value| value as u8)
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    Some(hex_nibble(hi)? << 4 | hex_nibble(lo)?)
}

/// One full-slot override: an optional foreground and/or background color.
/// Authored as a bare string (foreground shorthand) or an inline `{ fg, bg }`
/// table. Omitted components reset to terminal default when applied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThemeColorPair {
    pub fg: Option<ThemeColor>,
    pub bg: Option<ThemeColor>,
}

impl ThemeColorPair {
    fn from_toml_with_palette<'de>(
        ctx: &mut Context<'de>,
        item: &Item<'de>,
        palette: &BTreeMap<String, ThemeColor>,
    ) -> Result<Self, Failed> {
        if item.as_str().is_some() || item.as_i64().is_some() {
            return Ok(ThemeColorPair {
                fg: Some(theme_color_from_toml(ctx, item, palette)?),
                bg: None,
            });
        }
        let table = item.require_table(ctx)?;
        let mut pair = ThemeColorPair::default();
        for (key, value) in table {
            match key.name {
                "fg" => pair.fg = Some(theme_color_from_toml(ctx, value, palette)?),
                "bg" => pair.bg = Some(theme_color_from_toml(ctx, value, palette)?),
                _ => {
                    ctx.report_unexpected_key(0, value, key.span);
                }
            }
        }
        Ok(pair)
    }
}

fn theme_color_from_toml<'de>(
    ctx: &mut Context<'de>,
    item: &Item<'de>,
    palette: &BTreeMap<String, ThemeColor>,
) -> Result<ThemeColor, Failed> {
    let Some(text) = item.as_str() else {
        return ThemeColor::from_toml(ctx, item);
    };
    let name = text.trim();
    if name.is_empty() || is_reserved_theme_palette_name(name) {
        return ThemeColor::from_toml(ctx, item);
    }
    palette
        .get(name)
        .copied()
        .ok_or_else(|| ctx.report_custom_error(format!("unknown palette color {name:?}"), item))
}

/// One overridable slot in [`crate::theme::Theme`], keyed by its kebab-case
/// config name. Resolution match-dispatches each parsed entry onto the base
/// theme in a single pass (see `Theme::resolve`), so lookup stays O(1) per
/// override rather than scanning the full slot set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeSlot {
    Background,
    Panel,
    PanelAlt,
    Text,
    Muted,
    Subtle,
    Accent,
    Good,
    Warn,
    Error,
    LocalLine,
    SelectedLine,
    ChatVisualLine,
    ChatCursorLine,
    RoomSelected,
    StatusFill,
    StatusSection,
    JoinInputActive,
    JoinInputInactive,
    JoinInputBoundaryActive,
    RowFocused,
    SelectedFocused,
    DetailPanel,
    DialogPanel,
    DialogHeader,
    ModeServerSelect,
    ModeServerEdit,
    ModeCompose,
    ModeLog,
    ModeSettings,
    EditorSelectionCharwise,
    EditorSelectionLinewise,
    VuTrack,
    VuIdle,
    /// A VU meter level zone carrying both its fill background and its
    /// glyph/readout foreground; the renderer extracts whichever side it needs.
    VuLow,
    VuGood,
    VuWarn,
    VuPeak,
}

impl ThemeSlot {
    /// Maps a kebab-case override key to its slot, or `None` for keys that are
    /// not style slots (reported as unexpected by the caller).
    pub fn from_key(key: &str) -> Option<Self> {
        let slot = match key {
            "background" => Self::Background,
            "panel" => Self::Panel,
            "panel-alt" => Self::PanelAlt,
            "text" => Self::Text,
            "muted" => Self::Muted,
            "subtle" => Self::Subtle,
            "accent" => Self::Accent,
            "good" => Self::Good,
            "warn" => Self::Warn,
            "error" => Self::Error,
            "local-line" => Self::LocalLine,
            "selected-line" => Self::SelectedLine,
            "chat-visual-line" => Self::ChatVisualLine,
            "chat-cursor-line" => Self::ChatCursorLine,
            "room-selected" => Self::RoomSelected,
            "status-fill" => Self::StatusFill,
            "status-section" => Self::StatusSection,
            "join-input-active" => Self::JoinInputActive,
            "join-input-inactive" => Self::JoinInputInactive,
            "join-input-boundary-active" => Self::JoinInputBoundaryActive,
            "row-focused" => Self::RowFocused,
            "selected-focused" => Self::SelectedFocused,
            "detail-panel" => Self::DetailPanel,
            "dialog-panel" => Self::DialogPanel,
            "dialog-header" => Self::DialogHeader,
            "mode-server-select" => Self::ModeServerSelect,
            "mode-server-edit" => Self::ModeServerEdit,
            "mode-compose" => Self::ModeCompose,
            "mode-log" => Self::ModeLog,
            "mode-settings" => Self::ModeSettings,
            "editor-selection-charwise" => Self::EditorSelectionCharwise,
            "editor-selection-linewise" => Self::EditorSelectionLinewise,
            "vu-track" => Self::VuTrack,
            "vu-idle" => Self::VuIdle,
            "vu-low" => Self::VuLow,
            "vu-good" => Self::VuGood,
            "vu-warn" => Self::VuWarn,
            "vu-peak" => Self::VuPeak,
            _ => return None,
        };
        Some(slot)
    }
}

/// One overridable slot in [`crate::theme::SyntaxTheme`]. Syntax slots are
/// foreground-only, so each carries a single [`ThemeColor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyntaxSlot {
    Fg,
    Type,
    Function,
    Binding,
    Namespace,
    Keyword,
    String,
    Number,
    Comment,
}

impl SyntaxSlot {
    pub fn from_key(key: &str) -> Option<Self> {
        let slot = match key {
            "fg" => Self::Fg,
            "type" => Self::Type,
            "function" => Self::Function,
            "binding" => Self::Binding,
            "namespace" => Self::Namespace,
            "keyword" => Self::Keyword,
            "string" => Self::String,
            "number" => Self::Number,
            "comment" => Self::Comment,
            _ => return None,
        };
        Some(slot)
    }
}

/// A user-authored theme: a builtin `base` plus a list of per-slot color
/// overrides. Parsed into entry lists (not a wide struct or string-keyed map)
/// so resolution is a single match-dispatch pass, linear in the number of
/// authored overrides. Parses only; the registry re-serializes verbatim.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CustomTheme {
    pub(crate) base: ThemeChoice,
    pub(crate) palette: BTreeMap<String, ThemeColor>,
    pub(crate) overrides: Vec<(ThemeSlot, ThemeColorPair)>,
    pub(crate) syntax: Vec<(SyntaxSlot, ThemeColor)>,
}

impl<'de> FromToml<'de> for CustomTheme {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let table = item.require_table(ctx)?;
        let mut theme = CustomTheme::default();
        if let Some(palette) = table.get("palette") {
            theme.parse_palette(ctx, palette);
        }
        for (key, value) in table {
            match key.name {
                "base" => {
                    if let Ok(base) = ThemeChoice::from_toml(ctx, value) {
                        theme.base = base;
                    }
                }
                "palette" => {}
                "syntax" => theme.parse_syntax(ctx, value),
                other => {
                    if let Some(slot) = ThemeSlot::from_key(other) {
                        if let Ok(pair) =
                            ThemeColorPair::from_toml_with_palette(ctx, value, &theme.palette)
                        {
                            theme.overrides.push((slot, pair));
                        }
                    } else {
                        ctx.report_unexpected_key(0, value, key.span);
                    }
                }
            }
        }
        Ok(theme)
    }
}

impl CustomTheme {
    /// Parses the nested `[ui.themes.<name>.palette]` table into direct colors.
    fn parse_palette<'de>(&mut self, ctx: &mut Context<'de>, item: &Item<'de>) {
        let Ok(table) = item.require_table(ctx) else {
            return;
        };
        for (key, value) in table {
            if let Ok(color) = ThemeColor::from_toml(ctx, value) {
                self.palette.insert(key.name.to_string(), color);
            }
        }
    }

    /// Parses the nested `[ui.themes.<name>.syntax]` table into syntax slots.
    fn parse_syntax<'de>(&mut self, ctx: &mut Context<'de>, item: &Item<'de>) {
        let Ok(table) = item.require_table(ctx) else {
            return;
        };
        for (key, value) in table {
            if let Some(slot) = SyntaxSlot::from_key(key.name) {
                if let Ok(color) = theme_color_from_toml(ctx, value, &self.palette) {
                    self.syntax.push((slot, color));
                }
            } else {
                ctx.report_unexpected_key(0, value, key.span);
            }
        }
    }
}

/// The `[ui.themes.<name>]` registry of user-authored themes.
///
/// The typed `resolved` map drives theme resolution; the verbatim `raw` table
/// is what serializes back out. Storing the raw parse and re-emitting it keeps
/// a config re-save byte-faithful — the user's exact color spellings, comments,
/// and key order survive — because `toml-spanner`'s format preservation would
/// otherwise canonicalize values through the typed round-trip. This mirrors how
/// [`BindingRuntime`] preserves the `[bindings]` table.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThemesConfig {
    pub resolved: BTreeMap<String, CustomTheme>,
    raw: Option<OwnedTable>,
}

impl ThemesConfig {
    pub fn is_empty(&self) -> bool {
        self.resolved.is_empty()
    }
}

impl<'de> FromToml<'de> for ThemesConfig {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let table = item.require_table(ctx)?;
        let mut resolved = BTreeMap::new();
        for (key, value) in table {
            // Errors are recorded in `ctx`; skip the failing theme and continue
            // so every malformed theme reports at once.
            if let Ok(theme) = CustomTheme::from_toml(ctx, value) {
                resolved.insert(key.name.to_string(), theme);
            }
        }
        Ok(ThemesConfig {
            resolved,
            raw: Some(OwnedTable::from(table)),
        })
    }
}

impl ToToml for ThemesConfig {
    fn to_toml<'a>(&'a self, arena: &'a Arena) -> Result<Item<'a>, ToTomlError> {
        match &self.raw {
            Some(raw) => raw.to_toml(arena),
            // Unreachable in practice: an empty registry is skipped on output
            // via `skip_if = ThemesConfig::is_empty`.
            None => Ok(Table::new().into_item()),
        }
    }
}

/// Which theme `ui.theme` selects: a builtin, or a custom theme by name.
///
/// Serializes as a plain string (the builtin's kebab name or the custom name),
/// so it stays a simple scalar that format preservation restyles cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeSelection {
    Builtin(ThemeChoice),
    Custom(String),
}

impl Default for ThemeSelection {
    fn default() -> Self {
        ThemeSelection::Builtin(ThemeChoice::default())
    }
}

impl ThemeSelection {
    /// The display label shown in the settings/welcome Theme row.
    pub fn label(&self) -> String {
        match self {
            ThemeSelection::Builtin(choice) => choice.label().to_string(),
            ThemeSelection::Custom(name) => name.clone(),
        }
    }

    /// The full cycle list: the three builtins followed by `custom_names` in
    /// order (the registry is a `BTreeMap`, so names arrive sorted).
    pub fn cycle_list(custom_names: &[String]) -> Vec<ThemeSelection> {
        ThemeChoice::ALL
            .into_iter()
            .map(ThemeSelection::Builtin)
            .chain(custom_names.iter().cloned().map(ThemeSelection::Custom))
            .collect()
    }
}

impl<'de> FromToml<'de> for ThemeSelection {
    fn from_toml(ctx: &mut Context<'de>, item: &Item<'de>) -> Result<Self, Failed> {
        let name = item.require_string(ctx)?;
        Ok(match ThemeChoice::from_kebab(name) {
            Some(choice) => ThemeSelection::Builtin(choice),
            None => ThemeSelection::Custom(name.to_string()),
        })
    }
}

impl ToToml for ThemeSelection {
    fn to_toml<'a>(&'a self, arena: &'a Arena) -> Result<Item<'a>, ToTomlError> {
        Ok(match self {
            ThemeSelection::Builtin(choice) => Item::string(choice.kebab()),
            ThemeSelection::Custom(name) => Item::string(arena.alloc_str(name)),
        })
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
    /// Program and fixed arguments that open a clicked chat URL, with the URL
    /// appended as the final argument. Defaults to the platform launcher.
    #[toml(default = default_url_open(), ToToml skip_if = is_default_url_open)]
    pub url_open: Vec<String>,
    #[toml(default, style = Header, ToToml skip_if = Vec::is_empty)]
    pub servers: Vec<ServerEntry>,
    #[toml(default, style = Header)]
    pub audio: AudioConfig,
    #[toml(default, style = Header)]
    pub ui: UiConfig,
    #[toml(default, style = Header)]
    pub files: FileConfig,
    #[toml(default, style = Header)]
    pub p2p: P2pConfig,
    #[toml(default, style = Header)]
    pub history: HistoryConfig,
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

/// The result of [`Config::collect`]: the parsed config (when it could be
/// built) plus every diagnostic gathered while parsing and validating.
struct LoadOutcome {
    source: String,
    content: String,
    config: Option<Config>,
    diagnostics: Vec<Diag>,
}

pub(crate) enum AppConfigLoad {
    Existing(Config),
    Missing(Config),
}

impl Config {
    /// Loads and validates the config file, rendering diagnostics to stderr.
    ///
    /// Unknown and deprecated keys render as warnings and startup continues.
    /// Parse, type, and semantic-validation failures render as errors and this
    /// returns `Err`. Diagnostics are written before the TUI enters the
    /// alternate screen so they stay visible.
    ///
    /// # Errors
    /// Returns a terse summary string when the file cannot be read or when any
    /// error-level diagnostic was emitted. The detailed diagnostics have
    /// already been printed.
    pub fn load(path: Option<&str>) -> Result<Self, String> {
        let outcome = Self::collect(path)?;
        config_diagnostics::render(&outcome.source, &outcome.content, &outcome.diagnostics);
        let errors = outcome.diagnostics.iter().filter(|diag| diag.error).count();
        match outcome.config {
            Some(config) if errors == 0 => Ok(config),
            _ => Err(format!(
                "invalid configuration: {errors} error(s) in {}",
                outcome.source
            )),
        }
    }

    /// Loads config for interactive app startup. A missing file is not an
    /// error here: it means the first-run welcome flow must collect the
    /// user's privacy and storage choices before the app continues.
    pub fn load_for_app(path: Option<&str>) -> Result<AppConfigLoad, String> {
        let config_path = resolve_config_path(path)
            .ok_or_else(|| "HOME is not set; cannot determine config path".to_string())?;
        match fs::read_to_string(&config_path) {
            Ok(content) => {
                let outcome = Self::collect_content(
                    content,
                    config_path.display().to_string(),
                    Some(config_path),
                )?;
                config_diagnostics::render(&outcome.source, &outcome.content, &outcome.diagnostics);
                let errors = outcome.diagnostics.iter().filter(|diag| diag.error).count();
                match outcome.config {
                    Some(config) if errors == 0 => Ok(AppConfigLoad::Existing(config)),
                    _ => Err(format!(
                        "invalid configuration: {errors} error(s) in {}",
                        outcome.source
                    )),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut config = Config::default();
                config.config_path = Some(config_path);
                Ok(AppConfigLoad::Missing(config))
            }
            Err(error) => Err(format!("failed to read {}: {error}", config_path.display())),
        }
    }

    /// Reloads and validates the config for a running client, returning any
    /// diagnostics as the error string instead of rendering them to stderr.
    ///
    /// Startup loaders ([`Config::load`], [`Config::load_for_app`]) render
    /// diagnostics to stderr before the TUI takes the screen; that is wrong for a
    /// mid-session reload, so this renders them into the returned `Err` where the
    /// caller can forward them (e.g. over the control socket).
    ///
    /// # Errors
    /// Returns the rendered diagnostics when the file cannot be read or when any
    /// error-level diagnostic was emitted.
    pub(crate) fn reload(path: Option<&str>, styled_diagnostics: bool) -> Result<Config, String> {
        let outcome = Self::collect(path)?;
        let errors = outcome.diagnostics.iter().filter(|diag| diag.error).count();
        match outcome.config {
            Some(config) if errors == 0 => Ok(config),
            _ => Err(config_diagnostics::render_to_string(
                &outcome.source,
                &outcome.content,
                &outcome.diagnostics,
                styled_diagnostics,
            )),
        }
    }

    /// Reads, parses, and validates the config, accumulating diagnostics.
    ///
    /// This is the pure half of [`Config::load`]: it performs no rendering so
    /// tests can inspect the diagnostics directly. `Err` is reserved for a file
    /// that cannot be read, where there is no source to annotate.
    fn collect(path: Option<&str>) -> Result<LoadOutcome, String> {
        let config_path = resolve_config_path(path);

        let (content, source) = if let Some(path) = &config_path {
            let content = fs::read_to_string(path)
                .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
            (content, path.display().to_string())
        } else {
            (DEFAULT_CONFIG.to_string(), "<embedded default>".to_string())
        };

        Self::collect_content(content, source, config_path)
    }

    fn collect_content(
        content: String,
        source: String,
        config_path: Option<PathBuf>,
    ) -> Result<LoadOutcome, String> {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse_recoverable(&content, &arena);
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let (config, from_toml) = match doc.to_allowing_errors::<Config>() {
            Ok((config, errors)) => (Some(config), errors),
            Err(errors) => (None, errors),
        };

        let mut diagnostics: Vec<Diag> = from_toml
            .errors
            .iter()
            .map(|err| config_diagnostics::from_toml_error(err, &content))
            .collect();

        let config = config.map(|mut config| {
            config.config_path = config_path;
            config.apply_inferred_addresses_from_doc(&udp_addr_configured);
            config.normalize();
            config.validate(&mut diagnostics);
            config
        });

        Ok(LoadOutcome {
            source,
            content,
            config,
            diagnostics,
        })
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
            server.label = server.label.trim().to_string();
            server.username = server.effective_display_name();
            normalize_file_overrides(&mut server.files);
            normalize_history_overrides(&mut server.history);
            for room in &mut server.rooms {
                normalize_file_overrides(&mut room.files);
                normalize_history_overrides(&mut room.history);
            }
            server.rooms.retain(|room| !room.is_empty());
        }
        self.history.location = self
            .history
            .location
            .take()
            .and_then(|location| non_empty_string(&location));
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
        self.audio.output_volume = snap_output_volume_percent(self.audio.output_volume);
        self.soundboard.loss = self.soundboard.loss.trim().to_string();
        for clip in &mut self.soundboard.clips {
            clip.name = clip.name.trim().to_string();
            clip.path = clip.path.trim().to_string();
        }
    }

    /// Checks semantic constraints on the deserialized config, pushing one
    /// error [`Diag`] per violation.
    ///
    /// These checks run on the normalized struct rather than the source
    /// document, so the diagnostics are title-only (no span). Every check runs
    /// so all violations report at once.
    fn validate(&self, out: &mut Vec<Diag>) {
        if let Err(error) = self.audio.latency.validate() {
            out.push(Diag::error(format!("audio latency: {error}")));
        }
        if !(8_000..=96_000).contains(&self.audio.bitrate_bps) {
            out.push(Diag::error(
                "audio bitrate-bps must be between 8000 and 96000",
            ));
        }
        if self.files.download_memory_mb == 0 {
            // Memory mode advertises that downloads are active but would reject
            // every non-empty file with a zero-sized ring, so reject it at load
            // the same way the settings UI does.
            out.push(Diag::error(
                "files download-memory-mb must be a positive MiB count",
            ));
        }
        if let Err(error) = self.web.bind.parse::<std::net::SocketAddr>() {
            out.push(Diag::error(format!(
                "web bind must be a socket address: {error}"
            )));
        }
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
                out.push(Diag::error(format!(
                    "notifications {name} must be between {:.1} and {:.1}",
                    MIN_NOTIFICATION_VOLUME_DB, MAX_NOTIFICATION_VOLUME_DB
                )));
            }
        }
        let mut labels = HashSet::new();
        for server in &self.servers {
            let label = &server.label;
            if let Err(error) = validate_server_label(label) {
                out.push(Diag::error(format!("server {label}: {error}")));
            }
            if let Err(error) = validate_non_empty(&server.token, "token") {
                out.push(Diag::error(format!("server {label}: {error}")));
            }
            if let Err(error) = validate_non_empty(&server.username, "username") {
                out.push(Diag::error(format!("server {label}: {error}")));
            }
            if let Err(error) = validate_endpoint(&server.tcp_addr, "tcp-addr") {
                out.push(Diag::error(format!("server {label}: {error}")));
            }
            if let Err(error) = validate_endpoint(&server.effective_udp_addr(), "udp-addr") {
                out.push(Diag::error(format!("server {label}: {error}")));
            }
            if let Some(addr) = &server.udp_probe_addr {
                if let Err(error) = validate_endpoint(addr, "udp-probe-addr") {
                    out.push(Diag::error(format!("server {label}: {error}")));
                }
            }
            if !labels.insert(label.as_str()) {
                out.push(Diag::error(format!("duplicate server label {label}")));
            }
            let mut room_ids = HashSet::new();
            for room in &server.rooms {
                if !room_ids.insert(room.room_id) {
                    out.push(Diag::error(format!(
                        "server {label}: duplicate room override for room-id {}",
                        room.room_id.0
                    )));
                }
            }
        }
        let mut user_audio_keys = HashSet::new();
        for preference in &self.user_audio {
            let label = &preference.server_alias;
            let user_id = preference.user_id;
            if let Err(error) = validate_server_label(label) {
                out.push(Diag::error(format!(
                    "user-audio {label}:{user_id}: {error}"
                )));
            }
            if user_id.0 == 0 {
                out.push(Diag::error(format!(
                    "user-audio {label}: user-id must be non-zero"
                )));
            }
            if !(MIN_USER_VOLUME_DB..=MAX_USER_VOLUME_DB).contains(&preference.volume_db) {
                out.push(Diag::error(format!(
                    "user-audio {label}:{user_id} volume-db must be between {:.1} and {:.1}",
                    MIN_USER_VOLUME_DB, MAX_USER_VOLUME_DB
                )));
            }
            if !user_audio_keys.insert((label.as_str(), user_id)) {
                out.push(Diag::error(format!(
                    "duplicate user-audio entry for {label}:{user_id}"
                )));
            }
        }
        if self.soundboard.enabled {
            if self.soundboard.packet_loss().is_none() {
                out.push(Diag::error(format!(
                    "soundboard loss must be one of: {}",
                    LiveAudioPacketLossProfile::NAMES.join(", ")
                )));
            }
            if self.soundboard.clips.is_empty() {
                out.push(Diag::error(
                    "soundboard enabled but no [[soundboard.clips]] entries are configured",
                ));
            }
        }
        for (index, clip) in self.soundboard.clips.iter().enumerate() {
            if clip.name.is_empty() {
                out.push(Diag::error(format!(
                    "soundboard clip {}: name must not be empty",
                    index + 1
                )));
            }
            if clip.path.is_empty() {
                out.push(Diag::error(format!(
                    "soundboard clip {}: path must not be empty",
                    index + 1
                )));
            }
        }
        if let Some(program) = self.url_open.first()
            && program.is_empty()
        {
            out.push(Diag::error("url-open program must not be empty"));
        }
        self.validate_themes(out);
    }

    /// Checks the custom theme registry: names are well-formed and distinct from
    /// builtins, and palette keys are usable.
    fn validate_themes(&self, out: &mut Vec<Diag>) {
        for (name, theme) in &self.ui.themes.resolved {
            if let Err(error) = validate_server_label(name) {
                out.push(Diag::error(format!("theme {name}: {error}")));
            }
            if ThemeChoice::from_kebab(name).is_some() {
                out.push(Diag::error(format!(
                    "theme {name}: name collides with a builtin theme"
                )));
            }
            for palette_name in theme.palette.keys() {
                if let Err(error) = validate_theme_palette_name(palette_name) {
                    out.push(Diag::error(format!(
                        "theme {name} palette {palette_name}: {error}"
                    )));
                }
            }
        }
        if let ThemeSelection::Custom(name) = &self.ui.theme
            && !self.ui.themes.resolved.contains_key(name)
        {
            out.push(Diag::error(format!(
                "ui theme {name:?} is not a builtin or a configured [ui.themes.{name}]"
            )));
        }
    }

    /// Resolves download settings room > server > global, per field. A `None`
    /// `room_id` (or a room without overrides) yields the server-level value.
    pub fn effective_files(&self, server: &ServerEntry, room_id: Option<RoomId>) -> EffectiveFiles {
        let room = room_overrides(server, room_id).map(|room| &room.files);
        let mode = room
            .and_then(|files| files.download)
            .or(server.files.download)
            .unwrap_or(self.files.download);
        let download_dir = room
            .and_then(|files| files.download_dir.clone())
            .or_else(|| server.files.download_dir.clone())
            .unwrap_or_else(|| self.files.download_dir.clone());
        let download_dir = download_dir.trim();
        let target = match mode {
            DownloadMode::Off => DownloadTarget::Off,
            DownloadMode::Memory => DownloadTarget::Memory,
            DownloadMode::Persistent => {
                let dir = if download_dir.is_empty() {
                    paths::default_download_dir().unwrap_or_else(|| PathBuf::from("files"))
                } else {
                    PathBuf::from(download_dir)
                };
                DownloadTarget::Persistent(dir)
            }
        };
        let max_download_mb = room
            .and_then(|files| files.max_download_mb)
            .or(server.files.max_download_mb)
            .unwrap_or(self.files.max_download_mb);
        EffectiveFiles {
            target,
            max_download_bytes: max_download_mb.checked_mul(MIB).unwrap_or(u64::MAX),
        }
    }

    /// Every distinct directory a persistent download could be saved to: the
    /// global target plus each server- and room-level override. The web view
    /// registers the files already in these directories at startup so downloads
    /// from a previous session remain servable regardless of which directory
    /// they live in.
    pub fn persistent_download_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        let mut push = |target: DownloadTarget| {
            if let DownloadTarget::Persistent(dir) = target
                && !dirs.contains(&dir)
            {
                dirs.push(dir);
            }
        };
        push(self.effective_files(&ServerEntry::default(), None).target);
        for server in &self.servers {
            push(self.effective_files(server, None).target);
            for room in &server.rooms {
                push(self.effective_files(server, Some(room.room_id)).target);
            }
        }
        dirs
    }

    /// Resolves persistence settings room > server > global, per field.
    pub fn effective_history(
        &self,
        server: &ServerEntry,
        room_id: Option<RoomId>,
    ) -> EffectiveHistory {
        let room = room_overrides(server, room_id).map(|room| &room.history);
        let enabled = room
            .and_then(|history| history.enabled)
            .or(server.history.enabled)
            .unwrap_or(self.history.enabled);
        let location = room
            .and_then(|history| history.location.clone())
            .or_else(|| server.history.location.clone())
            .or_else(|| self.history.location.clone())
            .map(PathBuf::from);
        EffectiveHistory { enabled, location }
    }

    /// The fully-resolved per-room download policy handed to the network
    /// worker for one server connection.
    pub fn file_policy(&self, server: &ServerEntry) -> FilePolicy {
        let mut rooms = Vec::new();
        for room in &server.rooms {
            if room.files.is_empty() {
                continue;
            }
            rooms.push((
                room.room_id,
                self.effective_files(server, Some(room.room_id)),
            ));
        }
        FilePolicy {
            default: self.effective_files(server, None),
            rooms,
        }
    }

    pub fn server(&self, alias: &str) -> Result<&ServerEntry, String> {
        self.servers
            .iter()
            .find(|server| server.label == alias)
            .ok_or_else(|| format!("server {alias} is not configured"))
    }

    pub fn upsert_server(&mut self, server: ServerEntry) {
        if let Some(existing) = self
            .servers
            .iter_mut()
            .find(|existing| existing.label == server.label)
        {
            *existing = server;
        } else {
            self.servers.push(server);
        }
    }

    pub fn user_volume_db(&self, server_alias: &str, user_id: UserId) -> f32 {
        self.user_audio
            .iter()
            .find(|preference| {
                preference.server_alias == server_alias && preference.user_id == user_id
            })
            .map_or(0.0, |preference| preference.volume_db)
    }

    pub fn set_user_volume_db(&mut self, server_alias: &str, user_id: UserId, volume_db: f32) {
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
            .or_else(paths::client_config_path)
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

pub fn snap_output_volume_percent(volume_percent: f32) -> f32 {
    let normalized = crate::audio::normalize_output_volume_percent(volume_percent);
    if normalized.abs() < f32::EPSILON {
        0.0
    } else {
        normalized
    }
}

pub fn parse_output_volume_percent_number(text: &str) -> Result<f32, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("output volume cannot be empty".to_string());
    }
    let number = text.strip_suffix('%').unwrap_or(text).trim();
    if number.is_empty() {
        return Err("output volume cannot be empty".to_string());
    }
    let value = number
        .parse::<f32>()
        .map_err(|_| "output volume must be a number, optionally followed by %".to_string())?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err("output volume must be finite".to_string())
    }
}

pub fn parse_output_volume_percent(text: &str) -> Result<f32, String> {
    let value = parse_output_volume_percent_number(text)?;
    if (0.0..=MAX_OUTPUT_VOLUME_PERCENT).contains(&value) {
        Ok(snap_output_volume_percent(value))
    } else {
        Err(format!(
            "output volume must be between {} and {}",
            output_volume_percent_label(0.0),
            output_volume_percent_label(MAX_OUTPUT_VOLUME_PERCENT)
        ))
    }
}

pub fn output_volume_percent_label(value: f32) -> String {
    let value = snap_output_volume_percent(value);
    let mut label = format!("{value:.1}");
    if label.ends_with(".0") {
        label.truncate(label.len() - 2);
    }
    label.push('%');
    label
}

fn is_default_output_volume_percent(value: &f32) -> bool {
    (snap_output_volume_percent(*value) - DEFAULT_OUTPUT_VOLUME_PERCENT).abs() < f32::EPSILON
}

pub fn value_arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find_map(|window| (window[0] == key).then(|| window[1].clone()))
}

pub(crate) fn resolve_config_path(path: Option<&str>) -> Option<PathBuf> {
    path.map(PathBuf::from).or_else(paths::client_config_path)
}

fn default_server_alias() -> String {
    "local".to_string()
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

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn room_overrides(server: &ServerEntry, room_id: Option<RoomId>) -> Option<&RoomOverrides> {
    let room_id = room_id?;
    server.rooms.iter().find(|room| room.room_id == room_id)
}

fn normalize_file_overrides(files: &mut FileOverrides) {
    // An empty path override is meaningless now that `download = "off"`
    // expresses disabling; collapse it back to inherit.
    files.download_dir = files
        .download_dir
        .take()
        .and_then(|dir| non_empty_string(&dir));
}

fn normalize_history_overrides(history: &mut HistoryOverrides) {
    history.location = history
        .location
        .take()
        .and_then(|location| non_empty_string(&location));
}

pub fn validate_server_label(label: &str) -> Result<(), String> {
    let label = label.trim();
    if label.is_empty() {
        return Err("label is empty".to_string());
    }
    if label.len() > 64 {
        return Err("label exceeds 64 bytes".to_string());
    }
    if !label
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("label must use ASCII letters, numbers, '-' or '_'".to_string());
    }
    Ok(())
}

pub fn validate_username(username: &str) -> Result<(), String> {
    let username = username.trim();
    validate_non_empty(username, "username")?;
    if username.len() > 64 {
        return Err("username exceeds 64 bytes".to_string());
    }
    Ok(())
}

pub fn validate_server_entry(server: &ServerEntry) -> Result<(), String> {
    validate_server_label(&server.label)?;
    validate_non_empty(&server.token, "token")?;
    validate_username(&server.username)?;
    validate_endpoint(&server.tcp_addr, "tcp-addr")?;
    validate_endpoint(&server.effective_udp_addr(), "udp-addr")?;
    if let Some(addr) = &server.udp_probe_addr {
        validate_endpoint(addr, "udp-probe-addr")?;
    }
    Ok(())
}

fn validate_endpoint(value: &str, name: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if let Ok(addr) = value.parse::<std::net::SocketAddr>() {
        if addr.ip().is_unspecified() {
            return Err(format!("{name} must not use an unspecified address"));
        }
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

fn validate_theme_palette_name(name: &str) -> Result<(), String> {
    if is_reserved_theme_palette_name(name) {
        return Err("name conflicts with color literal syntax".to_string());
    }
    validate_server_label(name)
}

fn is_reserved_theme_palette_name(name: &str) -> bool {
    !name.is_empty()
        && (name.starts_with('#')
            || name.starts_with("ansi:")
            || name.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs semantic validation and returns the error messages, joined.
    fn validation_errors(config: &Config) -> String {
        let mut diagnostics = Vec::new();
        config.validate(&mut diagnostics);
        diagnostics
            .into_iter()
            .filter(|diag| diag.error)
            .map(|diag| diag.message)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn server_udp_addr_inherits_tcp_addr_when_omitted() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
label = "lab"
username = "Alice"
token = "alice-dev-token"
tcp-addr = "127.0.0.1:42000"
server-public-key = ""
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
label = "lab"
username = "Alice"
token = "alice-dev-token"
tcp-addr = "127.0.0.1:42000"
udp-addr = "127.0.0.1:42001"
udp-probe-addr = "127.0.0.1:42002"
server-public-key = ""
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
label = "prod"
username = "Alice"
token = "client-generated-token-with-at-least-32-bytes"
tcp-addr = "chat.example.com:443"
udp-addr = "media.example.com:54100"
udp-probe-addr = "probe.example.com:54101"
server-public-key = ""
"#,
            &arena,
        )
        .unwrap();
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses_from_doc(&udp_addr_configured);
        config.normalize();
        assert!(validation_errors(&config).is_empty());

        let server = config.server("prod").unwrap();
        assert_eq!(server.tcp_addr, "chat.example.com:443");
        assert_eq!(server.udp_addr, "media.example.com:54100");
        assert_eq!(
            server.udp_probe_addr.as_deref(),
            Some("probe.example.com:54101")
        );
    }

    #[test]
    fn server_endpoints_reject_unspecified_addresses() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
label = "lab"
username = "Alice"
token = "alice-dev-token"
tcp-addr = "0.0.0.0:41000"
server-public-key = ""
"#,
            &arena,
        )
        .unwrap();
        let udp_addr_configured = server_udp_addr_configured(&doc);
        let mut config: Config = doc.to().unwrap();
        config.apply_inferred_addresses_from_doc(&udp_addr_configured);
        config.normalize();

        let error = validation_errors(&config);

        assert!(error.contains("tcp-addr must not use an unspecified address"));
        assert!(error.contains("udp-addr must not use an unspecified address"));
    }

    #[test]
    fn soundboard_config_parses_clip_and_loss_profile() {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse(
            r#"
active-server = "lab"

[[servers]]
label = "lab"
username = "Carol"
token = "carol-dev-token"
tcp-addr = "127.0.0.1:42000"
server-public-key = ""

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

        assert!(validation_errors(&config).is_empty());
        assert!(config.soundboard.enabled);
        assert_eq!(
            config.soundboard.packet_loss(),
            Some(LiveAudioPacketLossProfile::Random60)
        );
        assert_eq!(config.soundboard.clips[0].name, "sample");
    }

    /// Parses `content` into a [`LoadOutcome`] via a temp file, exercising the
    /// real `Config::collect` path.
    fn collect_from(label: &str, content: &str) -> LoadOutcome {
        let path =
            std::env::temp_dir().join(format!("chatt-config-{label}-{}.toml", std::process::id()));
        std::fs::write(&path, content).unwrap();
        let outcome = Config::collect(Some(path.to_str().unwrap())).unwrap();
        let _ = std::fs::remove_file(path);
        outcome
    }

    #[test]
    fn load_for_app_missing_returns_first_run_config() {
        let path = std::env::temp_dir().join(format!(
            "chatt-missing-client-config-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let loaded = Config::load_for_app(Some(path.to_str().unwrap())).unwrap();
        let AppConfigLoad::Missing(config) = loaded else {
            panic!("missing config should request first-run setup");
        };

        assert_eq!(config.config_path.as_deref(), Some(path.as_path()));
        assert_eq!(config.ui.default_bindings, DefaultBindings::Standard);
        assert_eq!(
            config.ui.theme,
            ThemeSelection::Builtin(ThemeChoice::TomorrowNight)
        );
        assert!(!config.p2p.enabled);
        assert!(!config.history.enabled);
        assert_eq!(config.files.download, DownloadMode::Memory);
        assert_eq!(config.files.download_memory_mb, 512);
        assert_eq!(config.files.download_memory_bytes(), 512 * MIB);
        assert!(config.files.download_dir.is_empty());
    }

    #[test]
    fn unknown_key_warns_but_loads() {
        let outcome = collect_from("unknown-key", "totally-unknown-key = 7\n");
        assert!(outcome.config.is_some());
        assert!(outcome.diagnostics.iter().all(|diag| !diag.error));
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diag| !diag.error && diag.span.is_some())
        );
    }

    #[test]
    fn invalid_value_is_error() {
        let outcome = collect_from("invalid-bitrate", "[audio]\nbitrate-bps = 1\n");
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diag| diag.error && diag.message.contains("bitrate-bps"))
        );
    }

    #[test]
    fn unspecified_server_endpoint_is_load_error() {
        let outcome = collect_from(
            "unspecified-server-endpoint",
            r#"
active-server = "lab"

[[servers]]
label = "lab"
username = "Alice"
token = "alice-dev-token"
tcp-addr = "0.0.0.0:41000"
server-public-key = ""
"#,
        );

        assert!(outcome.diagnostics.iter().any(|diag| {
            diag.error
                && diag
                    .message
                    .contains("tcp-addr must not use an unspecified address")
        }));
        assert!(outcome.diagnostics.iter().any(|diag| {
            diag.error
                && diag
                    .message
                    .contains("udp-addr must not use an unspecified address")
        }));
    }

    #[test]
    fn unspecified_udp_endpoint_is_load_error() {
        let outcome = collect_from(
            "unspecified-udp-endpoint",
            r#"
active-server = "lab"

[[servers]]
label = "lab"
username = "Alice"
token = "alice-dev-token"
tcp-addr = "104.247.224.7:41000"
udp-addr = "0.0.0.0:41000"
server-public-key = ""
"#,
        );

        assert!(outcome.diagnostics.iter().any(|diag| {
            diag.error
                && diag
                    .message
                    .contains("udp-addr must not use an unspecified address")
        }));
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
    fn runtime_config_writes_first_run_defaults() {
        let content = render_runtime(&Config::default());

        assert!(content.contains("default-bindings = \"standard\""));
        assert!(content.contains("[p2p]\nenabled = false"));
        assert!(content.contains("[history]\nenabled = false"));
        assert!(content.contains("download = \"memory\""));
        assert!(content.contains("download-dir = \"\""));
        assert!(content.contains("download-memory-mb = 512"));
        assert!(content.contains("max-download-mb = 50"));
        assert!(content.contains("max-upload-mb = 4096"));
        assert!(!content.contains("form-bindings"));
    }

    #[test]
    fn server_overrides_round_trip_nested_tables() {
        let mut config = Config::default();
        let mut server = ServerEntry::default();
        server.files = FileOverrides {
            download: Some(DownloadMode::Persistent),
            download_dir: Some("/srv/dl".to_string()),
            max_download_mb: None,
        };
        server.history = HistoryOverrides {
            enabled: Some(true),
            location: Some("/tmp/.chatt-data".to_string()),
        };
        server.rooms = vec![
            RoomOverrides {
                room_id: RoomId(3),
                files: FileOverrides {
                    download: None,
                    download_dir: None,
                    max_download_mb: Some(100),
                },
                history: HistoryOverrides::default(),
            },
            RoomOverrides {
                room_id: RoomId(7),
                files: FileOverrides {
                    download: Some(DownloadMode::Off),
                    download_dir: None,
                    max_download_mb: None,
                },
                history: HistoryOverrides {
                    enabled: Some(false),
                    location: None,
                },
            },
        ];
        config.servers.push(server);
        let content = render_runtime(&config);

        assert!(content.contains("[servers.files]"), "{content}");
        assert!(content.contains("[servers.history]"), "{content}");
        assert!(content.contains("[[servers.rooms]]"), "{content}");
        assert!(content.contains("max-download-mb = 100"), "{content}");

        let arena = Arena::new();
        let parsed: Config = toml_spanner::parse(&content, &arena).unwrap().to().unwrap();
        let original = &config.servers[0];
        let reloaded = &parsed.servers[0];
        assert_eq!(reloaded.files, original.files);
        assert_eq!(reloaded.history, original.history);
        assert_eq!(reloaded.rooms, original.rooms);
    }

    #[test]
    fn unset_override_fields_are_omitted_from_save() {
        let mut config = Config::default();
        config.servers.push(ServerEntry::default());
        let content = render_runtime(&config);

        assert!(!content.contains("[servers.files]"));
        assert!(!content.contains("[servers.history]"));
        assert!(!content.contains("[[servers.rooms]]"));
    }

    fn overridden_server() -> (Config, ServerEntry) {
        let mut config = Config::default();
        config.files.download = DownloadMode::Persistent;
        config.files.download_dir = "/global/dl".to_string();
        config.files.max_download_mb = 100;
        config.history.enabled = false;
        config.history.location = None;
        let mut server = ServerEntry::default();
        server.files = FileOverrides {
            download: Some(DownloadMode::Persistent),
            download_dir: Some("/server/dl".to_string()),
            max_download_mb: None,
        };
        server.history = HistoryOverrides {
            enabled: None,
            location: Some("/server/data".to_string()),
        };
        server.rooms = vec![RoomOverrides {
            room_id: RoomId(3),
            files: FileOverrides {
                download: None,
                download_dir: None,
                max_download_mb: Some(300),
            },
            history: HistoryOverrides {
                enabled: Some(true),
                location: None,
            },
        }];
        (config, server)
    }

    #[test]
    fn zero_download_memory_mb_is_rejected() {
        let mut config = Config::default();
        config.files.download_memory_mb = 0;
        let mut diagnostics = Vec::new();
        config.validate(&mut diagnostics);
        let messages = diagnostics
            .iter()
            .filter(|diag| diag.error)
            .map(|diag| diag.message.as_str())
            .collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("download-memory-mb")),
            "expected a download-memory-mb error, got {messages:?}"
        );
    }

    #[test]
    fn effective_files_prefers_room_then_server_then_global() {
        let (config, server) = overridden_server();

        let global_only = config.effective_files(&ServerEntry::default(), None);
        assert_eq!(
            global_only.target,
            DownloadTarget::Persistent(PathBuf::from("/global/dl"))
        );
        assert_eq!(global_only.max_download_bytes, 100 * MIB);

        let server_level = config.effective_files(&server, None);
        assert_eq!(
            server_level.target,
            DownloadTarget::Persistent(PathBuf::from("/server/dl"))
        );
        assert_eq!(server_level.max_download_bytes, 100 * MIB);

        let room_level = config.effective_files(&server, Some(RoomId(3)));
        assert_eq!(
            room_level.target,
            DownloadTarget::Persistent(PathBuf::from("/server/dl"))
        );
        assert_eq!(room_level.max_download_bytes, 300 * MIB);

        let unknown_room = config.effective_files(&server, Some(RoomId(9)));
        assert_eq!(unknown_room, server_level);
    }

    #[test]
    fn room_download_off_disables_downloads_at_room_level() {
        let (config, mut server) = overridden_server();
        server.rooms[0].files.download = Some(DownloadMode::Off);

        let room_level = config.effective_files(&server, Some(RoomId(3)));
        assert_eq!(room_level.target, DownloadTarget::Off);
        assert!(config.effective_files(&server, None).target.is_active());
    }

    #[test]
    fn room_download_memory_resolves_to_memory_target() {
        let (config, mut server) = overridden_server();
        server.rooms[0].files.download = Some(DownloadMode::Memory);

        let room_level = config.effective_files(&server, Some(RoomId(3)));
        assert_eq!(room_level.target, DownloadTarget::Memory);
    }

    #[test]
    fn persistent_empty_dir_falls_back_to_default_download_dir() {
        let mut config = Config::default();
        config.files.download = DownloadMode::Persistent;
        config.files.download_dir = String::new();

        let expected = paths::default_download_dir().unwrap_or_else(|| PathBuf::from("files"));
        assert_eq!(
            config.effective_files(&ServerEntry::default(), None).target,
            DownloadTarget::Persistent(expected)
        );
    }

    #[test]
    fn download_mode_round_trips_toml() {
        for (value, expected) in [
            ("\"off\"", DownloadMode::Off),
            ("\"memory\"", DownloadMode::Memory),
            ("\"persistent\"", DownloadMode::Persistent),
        ] {
            let source = format!("[files]\ndownload = {value}\n");
            let arena = Arena::new();
            let config: Config = toml_spanner::parse(&source, &arena).unwrap().to().unwrap();
            assert_eq!(config.files.download, expected);
        }
    }

    #[test]
    fn effective_history_location_falls_back_per_field() {
        let (config, server) = overridden_server();

        let server_level = config.effective_history(&server, None);
        assert!(!server_level.enabled);
        assert_eq!(server_level.location, Some(PathBuf::from("/server/data")));

        let room_level = config.effective_history(&server, Some(RoomId(3)));
        assert!(room_level.enabled);
        assert_eq!(room_level.location, Some(PathBuf::from("/server/data")));

        let mut config = config;
        config.history.location = Some("/global/data".to_string());
        let plain = config.effective_history(&ServerEntry::default(), None);
        assert_eq!(plain.location, Some(PathBuf::from("/global/data")));
    }

    #[test]
    fn file_policy_only_lists_rooms_with_file_overrides() {
        let (mut config, mut server) = overridden_server();
        server.rooms.push(RoomOverrides {
            room_id: RoomId(5),
            files: FileOverrides::default(),
            history: HistoryOverrides {
                enabled: Some(true),
                location: None,
            },
        });
        config.servers.push(server.clone());

        let policy = config.file_policy(&server);
        assert_eq!(policy.rooms.len(), 1);
        assert_eq!(policy.rooms[0].0, RoomId(3));
        assert_eq!(policy.default, config.effective_files(&server, None));
    }

    #[test]
    fn duplicate_room_override_ids_are_rejected() {
        let mut config = Config::default();
        let mut server = ServerEntry::default();
        server.rooms = vec![
            RoomOverrides {
                room_id: RoomId(3),
                files: FileOverrides {
                    download: Some(DownloadMode::Persistent),
                    download_dir: Some("/a".to_string()),
                    max_download_mb: None,
                },
                history: HistoryOverrides::default(),
            },
            RoomOverrides {
                room_id: RoomId(3),
                files: FileOverrides {
                    download: Some(DownloadMode::Persistent),
                    download_dir: Some("/b".to_string()),
                    max_download_mb: None,
                },
                history: HistoryOverrides::default(),
            },
        ];
        config.servers.push(server);

        assert!(validation_errors(&config).contains("duplicate room override for room-id 3"));
    }

    #[test]
    fn web_autoplay_parses_supported_values() {
        for (value, expected) in [
            ("false", WebAutoplay::Disabled),
            ("true", WebAutoplay::Muted),
            ("\"with-audio\"", WebAutoplay::WithAudio),
        ] {
            let source = format!("[web]\nautoplay = {value}\n");
            let arena = Arena::new();
            let config: Config = toml_spanner::parse(&source, &arena).unwrap().to().unwrap();
            assert_eq!(config.web.autoplay, expected);
        }
    }

    #[test]
    fn web_autoplay_rejects_other_values() {
        let arena = Arena::new();
        let result = toml_spanner::parse("[web]\nautoplay = \"audio\"\n", &arena)
            .unwrap()
            .to::<Config>();

        assert!(result.is_err());
    }

    #[test]
    fn runtime_config_writes_web_view_settings() {
        let mut config = Config::default();
        config.web.autoplay = WebAutoplay::WithAudio;
        config.web.viewer_in_seperate_browser_tab = true;
        let content = render_runtime(&config);

        assert!(content.contains("autoplay = \"with-audio\""));
        assert!(content.contains("viewer-in-seperate-browser-tab = true"));
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
            assert_eq!(config.ui.theme, ThemeSelection::Builtin(expected));
        }
    }

    #[test]
    fn theme_choice_defaults_to_tomorrow_night() {
        let arena = Arena::new();
        let config: Config = toml_spanner::parse("", &arena).unwrap().to().unwrap();
        assert_eq!(
            config.ui.theme,
            ThemeSelection::Builtin(ThemeChoice::TomorrowNight)
        );
    }

    #[test]
    fn runtime_config_round_trips_theme() {
        let mut config = Config::default();
        config.ui.theme = ThemeSelection::Builtin(ThemeChoice::Base16Dark);
        let content = render_runtime(&config);
        assert!(content.contains("theme = \"base16-dark\""));

        let arena = Arena::new();
        let parsed: Config = toml_spanner::parse(&content, &arena).unwrap().to().unwrap();
        assert_eq!(
            parsed.ui.theme,
            ThemeSelection::Builtin(ThemeChoice::Base16Dark)
        );
    }

    #[test]
    fn theme_color_parses_hex_and_ansi_forms() {
        assert_eq!(
            parse_theme_color("#8aa6bd"),
            Some(Color::Rgb(Rgb(0x8a, 0xa6, 0xbd)))
        );
        assert_eq!(
            parse_theme_color("#fff"),
            Some(Color::Rgb(Rgb(0xff, 0xff, 0xff)))
        );
        assert_eq!(
            parse_theme_color("#ABC"),
            Some(Color::Rgb(Rgb(0xaa, 0xbb, 0xcc)))
        );
        assert_eq!(
            parse_theme_color("ansi:236"),
            Some(Color::Ansi(AnsiColor(236)))
        );
        assert_eq!(parse_theme_color("12"), Some(Color::Ansi(AnsiColor(12))));
        assert_eq!(parse_theme_color("#gggggg"), None);
        assert_eq!(parse_theme_color("#12345"), None);
        assert_eq!(parse_theme_color("ansi:300"), None);
        assert_eq!(parse_theme_color("blue"), None);
    }

    fn direct_color(color: Color) -> ThemeColor {
        ThemeColor(color)
    }

    /// Parses a config and returns the resolved [`CustomTheme`] for `name`.
    fn parse_custom_theme(text: &str, name: &str) -> CustomTheme {
        let arena = Arena::new();
        let config: Config = toml_spanner::parse(text, &arena).unwrap().to().unwrap();
        config.ui.themes.resolved.get(name).cloned().unwrap()
    }

    #[test]
    fn custom_theme_parses_base_overrides_and_syntax() {
        let theme = parse_custom_theme(
            concat!(
                "[ui.themes.mine]\n",
                "base = \"base16-dark\"\n",
                "text = \"#ffffff\"\n",
                "status-fill = { fg = \"#cccccc\", bg = \"#202030\" }\n",
                "[ui.themes.mine.syntax]\n",
                "keyword = \"#c792ea\"\n",
            ),
            "mine",
        );
        assert_eq!(theme.base, ThemeChoice::Base16Dark);
        // A bare string is a foreground-only override.
        let (slot, pair) = theme
            .overrides
            .iter()
            .find(|(slot, _)| *slot == ThemeSlot::Text)
            .unwrap();
        assert_eq!(*slot, ThemeSlot::Text);
        assert_eq!(
            pair.fg,
            Some(direct_color(Color::Rgb(Rgb(0xff, 0xff, 0xff))))
        );
        assert_eq!(pair.bg, None);
        // The table form carries both components.
        let (_, status) = theme
            .overrides
            .iter()
            .find(|(slot, _)| *slot == ThemeSlot::StatusFill)
            .unwrap();
        assert!(status.fg.is_some() && status.bg.is_some());
        assert_eq!(
            theme.syntax,
            vec![(
                SyntaxSlot::Keyword,
                direct_color(Color::Rgb(Rgb(0xc7, 0x92, 0xea)))
            )]
        );
    }

    #[test]
    fn custom_theme_resolves_overrides_over_base() {
        let theme = parse_custom_theme(
            concat!(
                "[ui.themes.mine]\n",
                "base = \"tomorrow-night\"\n",
                "accent = \"#010203\"\n",
            ),
            "mine",
        );
        let resolved = theme.apply_to();
        let base = crate::theme::Theme::from_choice(ThemeChoice::TomorrowNight);
        // Overridden slot changed to the requested color.
        assert_eq!(resolved.accent.fg(), Some(Color::Rgb(Rgb(1, 2, 3))));
        // Unset slots still match the base builtin.
        assert_eq!(resolved.text.fg(), base.text.fg());
        assert_eq!(resolved.background, base.background);
    }

    #[test]
    fn custom_theme_slot_override_replaces_entire_style() {
        let theme = parse_custom_theme(
            concat!(
                "[ui.themes.mine]\n",
                "base = \"tomorrow-night\"\n",
                "status-fill.fg = \"#010203\"\n",
            ),
            "mine",
        );
        let resolved = theme.apply_to();
        let base = crate::theme::Theme::from_choice(ThemeChoice::TomorrowNight);

        assert_eq!(
            base.status_fill.bg(),
            Some(Color::Rgb(Rgb(0x30, 0x30, 0x30)))
        );
        assert_eq!(resolved.status_fill.fg(), Some(Color::Rgb(Rgb(1, 2, 3))));
        assert_eq!(resolved.status_fill.bg(), None);
    }

    #[test]
    fn custom_theme_resolves_palette_refs_from_dotted_keys() {
        let theme = parse_custom_theme(
            concat!(
                "[ui.themes.mine]\n",
                "base = \"tomorrow-night\"\n",
                "text.fg = \"red\"\n",
                "status-fill.bg = \"red\"\n",
                "palette.red = \"#ff0000\"\n",
                "[ui.themes.mine.syntax]\n",
                "keyword = \"red\"\n",
            ),
            "mine",
        );

        assert_eq!(
            theme.palette.get("red"),
            Some(&ThemeColor(Color::Rgb(Rgb(0xff, 0, 0))))
        );
        let resolved = theme.apply_to();
        assert_eq!(resolved.text.fg(), Some(Color::Rgb(Rgb(0xff, 0, 0))));
        assert_eq!(resolved.status_fill.bg(), Some(Color::Rgb(Rgb(0xff, 0, 0))));
        assert_eq!(
            resolved.syntax.keyword.fg(),
            Some(Color::Rgb(Rgb(0xff, 0, 0)))
        );
    }

    #[test]
    fn custom_theme_accepts_integer_foreground_shorthand() {
        let theme = parse_custom_theme(concat!("[ui.themes.mine]\n", "accent = 12\n"), "mine");
        let resolved = theme.apply_to();
        assert_eq!(resolved.accent.fg(), Some(Color::Ansi(AnsiColor(12))));
    }

    /// Parses a config allowing errors and returns the accumulated messages.
    fn parse_diagnostics(text: &str) -> Vec<String> {
        let arena = Arena::new();
        let mut doc = toml_spanner::parse_recoverable(text, &arena);
        let errors = match doc.to_allowing_errors::<Config>() {
            Ok((_, errors)) => errors,
            Err(errors) => errors,
        };
        errors.errors.iter().map(|err| err.to_string()).collect()
    }

    #[test]
    fn custom_theme_rejects_malformed_color() {
        for value in ["\"#gggggg\"", "\"ansi:300\"", "\"300\"", "300"] {
            let messages = parse_diagnostics(&format!("[ui.themes.mine]\naccent = {value}\n"));
            assert!(
                messages
                    .iter()
                    .any(|message| message.contains("expected a color")),
                "expected a color diagnostic for {value}, got {messages:?}"
            );
        }
    }

    #[test]
    fn custom_theme_rejects_palette_alias_values() {
        let messages = parse_diagnostics(concat!(
            "[ui.themes.mine]\n",
            "palette.red = \"#ff0000\"\n",
            "palette.alias = \"red\"\n",
        ));
        assert!(
            messages
                .iter()
                .any(|message| message.contains("expected a color")),
            "expected a color diagnostic, got {messages:?}"
        );
    }

    #[test]
    fn custom_theme_rejects_unknown_palette_references() {
        let messages = parse_diagnostics(concat!(
            "[ui.themes.mine]\n",
            "text.fg = \"missing\"\n",
            "[ui.themes.mine.syntax]\n",
            "keyword = \"missing\"\n",
        ));
        assert!(
            messages
                .iter()
                .any(|message| message.contains("unknown palette color \"missing\"")),
            "expected unknown palette diagnostic, got {messages:?}"
        );
    }

    #[test]
    fn theme_validation_rejects_bad_palette_names() {
        let arena = Arena::new();
        let config: Config = toml_spanner::parse(
            concat!(
                "[ui.themes.mine.palette]\n",
                "\"12\" = \"#ffffff\"\n",
                "\"bad name\" = \"#000000\"\n",
            ),
            &arena,
        )
        .unwrap()
        .to()
        .unwrap();
        let errors = validation_errors(&config);
        assert!(errors.contains("palette 12"), "{errors}");
        assert!(errors.contains("color literal syntax"), "{errors}");
        assert!(errors.contains("palette bad name"), "{errors}");
    }

    #[test]
    fn theme_validation_rejects_unknown_reference_and_bad_names() {
        let arena = Arena::new();
        let config: Config = toml_spanner::parse(
            concat!(
                "[ui]\n",
                "theme = \"missing\"\n",
                "[ui.themes.tomorrow-night]\n",
                "base = \"base16-dark\"\n",
                "[ui.themes.\"bad name\"]\n",
                "base = \"base16-dark\"\n",
            ),
            &arena,
        )
        .unwrap()
        .to()
        .unwrap();
        let errors = validation_errors(&config);
        assert!(errors.contains("not a builtin or a configured"), "{errors}");
        assert!(errors.contains("collides with a builtin theme"), "{errors}");
        assert!(errors.contains("bad name"), "{errors}");
    }

    #[test]
    fn runtime_config_preserves_custom_theme_verbatim() {
        // Non-canonical spellings, an inline comment, and a deliberate key order.
        let source = concat!(
            "[ui]\n",
            "theme = \"mine\"\n",
            "\n",
            "[ui.themes.mine]\n",
            "base = \"tomorrow-night\"\n",
            "accent = { fg = \"#FFF\" } # my accent\n",
            "status-fill = { bg = \"#AABBCC\", fg = \"12\" }\n",
        );
        let arena = Arena::new();
        let mut config: Config = toml_spanner::parse(source, &arena).unwrap().to().unwrap();
        // Change an unrelated setting; the themes table must be untouched.
        config.ui.room_height = 7;
        let rendered = config.runtime_toml(source).unwrap();
        assert!(
            rendered.contains("accent = { fg = \"#FFF\" } # my accent"),
            "{rendered}"
        );
        assert!(
            rendered.contains("status-fill = { bg = \"#AABBCC\", fg = \"12\" }"),
            "{rendered}"
        );
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
    fn output_volume_percent_parses_and_formats_at_most_one_decimal() {
        assert_eq!(parse_output_volume_percent("50%").unwrap(), 50.0);
        assert_eq!(parse_output_volume_percent("99.5").unwrap(), 99.5);
        assert_eq!(output_volume_percent_label(50.0), "50%");
        assert_eq!(output_volume_percent_label(99.56), "99.6%");
        assert_eq!(output_volume_percent_label(200.0), "130%");
        assert!(parse_output_volume_percent("130.1%").is_err());
        assert!(parse_output_volume_percent("loud").is_err());
    }

    #[test]
    fn output_volume_clamps_on_load() {
        let outcome = Config::collect_content(
            "[audio]\noutput-volume = 999.0\n".to_string(),
            "test".to_string(),
            None,
        )
        .unwrap();
        let config = outcome.config.unwrap();

        assert_eq!(config.audio.output_volume, MAX_OUTPUT_VOLUME_PERCENT);
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
            "[bindings.insert]\n",
            "\"C-x\" = \"Quit\"\n",
        );
        let arena = Arena::new();
        let config: Config = toml_spanner::parse(existing, &arena).unwrap().to().unwrap();
        let content = config.runtime_toml(existing).unwrap();

        assert!(content.contains("# keep me"));
        assert!(content.contains("[bindings.insert]"));
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
        config.audio.latency.neteq_start_delay_ms = 80;
        config.audio.latency.neteq_min_delay_ms = 25;
        config.audio.latency.neteq_base_minimum_delay_ms = 90;
        config.audio.latency.neteq_max_delay_ms = 1_200;
        let content = render_runtime(&config);

        assert!(content.contains("[audio.latency]"));
        assert!(content.contains("neteq-start-delay-ms = 80"));
        assert!(content.contains("neteq-min-delay-ms = 25"));
        assert!(content.contains("neteq-base-minimum-delay-ms = 90"));
        assert!(content.contains("neteq-max-delay-ms = 1200"));
    }

    #[test]
    fn rejects_invalid_audio_latency_config() {
        let mut config = Config::default();
        config.audio.latency.hard_queue_bound_ms = 40;
        config.audio.latency.neteq_max_delay_ms = 1_000;

        let error = validation_errors(&config);

        assert!(error.contains("audio latency"));
        assert!(error.contains("hard-queue-bound-ms"));
    }

    #[test]
    fn audio_latency_neteq_max_delay_must_cover_start_delay() {
        let mut tuning = LiveAudioTuning::default();
        tuning.neteq_max_delay = tuning.neteq_start_delay;
        assert!(tuning.validate().is_ok());

        tuning.neteq_max_delay = Duration::from_millis(20);
        tuning.neteq_start_delay = Duration::from_millis(60);
        let error = tuning.validate().unwrap_err();
        assert!(error.contains("neteq-max-delay-ms"));
    }

    #[test]
    fn audio_latency_base_minimum_must_fit_constraints() {
        let mut tuning = LiveAudioTuning::default();
        tuning.neteq_base_minimum_delay = Duration::from_millis(80);
        assert!(tuning.validate().is_ok());

        tuning.neteq_base_minimum_delay = Duration::from_millis(1_200);
        let error = tuning.validate().unwrap_err();
        assert!(error.contains("neteq-base-minimum-delay-ms"));
    }

    #[test]
    fn rejects_invalid_audio_bitrate() {
        let mut config = Config::default();
        config.audio.bitrate_bps = 128_000;

        let error = validation_errors(&config);

        assert!(error.contains("bitrate-bps"));
    }

    #[test]
    fn user_volume_preferences_snap_sort_and_remove_zero() {
        let mut config = Config::default();

        config.set_user_volume_db("local", UserId(3), -5.4);
        config.set_user_volume_db("local", UserId(2), 7.6);

        assert_eq!(config.user_volume_db("local", UserId(3)), -5.5);
        assert_eq!(config.user_volume_db("local", UserId(2)), 7.5);
        assert_eq!(config.user_audio[0].user_id, UserId(2));
        assert_eq!(config.user_audio[1].user_id, UserId(3));

        config.set_user_volume_db("local", UserId(2), 0.0);

        assert_eq!(config.user_volume_db("local", UserId(2)), 0.0);
        assert_eq!(config.user_audio.len(), 1);
    }

    #[test]
    fn runtime_config_writes_user_audio_as_array_of_tables() {
        let mut config = Config::default();
        config.set_user_volume_db("local", UserId(2), -5.5);
        let content = render_runtime(&config);

        assert!(content.contains("[[user-audio]]"));
        assert!(content.contains("server-alias = \"local\""));
        assert!(content.contains("user-id = 2"));
        assert!(content.contains("volume-db = -5.5"));
    }

    #[test]
    fn reload_parses_valid_config_and_reports_errors() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-reload-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chatt.toml");

        fs::write(&path, DEFAULT_CONFIG).unwrap();
        let config = Config::reload(Some(path.to_str().unwrap()), false).unwrap();
        assert_eq!(config.config_path.as_deref(), Some(path.as_path()));

        fs::write(&path, "this is not = = valid toml [[[").unwrap();
        let error = match Config::reload(Some(path.to_str().unwrap()), false) {
            Ok(_) => panic!("expected the invalid config to fail"),
            Err(error) => error,
        };
        assert!(!error.is_empty(), "expected rendered diagnostics");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reload_can_render_styled_diagnostics() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-reload-styled-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chatt.toml");
        fs::write(&path, "this is not = = valid toml [[[").unwrap();

        let error = match Config::reload(Some(path.to_str().unwrap()), true) {
            Ok(_) => panic!("expected the invalid config to fail"),
            Err(error) => error,
        };
        assert!(error.contains("\x1b["), "expected ANSI-styled diagnostics");

        let _ = fs::remove_dir_all(dir);
    }
}
