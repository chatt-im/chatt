use crate::{
    audio::{
        BufferRequest, CAPTURE_LONG_SILENCE_STOP_MS_RANGE, CAPTURE_SILENCE_PREROLL_MS_RANGE,
        CAPTURE_SILENCE_RAMP_MS_RANGE, DEVICE_PERIOD_MARGIN_MS_RANGE, DenoiseConfig, DeviceInfo,
        DredConfig, HARD_QUEUE_BOUND_MS_RANGE, INITIAL_BUFFER_MS_RANGE, MAX_REORDER_DELAY_MS_RANGE,
        NETEQ_BASE_MINIMUM_DELAY_MS_RANGE, NETEQ_MAX_DELAY_MS_RANGE, NETEQ_MIN_DELAY_MS_RANGE,
        NETEQ_START_DELAY_MS_RANGE, StreamPreview,
    },
    config::{
        AudioConfig, AudioLatencyConfig, BufferSize, CandidatePrivacy, DEFAULT_DENOISE_RELEASE,
        DEFAULT_DENOISE_SUPPRESSION, DEFAULT_DENOISE_TYPING_VAD_ENTER,
        DEFAULT_DENOISE_TYPING_VAD_RELEASE, DEFAULT_INPUT_TARGET_LATENCY,
        DEFAULT_MAX_AMPLIFICATION, DEFAULT_OUTPUT_TARGET_LATENCY, DownloadMode, FileConfig,
        FormBindings, HistoryConfig, MAX_NOTIFICATION_VOLUME_DB, MIN_NOTIFICATION_VOLUME_DB,
        NotificationConfig, NotificationSoundMode, P2pConfig, ThemeSelection, UiConfig,
        WebAutoplay, WebConfig, WebViewer, output_volume_percent_label,
        parse_output_volume_percent, valid_web_origin,
    },
    paths,
    ui::select::{FuzzySelect, SelectableItem},
};

pub const BITRATES: [i32; 6] = [16_000, 24_000, 32_000, 48_000, 64_000, 96_000];
/// RNNoise over-suppression exponent options. `1.0` is stock RNNoise, higher
/// values push down residual non-voice noise (keyboard, fans).
pub const DENOISE_SUPPRESSIONS: [f32; 5] = [1.0, 1.5, 2.0, 2.5, 3.0];
pub const DENOISE_SUPPRESSION_LABELS: [&str; 5] = ["off", "1.5x", "2x", "2.5x", "3x"];
/// RNNoise release-smoothing options, expressed as the per-frame gain-rise cap.
/// `1.0` releases instantly (stock), lower values stop noise bursts swelling
/// back up after a silence.
pub const DENOISE_RELEASES: [f32; 5] = [1.0, 0.6, 0.4, 0.3, 0.15];
pub const DENOISE_RELEASE_LABELS: [&str; 5] = ["off", "light", "medium", "strong", "max"];
/// Earshot VAD thresholds for the post-RNNoise typing gate.
pub const DENOISE_TYPING_VAD_THRESHOLDS: [f32; 7] = [0.60, 0.70, 0.75, 0.80, 0.82, 0.85, 0.90];
pub const DENOISE_TYPING_VAD_LABELS: [&str; 7] = ["60%", "70%", "75%", "80%", "82%", "85%", "90%"];
/// Smallest accepted explicit buffer in samples (~0.7 ms at 48 kHz).
pub const MIN_BUFFER_SAMPLES: u32 = 32;
/// Largest accepted explicit buffer in samples (~170 ms at 48 kHz).
pub const MAX_BUFFER_SAMPLES: u32 = 8192;

/// Mutable settings state. Fields are crate-visible so the immediate-mode
/// settings form mutates them in place through `&mut`, replacing the previous
/// focus-enum dispatch. The `*_index` fields select into the option tables
/// ([`BITRATES`] and the denoise tuning tables).
pub struct SettingsDraft {
    pub(crate) input_device_id: Option<String>,
    pub(crate) output_device_id: Option<String>,
    pub(crate) bitrate_index: usize,
    /// Auto-gain ceiling in dB, kept as text so arbitrary valid values can be
    /// entered instead of being rounded to a fixed option table.
    pub(crate) max_amplification: String,
    pub(crate) suppression_index: usize,
    pub(crate) release_index: usize,
    pub(crate) typing_suppression: bool,
    pub(crate) typing_vad_enter_index: usize,
    pub(crate) typing_vad_release_index: usize,
    /// Single-line field values holding a sample count or `"default"` (see
    /// [`parse_buffer_size`]). The shared form editor commits into these.
    pub(crate) input_buffer: String,
    pub(crate) output_buffer: String,
    pub(crate) output_volume: String,
    /// When set, the device row is a free-form text field for a raw ALSA pcm
    /// string instead of the enumerated-device picker.
    pub(crate) input_raw: bool,
    pub(crate) output_raw: bool,
    pub(crate) web_enabled: bool,
    pub(crate) web_bind: String,
    /// WebSocket origin allowlist, edited as a one-origin-per-row list field.
    pub(crate) web_allowed_origins: Vec<String>,
    pub(crate) web_readonly: bool,
    pub(crate) web_autoplay: WebAutoplay,
    pub(crate) web_viewer: WebViewer,
    /// Transient add-row buffer for the allowed-origins list field. Never
    /// persisted; a committed value moves into `web_allowed_origins`.
    pub(crate) web_origins_new: String,
    pub(crate) notification_sounds: NotificationSoundMode,
    pub(crate) message_notification_volume: String,
    pub(crate) peer_join_notification_volume: String,
    pub(crate) peer_leave_notification_volume: String,
    pub(crate) form_bindings: FormBindings,
    pub(crate) theme: ThemeSelection,
    /// Custom theme names from the config registry, in sorted order. Seeded at
    /// session start so the Theme row can cycle builtins plus custom themes.
    pub(crate) theme_names: Vec<String>,
    pub(crate) p2p_enabled: bool,
    pub(crate) p2p_candidate_privacy: CandidatePrivacy,
    pub(crate) p2p_prefer_ipv6: bool,
    pub(crate) download_mode: DownloadMode,
    pub(crate) download_path: String,
    /// The in-memory download ring-buffer size, editable as a MiB count. Shown
    /// only in [`DownloadMode::Memory`].
    pub(crate) download_memory_mb: String,
    pub(crate) files_max_download_mb: String,
    pub(crate) files_max_upload_mb: String,
    /// Upload pacing ceiling as editable text with an optional `K`/`M`/`G`
    /// suffix; `0` streams at full socket speed.
    pub(crate) files_upload_rate: String,
    pub(crate) history_enabled: bool,
    /// Base directory for persisted history; empty means the platform default.
    pub(crate) history_location: String,
    pub(crate) ui_room_height: String,
    pub(crate) ui_max_composer_height: String,
    pub(crate) ui_composer_padding: bool,
    pub(crate) ui_max_messages: String,
    pub(crate) ui_overscan: String,
    /// The URL opener command, one argument per row; the clicked URL is
    /// appended last. Empty falls back to platform behavior (inert opener).
    pub(crate) url_open: Vec<String>,
    /// Transient add-row buffer for the url-open list field.
    pub(crate) url_open_new: String,
    pub(crate) denoise: DenoiseConfig,
    pub(crate) dred: DredConfig,
    pub(crate) echo_cancellation: bool,
    /// Carries the two latency booleans plus the last-valid numeric values the
    /// editable [`LatencyMsDraft`] fields fall back to while mid-edit.
    pub(crate) latency: AudioLatencyConfig,
    /// Editable text for the numeric latency-tuning fields.
    pub(crate) latency_ms: LatencyMsDraft,
    /// Transient settings-only microphone loopback monitor. Never persisted:
    /// [`SettingsDraft::from_audio`] always seeds it off and
    /// [`SettingsDraft::to_audio`] ignores it, matching the auto-disable-on-close
    /// contract enforced in the app.
    pub(crate) loopback: bool,
    /// Transient toggle revealing the advanced rows on tabs that have them.
    /// Session-only, like [`SettingsDraft::loopback`]: seeded off and never
    /// persisted.
    pub(crate) show_advanced: bool,
}

/// Editable text for every numeric [`AudioLatencyConfig`] field. Values are
/// millisecond counts except `silence_vad_max` (a raw `0..=255` VAD level).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LatencyMsDraft {
    pub(crate) neteq_start_delay_ms: String,
    pub(crate) neteq_min_delay_ms: String,
    pub(crate) neteq_base_minimum_delay_ms: String,
    pub(crate) neteq_max_delay_ms: String,
    pub(crate) hard_queue_bound_ms: String,
    pub(crate) initial_buffer_ms: String,
    pub(crate) max_reorder_delay_ms: String,
    pub(crate) device_period_margin_ms: String,
    pub(crate) silence_vad_max: String,
    pub(crate) capture_long_silence_stop_ms: String,
    pub(crate) capture_silence_preroll_ms: String,
    pub(crate) capture_silence_ramp_ms: String,
}

impl LatencyMsDraft {
    pub(crate) fn from_config(config: &AudioLatencyConfig) -> Self {
        Self {
            neteq_start_delay_ms: config.neteq_start_delay_ms.to_string(),
            neteq_min_delay_ms: config.neteq_min_delay_ms.to_string(),
            neteq_base_minimum_delay_ms: config.neteq_base_minimum_delay_ms.to_string(),
            neteq_max_delay_ms: config.neteq_max_delay_ms.to_string(),
            hard_queue_bound_ms: config.hard_queue_bound_ms.to_string(),
            initial_buffer_ms: config.initial_buffer_ms.to_string(),
            max_reorder_delay_ms: config.max_reorder_delay_ms.to_string(),
            device_period_margin_ms: config.device_period_margin_ms.to_string(),
            silence_vad_max: config.silence_vad_max.to_string(),
            capture_long_silence_stop_ms: config.capture_long_silence_stop_ms.to_string(),
            capture_silence_preroll_ms: config.capture_silence_preroll_ms.to_string(),
            capture_silence_ramp_ms: config.capture_silence_ramp_ms.to_string(),
        }
    }

