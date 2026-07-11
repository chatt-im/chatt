use std::{
    cmp::Ordering as CmpOrdering,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use extui::{AnsiColor, Buffer, Ellipsis, HAlign, Rect, Style, vt::Modifier};
use extui_bindings::LayerId;
use extui_editor::Mode as EditorMode;
use rpc::ids::FileTransferId;
use unicode_width::UnicodeWidthStr;

use crate::audio::StatsSnapshot;

use crate::{
    app::{
        ChatPanelFocus, LocalVoiceMode, ParticipantState, ParticipantVoiceFeedback, RoomSession,
        ScreencastPhase, ServerEditDraft, ServerSelectItem, StatusKind,
        audio_supervisor::AudioHealthState,
        room::{RoomSelectItem, TransferProgress, TransferStatus},
        volume_db_label,
    },
    bindings::{self, Reachable, ReachableKind},
    chat_buffer::{self, LineKind, NoticeKind},
    client_net::{TerminalVerb, TransferDirection, format_bytes},
    config::Config,
    theme::{self, Theme},
    tui::{
        mode::ViewCx,
        modes::{LobbyListFocus, RoomLayout, SettingsMode, WelcomeMode},
        view::ClientView,
    },
    ui,
    ui::select::FuzzySelect,
};

pub(crate) struct RenderState<'a> {
    pub(crate) view: &'a mut ClientView,
    pub(crate) room: &'a RoomSession,
    pub(crate) config: &'a Config,
}

impl<'a> RenderState<'a> {
    pub(crate) fn new(cx: &'a mut ViewCx<'_>) -> Self {
        Self {
            view: &mut *cx.view,
            room: cx.session,
            config: cx.config,
        }
    }

    fn room_select_items(&self) -> Vec<RoomSelectItem> {
        let mut items = self.room.room_select_items(self.room.voice_room);
        for item in &mut items {
            item.viewed = self.view.viewed_room == Some(item.room_id);
        }
        items
    }

    fn server_items(&self) -> &[ServerSelectItem] {
        self.view.server_catalog.items()
    }

    fn is_offline(&self) -> bool {
        !self.room.network_selected || self.room.network_disconnected
    }

    fn is_udp_unreachable(&self) -> bool {
        self.room.udp_unreachable
    }
}

fn prepare_screen(app: &mut RenderState<'_>, buf: &mut Buffer) -> Option<StatsSnapshot> {
    buf.rect().with(app.view.theme.background).fill(buf);
    buf.hide_cursor();
    let capture = app
        .room
        .capture_stats
        .as_ref()
        .map(|stats| stats.snapshot());
    let capture = match capture {
        Some(mut snapshot) => {
            let (rms, peak) =
                app.view
                    .mic_level_ballistics
                    .smooth(snapshot.rms, snapshot.peak, Instant::now());
            snapshot.rms = rms;
            snapshot.peak = peak;
            Some(snapshot)
        }
        None => {
            app.view.mic_level_ballistics.reset();
            None
        }
    };
    app.view.chrome.top_bar.live = Rect::EMPTY;
    app.view.chrome.top_bar.mute = Rect::EMPTY;
    app.view.chrome.top_bar.deafen = Rect::EMPTY;
    app.view.chrome.top_bar.video = Rect::EMPTY;
    app.view.chrome.transfer_buttons.clear();
    capture
}

/// Draws the `chatt join` fallback warning as a warm header block when set,
/// consuming the top three rows of `screen` when there is enough space.
fn draw_join_notice(screen: &mut Rect, app: &RenderState<'_>, buf: &mut Buffer) {
    let Some(notice) = app.room.join_notice.as_deref() else {
        return;
    };
    let height = screen.h.min(3);
    if height == 0 {
        return;
    }
    let block = screen.take_top(height as i32);
    let style = app.view.theme.status_section.patch(app.view.theme.warn);
    let header_style = join_notice_header_style(&app.view.theme);
    block.with(style).fill(buf);
    let lines = [
        " ! No saved server matched the join target",
        notice,
        "   Pairing with that address instead; verify the prompt before continuing",
    ];
    let mut rows = block;
    if let Some(heading) = lines.first() {
        rows.take_top(1)
            .with(header_style | Modifier::BOLD)
            .fill(buf)
            .with(Ellipsis(true))
            .text(buf, heading);
    }
    for line in lines.iter().skip(1).take(height.saturating_sub(1) as usize) {
        rows.take_top(1)
            .with(style | Modifier::BOLD)
            .with(Ellipsis(true))
            .text(buf, line);
    }
}

fn join_notice_header_style(theme: &Theme) -> Style {
    match theme.warn.fg().or_else(|| theme.warn.bg()) {
        Some(warn) => theme.mode_server_edit.with_bg(warn),
        None => theme.status_section.patch(theme.warn),
    }
}

pub(crate) fn draw_server_select_screen(
    app: &mut RenderState<'_>,
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
    draw_join_notice(&mut screen, app, buf);
    draw_server_select(screen, app, select, searching, buf);
    draw_status(status_area, app, buf, mode, status_label, capture.as_ref());
    draw_key_preview(key_preview_area, app, buf);
}

/// Centered panel geometry for a form dialog overlay: `form_height` body rows
/// plus a header row and vertical padding. `None` when the screen is too small
/// for a useful dialog.
pub(crate) fn form_dialog_panel(area: Rect, form_height: u16) -> Option<Rect> {
    if area.w < 30 || area.h < 8 {
        return None;
    }
    let width = area.w.saturating_sub(4).min(112);
    let height = form_height.saturating_add(3).min(area.h.saturating_sub(2));
    Some(Rect {
        x: area.x + area.w.saturating_sub(width) / 2,
        y: area.y + area.h.saturating_sub(height) / 2,
        w: width,
        h: height,
    })
}

/// Clears the dialog panel, draws its header row, and returns the padded body
/// rect the caller renders the form into.
pub(crate) fn draw_form_dialog_frame(
    panel: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    title: &str,
) -> Rect {
    buf.clear_rect(panel, theme.dialog_panel);
    let mut rows = panel;
    rows.take_top(1)
        .with(theme.dialog_header | Modifier::BOLD)
        .fill(buf)
        .with(HAlign::Center)
        .with(Ellipsis(true))
        .text(buf, &format!(" {title} "));
    rows.inset(1, 1)
}

pub(crate) fn draw_server_edit_overlay(
    app: &mut RenderState<'_>,
    draft: &mut ServerEditDraft,
    buf: &mut Buffer,
) {
    let area = buf.rect();
    let Some(panel) = form_dialog_panel(area, draft.form_height()) else {
        return;
    };
    let body = draw_form_dialog_frame(panel, buf, &app.view.theme, &draft.title());
    draft.render(body, buf, &app.view.theme);
    draw_overlay_key_preview(app, bindings::FORM_LAYER, buf);
}

