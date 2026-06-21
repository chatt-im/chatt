use extui::{Buffer, HAlign, Rect, Style, vt::Modifier};

use crate::{audio::StatsSnapshot, theme};

const FLOOR_DB: f32 = -60.0;
const LOW_DB: f32 = -36.0;
const GOOD_MIN_DB: f32 = -24.0;
const GOOD_MAX_DB: f32 = -9.0;
const PEAK_DB: f32 = -3.0;

const TRACK: Style = Style::DEFAULT.with_bg_rgb(0x1d, 0x22, 0x29);
const LOW_FILL: Style = Style::DEFAULT.with_bg_rgb(0x3f, 0x5f, 0x75);
const LOW_FG: Style = Style::DEFAULT.with_fg_rgb(0x5d, 0x86, 0xa1);
const GOOD_FILL: Style = Style::DEFAULT.with_bg_rgb(0x45, 0x78, 0x4e);
const GOOD_FG: Style = Style::DEFAULT.with_fg_rgb(0x8f, 0xd0, 0x88);
const WARN_FILL: Style = Style::DEFAULT.with_bg_rgb(0x8a, 0x6a, 0x35);
const WARN_FG: Style = Style::DEFAULT.with_fg_rgb(0xe6, 0xc3, 0x84);
const PEAK_FILL: Style = Style::DEFAULT.with_bg_rgb(0x8a, 0x35, 0x3d);
const PEAK_FG: Style = Style::DEFAULT.with_fg_rgb(0xff, 0x66, 0x6f);

pub fn dbfs(level: f32) -> f32 {
    if level <= f32::EPSILON {
        FLOOR_DB
    } else {
        (20.0 * level.clamp(f32::EPSILON, 1.0).log10()).max(FLOOR_DB)
    }
}

pub fn draw_status_vu(area: Rect, buf: &mut Buffer, capture: Option<&StatsSnapshot>) {
    let (rms, peak) = capture
        .map(|stats| (stats.rms, stats.peak))
        .unwrap_or_default();
    draw_vu_meter(area, buf, rms, peak, theme::STATUS_FILL);
}

pub fn draw_settings_vu_row(
    area: Rect,
    buf: &mut Buffer,
    capture: Option<&StatsSnapshot>,
    focused: bool,
) {
    if area.is_empty() {
        return;
    }

    let base = if focused {
        Style::DEFAULT
            .with_bg_rgb(0x24, 0x28, 0x30)
            .with_fg_rgb(0xd8, 0xdb, 0xd6)
    } else {
        theme::BACKGROUND
    };
    buf.clear_rect(area, base);

    let mut row = area;
    row.take_left(16)
        .with(base.patch(if focused { theme::GOOD } else { theme::MUTED }))
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
    draw_vu_meter(row, buf, rms, peak, base);

    if let Some(value_area) = value_area {
        let (label, style) = match capture {
            Some(_) => {
                let db = dbfs(rms);
                (format!("{db:>5.1} dB"), level_style(base, db, dbfs(peak)))
            }
            None => ("inactive".to_string(), base.patch(theme::MUTED)),
        };
        value_area.with(style).with(HAlign::Right).text(buf, &label);
    }
}

fn draw_vu_meter(area: Rect, buf: &mut Buffer, rms: f32, peak: f32, base: Style) {
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
        let (text, style) = meter_cell(covered, base, zone_db);
        buf.set_stringn(area.x + index as u16, area.y, text, 1, style);
    }

    if peak > rms.max(0.0) {
        let peak_quarters = filled_quarters(peak, width);
        if peak_quarters > 0 {
            let index = ((peak_quarters - 1) / 4).min(width - 1);
            let zone_db = cell_zone_db(index, width);
            let covered = filled.saturating_sub(index * 4).min(4);
            let bg = if covered == 4 {
                fill_bg(zone_db)
            } else {
                TRACK
            };
            let style = base.patch(bg).patch(PEAK_FG).with_modifier(Modifier::BOLD);
            buf.set_stringn(area.x + index as u16, area.y, "▏", 1, style);
        }
    }
}

fn meter_cell(covered_quarters: usize, base: Style, zone_db: f32) -> (&'static str, Style) {
    match covered_quarters {
        0 => (" ", base.patch(TRACK)),
        1 => ("▎", base.patch(TRACK).patch(fill_fg(zone_db))),
        2 => ("▌", base.patch(TRACK).patch(fill_fg(zone_db))),
        3 => ("▊", base.patch(TRACK).patch(fill_fg(zone_db))),
        _ => (" ", base.patch(fill_bg(zone_db))),
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

fn fill_bg(db: f32) -> Style {
    if db >= PEAK_DB {
        PEAK_FILL
    } else if db >= GOOD_MAX_DB {
        WARN_FILL
    } else if db >= GOOD_MIN_DB {
        GOOD_FILL
    } else {
        LOW_FILL
    }
}

fn fill_fg(db: f32) -> Style {
    if db >= PEAK_DB {
        PEAK_FG
    } else if db >= GOOD_MAX_DB {
        WARN_FG
    } else if db >= GOOD_MIN_DB {
        GOOD_FG
    } else {
        LOW_FG
    }
}

fn level_style(base: Style, rms_db: f32, peak_db: f32) -> Style {
    if peak_db >= PEAK_DB {
        base.patch(theme::ERROR).with_modifier(Modifier::BOLD)
    } else if rms_db < LOW_DB {
        base.patch(LOW_FG)
    } else if (GOOD_MIN_DB..=GOOD_MAX_DB).contains(&rms_db) {
        base.patch(theme::GOOD)
    } else {
        base.patch(theme::WARN)
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
        assert_eq!(meter_cell(1, Style::DEFAULT, -30.0).0, "▎");
        assert_eq!(meter_cell(2, Style::DEFAULT, -30.0).0, "▌");
        assert_eq!(meter_cell(3, Style::DEFAULT, -30.0).0, "▊");
        assert_eq!(meter_cell(4, Style::DEFAULT, -30.0).0, " ");
    }
}
