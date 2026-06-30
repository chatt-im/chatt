use std::{
    cmp::Ordering as CmpOrdering,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use extui::{AnsiColor, Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};
use extui_bindings::LayerId;
use extui_editor::Mode as EditorMode;
use unicode_width::UnicodeWidthStr;

use chatt::audio::StatsSnapshot;

use crate::{
    app::{
        App, ChatPanelFocus, ParticipantState, ParticipantVoiceFeedback, ServerEditDraft,
        ServerSelectItem, StatusKind, volume_db_label,
    },
    bindings::{self, Reachable, ReachableKind},
    chat_buffer::{self, LineKind},
    theme::{self, Theme},
    tui::modes::{RoomLayout, SettingsMode},
    ui,
    ui::select::FuzzySelect,
};

fn prepare_screen(app: &mut App, buf: &mut Buffer) -> Option<StatsSnapshot> {
    buf.rect().with(app.theme.background).fill(buf);
    buf.hide_cursor();
    let capture = app
        .capture
        .as_ref()
        .map(|capture| capture.stats().snapshot());
    app.chrome.top_bar.mute = Rect::EMPTY;
    app.chrome.top_bar.deafen = Rect::EMPTY;
    capture
}

pub(crate) fn draw_server_select_screen(
    app: &mut App,
    select: &mut FuzzySelect,
    searching: bool,
    mode: theme::UiMode,
    status_label: &'static str,
    layer: LayerId,
    buf: &mut Buffer,
) {
    let capture = prepare_screen(app, buf);
    let mut screen = buf.rect();
    refresh_key_preview_cache(app, Some(layer));
    let key_preview_height = key_preview_height(app, screen.w);
    let key_preview_area = screen.take_bottom(key_preview_height as i32);
    let status_area = screen.take_bottom(1);
    draw_server_select(screen, app, select, searching, buf);
    draw_status(status_area, app, buf, mode, status_label, capture.as_ref());
    draw_key_preview(key_preview_area, app, buf);
}

pub(crate) fn draw_server_edit_screen(
    app: &mut App,
    draft: &mut ServerEditDraft,
    mode: theme::UiMode,
    status_label: &'static str,
    layer: LayerId,
    buf: &mut Buffer,
) {
    let capture = prepare_screen(app, buf);
    let mut screen = buf.rect();
    refresh_key_preview_cache(app, Some(layer));
    let key_preview_height = key_preview_height(app, screen.w);
    let key_preview_area = screen.take_bottom(key_preview_height as i32);
    let status_area = screen.take_bottom(1);
    draw_server_edit(screen, app, draft, buf);
    draw_status(status_area, app, buf, mode, status_label, capture.as_ref());
    draw_key_preview(key_preview_area, app, buf);
}

pub(crate) fn draw_settings_screen(
    app: &mut App,
    settings_mode: &mut SettingsMode,
    mode: theme::UiMode,
    status_label: &'static str,
    layer: LayerId,
    buf: &mut Buffer,
) {
    let capture = prepare_screen(app, buf);
    let mut screen = buf.rect();
    let top_bar_area = screen.take_top(1);
    draw_top_bar(top_bar_area, app, buf, capture.as_ref());

    refresh_key_preview_cache(app, Some(layer));
    let composer_height = composer_height(app, screen.w);
    let key_preview_height = key_preview_height(app, screen.w);
    let key_preview_area = screen.take_bottom(key_preview_height as i32);
    let status_area = screen.take_bottom(1);
    let composer_area = screen.take_bottom(composer_height as i32);

    draw_composer(composer_area, app, ChatPanelFocus::Compose, buf);
    buf.hide_cursor();
    let session = settings_mode.session_mut();
    ui::settings::draw_settings(
        screen,
        buf,
        &app.theme,
        &mut session.draft,
        &mut session.form,
        session.dirty,
        capture.as_ref(),
        &session.input_items,
        &mut session.input_picker,
        &session.output_items,
        &mut session.output_picker,
    );
    draw_status(status_area, app, buf, mode, status_label, capture.as_ref());
    draw_key_preview(key_preview_area, app, buf);
}

pub(crate) fn draw_room_screen(
    app: &mut App,
    focus: ChatPanelFocus,
    layout: &mut RoomLayout,
    mode: theme::UiMode,
    status_label: &'static str,
    layer: LayerId,
    buf: &mut Buffer,
    now_ms: u64,
) {
    let capture = prepare_screen(app, buf);
    layout.clear_workspace();
    let mut screen = buf.rect();
    let top_bar_area = screen.take_top(1);
    draw_top_bar(top_bar_area, app, buf, capture.as_ref());

    refresh_key_preview_cache(app, Some(layer));
    let composer_height = composer_height(app, screen.w);
    let key_preview_height = key_preview_height(app, screen.w);
    let key_preview_area = screen.take_bottom(key_preview_height as i32);
    let status_area = screen.take_bottom(1);
    let composer_area = screen.take_bottom(composer_height as i32);
    layout.composer_rect = composer_area;
    layout.compose_bar_rect = status_area;
    let chat_log_bar_area = screen.take_bottom(1);
    layout.chat_log_bar_rect = chat_log_bar_area;
    draw_workspace(screen, app, focus, layout, buf, now_ms);
    draw_chat_log_bar(chat_log_bar_area, app, focus, buf);

    draw_compose_bar(status_area, app, focus, buf, mode, status_label);
    draw_composer(composer_area, app, focus, buf);
    draw_key_preview(key_preview_area, app, buf);
}

fn composer_height(app: &mut App, width: u16) -> u16 {
    app.room.composer.resize(width.max(1));
    app.room
        .composer
        .desired_height()
        .min(app.config.ui.max_composer_height.max(1))
        .max(1)
}

fn draw_room(area: Rect, app: &App, focus: ChatPanelFocus, buf: &mut Buffer) {
    area.with(app.theme.panel_alt).fill(buf);
    let mut rows = area;
    let visible = rows.h as usize;
    let start = app
        .room
        .participants
        .scroll
        .min(app.room.participants.entries.len());
    let lobby_focused = focus == ChatPanelFocus::Lobby;
    for participant in app
        .room
        .participants
        .entries
        .iter()
        .skip(start)
        .take(visible)
    {
        let row = rows.take_top(1);
        let selected =
            lobby_focused && Some(participant.user_id) == app.room.participants.selected_user;
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
            app.theme.room_selected
        } else {
            app.theme.panel_alt
        };
        let style = if selected {
            base.patch(app.theme.good)
        } else if participant.online {
            app.theme.text
        } else {
            app.theme.muted
        };
        let marker = if selected { ">" } else { " " };
        let (status_marker, status_style) = room_user_status_indicator(app, participant);
        let control = room_user_control_label(app, participant);
        let voice = room_user_voice_feedback_label(app, participant);
        row.with(base).fill(buf);
        row.with(style).with(Ellipsis(true)).text(
            buf,
            &format!(
                "{marker}   {:<16} {:<7} {:<5} {:<16} {}",
                participant.display_name(),
                state,
                spoke,
                voice,
                control
            ),
        );
        if row.w > 2 {
            buf.set_stringn(row.x + 2, row.y, status_marker, 1, base.patch(status_style));
        }
    }
}

