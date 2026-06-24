use crate::{
    audio::StatsSnapshot,
    settings::{
        AudioDeviceItem, AudioDevicePickerState, AudioInputItem, AudioInputPickerState,
        AudioOutputItem, AudioOutputPickerState, SettingsDraft, SettingsFocus,
        selected_audio_input_label, selected_audio_output_label,
    },
    theme,
    tui::widgets,
    ui::vu,
};
use extui::{Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};

const SELECTED_FOCUSED: Style = Style::DEFAULT
    .with_bg_rgb(0x35, 0x3b, 0x46)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);
const SELECTED_DIM: Style = Style::DEFAULT
    .with_bg_rgb(0x24, 0x28, 0x30)
    .with_fg_rgb(0xd8, 0xdb, 0xd6);
const DETAIL_PANEL: Style = Style::DEFAULT.with_bg_rgb(0x18, 0x1b, 0x20);
const LABEL_WIDTH: u16 = 18;
const DETAIL_WIDTH: u16 = 34;
const MIN_DETAIL_SCREEN_WIDTH: u16 = 92;
const MIN_PICKER_ROWS: u16 = 3;
const ACTION_ROWS: [SettingsFocus; 3] = [
    SettingsFocus::Refresh,
    SettingsFocus::Save,
    SettingsFocus::Close,
];
const INPUT_ROWS: [SettingsFocus; 5] = [
    SettingsFocus::Bitrate,
    SettingsFocus::Denoise,
    SettingsFocus::EchoCancellation,
    SettingsFocus::Amplification,
    SettingsFocus::InputBuffer,
];
const OUTPUT_ROWS: [SettingsFocus; 1] = [SettingsFocus::OutputBuffer];
const INPUT_FIXED_ROWS: u16 = 2 + INPUT_ROWS.len() as u16;
const OUTPUT_FIXED_ROWS: u16 = 2 + OUTPUT_ROWS.len() as u16;

pub fn draw_settings(
    area: Rect,
    buf: &mut Buffer,
    settings: &mut SettingsDraft,
    focus: SettingsFocus,
    dirty: bool,
    capture: Option<&StatsSnapshot>,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &mut AudioOutputPickerState,
) {
    area.with(theme::BACKGROUND).fill(buf);
    if area.is_empty() {
        return;
    }

    let mut rows = area;
    vu::draw_settings_vu_row(rows.take_top(1), buf, capture, false);

    let actions = rows.take_bottom(action_rows(rows.h) as i32);
    let mut body = rows;
    let detail = if body.w >= MIN_DETAIL_SCREEN_WIDTH {
        let mut detail = body.take_right(DETAIL_WIDTH as i32);
        body.take_right(1).with(theme::BACKGROUND).fill(buf);
        detail = detail.inset(1, 0);
        Some(detail)
    } else {
        None
    };

    draw_audio_sections(
        body,
        buf,
        settings,
        focus,
        dirty,
        input_items,
        input_picker,
        output_items,
        output_picker,
    );
    draw_actions(actions, buf, focus, dirty);

    if let Some(detail) = detail {
        draw_focus_detail(
            detail,
            buf,
            settings,
            focus,
            input_items,
            input_picker,
            output_items,
            output_picker,
        );
    }
}

fn action_rows(available: u16) -> u16 {
    available.min(ACTION_ROWS.len() as u16)
}

fn draw_audio_sections(
    area: Rect,
    buf: &mut Buffer,
    settings: &mut SettingsDraft,
    focus: SettingsFocus,
    dirty: bool,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &mut AudioOutputPickerState,
) {
    if area.is_empty() {
        return;
    }

    let mut rows = area;
    let input_height = input_section_height(rows.h, input_picker.open, output_picker.open);
    let input_area = rows.take_top(input_height as i32);
    draw_input_section(
        input_area,
        buf,
        settings,
        focus,
        dirty,
        input_items,
        input_picker,
    );
    if rows.h > 0 {
        rows.take_top(1).with(theme::BACKGROUND).fill(buf);
    }
    draw_output_section(
        rows,
        buf,
        settings,
        focus,
        dirty,
        output_items,
        output_picker,
    );
}

