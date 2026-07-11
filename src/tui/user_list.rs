use extui::{
    Buffer, Ellipsis, HAlign, Rect,
    event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    vt::Modifier,
};
use rpc::ids::UserId;
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
use crate::app::App;

use crate::{
    app::{
        command::CoreCommand,
        room::{UserListRow, UserPresence},
    },
    bindings::{self, BindCommand},
    fuzzy::fuzzy_score,
    theme,
    tui::{
        Action,
        form::rect_contains,
        mode::{
            AppMode, ChromeSpec, Coverage, ModePresentation, ModeTransition, ViewCx, is_quit_key,
        },
        modes::{BindingResolution, process_global_command_cx, resolve_binding_cx},
        render::age_label,
    },
};

/// The last row the selection pointed at. The index is the position in the
/// filtered list and the id the user occupying it at the time; either half may
/// go stale as presence changes or the filter narrows, and
/// [`UserListMode::resolved_index`] arbitrates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    index: usize,
    user_id: UserId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wrap {
    Cycle,
    Clamp,
}

enum DisplayLine {
    OnlineHeader,
    AwayHeader,
    User(usize),
}

/// Server-wide user directory popup: presence-grouped, filterable by user or
/// room name, opened from the room screen with the `OpenUserList` binding.
pub(crate) struct UserListMode {
    filter: String,
    searching: bool,
    /// The filtered rows the last refresh produced, in display order; actions
    /// and selection index into this list.
    visible: Vec<UserListRow>,
    selected: Option<Selection>,
    scroll: usize,
    page_rows: usize,
    row_hits: Vec<(Rect, usize)>,
}

impl UserListMode {
    pub(crate) fn new() -> Self {
        Self {
            filter: String::new(),
            searching: false,
            visible: Vec::new(),
            selected: None,
            scroll: 0,
            page_rows: 10,
            row_hits: Vec::new(),
        }
    }

    /// Replaces the visible list with the rows matching the filter, then
    /// re-anchors the selection: an id still visible keeps the selection and
    /// corrects its index, while a vanished id keeps the stale pair so a later
    /// movement can recover directionally.
    fn apply_rows(&mut self, rows: Vec<UserListRow>) {
        let filter = self.filter.as_str();
        self.visible = rows
            .into_iter()
            .filter(|row| row_matches(filter, row))
            .collect();
        match &mut self.selected {
            Some(selection) => {
                if let Some(index) = self
                    .visible
                    .iter()
                    .position(|row| row.user_id == selection.user_id)
                {
                    selection.index = index;
                }
            }
            None => {
                if let Some(first) = self.visible.first() {
                    self.selected = Some(Selection {
                        index: 0,
                        user_id: first.user_id,
                    });
                }
            }
        }
    }

    /// The row the selection lands on right now: the stored index when the id
    /// still occupies it, else wherever the id moved, else the stored index
    /// clamped to the list.
    fn resolved_index(&self) -> Option<usize> {
        let selection = self.selected?;
        if self
            .visible
            .get(selection.index)
            .is_some_and(|row| row.user_id == selection.user_id)
        {
            return Some(selection.index);
        }
        if let Some(index) = self
            .visible
            .iter()
            .position(|row| row.user_id == selection.user_id)
        {
            return Some(index);
        }
        if self.visible.is_empty() {
            return None;
        }
        Some(selection.index.min(self.visible.len() - 1))
    }

    fn selected_row(&self) -> Option<&UserListRow> {
        self.visible.get(self.resolved_index()?)
    }

    fn select_index(&mut self, index: usize) {
        let Some(row) = self.visible.get(index) else {
            return;
        };
        self.selected = Some(Selection {
            index,
            user_id: row.user_id,
        });
    }

    /// Moves the selection by `delta` rows. When the selected user is still
    /// visible this steps from their position; when they vanished from view,
    /// moving down lands on the row that slid into their place and moving up
    /// on the row above it, per the direction at the time of the key press.
    fn move_selection(&mut self, delta: isize, wrap: Wrap) {
        if self.visible.is_empty() {
            return;
        }
        let len = self.visible.len();
        let next = match self.selected {
            None => {
                if delta < 0 {
                    len - 1
                } else {
                    0
                }
            }
            Some(selection) => {
                let live = self
                    .visible
                    .iter()
                    .position(|row| row.user_id == selection.user_id);
                match live {
                    Some(index) => {
                        let stepped = index as isize + delta;
                        match wrap {
                            Wrap::Cycle => stepped.rem_euclid(len as isize) as usize,
                            Wrap::Clamp => stepped.clamp(0, len as isize - 1) as usize,
                        }
                    }
                    None => {
                        let anchor = selection.index.min(len - 1);
                        if delta < 0 {
                            anchor.saturating_sub(delta.unsigned_abs())
                        } else {
                            (anchor + delta.unsigned_abs() - 1).min(len - 1)
                        }
                    }
                }
            }
        };
        self.select_index(next);
    }