pub(crate) fn draw_settings_screen(
    app: &mut RenderState<'_>,
    _settings_mode: &mut SettingsMode,
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

    let Some(settings) = app.room.settings.clone() else {
        return;
    };
    let mut session = settings
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let crate::tui::modes::SettingsSession {
        draft,
        form,
        input_items,
        output_items,
        input_picker,
        output_picker,
        dirty,
        ..
    } = &mut *session;
    ui::settings::draw_settings(
        screen,
        buf,
        &app.view.theme,
        &app.config.bindings,
        draft,
        form,
        *dirty,
        capture.as_ref(),
        input_items,
        input_picker,
        output_items,
        output_picker,
    );
    draw_status(status_area, app, buf, mode, status_label, capture.as_ref());
    draw_key_preview(key_preview_area, app, buf);
}

pub(crate) fn draw_welcome_screen(
    app: &mut RenderState<'_>,
    welcome_mode: &mut WelcomeMode,
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

    welcome_mode.draw_body(screen, app, buf);
    draw_status(status_area, app, buf, mode, status_label, capture.as_ref());
    draw_key_preview(key_preview_area, app, buf);
}

pub(crate) fn draw_room_screen(
    app: &mut RenderState<'_>,
    focus: ChatPanelFocus,
    lobby_list_focus: LobbyListFocus,
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
    let key_preview_height = key_preview_height(app, screen.w);
    let key_preview_area = screen.take_bottom(key_preview_height as i32);
    let status_area = screen.take_bottom(1);
    let composer_height = composer_height(app, screen.w, screen.h.saturating_sub(4));
    let composer_area = screen.take_bottom(composer_height as i32);
    layout.composer_rect = composer_area;
    layout.compose_bar_rect = status_area;
    let chat_log_bar_area = screen.take_bottom(1);
    layout.chat_log_bar_rect = chat_log_bar_area;
    draw_workspace(screen, app, focus, lobby_list_focus, layout, buf, now_ms);
    draw_chat_log_bar(chat_log_bar_area, app, focus, buf);

    draw_compose_bar(status_area, app, focus, buf, mode, status_label);
    draw_composer(composer_area, app, focus, buf);
    draw_key_preview(key_preview_area, app, buf);
}

fn composer_height(app: &mut RenderState<'_>, width: u16, max_rows: u16) -> u16 {
    if max_rows == 0 {
        return 0;
    }
    app.view.composer.resize(width.max(1));
    let floor = app.view.composer_min_rows.unwrap_or(1).max(1);
    let config_cap = app.config.ui.max_composer_height.max(1);
    let desired = app.view.composer.desired_height();
    let height = if app.view.composer_min_rows.is_some() {
        desired.max(floor)
    } else {
        desired.min(config_cap)
    };
    height.clamp(1, max_rows)
}

