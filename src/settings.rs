use crate::{
    audio::{BufferRequest, DeviceInfo, StreamPreview},
    config::{
        AudioConfig, AudioLatencyConfig, BufferSize, DEFAULT_INPUT_BUFFER_SAMPLES,
        DEFAULT_MAX_AMPLIFICATION, DEFAULT_OUTPUT_BUFFER_SAMPLES,
    },
    tui::editor::FormEditor,
    ui::select::{FuzzySelect, SelectableItem},
};

pub const BITRATES: [i32; 6] = [16_000, 24_000, 32_000, 48_000, 64_000, 96_000];
/// Auto-gain ceiling options, in dB. `0` disables auto gain entirely for a
/// well-levelled rig; higher values let AGC2 lift a quieter mic further.
pub const MAX_AMPLIFICATIONS: [f32; 6] = [0.0, 6.0, 12.0, 18.0, 24.0, 30.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsFocus {
    InputDevice,
    OutputDevice,
    Bitrate,
    Denoise,
    EchoCancellation,
    Amplification,
    InputBuffer,
    OutputBuffer,
    Refresh,
    Save,
    Close,
}

impl SettingsFocus {
    pub const ORDER: [SettingsFocus; 11] = [
        SettingsFocus::InputDevice,
        SettingsFocus::OutputDevice,
        SettingsFocus::Bitrate,
        SettingsFocus::Denoise,
        SettingsFocus::EchoCancellation,
        SettingsFocus::Amplification,
        SettingsFocus::InputBuffer,
        SettingsFocus::OutputBuffer,
        SettingsFocus::Refresh,
        SettingsFocus::Save,
        SettingsFocus::Close,
    ];

    pub fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0)
    }
}

pub struct SettingsDraft {
    pub input_device_id: Option<String>,
    pub output_device_id: Option<String>,
    pub bitrate_index: usize,
    pub amplification_index: usize,
    /// Single-line field values holding a sample count or `"default"` (see
    /// [`parse_buffer_size`]). The active field is edited through `editor`.
    pub input_buffer: String,
    pub output_buffer: String,
    pub editor: FormEditor<SettingsFocus>,
    pub denoise: bool,
    pub echo_cancellation: bool,
    pub latency: AudioLatencyConfig,
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
            input_buffer: buffer_size_text(config.input_buffer),
            output_buffer: buffer_size_text(config.output_buffer),
            editor: FormEditor::new(),
            denoise: config.denoise,
            echo_cancellation: config.echo_cancellation,
            latency: config.latency.clone(),
        }
    }

    pub fn to_audio(&self) -> AudioConfig {
        AudioConfig {
            input_device_id: self.input_device_id.clone(),
            output_device_id: self.output_device_id.clone(),
            bitrate_bps: BITRATES[self.bitrate_index],
            denoise: self.denoise,
            echo_cancellation: self.echo_cancellation,
            max_amplification: self.max_amplification(),
            input_buffer: parse_buffer_size(&self.buffer_text(SettingsFocus::InputBuffer)),
            output_buffer: parse_buffer_size(&self.buffer_text(SettingsFocus::OutputBuffer)),
            latency: self.latency.clone(),
        }
    }

    pub fn bitrate_bps(&self) -> i32 {
        BITRATES[self.bitrate_index]
    }

    pub fn input_buffer_request(&self) -> BufferRequest {
        parse_buffer_size(&self.buffer_text(SettingsFocus::InputBuffer))
            .to_request(DEFAULT_INPUT_BUFFER_SAMPLES)
    }

    pub fn output_buffer_request(&self) -> BufferRequest {
        parse_buffer_size(&self.buffer_text(SettingsFocus::OutputBuffer))
            .to_request(DEFAULT_OUTPUT_BUFFER_SAMPLES)
    }

    /// Focuses the shared editor for `focus`, if it is one of the buffer rows.
    pub fn buffer_editor_mut(&mut self, focus: SettingsFocus) -> Option<&mut extui_editor::Editor> {
        self.focus_buffer_editor(focus)?;
        Some(self.editor.editor_mut())
    }

    pub fn focus_buffer_editor(&mut self, focus: SettingsFocus) -> Option<()> {
        match focus {
            SettingsFocus::InputBuffer | SettingsFocus::OutputBuffer => {
                let value = self.buffer_value(focus).to_string();
                if let Some((field, text)) = self.editor.focus(focus, &value) {
                    self.set_buffer_value(field, text);
                }
                Some(())
            }
            _ => {
                self.commit_buffer_editor();
                None
            }
        }
    }

    pub fn commit_buffer_editor(&mut self) {
        if let Some((field, text)) = self.editor.clear_focus() {
            self.set_buffer_value(field, text);
        }
    }

    pub fn buffer_text(&self, focus: SettingsFocus) -> String {
        if self.editor.active() == Some(focus) {
            self.editor.text()
        } else {
            self.buffer_value(focus).to_string()
        }
    }

    fn buffer_value(&self, focus: SettingsFocus) -> &str {
        match focus {
            SettingsFocus::InputBuffer => &self.input_buffer,
            SettingsFocus::OutputBuffer => &self.output_buffer,
            _ => "",
        }
    }

    fn set_buffer_value(&mut self, focus: SettingsFocus, text: String) {
        match focus {
            SettingsFocus::InputBuffer => self.input_buffer = text,
            SettingsFocus::OutputBuffer => self.output_buffer = text,
            _ => {}
        }
    }

    pub fn max_amplification(&self) -> f32 {
        MAX_AMPLIFICATIONS[self.amplification_index]
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
