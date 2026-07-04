use hashbrown::HashSet;
use rpc::ids::UserId;
use std::path::PathBuf;
use std::{fs, time::Duration};

use toml_spanner::Toml;
use toml_spanner::{Arena, Context, Failed, FromToml, Item, ToToml, ToTomlError};

use crate::{
    audio::{
        BufferRequest, DenoiseConfig, DenoiseSuppression, DenoiseTypingSuppression, DredConfig,
        LiveAudioPacketLossProfile, LiveAudioTuning, NotificationSound,
    },
    bindings::BindingRuntime,
    client_net::ClientConfig,
    config_diagnostics::{self, Diag},
    paths,
};
use rpc::control::DEFAULT_FILE_SIZE_LIMIT_BYTES;

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
            file_receive_dir: files.receive_dir_path(),
            max_upload_bytes: files.max_upload_bytes,
            max_receive_bytes: files.max_receive_bytes,
            upload_rate_bytes: files.upload_rate_bytes,
            p2p_enabled: p2p.enabled,
            candidate_privacy: p2p.candidate_privacy,
            prefer_ipv6: p2p.prefer_ipv6,
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
    #[toml(default = default_placeholder())]
    pub placeholder: String,
    #[toml(default = 50_000)]
    pub max_messages: u32,
    #[toml(default = 24)]
    pub overscan: u32,
    #[toml(default)]
    pub default_bindings: DefaultBindings,
    #[toml(default)]
    pub theme: ThemeChoice,
}

fn default_placeholder() -> String {
    "Message #lobby".to_string()
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
            placeholder: default_placeholder(),
            max_messages: 50_000,
            overscan: 24,
            default_bindings: DefaultBindings::Standard,
            theme: ThemeChoice::default(),
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

#[derive(Clone, Debug, Toml)]
#[toml(FromToml, ToToml, rename_all = "kebab-case")]
pub struct FileConfig {
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_BYTES)]
    pub max_upload_bytes: u64,
    #[toml(default = DEFAULT_FILE_SIZE_LIMIT_BYTES)]
    pub max_receive_bytes: u64,
    /// Upload pacing ceiling in bytes per second. `0` streams at full socket
    /// speed. Primarily a test lever to stretch a transfer so its progress is
    /// observable, and a mild bandwidth cap.
    #[toml(default = DEFAULT_UPLOAD_RATE_BYTES)]
    pub upload_rate_bytes: u64,
    #[toml(default)]
    pub receive_dir: String,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
            max_receive_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
            upload_rate_bytes: DEFAULT_UPLOAD_RATE_BYTES,
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
    #[toml(default = default_web_bind())]
    pub bind: String,
    /// Whether the browser view is view-only. When `true` (the default) the page
    /// cannot send chat messages or files. Set `false` to enable the compose box.
    #[toml(default = default_true())]
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
            bind: default_web_bind(),
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
    /// Program and fixed arguments that open a clicked chat URL, with the URL
    /// appended as the final argument. Defaults to the platform launcher.
    #[toml(default = default_url_open(), ToToml skip_if = is_default_url_open)]
    pub url_open: Vec<String>,
    #[toml(default = default_servers(), style = Header)]
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

pub fn value_arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find_map(|window| (window[0] == key).then(|| window[1].clone()))
}

fn resolve_config_path(path: Option<&str>) -> Option<PathBuf> {
    path.map(PathBuf::from).or_else(paths::client_config_path)
}

fn default_server_alias() -> String {
    "local".to_string()
}

fn default_web_bind() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_true() -> bool {
    true
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

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
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
        assert_eq!(config.ui.theme, ThemeChoice::TomorrowNight);
        assert!(!config.p2p.enabled);
        assert!(!config.history.enabled);
        assert!(config.files.receive_dir.is_empty());
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
        assert!(content.contains("receive-dir = \"\""));
        assert!(!content.contains("form-bindings"));
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
}
