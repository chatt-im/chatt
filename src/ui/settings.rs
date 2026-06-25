use crate::{
    audio::StatsSnapshot,
    settings::{
        AudioDeviceItem, AudioDevicePickerState, AudioInputItem, AudioInputPickerState,
        AudioOutputItem, AudioOutputPickerState, SettingsDraft, SettingsFocus,
        selected_audio_input_label, selected_audio_output_label,
    },
    theme::Theme,
    tui::{
        form::{FormFieldKind, FormState},
        widgets,
    },
    ui::vu,
};
use extui::{Buffer, Ellipsis, HAlign, Rect, vt::Modifier};

const LABEL_WIDTH: u16 = 18;
const DETAIL_WIDTH: u16 = 34;
const MIN_DETAIL_SCREEN_WIDTH: u16 = 92;
const MIN_PICKER_ROWS: u16 = 3;
const ACTION_ROWS: [SettingsFocus; 3] = [
    SettingsFocus::Refresh,
    SettingsFocus::Save,
    SettingsFocus::Close,
];
const CAPTURE_ROWS: [SettingsFocus; 10] = [
    SettingsFocus::Bitrate,
    SettingsFocus::Denoise,
    SettingsFocus::EchoCancellation,
    SettingsFocus::Amplification,
    SettingsFocus::Suppression,
    SettingsFocus::Release,
    SettingsFocus::TypingSuppression,
    SettingsFocus::TypingVadEnter,
    SettingsFocus::TypingVadRelease,
    SettingsFocus::CaptureBuffer,
];
const PLAYBACK_ROWS: [SettingsFocus; 1] = [SettingsFocus::PlaybackBuffer];
const INTERFACE_ROWS: [SettingsFocus; 2] = [SettingsFocus::FormBindings, SettingsFocus::Theme];

pub fn draw_settings(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    settings: &mut SettingsDraft,
    form: &mut FormState<SettingsFocus>,
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

    draw_form(
        body,
        buf,
        theme,
        settings,
        form,
        dirty,
        input_items,
        input_picker,
        output_items,
        output_picker,
    );

    if let Some(detail) = detail {
        draw_focus_detail(
            detail,
            buf,
            theme,
            settings,
            form.focus(),
            input_items,
            input_picker,
            output_items,
            output_picker,
        );
    }
}

fn draw_form(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    settings: &mut SettingsDraft,
    form: &mut FormState<SettingsFocus>,
    dirty: bool,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &mut AudioOutputPickerState,
) {
    if area.is_empty() {
        return;
    }

    form.begin_frame(area);
    draw_section(form, buf, theme, "Capture Settings");
    let input_selected = selected_audio_input_label(input_items, settings.input_selection());
    draw_device_row(
        form,
        buf,
        theme,
        settings,
        SettingsFocus::CaptureDevice,
        dirty,
        input_picker,
        input_selected,
    );
    draw_control_row(
        form,
        buf,
        theme,
        settings,
        SettingsFocus::RawCaptureDevice,
        dirty,
    );
    if input_picker.open && !settings.input_raw() {
        draw_audio_picker(
            form,
            buf,
            theme,
            SettingsFocus::CaptureDevice,
            input_items,
            input_picker,
        );
    }
    for row in CAPTURE_ROWS {
        draw_control_row(form, buf, theme, settings, row, dirty);
    }

    form.spacer(1);
    draw_section(form, buf, theme, "Playback Settings");
    let output_selected = selected_audio_output_label(output_items, settings.output_selection());
    draw_device_row(
        form,
        buf,
        theme,
        settings,
        SettingsFocus::PlaybackDevice,
        dirty,
        output_picker,
        output_selected,
    );
    draw_control_row(
        form,
        buf,
        theme,
        settings,
        SettingsFocus::RawPlaybackDevice,
        dirty,
    );
    if output_picker.open && !settings.output_raw() {
        draw_audio_picker(
            form,
            buf,
            theme,
            SettingsFocus::PlaybackDevice,
            output_items,
            output_picker,
        );
    }
    for row in PLAYBACK_ROWS {
        draw_control_row(form, buf, theme, settings, row, dirty);
    }

    form.spacer(1);
    draw_section(form, buf, theme, "Interface Settings");
    for row in INTERFACE_ROWS {
        draw_control_row(form, buf, theme, settings, row, dirty);
    }

    form.spacer(1);
    draw_section(form, buf, theme, "Actions");
    draw_action_buttons(form, buf, theme, dirty);
    form.finish_frame();
}

