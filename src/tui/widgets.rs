use extui::{Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};

use crate::theme::Theme;

pub(crate) const SCROLLBAR_UNITS_PER_CELL: u32 = 8;
pub(crate) const SCROLLBAR_MIN_THUMB_UNITS: u32 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollbarState {
    pub(crate) total: u32,
    pub(crate) viewport: u32,
    pub(crate) offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollbarLayout {
    pub(crate) rect: Rect,
    pub(crate) state: ScrollbarState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollbarGeometry {
    pub(crate) track_units: u32,
    pub(crate) thumb_start: u32,
    pub(crate) thumb_units: u32,
    pub(crate) travel: u32,
    pub(crate) max_scroll: u32,
}

impl ScrollbarGeometry {
    pub(crate) fn new(height: u16, state: ScrollbarState) -> Option<Self> {
        if height == 0 || state.viewport >= state.total {
            return None;
        }
        let track_units = u32::from(height) * SCROLLBAR_UNITS_PER_CELL;
        let proportional =
            ((u64::from(track_units) * u64::from(state.viewport)) / u64::from(state.total)) as u32;
        let thumb_units = proportional.max(SCROLLBAR_MIN_THUMB_UNITS).min(track_units);
        let travel = track_units.saturating_sub(thumb_units);
        let max_scroll = state.total.saturating_sub(state.viewport);
        let offset = state.offset.min(max_scroll);
        let mut thumb_start = if offset == 0 || max_scroll == 0 {
            0
        } else if offset == max_scroll {
            travel
        } else {
            ((u64::from(travel) * u64::from(offset)) / u64::from(max_scroll)) as u32
        };
        if offset > 0 && offset < max_scroll && travel >= 2 {
            thumb_start = thumb_start.clamp(1, travel - 1);
        }
        Some(Self {
            track_units,
            thumb_start,
            thumb_units,
            travel,
            max_scroll,
        })
    }

    pub(crate) fn offset_for_thumb_start(self, thumb_start: u32) -> u32 {
        if self.travel == 0 {
            return 0;
        }
        let thumb_start = thumb_start.min(self.travel);
        ((u64::from(thumb_start) * u64::from(self.max_scroll) + u64::from(self.travel / 2))
            / u64::from(self.travel)) as u32
    }
}

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

    pub(crate) fn dialog(theme: &Theme) -> Self {
        let mut palette = Self::from_theme(theme);
        palette.base = theme.dialog_panel;
        palette
    }
}

pub(crate) fn draw_section_header(area: Rect, buf: &mut Buffer, theme: &Theme, label: &str) {
    area.with(theme.status_section | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, label);
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
    palette: RowPalette,
    label_width: u16,
    label: &str,
    focused: bool,
    error: bool,
) -> Rect {
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

pub(crate) fn draw_action(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    label: &str,
    focused: bool,
    dialog: bool,
) {
    let style = if dialog {
        let style = if focused {
            theme.selected_focused
        } else {
            theme.dialog_panel.patch(theme.muted)
        };
        if focused {
            style | Modifier::BOLD
        } else {
            style
        }
    } else if focused {
        theme.background.patch(theme.good | Modifier::BOLD)
    } else {
        theme.background.patch(theme.muted)
    };
    draw_button(area, buf, style, label);
}

pub(crate) fn button_label(label: &str, key: Option<String>) -> String {
    match key {
        Some(key) => format!("{label} [{key}]"),
        None => label.to_string(),
    }
}

pub(crate) fn draw_button(area: Rect, buf: &mut Buffer, style: Style, label: &str) {
    area.with(style).fill(buf);
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
    let mut row = area;
    row.take_left(label_width as i32)
        .with(base.patch(theme.subtle))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(base.patch(theme.text))
        .with(Ellipsis(true))
        .text(buf, value);
}