    pub(crate) fn process_action_cx(
        &mut self,
        cx: &mut ViewCx<'_>,
        command: BindCommand,
    ) -> Action {
        use BindCommand::*;
        match command {
            SelectNext => self.move_selection(1, Wrap::Cycle),
            SelectPrev => self.move_selection(-1, Wrap::Cycle),
            HalfPageDown => self.move_selection((self.page_rows / 2).max(1) as isize, Wrap::Clamp),
            HalfPageUp => self.move_selection(-((self.page_rows / 2).max(1) as isize), Wrap::Clamp),
            Activate => self.activate(cx),
            StartDm => self.start_dm(cx),
            SearchServers => {
                self.searching = true;
                self.filter.clear();
            }
            Cancel => cx.request_transition(ModeTransition::Pop),
            command => return process_global_command_cx(cx, command),
        }
        Action::Continue
    }

    #[cfg(test)]
    pub(crate) fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        let action = {
            let mut cx = app.view_cx();
            self.process_action_cx(&mut cx, command)
        };
        app.drain_core_commands();
        action
    }

    /// Joins the selected user's room: view it and move the voice call there.
    fn activate(&mut self, cx: &mut ViewCx<'_>) {
        let Some(row) = self.selected_row() else {
            cx.set_error("no user selected");
            return;
        };
        match &row.presence {
            UserPresence::Online {
                room: Some((room_id, _)),
            } => {
                let room_id = *room_id;
                cx.send(CoreCommand::SetViewedRoom(room_id));
                cx.send(CoreCommand::JoinVoice(room_id));
                cx.request_transition(ModeTransition::Pop);
            }
            UserPresence::Online { room: None } => {
                cx.set_error(format!("{} is not in a room", row.name));
            }
            UserPresence::AwaySeen { .. } | UserPresence::AwayUnseen => {
                cx.set_error(format!("{} is away", row.name));
            }
        }
    }

    fn start_dm(&mut self, cx: &mut ViewCx<'_>) {
        let Some(row) = self.selected_row() else {
            cx.set_error("no user selected");
            return;
        };
        if row.is_local {
            cx.set_error("cannot dm yourself");
            return;
        }
        cx.send(CoreCommand::OpenDm(row.user_id));
        cx.request_transition(ModeTransition::Pop);
    }

    fn edit_filter(&mut self, key: KeyEvent) -> bool {
        let mut modifiers = key.modifiers;
        modifiers.remove(KeyModifiers::SHIFT);
        if !modifiers.is_empty() {
            return false;
        }
        match key.code {
            KeyCode::Backspace => {
                self.filter.pop();
                true
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                self.filter.push(ch);
                true
            }
            _ => false,
        }
    }

    fn display_lines(&self) -> Vec<DisplayLine> {
        let mut lines = Vec::new();
        let mut away_started = false;
        for (index, row) in self.visible.iter().enumerate() {
            let online = matches!(row.presence, UserPresence::Online { .. });
            if online && lines.is_empty() {
                lines.push(DisplayLine::OnlineHeader);
            }
            if !online && !away_started {
                away_started = true;
                lines.push(DisplayLine::AwayHeader);
            }
            lines.push(DisplayLine::User(index));
        }
        lines
    }

    fn keep_selected_visible(&mut self, lines: &[DisplayLine], visible_rows: usize) {
        let visible_rows = visible_rows.max(1);
        let selected = self.resolved_index();
        let selected_line = selected.and_then(|index| {
            lines
                .iter()
                .position(|line| matches!(line, DisplayLine::User(user) if *user == index))
        });
        if let Some(line) = selected_line {
            if line < self.scroll {
                self.scroll = line;
            } else if line >= self.scroll + visible_rows {
                self.scroll = line + 1 - visible_rows;
            }
        }
        self.scroll = self.scroll.min(lines.len().saturating_sub(visible_rows));
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.searching = false,
                _ => {
                    let _ = self.edit_filter(key);
                }
            }
            return Action::Continue;
        }
        match resolve_binding_cx(cx, bindings::USER_LIST_LAYER, key) {
            BindingResolution::Action(command) => self.process_action_cx(cx, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self
                    .row_hits
                    .iter()
                    .find(|(rect, _)| rect_contains(*rect, mouse.column, mouse.row))
                    .map(|(_, index)| *index);
                let Some(index) = hit else {
                    return Action::Continue;
                };
                if self.resolved_index() == Some(index) {
                    self.activate(cx);
                } else {
                    self.select_index(index);
                }
            }
            MouseEventKind::ScrollDown => self.move_selection(1, Wrap::Clamp),
            MouseEventKind::ScrollUp => self.move_selection(-1, Wrap::Clamp),
            _ => {}
        }
        Action::Continue
    }
}