    /// Writes every field that parses into `config`, leaving unparseable ones
    /// at their previous (last-valid) values.
    pub(crate) fn apply_to(&self, config: &mut AudioLatencyConfig) {
        let fields = [
            (&self.neteq_start_delay_ms, &mut config.neteq_start_delay_ms),
            (&self.neteq_min_delay_ms, &mut config.neteq_min_delay_ms),
            (
                &self.neteq_base_minimum_delay_ms,
                &mut config.neteq_base_minimum_delay_ms,
            ),
            (&self.neteq_max_delay_ms, &mut config.neteq_max_delay_ms),
            (&self.hard_queue_bound_ms, &mut config.hard_queue_bound_ms),
            (&self.initial_buffer_ms, &mut config.initial_buffer_ms),
            (&self.max_reorder_delay_ms, &mut config.max_reorder_delay_ms),
            (
                &self.device_period_margin_ms,
                &mut config.device_period_margin_ms,
            ),
            (
                &self.capture_long_silence_stop_ms,
                &mut config.capture_long_silence_stop_ms,
            ),
            (
                &self.capture_silence_preroll_ms,
                &mut config.capture_silence_preroll_ms,
            ),
            (
                &self.capture_silence_ramp_ms,
                &mut config.capture_silence_ramp_ms,
            ),
        ];
        for (text, slot) in fields {
            if let Ok(value) = text.trim().parse::<u64>() {
                *slot = value;
            }
        }
        if let Ok(value) = self.silence_vad_max.trim().parse::<u8>() {
            config.silence_vad_max = value;
        }
    }
}

impl SettingsDraft {
    pub fn from_audio(config: &AudioConfig) -> Self {
        Self {
            input_device_id: config.input_device_id.clone(),
            output_device_id: config.output_device_id.clone(),
            bitrate_index: BITRATES
                .iter()
                .position(|bitrate| *bitrate == config.bitrate_bps)
                .unwrap_or(3),
            max_amplification: db_value_text(config.max_amplification),
            suppression_index: nearest_index(
                &DENOISE_SUPPRESSIONS,
                config.denoise_suppression,
                DEFAULT_DENOISE_SUPPRESSION,
            ),
            release_index: nearest_index(
                &DENOISE_RELEASES,
                config.denoise_release,
                DEFAULT_DENOISE_RELEASE,
            ),
            typing_suppression: config.denoise_typing_suppression,
            typing_vad_enter_index: nearest_index(
                &DENOISE_TYPING_VAD_THRESHOLDS,
                config.denoise_typing_vad_enter,
                DEFAULT_DENOISE_TYPING_VAD_ENTER,
            ),
            typing_vad_release_index: nearest_index(
                &DENOISE_TYPING_VAD_THRESHOLDS,
                config.denoise_typing_vad_release,
                DEFAULT_DENOISE_TYPING_VAD_RELEASE,
            ),
            input_buffer: buffer_size_text(config.input_buffer),
            output_buffer: buffer_size_text(config.output_buffer),
            output_volume: output_volume_percent_label(config.output_volume),
            input_raw: config
                .input_device_id
                .as_deref()
                .is_some_and(is_raw_device_selection),
            output_raw: config
                .output_device_id
                .as_deref()
                .is_some_and(is_raw_device_selection),
            web_enabled: WebConfig::default().enabled,
            web_bind: WebConfig::default().bind,
            web_allowed_origins: WebConfig::default().allowed_origins,
            web_readonly: WebConfig::default().readonly,
            web_autoplay: WebConfig::default().autoplay,
            web_viewer: WebConfig::default().viewer,
            web_origins_new: String::new(),
            notification_sounds: NotificationConfig::default().sounds,
            message_notification_volume: db_value_text(0.0),
            peer_join_notification_volume: db_value_text(0.0),
            peer_leave_notification_volume: db_value_text(0.0),
            form_bindings: FormBindings::Standard,
            theme: ThemeSelection::default(),
            theme_names: Vec::new(),
            p2p_enabled: P2pConfig::default().enabled,
            p2p_candidate_privacy: P2pConfig::default().candidate_privacy,
            p2p_prefer_ipv6: P2pConfig::default().prefer_ipv6,
            download_mode: DownloadMode::default(),
            download_path: default_download_path_text(),
            download_memory_mb: FileConfig::default().download_memory_mb.to_string(),
            files_max_download_mb: FileConfig::default().max_download_mb.to_string(),
            files_max_upload_mb: FileConfig::default().max_upload_mb.to_string(),
            files_upload_rate: FileConfig::default().upload_rate_bytes.to_string(),
            history_enabled: HistoryConfig::default().enabled,
            history_location: String::new(),
            ui_room_height: UiConfig::default().room_height.to_string(),
            ui_max_composer_height: UiConfig::default().max_composer_height.to_string(),
            ui_composer_padding: UiConfig::default().composer_padding,
            ui_max_messages: UiConfig::default().max_messages.to_string(),
            ui_overscan: UiConfig::default().overscan.to_string(),
            url_open: Vec::new(),
            url_open_new: String::new(),
            denoise: config.denoise,
            dred: config.dred,
            echo_cancellation: config.echo_cancellation,
            latency: config.latency.clone(),
            latency_ms: LatencyMsDraft::from_config(&config.latency),
            loopback: false,
            show_advanced: false,
        }
    }

    pub fn set_web_from_config(&mut self, web: &WebConfig) {
        self.web_enabled = web.enabled;
        self.web_bind = web.bind.clone();
        self.web_allowed_origins = web.allowed_origins.clone();
        self.web_readonly = web.readonly;
        self.web_autoplay = web.autoplay;
        self.web_viewer = web.viewer;
    }

    pub fn set_notifications_from_config(&mut self, notifications: &NotificationConfig) {
        self.notification_sounds = notifications.sounds;
        self.message_notification_volume = db_value_text(notifications.message_volume_db);
        self.peer_join_notification_volume = db_value_text(notifications.peer_join_volume_db);
        self.peer_leave_notification_volume = db_value_text(notifications.peer_leave_volume_db);
    }

    pub fn set_form_bindings_from_config(&mut self, form_bindings: FormBindings) {
        self.form_bindings = form_bindings;
    }

    pub fn set_theme_from_config(&mut self, theme: ThemeSelection, custom_names: Vec<String>) {
        self.theme = theme;
        self.theme_names = custom_names;
    }

    pub fn set_files_from_config(&mut self, files: &FileConfig) {
        self.download_mode = files.download;
        self.download_path = if files.download_dir.trim().is_empty() {
            default_download_path_text()
        } else {
            files.download_dir.clone()
        };
        self.download_memory_mb = files.download_memory_mb.to_string();
        self.files_max_download_mb = files.max_download_mb.to_string();
        self.files_max_upload_mb = files.max_upload_mb.to_string();
        self.files_upload_rate = files.upload_rate_bytes.to_string();
    }

    pub fn set_p2p_from_config(&mut self, p2p: &P2pConfig) {
        self.p2p_enabled = p2p.enabled;
        self.p2p_candidate_privacy = p2p.candidate_privacy;
        self.p2p_prefer_ipv6 = p2p.prefer_ipv6;
    }

    pub fn set_history_from_config(&mut self, history: &HistoryConfig) {
        self.history_enabled = history.enabled;
        self.history_location = history.location.clone().unwrap_or_default();
    }

    pub fn set_ui_from_config(&mut self, ui: &UiConfig) {
        self.ui_room_height = ui.room_height.to_string();
        self.ui_max_composer_height = ui.max_composer_height.to_string();
        self.ui_composer_padding = ui.composer_padding;
        self.ui_max_messages = ui.max_messages.to_string();
        self.ui_overscan = ui.overscan.to_string();
    }

    pub fn set_url_open_from_config(&mut self, url_open: &[String]) {
        self.url_open = url_open.to_vec();
        self.url_open_new.clear();
    }

    pub fn theme(&self) -> ThemeSelection {
        self.theme.clone()
    }

    pub fn to_audio(&self) -> AudioConfig {
        AudioConfig {
            input_device_id: self.input_device_id.clone(),
            output_device_id: self.output_device_id.clone(),
            bitrate_bps: BITRATES[self.bitrate_index],
            denoise: self.denoise,
            dred: self.dred,
            echo_cancellation: self.echo_cancellation,
            max_amplification: self.max_amplification(),
            denoise_suppression: self.suppression_strength(),
            denoise_release: self.release_value(),
            denoise_typing_suppression: self.typing_suppression,
            denoise_typing_vad_enter: self.typing_vad_enter(),
            denoise_typing_vad_release: self.typing_vad_release(),
            input_buffer: parse_buffer_size(&self.input_buffer),
            output_buffer: parse_buffer_size(&self.output_buffer),
            output_volume: parse_output_volume_percent(&self.output_volume)
                .unwrap_or(crate::config::DEFAULT_OUTPUT_VOLUME_PERCENT),
            latency: self.latency_config(),
        }
    }

    /// The latency config assembled from the two carried booleans plus every
    /// numeric field that currently parses; mid-edit fields keep their
    /// last-valid values.
    pub(crate) fn latency_config(&self) -> AudioLatencyConfig {
        let mut latency = self.latency.clone();
        self.latency_ms.apply_to(&mut latency);
        latency
    }

    pub fn to_web(&self) -> WebConfig {
        WebConfig {
            enabled: self.web_enabled,
            bind: self.web_bind.trim().to_string(),
            allowed_origins: self.web_allowed_origins.clone(),
            readonly: self.web_readonly,
            autoplay: self.web_autoplay,
            viewer: self.web_viewer,
        }
    }

