use crate::audio::{DenoiseConfig, DredConfig};
use crate::config::{FormBindings, ThemeChoice};
use crate::{
    audio::StatsSnapshot,
    bindings::{self, BindCommand, BindingRuntime},
    settings::{
        AudioDeviceItem, AudioDevicePickerState, AudioInputItem, AudioInputPickerState,
        AudioOutputItem, AudioOutputPickerState, BITRATES, DENOISE_RELEASE_LABELS,
        DENOISE_RELEASES, DENOISE_SUPPRESSION_LABELS, DENOISE_SUPPRESSIONS,
        DENOISE_TYPING_VAD_LABELS, DENOISE_TYPING_VAD_THRESHOLDS, MAX_AMPLIFICATIONS,
        NOTIFICATION_VOLUMES_DB, SettingsDraft, buffer_field_error, download_path_error,
        form_bindings_label, output_volume_field_error, raw_device_error, raw_device_selection,
        selected_audio_input_label, selected_audio_output_label, volume_db_label, web_bind_error,
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

const LABEL_WIDTH: u16 = 18;
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

/// Builds the id for an arbitrary section/label pair. Used by tests that drive
/// the form without running the full declaration pass.
#[cfg(test)]
pub(crate) fn field_id_for(section: &str, label: &str) -> FieldId {
    FieldId::new(section, label)
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
    Exit,
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
    exit: String,
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
            exit: action_label(bindings, "Exit", BindCommand::Quit),
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

    /// Records the help text for the focused field. Call from the `is_focus`
    /// guard of a widget. The widget already filled the current value and any
    /// validation error.
    pub(crate) fn set_help(&mut self, help: &'static str) {
        if let Some(FocusDetail::Option { help: slot, .. }) = &mut self.detail {
            *slot = help;
        }
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

    /// The trailing action-button row. Buttons share a
    /// virtual row so left/right moves between them.
    fn actions(&mut self) {
        let specs = [
            ActionButton {
                key: "Exit",
                label: &self.action_labels.exit,
                value: SettingsButton::Exit,
                help: "Close chatt.",
                primary: false,
            },
            ActionButton {
                key: "Refresh",
                label: &self.action_labels.refresh,
                value: SettingsButton::Refresh,
                help: "Re-scan audio devices using the current buffer requests.",
                primary: false,
            },
            ActionButton {
                key: "Close",
                label: &self.action_labels.close,
                value: SettingsButton::Close,
                help: "Return to chat without saving further changes.",
                primary: false,
            },
            ActionButton {
                key: "Save",
                label: &self.action_labels.save,
                value: SettingsButton::Save,
                help: "Persist the draft to chatt.toml.",
                primary: true,
            },
        ];
        let response = self.form.actions(&specs);
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

/// Declares the whole settings form. The single place every field lives: one
/// widget call per row carries its label, the `&mut` it mutates, its input
/// kind, and (via the `is_focus` guard) its detail help.
fn settings_ui(
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
    }
    if form
        .choice("Bitrate", &mut draft.bitrate_index, BITRATES.len(), |i| {
            format!("{} kbps", BITRATES[i] / 1000)
        })
        .is_focus()
    {
        form.set_help("Opus target bitrate for outgoing voice packets.");
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
    }
    if form
        .checkbox("Echo Cancel", &mut draft.echo_cancellation)
        .is_focus()
    {
        form.set_help("Cancels speaker echo from the microphone path when supported.");
    }
    if form
        .choice(
            "Max Gain",
            &mut draft.amplification_index,
            MAX_AMPLIFICATIONS.len(),
            amplification_label,
        )
        .is_focus()
    {
        form.set_help("Auto-gain ceiling for quiet microphones; 0 dB disables amplification.");
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
        }
        if form.checkbox("Typing Gate", &mut draft.typing_suppression).is_focus() {
            form.set_help("Ducks loud low-VAD desk and keyboard thumps after RNNoise.");
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
        }
        if release.changed() && draft.typing_vad_release_index < draft.typing_vad_enter_index {
            draft.typing_vad_enter_index = draft.typing_vad_release_index;
        }
    });

    if form
        .text(
            "Capture Buffer",
            &mut draft.input_buffer,
            buffer_field_error,
        )
        .is_focus()
    {
        form.set_help("Requested capture buffer in samples, or default for the host backend.");
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
    }
    if form
        .checkbox("Render Assist", &mut draft.latency.render_assist)
        .is_focus()
    {
        form.set_help(
            "Pre-renders playout blocks off the audio callback thread. Enable only on devices too slow to decode within the callback; it adds output latency, so leave it off on capable hardware.",
        );
    }

    form.section("Web Log Server");
    if form.checkbox("Web Log", &mut draft.web_enabled).is_focus() {
        form.set_help("Starts the browser chat-log server. Bind is saved even while disabled.");
    }
    if form
        .text("Web Bind", &mut draft.web_bind, web_bind_error)
        .is_focus()
    {
        form.set_help(
            "Loopback socket address for the browser chat-log server, for example 127.0.0.1:8080.",
        );
    }

    form.section("Notifications");
    if form
        .choice(
            "Message Volume",
            &mut draft.message_notification_volume_index,
            NOTIFICATION_VOLUMES_DB.len(),
            notification_volume_label,
        )
        .is_focus()
    {
        form.set_help("Volume for incoming-message notification sounds.");
    }
    if form
        .choice(
            "Join Volume",
            &mut draft.peer_join_notification_volume_index,
            NOTIFICATION_VOLUMES_DB.len(),
            notification_volume_label,
        )
        .is_focus()
    {
        form.set_help("Volume for peer-joined notification sounds.");
    }
    if form
        .choice(
            "Leave Volume",
            &mut draft.peer_leave_notification_volume_index,
            NOTIFICATION_VOLUMES_DB.len(),
            notification_volume_label,
        )
        .is_focus()
    {
        form.set_help("Volume for peer-left notification sounds.");
    }

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
    }
    if form
        .choice_value("Theme", &mut draft.theme, &ThemeChoice::ALL, |theme| {
            theme.label().to_string()
        })
        .is_focus()
    {
        form.set_help("Color theme for the interface. Applies immediately; Save persists it.");
    }

    form.section("Privacy And Storage");
    if form.checkbox("P2P", &mut draft.p2p_enabled).is_focus() {
        form.set_help(
            "Direct peer media can reduce latency, but may expose your IP address to other users in a voice room.",
        );
    }
    if form
        .checkbox("Accept Downloads", &mut draft.accept_downloads)
        .is_focus()
    {
        form.set_help(
            "Allows incoming files up to the default 50 MB receive limit. The limit can be changed in the config file.",
        );
    }
    let accept_downloads = draft.accept_downloads;
    if accept_downloads
        && form
            .text("Download Path", &mut draft.download_path, |value| {
                download_path_error(accept_downloads, value)
            })
            .is_focus()
    {
        form.set_help("Directory where accepted files are saved.");
    }
    if form
        .checkbox("Chat Persistence", &mut draft.history_enabled)
        .is_focus()
    {
        form.set_help(
            "Stores local room catalogs and chat logs under the chatt data directory for offline browsing.",
        );
    }
    if draft.history_enabled
        && form
            .text("Persistence Path", &mut draft.history_location, |_| None)
            .is_focus()
    {
        form.set_help(
            "Base directory for room catalogs and chat logs. Empty uses the chatt data directory. Applies on the next connection.",
        );
    }

    form.form.spacer(1);
    form.actions();
}

fn amplification_label(index: usize) -> String {
    let value = MAX_AMPLIFICATIONS[index];
    if value <= 0.0 {
        "off".to_string()
    } else {
        format!("{value:.0} dB")
    }
}

fn notification_volume_label(index: usize) -> String {
    volume_db_label(NOTIFICATION_VOLUMES_DB[index])
}

/// Replays the form layout headlessly to apply `intent` (and any pending text
/// commit) to the focused field, returning what the app layer must act on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settings_logic(
    state: &mut FormState<FieldId>,
    draft: &mut SettingsDraft,
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
    vu::draw_settings_vu_row(rows.take_top(1), buf, capture, false, theme);

    let mut body = rows;
    let detail = if body.w >= MIN_DETAIL_SCREEN_WIDTH {
        let mut detail = body.take_right(DETAIL_WIDTH as i32);
        body.take_right(1).with(theme.background).fill(buf);
        detail = detail.inset(1, 0);
        Some(detail)
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
            error,
            help,
        } => draw_option_detail(rows, buf, theme, &current, error.as_deref(), help),
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
    error: Option<&str>,
    help: &str,
) {
    let panel = theme.detail_panel;
    let mut rows = area;
    if !current.is_empty() {
        widgets::draw_metadata_line(rows.take_top(1), buf, theme, panel, 10, "Current", current);
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
}