fn draw_user_list(
    area: Rect,
    app: &mut RenderState<'_>,
    focus: ChatPanelFocus,
    lobby_list_focus: LobbyListFocus,
    buf: &mut Buffer,
) {
    area.with(app.view.theme.panel_alt).fill(buf);
    let mut rows = area;
    let visible = rows.h as usize;
    let participants = app.room.participant_snapshot(app.view.viewed_room);
    app.view.selected_participant(&participants.entries);
    let start = app.view.participant_scroll.min(participants.entries.len());
    let lobby_focused = focus == ChatPanelFocus::Lobby;
    for participant in participants.entries.iter().skip(start).take(visible) {
        let row = rows.take_top(1);
        let selected = lobby_focused
            && lobby_list_focus == LobbyListFocus::Users
            && Some(participant.user_id) == app.view.participant_selected_user;
        let state = if Some(participant.user_id) == app.room.local_user
            && app.view.deafened.load(Ordering::Relaxed)
        {
            "deaf"
        } else if participant.online && Some(participant.user_id) == app.room.local_user {
            "voice"
        } else if participant.online && participant.p2p_direct {
            "p2p"
        } else if participant.online {
            "relay"
        } else {
            "away"
        };
        let uptime = participant
            .presence_since
            .map(age_label)
            .unwrap_or_else(|| "--".to_string());
        let base = if selected {
            app.view.theme.room_selected
        } else {
            app.view.theme.panel_alt
        };
        let style = if selected {
            base.patch(app.view.theme.good)
        } else if participant.online {
            app.view.theme.text
        } else {
            app.view.theme.muted
        };
        let marker = if selected { ">" } else { " " };
        let (status_marker, status_style) = room_user_status_indicator(app, participant);
        let control = room_user_control_label(app, participant);
        let voice = room_user_voice_feedback_label(app.view.lobby_details, participant);
        row.with(base).fill(buf);
        row.with(style).with(Ellipsis(true)).text(
            buf,
            &format!(
                "{marker}   {:<16} {:<7} {:<5} {:<16} {}",
                participant.display_name(),
                state,
                uptime,
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
    app: &mut RenderState<'_>,
    focus: ChatPanelFocus,
    lobby_list_focus: LobbyListFocus,
    layout: &mut RoomLayout,
    buf: &mut Buffer,
    now_ms: u64,
) {
    let mut rows = area;
    let room_height = app.config.ui.room_height.min(rows.h.saturating_sub(2));
    if room_height > 0 {
        let lobby_area = rows.take_top(room_height as i32);
        let (rooms_area, divider_area, users_area) = split_lobby_lists(lobby_area);
        layout.room_list_rect = rooms_area;
        layout.lobby_divider_rect = divider_area;
        layout.user_list_rect = users_area;
        draw_room_list(rooms_area, app, focus, lobby_list_focus, layout, buf);
        divider_area.with(app.view.theme.status_fill).fill(buf);
        draw_user_list(users_area, app, focus, lobby_list_focus, buf);
    }

    if rows.h > 0 {
        let lobby_bar = rows.take_top(1);
        layout.lobby_bar_rect = lobby_bar;
        draw_lobby_bar(
            lobby_bar,
            app,
            focus,
            lobby_list_focus,
            layout.room_list_rect,
            layout.lobby_divider_rect,
            layout.user_list_rect,
            buf,
        );
    }

    if rows.h > 0 {
        draw_chat(rows, app, focus, layout, buf, now_ms);
    } else {
        layout.clear_chat();
    }
}

fn split_lobby_lists(area: Rect) -> (Rect, Rect, Rect) {
    if area.is_empty() {
        return (Rect::EMPTY, Rect::EMPTY, Rect::EMPTY);
    }

    let mut columns = area;
    if area.w < 3 {
        let rooms_width = area.w / 2;
        let rooms = columns.take_left(rooms_width as i32);
        return (rooms, Rect::EMPTY, columns);
    }

    let rooms_width = (area.w / 3).clamp(1, 50);
    let rooms = columns.take_left(rooms_width as i32);
    let divider = columns.take_left(1);
    (rooms, divider, columns)
}

/// Full-height room list for the lobby panel, with one full-row click target
/// per visible room.
fn draw_room_list(
    area: Rect,
    app: &RenderState<'_>,
    focus: ChatPanelFocus,
    lobby_list_focus: LobbyListFocus,
    layout: &mut RoomLayout,
    buf: &mut Buffer,
) {
    if area.is_empty() {
        return;
    }
    let theme = app.view.theme;
    area.with(theme.panel_alt).fill(buf);

    let items = app.room_select_items();
    if items.is_empty() {
        area.with(theme.panel_alt.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, " No rooms known yet.");
        return;
    }

    let visible = area.h as usize;
    let viewed = items.iter().position(|item| item.viewed).unwrap_or(0);
    let start = viewed.saturating_sub(visible.saturating_sub(1));
    let active = focus == ChatPanelFocus::Lobby && lobby_list_focus == LobbyListFocus::Rooms;
    let mut rows = area;
    for item in items.iter().skip(start).take(visible) {
        let full_row = rows.take_top(1);
        layout.room_hits.push((full_row, item.room_id));
        draw_room_list_item(full_row, buf, item, active, &theme);
    }
}

fn draw_room_list_item(
    area: Rect,
    buf: &mut Buffer,
    item: &RoomSelectItem,
    active: bool,
    theme: &Theme,
) {
    let selected = active && item.viewed;
    let base = if selected {
        theme.room_selected
    } else {
        theme.panel_alt
    };
    area.with(base).fill(buf);

    let mut row = area;
    if item.voice {
        draw_status_segment_right(&mut row, buf, base.patch(theme.good), " V ");
    }
    if item.unread > 0 {
        draw_status_segment_right(
            &mut row,
            buf,
            base.patch(theme.warn) | Modifier::BOLD,
            &format!(" {} ", item.unread),
        );
    } else if item.behind_head {
        draw_status_segment_right(&mut row, buf, base.patch(theme.warn), " • ");
    }

    let marker_width = row.w.min(2);
    if marker_width > 0 {
        row.take_left(marker_width as i32)
            .with(base.patch(if item.viewed {
                theme.good
            } else {
                theme.subtle
            }))
            .text(buf, if selected { ">" } else { " " });
    }

    let name_style = if item.viewed {
        base.patch(theme.text | Modifier::BOLD)
    } else if item.unread > 0 || item.behind_head {
        base.patch(theme.warn)
    } else {
        base.patch(theme.muted)
    };
    row.with(name_style)
        .with(Ellipsis(true))
        .text(buf, &item.name);
}

pub(crate) fn draw_room_select_screen(
    app: &mut RenderState<'_>,
    select: &mut FuzzySelect,
    items: &[RoomSelectItem],
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
    draw_room_select(screen, app, select, items, searching, buf);
    draw_status(status_area, app, buf, mode, status_label, capture.as_ref());
    draw_key_preview(key_preview_area, app, buf);
}

fn draw_room_select(
    area: Rect,
    app: &RenderState<'_>,
    select: &mut FuzzySelect,
    items: &[RoomSelectItem],
    searching: bool,
    buf: &mut Buffer,
) {
    area.with(app.view.theme.background).fill(buf);
    let mut body = area;
    if body.h == 0 {
        return;
    }
    if items.is_empty() {
        body.take_top(1)
            .with(app.view.theme.background.patch(app.view.theme.muted))
            .with(Ellipsis(true))
            .text(buf, " No rooms known yet.");
        return;
    }

    if searching {
        let search = body.take_top(1);
        search
            .with(app.view.theme.background.patch(app.view.theme.subtle))
            .with(Ellipsis(true))
            .text(buf, &format!("/{}", select.query()));
    }

    let theme = &app.view.theme;
    select.render(body, 1, buf, |_, item_index, selected, area, buf| {
        if let Some(item) = items.get(item_index) {
            draw_room_select_item(area, buf, item, selected, theme);
        }
    });
}

fn draw_room_select_item(
    area: Rect,
    buf: &mut Buffer,
    item: &RoomSelectItem,
    selected: bool,
    theme: &Theme,
) {
    let base = if selected {
        theme.room_selected
    } else {
        theme.background
    };
    buf.clear_rect(area, base);
    let mut row = area;
    row.take_left(2)
        .with(base.patch(if selected { theme.good } else { theme.subtle }))
        .text(buf, if selected { ">" } else { " " });
    if item.voice {
        draw_status_segment_right(&mut row, buf, base.patch(theme.good), " voice ");
    }
    if item.unread > 0 {
        draw_status_segment_right(
            &mut row,
            buf,
            base.patch(theme.warn) | Modifier::BOLD,
            &format!(" {} ", item.unread),
        );
    } else if item.behind_head {
        draw_status_segment_right(&mut row, buf, base.patch(theme.warn), " • ");
    }
    if item.viewed {
        draw_status_segment_right(&mut row, buf, base.patch(theme.accent), " viewing ");
    }
    let name_style = if item.viewed {
        base.patch(theme.text | Modifier::BOLD)
    } else {
        base.patch(theme.text)
    };
    row.with(name_style)
        .with(Ellipsis(true))
        .text(buf, &item.name);
}

fn draw_server_select(
    area: Rect,
    app: &mut RenderState<'_>,
    select: &mut FuzzySelect,
    searching: bool,
    buf: &mut Buffer,
) {
    area.with(app.view.theme.background).fill(buf);
    let mut body = area;
    if body.h == 0 {
        return;
    }
    if app.server_items().is_empty() {
        draw_server_welcome(body, buf, &app.view.theme);
        return;
    }

    if searching {
        let search = body.take_top(1);
        search
            .with(app.view.theme.background.patch(app.view.theme.subtle))
            .with(Ellipsis(true))
            .text(buf, &format!("/{}", select.query()));
    }

    let theme = &app.view.theme;
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
            key: "chatt pair",
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
        .text(buf, &item.label);
    if rows.h > 0 {
        rows.take_top(1)
            .with(base.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, &format!("  {}", item.username));
    }
    if rows.h > 0 {
        let addr = rows
            .take_top(1)
            .with(base.patch(theme.subtle))
            .with(Ellipsis(true))
            .text(buf, &format!("  {}", item.tcp_addr));
        if !item.require_native_encryption {
            addr.with(base.patch(theme.warn))
                .with(Ellipsis(true))
                .text(buf, "  (encryption not enforced)");
        }
    }
}

fn room_user_voice_feedback_label(lobby_details: bool, participant: &ParticipantState) -> String {
    // Each directional link is owned by the receiver of that audio. Inbound
    // (this user -> me) is my local NetEQ estimate of their stream; outbound
    // (me -> this user) is this user's own inbound estimate for my stream, reported
    // back and attributed to their row. Both reuse the peer's end-to-end RTT
    // (round-trip already covers me -> them), halved to one way. The self row has
    // no peer link and carries neither side, so it renders blank.
    let rtt_ms = participant.peer_rtt_ms;
    let fresh = |feedback: &ParticipantVoiceFeedback| {
        feedback.updated_at.elapsed() <= Duration::from_secs(10)
    };
    let inbound = participant
        .voice_feedback
        .filter(|feedback| participant.voice_active && fresh(feedback));
    let outbound = participant.outbound_feedback.filter(fresh);

    let (inbound, outbound) = if lobby_details {
        (
            inbound.map(|feedback| voice_feedback_stats(&feedback, rtt_ms)),
            outbound.map(|feedback| voice_feedback_stats(&feedback, rtt_ms)),
        )
    } else {
        // Collapsed default: mouth-to-ear-ish estimate per direction.
        (
            inbound.map(|feedback| {
                format!("{}ms", participant_latency_estimate_ms(&feedback, rtt_ms))
            }),
            outbound.map(|feedback| {
                format!("{}ms", participant_latency_estimate_ms(&feedback, rtt_ms))
            }),
        )
    };
    // `inbound -> outbound`, with the arrow kept when one side is absent so the
    // remaining figure's direction stays unambiguous.
    match (inbound, outbound) {
        (Some(inbound), Some(outbound)) => format!("{inbound} -> {outbound}"),
        (Some(inbound), None) => format!("{inbound} ->"),
        (None, Some(outbound)) => format!("-> {outbound}"),
        (None, None) => String::new(),
    }
}

/// Renders one direction's detailed reception stats for the `/stats` lobby view.
fn voice_feedback_stats(feedback: &ParticipantVoiceFeedback, rtt_ms: Option<u16>) -> String {
    let net = match rtt_ms {
        Some(rtt) => format!("net{}", rtt / 2),
        None => "net?".to_string(),
    };
    format!(
        "loss{} jb{}/{} r{} j{} {net}",
        feedback.loss_percent,
        feedback.max_neteq_playout_delay_ms,
        feedback.max_neteq_target_ms,
        feedback.max_output_ring_ms,
        feedback.max_interarrival_jitter_ms,
    )
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

fn room_user_status_indicator(
    app: &RenderState<'_>,
    participant: &ParticipantState,
) -> (&'static str, Style) {
    let local_user = Some(participant.user_id) == app.room.local_user;
    if !participant.online {
        return ("▇", app.view.theme.muted);
    }
    if participant.voice_status.deafened
        || (local_user && app.view.deafened.load(Ordering::Relaxed))
    {
        return ("▇", app.view.theme.error);
    }
    if participant.voice_status.muted || (local_user && app.view.mic_muted.load(Ordering::Relaxed))
    {
        return ("▇", app.view.theme.warn);
    }
    if participant.voice_active {
        return (
            if participant.talking_display {
                "▇"
            } else {
                "░"
            },
            app.view.theme.good,
        );
    }
    ("▇", app.view.theme.muted)
}

fn room_user_control_label(app: &RenderState<'_>, participant: &ParticipantState) -> String {
    if Some(participant.user_id) == app.room.local_user {
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

fn draw_top_bar(
    area: Rect,
    app: &mut RenderState<'_>,
    buf: &mut Buffer,
    capture: Option<&StatsSnapshot>,
) {
    if area.is_empty() {
        return;
    }
    let theme = app.view.theme;
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
        &format!(" {server} "),
    );
    if let Some((label, style)) = connection_status_label(app) {
        draw_status_segment(
            &mut left,
            buf,
            style | Modifier::BOLD,
            &format!(" {label} "),
        );
    }
    if !app.room.local_user_name.trim().is_empty() {
        draw_status_segment(
            &mut left,
            buf,
            theme.status_fill.patch(theme.muted),
            &format!(" {} ", app.room.local_user_name),
        );
    }

    let mut right = area;
    draw_top_bar_voice_buttons(&mut right, app, buf);

    let meter_width = right.w.min(14);
    if meter_width > 0 {
        let meter = right.take_right(meter_width as i32);
        ui::vu::draw_status_vu(meter, buf, capture, &theme);
    }

    app.view.chrome.top_bar.video = draw_video_status_block(&mut right, app, buf);
}

fn draw_video_status_block(row: &mut Rect, app: &RenderState<'_>, buf: &mut Buffer) -> Rect {
    let (style, label) = match app.room.screencast_status.phase {
        ScreencastPhase::Starting => (
            top_bar_active_button_style(app.view.theme, app.view.theme.good),
            " VIDEO starting ".to_string(),
        ),
        ScreencastPhase::Live => (
            top_bar_active_button_style(app.view.theme, app.view.theme.good),
            format!(
                " VIDEO {}/s ",
                format_bytes(app.room.screencast_status.rolling_bytes_per_sec)
            ),
        ),
        ScreencastPhase::Off => (
            top_bar_active_button_style(app.view.theme, app.view.theme.warn),
            " VIDEO OFF ".to_string(),
        ),
        ScreencastPhase::Failed => (
            top_bar_active_button_style(app.view.theme, app.view.theme.error),
            " VIDEO FAILED ".to_string(),
        ),
        ScreencastPhase::Idle => return Rect::EMPTY,
    };
    draw_status_segment_right(row, buf, style | Modifier::BOLD, &label)
}

fn draw_top_bar_voice_buttons(row: &mut Rect, app: &mut RenderState<'_>, buf: &mut Buffer) {
    app.view.chrome.top_bar.deafen = draw_status_segment_right(
        row,
        buf,
        top_bar_voice_button_style(app, LocalVoiceMode::Deafened),
        " DEAF ",
    );
    app.view.chrome.top_bar.mute = draw_status_segment_right(
        row,
        buf,
        top_bar_voice_button_style(app, LocalVoiceMode::Muted),
        " MUTE ",
    );
    app.view.chrome.top_bar.live = draw_status_segment_right(
        row,
        buf,
        top_bar_voice_button_style(app, LocalVoiceMode::Live),
        " LIVE ",
    );
}

fn top_bar_voice_button_style(app: &RenderState<'_>, button: LocalVoiceMode) -> Style {
    let theme = app.view.theme;
    if app.view.local_voice_mode() != button {
        return top_bar_inactive_button_style(theme, button);
    }

    match button {
        LocalVoiceMode::Live => top_bar_active_button_style(theme, theme.good) | Modifier::BOLD,
        LocalVoiceMode::Muted => top_bar_active_button_style(theme, theme.warn) | Modifier::BOLD,
        LocalVoiceMode::Deafened => {
            top_bar_active_button_style(theme, theme.error) | Modifier::BOLD
        }
    }
}

fn top_bar_inactive_button_style(theme: Theme, button: LocalVoiceMode) -> Style {
    match button {
        LocalVoiceMode::Live => theme.status_section.patch(theme.muted),
        LocalVoiceMode::Muted => theme.status_fill.patch(theme.muted),
        LocalVoiceMode::Deafened => theme.selected_line.patch(theme.muted),
    }
}

fn top_bar_active_button_style(theme: Theme, accent: Style) -> Style {
    match accent.fg().or_else(|| accent.bg()) {
        Some(color) => theme.mode_server_edit.with_bg(color),
        None => theme.status_fill.patch(accent),
    }
}

fn draw_lobby_bar(
    area: Rect,
    app: &mut RenderState<'_>,
    focus: ChatPanelFocus,
    lobby_list_focus: LobbyListFocus,
    room_list_rect: Rect,
    divider_rect: Rect,
    user_list_rect: Rect,
    buf: &mut Buffer,
) {
    if area.is_empty() {
        return;
    }
    let lobby_focused = focus == ChatPanelFocus::Lobby;
    let fill = app.view.theme.status_fill;
    area.with(fill).fill(buf);

    let rooms_bar = bar_rect_for(area, room_list_rect);
    if !rooms_bar.is_empty() {
        let (_, label, detail) = section_bar_styles(
            app.view.theme,
            ChatPanelFocus::Lobby,
            lobby_focused && lobby_list_focus == LobbyListFocus::Rooms,
        );
        let room_count = app.room_select_items().len();
        let mut row = rooms_bar;
        draw_status_segment(&mut row, buf, label, " Rooms ");
        draw_status_segment(&mut row, buf, detail, &format!(" {} ", room_count));
    }

    bar_rect_for(area, divider_rect).with(fill).fill(buf);

    let lobby_bar = bar_rect_for(area, user_list_rect);
    if lobby_bar.is_empty() {
        draw_lobby_audio_widget(Rect::EMPTY, app, fill, buf);
        return;
    }

    let (_, label, detail) = section_bar_styles(
        app.view.theme,
        ChatPanelFocus::Lobby,
        lobby_focused && lobby_list_focus == LobbyListFocus::Users,
    );
    let voice_label = match app
        .room
        .voice_room
        .and_then(|room_id| app.room.room_meta(room_id))
    {
        Some(meta) => format!("voice: {} {}", meta.name, meta.voice_users.len()),
        None => "voice: off".to_string(),
    };
    let viewed_room_name = app
        .view
        .viewed_room
        .and_then(|room_id| app.room.room_meta(room_id))
        .map(|room| room.name.as_str())
        .unwrap_or(&app.room.room_name);
    let participants = app.room.participant_snapshot(app.view.viewed_room);
    let mut row = lobby_bar;
    draw_status_segment(&mut row, buf, label, " Lobby ");
    draw_status_segment(
        &mut row,
        buf,
        detail,
        &format!(
            " {} | in call {}/{} | {} ",
            viewed_room_name,
            participants.online_count(),
            participants.entries.len(),
            voice_label
        ),
    );
    draw_lobby_audio_widget(row, app, fill, buf);
}

fn bar_rect_for(bar: Rect, source: Rect) -> Rect {
    if bar.is_empty() || source.is_empty() {
        return Rect::EMPTY;
    }
    Rect {
        x: source.x,
        y: bar.y,
        w: source.w,
        h: bar.h,
    }
    .intersect(bar)
}

/// The audio health widget on the right of the lobby bar: compact device
/// names while healthy, recovery state plus a clickable `[reset]` button
/// while a stream is reconnecting or waiting for a device.
fn draw_lobby_audio_widget(
    remaining: Rect,
    app: &mut RenderState<'_>,
    fill: Style,
    buf: &mut Buffer,
) {
    app.view.chrome.lobby_bar.audio_widget = Rect::EMPTY;
    app.view.chrome.lobby_bar.audio_reset = Rect::EMPTY;
    let theme = app.view.theme;
    let mic = app.room.capture_health.clone();
    let spk = app.room.playback_health.clone();
    let mut right = remaining;

    let mut trouble = Vec::new();
    if let Some(text) = audio_health_status_text("mic", mic.state) {
        trouble.push(text);
    }
    if let Some(text) = audio_health_status_text("spk", spk.state) {
        trouble.push(text);
    }
    if !trouble.is_empty() {
        app.view.chrome.lobby_bar.audio_reset = draw_status_segment_right(
            &mut right,
            buf,
            theme.status_section.patch(theme.warn) | Modifier::BOLD,
            " [reset] ",
        );
        app.view.chrome.lobby_bar.audio_widget = draw_status_segment_right(
            &mut right,
            buf,
            fill.patch(theme.warn),
            &format!(" {} ", trouble.join(" | ")),
        );
        return;
    }

    let mut devices = Vec::new();
    if let Some(name) = spk.device_name.as_deref() {
        devices.push(format!("spk:{}", truncate_device_name(name)));
    }
    if let Some(name) = mic.device_name.as_deref() {
        devices.push(format!("mic:{}", truncate_device_name(name)));
    }
    if devices.is_empty() {
        return;
    }
    app.view.chrome.lobby_bar.audio_widget = draw_status_segment_right(
        &mut right,
        buf,
        fill.patch(theme.muted),
        &format!(" {} ", devices.join(" | ")),
    );
}

fn audio_health_status_text(prefix: &str, state: AudioHealthState) -> Option<String> {
    match state {
        AudioHealthState::Healthy => None,
        AudioHealthState::Settling => Some(format!("{prefix}: reconnecting…")),
        AudioHealthState::Reconnecting { attempt } => {
            Some(format!("{prefix}: reconnecting ({attempt})"))
        }
        AudioHealthState::WaitingForDevice => Some(format!("{prefix}: waiting for device")),
    }
}

fn truncate_device_name(name: &str) -> String {
    const MAX_CHARS: usize = 14;
    if name.chars().count() <= MAX_CHARS {
        return name.to_string();
    }
    let head: String = name.chars().take(MAX_CHARS.saturating_sub(1)).collect();
    format!("{head}…")
}

fn draw_chat_log_bar(area: Rect, app: &RenderState<'_>, focus: ChatPanelFocus, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    let focused = focus == ChatPanelFocus::ChatLog;
    let (fill, label, detail) =
        section_bar_styles(app.view.theme, ChatPanelFocus::ChatLog, focused);
    area.with(fill).fill(buf);
    let mut row = area;
    draw_status_segment(&mut row, buf, label, " Chat Log ");
    draw_status_segment(
        &mut row,
        buf,
        detail,
        &format!(
            " {} msg/{} rows ",
            app.view.active.chat.len(),
            app.view.active.chat.total_lines_estimate()
        ),
    );
}

fn draw_compose_bar(
    area: Rect,
    app: &RenderState<'_>,
    focus: ChatPanelFocus,
    buf: &mut Buffer,
    mode: theme::UiMode,
    status_label: &'static str,
) {
    if area.is_empty() {
        return;
    }
    let focused = focus == ChatPanelFocus::Compose;
    let (fill, mut label, detail) =
        section_bar_styles(app.view.theme, ChatPanelFocus::Compose, focused);
    if focused {
        label = app.view.theme.mode_style(mode) | Modifier::BOLD;
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

fn composer_mode_label(app: &RenderState<'_>) -> &'static str {
    if app.view.has_pending_edit() {
        return match app.view.composer.mode() {
            EditorMode::Normal => "edit normal",
            EditorMode::Insert => "edit insert",
            EditorMode::Visual => "edit visual",
            EditorMode::VisualLine => "edit visual line",
            EditorMode::VisualBlock => "edit visual block",
        };
    }
    match app.view.composer.mode() {
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
    app: &mut RenderState<'_>,
    focus: ChatPanelFocus,
    layout: &mut RoomLayout,
    buf: &mut Buffer,
    now_ms: u64,
) {
    area.with(app.view.theme.background).fill(buf);
    if area.is_empty() {
        layout.clear_chat();
        return;
    }
    // The leftmost column is a marker gutter (`▟`/`▌`), so content wraps to one
    // column less than the full chat width.
    let content_width = area.w.saturating_sub(1).max(1);
    if content_width != layout.chat_width {
        // Reflow invalidates wrapped-line coordinates: the visual anchor is
        // dropped and the cursor's line is clamped.
        app.view.active.chat.on_reflow(content_width);
    }
    layout.chat_width = content_width;
    layout.chat_height = area.h;
    layout.chat_rect = area;
    if app.view.active.chat.is_empty() {
        layout.visible_chat_lines.clear();
        area.with(app.view.theme.subtle)
            .with(HAlign::Center)
            .text(buf, "No messages");
        return;
    }
    // Clamp a stale cursor (eviction, collapse) before styling against it.
    app.view.active.chat.ensure_cursor(content_width);
    app.view.active.chat.visible_lines_into(
        content_width,
        area.h,
        app.config.ui.overscan as usize,
        &mut layout.visible_chat_lines,
    );
    let chat_focused = focus == ChatPanelFocus::ChatLog;
    // Content is top-anchored: lines are drawn from the top of `area` and the
    // already-background-filled rows below them stay empty.
    let mut row_area = area;
    for line in layout.visible_chat_lines.iter().copied() {
        let mut row = row_area.take_top(1);
        let marker = row.take_left(1);
        match line.kind {
            LineKind::Heading => draw_chat_heading(marker, row, app, line.message, now_ms, buf),
            LineKind::Body => {
                let msg = app.view.active.chat.message(line.message);
                // A file message overlays its single body line, keyed by the
                // server transfer id: an in-flight transfer draws a progress bar,
                // one that ended without landing draws a terminal label. `transfer`
                // clones the status, so no borrow of `app.room` outlives this read.
                let status = msg.file_transfer_id.and_then(|id| app.room.transfer(id));
                let visual_here =
                    chat_focused && app.view.active.chat.is_visual(line.message, line.line);
                let cursor_here =
                    chat_focused && app.view.active.chat.is_cursor_line(line.message, line.line);
                let base = if visual_here {
                    app.view.theme.chat_visual_line
                } else if cursor_here {
                    app.view.theme.chat_cursor_line
                } else if msg.local {
                    app.view.theme.local_line
                } else {
                    app.view.theme.background
                };
                let accent = chat_entry_accent(app.view.theme, msg.local, msg.notice_kind);
                marker.with(base).fill(buf);
                // The cursor row's gutter stays identifiable even inside a
                // visual range, where the background alone cannot mark it.
                let marker_style = if cursor_here {
                    base.patch(app.view.theme.text) | extui::vt::Modifier::BOLD
                } else {
                    base.patch(accent)
                };
                marker.with(marker_style).text(buf, "▌");
                row.with(base).fill(buf);
                if line.line == 0
                    && let Some(TransferStatus::Active(progress)) = status
                    && let Some(transfer_id) = msg.file_transfer_id
                {
                    // Copy out the name before the `msg` borrow of `app.room`
                    // ends: recording the button hit-box needs `&mut app`.
                    let name = msg.body.split('`').nth(1).unwrap_or(&msg.body).to_string();
                    draw_transfer_progress(row, base, progress, &name, transfer_id, app, buf);
                } else {
                    for seg in app.view.active.chat.line(line.message, line.line) {
                        let text = app.view.active.chat.segment_text(line.message, seg);
                        let mut style = base.patch(app.view.theme.text).patch(seg.style);
                        if !seg.synth
                            && msg
                                .links
                                .iter()
                                .any(|link| seg.start < link.end && link.start < seg.end)
                        {
                            style = style.patch(app.view.theme.syntax.namespace)
                                | extui::vt::Modifier::UNDERLINED;
                        }
                        let max_width = row.w.saturating_sub(seg.col) as usize;
                        if max_width > 0 {
                            buf.set_stringn(row.x + seg.col, row.y, text, max_width, style);
                        }
                    }
                    // A terminal transfer keeps its plain body and gains a dim
                    // `verb: reason` label where the cancel/skip button used to be.
                    if line.line == 0
                        && let Some(TransferStatus::Terminal { verb, reason }) = &status
                    {
                        draw_transfer_terminal(
                            row,
                            base,
                            *verb,
                            reason.as_deref(),
                            &app.view.theme,
                            buf,
                        );
                    }
                }
            }
            LineKind::Ellipsis => {
                let base = app.view.theme.background;
                marker.with(base).fill(buf);
                row.with(base).fill(buf);
                row.with(app.view.theme.subtle)
                    .with(HAlign::Center)
                    .text(buf, "...");
            }
        }
    }
}

/// Overlays an in-flight file transfer's progress on its chat-line `row`: the
/// filled portion is drawn reversed over a `verb name done/total (pct%)` label,
/// so the bar reads left to right and reverts to the plain body text once the
/// overlay is cleared on completion. A `[cancel]` (outgoing) or `[skip]`
/// (incoming) button is reserved on the right and its hit-box recorded in
/// `chrome.transfer_buttons` for [`App::process_chat_mouse`] to resolve clicks.
fn draw_transfer_progress(
    mut row: Rect,
    base: Style,
    progress: TransferProgress,
    file_name: &str,
    transfer_id: FileTransferId,
    app: &mut RenderState<'_>,
    buf: &mut Buffer,
) {
    if row.w == 0 {
        return;
    }
    let (verb, button_text) = match progress.direction {
        TransferDirection::Incoming => ("receiving", "[skip]"),
        TransferDirection::Outgoing => ("sending", "[cancel]"),
    };
    // Carve a padded right-hand segment for the button, leaving the rest for the
    // bar. On a row too narrow for both, the bar keeps the whole width.
    let button_w = button_text.chars().count() as u16 + 2;
    let button_rect = (row.w > button_w).then(|| row.take_right(button_w as i32));

    let width = row.w as usize;
    let ratio = if progress.total == 0 {
        0.0
    } else {
        (progress.transferred as f64 / progress.total as f64).clamp(0.0, 1.0)
    };
    let pct = (ratio * 100.0).round() as u64;
    let label = format!(
        " {verb} {file_name}  {}/{} ({pct}%)",
        format_bytes(progress.transferred),
        format_bytes(progress.total)
    );
    let filled = (width as f64 * ratio).round() as usize;
    let text_style = base.patch(app.view.theme.text);
    let chars: Vec<char> = label.chars().collect();
    let mut encoded = [0u8; 4];
    for col in 0..width {
        let ch = chars.get(col).copied().unwrap_or(' ');
        let style = if col < filled {
            text_style.with_modifier(Modifier::REVERSED)
        } else {
            text_style
        };
        buf.set_stringn(
            row.x + col as u16,
            row.y,
            ch.encode_utf8(&mut encoded),
            1,
            style,
        );
    }
    if let Some(button_rect) = button_rect {
        let button_style = base
            .patch(app.view.theme.text)
            .with_modifier(Modifier::REVERSED);
        button_rect
            .with(button_style)
            .with(HAlign::Center)
            .text(buf, button_text);
        app.view
            .chrome
            .transfer_buttons
            .push((button_rect, transfer_id));
    }
}

/// Draws the dim `verb: reason` (or bare `verb`) label for a transfer that ended
/// without landing, right-aligned in the band the cancel/skip button occupied.
/// Records no button hit-box, so the terminal line is inert.
fn draw_transfer_terminal(
    row: Rect,
    base: Style,
    verb: TerminalVerb,
    reason: Option<&str>,
    theme: &Theme,
    buf: &mut Buffer,
) {
    if row.w == 0 {
        return;
    }
    let label = match reason {
        Some(reason) => format!("{}: {reason}", verb.label()),
        None => verb.label().to_string(),
    };
    let width = (label.chars().count() as u16).min(row.w);
    let x = row.x + row.w - width;
    buf.set_stringn(x, row.y, &label, width as usize, base.patch(theme.subtle));
}

/// Draws a block heading: the `▟` marker, then the sender name on the left and
/// the relative age on the right, both padded one column inside `row`.
fn draw_chat_heading(
    marker: Rect,
    row: Rect,
    app: &RenderState<'_>,
    message: usize,
    now_ms: u64,
    buf: &mut Buffer,
) {
    let msg = app.view.active.chat.message(message);
    let base = if msg.local {
        app.view.theme.local_line
    } else {
        app.view.theme.background
    };
    let accent = chat_entry_accent(app.view.theme, msg.local, msg.notice_kind);
    marker.with(base).fill(buf);
    marker.with(base.patch(accent)).text(buf, "▟");
    row.with(base).fill(buf);
    let content = row.inset(1, 0);
    let mut name = if msg.edited {
        format!("{} (edited)", msg.sender)
    } else {
        msg.sender.clone()
    };
    if app.view.active.chat.is_collapsed(message) {
        name.push_str(" (Collapsed)");
    } else if app.view.active.chat.is_expanded(message) {
        name.push_str(" (Expanded)");
    }
    content
        .with(base.patch(accent))
        .with(Ellipsis(true))
        .text(buf, &name);
    let age = chat_age(msg.timestamp_ms, now_ms);
    if !age.is_empty() {
        content
            .with(base.patch(app.view.theme.subtle))
            .with(HAlign::Right)
            .text(buf, &age);
    }
}

fn chat_entry_accent(theme: Theme, local: bool, notice_kind: Option<NoticeKind>) -> Style {
    match notice_kind {
        Some(NoticeKind::Info) => theme.muted,
        Some(NoticeKind::Error) => theme.error,
        None if local => theme.good,
        None => theme.accent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::ids::{StreamId, UserId};

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
    fn voice_label_shows_both_directions_with_arrow() {
        let mut participant = ParticipantState::for_test(UserId(1));
        participant.peer_rtt_ms = Some(40);
        participant.active_stream = Some(StreamId(7));
        participant.voice_feedback = Some(feedback(60, 10)); // inbound: 60 + 10 + 20 = 90
        participant.outbound_feedback = Some(feedback(30, 10)); // outbound: 30 + 10 + 20 = 60

        assert_eq!(
            room_user_voice_feedback_label(false, &participant),
            "90ms -> 60ms"
        );
    }

    #[test]
    fn voice_label_keeps_arrow_for_single_present_direction() {
        let mut participant = ParticipantState::for_test(UserId(1));
        participant.peer_rtt_ms = Some(40);
        participant.outbound_feedback = Some(feedback(30, 10));

        // Only outbound present (no inbound stream/report yet).
        assert_eq!(
            room_user_voice_feedback_label(false, &participant),
            "-> 60ms"
        );

        // Only inbound present.
        participant.outbound_feedback = None;
        participant.voice_feedback = Some(feedback(60, 10));
        assert_eq!(
            room_user_voice_feedback_label(false, &participant),
            "90ms ->"
        );
    }

    #[test]
    fn voice_label_stats_view_joins_both_directions() {
        let mut participant = ParticipantState::for_test(UserId(1));
        participant.peer_rtt_ms = Some(40);
        participant.voice_feedback = Some(feedback(60, 10));
        participant.outbound_feedback = Some(feedback(30, 10));

        let label = room_user_voice_feedback_label(true, &participant);
        assert!(
            label.contains(" -> "),
            "stats view joins directions: {label}"
        );
        assert_eq!(label.matches("net20").count(), 2);
    }

    #[test]
    fn top_bar_active_voice_buttons_use_state_fill_and_badge_foreground() {
        let theme = Theme::tomorrow_night();
        for accent in [theme.good, theme.warn, theme.error] {
            let style = top_bar_active_button_style(theme, accent);
            assert_eq!(style.bg(), accent.fg());
            assert_eq!(style.fg(), theme.mode_server_edit.fg());
        }
    }

    #[test]
    fn top_bar_inactive_voice_buttons_use_visible_grey_backgrounds() {
        let theme = Theme::base16_dark();
        let live = top_bar_inactive_button_style(theme, LocalVoiceMode::Live);
        let mute = top_bar_inactive_button_style(theme, LocalVoiceMode::Muted);
        let deaf = top_bar_inactive_button_style(theme, LocalVoiceMode::Deafened);

        assert!(live.bg().is_some());
        assert!(mute.bg().is_some());
        assert!(deaf.bg().is_some());
        assert_ne!(live.bg(), mute.bg());
        assert_ne!(mute.bg(), deaf.bg());
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

    #[test]
    fn password_key_preview_uses_password_layer_only() {
        let runtime = bindings::BindingRuntime::default();
        let mut entries = Vec::new();

        build_key_preview_entries(&runtime, &None, bindings::PASSWORD_LAYER, &mut entries);

        assert!(
            entries.iter().any(|entry| entry.label == "Reveal"),
            "password layer should expose the reveal toggle"
        );
        assert!(
            entries.iter().all(|entry| entry.key_hint != "q"),
            "picker cancel binding must not leak into password prompt"
        );
        assert!(
            entries.iter().all(|entry| entry.key_hint != "/"),
            "picker search binding must not leak into password prompt"
        );
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
    app: &RenderState<'_>,
    buf: &mut Buffer,
    mode: theme::UiMode,
    label: &'static str,
    _capture: Option<&StatsSnapshot>,
) {
    if area.is_empty() {
        return;
    }
    let theme = &app.view.theme;
    area.with(theme.status_fill).fill(buf);
    let mut row = area;
    draw_status_segment(&mut row, buf, theme.mode_style(mode), &format!(" {label} "));
    draw_status_text_right(row, app, buf, theme.status_fill);
}

fn draw_status_text_right(area: Rect, app: &RenderState<'_>, buf: &mut Buffer, fill: Style) {
    let theme = &app.view.theme;
    let right_style = match app.view.status.kind() {
        StatusKind::Info => fill.patch(theme.muted),
        StatusKind::Error => fill.patch(theme.error),
    };
    let status_text = if let Some(chord) = &app.view.chrome.binding.pending_chord {
        format!(
            "{} chord {}ms",
            chord.label.as_deref().unwrap_or("pending"),
            chord.activated_at.elapsed().as_millis()
        )
    } else {
        app.view.status.text().to_string()
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

fn key_preview_height(app: &RenderState<'_>, width: u16) -> u16 {
    let entries = &app.view.chrome.key_preview.cache.entries;
    if entries.is_empty() {
        return 0;
    }
    let rows = key_preview_rows(entries, width);
    if app.view.chrome.key_preview.expanded && rows.len() > 1 {
        rows.len().try_into().unwrap_or(u16::MAX)
    } else {
        1
    }
}

fn draw_key_preview(area: Rect, app: &RenderState<'_>, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }

    let entries = &app.view.chrome.key_preview.cache.entries;
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
        let label = if app.view.chrome.key_preview.expanded {
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

pub(crate) fn draw_overlay_key_preview(
    app: &mut RenderState<'_>,
    layer: LayerId,
    buf: &mut Buffer,
) {
    let mut screen = buf.rect();
    let previous_height = key_preview_height(app, screen.w);
    refresh_key_preview_cache(app, Some(layer));
    let height = key_preview_height(app, screen.w);
    let clear_height = previous_height.max(height);
    if clear_height == 0 {
        return;
    }

    let clear = screen.take_bottom(clear_height as i32);
    clear.with(key_preview_bar_style()).fill(buf);
    if height == 0 {
        return;
    }

    let preview = Rect {
        x: clear.x,
        y: clear.y.saturating_add(clear.h.saturating_sub(height)),
        w: clear.w,
        h: height,
    };
    draw_key_preview(preview, app, buf);
}

/// Rebuilds the shared key-preview cache when the active binding layer or pending
/// chord layer has changed since the last render, and is a cheap key comparison
/// otherwise. The stored entries back both [`key_preview_height`] and
/// [`draw_key_preview`], which read them without recomputing.
fn refresh_key_preview_cache(app: &mut RenderState<'_>, base: Option<LayerId>) {
    let pending = app
        .view
        .chrome
        .binding
        .pending_chord
        .as_ref()
        .map(|chord| chord.layer);
    let key = Some((base, pending));
    if app.view.chrome.key_preview.cache.key == key {
        return;
    }
    app.view.chrome.key_preview.cache.key = key;
    let mut entries = std::mem::take(&mut app.view.chrome.key_preview.cache.entries);
    entries.clear();
    if let Some(layer) = base {
        build_key_preview_entries(
            &app.config.bindings,
            &app.view.chrome.binding.pending_chord,
            layer,
            &mut entries,
        );
    }
    app.view.chrome.key_preview.cache.entries = entries;
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

fn draw_status_segment(row: &mut Rect, buf: &mut Buffer, style: Style, text: &str) -> Rect {
    if row.is_empty() {
        return Rect::EMPTY;
    }
    let width = UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16;
    let area = row.take_left(width as i32);
    area.with(style).with(Ellipsis(true)).text(buf, text);
    area
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

fn draw_composer(area: Rect, app: &mut RenderState<'_>, focus: ChatPanelFocus, buf: &mut Buffer) {
    if area.is_empty() {
        return;
    }
    area.with(app.view.theme.panel).fill(buf);
    app.view.composer.resize(area.w.max(1));
    app.view
        .refresh_command_completion(focus == ChatPanelFocus::Compose, app.view.theme.subtle);
    app.view
        .composer_hl
        .render(&mut app.view.composer, area, buf, &app.view.theme);
    if focus != ChatPanelFocus::Compose {
        buf.hide_cursor();
    }
    if app.view.composer.text_len() == 0 {
        area.with(app.view.theme.muted)
            .with(Ellipsis(true))
            .text(buf, &format!(" {}", app.config.ui.placeholder));
    }
}

/// The top-bar status suffix and its style, or `None` in the healthy connected
/// state (where only the server name shows). Failures take priority over the
/// transient "Connecting" phase.
fn connection_status_label(app: &RenderState<'_>) -> Option<(&'static str, Style)> {
    let theme = app.view.theme;
    if app.is_offline() {
        Some(("Offline", theme.status_fill.patch(theme.error)))
    } else if app.room.local_user.is_none() {
        Some(("Connecting", theme.status_fill.patch(theme.muted)))
    } else if app.is_udp_unreachable() {
        Some((
            "UDP Connection Failure",
            theme.status_fill.patch(theme.error),
        ))
    } else {
        None
    }
}

pub(crate) fn age_label(instant: Instant) -> String {
    let secs = instant.elapsed().as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