    pub fn to_notifications(&self) -> NotificationConfig {
        NotificationConfig {
            sounds: self.notification_sounds,
            message_volume_db: self.message_notification_volume_db(),
            peer_join_volume_db: self.peer_join_notification_volume_db(),
            peer_leave_volume_db: self.peer_leave_notification_volume_db(),
        }
    }

    pub fn form_bindings(&self) -> FormBindings {
        self.form_bindings
    }

    pub fn to_files(&self, previous: &FileConfig) -> FileConfig {
        let mut files = previous.clone();
        files.download = self.download_mode;
        if self.download_mode == DownloadMode::Persistent {
            files.download_dir = self.download_path.trim().to_string();
        }
        if let Some(mb) = parse_mib_count(self.download_memory_mb.trim()) {
            files.download_memory_mb = mb;
        }
        if let Some(mb) = parse_mib_count(self.files_max_download_mb.trim()) {
            files.max_download_mb = mb;
        }
        if let Some(mb) = parse_mib_count(self.files_max_upload_mb.trim()) {
            files.max_upload_mb = mb;
        }
        if let Some(rate) = parse_byte_size(&self.files_upload_rate) {
            files.upload_rate_bytes = rate;
        }
        files
    }

    pub fn to_p2p(&self, previous: &P2pConfig) -> P2pConfig {
        let mut p2p = previous.clone();
        p2p.enabled = self.p2p_enabled;
        p2p.candidate_privacy = self.p2p_candidate_privacy;
        p2p.prefer_ipv6 = self.p2p_prefer_ipv6;
        p2p
    }

    /// The UI section rebuilt from the draft, preserving fields this form
    /// applies through other paths (theme, bindings, the custom theme
    /// registry) at their `previous` values.
    pub fn to_ui(&self, previous: &UiConfig) -> UiConfig {
        let mut ui = previous.clone();
        if let Ok(value) = self.ui_room_height.trim().parse::<u16>() {
            ui.room_height = value;
        }
        if let Ok(value) = self.ui_max_composer_height.trim().parse::<u16>() {
            ui.max_composer_height = value;
        }
        ui.composer_padding = self.ui_composer_padding;
        if let Ok(value) = self.ui_max_messages.trim().parse::<u32>() {
            ui.max_messages = value;
        }
        if let Ok(value) = self.ui_overscan.trim().parse::<u32>() {
            ui.overscan = value;
        }
        ui
    }

    /// The url-open command with surrounding whitespace and empty rows dropped.
    pub fn url_open_clean(&self) -> Vec<String> {
        let mut command = Vec::with_capacity(self.url_open.len());
        for argument in &self.url_open {
            let argument = argument.trim();
            if !argument.is_empty() {
                command.push(argument.to_string());
            }
        }
        command
    }

    pub fn to_history(&self) -> HistoryConfig {
        let location = self.history_location.trim();
        HistoryConfig {
            enabled: self.history_enabled,
            location: (!location.is_empty()).then(|| location.to_string()),
        }
    }

    pub fn input_selection(&self) -> Option<&str> {
        self.input_device_id.as_deref()
    }

    pub fn output_selection(&self) -> Option<&str> {
        self.output_device_id.as_deref()
    }

    pub fn set_input_selection(&mut self, selection: Option<String>) -> bool {
        if self.input_device_id == selection {
            return false;
        }
        self.input_device_id = selection;
        true
    }

    pub fn restore_input_selection(&mut self, selection: Option<String>) {
        self.input_device_id = selection;
    }

    pub fn set_output_selection(&mut self, selection: Option<String>) -> bool {
        if self.output_device_id == selection {
            return false;
        }
        self.output_device_id = selection;
        true
    }

    pub fn restore_output_selection(&mut self, selection: Option<String>) {
        self.output_device_id = selection;
    }

    /// Returns a blocking reason when either raw device string is invalid. Used
    /// to gate live apply and Save so a malformed ALSA string is never opened.
    pub fn device_string_invalid(&self) -> Option<String> {
        let input = self
            .input_raw
            .then(|| raw_device_error(self.input_device_id.as_deref().unwrap_or("")))
            .flatten();
        let output = self
            .output_raw
            .then(|| raw_device_error(self.output_device_id.as_deref().unwrap_or("")))
            .flatten();
        input.or(output)
    }

    pub fn settings_text_invalid(&self) -> Option<String> {
        self.device_string_invalid()
            .or_else(|| output_volume_field_error(&self.output_volume))
            .or_else(|| max_amplification_error(&self.max_amplification))
            .or_else(|| notification_volume_error(&self.message_notification_volume))
            .or_else(|| notification_volume_error(&self.peer_join_notification_volume))
            .or_else(|| notification_volume_error(&self.peer_leave_notification_volume))
            .or_else(|| web_bind_error(&self.web_bind))
            .or_else(|| {
                download_path_error(
                    self.download_mode == DownloadMode::Persistent,
                    &self.download_path,
                )
            })
            .or_else(|| {
                (self.download_mode == DownloadMode::Memory)
                    .then(|| download_memory_error(&self.download_memory_mb))
                    .flatten()
            })
            .or_else(|| self.ui_text_invalid())
            .or_else(|| self.files_text_invalid())
            .or_else(|| self.web_origins_invalid())
            .or_else(|| self.latency_field_error())
            .or_else(|| self.latency_cross_error())
    }

    fn ui_text_invalid(&self) -> Option<String> {
        labeled_error(
            "room height",
            int_range_error(&self.ui_room_height, UI_ROOM_HEIGHT_RANGE),
        )
        .or_else(|| {
            labeled_error(
                "composer height",
                int_range_error(&self.ui_max_composer_height, UI_MAX_COMPOSER_HEIGHT_RANGE),
            )
        })
        .or_else(|| {
            labeled_error(
                "max messages",
                int_range_error(&self.ui_max_messages, UI_MAX_MESSAGES_RANGE),
            )
        })
        .or_else(|| {
            labeled_error(
                "overscan",
                int_range_error(&self.ui_overscan, UI_OVERSCAN_RANGE),
            )
        })
    }

    fn files_text_invalid(&self) -> Option<String> {
        labeled_error(
            "max download",
            positive_mib_error(&self.files_max_download_mb),
        )
        .or_else(|| labeled_error("max upload", positive_mib_error(&self.files_max_upload_mb)))
        .or_else(|| labeled_error("upload rate", byte_size_error(&self.files_upload_rate)))
    }

    fn web_origins_invalid(&self) -> Option<String> {
        for origin in &self.web_allowed_origins {
            if let Some(error) = web_origin_error(origin) {
                return Some(format!("allowed origin {error}"));
            }
        }
        None
    }

    /// The first per-field latency error, checked field-by-field so each row
    /// can also validate itself with the same range.
    pub(crate) fn latency_field_error(&self) -> Option<String> {
        let fields = [
            (
                "start delay",
                &self.latency_ms.neteq_start_delay_ms,
                NETEQ_START_DELAY_MS_RANGE,
            ),
            (
                "min delay",
                &self.latency_ms.neteq_min_delay_ms,
                NETEQ_MIN_DELAY_MS_RANGE,
            ),
            (
                "base min delay",
                &self.latency_ms.neteq_base_minimum_delay_ms,
                NETEQ_BASE_MINIMUM_DELAY_MS_RANGE,
            ),
            (
                "max delay",
                &self.latency_ms.neteq_max_delay_ms,
                NETEQ_MAX_DELAY_MS_RANGE,
            ),
            (
                "queue bound",
                &self.latency_ms.hard_queue_bound_ms,
                HARD_QUEUE_BOUND_MS_RANGE,
            ),
            (
                "initial buffer",
                &self.latency_ms.initial_buffer_ms,
                INITIAL_BUFFER_MS_RANGE,
            ),
            (
                "reorder delay",
                &self.latency_ms.max_reorder_delay_ms,
                MAX_REORDER_DELAY_MS_RANGE,
            ),
            (
                "period margin",
                &self.latency_ms.device_period_margin_ms,
                DEVICE_PERIOD_MARGIN_MS_RANGE,
            ),
            (
                "silence stop",
                &self.latency_ms.capture_long_silence_stop_ms,
                CAPTURE_LONG_SILENCE_STOP_MS_RANGE,
            ),
            (
                "silence preroll",
                &self.latency_ms.capture_silence_preroll_ms,
                CAPTURE_SILENCE_PREROLL_MS_RANGE,
            ),
            (
                "silence ramp",
                &self.latency_ms.capture_silence_ramp_ms,
                CAPTURE_SILENCE_RAMP_MS_RANGE,
            ),
        ];
        for (label, text, range) in fields {
            if let Some(error) = latency_ms_error(text, range) {
                return Some(format!("{label} {error}"));
            }
        }
        labeled_error(
            "silence vad max",
            vad_level_error(&self.latency_ms.silence_vad_max),
        )
    }

    /// The cross-field latency constraint violation, or `None`. Suppressed
    /// while any single field is out of range, so its own error shows instead.
    pub(crate) fn latency_cross_error(&self) -> Option<String> {
        if self.latency_field_error().is_some() {
            return None;
        }
        self.latency_config().to_tuning().validate().err()
    }