fn input_section_height(available: u16, input_picker_open: bool, output_picker_open: bool) -> u16 {
    if available <= INPUT_FIXED_ROWS {
        return available;
    }
    if output_picker_open {
        return available.min(INPUT_FIXED_ROWS);
    }
    if input_picker_open {
        return available
            .saturating_sub(OUTPUT_FIXED_ROWS + 1)
            .max(INPUT_FIXED_ROWS + MIN_PICKER_ROWS)
            .min(available);
    }
    available.min(INPUT_FIXED_ROWS)
}

fn draw_input_section(
    area: Rect,
    buf: &mut Buffer,
    settings: &mut SettingsDraft,
    focus: SettingsFocus,
    dirty: bool,
    input_items: &[AudioInputItem],
    picker: &mut AudioInputPickerState,
) {
    let mut rows = area;
    draw_section_header(rows.take_top(1), buf, " INPUT ");
    draw_device_row(
        rows.take_top(1),
        buf,
        "Device",
        focus == SettingsFocus::InputDevice,
        dirty,
        picker,
        selected_audio_input_label(input_items, settings.input_selection()),
    );
    if picker.open {
        let picker_rows = rows
            .h
            .saturating_sub(INPUT_ROWS.len() as u16)
            .max(MIN_PICKER_ROWS)
            .min(rows.h);
        draw_audio_picker(
            rows.take_top(picker_rows as i32),
            buf,
            focus == SettingsFocus::InputDevice,
            input_items,
            picker,
        );
    }
    for row in INPUT_ROWS {
        draw_control_row(rows.take_top(1), buf, settings, row, focus, dirty);
    }
}

fn draw_output_section(
    area: Rect,
    buf: &mut Buffer,
    settings: &mut SettingsDraft,
    focus: SettingsFocus,
    dirty: bool,
    output_items: &[AudioOutputItem],
    picker: &mut AudioOutputPickerState,
) {
    let mut rows = area;
    draw_section_header(rows.take_top(1), buf, " OUTPUT ");
    draw_device_row(
        rows.take_top(1),
        buf,
        "Device",
        focus == SettingsFocus::OutputDevice,
        dirty,
        picker,
        selected_audio_output_label(output_items, settings.output_selection()),
    );
    if picker.open {
        let picker_rows = rows
            .h
            .saturating_sub(OUTPUT_ROWS.len() as u16)
            .max(MIN_PICKER_ROWS)
            .min(rows.h);
        draw_audio_picker(
            rows.take_top(picker_rows as i32),
            buf,
            focus == SettingsFocus::OutputDevice,
            output_items,
            picker,
        );
    }
    for row in OUTPUT_ROWS {
        draw_control_row(rows.take_top(1), buf, settings, row, focus, dirty);
    }
}

fn draw_section_header(area: Rect, buf: &mut Buffer, label: &str) {
    widgets::draw_section_header(area, buf, label);
}

fn draw_device_row(
    area: Rect,
    buf: &mut Buffer,
    label: &str,
    focused: bool,
    dirty: bool,
    picker: &AudioDevicePickerState,
    selected: String,
) {
    let value = if picker.open && picker.searching {
        format!("/{}", picker.selector.query())
    } else {
        selected
    };
    widgets::draw_labeled_value(area, buf, LABEL_WIDTH, label, &value, focused, dirty);
}

fn draw_control_row(
    area: Rect,
    buf: &mut Buffer,
    settings: &mut SettingsDraft,
    row: SettingsFocus,
    focus: SettingsFocus,
    dirty: bool,
) {
    if matches!(
        row,
        SettingsFocus::InputBuffer | SettingsFocus::OutputBuffer
    ) && focus == row
    {
        let input =
            widgets::draw_labeled_editor_frame(area, buf, LABEL_WIDTH, setting_label(row), true);
        settings.render_buffer_editor(row, input, buf);
        return;
    }
    widgets::draw_labeled_value(
        area,
        buf,
        LABEL_WIDTH,
        setting_label(row),
        &settings.option_label(row),
        focus == row,
        dirty,
    );
}

