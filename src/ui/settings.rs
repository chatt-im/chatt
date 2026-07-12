use crate::audio::{DenoiseConfig, DredConfig};
use crate::config::{
    AudioConfig, AudioLatencyConfig, CandidatePrivacy, DEFAULT_MAX_AMPLIFICATION, FileConfig,
    FormBindings, MAX_NOTIFICATION_VOLUME_DB, MIN_NOTIFICATION_VOLUME_DB, NotificationSoundMode,
    ThemeSelection, UiConfig, WebAutoplay, WebConfig, WebViewer, default_url_open,
};
use crate::{
    audio::{
        CAPTURE_LONG_SILENCE_STOP_MS_RANGE, CAPTURE_SILENCE_PREROLL_MS_RANGE,
        CAPTURE_SILENCE_RAMP_MS_RANGE, DEVICE_PERIOD_MARGIN_MS_RANGE, HARD_QUEUE_BOUND_MS_RANGE,
        INITIAL_BUFFER_MS_RANGE, MAX_REORDER_DELAY_MS_RANGE, NETEQ_BASE_MINIMUM_DELAY_MS_RANGE,
        NETEQ_MAX_DELAY_MS_RANGE, NETEQ_MIN_DELAY_MS_RANGE, NETEQ_START_DELAY_MS_RANGE,
        StatsSnapshot,
    },
    bindings::{self, BindCommand, BindingRuntime},
    settings::{
        AudioDeviceItem, AudioDevicePickerState, AudioInputItem, AudioInputPickerState,
        AudioOutputItem, AudioOutputPickerState, BITRATES, DENOISE_RELEASE_LABELS,
        DENOISE_RELEASES, DENOISE_SUPPRESSION_LABELS, DENOISE_SUPPRESSIONS,
        DENOISE_TYPING_VAD_LABELS, DENOISE_TYPING_VAD_THRESHOLDS, MAX_AMPLIFICATION_DB_RANGE,
        SettingsDraft, UI_MAX_COMPOSER_HEIGHT_RANGE, UI_MAX_MESSAGES_RANGE, UI_OVERSCAN_RANGE,
        UI_ROOM_HEIGHT_RANGE, buffer_field_error, byte_size_error, download_path_error,
        form_bindings_label, int_range_error, latency_ms_error, max_amplification_error,
        notification_volume_error, output_volume_field_error, parse_db_value, positive_mib_error,
        raw_device_error, raw_device_selection, selected_audio_input_label,
        selected_audio_output_label, vad_level_error, volume_db_label, web_bind_error,
        web_origin_error,
    },
    theme::Theme,
    tui::{
        form::{FormFieldKind, FormState},
        widgets,
    },
    ui::{
        form::{ActionButton, Form as CoreForm},
        vu,
    },
};
use extui::{Buffer, Ellipsis, HAlign, Rect, vt::Modifier};

pub(crate) use crate::ui::form::{FieldId, FieldIntent};

const LABEL_WIDTH: u16 = 20;
const DETAIL_WIDTH: u16 = 34;
const MIN_DETAIL_SCREEN_WIDTH: u16 = 92;
const MIN_PICKER_ROWS: u16 = 3;

const CAPTURE_SECTION: &str = "Capture Settings";
const PLAYBACK_SECTION: &str = "Playback Settings";

/// Id of the capture device row, used by the app layer to route the open
/// device picker's key, mouse, and scroll input.
pub(crate) fn capture_device_id() -> FieldId {
    FieldId::new(CAPTURE_SECTION, "Device")
}

/// Id of the playback device row.
pub(crate) fn playback_device_id() -> FieldId {
    FieldId::new(PLAYBACK_SECTION, "Device")
}

/// Initial focus when a settings session opens.
pub(crate) fn initial_focus() -> FieldId {
    capture_device_id()
}

/// Whether `field` is one of the two trailing list-entry rows. These rows are
/// committed on the first text change so the next blank row can appear while
/// the user keeps typing in the same editor.
pub(crate) fn is_list_add_field(draft: &SettingsDraft, field: FieldId) -> bool {
    field == FieldId::new("Links", &format!("Open Arg {}", draft.url_open.len() + 1))
        || field
            == FieldId::new(
                "Advanced",
                &format!("Origin {}", draft.web_allowed_origins.len() + 1),
            )
}

/// Builds the id for an arbitrary section/label pair. Used by tests that drive
/// the form without running the full declaration pass.
#[cfg(test)]
pub(crate) fn field_id_for(section: &str, label: &str) -> FieldId {
    FieldId::new(section, label)
}

/// The four settings tabs. Every row lives on exactly one tab; both the draw
/// and logic passes gate on the session's active tab so field registration
/// stays identical between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsTab {
    Audio,
    Interface,
    Data,
    Extra,
}

impl SettingsTab {
    pub(crate) const ALL: [SettingsTab; 4] = [
        SettingsTab::Audio,
        SettingsTab::Interface,
        SettingsTab::Data,
        SettingsTab::Extra,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            SettingsTab::Audio => "Audio",
            SettingsTab::Interface => "Interface",
            SettingsTab::Data => "Data",
            SettingsTab::Extra => "Extra",
        }
    }

    pub(crate) fn cycle(self, delta: isize) -> SettingsTab {
        let index = Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .unwrap_or_default();
        let len = Self::ALL.len() as isize;
        let index = (index as isize + delta).rem_euclid(len) as usize;
        Self::ALL[index]
    }
}

/// Draws the tab bar as flat colored segments and returns each segment's rect
/// for mouse hit testing.
pub(crate) fn draw_settings_tabs(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    active: SettingsTab,
) -> [Rect; 4] {
    area.with(theme.background).fill(buf);
    let mut rects = [Rect::EMPTY; 4];
    let mut row = area;
    for (index, tab) in SettingsTab::ALL.into_iter().enumerate() {
        let label = tab.label();
        let segment = row.take_left(label.len() as i32 + 2);
        let style = if tab == active {
            theme.mode_settings
        } else {
            theme.status_section_inactive
        };
        buf.clear_rect(segment, style);
        segment.with(style).with(HAlign::Center).text(buf, label);
        rects[index] = segment;
        row.take_left(1);
    }
    rects
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceSide {
    Input,
    Output,
}

/// Picker request emitted by a focused device row during the logic pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceAction {
    /// Open the picker, or confirm the highlighted item when already open.
    Activate(DeviceSide),
    /// Cancel the open picker, restoring the prior selection.
    Cancel(DeviceSide),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsButton {
    Refresh,
    Save,
    Close,
}

/// What the focused field did this logic pass, applied by the app layer.
#[derive(Default)]
pub(crate) struct SettingsOutput {
    /// A draft value changed and the live config should resync.
    pub(crate) changed: bool,
    pub(crate) button: Option<SettingsButton>,
    pub(crate) device: Option<DeviceAction>,
}

/// Detail captured for the focused field, rendered into the side panel.
enum FocusDetail {
    Option {
        current: String,
        default: Option<String>,
        error: Option<String>,
        help: &'static str,
    },
    Device(DeviceSide),
}

struct SettingsForm<'a> {
    form: CoreForm<'a>,
    action_labels: &'a SettingsActionLabels,
    detail: Option<FocusDetail>,
    output: SettingsOutput,
}