    /// Reason the rnnoise-tuning rows are inert, or `None` when they apply. The
    /// settings form uses this to grey out and skip the suppression, release,
    /// and typing-gate rows.
    pub(crate) fn denoise_tuning_disabled(&self) -> Option<&'static str> {
        (self.denoise != DenoiseConfig::RnnNoise).then_some("Requires the rnnoise denoise engine.")
    }

    /// Reason the typing-gate threshold rows are inert, or `None`. They require
    /// both the rnnoise engine and the typing gate enabled.
    pub(crate) fn typing_vad_disabled(&self) -> Option<&'static str> {
        self.denoise_tuning_disabled()
            .or_else(|| (!self.typing_suppression).then_some("Requires Typing Gate to be on."))
    }

    pub fn input_buffer_request(&self) -> BufferRequest {
        parse_buffer_size(&self.input_buffer).to_request(DEFAULT_INPUT_TARGET_LATENCY)
    }

    pub fn output_buffer_request(&self) -> BufferRequest {
        parse_buffer_size(&self.output_buffer).to_request(DEFAULT_OUTPUT_TARGET_LATENCY)
    }

    pub fn max_amplification(&self) -> f32 {
        parse_db_value(&self.max_amplification).unwrap_or(DEFAULT_MAX_AMPLIFICATION)
    }

    pub fn suppression_strength(&self) -> f32 {
        DENOISE_SUPPRESSIONS[self.suppression_index]
    }

    pub fn release_value(&self) -> f32 {
        DENOISE_RELEASES[self.release_index]
    }

    pub fn typing_vad_enter(&self) -> f32 {
        DENOISE_TYPING_VAD_THRESHOLDS[self.typing_vad_enter_index]
    }

    pub fn typing_vad_release(&self) -> f32 {
        DENOISE_TYPING_VAD_THRESHOLDS[self.typing_vad_release_index]
    }

    pub fn message_notification_volume_db(&self) -> f32 {
        parse_db_value(&self.message_notification_volume).unwrap_or(0.0)
    }

    pub fn peer_join_notification_volume_db(&self) -> f32 {
        parse_db_value(&self.peer_join_notification_volume).unwrap_or(0.0)
    }

    pub fn peer_leave_notification_volume_db(&self) -> f32 {
        parse_db_value(&self.peer_leave_notification_volume).unwrap_or(0.0)
    }
}

pub(crate) fn volume_db_label(value: f32) -> String {
    if value == 0.0 {
        "0 dB".to_string()
    } else {
        format!("{value:+.0} dB")
    }
}

pub fn form_bindings_label(value: FormBindings) -> &'static str {
    match value {
        FormBindings::Standard => "Standard",
        FormBindings::Vim => "Vim",
    }
}

/// Tri-state control for a per-server or per-room override of an on/off
/// setting. `Inherit` maps to an unset (`None`) config field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverrideToggle {
    Inherit,
    On,
    Off,
}

impl OverrideToggle {
    pub const ALL: [OverrideToggle; 3] = [
        OverrideToggle::Inherit,
        OverrideToggle::On,
        OverrideToggle::Off,
    ];

    pub fn from_option(value: Option<bool>) -> Self {
        match value {
            None => OverrideToggle::Inherit,
            Some(true) => OverrideToggle::On,
            Some(false) => OverrideToggle::Off,
        }
    }

    pub fn to_option(self) -> Option<bool> {
        match self {
            OverrideToggle::Inherit => None,
            OverrideToggle::On => Some(true),
            OverrideToggle::Off => Some(false),
        }
    }

    /// The visible option label. `inherited` is the effective value one level
    /// up, so `Inherit` reads as what it resolves to.
    pub fn label(self, inherited: bool) -> String {
        match self {
            OverrideToggle::Inherit => {
                format!("inherit ({})", if inherited { "on" } else { "off" })
            }
            OverrideToggle::On => "on".to_string(),
            OverrideToggle::Off => "off".to_string(),
        }
    }
}

/// Four-state control for a per-server or per-room download override:
/// inherit the next level up, or explicitly off / in-memory / persistent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadChoice {
    Inherit,
    Off,
    Memory,
    Persistent,
}

impl DownloadChoice {
    pub const ALL: [DownloadChoice; 4] = [
        DownloadChoice::Inherit,
        DownloadChoice::Off,
        DownloadChoice::Memory,
        DownloadChoice::Persistent,
    ];

    pub fn from_override(value: Option<DownloadMode>) -> Self {
        match value {
            None => DownloadChoice::Inherit,
            Some(DownloadMode::Off) => DownloadChoice::Off,
            Some(DownloadMode::Memory) => DownloadChoice::Memory,
            Some(DownloadMode::Persistent) => DownloadChoice::Persistent,
        }
    }

    pub fn to_override(self) -> Option<DownloadMode> {
        match self {
            DownloadChoice::Inherit => None,
            DownloadChoice::Off => Some(DownloadMode::Off),
            DownloadChoice::Memory => Some(DownloadMode::Memory),
            DownloadChoice::Persistent => Some(DownloadMode::Persistent),
        }
    }

    /// Whether this choice needs the download-path field shown.
    pub fn shows_path(self) -> bool {
        matches!(self, DownloadChoice::Persistent)
    }

    /// The visible option label. `inherited` is the effective mode one level up,
    /// so `Inherit` reads as what it resolves to.
    pub fn label(self, inherited: DownloadMode) -> String {
        match self {
            DownloadChoice::Inherit => format!("inherit ({})", inherited.label()),
            DownloadChoice::Off => "off".to_string(),
            DownloadChoice::Memory => "memory".to_string(),
            DownloadChoice::Persistent => "persistent".to_string(),
        }
    }
}

/// Accepted bounds for the numeric interface-settings fields.
pub(crate) const UI_ROOM_HEIGHT_RANGE: (u64, u64) = (1, 32);
pub(crate) const UI_MAX_COMPOSER_HEIGHT_RANGE: (u64, u64) = (1, 32);
pub(crate) const UI_MAX_MESSAGES_RANGE: (u64, u64) = (100, 1_000_000);
pub(crate) const UI_OVERSCAN_RANGE: (u64, u64) = (0, 1_000);
pub(crate) const MAX_AMPLIFICATION_DB_RANGE: (f32, f32) = (0.0, 30.0);

/// Prefixes `error` with the human field name for the settings status line.
fn labeled_error(label: &str, error: Option<String>) -> Option<String> {
    error.map(|error| format!("{label} {error}"))
}

/// Validates integer field text against an inclusive range.
pub(crate) fn int_range_error(text: &str, (min, max): (u64, u64)) -> Option<String> {
    match text.trim().parse::<u64>() {
        Ok(value) if (min..=max).contains(&value) => None,
        _ => Some(format!("must be {min}-{max}")),
    }
}

/// Parses a dB field, accepting either a bare number or a trailing `dB`.
pub(crate) fn parse_db_value(text: &str) -> Option<f32> {
    let text = text.trim();
    let number = text
        .strip_suffix("dB")
        .or_else(|| text.strip_suffix("db"))
        .unwrap_or(text)
        .trim();
    number.parse::<f32>().ok().filter(|value| value.is_finite())
}

fn db_value_text(value: f32) -> String {
    value.to_string()
}

fn db_range_error(text: &str, (min, max): (f32, f32)) -> Option<String> {
    match parse_db_value(text) {
        Some(value) if (min..=max).contains(&value) => None,
        _ => Some(format!("must be between {min} and {max} dB")),
    }
}

pub(crate) fn max_amplification_error(text: &str) -> Option<String> {
    db_range_error(text, MAX_AMPLIFICATION_DB_RANGE)
}

pub(crate) fn notification_volume_error(text: &str) -> Option<String> {
    db_range_error(
        text,
        (MIN_NOTIFICATION_VOLUME_DB, MAX_NOTIFICATION_VOLUME_DB),
    )
}

/// Validates a millisecond latency field against its shared tuning range.
pub(crate) fn latency_ms_error(text: &str, (min, max): (u64, u64)) -> Option<String> {
    match text.trim().parse::<u64>() {
        Ok(value) if (min..=max).contains(&value) => None,
        _ => Some(format!("must be {min}-{max} ms")),
    }
}

/// Validates the silence VAD level field (`0..=255`).
pub(crate) fn vad_level_error(text: &str) -> Option<String> {
    match text.trim().parse::<u8>() {
        Ok(_) => None,
        _ => Some("must be 0-255".to_string()),
    }
}

/// Validates a required positive MiB count field.
pub(crate) fn positive_mib_error(text: &str) -> Option<String> {
    match parse_mib_count(text.trim()) {
        Some(_) => None,
        None => Some("must be a positive MiB count".to_string()),
    }
}

/// Validates a byte-count field with an optional `K`/`M`/`G` suffix. `0` is
/// accepted (it means unthrottled for the upload rate).
pub(crate) fn byte_size_error(text: &str) -> Option<String> {
    match parse_byte_size(text) {
        Some(_) => None,
        None => Some("must be a byte count, optionally with a K/M/G suffix".to_string()),
    }
}

/// Validates one allowed-origins row: empty (removed on commit) or an http(s)
/// origin without a path.
pub(crate) fn web_origin_error(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || valid_web_origin(text) {
        None
    } else {
        Some("must be an http(s) origin without a path".to_string())
    }
}

/// Parses a byte count with an optional `K`/`M`/`G` suffix (powers of 1024).
pub fn parse_byte_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let (digits, multiplier) = match text.as_bytes().last() {
        Some(b'k' | b'K') => (&text[..text.len() - 1], 1024),
        Some(b'm' | b'M') => (&text[..text.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
        _ => (text, 1),
    };
    let value: u64 = digits.trim().parse().ok()?;
    Some(value.saturating_mul(multiplier))
}

/// Parses a MiB-limit override field. Empty inherits (`None`); otherwise the
/// value is a positive count of 1024*1024-byte units.
pub fn parse_mb_limit(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    match parse_mib_count(text) {
        Some(mb) => Ok(Some(mb)),
        None => Err(format!("invalid MiB limit: {text}")),
    }
}

pub fn mb_limit_error(text: &str) -> Option<String> {
    parse_mb_limit(text).err()
}

/// Renders a MiB-limit override as editable text; empty means inherit.
pub fn mb_limit_text(value: Option<u64>) -> String {
    value.map(|mb| mb.to_string()).unwrap_or_default()
}

pub fn default_download_path_text() -> String {
    paths::default_download_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "files".to_string())
}

pub fn download_path_error(persistent: bool, value: &str) -> Option<String> {
    if persistent && value.trim().is_empty() {
        Some("download path cannot be empty while downloads are saved to disk".to_string())
    } else {
        None
    }
}

