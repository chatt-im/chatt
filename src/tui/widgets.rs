use extui::{Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};

use crate::theme;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RowPalette {
    pub(crate) base: Style,
    pub(crate) focused: Style,
    pub(crate) label: Style,
    pub(crate) focused_label: Style,
    pub(crate) value: Style,
    pub(crate) dirty_value: Style,
}

impl Default for RowPalette {
    fn default() -> Self {
        Self {
            base: theme::BACKGROUND,
            focused: Style::DEFAULT
                .with_bg_rgb(0x24, 0x28, 0x30)
                .with_fg_rgb(0xd8, 0xdb, 0xd6),
            label: theme::MUTED,
            focused_label: theme::GOOD,
            value: theme::TEXT,
            dirty_value: theme::WARN,
        }
    }
}

pub(crate) fn draw_section_header(area: Rect, buf: &mut Buffer, label: &str) {
    if area.is_empty() {
        return;
    }
    area.with(theme::STATUS_SECTION | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, label);
}

pub(crate) fn draw_labeled_value(
    area: Rect,
    buf: &mut Buffer,
    label_width: u16,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
) {
    draw_labeled_value_with(
        area,
        buf,
        RowPalette::default(),
        label_width,
        label,
        value,
        focused,
        dirty,
    );
}

pub(crate) fn draw_labeled_value_with(
    area: Rect,
    buf: &mut Buffer,
    palette: RowPalette,
    label_width: u16,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
) {
    let base = if focused {
        palette.focused
    } else {
        palette.base
    };
    buf.clear_rect(area, base);
    let mut row = area;
    row.take_left(label_width as i32)
        .with(base.patch(if focused {
            palette.focused_label
        } else {
            palette.label
        }))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(base.patch(if dirty {
        palette.dirty_value
    } else {
        palette.value
    }))
    .with(Ellipsis(true))
    .text(buf, value);
}

pub(crate) fn draw_labeled_editor_frame(
    area: Rect,
    buf: &mut Buffer,
    label_width: u16,
    label: &str,
    focused: bool,
) -> Rect {
    let palette = RowPalette::default();
    let base = if focused {
        palette.focused
    } else {
        palette.base
    };
    buf.clear_rect(area, base);
    let mut row = area;
    row.take_left(label_width as i32)
        .with(base.patch(if focused {
            palette.focused_label
        } else {
            palette.label
        }))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(if focused {
        theme::JOIN_INPUT_ACTIVE
    } else {
        theme::JOIN_INPUT_INACTIVE
    })
    .fill(buf);
    row
}

pub(crate) fn draw_action(area: Rect, buf: &mut Buffer, label: &str, focused: bool) {
    if area.is_empty() {
        return;
    }
    let style = if focused {
        RowPalette::default().focused.patch(theme::GOOD)
    } else {
        theme::BACKGROUND.patch(theme::TEXT)
    };
    area.with(style)
        .with(HAlign::Center)
        .with(Ellipsis(true))
        .text(buf, label);
}

pub(crate) fn draw_metadata_line(
    area: Rect,
    buf: &mut Buffer,
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
        .with(base.patch(theme::SUBTLE))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(base.patch(theme::TEXT))
        .with(Ellipsis(true))
        .text(buf, value);
}
