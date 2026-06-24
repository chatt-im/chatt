use extui::{Buffer, HAlign, Rect, Style, vt::Modifier};

use crate::{audio::StatsSnapshot, theme::Theme};

const FLOOR_DB: f32 = -60.0;
const LOW_DB: f32 = -36.0;
const GOOD_MIN_DB: f32 = -24.0;
const GOOD_MAX_DB: f32 = -9.0;
const PEAK_DB: f32 = -3.0;

pub fn dbfs(level: f32) -> f32 {
    if level <= f32::EPSILON {
        FLOOR_DB
    } else {
        (20.0 * level.clamp(f32::EPSILON, 1.0).log10()).max(FLOOR_DB)
    }
}

pub fn draw_status_vu(
    area: Rect,
    buf: &mut Buffer,
    capture: Option<&StatsSnapshot>,
    theme: &Theme,
) {
    let (rms, peak) = capture
        .map(|stats| (stats.rms, stats.peak))
        .unwrap_or_default();
    draw_vu_meter(area, buf, rms, peak, theme.status_fill, theme);
}

pub fn draw_settings_vu_row(
    area: Rect,
    buf: &mut Buffer,
    capture: Option<&StatsSnapshot>,
    focused: bool,
    theme: &Theme,
) {
    if area.is_empty() {
        return;
    }

    let base = if focused {
        theme.row_focused
    } else {
        theme.background
    };
    buf.clear_rect(area, base);

    let mut row = area;
    row.take_left(16)
        .with(base.patch(if focused { theme.good } else { theme.muted }))
        .text(buf, "Mic Level");

    let value_width = if row.w >= 28 { 11 } else { 0 };
    let value_area = if value_width > 0 {
        Some(row.take_right(value_width as i32))
    } else {
        None
    };

    let (rms, peak) = capture
        .map(|stats| (stats.rms, stats.peak))
        .unwrap_or_default();
    draw_vu_meter(row, buf, rms, peak, base, theme);

    if let Some(value_area) = value_area {
        let (label, style) = match capture {
            Some(_) => {
                let db = dbfs(rms);
                (
                    format!("{db:>5.1} dB"),
                    level_style(base, db, dbfs(peak), theme),
                )
            }
            None => ("inactive".to_string(), base.patch(theme.muted)),
        };
        value_area.with(style).with(HAlign::Right).text(buf, &label);
    }
}

fn draw_vu_meter(area: Rect, buf: &mut Buffer, rms: f32, peak: f32, base: Style, theme: &Theme) {
    if area.is_empty() {
        return;
    }

    let width = area.w as usize;
    if width == 0 {
        return;
    }

    let filled = filled_quarters(rms, width);
    for index in 0..width {
        let zone_db = cell_zone_db(index, width);
        let covered = filled.saturating_sub(index * 4).min(4);
        let (text, style) = meter_cell(covered, base, zone_db, theme);
        buf.set_stringn(area.x + index as u16, area.y, text, 1, style);
    }

    if peak > rms.max(0.0) {
        let peak_quarters = filled_quarters(peak, width);
        if peak_quarters > 0 {
            let index = ((peak_quarters - 1) / 4).min(width - 1);
            let zone_db = cell_zone_db(index, width);
            let covered = filled.saturating_sub(index * 4).min(4);
            let bg = if covered == 4 {
                fill_bg(zone_db, theme)
            } else {
                theme.vu_track
            };
            let style = base
                .patch(bg)
                .patch(theme.vu_peak_fg)
                .with_modifier(Modifier::BOLD);
            buf.set_stringn(area.x + index as u16, area.y, "▏", 1, style);
        }
    }
}

fn meter_cell(
    covered_quarters: usize,
    base: Style,
    zone_db: f32,
    theme: &Theme,
) -> (&'static str, Style) {
    let track = theme.vu_track;
    match covered_quarters {
        0 => (" ", base.patch(track)),
        1 => ("▎", base.patch(track).patch(fill_fg(zone_db, theme))),
        2 => ("▌", base.patch(track).patch(fill_fg(zone_db, theme))),
        3 => ("▊", base.patch(track).patch(fill_fg(zone_db, theme))),
        _ => (" ", base.patch(fill_bg(zone_db, theme))),
    }
}

fn filled_quarters(level: f32, width: usize) -> usize {
    let normalized = ((dbfs(level) - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
    (normalized * width as f32 * 4.0).round() as usize
}

fn cell_zone_db(index: usize, width: usize) -> f32 {
    let right_edge = (index + 1) as f32 / width.max(1) as f32;
    FLOOR_DB + (-FLOOR_DB * right_edge)
}

fn fill_bg(db: f32, theme: &Theme) -> Style {
    if db >= PEAK_DB {
        theme.vu_peak_fill
    } else if db >= GOOD_MAX_DB {
        theme.vu_warn_fill
    } else if db >= GOOD_MIN_DB {
        theme.vu_good_fill
    } else {
        theme.vu_low_fill
    }
}

fn fill_fg(db: f32, theme: &Theme) -> Style {
    if db >= PEAK_DB {
        theme.vu_peak_fg
    } else if db >= GOOD_MAX_DB {
        theme.vu_warn_fg
    } else if db >= GOOD_MIN_DB {
        theme.vu_good_fg
    } else {
        theme.vu_low_fg
    }
}

fn level_style(base: Style, rms_db: f32, peak_db: f32, theme: &Theme) -> Style {
    if peak_db >= PEAK_DB {
        base.patch(theme.error).with_modifier(Modifier::BOLD)
    } else if rms_db < LOW_DB {
        base.patch(theme.vu_low_fg)
    } else if (GOOD_MIN_DB..=GOOD_MAX_DB).contains(&rms_db) {
        base.patch(theme.good)
    } else {
        base.patch(theme.warn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbfs_clamps_silence_to_floor() {
        assert_eq!(dbfs(0.0), FLOOR_DB);
        assert_eq!(dbfs(-1.0), FLOOR_DB);
    }

    #[test]
    fn filled_quarters_maps_db_range_to_meter_width() {
        assert_eq!(filled_quarters(0.0, 10), 0);
        assert_eq!(filled_quarters(1.0, 10), 40);
        assert_eq!(filled_quarters(10.0_f32.powf(-30.0 / 20.0), 10), 20);
    }

    #[test]
    fn meter_cells_use_quarter_block_precision() {
        let theme = Theme::tomorrow_night();
        assert_eq!(meter_cell(1, Style::DEFAULT, -30.0, &theme).0, "▎");
        assert_eq!(meter_cell(2, Style::DEFAULT, -30.0, &theme).0, "▌");
        assert_eq!(meter_cell(3, Style::DEFAULT, -30.0, &theme).0, "▊");
        assert_eq!(meter_cell(4, Style::DEFAULT, -30.0, &theme).0, " ");
    }
}