/// Validates the in-memory download buffer size field: a non-empty positive MiB
/// count.
pub fn download_memory_error(value: &str) -> Option<String> {
    match parse_mib_count(value.trim()) {
        Some(mb) if mb > 0 => None,
        _ => Some("memory buffer size must be a positive MiB count".to_string()),
    }
}

fn parse_mib_count(text: &str) -> Option<u64> {
    let mb = text.trim().parse::<u64>().ok()?;
    (mb > 0).then_some(mb)
}

/// Renders a [`BufferSize`] as the editable settings text: `"default"` or the
/// raw sample count.
fn buffer_size_text(size: BufferSize) -> String {
    match size {
        BufferSize::Default => "default".to_string(),
        BufferSize::Samples(samples) => samples.to_string(),
    }
}

/// Parses the editable settings text into a [`BufferSize`]. Empty input or
/// `"default"` (any case) means [`BufferSize::Default`]; a positive integer is
/// an explicit sample count. Anything else falls back to the default.
fn parse_buffer_size(text: &str) -> BufferSize {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        return BufferSize::Default;
    }
    match trimmed.parse::<u32>() {
        Ok(samples) if samples > 0 => BufferSize::Samples(samples),
        _ => BufferSize::Default,
    }
}

/// Validates buffer field text, returning an error message when it is neither
/// `default`/empty nor an integer within `[MIN_BUFFER_SAMPLES, MAX_BUFFER_SAMPLES]`.
pub(crate) fn buffer_field_error(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("default") {
        return None;
    }
    let invalid = || {
        Some(format!(
            "buffer must be \"default\" or {MIN_BUFFER_SAMPLES}-{MAX_BUFFER_SAMPLES} samples"
        ))
    };
    match trimmed.parse::<u32>() {
        Ok(samples) if (MIN_BUFFER_SAMPLES..=MAX_BUFFER_SAMPLES).contains(&samples) => None,
        _ => invalid(),
    }
}

pub(crate) fn output_volume_field_error(text: &str) -> Option<String> {
    parse_output_volume_percent(text).err()
}

/// Validates a raw ALSA device string. Empty means system default. Otherwise it
/// must look like an ALSA pcm name.
pub(crate) fn raw_device_error(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let pcm = trimmed
        .strip_prefix("alsa:")
        .or_else(|| trimmed.strip_prefix("alsa/"))
        .unwrap_or(trimmed);
    if pcm.is_empty() || crate::audio::looks_like_alsa_pcm_name(pcm) {
        return None;
    }
    Some("not a valid ALSA device string (e.g. hw:0,0)".to_string())
}

pub(crate) fn web_bind_error(text: &str) -> Option<String> {
    let trimmed = text.trim();
    match trimmed.parse::<std::net::SocketAddr>() {
        Ok(_) => None,
        Err(error) => Some(format!("web bind must be a socket address: {error}")),
    }
}