fn row_matches(filter: &str, row: &UserListRow) -> bool {
    if filter.is_empty() {
        return true;
    }
    if fuzzy_score(filter, &row.name).is_some() {
        return true;
    }
    let UserPresence::Online {
        room: Some((_, room_name)),
    } = &row.presence
    else {
        return false;
    };
    fuzzy_score(filter, room_name).is_some()
}

impl AppMode for UserListMode {
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, _now_ms: u64) {
        self.apply_rows(cx.session.user_list_rows());
        let mut app = crate::tui::render::RenderState::new(cx);
        self.row_hits.clear();
        let theme = &app.view.theme;
        let area = buf.rect();
        if area.w < 24 || area.h < 7 {
            return;
        }
        let lines = self.display_lines();
        let width = area.w.min(60).max(24);
        let body_rows = lines
            .len()
            .max(1)
            .min(area.h.saturating_sub(4) as usize)
            .max(3);
        let height = (body_rows + 4) as u16;
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        let title = if app.room.server_alias.is_empty() {
            "Users".to_string()
        } else {
            format!("Users — {}", app.room.server_alias)
        };
        let mut rows = crate::tui::render::draw_dialog_frame(panel, buf, theme, &title);

        let search = rows.take_top(1);
        if self.searching {
            search.with(theme.join_input_active).fill(buf);
            search
                .with(theme.join_input_active)
                .with(Ellipsis(true))
                .text(buf, &format!(" /{}", self.filter));
        } else if !self.filter.is_empty() {
            search
                .with(theme.dialog_panel.patch(theme.subtle))
                .with(Ellipsis(true))
                .text(buf, &format!(" /{}", self.filter));
        }

        let body = rows;
        self.page_rows = body.h as usize;
        self.keep_selected_visible(&lines, body.h as usize);

        if self.visible.is_empty() {
            let notice = if self.filter.is_empty() {
                "no users known yet"
            } else {
                "no matching users"
            };
            let mut body = body;
            body.take_top(1)
                .with(theme.dialog_panel.patch(theme.muted))
                .with(HAlign::Center)
                .with(Ellipsis(true))
                .text(buf, notice);
        } else {
            let selected_index = self.resolved_index();
            let mut body = body;
            for line in lines.iter().skip(self.scroll).take(body.h as usize) {
                let line_rect = body.take_top(1);
                match line {
                    DisplayLine::OnlineHeader => draw_header(line_rect, buf, theme, " online"),
                    DisplayLine::AwayHeader => draw_header(line_rect, buf, theme, " away"),
                    DisplayLine::User(index) => {
                        let row = &self.visible[*index];
                        let selected = selected_index == Some(*index);
                        draw_user_row(line_rect, buf, theme, row, selected);
                        self.row_hits.push((line_rect, *index));
                    }
                }
            }
        }

        crate::tui::render::draw_overlay_key_preview(&mut app, bindings::USER_LIST_LAYER, buf);
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        self.process_mouse_cx(cx, mouse)
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation {
            coverage: Coverage::Overlay,
            chrome: Some(ChromeSpec {
                theme_mode: theme::UiMode::ServerSelect,
                status_label: "Users",
                layer: bindings::USER_LIST_LAYER,
            }),
        }
    }
}

#[cfg(test)]
impl UserListMode {
    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        let action = {
            let mut cx = app.view_cx();
            AppMode::process_input(self, &mut cx, key)
        };
        app.drain_core_commands();
        action
    }
}

fn draw_header(area: Rect, buf: &mut Buffer, theme: &theme::Theme, label: &str) {
    area.with(theme.status_section).fill(buf);
    area.with(theme.status_section)
        .with(Ellipsis(true))
        .text(buf, label);
}

fn draw_user_row(
    area: Rect,
    buf: &mut Buffer,
    theme: &theme::Theme,
    row: &UserListRow,
    selected: bool,
) {
    let base = if selected {
        theme.row_focused
    } else {
        theme.dialog_panel
    };
    area.with(base).fill(buf);
    let mut rect = area;
    rect.take_left(1);
    rect.take_left(2)
        .with(base.patch(if selected { theme.good } else { theme.subtle }))
        .text(buf, if selected { "> " } else { "  " });

    let (label, label_style) = match &row.presence {
        UserPresence::Online {
            room: Some((_, room_name)),
        } => (room_name.clone(), theme.accent),
        UserPresence::Online { room: None } => (String::new(), theme.muted),
        UserPresence::AwaySeen { since } => (age_label(*since), theme.muted),
        UserPresence::AwayUnseen => (String::new(), theme.muted),
    };
    if !label.is_empty() {
        let width = (label.width() as i32 + 1).min(rect.w as i32 / 2);
        rect.take_right(width)
            .with(base.patch(label_style))
            .with(HAlign::Right)
            .with(Ellipsis(true))
            .text(buf, &label);
    }

    let name = if row.is_local {
        format!("{} (you)", row.name)
    } else {
        row.name.clone()
    };
    let name_style = if row.is_local {
        base.patch(theme.text | Modifier::BOLD)
    } else {
        base.patch(theme.text)
    };
    rect.with(name_style).with(Ellipsis(true)).text(buf, &name);
}