struct SettingsActionLabels {
    refresh: String,
    save: String,
    close: String,
}

impl SettingsActionLabels {
    fn new(bindings: &BindingRuntime, dirty: bool) -> Self {
        Self {
            refresh: action_label(bindings, "Refresh devices", BindCommand::RefreshDevices),
            save: action_label(
                bindings,
                if dirty {
                    "Save config *"
                } else {
                    "Save config"
                },
                BindCommand::SaveSettings,
            ),
            close: action_label(bindings, "Back to chat", BindCommand::CloseSettings),
        }
    }
}

fn action_label(bindings: &BindingRuntime, label: &str, command: BindCommand) -> String {
    widgets::button_label(
        label,
        bindings::command_key_hint(bindings, bindings::SETTINGS_LAYER, command),
    )
}

impl<'a> SettingsForm<'a> {
    fn new(
        state: &'a mut FormState<FieldId>,
        buf: Option<&'a mut Buffer>,
        theme: &'a Theme,
        action_labels: &'a SettingsActionLabels,
        dirty: bool,
        intent: FieldIntent,
        commit: Option<(FieldId, String)>,
        focus_column: Option<u16>,
    ) -> Self {
        Self {
            form: CoreForm::new(state, buf, theme, dirty, intent, commit, focus_column)
                .with_label_width(LABEL_WIDTH),
            action_labels,
            detail: None,
            output: SettingsOutput::default(),
        }
    }

    fn section(&mut self, title: &str) {
        self.form.section(title);
    }

    fn enabled(&mut self, reason: Option<&'static str>, body: impl FnOnce(&mut Self)) {
        let previous = self.form.set_enabled(reason.is_none());
        body(self);
        self.form.set_enabled(previous);
    }

    fn checkbox(&mut self, label: &str, value: &mut bool) -> crate::ui::form::Response {
        let response = self.form.checkbox(label, value);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(if *value { "on" } else { "off" }.to_string(), None);
        }
        response
    }

    fn choice(
        &mut self,
        label: &str,
        index: &mut usize,
        len: usize,
        fmt: impl Fn(usize) -> String,
    ) -> crate::ui::form::Response {
        let response = self.form.choice(label, index, len, &fmt);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(fmt(*index), None);
        }
        response
    }

    fn choice_value<T: Copy + PartialEq>(
        &mut self,
        label: &str,
        value: &mut T,
        options: &[T],
        fmt: impl Fn(T) -> String,
    ) -> crate::ui::form::Response {
        let response = self.form.choice_value(label, value, options, &fmt);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(fmt(*value), None);
        }
        response
    }

    fn text(
        &mut self,
        label: &str,
        value: &mut String,
        validate: impl Fn(&str) -> Option<String>,
    ) -> crate::ui::form::Response {
        let response = self.form.text(label, value, &validate);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(value.clone(), validate(value));
        }
        response
    }

    fn adjustable_text(
        &mut self,
        label: &str,
        value: &mut String,
        validate: impl Fn(&str) -> Option<String>,
        adjust: impl Fn(&str, isize) -> String,
    ) -> crate::ui::form::Response {
        let response = self.form.adjustable_text(label, value, &validate, adjust);
        if response.changed() {
            self.output.changed = true;
        }
        if response.is_focus() {
            self.set_option_detail(value.clone(), validate(value));
        }
        response
    }

    /// Records the help text for the focused field. Call from the `is_focus`
    /// guard of a widget. The widget already filled the current value and any
    /// validation error.
    pub(crate) fn set_help(&mut self, help: &'static str) {
        if let Some(FocusDetail::Option { help: slot, .. }) = &mut self.detail {
            *slot = help;
        }
    }

    /// Records the default value shown under `Current` in the detail panel.
    /// Call next to [`SettingsForm::set_help`] from the `is_focus` guard.
    fn set_default(&mut self, default: impl Into<String>) {
        if let Some(FocusDetail::Option { default: slot, .. }) = &mut self.detail {
            *slot = Some(default.into());
        }
    }

    /// A checkbox for session-only state: identical to
    /// [`SettingsForm::checkbox`] but never marks the draft changed, so
    /// toggling it cannot dirty the config or trigger a live resync.
    fn transient_checkbox(&mut self, label: &str, value: &mut bool) -> crate::ui::form::Response {
        let response = self.form.checkbox(label, value);
        if response.is_focus() {
            self.set_option_detail(if *value { "on" } else { "off" }.to_string(), None);
        }
        response
    }

    /// A one-item-per-row list field with one trailing blank numbered row.
    /// Once that row receives text it is promoted into `items`; its stable
    /// numbered id keeps the editor on it while the next blank row appears.
    fn string_list(
        &mut self,
        label: &str,
        items: &mut Vec<String>,
        scratch: &mut String,
        validate: impl Fn(&str) -> Option<String>,
    ) -> crate::ui::form::Response {
        let mut focused = false;
        let mut changed = false;
        let mut index = 0;
        while index < items.len() {
            let row_label = format!("{label} {}", index + 1);
            let response = self.text(&row_label, &mut items[index], &validate);
            focused |= response.is_focus();
            if response.changed() && items[index].trim().is_empty() {
                items.remove(index);
                changed = true;
                continue;
            }
            changed |= response.changed();
            index += 1;
        }
        let add_label = format!("{label} {}", items.len() + 1);
        let response =
            self.form
                .text_with_placeholder(&add_label, scratch, Some("add\u{2026}"), &validate);
        focused |= response.is_focus();
        if response.is_focus() {
            self.set_option_detail(scratch.clone(), validate(scratch));
        }
        if response.changed() && !scratch.trim().is_empty() {
            items.push(scratch.trim().to_string());
            scratch.clear();
            changed = true;
        }
        if changed {
            self.output.changed = true;
        }
        self.form.respond(focused, changed)
    }

    /// A device picker row: an enumerated [`FormFieldKind::Select`], or a raw
    /// ALSA text field while `raw` is on. Emits a [`DeviceAction`] when focused
    /// and activated, and the picker overlay when open.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn device(
        &mut self,
        side: DeviceSide,
        raw: bool,
        selection: &mut Option<String>,
        items: &[AudioDeviceItem],
        picker: &mut AudioDevicePickerState,
        selected_label: String,
    ) -> crate::ui::form::Response {
        let id = self.form.id("Device");
        let row = self.form.next_row(1);
        if raw {
            let area = self.form.register_field(row, id, FormFieldKind::Text);
            let focused = self.form.focus() == id;
            let mut changed = false;
            if let Some((commit_id, text)) = self.form.take_commit() {
                if commit_id == id {
                    let next = raw_device_selection(&text);
                    if *selection != next {
                        *selection = next;
                        changed = true;
                    }
                } else {
                    self.form.restore_commit((commit_id, text));
                }
            }
            let current = selection.clone().unwrap_or_default();
            if focused {
                self.form.seed_editor(id, &current, area);
            }
            let error = raw_device_error(&current);
            if focused {
                let shown = if current.is_empty() {
                    "system default".to_string()
                } else {
                    current.clone()
                };
                self.set_option_detail(shown, error.clone());
            }
            self.form
                .render_text_row(id, "Device", &current, None, focused, error.is_some(), area);
            if changed {
                self.output.changed = true;
            }
            return self.form.respond(focused, changed);
        }

        let area = self.form.register_field(row, id, FormFieldKind::Select);
        let focused = self.form.focus() == id;
        if focused {
            match self.form.intent() {
                FieldIntent::Activate => self.output.device = Some(DeviceAction::Activate(side)),
                FieldIntent::Adjust(delta) if delta > 0 => {
                    self.output.device = Some(DeviceAction::Activate(side))
                }
                FieldIntent::Adjust(_) => self.output.device = Some(DeviceAction::Cancel(side)),
                FieldIntent::None => {}
            }
            self.detail = Some(FocusDetail::Device(side));
        }
        let value = if picker.open && picker.searching {
            format!("/{}", picker.selector.query())
        } else {
            selected_label
        };
        if let Some(area) = area {
            self.form
                .draw_labeled_value(area, "Device", &value, focused);
        }
        if picker.open {
            self.draw_picker(id, items, picker);
        }
        self.form.respond(focused, false)
    }

    /// The trailing action-button row. Buttons share a virtual row so
    /// left/right moves between them. The device Refresh button only appears
    /// on the Audio tab.
    fn actions(&mut self, show_refresh: bool) {
        let refresh = ActionButton {
            key: "Refresh",
            label: &self.action_labels.refresh,
            value: SettingsButton::Refresh,
            help: "Re-scan audio devices using the current buffer requests.",
        };
        let close = ActionButton {
            key: "Close",
            label: &self.action_labels.close,
            value: SettingsButton::Close,
            help: "Return to chat without saving further changes.",
        };
        let save = ActionButton {
            key: "Save",
            label: &self.action_labels.save,
            value: SettingsButton::Save,
            help: "Persist the draft to chatt.toml.",
        };
        let response = if show_refresh {
            self.form.actions(&[refresh, close, save])
        } else {
            self.form.actions(&[close, save])
        };
        if let Some(button) = response.activated {
            self.output.button = Some(button);
        }
        if response.focused.is_some() {
            self.set_option_detail(String::new(), None);
            if let Some(help) = response.help {
                self.set_help(help);
            }
        }
    }

    fn set_option_detail(&mut self, current: String, error: Option<String>) {
        self.detail = Some(FocusDetail::Option {
            current,
            default: None,
            error,
            help: "",
        });
    }

    fn draw_picker(
        &mut self,
        id: FieldId,
        items: &[AudioDeviceItem],
        picker: &mut AudioDevicePickerState,
    ) {
        let rows = picker_rows(items);
        if rows == 0 {
            return;
        }
        let area_row = self.form.next_row(rows);
        let Some(area) = area_row.rect else {
            return;
        };
        let focused = self.form.focus() == id;
        self.form.with_draw(|state, buf, theme| {
            buf.clear_rect(area, theme.background);
            if picker.selector.filtered_len() == 0 {
                area.with(theme.subtle)
                    .with(HAlign::Center)
                    .text(buf, "No matching audio devices");
                return;
            }
            let item_height = if area.h < 4 { 1 } else { 2 };
            picker.selector.render(
                area,
                item_height,
                buf,
                |_, item_index, selected, area, buf| {
                    state.register_picker_item(id, area, item_index);
                    if let Some(item) = items.get(item_index) {
                        draw_audio_item(area, buf, theme, item, selected, focused);
                    }
                },
            );
        });
    }
}