fn draw_actions(area: Rect, buf: &mut Buffer, focus: SettingsFocus, dirty: bool) {
    if area.is_empty() {
        return;
    }
    let mut rows = area;
    draw_action_row(
        rows.take_top(1),
        buf,
        "Refresh devices",
        focus == SettingsFocus::Refresh,
        false,
    );
    draw_action_row(
        rows.take_top(1),
        buf,
        if dirty {
            "Save config *"
        } else {
            "Save config"
        },
        focus == SettingsFocus::Save,
        dirty,
    );
    draw_action_row(
        rows.take_top(1),
        buf,
        "Back to chat",
        focus == SettingsFocus::Close,
        false,
    );
}

fn draw_action_row(area: Rect, buf: &mut Buffer, label: &str, focused: bool, dirty: bool) {
    let mut label = label.to_string();
    if dirty && !label.ends_with('*') {
        label.push_str(" *");
    }
    widgets::draw_action(area, buf, &label, focused);
}

fn setting_label(focus: SettingsFocus) -> &'static str {
    match focus {
        SettingsFocus::InputDevice | SettingsFocus::OutputDevice => "Device",
        SettingsFocus::Bitrate => "Bitrate",
        SettingsFocus::Denoise => "Denoise",
        SettingsFocus::EchoCancellation => "Echo Cancel",
        SettingsFocus::Amplification => "Max Gain",
        SettingsFocus::InputBuffer => "Input Buffer",
        SettingsFocus::OutputBuffer => "Output Buffer",
        SettingsFocus::Refresh => "Refresh",
        SettingsFocus::Save => "Save",
        SettingsFocus::Close => "Close",
    }
}

fn draw_audio_picker(
    area: Rect,
    buf: &mut Buffer,
    focused: bool,
    items: &[AudioDeviceItem],
    picker: &mut AudioDevicePickerState,
) {
    if area.is_empty() {
        return;
    }
    buf.clear_rect(area, theme::BACKGROUND);
    if picker.selector.filtered_len() == 0 {
        area.with(theme::SUBTLE)
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
            if let Some(item) = items.get(item_index) {
                draw_audio_item(area, buf, item, selected, focused);
            }
        },
    );
}