fn draw_section(form: &mut FormState<SettingsFocus>, buf: &mut Buffer, theme: &Theme, title: &str) {
    let row = form.next_row(1);
    if let Some(area) = row.rect {
        widgets::draw_section_header(area, buf, theme, &format!(" {title} "));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_device_row(
    form: &mut FormState<SettingsFocus>,
    buf: &mut Buffer,
    theme: &Theme,
    settings: &mut SettingsDraft,
    field: SettingsFocus,
    dirty: bool,
    picker: &AudioDevicePickerState,
    selected: String,
) {
    let row = form.next_row(1);
    let kind = settings.field_kind(field);
    let Some(area) = form.register_field(row, field, kind) else {
        return;
    };
    let focused = form.focus() == field;
    if kind == FormFieldKind::Text {
        let error = settings.field_error(field).is_some();
        if focused {
            let value = device_selection_text(settings, field);
            if let Some((commit_field, text)) = form.focus_text(field, &value, false) {
                let _ = settings.commit_field_text(commit_field, text);
            }
            let input = widgets::draw_labeled_editor_frame(
                area,
                buf,
                theme,
                LABEL_WIDTH,
                "Device",
                true,
                error,
            );
            form.register_text_area(field, input);
            form.render_editor(input, buf, theme);
        } else {
            let value = device_selection_display(settings, field);
            draw_value_row(area, buf, theme, "Device", &value, false, dirty, error);
        }
        return;
    }

    let value = if picker.open && picker.searching {
        format!("/{}", picker.selector.query())
    } else {
        selected
    };
    widgets::draw_labeled_value(
        area,
        buf,
        theme,
        LABEL_WIDTH,
        "Device",
        &value,
        focused,
        dirty,
    );
}

fn draw_control_row(
    form: &mut FormState<SettingsFocus>,
    buf: &mut Buffer,
    theme: &Theme,
    settings: &mut SettingsDraft,
    field: SettingsFocus,
    dirty: bool,
) {
    let row = form.next_row(1);
    let kind = settings.field_kind(field);
    let Some(area) = form.register_field(row, field, kind) else {
        return;
    };
    let focused = form.focus() == field;
    let error = settings.field_error(field).is_some();
    if kind == FormFieldKind::Text {
        if focused {
            let value = settings.buffer_text(field);
            if let Some((commit_field, text)) = form.focus_text(field, &value, false) {
                let _ = settings.commit_field_text(commit_field, text);
            }
            let input = widgets::draw_labeled_editor_frame(
                area,
                buf,
                theme,
                LABEL_WIDTH,
                setting_label(field),
                true,
                error,
            );
            form.register_text_area(field, input);
            form.render_editor(input, buf, theme);
        } else {
            draw_value_row(
                area,
                buf,
                theme,
                setting_label(field),
                &settings.option_label(field),
                false,
                dirty,
                error,
            );
        }
        return;
    }

    draw_value_row(
        area,
        buf,
        theme,
        setting_label(field),
        &settings.option_label(field),
        focused,
        dirty,
        error,
    );
    if kind == FormFieldKind::Choice {
        let value_x = area.x.saturating_add(LABEL_WIDTH.min(area.w));
        let value_w = area.w.saturating_sub(LABEL_WIDTH);
        let left_w = value_w / 2;
        if left_w > 0 {
            form.register_adjust(
                field,
                Rect {
                    x: value_x,
                    y: area.y,
                    w: left_w,
                    h: area.h,
                },
                -1,
            );
        }
        let right_w = value_w.saturating_sub(left_w);
        if right_w > 0 {
            form.register_adjust(
                field,
                Rect {
                    x: value_x.saturating_add(left_w),
                    y: area.y,
                    w: right_w,
                    h: area.h,
                },
                1,
            );
        }
    }
}

fn draw_action_buttons(
    form: &mut FormState<SettingsFocus>,
    buf: &mut Buffer,
    theme: &Theme,
    dirty: bool,
) {
    let row = form.next_row(1);
    let Some(area) = row.rect else {
        for field in ACTION_ROWS {
            form.register_field(row, field, FormFieldKind::Action);
        }
        return;
    };
    let width = (area.w / ACTION_ROWS.len() as u16).max(1);
    let mut buttons = area;
    for (index, field) in ACTION_ROWS.iter().copied().enumerate() {
        let button = if index + 1 == ACTION_ROWS.len() {
            buttons
        } else {
            buttons.take_left(width as i32)
        };
        form.register_rect(row, button, field, FormFieldKind::Action);
        draw_action_button(
            button,
            buf,
            theme,
            action_label(field, dirty),
            form.focus() == field,
        );
    }
}

fn draw_action_button(area: Rect, buf: &mut Buffer, theme: &Theme, label: &str, focused: bool) {
    widgets::draw_action(area, buf, theme, label, focused);
}

fn action_label(field: SettingsFocus, dirty: bool) -> &'static str {
    match field {
        SettingsFocus::Refresh => "Refresh devices",
        SettingsFocus::Save if dirty => "Save config *",
        SettingsFocus::Save => "Save config",
        SettingsFocus::Close => "Back to chat",
        _ => "",
    }
}

/// Current raw device string for the editor seed, empty when unset.
fn device_selection_text(settings: &SettingsDraft, field: SettingsFocus) -> String {
    let selection = match field {
        SettingsFocus::CaptureDevice => settings.input_selection(),
        SettingsFocus::PlaybackDevice => settings.output_selection(),
        _ => None,
    };
    selection.unwrap_or("").to_string()
}

/// Display label for an unfocused raw device row, falling back to a placeholder.
fn device_selection_display(settings: &SettingsDraft, field: SettingsFocus) -> String {
    let text = device_selection_text(settings, field);
    if text.is_empty() {
        "system default".to_string()
    } else {
        text
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_value_row(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
    error: bool,
) {
    widgets::draw_labeled_value_with(
        area,
        buf,
        widgets::RowPalette::from_theme(theme),
        LABEL_WIDTH,
        label,
        value,
        focused,
        dirty,
        error,
    );
}

pub(crate) fn setting_label(focus: SettingsFocus) -> &'static str {
    match focus {
        SettingsFocus::CaptureDevice | SettingsFocus::PlaybackDevice => "Device",
        SettingsFocus::RawCaptureDevice | SettingsFocus::RawPlaybackDevice => "Raw Device",
        SettingsFocus::Bitrate => "Bitrate",
        SettingsFocus::Denoise => "Denoise",
        SettingsFocus::EchoCancellation => "Echo Cancel",
        SettingsFocus::Amplification => "Max Gain",
        SettingsFocus::Suppression => "Suppression",
        SettingsFocus::Release => "Release",
        SettingsFocus::TypingSuppression => "Typing Gate",
        SettingsFocus::TypingVadEnter => "Gate Start",
        SettingsFocus::TypingVadRelease => "Gate Release",
        SettingsFocus::CaptureBuffer => "Capture Buffer",
        SettingsFocus::PlaybackBuffer => "Playback Buffer",
        SettingsFocus::FormBindings => "Form Bindings",
        SettingsFocus::Theme => "Theme",
        SettingsFocus::Refresh => "Refresh",
        SettingsFocus::Save => "Save",
        SettingsFocus::Close => "Close",
    }
}

fn draw_audio_picker(
    form: &mut FormState<SettingsFocus>,
    buf: &mut Buffer,
    theme: &Theme,
    field: SettingsFocus,
    items: &[AudioDeviceItem],
    picker: &mut AudioDevicePickerState,
) {
    let rows = picker_rows(form, items);
    if rows == 0 {
        return;
    }
    let area_row = form.next_row(rows);
    let Some(area) = area_row.rect else {
        return;
    };
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
            form.register_picker_item(field, area, item_index);
            if let Some(item) = items.get(item_index) {
                draw_audio_item(area, buf, theme, item, selected, form.focus() == field);
            }
        },
    );
}

