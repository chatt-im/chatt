use crate::{
    audio::{BufferRequest, DenoiseConfig, DeviceInfo, DredConfig, StreamPreview},
    config::{
        AudioConfig, AudioLatencyConfig, BufferSize, DEFAULT_DENOISE_RELEASE,
        DEFAULT_DENOISE_SUPPRESSION, DEFAULT_DENOISE_TYPING_VAD_ENTER,
        DEFAULT_DENOISE_TYPING_VAD_RELEASE, DEFAULT_INPUT_BUFFER_SAMPLES,
        DEFAULT_MAX_AMPLIFICATION, DEFAULT_OUTPUT_BUFFER_SAMPLES, FormBindings, NotificationConfig,
        ThemeChoice, WebConfig,
    },
    ui::select::{FuzzySelect, SelectableItem},
};

pub const BITRATES: [i32; 6] = [16_000, 24_000, 32_000, 48_000, 64_000, 96_000];
/// Auto-gain ceiling options, in dB. `0` disables auto gain entirely for a
/// well-levelled rig; higher values let AGC2 lift a quieter mic further.
pub const MAX_AMPLIFICATIONS: [f32; 6] = [0.0, 6.0, 12.0, 18.0, 24.0, 30.0];
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
pub const NOTIFICATION_VOLUMES_DB: [f32; 11] = [
    -24.0, -18.0, -12.0, -9.0, -6.0, -3.0, 0.0, 3.0, 6.0, 9.0, 12.0,
];

/// Smallest accepted explicit buffer in samples (~0.7 ms at 48 kHz).
pub const MIN_BUFFER_SAMPLES: u32 = 32;
/// Largest accepted explicit buffer in samples (~170 ms at 48 kHz).
pub const MAX_BUFFER_SAMPLES: u32 = 8192;

/// Mutable settings state. Fields are crate-visible so the immediate-mode
/// settings form mutates them in place through `&mut`, replacing the previous
/// focus-enum dispatch. The `*_index` fields select into the option tables
/// ([`BITRATES`], [`MAX_AMPLIFICATIONS`], and the rest).
pub struct SettingsDraft {
    pub(crate) input_device_id: Option<String>,
    pub(crate) output_device_id: Option<String>,
    pub(crate) bitrate_index: usize,
    pub(crate) amplification_index: usize,
    pub(crate) suppression_index: usize,
    pub(crate) release_index: usize,
    pub(crate) typing_suppression: bool,
    pub(crate) typing_vad_enter_index: usize,
    pub(crate) typing_vad_release_index: usize,
    /// Single-line field values holding a sample count or `"default"` (see
    /// [`parse_buffer_size`]). The shared form editor commits into these.
    pub(crate) input_buffer: String,
    pub(crate) output_buffer: String,
    /// When set, the device row is a free-form text field for a raw ALSA pcm
    /// string instead of the enumerated-device picker.
    pub(crate) input_raw: bool,
    pub(crate) output_raw: bool,
    pub(crate) web_enabled: bool,
    pub(crate) web_bind: String,
    /// Preserved across the settings round-trip. The compose box is a dev-only
    /// flag with no settings-UI control, so it is carried but never edited here.
    pub(crate) web_readonly: bool,
    pub(crate) message_notification_volume_index: usize,
    pub(crate) peer_join_notification_volume_index: usize,
    pub(crate) peer_leave_notification_volume_index: usize,
    pub(crate) form_bindings: FormBindings,
    pub(crate) theme: ThemeChoice,
    pub(crate) denoise: DenoiseConfig,
    pub(crate) dred: DredConfig,
    pub(crate) echo_cancellation: bool,
    pub(crate) latency: AudioLatencyConfig,
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
            amplification_index: amplification_index(config.max_amplification),
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
            web_readonly: WebConfig::default().readonly,
            message_notification_volume_index: notification_volume_index(0.0),
            peer_join_notification_volume_index: notification_volume_index(0.0),
            peer_leave_notification_volume_index: notification_volume_index(0.0),
            form_bindings: FormBindings::Standard,
            theme: ThemeChoice::default(),
            denoise: config.denoise,
            dred: config.dred,
            echo_cancellation: config.echo_cancellation,
            latency: config.latency.clone(),
        }
    }

    pub fn set_web_from_config(&mut self, web: &WebConfig) {
        self.web_enabled = web.enabled;
        self.web_bind = web.bind.clone();
        self.web_readonly = web.readonly;
    }

    pub fn set_notifications_from_config(&mut self, notifications: &NotificationConfig) {
        self.message_notification_volume_index =
            notification_volume_index(notifications.message_volume_db);
        self.peer_join_notification_volume_index =
            notification_volume_index(notifications.peer_join_volume_db);
        self.peer_leave_notification_volume_index =
            notification_volume_index(notifications.peer_leave_volume_db);
    }

    pub fn set_form_bindings_from_config(&mut self, form_bindings: FormBindings) {
        self.form_bindings = form_bindings;
    }

    pub fn set_theme_from_config(&mut self, theme: ThemeChoice) {
        self.theme = theme;
    }

    pub fn theme(&self) -> ThemeChoice {
        self.theme
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
            latency: self.latency.clone(),
        }
    }

    pub fn to_web(&self) -> WebConfig {
        WebConfig {
            enabled: self.web_enabled,
            bind: self.web_bind.trim().to_string(),
            readonly: self.web_readonly,
        }
    }

    pub fn to_notifications(&self) -> NotificationConfig {
        NotificationConfig {
            message_volume_db: self.message_notification_volume_db(),
            peer_join_volume_db: self.peer_join_notification_volume_db(),
            peer_leave_volume_db: self.peer_leave_notification_volume_db(),
        }
    }

    pub fn form_bindings(&self) -> FormBindings {
        self.form_bindings
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
            .or_else(|| web_bind_error(&self.web_bind))
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
        parse_buffer_size(&self.input_buffer).to_request(DEFAULT_INPUT_BUFFER_SAMPLES)
    }

    pub fn output_buffer_request(&self) -> BufferRequest {
        parse_buffer_size(&self.output_buffer).to_request(DEFAULT_OUTPUT_BUFFER_SAMPLES)
    }

    pub fn max_amplification(&self) -> f32 {
        MAX_AMPLIFICATIONS[self.amplification_index]
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
        NOTIFICATION_VOLUMES_DB[self.message_notification_volume_index]
    }

    pub fn peer_join_notification_volume_db(&self) -> f32 {
        NOTIFICATION_VOLUMES_DB[self.peer_join_notification_volume_index]
    }

    pub fn peer_leave_notification_volume_db(&self) -> f32 {
        NOTIFICATION_VOLUMES_DB[self.peer_leave_notification_volume_index]
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

fn amplification_index(value: f32) -> usize {
    let value = if value.is_finite() {
        value
    } else {
        DEFAULT_MAX_AMPLIFICATION
    };
    MAX_AMPLIFICATIONS
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            (*left - value)
                .abs()
                .partial_cmp(&(*right - value).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(index, _)| index)
        .unwrap_or(1)
}

fn notification_volume_index(value: f32) -> usize {
    nearest_index(&NOTIFICATION_VOLUMES_DB, value, 0.0)
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
            max_amplification: 30.0,
            ..AudioConfig::default()
        };

        let draft = SettingsDraft::from_audio(&config);
        let audio = draft.to_audio();

        assert_eq!(audio.input_device_id.as_deref(), Some("usb mic"));
        assert_eq!(audio.output_device_id.as_deref(), Some("usb speakers"));
        assert!(audio.echo_cancellation);
        assert_eq!(audio.max_amplification, 30.0);
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
