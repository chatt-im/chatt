use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use extui::{Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};
use unicode_width::UnicodeWidthStr;

use chatt::audio::StatsSnapshot;

use crate::{
    app::{App, ParticipantState, ServerSelectItem, StatusKind, volume_db_label},
    theme, ui,
};

const NAME_COL_WIDTH: u16 = 16;
const ROOM_SELECTED: Style = Style::DEFAULT
    .with_bg_rgb(0x24, 0x28, 0x30)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);

pub(crate) fn render(app: &mut App, buf: &mut Buffer) {
    buf.rect().with(theme::BACKGROUND).fill(buf);
    buf.hide_cursor();
    let capture = app
        .capture
        .as_ref()
        .map(|capture| capture.stats().snapshot());

    let mut screen = buf.rect();
    if matches!(
        app.mode,
        theme::UiMode::ServerSelect | theme::UiMode::ServerEdit
    ) {
        let status_area = screen.take_bottom(1);
        match app.mode {
            theme::UiMode::ServerSelect => draw_server_select(screen, app, buf),
            theme::UiMode::ServerEdit => draw_server_edit(screen, app, buf),
            _ => {}
        }
        draw_status(status_area, app, buf, capture.as_ref());
        return;
    }

    let composer_height = composer_height(app, screen.w);
    let composer_area = screen.take_bottom(composer_height as i32);
    let status_area = screen.take_bottom(1);
    let room_height = app.config.ui.room_height.min(screen.h.saturating_sub(1));
    if room_height > 0 {
        let room_area = screen.take_top(room_height as i32);
        draw_room(room_area, app, buf);
    }
    if screen.h > 0 {
        let title_area = screen.take_top(1);
        draw_room_title(title_area, app, buf);
    }

    match app.mode {
        theme::UiMode::Settings => ui::settings::draw_settings(
            screen,
            buf,
            &mut app.settings,
            app.settings_focus,
            app.settings_dirty,
            capture.as_ref(),
            &app.audio_input_items,
            &mut app.audio_input_picker,
            &app.audio_output_items,
            &mut app.audio_output_picker,
        ),
        theme::UiMode::Compose | theme::UiMode::Log => draw_chat(screen, app, buf),
        theme::UiMode::ServerSelect | theme::UiMode::ServerEdit => {}
    }
    draw_status(status_area, app, buf, capture.as_ref());
    draw_composer(composer_area, app, buf);
    draw_volume_dialog(buf.rect(), app, buf);
}

fn composer_height(app: &mut App, width: u16) -> u16 {
    app.composer.resize(width.max(1));
    app.composer
        .desired_height()
        .min(app.config.ui.max_composer_height.max(1))
        .max(1)
}

fn draw_room(area: Rect, app: &App, buf: &mut Buffer) {
    area.with(theme::PANEL_ALT).fill(buf);
    let mut rows = area;
    let visible = rows.h as usize;
    let start = app.participants.scroll.min(app.participants.entries.len());
    for participant in app.participants.entries.iter().skip(start).take(visible) {
        let row = rows.take_top(1);
        let selected = Some(participant.user_id) == app.participants.selected_user;
        let state =
            if Some(participant.user_id) == app.user_id && app.deafened.load(Ordering::Relaxed) {
                "deaf"
            } else if participant.online && Some(participant.user_id) == app.user_id {
                "voice"
            } else if participant.online && participant.p2p_direct {
                "p2p"
            } else if participant.online {
                "relay"
            } else {
                "away"
            };
        let spoke = participant
            .last_voice_at
            .map(age_label)
            .or_else(|| participant.last_message_ms.map(|_| "msg".to_string()))
            .unwrap_or_else(|| "--".to_string());
        let base = if selected {
            ROOM_SELECTED
        } else {
            theme::PANEL_ALT
        };
        let style = if selected {
            base.patch(theme::GOOD)
        } else if participant.online {
            theme::TEXT
        } else {
            theme::MUTED
        };
        let marker = if selected { ">" } else { " " };
        let control = room_user_control_label(app, participant);
        let voice = room_user_voice_feedback_label(participant);
        row.with(base).fill(buf);
        row.with(style).with(Ellipsis(true)).text(
            buf,
            &format!(
                "{marker} {:<16} {:<7} {:<5} {:<16} {}",
                participant.name, state, spoke, voice, control
            ),
        );
    }
}