fn picker_rows(form: &FormState<SettingsFocus>, items: &[AudioDeviceItem]) -> u16 {
    let wanted = items.len().clamp(MIN_PICKER_ROWS as usize, 8) as u16;
    wanted.min(form_rows_available_hint(form).max(MIN_PICKER_ROWS))
}

fn form_rows_available_hint(_: &FormState<SettingsFocus>) -> u16 {
    8
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

fn draw_focus_detail(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    settings: &SettingsDraft,
    focus: SettingsFocus,
    input_items: &[AudioInputItem],
    input_picker: &AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &AudioOutputPickerState,
) {
    buf.clear_rect(area, theme.detail_panel);
    let mut rows = area;
    rows.take_top(1)
        .with(theme.detail_panel.patch(theme.accent | Modifier::BOLD))
        .with(Ellipsis(true))
        .text(buf, &format!(" {} ", detail_title(focus)));

    match focus {
        SettingsFocus::CaptureDevice if !settings.input_raw() => draw_device_detail(
            rows,
            buf,
            theme,
            focused_device(input_items, input_picker, settings.input_selection()),
        ),
        SettingsFocus::PlaybackDevice if !settings.output_raw() => draw_device_detail(
            rows,
            buf,
            theme,
            focused_device(output_items, output_picker, settings.output_selection()),
        ),
        _ => draw_option_detail(rows, buf, theme, settings, focus),
    }
}

fn detail_title(focus: SettingsFocus) -> &'static str {
    match focus {
        SettingsFocus::CaptureDevice => "Capture Device",
        SettingsFocus::PlaybackDevice => "Playback Device",
        _ => setting_label(focus),
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
    settings: &SettingsDraft,
    focus: SettingsFocus,
) {
    let panel = theme.detail_panel;
    let mut rows = area;
    let current = match focus {
        SettingsFocus::CaptureDevice | SettingsFocus::PlaybackDevice => {
            device_selection_display(settings, focus)
        }
        _ => settings.option_label(focus),
    };
    widgets::draw_metadata_line(rows.take_top(1), buf, theme, panel, 10, "Current", &current);
    if let Some(error) = settings.field_error(focus) {
        rows.take_top(1).with(panel).fill(buf);
        for line in wrap_detail(&error, rows.w as usize) {
            rows.take_top(1)
                .with(panel.patch(theme.error))
                .with(Ellipsis(true))
                .text(buf, &line);
        }
    }
    rows.take_top(1).with(panel).fill(buf);
    for line in wrap_detail(settings.option_detail(focus), rows.w as usize) {
        rows.take_top(1)
            .with(panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, &line);
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
    fn focus_order_matches_rendered_settings_layout() {
        let mut expected = vec![
            SettingsFocus::CaptureDevice,
            SettingsFocus::RawCaptureDevice,
        ];
        expected.extend(CAPTURE_ROWS);
        expected.push(SettingsFocus::PlaybackDevice);
        expected.push(SettingsFocus::RawPlaybackDevice);
        expected.extend(PLAYBACK_ROWS);
        expected.extend(INTERFACE_ROWS);
        expected.extend(ACTION_ROWS);

        assert_eq!(SettingsFocus::ORDER.as_slice(), expected.as_slice());
    }

    #[test]
    fn settings_sections_are_title_case() {
        for title in [
            "Capture Settings",
            "Playback Settings",
            "Interface Settings",
            "Actions",
        ] {
            assert_ne!(title, title.to_ascii_uppercase());
        }
    }

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
}
