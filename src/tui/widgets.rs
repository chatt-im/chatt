use extui::{Buffer, Cell, Ellipsis, HAlign, Rect, Style, vt::Modifier};

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
    pub(crate) id: ScrollbarId,
    pub(crate) rect: Rect,
    pub(crate) state: ScrollbarState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollbarId {
    Rooms,
    Compose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollbarDrag {
    pub(crate) id: ScrollbarId,
    grab_units: u32,
    pub(crate) current_offset: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollbarPress {
    pub(crate) drag: ScrollbarDrag,
    /// Track presses jump immediately. Thumb presses preserve the current
    /// offset until the pointer moves.
    pub(crate) target: Option<u32>,
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

impl ScrollbarLayout {
    /// Includes the cell immediately left of the painted gutter. Some
    /// terminals and multiplexers report a right-edge click one cell inward;
    /// keeping this policy here makes every scrollbar behave identically.
    pub(crate) fn contains(self, column: u16, row: u16) -> bool {
        !self.rect.is_empty()
            && row >= self.rect.y
            && row < self.rect.y.saturating_add(self.rect.h)
            && (column == self.rect.x || column.saturating_add(1) == self.rect.x)
    }

    pub(crate) fn press(self, column: u16, row: u16) -> Option<ScrollbarPress> {
        if !self.contains(column, row) {
            return None;
        }
        let geometry = ScrollbarGeometry::new(self.rect.h, self.state)?;
        let pointer = self.pointer_units(row);
        let cell_start = pointer.saturating_sub(SCROLLBAR_UNITS_PER_CELL / 2);
        let cell_end = cell_start + SCROLLBAR_UNITS_PER_CELL;
        let thumb_end = geometry.thumb_start + geometry.thumb_units;
        let on_thumb = cell_start < thumb_end && cell_end > geometry.thumb_start;
        let grab_units = if on_thumb {
            pointer
                .saturating_sub(geometry.thumb_start)
                .min(geometry.thumb_units)
        } else {
            geometry.thumb_units / 2
        };
        let drag = ScrollbarDrag {
            id: self.id,
            grab_units,
            current_offset: self.state.offset.min(geometry.max_scroll),
        };
        let target = (!on_thumb).then(|| self.target_for_pointer(geometry, drag, row));
        Some(ScrollbarPress { drag, target })
    }

    pub(crate) fn drag_target(self, drag: ScrollbarDrag, row: u16) -> Option<u32> {
        if drag.id != self.id {
            return None;
        }
        let geometry = ScrollbarGeometry::new(self.rect.h, self.state)?;
        Some(self.target_for_pointer(geometry, drag, row))
    }

    fn pointer_units(self, row: u16) -> u32 {
        let bottom = self.rect.y + self.rect.h.saturating_sub(1);
        let row = row.clamp(self.rect.y, bottom).saturating_sub(self.rect.y);
        u32::from(row) * SCROLLBAR_UNITS_PER_CELL + SCROLLBAR_UNITS_PER_CELL / 2
    }

    fn target_for_pointer(self, geometry: ScrollbarGeometry, drag: ScrollbarDrag, row: u16) -> u32 {
        let bottom = self.rect.y + self.rect.h.saturating_sub(1);
        let thumb_start = if row <= self.rect.y {
            0
        } else if row >= bottom {
            geometry.travel
        } else {
            self.pointer_units(row)
                .saturating_sub(drag.grab_units)
                .min(geometry.travel)
        };
        geometry.offset_for_thumb_start(thumb_start)
    }
}

/// Draws a themed scrollbar in eighth-cell virtual units. The theme's
/// foreground is the thumb and its background is the gutter.
pub(crate) fn draw_scrollbar(layout: ScrollbarLayout, theme: Theme, buf: &mut Buffer) {
    if layout.rect.is_empty() {
        return;
    }
    let Some(geometry) = ScrollbarGeometry::new(layout.rect.h, layout.state) else {
        return;
    };

    const BLOCKS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let style = theme.scrollbar;
    let x = layout.rect.x + layout.rect.w - 1;
    let thumb_end = geometry.thumb_start + geometry.thumb_units;
    let blank = Cell::new_unchecked(" ", style);

    for row in 0..layout.rect.h {
        let y = layout.rect.y + row;
        let cell_start = u32::from(row) * SCROLLBAR_UNITS_PER_CELL;
        let cell_end = cell_start + SCROLLBAR_UNITS_PER_CELL;
        let overlap_start = cell_start.max(geometry.thumb_start);
        let overlap_end = cell_end.min(thumb_end);
        let coverage = overlap_end.saturating_sub(overlap_start);
        let cell = match coverage {
            0 => blank,
            SCROLLBAR_UNITS_PER_CELL => Cell::new_unchecked(BLOCKS[8], style),
            coverage if overlap_start == cell_start => {
                let uncovered_at_bottom = (SCROLLBAR_UNITS_PER_CELL - coverage) as usize;
                Cell::new_unchecked(BLOCKS[uncovered_at_bottom], style)
                    .with_style_merged(Modifier::REVERSED.into())
            }
            coverage => Cell::new_unchecked(BLOCKS[coverage as usize], style),
        };
        buf.set_cell(x, y, cell);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(state: ScrollbarState) -> ScrollbarLayout {
        ScrollbarLayout {
            id: ScrollbarId::Rooms,
            rect: Rect {
                x: 10,
                y: 5,
                w: 1,
                h: 8,
            },
            state,
        }
    }

    #[test]
    fn geometry_has_seven_eighth_minimum_and_clamps_state() {
        let geometry = ScrollbarGeometry::new(
            2,
            ScrollbarState {
                total: 10_000,
                viewport: 1,
                offset: u32::MAX,
            },
        )
        .unwrap();
        assert_eq!(geometry.thumb_units, SCROLLBAR_MIN_THUMB_UNITS);
        assert_eq!(geometry.thumb_start, geometry.travel);
        assert_eq!(
            geometry.offset_for_thumb_start(u32::MAX),
            geometry.max_scroll
        );
    }

    #[test]
    fn proportional_mapping_and_endpoints_cover_complete_scroll_range() {
        let geometry = ScrollbarGeometry::new(
            8,
            ScrollbarState {
                total: 100,
                viewport: 20,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(geometry.offset_for_thumb_start(0), 0);
        assert_eq!(geometry.offset_for_thumb_start(geometry.travel), 80);
        assert!((35..=45).contains(&geometry.offset_for_thumb_start(geometry.travel / 2)));
    }

    #[test]
    fn hit_testing_uses_shared_adjacent_cell_policy() {
        let scrollbar = layout(ScrollbarState {
            total: 10,
            viewport: 2,
            offset: 0,
        });
        assert!(scrollbar.contains(10, 5));
        assert!(scrollbar.contains(9, 12));
        assert!(!scrollbar.contains(8, 5));
        assert!(!scrollbar.contains(10, 13));
    }

    #[test]
    fn thumb_press_does_not_jump_and_track_press_snaps_to_end() {
        let scrollbar = layout(ScrollbarState {
            total: 100,
            viewport: 20,
            offset: 0,
        });
        let thumb = scrollbar.press(10, 5).unwrap();
        assert_eq!(thumb.target, None);
        let track = scrollbar.press(10, 12).unwrap();
        assert_eq!(track.target, Some(80));
        assert_eq!(scrollbar.drag_target(thumb.drag, 12), Some(80));
    }

    #[test]
    fn drag_ownership_must_match_layout() {
        let scrollbar = layout(ScrollbarState {
            total: 10,
            viewport: 2,
            offset: 0,
        });
        let mut drag = scrollbar.press(10, 5).unwrap().drag;
        drag.id = ScrollbarId::Compose;
        assert_eq!(scrollbar.drag_target(drag, 12), None);
    }
}