/// Declares the settings form for the active tab. The single place every
/// field lives: one widget call per row carries its label, the `&mut` it
/// mutates, its input kind, and (via the `is_focus` guard) its detail help
/// and default. Both the draw and logic passes replay this with the same tab.
fn settings_ui(
    form: &mut SettingsForm,
    draft: &mut SettingsDraft,
    tab: SettingsTab,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &mut AudioOutputPickerState,
) {
    match tab {
        SettingsTab::Audio => audio_tab(
            form,
            draft,
            input_items,
            input_picker,
            output_items,
            output_picker,
        ),
        SettingsTab::Interface => interface_tab(form, draft),
        SettingsTab::Data => data_tab(form, draft),
        SettingsTab::Extra => extra_tab(form, draft),
    }
    form.form.spacer(1);
    form.actions(tab == SettingsTab::Audio);
}

/// The shared `Advanced` section opener: a session-only toggle revealing the
/// tab's low-level rows. Returns whether they should be declared.
fn advanced_section(form: &mut SettingsForm, draft: &mut SettingsDraft) -> bool {
    form.section("Advanced");
    if form
        .transient_checkbox("Show Advanced", &mut draft.show_advanced)
        .is_focus()
    {
        form.set_help(
            "Reveals this tab's low-level tuning rows. Session-only; never saved to the config.",
        );
        form.set_default("off");
    }
    draft.show_advanced
}