fn draw_server_select(area: Rect, app: &mut App, buf: &mut Buffer) {
    area.with(theme::BACKGROUND).fill(buf);
    let mut rows = area;
    rows.take_top(1)
        .with(theme::STATUS_SECTION | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, " SERVERS ");

    let mut body = rows;
    if body.h == 0 {
        return;
    }
    if app.server_items.is_empty() {
        draw_server_welcome(body, buf);
        return;
    }

    let search = body.take_top(1);
    let search_label = if app.server_select_searching {
        format!("/{}", app.server_select.query())
    } else {
        "Press / to search   Enter join   e edit   d delete   F2 audio   Ctrl-C quit".to_string()
    };
    search
        .with(theme::BACKGROUND.patch(theme::SUBTLE))
        .with(Ellipsis(true))
        .text(buf, &search_label);

    let items = &app.server_items;
    app.server_select
        .render(body, 3, buf, |_, item_index, selected, area, buf| {
            if let Some(item) = items.get(item_index) {
                draw_server_select_item(area, buf, item, selected);
            }
        });
}

enum ServerWelcomeLine {
    Text(&'static str),
    Section(&'static str),
    Binding {
        key: &'static str,
        desc: &'static str,
    },
    Binding3 {
        key1: &'static str,
        key2: &'static str,
        key3: &'static str,
        desc: &'static str,
    },
    Empty,
}

const SERVER_WELCOME_KEY_WIDTH: usize = 15;
const SERVER_WELCOME_COLUMN_GAP: usize = 4;

fn draw_server_welcome(area: Rect, buf: &mut Buffer) {
    use ServerWelcomeLine::*;

    let header = [
        Section("Welcome to Chatt"),
        Empty,
        Text("No servers are configured yet."),
        Empty,
    ];
    let quick_start = [
        Section("Quick start:"),
        Binding {
            key: "chatt join",
            desc: "Pair with a server from a join string",
        },
        Binding {
            key: "F2",
            desc: "Audio settings",
        },
    ];
    let server_actions = [
        Section("Server actions:"),
        Binding {
            key: "Enter",
            desc: "Join selected",
        },
        Binding {
            key: "e",
            desc: "Edit selected",
        },
        Binding {
            key: "d",
            desc: "Delete selected",
        },
        Binding {
            key: "/",
            desc: "Search servers",
        },
    ];
    let navigation = [
        Section("Navigation:"),
        Binding3 {
            key1: "j",
            key2: "k",
            key3: "Arrows",
            desc: "Move through the server list",
        },
        Empty,
        Binding {
            key: "Ctrl-C",
            desc: "Quit",
        },
    ];
    let notes = [
        Section("Configuration:"),
        Text(" Servers are saved in chatt.toml."),
        Text(" Pairing is one-shot; reconnects authenticate normally."),
    ];

    let left_width = quick_start
        .iter()
        .chain(&server_actions)
        .map(server_welcome_line_width)
        .max()
        .unwrap_or(0);
    let right_width = navigation
        .iter()
        .chain(&notes)
        .map(server_welcome_line_width)
        .max()
        .unwrap_or(0);
    let two_col_width = left_width + SERVER_WELCOME_COLUMN_GAP + right_width;

    if area.w as usize >= two_col_width + 4 {
        let mut left = Vec::new();
        left.extend(quick_start);
        left.push(Empty);
        left.extend(server_actions);
        let mut right = Vec::new();
        right.extend(navigation);
        right.push(Empty);
        right.extend(notes);

        let body_height = left.len().max(right.len());
        let content_height = header.len() + body_height;
        let start_y = area.y + area.h.saturating_sub(content_height as u16) / 2;
        let left_x = area.x + area.w.saturating_sub(two_col_width as u16) / 2;
        let right_x = left_x + left_width as u16 + SERVER_WELCOME_COLUMN_GAP as u16;

        for (index, line) in header.iter().enumerate() {
            draw_server_welcome_line(buf, left_x, start_y + index as u16, line);
        }
        let body_y = start_y + header.len() as u16;
        for index in 0..body_height {
            let y = body_y + index as u16;
            if let Some(line) = left.get(index) {
                draw_server_welcome_line(buf, left_x, y, line);
            }
            if let Some(line) = right.get(index) {
                draw_server_welcome_line(buf, right_x, y, line);
            }
        }
    } else {
        let mut lines = Vec::new();
        lines.extend(header);
        lines.extend(quick_start);
        lines.push(Empty);
        lines.extend(navigation);
        lines.push(Empty);
        lines.extend(server_actions);
        lines.push(Empty);
        lines.extend(notes);

        let content_height = lines.len() as u16;
        let start_y = area.y + area.h.saturating_sub(content_height) / 2;
        let max_width = lines
            .iter()
            .map(server_welcome_line_width)
            .max()
            .unwrap_or(0) as u16;
        let x = area.x + area.w.saturating_sub(max_width) / 2;
        for (index, line) in lines.iter().enumerate() {
            draw_server_welcome_line(buf, x, start_y + index as u16, line);
        }
    }
}

fn server_welcome_line_width(line: &ServerWelcomeLine) -> usize {
    match line {
        ServerWelcomeLine::Text(text) | ServerWelcomeLine::Section(text) => text.width(),
        ServerWelcomeLine::Binding { desc, .. } => 2 + SERVER_WELCOME_KEY_WIDTH + desc.width(),
        ServerWelcomeLine::Binding3 {
            key1,
            key2,
            key3,
            desc,
        } => {
            let keys = key1.width() + 1 + key2.width() + 1 + key3.width();
            2 + SERVER_WELCOME_KEY_WIDTH.max(keys) + desc.width()
        }
        ServerWelcomeLine::Empty => 0,
    }
}

fn draw_server_welcome_line(buf: &mut Buffer, x: u16, y: u16, line: &ServerWelcomeLine) {
    let section_style = theme::BACKGROUND.patch(theme::TEXT | Modifier::BOLD);
    let text_style = theme::BACKGROUND.patch(theme::MUTED);
    let key_style = theme::BACKGROUND.patch(theme::ACCENT);

    match line {
        ServerWelcomeLine::Text(text) => draw_text_at(buf, x, y, text, text_style),
        ServerWelcomeLine::Section(text) => draw_text_at(buf, x, y, text, section_style),
        ServerWelcomeLine::Binding { key, desc } => {
            draw_text_at(buf, x, y, "  ", text_style);
            draw_text_at(buf, x.saturating_add(2), y, key, key_style);
            let desc_x = x.saturating_add(2 + SERVER_WELCOME_KEY_WIDTH as u16);
            draw_text_at(buf, desc_x, y, desc, text_style);
        }
        ServerWelcomeLine::Binding3 {
            key1,
            key2,
            key3,
            desc,
        } => {
            draw_text_at(buf, x, y, "  ", text_style);
            let keys = format!("{key1} {key2} {key3}");
            draw_text_at(buf, x.saturating_add(2), y, &keys, key_style);
            let desc_x = x.saturating_add(2 + SERVER_WELCOME_KEY_WIDTH as u16);
            draw_text_at(buf, desc_x, y, desc, text_style);
        }
        ServerWelcomeLine::Empty => {}
    }
}

fn draw_text_at(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style) {
    let area = Rect {
        x,
        y,
        w: text.width().min(u16::MAX as usize) as u16,
        h: 1,
    };
    area.with(style).with(Ellipsis(true)).text(buf, text);
}

fn draw_server_select_item(area: Rect, buf: &mut Buffer, item: &ServerSelectItem, selected: bool) {
    let base = if selected {
        ROOM_SELECTED
    } else {
        theme::BACKGROUND
    };
    buf.clear_rect(area, base);
    let mut rows = area;
    let mut top = rows.take_top(1);
    top.take_left(2)
        .with(base.patch(if selected { theme::GOOD } else { theme::SUBTLE }))
        .text(buf, if selected { ">" } else { " " });
    top.with(base.patch(theme::TEXT | Modifier::BOLD))
        .with(Ellipsis(true))
        .text(buf, &item.alias);
    if rows.h > 0 {
        rows.take_top(1)
            .with(base.patch(theme::MUTED))
            .with(Ellipsis(true))
            .text(
                buf,
                &format!(
                    "  {} as {}  room {}",
                    item.user, item.display_name, item.room_id
                ),
            );
    }
    if rows.h > 0 {
        rows.take_top(1)
            .with(base.patch(theme::SUBTLE))
            .with(Ellipsis(true))
            .text(buf, &format!("  {}", item.tcp_addr));
    }
}

fn draw_server_edit(area: Rect, app: &mut App, buf: &mut Buffer) {
    area.with(theme::BACKGROUND).fill(buf);
    let Some(draft) = app.server_edit.as_mut() else {
        area.with(theme::SUBTLE)
            .with(HAlign::Center)
            .text(buf, "No server edit is open");
        return;
    };
    draft.render(area, buf);
}

fn room_user_voice_feedback_label(participant: &ParticipantState) -> String {
    let Some(feedback) = participant.voice_feedback else {
        return String::new();
    };
    if !participant.voice_active || feedback.updated_at.elapsed() > Duration::from_secs(10) {
        return String::new();
    }
    format!(
        "loss{} q{} j{}",
        feedback.loss_percent, feedback.max_queue_ms, feedback.max_interarrival_jitter_ms
    )
}

fn room_user_control_label(app: &App, participant: &ParticipantState) -> String {
    if Some(participant.user_id) == app.user_id {
        return String::new();
    }
    let muted = app.muted_users.contains(&participant.user_id);
    let volume_db = app.effective_user_volume_db(participant.user_id);
    match (muted, volume_db == 0.0) {
        (false, true) => String::new(),
        (false, false) => volume_db_label(volume_db),
        (true, true) => "muted".to_string(),
        (true, false) => format!("muted {}", volume_db_label(volume_db)),
    }
}

fn draw_volume_dialog(area: Rect, app: &mut App, buf: &mut Buffer) {
    let Some(dialog) = app.volume_dialog.as_mut() else {
        return;
    };
    dialog.render(area, buf);
}

fn draw_room_title(area: Rect, app: &App, buf: &mut Buffer) {
    area.with(theme::STATUS_SECTION | Modifier::BOLD).fill(buf);
    area.with(theme::STATUS_SECTION | Modifier::BOLD).text(
        buf,
        &format!(
            " ROOM {}  online {}/{}  voice {} ",
            app.room_name,
            app.participants.online_count(),
            app.participants.entries.len(),
            app.participants.online_count()
        ),
    );
}

fn draw_chat(area: Rect, app: &mut App, buf: &mut Buffer) {
    area.with(theme::BACKGROUND).fill(buf);
    if area.is_empty() {
        return;
    }
    let name_width = NAME_COL_WIDTH.min(area.w.saturating_sub(1));
    let body_width = area.w.saturating_sub(name_width).max(1);
    app.last_chat_width = body_width;
    if app.chat.is_empty() {
        area.with(theme::SUBTLE)
            .with(HAlign::Center)
            .text(buf, "No messages");
        return;
    }
    let lines = app
        .chat
        .visible_lines(body_width, area.h, app.config.ui.overscan as usize);
    let mut row_area = area;
    let empty_rows = (area.h as usize).saturating_sub(lines.len());
    for _ in 0..empty_rows {
        row_area.take_top(1).with(theme::BACKGROUND).fill(buf);
    }
    for line in lines {
        let msg = app.chat.message(line.message);
        let mut row = row_area.take_top(1);
        let base = if msg.local {
            theme::LOCAL_LINE
        } else {
            theme::BACKGROUND
        };
        row.with(base).fill(buf);
        let name_area = row.take_left(name_width as i32);
        if line.line == 0 {
            name_area
                .with(base.patch(if msg.local {
                    theme::GOOD
                } else {
                    theme::ACCENT
                }))
                .with(HAlign::Right)
                .with(Ellipsis(true))
                .text(buf, &format!("{} ", msg.sender));
        } else {
            name_area
                .with(base.patch(theme::SUBTLE))
                .with(HAlign::Right)
                .text(buf, "│ ");
        }
        for seg in app.chat.line(line.message, line.line) {
            let start = seg.start as usize;
            let end = seg.end as usize;
            let text = &msg.body[start..end];
            let style = base.patch(theme::TEXT).patch(seg.style);
            let max_width = row.w.saturating_sub(seg.col) as usize;
            if max_width > 0 {
                buf.set_stringn(row.x + seg.col, row.y, text, max_width, style);
            }
        }
    }
}

fn draw_status(area: Rect, app: &App, buf: &mut Buffer, capture: Option<&StatsSnapshot>) {
    area.with(theme::STATUS_FILL).fill(buf);
    let mut row = area;
    draw_status_segment(
        &mut row,
        buf,
        theme::mode_style(app.mode),
        &format!(" {} ", app.modes.top().label()),
    );
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_SECTION,
        &format!(" {} ", app.room_name),
    );
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_FILL,
        &format!(" {} ", app.user),
    );
    draw_status_segment(
        &mut row,
        buf,
        voice_style(app),
        &format!(" {} ", voice_state_label(app)),
    );
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_FILL,
        &format!(" {} ", mic_status_compact(app, capture)),
    );
    draw_status_segment(&mut row, buf, theme::STATUS_FILL, " ");
    let meter_width = row.w.min(12);
    if meter_width > 0 {
        let meter = row.take_left(meter_width as i32);
        ui::vu::draw_status_vu(meter, buf, capture);
    }
    draw_status_segment(&mut row, buf, theme::STATUS_FILL, " ");
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_FILL.patch(theme::SUBTLE),
        &format!(
            " {} msg/{} rows ",
            app.chat.len(),
            app.chat.total_lines_estimate()
        ),
    );

    let right_style = match app.status_kind {
        StatusKind::Info => theme::STATUS_FILL.patch(theme::MUTED),
        StatusKind::Error => theme::STATUS_FILL.patch(theme::ERROR),
    };
    let status_text = if let Some(chord) = &app.pending_chord {
        format!(
            "{} chord {}ms",
            chord.label.as_deref().unwrap_or("pending"),
            chord.activated_at.elapsed().as_millis()
        )
    } else {
        format!("{} | {}", app.focus.active().label(), app.status)
    };
    row.with(HAlign::Right)
        .with(right_style)
        .with(Ellipsis(true))
        .text(buf, &format!(" {} ", status_text));
}