/// Maps raw device editor text to a selection: trimmed, with an empty string
/// becoming `None` (system default).
pub(crate) fn raw_device_selection(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Detects whether a stored device selection was a hand-entered raw ALSA pcm
/// string rather than an enumerated picker id (which carries an `alsa:` backend
/// prefix). Used to seed raw mode when settings open on a hand-edited config.
fn is_raw_device_selection(selection: &str) -> bool {
    if let Some(forced) = selection.strip_prefix("alsa/") {
        return crate::audio::looks_like_alsa_pcm_name(forced);
    }
    if selection.starts_with("alsa:") {
        return false;
    }
    crate::audio::looks_like_alsa_pcm_name(selection)
}

/// Returns the index of the option nearest to `value`, falling back to the
/// option nearest `default` when `value` is non-finite.
fn nearest_index(options: &[f32], value: f32, default: f32) -> usize {
    let value = if value.is_finite() { value } else { default };
    options
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            (*left - value)
                .abs()
                .partial_cmp(&(*right - value).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct AudioDeviceItem {
    pub selection: Option<String>,
    pub aliases: Vec<String>,
    pub backend_id: Option<String>,
    pub device_index: Option<u32>,
    pub name: String,
    pub search_text: String,
    pub rank: i32,
    pub supported: bool,
    pub preview: Option<StreamPreview>,
    pub issue: Option<String>,
    pub variants: Vec<AudioDeviceVariant>,
    pub default_source: &'static str,
}

#[derive(Clone, Debug)]
pub struct AudioDeviceVariant {
    pub index: u32,
    pub rank: i32,
    pub supported: bool,
    pub preview: Option<StreamPreview>,
    pub issue: Option<String>,
}

pub type AudioInputItem = AudioDeviceItem;
pub type AudioOutputItem = AudioDeviceItem;
pub type AudioInputPickerState = AudioDevicePickerState;
pub type AudioOutputPickerState = AudioDevicePickerState;

#[derive(Clone, Debug, Default)]
pub struct AudioDevicePickerState {
    pub selector: FuzzySelect,
    pub open: bool,
    pub searching: bool,
    restore_selection: Option<Option<String>>,
}

impl AudioDevicePickerState {
    pub fn reset(&mut self, items: &[AudioDeviceItem], selection: Option<&str>) {
        self.open = false;
        self.searching = false;
        self.restore_selection = None;
        self.selector.clear_query();
        self.selector.refresh(items);
        self.selector
            .set_selected_item(audio_device_item_index(items, selection));
    }

    pub fn open(&mut self, items: &[AudioDeviceItem], selection: Option<&str>) {
        self.open = true;
        self.searching = false;
        self.restore_selection = Some(selection.map(str::to_owned));
        self.selector.clear_query();
        self.selector.refresh(items);
        self.selector
            .set_selected_item(audio_device_item_index(items, selection));
    }

    pub fn refresh_items(&mut self, items: &[AudioDeviceItem], selection: Option<&str>) {
        self.selector.refresh(items);
        self.selector
            .set_selected_item(audio_device_item_index(items, selection));
    }

    pub fn start_search(&mut self, items: &[AudioDeviceItem]) {
        if !self.open {
            return;
        }
        self.searching = true;
        self.selector.clear_query();
        self.selector.refresh(items);
    }

    pub fn edit_search(&mut self, key: extui::event::KeyEvent, items: &[AudioDeviceItem]) -> bool {
        if !self.open || !self.searching || !self.selector.edit_query(key) {
            return false;
        }
        self.selector.refresh(items);
        true
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.open {
            self.selector.move_selection(delta);
        }
    }

    pub fn confirm(&mut self, items: &[AudioDeviceItem]) -> Option<Option<String>> {
        if !self.open {
            return None;
        }
        let selection = self
            .selector
            .current_item_index()
            .and_then(|index| items.get(index))
            .map(|item| item.selection.clone())?;
        self.reset(items, selection.as_deref());
        Some(selection)
    }

    pub fn cancel(&mut self, items: &[AudioDeviceItem]) -> Option<Option<String>> {
        if !self.open {
            return None;
        }
        let selection = self.restore_selection.take().unwrap_or(None);
        self.reset(items, selection.as_deref());
        Some(selection)
    }
}

impl AudioDeviceItem {
    pub fn detail(&self) -> String {
        match &self.preview {
            Some(preview) => stream_preview_detail(preview),
            None => self
                .issue
                .clone()
                .unwrap_or_else(|| self.default_source.to_string()),
        }
    }

    pub fn primary_metadata(&self) -> String {
        let mut metadata = self.detail();
        if let Some(id) = &self.backend_id {
            metadata.push_str("  ");
            metadata.push_str(id);
        }
        metadata
    }

    pub fn matches_selection(&self, selection: Option<&str>) -> bool {
        match selection {
            None => self.selection.is_none(),
            Some(selection) => {
                self.selection.as_deref() == Some(selection)
                    || self.aliases.iter().any(|alias| alias == selection)
            }
        }
    }
}

impl SelectableItem for AudioDeviceItem {
    fn search_text(&self) -> &str {
        &self.search_text
    }

    fn rank(&self) -> i32 {
        self.rank
    }
}

pub fn audio_input_items(devices: &[DeviceInfo]) -> Vec<AudioInputItem> {
    audio_device_items(devices, AudioDeviceKind::Input)
}

pub fn audio_output_items(devices: &[DeviceInfo]) -> Vec<AudioOutputItem> {
    audio_device_items(devices, AudioDeviceKind::Output)
}

fn audio_device_items(devices: &[DeviceInfo], kind: AudioDeviceKind) -> Vec<AudioDeviceItem> {
    let mut items = Vec::with_capacity(devices.len() + 1);
    items.push(AudioDeviceItem {
        selection: None,
        aliases: Vec::new(),
        backend_id: None,
        device_index: None,
        name: "System default".to_string(),
        search_text: format!("system default {}", kind.name()),
        rank: 900,
        supported: true,
        preview: None,
        issue: None,
        variants: Vec::new(),
        default_source: kind.default_source(),
    });

    let mut grouped: Vec<(String, AudioDeviceItem)> = Vec::new();
    for (index, device) in devices.iter().enumerate() {
        let item = audio_device_item(index as u32, device, kind);
        let key = audio_device_group_key(device, kind);
        if let Some((_, existing)) = grouped
            .iter_mut()
            .find(|(existing_key, _)| *existing_key == key)
        {
            merge_audio_device_item(existing, item);
        } else {
            grouped.push((key, item));
        }
    }

    items.extend(grouped.into_iter().map(|(_, item)| item));
    items
}

pub fn audio_device_item_index(
    items: &[AudioDeviceItem],
    selection: Option<&str>,
) -> Option<usize> {
    items
        .iter()
        .position(|item| item.matches_selection(selection))
}

pub fn selected_audio_input_label(items: &[AudioInputItem], selection: Option<&str>) -> String {
    selected_audio_device_label(items, selection)
}

pub fn selected_audio_output_label(items: &[AudioOutputItem], selection: Option<&str>) -> String {
    selected_audio_device_label(items, selection)
}

fn selected_audio_device_label(items: &[AudioDeviceItem], selection: Option<&str>) -> String {
    items
        .iter()
        .find(|item| item.matches_selection(selection))
        .map(|item| {
            if item.selection.is_some() {
                format!("{} ({})", item.name, item.detail())
            } else {
                item.name.clone()
            }
        })
        .unwrap_or_else(|| {
            selection
                .map(|selection| format!("selected device `{selection}` is unavailable"))
                .unwrap_or_else(|| "System default".to_string())
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioDeviceKind {
    Input,
    Output,
}

impl AudioDeviceKind {
    fn name(self) -> &'static str {
        match self {
            AudioDeviceKind::Input => "input",
            AudioDeviceKind::Output => "output",
        }
    }

    fn default_source(self) -> &'static str {
        match self {
            AudioDeviceKind::Input => "OS default input; exact device chosen when capture starts",
            AudioDeviceKind::Output => {
                "OS default output; exact device chosen when playback starts"
            }
        }
    }
}

fn audio_device_item(index: u32, device: &DeviceInfo, kind: AudioDeviceKind) -> AudioDeviceItem {
    let rank = audio_device_rank(device, kind);
    let mut search_text = device.name.clone();
    if let Some(id) = &device.id {
        search_text.push(' ');
        search_text.push_str(id);
        if let Some(alsa_pcm) = id.strip_prefix("alsa:") {
            search_text.push(' ');
            search_text.push_str(alsa_pcm);
        }
    }
    if let Some(preview) = &device.preview {
        search_text.push(' ');
        search_text.push_str(&stream_preview_detail(preview));
    }
    if let Some(issue) = &device.issue {
        search_text.push(' ');
        search_text.push_str(issue);
    }

    let stable_selection = match kind {
        AudioDeviceKind::Input => crate::audio::stable_input_device_id(&device.name),
        AudioDeviceKind::Output => crate::audio::stable_output_device_id(&device.name),
    };
    let selection = device
        .id
        .clone()
        .unwrap_or_else(|| stable_selection.clone());
    let mut aliases = Vec::new();
    if selection != stable_selection {
        aliases.push(stable_selection);
    }
    if let Some(alsa_pcm) = selection.strip_prefix("alsa:")
        && alsa_pcm != selection
    {
        aliases.push(alsa_pcm.to_string());
        aliases.push(format!("alsa/{alsa_pcm}"));
    }

    AudioDeviceItem {
        selection: Some(selection),
        aliases,
        backend_id: device.id.clone(),
        device_index: Some(index),
        name: device.name.clone(),
        search_text,
        rank,
        supported: device.supported,
        preview: device.preview.clone(),
        issue: device.issue.clone(),
        variants: vec![AudioDeviceVariant {
            index,
            rank,
            supported: device.supported,
            preview: device.preview.clone(),
            issue: device.issue.clone(),
        }],
        default_source: kind.default_source(),
    }
}

fn merge_audio_device_item(existing: &mut AudioDeviceItem, item: AudioDeviceItem) {
    existing.search_text.push(' ');
    existing.search_text.push_str(&item.search_text);
    if let Some(selection) = &item.selection {
        push_audio_device_alias(&mut existing.aliases, selection);
    }
    for alias in &item.aliases {
        push_audio_device_alias(&mut existing.aliases, alias);
    }
    existing.variants.extend(item.variants);
    existing.variants.sort_by(|a, b| {
        b.rank
            .cmp(&a.rank)
            .then_with(|| b.supported.cmp(&a.supported))
            .then_with(|| a.index.cmp(&b.index))
    });

    if item.rank > existing.rank
        || item.supported && !existing.supported
        || item.rank == existing.rank
            && item
                .device_index
                .zip(existing.device_index)
                .is_some_and(|(item, existing)| item < existing)
    {
        if let Some(selection) = &existing.selection {
            push_audio_device_alias(&mut existing.aliases, selection);
        }
        existing.selection = item.selection;
        existing.backend_id = item.backend_id;
        existing.device_index = item.device_index;
        existing.name = item.name;
        existing.rank = item.rank;
        existing.supported = item.supported;
        existing.preview = item.preview;
        existing.issue = item.issue;
        existing.default_source = item.default_source;
    }
}

fn push_audio_device_alias(aliases: &mut Vec<String>, alias: &str) {
    if !aliases.iter().any(|existing| existing == alias) {
        aliases.push(alias.to_string());
    }
}

fn audio_device_group_key(device: &DeviceInfo, kind: AudioDeviceKind) -> String {
    if let Some(id) = &device.id
        && is_explicit_alsa_device_id(id)
    {
        return id.clone();
    }
    match kind {
        AudioDeviceKind::Input => crate::audio::stable_input_device_id(&device.name),
        AudioDeviceKind::Output => crate::audio::stable_output_device_id(&device.name),
    }
}

fn is_explicit_alsa_device_id(id: &str) -> bool {
    let Some(pcm) = id.strip_prefix("alsa:") else {
        return false;
    };
    let head = pcm
        .split([':', ','])
        .next()
        .unwrap_or(pcm)
        .to_ascii_lowercase();
    matches!(
        head.as_str(),
        "hw" | "plughw"
            | "sysdefault"
            | "front"
            | "center_lfe"
            | "side"
            | "iec958"
            | "spdif"
            | "dmix"
            | "dsnoop"
            | "usbstream"
    ) || head.starts_with("surround")
        || head.starts_with("hdmi")
}

fn audio_device_rank(device: &DeviceInfo, kind: AudioDeviceKind) -> i32 {
    match kind {
        AudioDeviceKind::Input => audio_input_rank(device),
        AudioDeviceKind::Output => audio_output_rank(device),
    }
}

fn audio_input_rank(device: &DeviceInfo) -> i32 {
    let name = device.name.to_ascii_lowercase();
    let mut rank = if device.supported { 1_000 } else { -25_000 };

    for (needle, bonus) in [
        ("umc", 1_800),
        ("microphone", 1_700),
        ("condenser", 1_200),
        ("hd-audio", 1_200),
        ("usb audio", 1_100),
        ("analog", 700),
        ("mic", 550),
        ("headset", 450),
        ("webcam", 350),
        ("camera", 300),
        ("alc", 300),
        ("array", 250),
        ("usb", 220),
        ("built-in", 180),
        ("internal", 160),
        ("input", 120),
    ] {
        if name.contains(needle) {
            rank += bonus;
        }
    }

    for (needle, penalty) in [
        ("discard all samples", 55_000),
        ("generate zero samples", 55_000),
        ("zero samples", 55_000),
        ("rate converter plugin", 16_000),
        ("plugin using", 16_000),
        ("plugin for", 16_000),
        ("resampler", 14_000),
        ("upmix", 14_000),
        ("downmix", 14_000),
        ("pipewire sound server", 12_000),
        ("pulseaudio sound server", 12_000),
        ("jack audio connection kit", 12_000),
        ("open sound system", 12_000),
        ("default alsa output", 12_000),
        ("loopback", 18_000),
        ("monitor", 18_000),
        ("stereo mix", 16_000),
        ("what u hear", 16_000),
        ("playback", 15_000),
        ("output", 15_000),
        ("speaker", 12_000),
        ("sink", 12_000),
        ("desktop audio", 12_000),
        ("system audio", 12_000),
        ("null", 12_000),
        ("virtual", 6_000),
    ] {
        if name.contains(needle) {
            rank -= penalty;
        }
    }

    if let Some(preview) = &device.preview {
        match preview.channels {
            1 => rank += 120,
            2 => rank += 70,
            channels => rank -= i32::from(channels.saturating_sub(2)) * 20,
        }
    }

    rank
}

fn audio_output_rank(device: &DeviceInfo) -> i32 {
    let name = device.name.to_ascii_lowercase();
    let mut rank = if device.supported { 1_000 } else { -25_000 };

    for (needle, bonus) in [
        ("headphone", 1_800),
        ("speaker", 1_700),
        ("headset", 1_500),
        ("hd-audio", 1_200),
        ("usb audio", 1_100),
        ("analog", 900),
        ("output", 700),
        ("playback", 600),
        ("sink", 550),
        ("built-in", 350),
        ("internal", 300),
        ("alc", 300),
        ("usb", 220),
        ("hdmi", 180),
        ("displayport", 160),
    ] {
        if name.contains(needle) {
            rank += bonus;
        }
    }

    for (needle, penalty) in [
        ("discard all samples", 55_000),
        ("generate zero samples", 55_000),
        ("zero samples", 55_000),
        ("rate converter plugin", 16_000),
        ("plugin using", 16_000),
        ("plugin for", 16_000),
        ("resampler", 14_000),
        ("upmix", 14_000),
        ("downmix", 14_000),
        ("pipewire sound server", 12_000),
        ("pulseaudio sound server", 12_000),
        ("jack audio connection kit", 12_000),
        ("open sound system", 12_000),
        ("loopback", 18_000),
        ("monitor", 18_000),
        ("stereo mix", 16_000),
        ("what u hear", 16_000),
        ("microphone", 16_000),
        ("mic", 12_000),
        ("webcam", 12_000),
        ("camera", 12_000),
        ("array", 12_000),
        ("capture", 12_000),
        ("input", 12_000),
        ("null", 12_000),
        ("virtual", 6_000),
    ] {
        if name.contains(needle) {
            rank -= penalty;
        }
    }

    if let Some(preview) = &device.preview {
        match preview.channels {
            1 => rank += 60,
            2 => rank += 160,
            channels => rank -= i32::from(channels.saturating_sub(2)) * 10,
        }
    }

    rank
}

fn stream_preview_detail(preview: &StreamPreview) -> String {
    let mut detail = format!("{} ch {}", preview.channels, preview.sample_format);
    if let cpal::BufferSize::Fixed(frames) = preview.buffer_size {
        detail.push_str(&format!(", {frames} frame buffer"));
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, supported: bool) -> DeviceInfo {
        device_with_id(None, name, supported)
    }

    fn device_with_id(id: Option<&str>, name: &str, supported: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.map(str::to_string),
            name: name.to_string(),
            supported,
            preview: None,
            issue: (!supported).then(|| "unsupported".to_string()),
        }
    }

    #[test]
    fn settings_draft_never_persists_loopback() {
        // Loopback is a transient settings-only toggle: it must default off and
        // never leak into the saved audio config.
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        assert!(!draft.loopback);

        draft.loopback = true;
        let audio = draft.to_audio();
        // Re-deriving a draft from the produced config keeps loopback off.
        assert!(!SettingsDraft::from_audio(&audio).loopback);
    }

    #[test]
    fn settings_draft_round_trips_denoise_engine() {
        let config = AudioConfig {
            denoise: DenoiseConfig::Spectral,
            ..AudioConfig::default()
        };
        let mut draft = SettingsDraft::from_audio(&config);
        assert_eq!(draft.to_audio().denoise, DenoiseConfig::Spectral);

        draft.denoise = DenoiseConfig::RnnNoise;
        assert_eq!(draft.to_audio().denoise, DenoiseConfig::RnnNoise);
    }

    #[test]
    fn settings_draft_round_trips_dred() {
        let config = AudioConfig {
            dred: DredConfig::Off,
            ..AudioConfig::default()
        };
        let mut draft = SettingsDraft::from_audio(&config);
        assert_eq!(draft.to_audio().dred, DredConfig::Off);

        draft.dred = DredConfig::On;
        assert_eq!(draft.to_audio().dred, DredConfig::On);
    }

    #[test]
    fn settings_draft_round_trips_web_options() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        let web = WebConfig {
            allowed_origins: vec!["https://chat.example.test".to_string()],
            readonly: false,
            autoplay: WebAutoplay::WithAudio,
            viewer: WebViewer::Tab,
            ..WebConfig::default()
        };

        draft.set_web_from_config(&web);

        assert_eq!(draft.to_web(), web);
    }

    #[test]
    fn settings_draft_round_trips_ui_knobs() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        let ui = UiConfig {
            room_height: 7,
            max_composer_height: 12,
            composer_padding: false,
            max_messages: 12_345,
            overscan: 48,
            ..UiConfig::default()
        };

        draft.set_ui_from_config(&ui);
        let produced = draft.to_ui(&UiConfig::default());

        assert_eq!(produced.room_height, 7);
        assert_eq!(produced.max_composer_height, 12);
        assert!(!produced.composer_padding);
        assert_eq!(produced.max_messages, 12_345);
        assert_eq!(produced.overscan, 48);
    }

    #[test]
    fn settings_draft_round_trips_file_limits() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        let files = FileConfig {
            max_download_mb: 100,
            max_upload_mb: 2_048,
            upload_rate_bytes: 512 * 1024,
            ..FileConfig::default()
        };

        draft.set_files_from_config(&files);
        let produced = draft.to_files(&FileConfig::default());

        assert_eq!(produced.max_download_mb, 100);
        assert_eq!(produced.max_upload_mb, 2_048);
        assert_eq!(produced.upload_rate_bytes, 512 * 1024);
    }

    #[test]
    fn upload_rate_field_accepts_size_suffixes() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        draft.files_upload_rate = "2M".to_string();

        let produced = draft.to_files(&FileConfig::default());
        assert_eq!(produced.upload_rate_bytes, 2 * 1024 * 1024);
        assert!(byte_size_error("2M").is_none());
        assert!(byte_size_error("0").is_none());
        assert!(byte_size_error("fast").is_some());
        assert!(byte_size_error("").is_some());
    }

    #[test]
    fn settings_draft_round_trips_p2p_privacy() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        let p2p = P2pConfig {
            enabled: true,
            candidate_privacy: CandidatePrivacy::NoHost,
            prefer_ipv6: false,
        };

        draft.set_p2p_from_config(&p2p);

        assert_eq!(draft.to_p2p(&P2pConfig::default()), p2p);
    }

    #[test]
    fn settings_draft_round_trips_latency_ms_fields() {
        let config = AudioConfig {
            latency: AudioLatencyConfig {
                neteq_start_delay_ms: 80,
                silence_vad_max: 100,
                ..AudioLatencyConfig::default()
            },
            ..AudioConfig::default()
        };

        let mut draft = SettingsDraft::from_audio(&config);
        assert_eq!(draft.latency_ms.neteq_start_delay_ms, "80");
        assert_eq!(draft.latency_ms.silence_vad_max, "100");
        assert_eq!(draft.to_audio().latency.neteq_start_delay_ms, 80);

        draft.latency_ms.neteq_start_delay_ms = "120".to_string();
        assert_eq!(draft.to_audio().latency.neteq_start_delay_ms, 120);
        // An unparseable field keeps the last-valid value instead of resetting.
        draft.latency_ms.neteq_start_delay_ms = "junk".to_string();
        assert_eq!(draft.to_audio().latency.neteq_start_delay_ms, 80);
    }

    #[test]
    fn latency_cross_error_reports_min_over_start() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        assert!(draft.latency_cross_error().is_none());

        draft.latency_ms.neteq_min_delay_ms = "200".to_string();
        draft.latency_ms.neteq_start_delay_ms = "100".to_string();
        let error = draft.latency_cross_error().unwrap();
        assert!(error.contains("neteq-min-delay-ms"), "{error}");
        assert!(draft.settings_text_invalid().is_some());

        // A per-field range error suppresses the cross-field message.
        draft.latency_ms.neteq_min_delay_ms = "abc".to_string();
        assert!(draft.latency_cross_error().is_none());
        assert!(draft.latency_field_error().is_some());
    }

    #[test]
    fn settings_text_invalid_blocks_on_bad_overscan() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        assert!(draft.settings_text_invalid().is_none());

        draft.ui_overscan = "9999".to_string();
        let error = draft.settings_text_invalid().unwrap();
        assert!(error.contains("overscan"), "{error}");
    }

    #[test]
    fn web_origin_error_rejects_paths_and_garbage() {
        assert!(web_origin_error("https://chat.example.test").is_none());
        assert!(web_origin_error("http://localhost:8080").is_none());
        assert!(web_origin_error("").is_none());
        assert!(web_origin_error("https://chat.example.test/path").is_some());
        assert!(web_origin_error("chat.example.test").is_some());
        assert!(web_origin_error("ftp://chat.example.test").is_some());
    }

    #[test]
    fn url_open_clean_drops_blank_rows() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        draft.set_url_open_from_config(&[
            "firefox".to_string(),
            "  ".to_string(),
            " --new-tab ".to_string(),
        ]);

        assert_eq!(draft.url_open_clean(), vec!["firefox", "--new-tab"]);
    }

    #[test]
    fn buffer_field_error_accepts_default_and_in_range() {
        assert!(buffer_field_error("default").is_none());
        assert!(buffer_field_error("").is_none());
        assert!(buffer_field_error("  960 ").is_none());
        assert!(buffer_field_error(&MIN_BUFFER_SAMPLES.to_string()).is_none());
        assert!(buffer_field_error(&MAX_BUFFER_SAMPLES.to_string()).is_none());
    }

    #[test]
    fn buffer_field_error_rejects_out_of_range_and_garbage() {
        assert!(buffer_field_error("0").is_some());
        assert!(buffer_field_error(&(MAX_BUFFER_SAMPLES + 1).to_string()).is_some());
        assert!(buffer_field_error("abc").is_some());
        assert!(buffer_field_error("12.5").is_some());
    }

    #[test]
    fn raw_device_error_only_flags_invalid_when_raw_mode_on() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        // Off: device strings never gate apply.
        let _ = draft.set_input_selection(Some("not a device".to_string()));
        assert!(draft.device_string_invalid().is_none());

        // On: a garbage string is flagged, a valid ALSA pcm is accepted.
        draft.input_raw = true;
        assert!(draft.device_string_invalid().is_some());

        let _ = draft.set_input_selection(raw_device_selection("hw:0,0"));
        assert_eq!(draft.input_selection(), Some("hw:0,0"));
        assert!(draft.device_string_invalid().is_none());

        // Empty means system default and is valid.
        let _ = draft.set_input_selection(raw_device_selection(""));
        assert_eq!(draft.input_selection(), None);
        assert!(draft.device_string_invalid().is_none());
    }

    #[test]
    fn from_audio_seeds_raw_mode_from_hand_entered_pcm() {
        let config = AudioConfig {
            input_device_id: Some("plughw:CARD=PCH,DEV=0".to_string()),
            output_device_id: Some("alsa:hw:CARD=2,DEV=0".to_string()),
            ..AudioConfig::default()
        };
        let draft = SettingsDraft::from_audio(&config);
        // Bare pcm string is raw, a picker backend id (alsa:...) is not.
        assert!(draft.input_raw);
        assert!(!draft.output_raw);
    }

    #[test]
    fn settings_draft_round_trips_max_amplification() {
        let config = AudioConfig {
            input_device_id: Some("usb mic".to_string()),
            output_device_id: Some("usb speakers".to_string()),
            echo_cancellation: true,
            max_amplification: 17.25,
            ..AudioConfig::default()
        };

        let draft = SettingsDraft::from_audio(&config);
        let audio = draft.to_audio();

        assert_eq!(audio.input_device_id.as_deref(), Some("usb mic"));
        assert_eq!(audio.output_device_id.as_deref(), Some("usb speakers"));
        assert!(audio.echo_cancellation);
        assert_eq!(draft.max_amplification, "17.25");
        assert_eq!(audio.max_amplification, 17.25);
    }

    #[test]
    fn settings_draft_accepts_typed_db_values() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        draft.max_amplification = "9.75 dB".to_string();
        draft.message_notification_volume = "-7.25".to_string();
        draft.peer_join_notification_volume = "1.5 dB".to_string();
        draft.peer_leave_notification_volume = "11.75".to_string();

        assert!(draft.settings_text_invalid().is_none());
        assert_eq!(draft.to_audio().max_amplification, 9.75);
        assert_eq!(
            draft.to_notifications(),
            NotificationConfig {
                sounds: NotificationSoundMode::Always,
                message_volume_db: -7.25,
                peer_join_volume_db: 1.5,
                peer_leave_volume_db: 11.75,
            }
        );

        draft.message_notification_volume = "12.1".to_string();
        assert!(draft.settings_text_invalid().is_some());
    }

    #[test]
    fn settings_draft_round_trips_notification_sounds() {
        let mut draft = SettingsDraft::from_audio(&AudioConfig::default());
        assert_eq!(draft.notification_sounds, NotificationSoundMode::Always);

        draft.notification_sounds = NotificationSoundMode::Never;
        assert_eq!(
            draft.to_notifications().sounds,
            NotificationSoundMode::Never
        );

        draft.set_notifications_from_config(&NotificationConfig {
            sounds: NotificationSoundMode::InCalls,
            ..NotificationConfig::default()
        });
        assert_eq!(draft.notification_sounds, NotificationSoundMode::InCalls);
    }

    #[test]
    fn settings_draft_round_trips_output_volume() {
        let config = AudioConfig {
            output_volume: 99.5,
            ..AudioConfig::default()
        };

        let mut draft = SettingsDraft::from_audio(&config);
        assert_eq!(draft.output_volume, "99.5%");
        assert_eq!(draft.to_audio().output_volume, 99.5);

        draft.output_volume = "50%".to_string();
        assert_eq!(draft.to_audio().output_volume, 50.0);
        assert!(output_volume_field_error("130.1%").is_some());
    }

    #[test]
    fn settings_draft_round_trips_denoise_suppression() {
        let config = AudioConfig {
            denoise_suppression: 2.0,
            denoise_release: 0.3,
            ..AudioConfig::default()
        };

        let draft = SettingsDraft::from_audio(&config);
        assert_eq!(DENOISE_SUPPRESSION_LABELS[draft.suppression_index], "2x");
        assert_eq!(DENOISE_RELEASE_LABELS[draft.release_index], "strong");

        let audio = draft.to_audio();
        assert_eq!(audio.denoise_suppression, 2.0);
        assert_eq!(audio.denoise_release, 0.3);
    }

    #[test]
    fn settings_draft_round_trips_typing_suppression() {
        let config = AudioConfig {
            denoise_typing_suppression: true,
            denoise_typing_vad_enter: 0.75,
            denoise_typing_vad_release: 0.85,
            ..AudioConfig::default()
        };

        let mut draft = SettingsDraft::from_audio(&config);
        assert!(draft.typing_suppression);
        assert_eq!(
            DENOISE_TYPING_VAD_LABELS[draft.typing_vad_enter_index],
            "75%"
        );
        assert_eq!(
            DENOISE_TYPING_VAD_LABELS[draft.typing_vad_release_index],
            "85%"
        );
        assert!(draft.to_audio().denoise_typing_suppression);
        assert_eq!(draft.to_audio().denoise_typing_vad_enter, 0.75);
        assert_eq!(draft.to_audio().denoise_typing_vad_release, 0.85);

        draft.typing_suppression = false;
        assert!(!draft.to_audio().denoise_typing_suppression);
    }

    #[test]
    fn default_denoise_tuning_is_stock() {
        let draft = SettingsDraft::from_audio(&AudioConfig::default());
        assert_eq!(draft.suppression_strength(), 1.0);
        assert_eq!(draft.release_value(), 1.0);
        assert_eq!(DENOISE_SUPPRESSION_LABELS[draft.suppression_index], "off");
        assert_eq!(DENOISE_RELEASE_LABELS[draft.release_index], "off");
        assert!(!draft.typing_suppression);
        assert_eq!(
            DENOISE_TYPING_VAD_LABELS[draft.typing_vad_enter_index],
            "80%"
        );
        assert_eq!(
            DENOISE_TYPING_VAD_LABELS[draft.typing_vad_release_index],
            "82%"
        );
    }

    #[test]
    fn audio_device_items_use_backend_id_with_legacy_name_alias() {
        let items = audio_output_items(&[device_with_id(
            Some("alsa:plughw:CARD=2,DEV=0"),
            "HD-Audio Generic, ALC897 Analog",
            true,
        )]);
        let device = items
            .iter()
            .find(|item| item.name.contains("ALC897"))
            .unwrap();

        assert_eq!(
            device.selection.as_deref(),
            Some("alsa:plughw:CARD=2,DEV=0")
        );
        assert_eq!(
            device.backend_id.as_deref(),
            Some("alsa:plughw:CARD=2,DEV=0")
        );
        assert!(
            device
                .primary_metadata()
                .contains("alsa:plughw:CARD=2,DEV=0")
        );
        assert!(device.matches_selection(Some("plughw:CARD=2,DEV=0")));
        assert!(device.matches_selection(Some("alsa/plughw:CARD=2,DEV=0")));
        assert!(device.matches_selection(Some("hd-audio generic, alc897 analog")));
        assert_eq!(
            audio_device_item_index(&items, Some("hd-audio generic, alc897 analog")),
            Some(1)
        );
    }

    #[test]
    fn explicit_alsa_ids_are_not_grouped_by_display_name() {
        let items = audio_output_items(&[
            device_with_id(Some("alsa:plughw:CARD=2,DEV=0"), "ALC897 Analog", true),
            device_with_id(Some("alsa:hw:CARD=2,DEV=0"), "ALC897 Analog", true),
        ]);

        assert_eq!(
            items
                .iter()
                .filter(|item| item.name == "ALC897 Analog")
                .count(),
            2
        );
    }

    #[test]
    fn ranks_microphones_above_monitor_sources() {
        let items = audio_input_items(&[
            device("Monitor of Built-in Audio", true),
            device("USB Microphone", true),
        ]);
        let monitor = items
            .iter()
            .find(|item| item.name.contains("Monitor"))
            .unwrap();
        let mic = items
            .iter()
            .find(|item| item.name.contains("Microphone"))
            .unwrap();

        assert!(mic.rank > monitor.rank);
    }

    #[test]
    fn ranks_speakers_above_microphones_for_output() {
        let items =
            audio_output_items(&[device("USB Microphone", true), device("USB Speakers", true)]);
        let mic = items
            .iter()
            .find(|item| item.name.contains("Microphone"))
            .unwrap();
        let speakers = items
            .iter()
            .find(|item| item.name.contains("Speakers"))
            .unwrap();

        assert!(speakers.rank > mic.rank);
    }

    #[test]
    fn unsupported_devices_rank_last() {
        let items = audio_input_items(&[
            device("USB Microphone", false),
            device("Analog Input", true),
        ]);
        let unsupported = items
            .iter()
            .find(|item| item.name.contains("Microphone"))
            .unwrap();
        let supported = items
            .iter()
            .find(|item| item.name.contains("Analog"))
            .unwrap();

        assert!(supported.rank > unsupported.rank);
    }

    #[test]
    fn groups_duplicate_cpal_device_variants() {
        let items = audio_input_items(&[
            device("UMC204HD 192k, USB Audio", true),
            device("UMC204HD 192k", false),
            device("USB Condenser Microphone, USB Audio", false),
            device("USB Condenser Microphone, USB Audio", false),
            device("USB Condenser Microphone", false),
            device("Loopback, Loopback PCM", true),
            device("Loopback", false),
        ]);

        let umc = items
            .iter()
            .find(|item| item.name == "UMC204HD 192k, USB Audio")
            .unwrap();
        let condenser = items
            .iter()
            .find(|item| item.name == "USB Condenser Microphone, USB Audio")
            .unwrap();
        let loopback = items
            .iter()
            .find(|item| item.name == "Loopback, Loopback PCM")
            .unwrap();

        assert_eq!(umc.variants.len(), 2);
        assert_eq!(condenser.variants.len(), 3);
        assert_eq!(loopback.variants.len(), 2);
        assert_eq!(umc.selection.as_deref(), Some("umc204hd 192k"));
        assert_eq!(
            condenser.selection.as_deref(),
            Some("usb condenser microphone")
        );
        assert_eq!(loopback.selection.as_deref(), Some("loopback"));
        assert_eq!(loopback.backend_id.as_deref(), None);
    }

    #[test]
    fn real_machine_noise_sources_rank_below_audio_interfaces() {
        let items = audio_input_items(&[
            device(
                "Discard all samples (playback) or generate zero samples (capture)",
                true,
            ),
            device("Rate Converter Plugin Using Libav/FFmpeg Library", true),
            device("PipeWire Sound Server", true),
            device(
                "Default ALSA Output (currently PipeWire Media Server)",
                true,
            ),
            device("Loopback, Loopback PCM", true),
            device("HD-Audio Generic, ALC1220 Analog", true),
            device("UMC204HD 192k, USB Audio", true),
        ]);
        let rank = |name: &str| {
            items
                .iter()
                .find(|item| item.name == name)
                .map(|item| item.rank)
                .unwrap()
        };

        assert!(rank("UMC204HD 192k, USB Audio") > rank("HD-Audio Generic, ALC1220 Analog"));
        assert!(
            rank("HD-Audio Generic, ALC1220 Analog")
                > rank("Rate Converter Plugin Using Libav/FFmpeg Library")
        );
        assert!(rank("HD-Audio Generic, ALC1220 Analog") > rank("PipeWire Sound Server"));
        assert!(
            rank("HD-Audio Generic, ALC1220 Analog")
                > rank("Default ALSA Output (currently PipeWire Media Server)")
        );
        assert!(rank("HD-Audio Generic, ALC1220 Analog") > rank("Loopback, Loopback PCM"));
        assert!(
            rank("Loopback, Loopback PCM")
                > rank("Discard all samples (playback) or generate zero samples (capture)")
        );
    }
}