fn audio_tab(
    form: &mut SettingsForm,
    draft: &mut SettingsDraft,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &mut AudioOutputPickerState,
) {
    form.section(CAPTURE_SECTION);
    let input_label = selected_audio_input_label(input_items, draft.input_device_id.as_deref());
    form.device(
        DeviceSide::Input,
        draft.input_raw,
        &mut draft.input_device_id,
        input_items,
        input_picker,
        input_label,
    );
    if form.checkbox("Raw Device", &mut draft.input_raw).is_focus() {
        form.set_help(
            "Type a raw ALSA device string (e.g. hw:0,0) instead of picking an enumerated device.",
        );
        form.set_default("off");
    }
    let audio_defaults = AudioConfig::default();
    if form
        .choice("Bitrate", &mut draft.bitrate_index, BITRATES.len(), |i| {
            format!("{} kbps", BITRATES[i] / 1000)
        })
        .is_focus()
    {
        form.set_help("Opus target bitrate for outgoing voice packets.");
        form.set_default(format!("{} kbps", audio_defaults.bitrate_bps / 1000));
    }
    if form
        .choice_value(
            "Denoise",
            &mut draft.denoise,
            &DenoiseConfig::ALL,
            |denoise| denoise.label().to_string(),
        )
        .is_focus()
    {
        form.set_help("Noise suppression before encoding. Useful for fans and room noise.");
        form.set_default(audio_defaults.denoise.label());
    }
    if form
        .choice_value("DRED", &mut draft.dred, &DredConfig::ALL, |dred| {
            dred.label().to_string()
        })
        .is_focus()
    {
        form.set_help(
            "Embed Opus DRED redundancy in outgoing voice so peers recover lost packets. Auto matches on for now.",
        );
        form.set_default(audio_defaults.dred.label());
    }
    if form
        .checkbox("Echo Cancel", &mut draft.echo_cancellation)
        .is_focus()
    {
        form.set_help("Cancels speaker echo from the microphone path when supported.");
        form.set_default("off");
    }
    if form
        .adjustable_text(
            "Max Gain",
            &mut draft.max_amplification,
            max_amplification_error,
            |value, delta| {
                adjust_db(
                    value,
                    delta,
                    MAX_AMPLIFICATION_DB_RANGE,
                    DEFAULT_MAX_AMPLIFICATION,
                )
            },
        )
        .is_focus()
    {
        form.set_help("Auto-gain ceiling for quiet microphones; 0 dB disables amplification.");
        form.set_default(format!("{:.0} dB", audio_defaults.max_amplification));
    }

    form.section(PLAYBACK_SECTION);
    let output_label = selected_audio_output_label(output_items, draft.output_device_id.as_deref());
    form.device(
        DeviceSide::Output,
        draft.output_raw,
        &mut draft.output_device_id,
        output_items,
        output_picker,
        output_label,
    );
    if form
        .checkbox("Raw Device", &mut draft.output_raw)
        .is_focus()
    {
        form.set_help(
            "Type a raw ALSA device string (e.g. hw:0,0) instead of picking an enumerated device.",
        );
        form.set_default("off");
    }
    if form
        .text(
            "Output Volume",
            &mut draft.output_volume,
            output_volume_field_error,
        )
        .is_focus()
    {
        form.set_help("Global playback volume. 100% is unchanged; the maximum is 130%.");
        form.set_default("100%");
    }
    if form.checkbox("Loopback", &mut draft.loopback).is_focus() {
        form.set_help(
            "Plays your microphone back through the output device to test your setup. Turns off automatically when settings closes; use headphones to avoid feedback.",
        );
        form.set_default("off");
    }

    form.section("Notifications");
    if form
        .choice_value(
            "Play Sounds",
            &mut draft.notification_sounds,
            &NotificationSoundMode::ALL,
            |mode| mode.label().to_string(),
        )
        .is_focus()
    {
        form.set_help(
            "When notification sounds play: never, only during a voice call, or always. Deafen always silences them.",
        );
        form.set_default(NotificationSoundMode::default().label());
    }
    if form
        .adjustable_text(
            "Message Volume",
            &mut draft.message_notification_volume,
            notification_volume_error,
            adjust_notification_db,
        )
        .is_focus()
    {
        form.set_help("Volume for incoming-message notification sounds.");
        form.set_default(volume_db_label(0.0));
    }
    if form
        .adjustable_text(
            "Join Volume",
            &mut draft.peer_join_notification_volume,
            notification_volume_error,
            adjust_notification_db,
        )
        .is_focus()
    {
        form.set_help("Volume for peer-joined notification sounds.");
        form.set_default(volume_db_label(0.0));
    }
    if form
        .adjustable_text(
            "Leave Volume",
            &mut draft.peer_leave_notification_volume,
            notification_volume_error,
            adjust_notification_db,
        )
        .is_focus()
    {
        form.set_help("Volume for peer-left notification sounds.");
        form.set_default(volume_db_label(0.0));
    }

    if !advanced_section(form, draft) {
        return;
    }
    if form
        .text(
            "Capture Buffer",
            &mut draft.input_buffer,
            buffer_field_error,
        )
        .is_focus()
    {
        form.set_help("Requested capture buffer in samples, or default for the host backend.");
        form.set_default("default");
    }
    if form
        .text(
            "Playback Buffer",
            &mut draft.output_buffer,
            buffer_field_error,
        )
        .is_focus()
    {
        form.set_help("Requested playback buffer in samples, or default for the host backend.");
        form.set_default("default");
    }

    let denoise_disabled = draft.denoise_tuning_disabled();
    form.enabled(denoise_disabled, |form| {
        if form
            .choice(
                "Suppression",
                &mut draft.suppression_index,
                DENOISE_SUPPRESSIONS.len(),
                |i| DENOISE_SUPPRESSION_LABELS[i].to_string(),
            )
            .is_focus()
        {
            form.set_help(
                "RNNoise over-suppression of residual noise (typing, fans). off keeps stock denoising.",
            );
            form.set_default(DENOISE_SUPPRESSION_LABELS[0]);
        }
        if form
            .choice("Release", &mut draft.release_index, DENOISE_RELEASES.len(), |i| {
                DENOISE_RELEASE_LABELS[i].to_string()
            })
            .is_focus()
        {
            form.set_help(
                "Smooths how fast RNNoise releases suppression, stopping noise swelling up after a pause.",
            );
            form.set_default(DENOISE_RELEASE_LABELS[0]);
        }
        if form.checkbox("Typing Gate", &mut draft.typing_suppression).is_focus() {
            form.set_help("Ducks loud low-VAD desk and keyboard thumps after RNNoise.");
            form.set_default("off");
        }
    });

    let vad_disabled = draft.typing_vad_disabled();
    form.enabled(vad_disabled, |form| {
        let enter = form.choice(
            "Gate Start",
            &mut draft.typing_vad_enter_index,
            DENOISE_TYPING_VAD_THRESHOLDS.len(),
            |i| DENOISE_TYPING_VAD_LABELS[i].to_string(),
        );
        if enter.is_focus() {
            form.set_help(
                "Typing gate engages when Earshot VAD is below this threshold and acoustic guards match.",
            );
            form.set_default("80%");
        }
        if enter.changed() && draft.typing_vad_release_index < draft.typing_vad_enter_index {
            draft.typing_vad_release_index = draft.typing_vad_enter_index;
        }
        let release = form.choice(
            "Gate Release",
            &mut draft.typing_vad_release_index,
            DENOISE_TYPING_VAD_THRESHOLDS.len(),
            |i| DENOISE_TYPING_VAD_LABELS[i].to_string(),
        );
        if release.is_focus() {
            form.set_help("Typing gate releases only after Earshot VAD reaches this threshold.");
            form.set_default("82%");
        }
        if release.changed() && draft.typing_vad_release_index < draft.typing_vad_enter_index {
            draft.typing_vad_enter_index = draft.typing_vad_release_index;
        }
    });

    form.section("Latency");
    if form
        .checkbox("Silence Gate", &mut draft.latency.capture_silence_gate)
        .is_focus()
    {
        form.set_help(
            "Stops sending packets during capture silence so the receiver can expand and re-converge to a shallower buffer.",
        );
        form.set_default("on");
    }
    if form
        .checkbox("Render Assist", &mut draft.latency.render_assist)
        .is_focus()
    {
        form.set_help(
            "Pre-renders playout blocks off the audio callback thread. Enable only on devices too slow to decode within the callback; it adds output latency, so leave it off on capable hardware.",
        );
        form.set_default("off");
    }
    let latency_defaults = AudioLatencyConfig::default();
    let cross = draft.latency_cross_error();
    let rows: [(&str, &mut String, (u64, u64), u64, &'static str); 11] = [
        (
            "Start Delay",
            &mut draft.latency_ms.neteq_start_delay_ms,
            NETEQ_START_DELAY_MS_RANGE,
            latency_defaults.neteq_start_delay_ms,
            "NetEQ target delay when a stream starts, before interarrival statistics take over.",
        ),
        (
            "Min Delay",
            &mut draft.latency_ms.neteq_min_delay_ms,
            NETEQ_MIN_DELAY_MS_RANGE,
            latency_defaults.neteq_min_delay_ms,
            "Floor for the learned NetEQ target delay.",
        ),
        (
            "Base Min Delay",
            &mut draft.latency_ms.neteq_base_minimum_delay_ms,
            NETEQ_BASE_MINIMUM_DELAY_MS_RANGE,
            latency_defaults.neteq_base_minimum_delay_ms,
            "Application-requested minimum playout delay, like WebRTC's base minimum.",
        ),
        (
            "Max Delay",
            &mut draft.latency_ms.neteq_max_delay_ms,
            NETEQ_MAX_DELAY_MS_RANGE,
            latency_defaults.neteq_max_delay_ms,
            "Ceiling for the learned NetEQ target delay.",
        ),
        (
            "Queue Bound",
            &mut draft.latency_ms.hard_queue_bound_ms,
            HARD_QUEUE_BOUND_MS_RANGE,
            latency_defaults.hard_queue_bound_ms,
            "Hard cap on buffered audio; packets beyond this are dropped to bound worst-case latency.",
        ),
        (
            "Initial Buffer",
            &mut draft.latency_ms.initial_buffer_ms,
            INITIAL_BUFFER_MS_RANGE,
            latency_defaults.initial_buffer_ms,
            "Audio buffered before playback starts on a fresh stream.",
        ),
        (
            "Reorder Delay",
            &mut draft.latency_ms.max_reorder_delay_ms,
            MAX_REORDER_DELAY_MS_RANGE,
            latency_defaults.max_reorder_delay_ms,
            "How long a gap waits for a late packet before concealment fills it.",
        ),
        (
            "Period Margin",
            &mut draft.latency_ms.device_period_margin_ms,
            DEVICE_PERIOD_MARGIN_MS_RANGE,
            latency_defaults.device_period_margin_ms,
            "Safety margin added on top of the device callback period.",
        ),
        (
            "Silence Stop",
            &mut draft.latency_ms.capture_long_silence_stop_ms,
            CAPTURE_LONG_SILENCE_STOP_MS_RANGE,
            latency_defaults.capture_long_silence_stop_ms,
            "Continuous capture silence before the sender stops emitting packets entirely.",
        ),
        (
            "Silence Preroll",
            &mut draft.latency_ms.capture_silence_preroll_ms,
            CAPTURE_SILENCE_PREROLL_MS_RANGE,
            latency_defaults.capture_silence_preroll_ms,
            "Audio replayed from before speech resumed so onsets are not clipped.",
        ),
        (
            "Silence Ramp",
            &mut draft.latency_ms.capture_silence_ramp_ms,
            CAPTURE_SILENCE_RAMP_MS_RANGE,
            latency_defaults.capture_silence_ramp_ms,
            "Fade applied when the silence gate opens or closes to avoid clicks.",
        ),
    ];
    for (label, value, range, default_ms, help) in rows {
        if form
            .text(label, value, |text| {
                latency_ms_error(text, range).or_else(|| cross.clone())
            })
            .is_focus()
        {
            form.set_help(help);
            form.set_default(format!("{default_ms} ms"));
        }
    }
    if form
        .text(
            "Silence VAD Max",
            &mut draft.latency_ms.silence_vad_max,
            |text| vad_level_error(text).or_else(|| cross.clone()),
        )
        .is_focus()
    {
        form.set_help(
            "Highest Earshot VAD level (0-255) still treated as capture silence by the silence gate.",
        );
        form.set_default(latency_defaults.silence_vad_max.to_string());
    }
}

fn interface_tab(form: &mut SettingsForm, draft: &mut SettingsDraft) {
    form.section("Interface Settings");
    if form
        .choice_value(
            "Default Bindings",
            &mut draft.form_bindings,
            &[FormBindings::Standard, FormBindings::Vim],
            |bindings| form_bindings_label(bindings).to_string(),
        )
        .is_focus()
    {
        form.set_help("Keyboard model used by editable controls throughout the interface.");
        form.set_default(form_bindings_label(FormBindings::Standard));
    }
    let themes = ThemeSelection::cycle_list(&draft.theme_names);
    let mut theme_index = themes
        .iter()
        .position(|selection| *selection == draft.theme)
        .unwrap_or(0);
    let theme_response = form.choice("Theme", &mut theme_index, themes.len(), |index| {
        themes[index].label()
    });
    draft.theme = themes[theme_index].clone();
    if theme_response.is_focus() {
        form.set_help("Color theme for the interface. Applies immediately; Save persists it.");
        form.set_default(ThemeSelection::default().label());
    }
    let ui_defaults = UiConfig::default();
    if form
        .text("Room Height", &mut draft.ui_room_height, |text| {
            int_range_error(text, UI_ROOM_HEIGHT_RANGE)
        })
        .is_focus()
    {
        form.set_help("Rows each room occupies in the room list.");
        form.set_default(ui_defaults.room_height.to_string());
    }
    if form
        .text(
            "Composer Height",
            &mut draft.ui_max_composer_height,
            |text| int_range_error(text, UI_MAX_COMPOSER_HEIGHT_RANGE),
        )
        .is_focus()
    {
        form.set_help("Maximum rows the message composer grows to while typing.");
        form.set_default(ui_defaults.max_composer_height.to_string());
    }
    if form
        .checkbox("Composer Padding", &mut draft.ui_composer_padding)
        .is_focus()
    {
        form.set_help("Draws a padded frame around the message composer.");
        form.set_default("on");
    }

    form.section("Links");
    if form
        .string_list(
            "Open Arg",
            &mut draft.url_open,
            &mut draft.url_open_new,
            |_| None,
        )
        .is_focus()
    {
        form.set_help(
            "Command that opens a clicked URL, one argument per row; the URL is appended last. Empty rows are removed; an empty list uses the platform opener.",
        );
        form.set_default(default_url_open_label());
    }

    form.section("Web Log Server");
    if form.checkbox("Web Log", &mut draft.web_enabled).is_focus() {
        form.set_help("Starts the browser chat-log server. Bind is saved even while disabled.");
        form.set_default("off");
    }
    if form
        .text("Web Bind", &mut draft.web_bind, web_bind_error)
        .is_focus()
    {
        form.set_help(
            "Loopback socket address for the browser chat-log server, for example 127.0.0.1:8080.",
        );
        form.set_default(WebConfig::default().bind);
    }
    if form
        .choice_value("Viewer", &mut draft.web_viewer, &WebViewer::ALL, |viewer| {
            viewer.label().to_string()
        })
        .is_focus()
    {
        form.set_help(
            "Where the browser opens a clicked file preview: the in-page side panel or its own browser tab. Applies live to connected browsers.",
        );
        form.set_default(WebViewer::Panel.label());
    }
    if form
        .choice_value(
            "Autoplay",
            &mut draft.web_autoplay,
            &WebAutoplay::ALL,
            |autoplay| autoplay.label().to_string(),
        )
        .is_focus()
    {
        form.set_help(
            "Automatically play newly received videos in the browser. Muted playback starts without interaction; with audio needs the browser's autoplay policy to allow it.",
        );
        form.set_default(WebAutoplay::Disabled.label());
    }
    if form
        .checkbox("Readonly", &mut draft.web_readonly)
        .is_focus()
    {
        form.set_help(
            "View-only browser page. Turning it off enables the browser compose box and uploads. Applies live to connected browsers.",
        );
        form.set_default("on");
    }

    if !advanced_section(form, draft) {
        return;
    }
    let ui_defaults = UiConfig::default();
    if form
        .text("Overscan", &mut draft.ui_overscan, |text| {
            int_range_error(text, UI_OVERSCAN_RANGE)
        })
        .is_focus()
    {
        form.set_help("Extra chat rows rendered beyond the viewport for smoother scrolling.");
        form.set_default(ui_defaults.overscan.to_string());
    }
    if form
        .text("Max Messages", &mut draft.ui_max_messages, |text| {
            int_range_error(text, UI_MAX_MESSAGES_RANGE)
        })
        .is_focus()
    {
        form.set_help(
            "In-memory scrollback ceiling per room; the oldest messages are dropped past it.",
        );
        form.set_default(ui_defaults.max_messages.to_string());
    }
    if form
        .string_list(
            "Origin",
            &mut draft.web_allowed_origins,
            &mut draft.web_origins_new,
            web_origin_error,
        )
        .is_focus()
    {
        form.set_help(
            "Browser origins allowed to open the web log WebSocket, one per row. Empty derives origins from the bind address. Changes restart the web server.",
        );
        form.set_default("derived from Web Bind");
    }
}

fn data_tab(form: &mut SettingsForm, draft: &mut SettingsDraft) {
    form.section("Downloads");
    if form
        .choice_value(
            "Downloads",
            &mut draft.download_mode,
            &crate::config::DownloadMode::ALL,
            |mode| mode.label().to_string(),
        )
        .is_focus()
    {
        form.set_help(
            "How received files are handled: off rejects them, memory keeps them in a RAM buffer (lost on restart, viewable in the web log), persistent saves them to disk.",
        );
        form.set_default(crate::config::DownloadMode::Memory.label());
    }
    let download_mode = draft.download_mode;
    if download_mode == crate::config::DownloadMode::Persistent
        && form
            .text("Download Path", &mut draft.download_path, |value| {
                download_path_error(true, value)
            })
            .is_focus()
    {
        form.set_help("Directory where received files are saved.");
        form.set_default(crate::settings::default_download_path_text());
    }
    if download_mode == crate::config::DownloadMode::Memory
        && form
            .text(
                "Memory Buffer Size",
                &mut draft.download_memory_mb,
                |value| crate::settings::download_memory_error(value),
            )
            .is_focus()
    {
        form.set_help(
            "How much RAM the in-memory download buffer may use, in MiB (e.g. 512). The oldest files are dropped once this fills.",
        );
        form.set_default(FileConfig::default().download_memory_mb.to_string());
    }
    if form
        .text(
            "Max Download",
            &mut draft.files_max_download_mb,
            positive_mib_error,
        )
        .is_focus()
    {
        form.set_help(
            "Largest incoming file accepted, in MiB. Per-server and per-room overrides can lower it.",
        );
        form.set_default(FileConfig::default().max_download_mb.to_string());
    }

    form.section("History");
    if form
        .checkbox("Chat Persistence", &mut draft.history_enabled)
        .is_focus()
    {
        form.set_help(
            "Stores local room catalogs and chat logs under the chatt data directory for offline browsing.",
        );
        form.set_default("off");
    }
    if draft.history_enabled
        && form
            .text("Persistence Path", &mut draft.history_location, |_| None)
            .is_focus()
    {
        form.set_help(
            "Base directory for room catalogs and chat logs. Empty uses the chatt data directory. Applies on the next connection.",
        );
        form.set_default("chatt data directory");
    }

    form.section("Uploads");
    if form
        .text(
            "Max Upload",
            &mut draft.files_max_upload_mb,
            positive_mib_error,
        )
        .is_focus()
    {
        form.set_help("Largest file this client offers for upload, in MiB.");
        form.set_default(FileConfig::default().max_upload_mb.to_string());
    }

    if !advanced_section(form, draft) {
        return;
    }
    if form
        .text("Upload Rate", &mut draft.files_upload_rate, byte_size_error)
        .is_focus()
    {
        form.set_help(
            "Upload pacing ceiling in bytes per second, with optional K/M/G suffix. 0 streams at full socket speed.",
        );
        form.set_default("0 (unthrottled)");
    }
}

fn extra_tab(form: &mut SettingsForm, draft: &mut SettingsDraft) {
    form.section("Peer To Peer");
    if form.checkbox("P2P", &mut draft.p2p_enabled).is_focus() {
        form.set_help(
            "Direct peer media can reduce latency, but may expose your IP address to other users in a voice room.",
        );
        form.set_default("off");
    }
    if form
        .choice_value(
            "Candidate Privacy",
            &mut draft.p2p_candidate_privacy,
            &CandidatePrivacy::ALL,
            |privacy| privacy.label().to_string(),
        )
        .is_focus()
    {
        form.set_help(
            "How local host candidates are exposed to peers: mdns publishes random .local names, ip address publishes literal IPs, no host relies on reflexive and relay only. Applies on the next connection.",
        );
        form.set_default(CandidatePrivacy::Mdns.label());
    }
    if form
        .checkbox("Prefer IPv6", &mut draft.p2p_prefer_ipv6)
        .is_focus()
    {
        form.set_help(
            "Prefer native IPv6 over IPv4 at equal candidate type. Turn off to force IPv4-first for diagnostics. Applies on the next connection.",
        );
        form.set_default("on");
    }
}

/// The platform url-open default, rendered for the detail panel.
fn default_url_open_label() -> String {
    let command = default_url_open();
    if command.is_empty() {
        "none (clicks are inert)".to_string()
    } else {
        command.join(" ")
    }
}

fn adjust_db(value: &str, delta: isize, (min, max): (f32, f32), fallback: f32) -> String {
    let value = parse_db_value(value).unwrap_or(fallback);
    (value + delta as f32 * 3.0).clamp(min, max).to_string()
}

fn adjust_notification_db(value: &str, delta: isize) -> String {
    adjust_db(
        value,
        delta,
        (MIN_NOTIFICATION_VOLUME_DB, MAX_NOTIFICATION_VOLUME_DB),
        0.0,
    )
}

/// Replays the form layout headlessly to apply `intent` (and any pending text
/// commit) to the focused field, returning what the app layer must act on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settings_logic(
    state: &mut FormState<FieldId>,
    draft: &mut SettingsDraft,
    tab: SettingsTab,
    theme: &Theme,
    bindings: &BindingRuntime,
    dirty: bool,
    intent: FieldIntent,
    commit: Option<(FieldId, String)>,
    focus_column: Option<u16>,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &mut AudioOutputPickerState,
) -> SettingsOutput {
    let viewport = state.viewport();
    state.begin_frame(viewport);
    let action_labels = SettingsActionLabels::new(bindings, dirty);
    let mut form = SettingsForm::new(
        state,
        None,
        theme,
        &action_labels,
        dirty,
        intent,
        commit,
        focus_column,
    );
    settings_ui(
        &mut form,
        draft,
        tab,
        input_items,
        input_picker,
        output_items,
        output_picker,
    );
    let output = std::mem::take(&mut form.output);
    state.finish_frame();
    output
}