fn draw_status_segment(row: &mut Rect, buf: &mut Buffer, style: Style, text: &str) {
    if row.is_empty() {
        return;
    }
    let width = UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16;
    row.take_left(width as i32)
        .with(style)
        .with(Ellipsis(true))
        .text(buf, text);
}

fn draw_composer(area: Rect, app: &mut App, buf: &mut Buffer) {
    area.with(theme::PANEL).fill(buf);
    app.composer.resize(area.w.max(1));
    app.composer_hl.render(&mut app.composer, area, buf);
    if app.composer.text_len() == 0 {
        area.with(theme::MUTED)
            .with(Ellipsis(true))
            .text(buf, &format!(" {}", app.config.ui.placeholder));
    }
}

fn voice_style(app: &App) -> Style {
    if audio_failed(app) {
        theme::ERROR
    } else if app.deafened.load(Ordering::Relaxed) {
        theme::WARN
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        theme::GOOD
    } else if app.user_id.is_some() {
        theme::WARN
    } else {
        theme::STATUS_FILL
    }
}

/// True when capture or playback failed to start while the client is in a call.
/// Drives the persistent status-bar indicator so a dead audio path is visible
/// rather than only flashing a transient status line.
fn audio_failed(app: &App) -> bool {
    app.user_id.is_some() && (app.mic_error.is_some() || app.playback_error.is_some())
}