#[cfg(test)]
mod tests {
    use extui::event::{KeyCode, KeyEvent, KeyModifiers};
    use rpc::ids::RoomId;

    use super::*;
    use crate::config::Config;

    fn online(id: u64, name: &str, room: Option<(u32, &str)>) -> UserListRow {
        UserListRow {
            user_id: UserId(id),
            name: name.to_string(),
            presence: UserPresence::Online {
                room: room.map(|(room_id, room_name)| (RoomId(room_id), room_name.to_string())),
            },
            is_local: false,
        }
    }

    fn away(id: u64, name: &str) -> UserListRow {
        UserListRow {
            user_id: UserId(id),
            name: name.to_string(),
            presence: UserPresence::AwayUnseen,
            is_local: false,
        }
    }

    fn names(mode: &UserListMode) -> Vec<&str> {
        mode.visible.iter().map(|row| row.name.as_str()).collect()
    }

    fn app() -> App {
        App::new(Config::default(), None).expect("test app")
    }

    #[test]
    fn selection_follows_user_id_when_index_shifts() {
        let mut mode = UserListMode::new();
        mode.apply_rows(vec![online(1, "ann", None), online(2, "bob", None)]);
        mode.move_selection(1, Wrap::Cycle);
        assert_eq!(
            mode.selected,
            Some(Selection {
                index: 1,
                user_id: UserId(2)
            })
        );

        mode.apply_rows(vec![
            online(3, "abe", None),
            online(1, "ann", None),
            online(2, "bob", None),
        ]);

        assert_eq!(
            mode.selected,
            Some(Selection {
                index: 2,
                user_id: UserId(2)
            })
        );
    }

    #[test]
    fn movement_down_after_selected_user_vanishes_picks_row_below() {
        let mut mode = UserListMode::new();
        mode.apply_rows(vec![
            online(1, "ann", None),
            online(2, "bob", None),
            online(3, "cid", None),
        ]);
        mode.move_selection(1, Wrap::Cycle);
        assert_eq!(mode.selected_row().unwrap().name, "bob");

        mode.apply_rows(vec![online(1, "ann", None), online(3, "cid", None)]);
        mode.move_selection(1, Wrap::Cycle);

        assert_eq!(mode.selected_row().unwrap().name, "cid");
    }

    #[test]
    fn movement_up_after_selected_user_vanishes_picks_row_above() {
        let mut mode = UserListMode::new();
        mode.apply_rows(vec![
            online(1, "ann", None),
            online(2, "bob", None),
            online(3, "cid", None),
        ]);
        mode.move_selection(1, Wrap::Cycle);
        assert_eq!(mode.selected_row().unwrap().name, "bob");

        mode.apply_rows(vec![online(1, "ann", None), online(3, "cid", None)]);
        mode.move_selection(-1, Wrap::Cycle);

        assert_eq!(mode.selected_row().unwrap().name, "ann");
    }

    #[test]
    fn filter_matches_user_or_room_name_and_keeps_order() {
        let mut mode = UserListMode::new();
        mode.filter = "lobby".to_string();
        mode.apply_rows(vec![
            online(1, "ann", Some((1, "lobby"))),
            online(2, "bob", Some((2, "games"))),
            online(3, "lobbyist", None),
            away(4, "dan"),
        ]);

        assert_eq!(names(&mode), ["ann", "lobbyist"]);
    }

    #[test]
    fn activate_on_away_user_keeps_the_popup_open() {
        let mut app = app();
        let mut mode = UserListMode::new();
        mode.apply_rows(vec![away(1, "ann")]);

        mode.process_action(&mut app, BindCommand::Activate);

        assert!(app.view.pending_transition.is_empty());
        assert_eq!(app.view.status.text(), "ann is away");
    }

    #[test]
    fn search_typing_edits_filter_instead_of_moving_selection() {
        let mut app = app();
        let mut mode = UserListMode::new();
        mode.apply_rows(vec![online(1, "ann", None), online(2, "jim", None)]);
        mode.searching = true;

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()),
        );

        assert_eq!(mode.filter, "j");
        assert_eq!(mode.selected_row().unwrap().name, "ann");
    }
}