fn draw_audio_item(
    area: Rect,
    buf: &mut Buffer,
    item: &AudioDeviceItem,
    selected: bool,
    focused: bool,
) {
    let base = if selected && focused {
        SELECTED_FOCUSED
    } else if selected {
        SELECTED_DIM
    } else {
        theme::BACKGROUND
    };
    buf.clear_rect(area, base);

    let mut rows = area;
    let mut top = rows.take_top(1);
    top.take_left(2)
        .with(base.patch(if selected { theme::GOOD } else { theme::SUBTLE }))
        .text(buf, if selected { ">" } else { " " });
    top.with(base.patch(if item.supported {
        theme::TEXT
    } else {
        theme::ERROR
    }))
    .with(Ellipsis(true))
    .text(buf, &item.name);

    if rows.h > 0 {
        let mut detail = rows.take_top(1);
        detail.take_left(2).with(base).text(buf, " ");
        detail
            .with(base.patch(theme::MUTED))
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
    settings: &SettingsDraft,
    focus: SettingsFocus,
    input_items: &[AudioInputItem],
    input_picker: &AudioInputPickerState,
    output_items: &[AudioOutputItem],
    output_picker: &AudioOutputPickerState,
) {
    buf.clear_rect(area, DETAIL_PANEL);
    let mut rows = area;
    rows.take_top(1)
        .with(DETAIL_PANEL.patch(theme::ACCENT | Modifier::BOLD))
        .with(Ellipsis(true))
        .text(buf, &format!(" {} ", detail_title(focus)));

    match focus {
        SettingsFocus::InputDevice => draw_device_detail(
            rows,
            buf,
            focused_device(input_items, input_picker, settings.input_selection()),
        ),
        SettingsFocus::OutputDevice => draw_device_detail(
            rows,
            buf,
            focused_device(output_items, output_picker, settings.output_selection()),
        ),
        _ => draw_option_detail(rows, buf, settings, focus),
    }
}

fn detail_title(focus: SettingsFocus) -> &'static str {
    match focus {
        SettingsFocus::InputDevice => "Input Device",
        SettingsFocus::OutputDevice => "Output Device",
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

fn draw_device_detail(area: Rect, buf: &mut Buffer, item: Option<&AudioDeviceItem>) {
    let Some(item) = item else {
        area.with(DETAIL_PANEL.patch(theme::SUBTLE))
            .with(HAlign::Center)
            .text(buf, "No device");
        return;
    };

    let mut rows = area;
    rows.take_top(1)
        .with(DETAIL_PANEL.patch(theme::TEXT | Modifier::BOLD))
        .with(Ellipsis(true))
        .text(buf, &item.name);
    widgets::draw_metadata_line(
        rows.take_top(1),
        buf,
        DETAIL_PANEL,
        10,
        "Index",
        &item
            .device_index
            .map(|index| format!("CPAL #{index}"))
            .unwrap_or_else(|| "OS default".to_string()),
    );
    if let Some(id) = item.backend_id.as_ref().or(item.selection.as_ref()) {
        widgets::draw_metadata_line(rows.take_top(1), buf, DETAIL_PANEL, 10, "ID", id);
    }
    if item.variants.len() > 1 {
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            DETAIL_PANEL,
            10,
            "Variants",
            &item_variant_indexes(item),
        );
    }
    if let Some(preview) = &item.preview {
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            DETAIL_PANEL,
            10,
            "Channels",
            &preview.channels.to_string(),
        );
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            DETAIL_PANEL,
            10,
            "Format",
            &preview.sample_format.to_string(),
        );
        widgets::draw_metadata_line(rows.take_top(1), buf, DETAIL_PANEL, 10, "Rate", "48 kHz");
        if let cpal::BufferSize::Fixed(frames) = preview.buffer_size {
            widgets::draw_metadata_line(
                rows.take_top(1),
                buf,
                DETAIL_PANEL,
                10,
                "Buffer",
                &format!("{frames} frames"),
            );
        }
    } else if let Some(issue) = &item.issue {
        widgets::draw_metadata_line(rows.take_top(1), buf, DETAIL_PANEL, 10, "Issue", issue);
    } else {
        widgets::draw_metadata_line(
            rows.take_top(1),
            buf,
            DETAIL_PANEL,
            10,
            "Source",
            item.default_source,
        );
    }
}

fn draw_option_detail(
    area: Rect,
    buf: &mut Buffer,
    settings: &SettingsDraft,
    focus: SettingsFocus,
) {
    let mut rows = area;
    widgets::draw_metadata_line(
        rows.take_top(1),
        buf,
        DETAIL_PANEL,
        10,
        "Current",
        &settings.option_label(focus),
    );
    rows.take_top(1).with(DETAIL_PANEL).fill(buf);
    for line in wrap_detail(settings.option_detail(focus), rows.w as usize) {
        rows.take_top(1)
            .with(DETAIL_PANEL.patch(theme::MUTED))
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
    fn input_section_expands_for_open_input_picker() {
        assert_eq!(input_section_height(14, true, false), 10);
        assert_eq!(input_section_height(8, true, false), 8);
    }

    #[test]
    fn output_picker_keeps_input_section_compact() {
        assert_eq!(input_section_height(14, false, true), 7);
    }

    #[test]
    fn focus_order_matches_rendered_settings_layout() {
        let mut expected = vec![SettingsFocus::InputDevice];
        expected.extend(INPUT_ROWS);
        expected.push(SettingsFocus::OutputDevice);
        expected.extend(OUTPUT_ROWS);
        expected.extend(ACTION_ROWS);

        assert_eq!(SettingsFocus::ORDER.as_slice(), expected.as_slice());
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
