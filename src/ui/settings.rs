use extui::{Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};

use crate::{
    audio::StatsSnapshot,
    settings::{
        AudioInputItem, AudioInputPickerState, SettingsDraft, SettingsFocus,
        selected_audio_input_label,
    },
    theme,
    ui::vu,
};

const SELECTED_FOCUSED: Style = Style::DEFAULT
    .with_bg_rgb(0x35, 0x3b, 0x46)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);
const SELECTED_DIM: Style = Style::DEFAULT
    .with_bg_rgb(0x24, 0x28, 0x30)
    .with_fg_rgb(0xd8, 0xdb, 0xd6);
const PANEL_EDGE: Style = Style::DEFAULT.with_bg_rgb(0x18, 0x1b, 0x20);
const SETTINGS_CONTROLS_ROWS: u16 = 9;
const MIN_DEVICE_PICKER_ROWS: u16 = 4;

pub fn draw_settings(
    area: Rect,
    buf: &mut Buffer,
    settings: &SettingsDraft,
    focus: SettingsFocus,
    dirty: bool,
    capture: Option<&StatsSnapshot>,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
) {
    area.with(theme::BACKGROUND).fill(buf);
    if area.is_empty() {
        return;
    }

    let mut rows = area;
    draw_input_header(
        rows.take_top(1),
        buf,
        settings,
        focus == SettingsFocus::Device,
        input_items,
        input_picker,
    );

    if input_picker.open {
        let controls_height = settings_controls_height(rows.h, input_items.len());
        let controls = rows.take_bottom(controls_height as i32);
        draw_audio_picker(
            rows,
            buf,
            focus == SettingsFocus::Device,
            input_items,
            input_picker,
        );
        draw_settings_controls(controls, buf, settings, focus, dirty, capture);
    } else {
        draw_settings_controls(rows, buf, settings, focus, dirty, capture);
    }
}

fn settings_controls_height(available: u16, input_count: usize) -> u16 {
    let min_picker_rows = if input_count > 1 {
        MIN_DEVICE_PICKER_ROWS
    } else {
        1
    };
    available
        .saturating_sub(min_picker_rows)
        .min(SETTINGS_CONTROLS_ROWS)
}

fn draw_input_header(
    area: Rect,
    buf: &mut Buffer,
    settings: &SettingsDraft,
    focused: bool,
    input_items: &[AudioInputItem],
    input_picker: &AudioInputPickerState,
) {
    let style = if focused {
        SELECTED_DIM
    } else {
        theme::BACKGROUND
    };
    buf.clear_rect(area, style);

    let selected = selected_audio_input_label(input_items, settings.input_device_id.as_deref());
    let mut row = area
        .with(style.patch(if focused { theme::GOOD } else { theme::MUTED }))
        .text(buf, "Input ");
    if input_picker.open && input_picker.searching {
        row = row.with(style.patch(theme::SUBTLE)).text(buf, "/");
        row = row
            .with(style.patch(theme::TEXT))
            .with(Ellipsis(true))
            .text(buf, input_picker.selector.query());
    }
    row.with(HAlign::Right)
        .with(style.patch(if focused { theme::GOOD } else { theme::TEXT }))
        .with(Ellipsis(true))
        .text(buf, &format!(" {} ", selected));
}

fn draw_audio_picker(
    area: Rect,
    buf: &mut Buffer,
    focused: bool,
    input_items: &[AudioInputItem],
    input_picker: &mut AudioInputPickerState,
) {
    if area.is_empty() {
        return;
    }

    let mut picker = area;
    let metadata = if picker.w >= 72 {
        Some(picker.take_right(34))
    } else if picker.h >= 9 {
        Some(picker.take_bottom(5))
    } else {
        None
    };

    buf.clear_rect(picker, theme::BACKGROUND);
    if input_picker.selector.filtered_len() == 0 {
        picker
            .with(theme::SUBTLE)
            .with(HAlign::Center)
            .text(buf, "No matching audio inputs");
    } else {
        let item_height = if picker.h < 4 { 1 } else { 2 };
        input_picker.selector.render(
            picker,
            item_height,
            buf,
            |_, item_index, selected, area, buf| {
                if let Some(item) = input_items.get(item_index) {
                    draw_audio_input_item(area, buf, item, selected, focused);
                }
            },
        );
    }

    if let Some(metadata) = metadata {
        draw_audio_metadata(metadata, buf, input_items, input_picker);
    }
}

fn draw_audio_input_item(
    area: Rect,
    buf: &mut Buffer,
    item: &AudioInputItem,
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
    let marker = if selected { ">" } else { " " };
    top.take_left(2)
        .with(base.patch(if selected { theme::GOOD } else { theme::SUBTLE }))
        .text(buf, marker);
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
                &format!("{}  {}", item_variant_summary(item), item.detail()),
            );
    }
}