#[allow(clippy::too_many_arguments)]
pub fn draw_settings(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    bindings: &BindingRuntime,
    settings: &mut SettingsDraft,
    form: &mut FormState<FieldId>,
    tab: SettingsTab,
    dirty: bool,
    capture: Option<&StatsSnapshot>,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &mut AudioOutputPickerState,
) {
    area.with(theme.background).fill(buf);
    if area.is_empty() {
        return;
    }

    let mut rows = area;
    if tab == SettingsTab::Audio {
        vu::draw_settings_vu_row(rows.take_top(1), buf, capture, false, theme);
    }

    let mut body = rows;
    let detail = if body.w >= MIN_DETAIL_SCREEN_WIDTH {
        let detail = body.take_right(DETAIL_WIDTH as i32);
        body.take_right(1).with(theme.background).fill(buf);
        Some(Rect {
            x: detail.x.saturating_add(1),
            w: detail.w.saturating_sub(1),
            ..detail
        })
    } else {
        None
    };

    if body.is_empty() {
        return;
    }
    form.begin_frame(body);
    let focus_detail = {
        let action_labels = SettingsActionLabels::new(bindings, dirty);
        let mut context = SettingsForm::new(
            form,
            Some(buf),
            theme,
            &action_labels,
            dirty,
            FieldIntent::None,
            None,
            None,
        );
        settings_ui(
            &mut context,
            settings,
            tab,
            input_items,
            input_picker,
            output_items,
            output_picker,
        );
        context.detail.take()
    };
    form.finish_frame();

    if let Some(detail) = detail {
        draw_focus_detail(
            detail,
            buf,
            theme,
            settings,
            focus_detail,
            input_items,
            input_picker,
            output_items,
            output_picker,
        );
    }
}