fn mic_status_compact(app: &App, capture: Option<&StatsSnapshot>) -> String {
    if app.mic_error.is_some() {
        return "mic unavailable".to_string();
    }
    if app.playback_error.is_some() {
        return "speaker unavailable".to_string();
    }
    if app.config.soundboard.enabled {
        return if app.soundboard_busy.load(Ordering::Relaxed) {
            "soundboard playing".to_string()
        } else {
            format!("soundboard {} clips", app.config.soundboard.clips.len())
        };
    }
    let mute = if app.deafened.load(Ordering::Relaxed) {
        "deaf"
    } else if app.settings_preview_capture && !app.voice_tx_enabled.load(Ordering::Relaxed) {
        "preview"
    } else if app.mic_muted.load(Ordering::Relaxed) {
        "muted"
    } else {
        "open"
    };
    match capture {
        Some(capture) => format!(
            "{mute} {}kbps vad{:02}%",
            app.config.audio.bitrate_bps / 1000,
            (capture.vad_probability.clamp(0.0, 1.0) * 100.0).round() as u32
        ),
        None => format!("{mute} inactive"),
    }
}

fn voice_state_label(app: &App) -> &'static str {
    if audio_failed(app) {
        "audio error"
    } else if app.deafened.load(Ordering::Relaxed) {
        "deafened"
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        "voice"
    } else if app.user_id.is_some() {
        "voice"
    } else {
        "offline"
    }
}

fn age_label(instant: Instant) -> String {
    let secs = instant.elapsed().as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}
