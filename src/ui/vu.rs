use std::time::Instant;

use extui::{Buffer, HAlign, Rect, Style, vt::Modifier};

use crate::{audio::StatsSnapshot, theme::Theme};

const FLOOR_DB: f32 = -60.0;
const LOW_DB: f32 = -36.0;
const GOOD_MIN_DB: f32 = -24.0;
const GOOD_MAX_DB: f32 = -9.0;
const PEAK_DB: f32 = -3.0;

/// Rise time constant for the RMS bar and dB number: short so real speech
/// onsets light the meter almost immediately.
const RMS_ATTACK_TAU_S: f32 = 0.02;
/// Fall time constant for the RMS bar and dB number: long enough to bridge the
/// brief silences RNNoise punches into faint background noise, so the meter
/// decays smoothly instead of flickering to the floor between dropouts.
const RMS_RELEASE_TAU_S: f32 = 0.110;
/// Decay time constant for the peak marker once the incoming peak drops below
/// the held value; the peak still jumps up instantly.
const PEAK_DECAY_TAU_S: f32 = 0.375;
/// Gap beyond which the filter is treated as freshly started and adopts the raw
/// values directly, rather than integrating one enormous `dt`.
const RESYNC_GAP_S: f32 = 1.0;

/// Fast-attack, slow-release ballistics for the mic level display. Holds the
/// smoothed `rms`/`peak` between frames so faint noise gated on and off by
/// noise reduction reads as a steady level instead of sporadic flicker. Purely
/// a display filter: the stored capture levels are untouched.
#[derive(Default)]
pub(crate) struct MicLevelBallistics {
    rms: f32,
    peak: f32,
    last: Option<Instant>,
}

impl MicLevelBallistics {
    /// Advances the filter with the raw `rms`/`peak` from the latest capture
    /// snapshot and returns the smoothed pair to display. The first call, or
    /// one after a [`RESYNC_GAP_S`] gap, adopts the raw values so the meter does
    /// not crawl up from zero on stream start.
    pub(crate) fn smooth(&mut self, rms: f32, peak: f32, now: Instant) -> (f32, f32) {
        let dt = match self.last {
            Some(last) => now.saturating_duration_since(last).as_secs_f32(),
            None => f32::INFINITY,
        };
        self.last = Some(now);

        if !(dt < RESYNC_GAP_S) {
            self.rms = rms;
            self.peak = peak;
            return (rms, peak);
        }

        let rms_tau = if rms > self.rms {
            RMS_ATTACK_TAU_S
        } else {
            RMS_RELEASE_TAU_S
        };
        self.rms += (rms - self.rms) * coefficient(dt, rms_tau);

        if peak >= self.peak {
            self.peak = peak;
        } else {
            self.peak += (peak - self.peak) * coefficient(dt, PEAK_DECAY_TAU_S);
        }

        (self.rms, self.peak)
    }

    /// Clears the held state so the next [`Self::smooth`] starts fresh. Called
    /// when capture stops, so a later resume does not decay a stale level.
    pub(crate) fn reset(&mut self) {
        self.rms = 0.0;
        self.peak = 0.0;
        self.last = None;
    }
}

/// Frame-rate-independent smoothing weight for a step of `dt` seconds toward a
/// target with time constant `tau`.
fn coefficient(dt: f32, tau: f32) -> f32 {
    1.0 - (-dt / tau).exp()
}

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
    use std::time::Duration;

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
    fn first_sample_adopts_raw() {
        let mut ballistics = MicLevelBallistics::default();
        let now = Instant::now();
        assert_eq!(ballistics.smooth(0.4, 0.6, now), (0.4, 0.6));
    }

    #[test]
    fn attack_rises_quickly() {
        let mut ballistics = MicLevelBallistics::default();
        let start = Instant::now();
        ballistics.smooth(0.0, 0.0, start);
        // One 10 ms frame with a loud target reaches most of the way there.
        let (rms, _) = ballistics.smooth(1.0, 1.0, start + Duration::from_millis(10));
        assert!(rms > 0.15, "attack too slow: {rms}");
    }

    #[test]
    fn release_bridges_dropouts() {
        let mut ballistics = MicLevelBallistics::default();
        let start = Instant::now();
        // Settle at a steady level, then a single frame of NR-induced silence.
        let mut now = start;
        ballistics.smooth(0.5, 0.5, now);
        for _ in 0..50 {
            now += Duration::from_millis(10);
            ballistics.smooth(0.5, 0.5, now);
        }
        now += Duration::from_millis(10);
        let (rms, _) = ballistics.smooth(0.0, 0.0, now);
        assert!(rms > 0.4, "release collapsed on a single dropout: {rms}");
    }

    #[test]
    fn peak_holds_then_decays() {
        let mut ballistics = MicLevelBallistics::default();
        let start = Instant::now();
        ballistics.smooth(0.0, 0.0, start);
        // A spike is adopted instantly.
        let (_, peak) = ballistics.smooth(0.0, 0.9, start + Duration::from_millis(10));
        assert_eq!(peak, 0.9);
        // A following lower reading decays rather than snapping down.
        let (_, peak) = ballistics.smooth(0.0, 0.1, start + Duration::from_millis(20));
        assert!(
            peak > 0.1 && peak < 0.9,
            "peak did not decay smoothly: {peak}"
        );
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