fn draw_workspace(
    area: Rect,
    app: &mut App,
    focus: ChatPanelFocus,
    layout: &mut RoomLayout,
    buf: &mut Buffer,
    now_ms: u64,
) {
    let mut rows = area;
    let room_height = app.config.ui.room_height.min(rows.h.saturating_sub(1));
    if room_height > 0 {
        let room_area = rows.take_top(room_height as i32);
        layout.room_rect = room_area;
        draw_room(room_area, app, focus, buf);
    }

    if rows.h > 0 {
        let lobby_bar = rows.take_top(1);
        layout.lobby_bar_rect = lobby_bar;
        draw_lobby_bar(lobby_bar, app, focus, buf);
    }

    if rows.h > 0 {
        draw_chat(rows, app, focus, layout, buf, now_ms);
    } else {
        layout.clear_chat();
    }
}

fn draw_server_select(
    area: Rect,
    app: &mut App,
    select: &mut FuzzySelect,
    searching: bool,
    buf: &mut Buffer,
) {
    area.with(app.theme.background).fill(buf);
    let mut rows = area;
    rows.take_top(1)
        .with(app.theme.status_section | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, " SERVERS ");

    let mut body = rows;
    if body.h == 0 {
        return;
    }
    if app.server_items().is_empty() {
        draw_server_welcome(body, buf, &app.theme);
        return;
    }

    if searching {
        let search = body.take_top(1);
        search
            .with(app.theme.background.patch(app.theme.subtle))
            .with(Ellipsis(true))
            .text(buf, &format!("/{}", select.query()));
    }

    let theme = &app.theme;
    let items = app.server_items();
    select.render(body, 3, buf, |_, item_index, selected, area, buf| {
        if let Some(item) = items.get(item_index) {
            draw_server_select_item(area, buf, item, selected, theme);
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

fn draw_server_welcome(area: Rect, buf: &mut Buffer, theme: &Theme) {
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
            draw_server_welcome_line(buf, left_x, start_y + index as u16, line, theme);
        }
        let body_y = start_y + header.len() as u16;
        for index in 0..body_height {
            let y = body_y + index as u16;
            if let Some(line) = left.get(index) {
                draw_server_welcome_line(buf, left_x, y, line, theme);
            }
            if let Some(line) = right.get(index) {
                draw_server_welcome_line(buf, right_x, y, line, theme);
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
            draw_server_welcome_line(buf, x, start_y + index as u16, line, theme);
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

fn draw_server_welcome_line(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    line: &ServerWelcomeLine,
    theme: &Theme,
) {
    let section_style = theme.background.patch(theme.text | Modifier::BOLD);
    let text_style = theme.background.patch(theme.muted);
    let key_style = theme.background.patch(theme.accent);

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

fn draw_server_select_item(
    area: Rect,
    buf: &mut Buffer,
    item: &ServerSelectItem,
    selected: bool,
    theme: &Theme,
) {
    let base = if selected {
        theme.room_selected
    } else {
        theme.background
    };
    buf.clear_rect(area, base);
    let mut rows = area;
    let mut top = rows.take_top(1);
    top.take_left(2)
        .with(base.patch(if selected { theme.good } else { theme.subtle }))
        .text(buf, if selected { ">" } else { " " });
    top.with(base.patch(theme.text | Modifier::BOLD))
        .with(Ellipsis(true))
        .text(buf, &item.alias);
    if rows.h > 0 {
        rows.take_top(1)
            .with(base.patch(theme.muted))
            .with(Ellipsis(true))
            .text(
                buf,
                &format!("  {}  room {}", item.display_name, item.room_id),
            );
    }
    if rows.h > 0 {
        rows.take_top(1)
            .with(base.patch(theme.subtle))
            .with(Ellipsis(true))
            .text(buf, &format!("  {}", item.tcp_addr));
    }
}

fn draw_server_edit(area: Rect, app: &mut App, draft: &mut ServerEditDraft, buf: &mut Buffer) {
    area.with(app.theme.background).fill(buf);
    let theme = &app.theme;
    draft.render(area, buf, theme);
}

fn room_user_voice_feedback_label(app: &App, participant: &ParticipantState) -> String {
    let Some(feedback) = participant.voice_feedback else {
        return String::new();
    };
    if !participant.voice_active || feedback.updated_at.elapsed() > Duration::from_secs(10) {
        return String::new();
    }
    // A directly connected peer uses its own measured RTT, everyone else rides
    // the relay so the shared server RTT is the network leg. Halved to one-way.
    let rtt_ms = if participant.p2p_direct {
        participant.peer_rtt_ms
    } else {
        app.server_rtt_ms
    };
    if app.lobby_details {
        return format!(
            "loss{} jb{}/{} r{} j{} net{}",
            feedback.loss_percent,
            feedback.max_neteq_playout_delay_ms,
            feedback.max_neteq_target_ms,
            feedback.max_output_ring_ms,
            feedback.max_interarrival_jitter_ms,
            rtt_ms.unwrap_or(0) / 2
        );
    }
    // Collapsed default: a single mouth-to-ear-ish latency estimate.
    format!("~{}ms", participant_latency_estimate_ms(&feedback, rtt_ms))
}

/// Combines the stabilized jitter-buffer depth (an EWMA of the NetEQ target that
/// holds steady through silence), the output device ring, and one-way network
/// latency (half the measured RTT) into a single latency figure in milliseconds.
fn participant_latency_estimate_ms(
    feedback: &ParticipantVoiceFeedback,
    rtt_ms: Option<u16>,
) -> u16 {
    feedback
        .jitter_buffer_ms
        .saturating_add(feedback.max_output_ring_ms)
        .saturating_add(rtt_ms.unwrap_or(0) / 2)
}

fn room_user_status_indicator(app: &App, participant: &ParticipantState) -> (&'static str, Style) {
    let local_user = Some(participant.user_id) == app.user_id;
    if !participant.online {
        return ("▇", app.theme.muted);
    }
    if participant.voice_status.deafened || (local_user && app.deafened.load(Ordering::Relaxed)) {
        return ("▇", app.theme.error);
    }
    if participant.voice_status.muted || (local_user && app.mic_muted.load(Ordering::Relaxed)) {
        return ("▇", app.theme.warn);
    }
    if participant.voice_active {
        return (
            if participant.talking_display {
                "▇"
            } else {
                "░"
            },
            app.theme.good,
        );
    }
    ("▇", app.theme.muted)
}

fn room_user_control_label(app: &App, participant: &ParticipantState) -> String {
    if Some(participant.user_id) == app.user_id {
        return String::new();
    }
    let muted = app.room.muted_user(participant.user_id);
    let volume_db = app
        .room
        .effective_user_volume_db(&app.config, participant.user_id);
    match (muted, volume_db == 0.0) {
        (false, true) => String::new(),
        (false, false) => volume_db_label(volume_db),
        (true, true) => "muted".to_string(),
        (true, false) => format!("muted {}", volume_db_label(volume_db)),
    }
}

fn draw_top_bar(area: Rect, app: &mut App, buf: &mut Buffer, capture: Option<&StatsSnapshot>) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    area.with(theme.status_fill).fill(buf);

    let server = if app.room.server_alias.trim().is_empty() {
        "Server"
    } else {
        app.room.server_alias.as_str()
    };
    let mut left = area;
    draw_status_segment(
        &mut left,
        buf,
        theme.status_section | Modifier::BOLD,
        &format!(" {server} - {} ", connection_state_label(app)),
    );
    if !app.room.local_user_name.trim().is_empty() {
        draw_status_segment(
            &mut left,
            buf,
            theme.status_fill.patch(theme.muted),
            &format!(" {} ", app.room.local_user_name),
        );
    }

    let mut right = area;
    let deafened = app.deafened.load(Ordering::Relaxed);
    let muted = deafened || app.mic_muted.load(Ordering::Relaxed);
    app.chrome.top_bar.deafen = draw_status_segment_right(
        &mut right,
        buf,
        if deafened {
            theme.status_section.patch(theme.warn) | Modifier::BOLD
        } else {
            theme.status_fill.patch(theme.good)
        },
        if deafened { " DEAF " } else { " HEAR " },
    );
    app.chrome.top_bar.mute = draw_status_segment_right(
        &mut right,
        buf,
        if muted {
            theme.status_section.patch(theme.warn) | Modifier::BOLD
        } else {
            theme.status_fill.patch(theme.good)
        },
        if muted { " MUTED " } else { " MIC " },
    );
    draw_status_segment_right(&mut right, buf, theme.status_fill, " ");

    let meter_width = right.w.min(14);
    if meter_width > 0 {
        let meter = right.take_right(meter_width as i32);
        ui::vu::draw_status_vu(meter, buf, capture, &theme);
    }
    draw_status_segment_right(&mut right, buf, theme.status_fill, " ");
    draw_status_segment_right(
        &mut right,
        buf,
        theme.status_fill.patch(voice_style(app)),
        &format!(
            " {} {} ",
            voice_state_label(app),
            mic_status_compact(app, capture)
        ),
    );
}

fn draw_lobby_bar(area: Rect, app: &App, focus: ChatPanelFocus, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let focused = focus == ChatPanelFocus::Lobby;
    let (fill, label, detail) = section_bar_styles(app.theme, ChatPanelFocus::Lobby, focused);
    area.with(fill).fill(buf);
    let voice_count = app
        .room
        .participants
        .entries
        .iter()
        .filter(|participant| participant.online && participant.voice_active)
        .count();
    let mut row = area;
    draw_status_segment(&mut row, buf, label, " Lobby ");
    draw_status_segment(
        &mut row,
        buf,
        detail,
        &format!(
            " room {}  online {}/{}  voice {} ",
            app.room.room_name,
            app.room.participants.online_count(),
            app.room.participants.entries.len(),
            voice_count
        ),
    );
}

fn draw_chat_log_bar(area: Rect, app: &App, focus: ChatPanelFocus, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let focused = focus == ChatPanelFocus::ChatLog;
    let (fill, label, detail) = section_bar_styles(app.theme, ChatPanelFocus::ChatLog, focused);
    area.with(fill).fill(buf);
    let mut row = area;
    draw_status_segment(&mut row, buf, label, " Chat Log ");
    draw_status_segment(
        &mut row,
        buf,
        detail,
        &format!(
            " {} msg/{} rows ",
            app.room.chat.len(),
            app.room.chat.total_lines_estimate()
        ),
    );
}

fn draw_compose_bar(
    area: Rect,
    app: &App,
    focus: ChatPanelFocus,
    buf: &mut Buffer,
    mode: theme::UiMode,
    status_label: &'static str,
) {
    if area.is_empty() {
        return;
    }
    let focused = focus == ChatPanelFocus::Compose;
    let (fill, mut label, detail) = section_bar_styles(app.theme, ChatPanelFocus::Compose, focused);
    if focused {
        label = app.theme.mode_style(mode) | Modifier::BOLD;
    }
    area.with(fill).fill(buf);
    let mut row = area;
    draw_status_segment(&mut row, buf, label, &format!(" {status_label} "));
    draw_status_segment(
        &mut row,
        buf,
        detail,
        &format!(" {} ", composer_mode_label(app)),
    );
    draw_status_text_right(row, app, buf, fill);
}

fn composer_mode_label(app: &App) -> &'static str {
    match app.room.composer.mode() {
        EditorMode::Normal => "normal",
        EditorMode::Insert => "insert",
        EditorMode::Visual => "visual",
        EditorMode::VisualLine => "visual line",
        EditorMode::VisualBlock => "visual block",
    }
}

fn section_bar_styles(theme: Theme, panel: ChatPanelFocus, focused: bool) -> (Style, Style, Style) {
    let fill = theme.status_fill;
    if focused {
        (
            fill,
            panel_mode_style(theme, panel) | Modifier::BOLD,
            fill.patch(theme.muted),
        )
    } else {
        let dim = fill.patch(theme.subtle);
        (fill, dim, dim)
    }
}

fn panel_mode_style(theme: Theme, panel: ChatPanelFocus) -> Style {
    match panel {
        ChatPanelFocus::Lobby => theme.mode_server_edit,
        ChatPanelFocus::ChatLog => theme.mode_log,
        ChatPanelFocus::Compose => theme.mode_compose,
    }
}

fn draw_chat(
    area: Rect,
    app: &mut App,
    focus: ChatPanelFocus,
    layout: &mut RoomLayout,
    buf: &mut Buffer,
    now_ms: u64,
) {
    area.with(app.theme.background).fill(buf);
    if area.is_empty() {
        layout.clear_chat();
        return;
    }
    // The leftmost column is a marker gutter (`▟`/`▌`), so content wraps to one
    // column less than the full chat width.
    let content_width = area.w.saturating_sub(1).max(1);
    if content_width != layout.chat_width {
        // Reflow invalidates the (message, line) coordinates a selection holds.
        app.room.chat.clear_selection();
    }
    layout.chat_width = content_width;
    layout.chat_height = area.h;
    layout.chat_rect = area;
    if app.room.chat.is_empty() {
        layout.visible_chat_lines.clear();
        area.with(app.theme.subtle)
            .with(HAlign::Center)
            .text(buf, "No messages");
        return;
    }
    let lines = app
        .room
        .chat
        .visible_lines(content_width, area.h, app.config.ui.overscan as usize);
    layout.visible_chat_lines = lines.clone();
    let chat_focused = focus == ChatPanelFocus::ChatLog;
    // Content is top-anchored: lines are drawn from the top of `area` and the
    // already-background-filled rows below them stay empty.
    let mut row_area = area;
    for line in lines {
        let mut row = row_area.take_top(1);
        let marker = row.take_left(1);
        match line.kind {
            LineKind::Heading => draw_chat_heading(
                marker,
                row,
                app,
                line.message,
                now_ms,
                chat_focused && app.room.chat.is_header_selected(line),
                buf,
            ),
            LineKind::Body => {
                let msg = app.room.chat.message(line.message);
                let selected = chat_focused && app.room.chat.is_selected(line.message, line.line);
                let base = if selected {
                    app.theme.selected_line
                } else if msg.local {
                    app.theme.local_line
                } else {
                    app.theme.background
                };
                let accent = if msg.local {
                    app.theme.good
                } else {
                    app.theme.accent
                };
                marker.with(base).fill(buf);
                marker.with(base.patch(accent)).text(buf, "▌");
                row.with(base).fill(buf);
                for seg in app.room.chat.line(line.message, line.line) {
                    let start = seg.start as usize;
                    let end = seg.end as usize;
                    let text = &msg.body[start..end];
                    let style = base.patch(app.theme.text).patch(seg.style);
                    let max_width = row.w.saturating_sub(seg.col) as usize;
                    if max_width > 0 {
                        buf.set_stringn(row.x + seg.col, row.y, text, max_width, style);
                    }
                }
            }
            LineKind::Ellipsis => {
                let base = app.theme.background;
                marker.with(base).fill(buf);
                row.with(base).fill(buf);
                row.with(app.theme.subtle)
                    .with(HAlign::Center)
                    .text(buf, "...");
            }
        }
    }
}

/// Draws a block heading: the `▟` marker, then the sender name on the left and
/// the relative age on the right, both padded one column inside `row`.
fn draw_chat_heading(
    marker: Rect,
    row: Rect,
    app: &App,
    message: usize,
    now_ms: u64,
    selected: bool,
    buf: &mut Buffer,
) {
    let msg = app.room.chat.message(message);
    let normal_base = if msg.local {
        app.theme.local_line
    } else {
        app.theme.background
    };
    let base = if selected {
        app.theme.mode_log
    } else {
        normal_base
    };
    let accent = if msg.local {
        app.theme.good
    } else {
        app.theme.accent
    };
    let selected_base = selected_chat_heading_style(app.theme, accent);
    let header_base = if selected {
        selected_base.unwrap_or(base)
    } else {
        base
    };
    marker.with(normal_base).fill(buf);
    marker.with(normal_base.patch(accent)).text(buf, "▟");
    row.with(header_base).fill(buf);
    let content = row.inset(1, 0);
    let name = if app.room.chat.is_collapsed(message) {
        format!("{} (Collapsed)", msg.sender)
    } else if app.room.chat.is_expanded(message) {
        format!("{} (Expanded)", msg.sender)
    } else {
        msg.sender.clone()
    };
    let name_style = if selected {
        header_base
    } else {
        base.patch(accent)
    };
    content
        .with(name_style)
        .with(Ellipsis(true))
        .text(buf, &name);
    let age = chat_age(msg.timestamp_ms, now_ms);
    if !age.is_empty() {
        let age_style = if selected {
            header_base
        } else {
            base.patch(app.theme.subtle)
        };
        content.with(age_style).with(HAlign::Right).text(buf, &age);
    }
}

fn selected_chat_heading_style(theme: Theme, accent: Style) -> Option<Style> {
    let color = accent.fg().or_else(|| accent.bg())?;
    Some(theme.mode_log.with_bg(color))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feedback(jitter_buffer_ms: u16, ring_ms: u16) -> ParticipantVoiceFeedback {
        ParticipantVoiceFeedback {
            loss_percent: 0,
            max_output_ring_ms: ring_ms,
            max_neteq_target_ms: jitter_buffer_ms,
            max_neteq_playout_delay_ms: jitter_buffer_ms,
            max_interarrival_jitter_ms: 0,
            jitter_buffer_ms,
            updated_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn latency_estimate_sums_buffer_and_half_rtt() {
        // jitter buffer 60 + ring 10 + rtt 40/2 = 90.
        assert_eq!(
            participant_latency_estimate_ms(&feedback(60, 10), Some(40)),
            90
        );
    }

    #[test]
    fn latency_estimate_tolerates_missing_rtt() {
        // Without an RTT sample the network leg contributes nothing.
        assert_eq!(participant_latency_estimate_ms(&feedback(60, 10), None), 70);
    }

    #[test]
    fn selected_chat_heading_uses_message_accent_as_fill() {
        let theme = Theme::tomorrow_night();
        let local = selected_chat_heading_style(theme, theme.good).expect("local accent");
        let remote = selected_chat_heading_style(theme, theme.accent).expect("remote accent");

        assert_eq!(local.bg(), theme.good.fg());
        assert_eq!(remote.bg(), theme.accent.fg());
        assert_eq!(local.fg(), theme.mode_log.fg());
        assert_eq!(remote.fg(), theme.mode_log.fg());
    }

    #[test]
    fn key_preview_entries_are_deterministic_and_reuse_buffer() {
        let runtime = bindings::BindingRuntime::default();
        let layer = bindings::WORKSPACE_LAYER;

        fn snapshot(entries: &[KeyPreviewEntry]) -> Vec<(String, String, i8)> {
            let mut out = Vec::new();
            for entry in entries {
                out.push((entry.key_hint.clone(), entry.label.clone(), entry.order));
            }
            out
        }

        let mut buffer = Vec::new();
        build_key_preview_entries(&runtime, &None, layer, &mut buffer);
        assert!(!buffer.is_empty(), "workspace layer should expose bindings");
        let first = snapshot(&buffer);

        // Output is already sorted by the comparator render relies on.
        let mut sorted = buffer.iter().map(|e| e.order).collect::<Vec<_>>();
        sorted.sort_unstable();
        assert_eq!(
            buffer.iter().map(|e| e.order).collect::<Vec<_>>(),
            sorted,
            "entries must be returned in sorted order"
        );

        // Rebuilding into the same cleared buffer (the cache-miss reuse path)
        // reproduces the entries exactly.
        buffer.clear();
        build_key_preview_entries(&runtime, &None, layer, &mut buffer);
        assert_eq!(first, snapshot(&buffer));
    }
}

/// Formats a message age for the heading. Empty for notices (`timestamp_ms == 0`).
fn chat_age(timestamp_ms: u64, now_ms: u64) -> String {
    if timestamp_ms == 0 {
        return String::new();
    }
    chat_buffer::format_age(now_ms.saturating_sub(timestamp_ms))
}

fn draw_status(
    area: Rect,
    app: &App,
    buf: &mut Buffer,
    mode: theme::UiMode,
    label: &'static str,
    _capture: Option<&StatsSnapshot>,
) {
    if area.is_empty() {
        return;
    }
    let theme = &app.theme;
    area.with(theme.status_fill).fill(buf);
    let mut row = area;
    draw_status_segment(&mut row, buf, theme.mode_style(mode), &format!(" {label} "));
    draw_status_text_right(row, app, buf, theme.status_fill);
}

fn draw_status_text_right(area: Rect, app: &App, buf: &mut Buffer, fill: Style) {
    let theme = &app.theme;
    let right_style = match app.status.kind() {
        StatusKind::Info => fill.patch(theme.muted),
        StatusKind::Error => fill.patch(theme.error),
    };
    let status_text = if let Some(chord) = &app.chrome.binding.pending_chord {
        format!(
            "{} chord {}ms",
            chord.label.as_deref().unwrap_or("pending"),
            chord.activated_at.elapsed().as_millis()
        )
    } else {
        app.status.text().to_string()
    };
    area.with(HAlign::Right)
        .with(right_style)
        .with(Ellipsis(true))
        .text(buf, &format!(" {} ", status_text));
}

pub(crate) struct KeyPreviewEntry {
    key_hint: String,
    label: String,
    width: usize,
    order: i8,
    toggle: bool,
}

/// Memoized key-preview entries plus the inputs that produced them. `render`
/// rebuilds the entries only when the active binding layer or the pending chord
/// layer changes, so the steady ~20 Hz redraw does not re-run
/// [`bindings::reachable`] or reallocate the per-entry strings.
#[derive(Default)]
pub(crate) struct KeyPreviewCache {
    key: Option<(Option<LayerId>, Option<LayerId>)>,
    entries: Vec<KeyPreviewEntry>,
}

#[derive(Clone, Copy)]
struct KeyPreviewRow {
    start: usize,
    end: usize,
}

const KEY_PREVIEW_ENTRY_GAP: &str = "  ";
const KEY_PREVIEW_ENTRY_GAP_WIDTH: usize = 2;

fn key_preview_height(app: &App, width: u16) -> u16 {
    let entries = &app.chrome.key_preview.cache.entries;
    if entries.is_empty() {
        return 0;
    }
    let rows = key_preview_rows(entries, width);
    if app.chrome.key_preview.expanded && rows.len() > 1 {
        rows.len().try_into().unwrap_or(u16::MAX)
    } else {
        1
    }
}

fn draw_key_preview(area: Rect, app: &App, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }

    let entries = &app.chrome.key_preview.cache.entries;
    if entries.is_empty() {
        area.with(Style::default()).fill(buf);
        return;
    }

    let rows = key_preview_rows(entries, area.w);
    let bar_style = key_preview_bar_style();
    let key_style = key_preview_key_style();
    let label_style = key_preview_label_style();
    area.with(bar_style).fill(buf);

    for (row_index, row) in rows.iter().enumerate() {
        let Ok(row_index) = u16::try_from(row_index) else {
            break;
        };
        if row_index >= area.h {
            break;
        }

        let line = Rect {
            x: area.x,
            y: area.y.saturating_add(row_index),
            w: area.w,
            h: 1,
        };
        let mut display_row = line.with(bar_style).fill(buf);
        for (entry_index, entry) in entries[row.start..row.end].iter().enumerate() {
            if entry_index > 0 {
                display_row = display_row.with(bar_style).text(buf, KEY_PREVIEW_ENTRY_GAP);
            }
            display_row = display_row
                .with(key_style)
                .text(buf, entry.key_hint.as_str());
            if !entry.label.is_empty() {
                display_row = display_row
                    .with(label_style)
                    .text(buf, " ")
                    .text(buf, entry.label.as_str());
            }
        }
    }

    if rows.len() > 1 {
        let hint = key_preview_toggle_hint(entries);
        let label = if app.chrome.key_preview.expanded {
            format!("less [{hint}]")
        } else {
            format!("more [{hint}]")
        };
        let width = key_preview_more_width(entries);
        let toggle = Rect {
            x: area.x.saturating_add(area.w.saturating_sub(width)),
            y: area.y + area.h.saturating_sub(1),
            w: width.min(area.w),
            h: 1.min(area.h),
        };
        toggle
            .with(label_style)
            .with(HAlign::Right)
            .text(buf, &label);
    }
}

/// Rebuilds the shared key-preview cache when the active binding layer or pending
/// chord layer has changed since the last render, and is a cheap key comparison
/// otherwise. The stored entries back both [`key_preview_height`] and
/// [`draw_key_preview`], which read them without recomputing.
fn refresh_key_preview_cache(app: &mut App, base: Option<LayerId>) {
    let pending = app
        .chrome
        .binding
        .pending_chord
        .as_ref()
        .map(|chord| chord.layer);
    let key = Some((base, pending));
    if app.chrome.key_preview.cache.key == key {
        return;
    }
    app.chrome.key_preview.cache.key = key;
    let mut entries = std::mem::take(&mut app.chrome.key_preview.cache.entries);
    entries.clear();
    if let Some(layer) = base {
        build_key_preview_entries(
            &app.config.bindings,
            &app.chrome.binding.pending_chord,
            layer,
            &mut entries,
        );
    }
    app.chrome.key_preview.cache.entries = entries;
}

fn build_key_preview_entries(
    bindings: &bindings::BindingRuntime,
    pending: &Option<bindings::PendingChord>,
    layer: LayerId,
    out: &mut Vec<KeyPreviewEntry>,
) {
    for Reachable { key, kind } in bindings::reachable(bindings, layer, pending) {
        let key_hint = key.to_string();
        let (label, order, toggle) = match kind {
            ReachableKind::Action(command) => {
                let toggle = matches!(command, bindings::BindCommand::ToggleKeyPreview);
                let spec = command.spec();
                (spec.label.to_string(), spec.order, toggle)
            }
            ReachableKind::EnterLayer(label) => {
                (label.unwrap_or_else(|| "more".to_string()), 10, false)
            }
        };
        let width = key_hint.width() + usize::from(!label.is_empty()) + label.width();
        out.push(KeyPreviewEntry {
            key_hint,
            label,
            width,
            order,
            toggle,
        });
    }
    out.sort_by(|left, right| key_preview_entry_cmp(left, right));
}

fn key_preview_entry_cmp(left: &KeyPreviewEntry, right: &KeyPreviewEntry) -> CmpOrdering {
    left.order
        .cmp(&right.order)
        .then_with(|| left.key_hint.cmp(&right.key_hint))
        .then_with(|| left.label.cmp(&right.label))
}

fn key_preview_rows(entries: &[KeyPreviewEntry], width: u16) -> Vec<KeyPreviewRow> {
    let mut rows = key_preview_rows_for_width(entries, width);
    if rows.len() > 1 {
        rows = key_preview_rows_for_width(
            entries,
            width.saturating_sub(key_preview_more_width(entries)),
        );
    }
    rows
}

fn key_preview_toggle_hint(entries: &[KeyPreviewEntry]) -> &str {
    entries
        .iter()
        .find(|entry| entry.toggle)
        .map(|entry| entry.key_hint.as_str())
        .unwrap_or(".")
}

fn key_preview_more_width(entries: &[KeyPreviewEntry]) -> u16 {
    (7 + key_preview_toggle_hint(entries).width())
        .max(9)
        .try_into()
        .unwrap_or(u16::MAX)
}

fn key_preview_rows_for_width(entries: &[KeyPreviewEntry], width: u16) -> Vec<KeyPreviewRow> {
    if entries.is_empty() {
        return Vec::new();
    }

    let width = usize::from(width);
    let mut rows = Vec::new();
    let mut row_start = 0;
    let mut line_width = 0;

    for (index, entry) in entries.iter().enumerate() {
        let entry_width = entry.width
            + if line_width > 0 {
                KEY_PREVIEW_ENTRY_GAP_WIDTH
            } else {
                0
            };
        if line_width > 0 && line_width + entry_width > width {
            rows.push(KeyPreviewRow {
                start: row_start,
                end: index,
            });
            row_start = index;
            line_width = entry.width;
        } else {
            line_width += entry_width;
        }
    }

    rows.push(KeyPreviewRow {
        start: row_start,
        end: entries.len(),
    });
    rows
}

fn key_preview_bar_style() -> Style {
    Style::default()
}

fn key_preview_key_style() -> Style {
    Style::default().with_fg_ansi(AnsiColor::Grey[23])
}

fn key_preview_label_style() -> Style {
    Style::default().with_fg_ansi(AnsiColor::Grey[14])
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

fn draw_status_segment_right(row: &mut Rect, buf: &mut Buffer, style: Style, text: &str) -> Rect {
    if row.is_empty() {
        return Rect::EMPTY;
    }
    let width = UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16;
    let area = row.take_right(width as i32);
    area.with(style).with(Ellipsis(true)).text(buf, text);
    area
}

fn draw_composer(area: Rect, app: &mut App, focus: ChatPanelFocus, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    area.with(app.theme.panel).fill(buf);
    app.room.composer.resize(area.w.max(1));
    app.room
        .refresh_command_completion(focus == ChatPanelFocus::Compose, app.theme.subtle);
    app.room
        .composer_hl
        .render(&mut app.room.composer, area, buf, &app.theme);
    if focus != ChatPanelFocus::Compose {
        buf.hide_cursor();
    }
    if app.room.composer.text_len() == 0 {
        area.with(app.theme.muted)
            .with(Ellipsis(true))
            .text(buf, &format!(" {}", app.config.ui.placeholder));
    }
}

fn connection_state_label(app: &App) -> &'static str {
    if app.network.is_none() {
        "Disconnected"
    } else if app.user_id.is_some() {
        "Connected"
    } else {
        "Connecting"
    }
}

fn voice_style(app: &App) -> Style {
    if audio_failed(app) {
        app.theme.error
    } else if app.deafened.load(Ordering::Relaxed) {
        app.theme.warn
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        app.theme.good
    } else if app.user_id.is_some() {
        app.theme.warn
    } else {
        app.theme.status_fill
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