fn draw_audio_metadata(
    area: Rect,
    buf: &mut Buffer,
    input_items: &[AudioInputItem],
    input_picker: &AudioInputPickerState,
) {
    buf.clear_rect(area, PANEL_EDGE);
    let Some(item) = input_picker
        .selector
        .current_item_index()
        .and_then(|index| input_items.get(index))
    else {
        area.with(PANEL_EDGE.patch(theme::SUBTLE))
            .with(HAlign::Center)
            .text(buf, "No input");
        return;
    };

    let mut rows = area;
    rows.take_top(1)
        .with(PANEL_EDGE.patch(theme::ACCENT | Modifier::BOLD))
        .with(Ellipsis(true))
        .text(buf, &format!(" {}", item.name));

    draw_metadata_line(
        rows.take_top(1),
        buf,
        "Index",
        &item
            .device_index
            .map(|index| format!("CPAL #{index}"))
            .unwrap_or_else(|| "OS default".to_string()),
    );
    if item.variants.len() > 1 {
        draw_metadata_line(
            rows.take_top(1),
            buf,
            "Variants",
            &item_variant_indexes(item),
        );
    }

    if let Some(preview) = &item.preview {
        draw_metadata_line(
            rows.take_top(1),
            buf,
            "Channels",
            &preview.channels.to_string(),
        );
        draw_metadata_line(
            rows.take_top(1),
            buf,
            "Format",
            &preview.sample_format.to_string(),
        );
        draw_metadata_line(rows.take_top(1), buf, "Rate", "48 kHz");
        if let cpal::BufferSize::Fixed(frames) = preview.buffer_size {
            draw_metadata_line(rows.take_top(1), buf, "Buffer", &format!("{frames} frames"));
        }
    } else if let Some(issue) = &item.issue {
        draw_metadata_line(rows.take_top(1), buf, "Issue", issue);
    } else {
        draw_metadata_line(rows.take_top(1), buf, "Source", "OS default input");
    }
}

fn item_variant_summary(item: &AudioInputItem) -> String {
    match (item.device_index, item.variants.len()) {
        (Some(index), 0 | 1) => format!("#{index}"),
        (Some(index), len) => format!("#{index}, {len} variants"),
        (None, _) => "default".to_string(),
    }
}

fn item_variant_indexes(item: &AudioInputItem) -> String {
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

fn draw_metadata_line(area: Rect, buf: &mut Buffer, label: &str, value: &str) {
    if area.is_empty() {
        return;
    }
    let mut row = area;
    row.take_left(10)
        .with(PANEL_EDGE.patch(theme::SUBTLE))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(PANEL_EDGE.patch(theme::TEXT))
        .with(Ellipsis(true))
        .text(buf, value);
}

fn draw_settings_controls(
    area: Rect,
    buf: &mut Buffer,
    settings: &SettingsDraft,
    focus: SettingsFocus,
    dirty: bool,
    capture: Option<&StatsSnapshot>,
) {
    if area.is_empty() {
        return;
    }
    buf.clear_rect(area, theme::BACKGROUND);
    let mut rows = area;
    vu::draw_settings_vu_row(rows.take_top(1), buf, capture, false);
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Bitrate",
        &format!("{} kbps", settings.bitrate_bps() / 1000),
        focus == SettingsFocus::Bitrate,
        dirty,
    );
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Denoise",
        if settings.denoise { "on" } else { "off" },
        focus == SettingsFocus::Denoise,
        dirty,
    );
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Max. Amplification",
        &format!("{:.2}", settings.max_amplification()),
        focus == SettingsFocus::Amplification,
        dirty,
    );
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Buffer",
        settings.buffer_request().label(),
        focus == SettingsFocus::Buffer,
        dirty,
    );
    rows.take_top(1).with(theme::BACKGROUND).fill(buf);
    draw_button_row(
        rows.take_top(1),
        buf,
        "Refresh devices",
        focus == SettingsFocus::Refresh,
    );
    draw_button_row(
        rows.take_top(1),
        buf,
        "Save config",
        focus == SettingsFocus::Save,
    );
    draw_button_row(
        rows.take_top(1),
        buf,
        "Back to chat",
        focus == SettingsFocus::Close,
    );
}

fn draw_settings_row(
    area: Rect,
    buf: &mut Buffer,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
) {
    let style = if focused {
        SELECTED_DIM
    } else {
        theme::BACKGROUND
    };
    buf.clear_rect(area, style);
    let mut row = area;
    row.take_left(16)
        .with(style.patch(if focused { theme::GOOD } else { theme::MUTED }))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(style.patch(if dirty { theme::WARN } else { theme::TEXT }))
        .with(Ellipsis(true))
        .text(buf, value);
}

fn draw_button_row(area: Rect, buf: &mut Buffer, label: &str, focused: bool) {
    let style = if focused {
        SELECTED_DIM
    } else {
        theme::BACKGROUND
    };
    buf.clear_rect(area, style);
    area.with(style.patch(if focused { theme::GOOD } else { theme::TEXT }))
        .text(buf, "  ")
        .text(buf, label);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_layout_preserves_device_picker_rows() {
        assert_eq!(settings_controls_height(7, 3), 3);
        assert_eq!(settings_controls_height(16, 3), SETTINGS_CONTROLS_ROWS);
    }

    #[test]
    fn settings_layout_keeps_full_controls_for_default_only_picker() {
        assert_eq!(settings_controls_height(7, 1), 6);
    }
}