fn picker_rows(items: &[AudioDeviceItem]) -> u16 {
    let wanted = items.len().clamp(MIN_PICKER_ROWS as usize, 8) as u16;
    wanted.min(8u16.max(MIN_PICKER_ROWS))
}

fn draw_audio_item(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    item: &AudioDeviceItem,
    selected: bool,
    focused: bool,
) {
    let base = if selected && focused {
        theme.selected_focused
    } else if selected {
        theme.row_focused
    } else {
        theme.background
    };
    buf.clear_rect(area, base);

    let mut rows = area;
    let mut top = rows.take_top(1);
    top.take_left(2)
        .with(base.patch(if selected { theme.good } else { theme.subtle }))
        .text(buf, if selected { ">" } else { " " });
    top.with(base.patch(if item.supported {
        theme.text
    } else {
        theme.error
    }))
    .with(Ellipsis(true))
    .text(buf, &item.name);

    if rows.h > 0 {
        let mut detail = rows.take_top(1);
        detail.take_left(2).with(base).text(buf, " ");
        detail
            .with(base.patch(theme.muted))
            .with(Ellipsis(true))
            .text(
                buf,
                &format!(
                    "{}  {}",
                    item_variant_summary(item),
                    item.primary_metadata()
                ),
            );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_focus_detail(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    settings: &SettingsDraft,
    detail: Option<FocusDetail>,
    input_items: &[AudioInputItem],
    input_picker: &AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &AudioOutputPickerState,
) {
    buf.clear_rect(area, theme.detail_panel);
    let Some(detail) = detail else {
        return;
    };
    let mut rows = area;
    let title = match &detail {
        FocusDetail::Device(DeviceSide::Input) => "Capture Device",
        FocusDetail::Device(DeviceSide::Output) => "Playback Device",
        FocusDetail::Option { .. } => "",
    };
    if !title.is_empty() {
        rows.take_top(1)
            .with(theme.detail_panel.patch(theme.accent | Modifier::BOLD))
            .with(Ellipsis(true))
            .text(buf, &format!(" {title} "));
    }

    match detail {
        FocusDetail::Device(DeviceSide::Input) => draw_device_detail(
            rows,
            buf,
            theme,
            focused_device(
                input_items,
                input_picker,
                settings.input_device_id.as_deref(),
            ),
        ),
        FocusDetail::Device(DeviceSide::Output) => draw_device_detail(
            rows,
            buf,
            theme,
            focused_device(
                output_items,
                output_picker,
                settings.output_device_id.as_deref(),
            ),
        ),
        FocusDetail::Option {
            current,
            default,
            error,
            help,
        } => draw_option_detail(
            rows,
            buf,
            theme,
            &current,
            default.as_deref(),
            error.as_deref(),
            help,
        ),
    }
}

fn focused_device<'a>(
    items: &'a [AudioDeviceItem],
    picker: &AudioDevicePickerState,
    selection: Option<&str>,
) -> Option<&'a AudioDeviceItem> {
    if picker.open {
        return picker
            .selector
            .current_item_index()
            .and_then(|index| items.get(index));
    }
    items.iter().find(|item| item.matches_selection(selection))
}

