use extui::{Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};

use crate::theme::Theme;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RowPalette {
    pub(crate) base: Style,
    pub(crate) focused: Style,
    pub(crate) label: Style,
    pub(crate) focused_label: Style,
    pub(crate) value: Style,
    pub(crate) dirty_value: Style,
    pub(crate) error: Style,
}

impl RowPalette {
    pub(crate) fn from_theme(theme: &Theme) -> Self {
        Self {
            base: theme.background,
            focused: theme.row_focused,
            label: theme.muted,
            focused_label: theme.good,
            value: theme.text,
            dirty_value: theme.warn,
            error: theme.error,
        }
    }
}

pub(crate) fn draw_section_header(area: Rect, buf: &mut Buffer, theme: &Theme, label: &str) {
    if area.is_empty() {
        return;
    }
    area.with(theme.status_section | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, label);
}

pub(crate) fn draw_labeled_value(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    label_width: u16,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
) {
    draw_labeled_value_with(
        area,
        buf,
        RowPalette::from_theme(theme),
        label_width,
        label,
        value,
        focused,
        dirty,
        false,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_labeled_value_with(
    area: Rect,
    buf: &mut Buffer,
    palette: RowPalette,
    label_width: u16,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
    error: bool,
) {
    let base = if focused {
        palette.focused
    } else {
        palette.base
    };
    buf.clear_rect(area, base);
    let mut row = area;
    let label_style = if error {
        palette.error
    } else if focused {
        palette.focused_label
    } else {
        palette.label
    };
    row.take_left(label_width as i32)
        .with(base.patch(label_style))
        .with(Ellipsis(true))
        .text(buf, label);
    let value_style = if error {
        palette.error
    } else if dirty {
        palette.dirty_value
    } else {
        palette.value
    };
    row.with(base.patch(value_style))
        .with(Ellipsis(true))
        .text(buf, value);
}

pub(crate) fn draw_labeled_editor_frame(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    label_width: u16,
    label: &str,
    focused: bool,
    error: bool,
) -> Rect {
    let palette = RowPalette::from_theme(theme);
    let base = if focused {
        palette.focused
    } else {
        palette.base
    };
    buf.clear_rect(area, base);
    let mut row = area;
    let label_style = if error {
        palette.error
    } else if focused {
        palette.focused_label
    } else {
        palette.label
    };
    row.take_left(label_width as i32)
        .with(base.patch(label_style))
        .with(Ellipsis(true))
        .text(buf, label);
    let frame_style = if error {
        theme.error
    } else if focused {
        theme.join_input_active
    } else {
        theme.join_input_inactive
    };
    row.with(frame_style).fill(buf);
    row
}

pub(crate) fn draw_action(area: Rect, buf: &mut Buffer, theme: &Theme, label: &str, focused: bool) {
    if area.is_empty() {
        return;
    }
    let style = if focused {
        RowPalette::from_theme(theme).focused.patch(theme.good)
    } else {
        theme.background.patch(theme.text)
    };
    area.with(style)
        .with(HAlign::Center)
        .with(Ellipsis(true))
        .text(buf, label);
}

pub(crate) fn draw_metadata_line(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    base: Style,
    label_width: u16,
    label: &str,
    value: &str,
) {
    if area.is_empty() {
        return;
    }
    let mut row = area;
    row.take_left(label_width as i32)
        .with(base.patch(theme.subtle))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(base.patch(theme.text))
        .with(Ellipsis(true))
        .text(buf, value);
}