fn draw_device_detail(area: Rect, buf: &mut Buffer, theme: &Theme, item: Option<&AudioDeviceItem>) {
    let panel = theme.detail_panel;
    let Some(item) = item else {
        area.with(panel.patch(theme.subtle))
            .with(HAlign::Center)
            .text(buf, "No device");
        return;
    };

    let mut rows = area;
    rows.take_top(1)
        .with(panel.patch(theme.text | Modifier::BOLD))
        .with(Ellipsis(true))
        .text(buf, &item.name);
    widgets::draw_metadata_line(
        rows.take_top(1),
        buf,
        theme,
        panel,
        10,
        "Index",
        &item
            .device_index
            .map(|index| format!("CPAL #{index}"))
            .unwrap_or_else(|| "OS default".to_string()),
    );
    if let Some(id) = item.backend_id.as_ref().or(item.selection.as_ref()) {
        widgets::draw_metadata_line(rows.take_top(1), buf, theme, panel, 10, "ID", id);
    }
    if item.variants.len() > 1 {
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            theme,
            panel,
            10,
            "Variants",
            &item_variant_indexes(item),
        );
    }
    if let Some(preview) = &item.preview {
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            theme,
            panel,
            10,
            "Channels",
            &preview.channels.to_string(),
        );
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            theme,
            panel,
            10,
            "Format",
            &preview.sample_format.to_string(),
        );
        widgets::draw_metadata_line(rows.take_top(1), buf, theme, panel, 10, "Rate", "48 kHz");
        if let cpal::BufferSize::Fixed(frames) = preview.buffer_size {
            widgets::draw_metadata_line(
                rows.take_top(1),
                buf,
                theme,
                panel,
                10,
                "Buffer",
                &format!("{frames} frames"),
            );
        }
    } else if let Some(issue) = &item.issue {
        widgets::draw_metadata_line(rows.take_top(1), buf, theme, panel, 10, "Issue", issue);
    } else {
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            theme,
            panel,
            10,
            "Source",
            item.default_source,
        );
    }
}

fn draw_option_detail(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    current: &str,
    default: Option<&str>,
    error: Option<&str>,
    help: &str,
) {
    let panel = theme.detail_panel;
    let mut rows = area;
    if !current.is_empty() {
        widgets::draw_metadata_line(rows.take_top(1), buf, theme, panel, 10, "Current", current);
    }
    if let Some(default) = default {
        widgets::draw_metadata_line(rows.take_top(1), buf, theme, panel, 10, "Default", default);
    }
    if let Some(error) = error {
        rows.take_top(1).with(panel).fill(buf);
        for line in wrap_detail(error, rows.w as usize) {
            rows.take_top(1)
                .with(panel.patch(theme.error))
                .with(Ellipsis(true))
                .text(buf, &line);
        }
    }
    if !help.is_empty() {
        rows.take_top(1).with(panel).fill(buf);
        for line in wrap_detail(help, rows.w as usize) {
            rows.take_top(1)
                .with(panel.patch(theme.muted))
                .with(Ellipsis(true))
                .text(buf, &line);
        }
    }
}

fn wrap_detail(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let next_len = if current.is_empty() {
            word.len()
        } else {
            current.len() + 1 + word.len()
        };
        if next_len > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn item_variant_summary(item: &AudioDeviceItem) -> String {
    match (item.device_index, item.variants.len()) {
        (Some(index), 0 | 1) => format!("#{index}"),
        (Some(index), len) => format!("#{index}, {len} variants"),
        (None, _) => "default".to_string(),
    }
}

fn item_variant_indexes(item: &AudioDeviceItem) -> String {
    let mut label = String::new();
    for variant in &item.variants {
        if !label.is_empty() {
            label.push(' ');
        }
        label.push('#');
        label.push_str(&variant.index.to_string());
    }
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_wraps_on_word_boundaries() {
        assert_eq!(
            wrap_detail("one two three four", 8),
            vec![
                "one two".to_string(),
                "three".to_string(),
                "four".to_string()
            ]
        );
    }

    #[test]
    fn device_field_ids_are_distinct() {
        assert_ne!(capture_device_id(), playback_device_id());
    }

    fn run_logic(
        state: &mut FormState<FieldId>,
        draft: &mut SettingsDraft,
        tab: SettingsTab,
        commit: Option<(FieldId, String)>,
    ) -> SettingsOutput {
        let config = crate::config::Config::default();
        let theme = Theme::tomorrow_night();
        let mut input_picker = AudioInputPickerState::default();
        let mut output_picker = AudioOutputPickerState::default();
        settings_logic(
            state,
            draft,
            tab,
            &theme,
            &config.bindings,
            false,
            FieldIntent::None,
            commit,
            None,
            &[],
            &mut input_picker,
            &[],
            &mut output_picker,
        )
    }

    fn test_draft() -> SettingsDraft {
        SettingsDraft::from_audio(&crate::config::AudioConfig::default())
    }

    #[test]
    fn tab_cycle_wraps_both_directions() {
        assert_eq!(SettingsTab::Audio.cycle(1), SettingsTab::Interface);
        assert_eq!(SettingsTab::Extra.cycle(1), SettingsTab::Audio);
        assert_eq!(SettingsTab::Audio.cycle(-1), SettingsTab::Extra);
    }

    #[test]
    fn db_adjustment_moves_three_db_and_clamps_to_bounds() {
        assert_eq!(adjust_notification_db("-7.25", 1), "-4.25");
        assert_eq!(adjust_notification_db("11.5", 1), "12");
        assert_eq!(adjust_notification_db("-23", -1), "-24");
        assert_eq!(
            adjust_db("9.5", -1, MAX_AMPLIFICATION_DB_RANGE, 12.0),
            "6.5"
        );
    }

    #[test]
    fn tabs_register_disjoint_fields() {
        // A field id registered on the Audio tab must vanish on the Data tab,
        // so focus falls back to the new tab's first field.
        let config = crate::config::Config::default();
        let mut draft = test_draft();

        let mut state = FormState::new(capture_device_id(), config.ui.default_bindings);
        run_logic(&mut state, &mut draft, SettingsTab::Audio, None);
        assert_eq!(state.focus(), capture_device_id());

        let mut state = FormState::new(capture_device_id(), config.ui.default_bindings);
        run_logic(&mut state, &mut draft, SettingsTab::Data, None);
        assert_ne!(state.focus(), capture_device_id());
    }

    #[test]
    fn advanced_rows_hidden_until_toggle() {
        let config = crate::config::Config::default();
        let buffer_field = field_id_for("Advanced", "Capture Buffer");
        let mut draft = test_draft();

        let mut state = FormState::new(buffer_field, config.ui.default_bindings);
        run_logic(&mut state, &mut draft, SettingsTab::Audio, None);
        assert_ne!(state.focus(), buffer_field);

        draft.show_advanced = true;
        let mut state = FormState::new(buffer_field, config.ui.default_bindings);
        run_logic(&mut state, &mut draft, SettingsTab::Audio, None);
        assert_eq!(state.focus(), buffer_field);
    }

    #[test]
    fn web_readonly_is_available_without_advanced_settings() {
        let config = crate::config::Config::default();
        let field = field_id_for("Web Log Server", "Readonly");
        let mut draft = test_draft();
        assert!(!draft.show_advanced);
        assert!(draft.web_readonly);

        let mut state = FormState::new(field, config.ui.default_bindings);
        let theme = Theme::tomorrow_night();
        let mut input_picker = AudioInputPickerState::default();
        let mut output_picker = AudioOutputPickerState::default();
        let output = settings_logic(
            &mut state,
            &mut draft,
            SettingsTab::Interface,
            &theme,
            &config.bindings,
            false,
            FieldIntent::Activate,
            None,
            None,
            &[],
            &mut input_picker,
            &[],
            &mut output_picker,
        );

        assert_eq!(state.focus(), field);
        assert!(!draft.web_readonly);
        assert!(output.changed);
    }

    #[test]
    fn string_list_promotes_numbered_add_row_and_registers_the_next() {
        let config = crate::config::Config::default();
        let mut draft = test_draft();
        draft.show_advanced = true;
        let add_row = field_id_for("Advanced", "Origin 1");

        let mut state = FormState::new(add_row, config.ui.default_bindings);
        let output = run_logic(
            &mut state,
            &mut draft,
            SettingsTab::Interface,
            Some((add_row, "https://chat.example.test".to_string())),
        );
        assert!(output.changed);
        assert_eq!(draft.web_allowed_origins, ["https://chat.example.test"]);
        assert!(draft.web_origins_new.is_empty());

        run_logic(&mut state, &mut draft, SettingsTab::Interface, None);
        let next_row = field_id_for("Advanced", "Origin 2");
        state.move_focus(1);
        assert_eq!(state.focus(), next_row);

        let first_row = field_id_for("Advanced", "Origin 1");
        let mut state = FormState::new(first_row, config.ui.default_bindings);
        let output = run_logic(
            &mut state,
            &mut draft,
            SettingsTab::Interface,
            Some((first_row, "  ".to_string())),
        );
        assert!(output.changed);
        assert!(draft.web_allowed_origins.is_empty());
    }

    #[test]
    fn latency_row_commit_lands_in_draft() {
        let config = crate::config::Config::default();
        let mut draft = test_draft();
        draft.show_advanced = true;
        let field = field_id_for("Latency", "Min Delay");

        let mut state = FormState::new(field, config.ui.default_bindings);
        let output = run_logic(
            &mut state,
            &mut draft,
            SettingsTab::Audio,
            Some((field, "205".to_string())),
        );
        assert!(output.changed);
        assert_eq!(draft.latency_ms.neteq_min_delay_ms, "205");
        assert!(draft.latency_cross_error().is_some());
    }

    #[test]
    fn tab_bar_returns_four_hit_rects() {
        let theme = Theme::tomorrow_night();
        let mut buf = Buffer::new(60, 1);
        let rects = draw_settings_tabs(buf.rect(), &mut buf, &theme, SettingsTab::Audio);

        for rect in rects {
            assert!(!rect.is_empty());
        }
        for pair in rects.windows(2) {
            assert!(pair[0].x + pair[0].w < pair[1].x);
        }
    }
}
