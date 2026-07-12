use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use extui::{Buffer, Rect};
use extui_bindings::{InputKey, LayerId};
use extui_editor::{Editor, Mode as EditorMode, Span as EditorSpan};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    app::{
        App, ChatPanelFocus, DeleteSelection, PendingJoin, RoomSettingsDraft, ServerEditDraft,
        ServerEditEvent, ToggleExpandResult, UserVolumeDialog,
        command::{CoreCommand, SettingsOp},
    },
    bindings::{self, BindCommand, Resolved},
    chat_buffer::{Cursor as ChatCursor, LineKind, VisibleLine},
    client_channel::DirtySections,
    settings::{self, AudioInputPickerState, AudioOutputPickerState, SettingsDraft},
    theme,
    tui::{
        editor::{composer_offset_at_visual_row, composer_visual_position},
        form::{FormAction, FormFieldKind, FormMouseIntent, FormState},
        history_search::{HistorySearch, SearchAction},
        mode::{
            AppMode, ChromeSpec, Coverage, ExitReason, ModePresentation, ModeTransition, ViewCx,
            is_quit_key,
        },
        overlay::{ConfirmDisposition, ConfirmMode, DialogMode, PasteImageUploadMode},
        room_settings::RoomSettingsMode,
        user_list::UserListMode,
        widgets::{ScrollbarDrag, ScrollbarId, ScrollbarLayout},
    },
    ui::{
        select::FuzzySelect,
        settings::{FieldId, FieldIntent},
        welcome::{self, WelcomeButton, WelcomeDraft, WelcomeOutput},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Continue,
    Quit,
}

pub(crate) struct WelcomeMode {
    form: FormState<FieldId>,
    draft: WelcomeDraft,
    pending_join: Option<PendingJoin>,
    config_path_text: String,
    data_dir_text: String,
}

impl WelcomeMode {
    pub(crate) fn new(app: &App, pending_join: Option<PendingJoin>) -> Self {
        let draft = WelcomeDraft::privacy_first();
        Self {
            form: FormState::new(welcome::initial_focus(), draft.default_bindings),
            draft,
            pending_join,
            config_path_text: app
                .config
                .config_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            data_dir_text: crate::paths::client_data_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
        }
    }

    fn drive(
        &mut self,
        cx: &mut ViewCx<'_>,
        intent: FieldIntent,
        commit: Option<(FieldId, String)>,
        focus_column: Option<u16>,
    ) {
        let output = welcome::welcome_logic(
            &mut self.form,
            &mut self.draft,
            &cx.view.theme,
            &cx.config.bindings,
            intent,
            commit,
            focus_column,
        );
        self.handle_output(cx, output);
    }

    fn handle_output(&mut self, cx: &mut ViewCx<'_>, output: WelcomeOutput) {
        if output.changed {
            let _ = self.form.set_bindings(self.draft.default_bindings);
            cx.view.apply_theme(crate::theme::Theme::resolve(
                &self.draft.theme,
                &cx.config.ui.themes,
            ));
        }
        match output.button {
            Some(WelcomeButton::Save) => self.save_and_continue(cx),
            Some(WelcomeButton::Exit) => cx.send(CoreCommand::Quit),
            None => {}
        }
    }

    fn save_and_continue(&mut self, cx: &mut ViewCx<'_>) {
        let commit = self.form.clear_text();
        self.drive(cx, FieldIntent::None, commit, None);
        cx.send(CoreCommand::SaveWelcome {
            draft: self.draft.clone(),
            pending_join: self.pending_join.clone(),
        });
    }

    fn process_action(&mut self, cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
        match command {
            BindCommand::SaveSettings => self.save_and_continue(cx),
            BindCommand::Cancel | BindCommand::CloseSettings => {
                cx.set_status("save setup to continue");
            }
            BindCommand::Quit => return Action::Quit,
            _ => return process_global_command_cx(cx, command),
        }
        Action::Continue
    }

    fn resolve_binding(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        match resolve_binding_cx(cx, bindings::SETTINGS_LAYER, key) {
            BindingResolution::Action(command) => self.process_action(cx, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if is_control_save_chord_cx(cx, &key) {
            self.save_and_continue(cx);
            return Action::Continue;
        }

        let kind = self.form.focused_kind();
        let text_focused = kind == FormFieldKind::Text;
        let event = self.form.handle_key(key, kind);
        match event.action {
            FormAction::None if !text_focused => return self.resolve_binding(cx, key),
            FormAction::None => self.drive(cx, FieldIntent::None, event.commit, None),
            FormAction::Cancel => cx.set_status("save setup to continue"),
            FormAction::ActivateNextInsert => {
                self.drive(cx, FieldIntent::None, event.commit, None);
                let commit = self.form.move_focus(1);
                self.drive(cx, FieldIntent::None, commit, None);
                self.form.enter_insert_mode();
            }
            FormAction::MoveFocus(delta) => {
                let commit = self.form.move_focus(delta);
                self.drive(cx, FieldIntent::None, commit, None);
            }
            FormAction::Activate if text_focused => {
                self.drive(cx, FieldIntent::None, event.commit, None);
                let commit = self.form.move_focus(1);
                self.drive(cx, FieldIntent::None, commit, None);
            }
            FormAction::Activate => {
                self.drive(cx, FieldIntent::Activate, event.commit, None);
            }
            FormAction::Adjust(delta) => {
                self.drive(cx, FieldIntent::Adjust(delta), event.commit, None);
            }
            FormAction::FocusMoved | FormAction::Scrolled => {
                self.drive(cx, FieldIntent::None, event.commit, None);
            }
            FormAction::TextChanged => {}
        }
        Action::Continue
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        let event = self.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::None | FormMouseIntent::PickerItem(_, _) => {
                self.drive(cx, FieldIntent::None, event.commit, None);
            }
            FormMouseIntent::Activate(_) => {
                self.drive(cx, FieldIntent::Activate, event.commit, None);
            }
            FormMouseIntent::Adjust(_, delta) => {
                self.drive(cx, FieldIntent::Adjust(delta), event.commit, None);
            }
            FormMouseIntent::Text(_, _, column) => {
                self.drive(cx, FieldIntent::None, event.commit, Some(column));
            }
        }
        Action::Continue
    }

    pub(crate) fn draw_body(
        &mut self,
        area: Rect,
        app: &crate::tui::render::RenderState<'_>,
        buf: &mut Buffer,
    ) {
        welcome::draw_welcome(
            area,
            buf,
            &app.view.theme,
            &app.config.bindings,
            &mut self.draft,
            &mut self.form,
            &self.config_path_text,
            &self.data_dir_text,
        );
    }
}

impl AppMode for WelcomeMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        let chrome = self.presentation(cx).chrome.expect("base mode has chrome");
        let mut render = crate::tui::render::RenderState::new(cx);
        crate::tui::render::draw_welcome_screen(
            &mut render,
            self,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        self.process_mouse_cx(cx, mouse)
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation::full_screen(ChromeSpec {
            theme_mode: theme::UiMode::Settings,
            status_label: "Setup",
            layer: bindings::SETTINGS_LAYER,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LobbyListFocus {
    Rooms,
    #[default]
    Users,
}

#[derive(Debug)]
pub(crate) enum BindingResolution {
    Action(BindCommand),
    Consumed,
    Unmatched,
}

#[cfg(test)]
pub(crate) fn resolve_binding(app: &mut App, layer: LayerId, key: KeyEvent) -> BindingResolution {
    let mut cx = app.view_cx();
    resolve_binding_cx(&mut cx, layer, key)
}

pub(crate) fn resolve_binding_cx(
    cx: &mut ViewCx<'_>,
    layer: LayerId,
    key: KeyEvent,
) -> BindingResolution {
    let Some(input) = InputKey::from_event(&key) else {
        return BindingResolution::Unmatched;
    };
    match bindings::resolve(
        &cx.config.bindings.router,
        layer,
        &mut cx.view.chrome.binding.pending_chord,
        input,
    ) {
        Resolved::Action(id) => {
            BindingResolution::Action(cx.config.bindings.actions.get(id).clone())
        }
        Resolved::Consumed => BindingResolution::Consumed,
        Resolved::Unmatched => BindingResolution::Unmatched,
    }
}

pub(crate) fn process_global_command_cx(cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
    use BindCommand::*;
    match command {
        OpenSettings => cx.send(CoreCommand::OpenSettings),
        Quit => return Action::Quit,
        ToggleMute => cx.send(CoreCommand::ToggleMute),
        ToggleDeafen => cx.send(CoreCommand::ToggleDeafen),
        PlaySoundboard1 => cx.send(CoreCommand::PlaySoundboard(0)),
        PlaySoundboard2 => cx.send(CoreCommand::PlaySoundboard(1)),
        PlaySoundboard3 => cx.send(CoreCommand::PlaySoundboard(2)),
        PlaySoundboard4 => cx.send(CoreCommand::PlaySoundboard(3)),
        PlaySoundboard5 => cx.send(CoreCommand::PlaySoundboard(4)),
        PlaySoundboard6 => cx.send(CoreCommand::PlaySoundboard(5)),
        PlaySoundboard7 => cx.send(CoreCommand::PlaySoundboard(6)),
        PlaySoundboard8 => cx.send(CoreCommand::PlaySoundboard(7)),
        PlaySoundboard9 => cx.send(CoreCommand::PlaySoundboard(8)),
        ToggleKeyPreview => {
            cx.view.chrome.key_preview.expanded = !cx.view.chrome.key_preview.expanded;
        }
        _ => {}
    }
    Action::Continue
}

fn process_top_bar_mouse_cx(cx: &mut ViewCx<'_>, mouse: MouseEvent) -> bool {
    use extui::event::{MouseButton, MouseEventKind};

    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    let requested =
        if crate::tui::form::rect_contains(cx.view.chrome.top_bar.live, mouse.column, mouse.row) {
            Some(crate::app::LocalVoiceMode::Live)
        } else if crate::tui::form::rect_contains(
            cx.view.chrome.top_bar.mute,
            mouse.column,
            mouse.row,
        ) {
            Some(crate::app::LocalVoiceMode::Muted)
        } else if crate::tui::form::rect_contains(
            cx.view.chrome.top_bar.deafen,
            mouse.column,
            mouse.row,
        ) {
            Some(crate::app::LocalVoiceMode::Deafened)
        } else {
            None
        };
    if let Some(requested) = requested {
        let mode = if cx.view.local_voice_mode() == requested {
            crate::app::LocalVoiceMode::Live
        } else {
            requested
        };
        cx.send(CoreCommand::SetVoiceMode(mode));
        return true;
    }
    if crate::tui::form::rect_contains(cx.view.chrome.top_bar.video, mouse.column, mouse.row) {
        cx.send(CoreCommand::ToggleVideo);
        return true;
    }
    false
}

fn maybe_auto_close_markdown_code_fence(editor: &mut Editor, key: KeyEvent) {
    if key.code != KeyCode::Char('`') || !key.modifiers.is_empty() {
        return;
    }

    let cursor = editor.cursor_offset();
    let text = editor.text();
    let cursor = cursor as usize;
    let Some(before_cursor) = text.get(..cursor) else {
        return;
    };
    if !before_cursor.ends_with("```") || before_cursor.ends_with("````") {
        return;
    }
    let Some(after_cursor) = text.get(cursor..) else {
        return;
    };
    if !(after_cursor.is_empty() || after_cursor.starts_with('\n')) {
        return;
    }
    if after_cursor.starts_with("\n```") {
        return;
    }

    editor.replace_range(EditorSpan::empty_at(cursor as u32), "\n```");
    editor.set_cursor_offset(cursor as u32);
}

/// Resolves a tab-cycling chord on the settings layer, checked before the
/// form editor sees the key so switching works while a text field is focused.
/// Requiring a modifier keeps plain typed characters (like a `]` in an IPv6
/// bind address) out of the intercept.
fn settings_tab_chord_cx(cx: &ViewCx<'_>, key: &KeyEvent) -> Option<isize> {
    if key.modifiers.is_empty() {
        return None;
    }
    let input = InputKey::from_event(key)?;
    let pending = None;
    for reachable in bindings::reachable(&cx.config.bindings, bindings::SETTINGS_LAYER, &pending) {
        if reachable.key != input {
            continue;
        }
        match reachable.kind {
            bindings::ReachableKind::Action(BindCommand::NextSettingsTab) => return Some(1),
            bindings::ReachableKind::Action(BindCommand::PrevSettingsTab) => return Some(-1),
            _ => {}
        }
    }
    None
}

fn is_control_save_chord_cx(cx: &ViewCx<'_>, key: &KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    let Some(input) = InputKey::from_event(key) else {
        return false;
    };
    let pending = None;
    bindings::reachable(&cx.config.bindings, bindings::SETTINGS_LAYER, &pending)
        .into_iter()
        .any(|reachable| {
            reachable.key == input
                && matches!(
                    reachable.kind,
                    bindings::ReachableKind::Action(BindCommand::SaveSettings)
                )
        })
}

pub(crate) struct ServerListMode {
    select: FuzzySelect,
    searching: bool,
}

impl ServerListMode {
    pub(crate) fn new() -> Self {
        Self {
            select: FuzzySelect::default(),
            searching: false,
        }
    }

    /// Builds the picker with `query` pre-applied and search mode active, so a
    /// `chatt join` specifier that could mean several servers opens filtered and
    /// editable.
    pub(crate) fn with_query(query: String) -> Self {
        let mut select = FuzzySelect::default();
        select.set_query(&query);
        Self {
            select,
            searching: true,
        }
    }

    fn selected_label(&self, cx: &ViewCx<'_>) -> Option<String> {
        self.select
            .current_item_index()
            .and_then(|index| cx.view.server_catalog.items().get(index))
            .map(|item| item.label.clone())
    }

    pub(crate) fn process_action_cx(
        &mut self,
        cx: &mut ViewCx<'_>,
        command: BindCommand,
    ) -> Action {
        use BindCommand::*;
        match command {
            Activate => {
                let Some(label) = self.selected_label(cx) else {
                    cx.set_error("no server selected");
                    return Action::Continue;
                };
                cx.send(CoreCommand::Connect { alias: label });
            }
            SelectNext => {
                self.select.move_selection(1);
            }
            SelectPrev => {
                self.select.move_selection(-1);
            }
            EditServer => {
                let Some(label) = self.selected_label(cx) else {
                    cx.set_error("no server selected");
                    return Action::Continue;
                };
                let server = match cx.config.server(&label) {
                    Ok(server) => server,
                    Err(error) => {
                        cx.set_error(error);
                        return Action::Continue;
                    }
                };
                let draft = ServerEditDraft::from_server(server, cx.config);
                cx.request_transition(ModeTransition::Push(Box::new(ServerEditMode::new(draft))));
                cx.set_status(format!("editing server {label}"));
            }
            DeleteServer => {
                let Some(label) = self.selected_label(cx) else {
                    cx.set_error("no server selected");
                    return Action::Continue;
                };
                let prompt = format!("Delete server '{label}'?");
                cx.request_transition(ModeTransition::Push(Box::new(ConfirmMode::new(
                    prompt,
                    "Delete",
                    "Cancel",
                    move |cx| {
                        cx.send(CoreCommand::DeleteServer { label });
                        ConfirmDisposition::Transition(ModeTransition::Pop)
                    },
                ))));
            }
            SearchServers => {
                self.searching = true;
                self.select.clear_query();
                self.select.refresh(cx.view.server_catalog.items());
            }
            Cancel if cx.session.active_server_label.is_some() => {
                cx.request_transition(ModeTransition::Push(Box::new(RoomMode::default())));
            }
            _ => return process_global_command_cx(cx, command),
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

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if self.searching {
            match key.code {
                extui::event::KeyCode::Esc | extui::event::KeyCode::Enter => {
                    self.searching = false;
                }
                _ if self.select.edit_query(key) => {
                    self.select.refresh(cx.view.server_catalog.items())
                }
                _ => {}
            }
            return Action::Continue;
        }
        match resolve_binding_cx(cx, bindings::PICKER_LAYER, key) {
            BindingResolution::Action(command) => self.process_action_cx(cx, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }
}

impl Default for ServerListMode {
    fn default() -> Self {
        Self::new()
    }
}

impl AppMode for ServerListMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        self.select.refresh(cx.view.server_catalog.items());
        let chrome = self.presentation(cx).chrome.expect("base mode has chrome");
        let mut render = crate::tui::render::RenderState::new(cx);
        crate::tui::render::draw_server_select_screen(
            &mut render,
            &mut self.select,
            self.searching,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation::full_screen(ChromeSpec {
            theme_mode: theme::UiMode::ServerSelect,
            status_label: "Servers",
            layer: bindings::PICKER_LAYER,
        })
    }
}

/// Fuzzy room picker pushed over [`RoomMode`] by `/rooms` or the switcher key.
pub(crate) struct RoomSwitchMode {
    select: FuzzySelect,
    searching: bool,
    /// Rows rebuilt every render so unread and voice markers stay live; actions
    /// index into the same list the last `refresh` saw.
    items: Vec<crate::app::room::RoomSelectItem>,
}

impl RoomSwitchMode {
    pub(crate) fn new() -> Self {
        Self {
            select: FuzzySelect::default(),
            searching: false,
            items: Vec::new(),
        }
    }

    fn selected_room(&self) -> Option<rpc::ids::RoomId> {
        self.select
            .current_item_index()
            .and_then(|index| self.items.get(index))
            .map(|item| item.room_id)
    }

    fn refresh_cx(&mut self, cx: &ViewCx<'_>) {
        self.items = cx.session.room_select_items(cx.session.voice_room);
        for item in &mut self.items {
            item.viewed = cx.view.viewed_room == Some(item.room_id);
        }
        self.select.refresh(&self.items);
    }

    #[cfg(test)]
    fn refresh(&mut self, app: &App) {
        self.items = app.room.room_select_items(app.room.voice_room);
        self.select.refresh(&self.items);
    }

    pub(crate) fn process_action_cx(
        &mut self,
        cx: &mut ViewCx<'_>,
        command: BindCommand,
    ) -> Action {
        use BindCommand::*;
        match command {
            Activate => {
                let Some(room_id) = self.selected_room() else {
                    cx.set_error("no room selected");
                    return Action::Continue;
                };
                cx.send(CoreCommand::SetViewedRoom(room_id));
                cx.request_transition(ModeTransition::Pop);
            }
            SelectNext => {
                self.select.move_selection(1);
            }
            SelectPrev => {
                self.select.move_selection(-1);
            }
            SearchServers => {
                self.searching = true;
                self.select.clear_query();
                self.refresh_cx(cx);
            }
            Cancel | RoomSwitcher => cx.request_transition(ModeTransition::Pop),
            _ => return process_global_command_cx(cx, command),
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

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if self.searching {
            match key.code {
                extui::event::KeyCode::Esc | extui::event::KeyCode::Enter => {
                    self.searching = false;
                }
                _ if self.select.edit_query(key) => self.refresh_cx(cx),
                _ => {}
            }
            return Action::Continue;
        }
        match resolve_binding_cx(cx, bindings::PICKER_LAYER, key) {
            BindingResolution::Action(command) => self.process_action_cx(cx, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }
}

impl AppMode for RoomSwitchMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        self.refresh_cx(cx);
        let chrome = self.presentation(cx).chrome.expect("base mode has chrome");
        let mut render = crate::tui::render::RenderState::new(cx);
        crate::tui::render::draw_room_select_screen(
            &mut render,
            &mut self.select,
            &self.items,
            self.searching,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation::full_screen(ChromeSpec {
            theme_mode: theme::UiMode::ServerSelect,
            status_label: "Rooms",
            layer: bindings::PICKER_LAYER,
        })
    }
}

pub(crate) struct ServerEditMode {
    draft: Option<ServerEditDraft>,
}

impl ServerEditMode {
    pub(crate) fn new(draft: ServerEditDraft) -> Self {
        Self { draft: Some(draft) }
    }

    fn handle_event(&mut self, cx: &mut ViewCx<'_>, event: ServerEditEvent) {
        match event {
            ServerEditEvent::Consumed => {}
            ServerEditEvent::Cancel => cx.request_transition(ModeTransition::Pop),
            ServerEditEvent::Save { join_after_save } => {
                if let Some(draft) = self.draft.take() {
                    cx.send(CoreCommand::SaveServerEdit {
                        draft,
                        join_after_save,
                    });
                }
            }
        }
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if let Some(draft) = self.draft.as_mut() {
            let event = draft.handle_key(key, &cx.view.theme);
            self.handle_event(cx, event);
        }
        Action::Continue
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if let Some(draft) = self.draft.as_mut() {
            let event = draft.handle_mouse(mouse, &cx.view.theme);
            self.handle_event(cx, event);
        }
        Action::Continue
    }
}

impl AppMode for ServerEditMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        if let Some(draft) = self.draft.as_mut() {
            let mut render = crate::tui::render::RenderState::new(cx);
            crate::tui::render::draw_server_edit_overlay(&mut render, draft, buf);
        }
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
                theme_mode: theme::UiMode::ServerEdit,
                status_label: "Server",
                layer: bindings::FORM_LAYER,
            }),
        }
    }
}

pub(crate) struct SettingsSession {
    pub(crate) form: FormState<FieldId>,
    pub(crate) draft: SettingsDraft,
    pub(crate) input_items: Vec<settings::AudioInputItem>,
    pub(crate) output_items: Vec<settings::AudioOutputItem>,
    pub(crate) input_picker: AudioInputPickerState,
    pub(crate) output_picker: AudioOutputPickerState,
    pub(crate) dirty: bool,
    /// The active tab. Both the draw and logic passes gate their field
    /// declarations on this, so it lives in the shared session.
    pub(crate) tab: crate::ui::settings::SettingsTab,
    /// Tab-bar segment hit boxes written by the draw pass for mouse routing.
    pub(crate) tab_rects: [Rect; 4],
    catalog_generation: u64,
}

impl SettingsSession {
    pub(crate) fn new(
        config: &crate::config::Config,
        devices: &crate::app::AudioDeviceCatalog,
    ) -> Self {
        let mut draft = SettingsDraft::from_audio(&config.audio);
        draft.set_form_bindings_from_config(config.ui.default_bindings);
        draft.set_theme_from_config(
            config.ui.theme.clone(),
            config.ui.themes.resolved.keys().cloned().collect(),
        );
        draft.set_web_from_config(&config.web);
        draft.set_notifications_from_config(&config.notifications);
        draft.set_files_from_config(&config.files);
        draft.set_p2p_from_config(&config.p2p);
        draft.set_history_from_config(&config.history);
        draft.set_ui_from_config(&config.ui);
        draft.set_url_open_from_config(&config.url_open);
        let input_items = settings::audio_input_items(devices.input_devices());
        let output_items = settings::audio_output_items(devices.output_devices());
        let mut input_picker = AudioInputPickerState::default();
        input_picker.reset(&input_items, draft.input_selection());
        let mut output_picker = AudioOutputPickerState::default();
        output_picker.reset(&output_items, draft.output_selection());
        Self {
            form: FormState::new(
                crate::ui::settings::initial_focus(),
                config.ui.default_bindings,
            ),
            draft,
            input_items,
            output_items,
            input_picker,
            output_picker,
            dirty: false,
            tab: crate::ui::settings::SettingsTab::Audio,
            tab_rects: [Rect::EMPTY; 4],
            catalog_generation: devices.generation(),
        }
    }

    pub(crate) fn sync_catalog(&mut self, devices: &crate::app::AudioDeviceCatalog) {
        if self.catalog_generation == devices.generation() {
            return;
        }
        self.catalog_generation = devices.generation();
        self.input_items = settings::audio_input_items(devices.input_devices());
        if self.input_picker.open {
            self.input_picker
                .refresh_items(&self.input_items, self.draft.input_selection());
        } else {
            self.input_picker
                .reset(&self.input_items, self.draft.input_selection());
        }
        self.output_items = settings::audio_output_items(devices.output_devices());
        if self.output_picker.open {
            self.output_picker
                .refresh_items(&self.output_items, self.draft.output_selection());
        } else {
            self.output_picker
                .reset(&self.output_items, self.draft.output_selection());
        }
    }
}

pub(crate) struct SettingsMode;

impl SettingsMode {
    pub(crate) fn new() -> Self {
        Self
    }

    #[cfg(test)]
    pub(crate) fn with_form_for_test(form: FormState<FieldId>, app: &mut App) -> Self {
        let mut session = SettingsSession::new(&app.config, &app.room.audio_devices);
        session.form = form;
        app.room.settings = Some(std::sync::Arc::new(std::sync::Mutex::new(session)));
        app.room
            .set_settings_owner_for_test(crate::client_channel::ClientId::PRIMARY);
        Self
    }

    fn process_action(&mut self, cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            SaveSettings => cx.send(CoreCommand::Settings(SettingsOp::Save)),
            Activate => cx.send(CoreCommand::Settings(SettingsOp::Drive {
                intent: FieldIntent::Activate,
                commit: None,
                focus_column: None,
            })),
            FocusNext => cx.send(CoreCommand::Settings(SettingsOp::MoveFocus(1))),
            FocusPrev => cx.send(CoreCommand::Settings(SettingsOp::MoveFocus(-1))),
            SelectNext => cx.send(CoreCommand::Settings(SettingsOp::MoveSelection(1))),
            SelectPrev => cx.send(CoreCommand::Settings(SettingsOp::MoveSelection(-1))),
            AdjustLeft => cx.send(CoreCommand::Settings(SettingsOp::Drive {
                intent: FieldIntent::Adjust(-1),
                commit: None,
                focus_column: None,
            })),
            AdjustRight => cx.send(CoreCommand::Settings(SettingsOp::Drive {
                intent: FieldIntent::Adjust(1),
                commit: None,
                focus_column: None,
            })),
            Cancel | CloseSettings => {
                cx.send(CoreCommand::Settings(SettingsOp::CancelOrClose));
            }
            RefreshDevices => cx.send(CoreCommand::Settings(SettingsOp::RefreshDevices)),
            NextSettingsTab => cx.send(CoreCommand::Settings(SettingsOp::CycleTab(1))),
            PrevSettingsTab => cx.send(CoreCommand::Settings(SettingsOp::CycleTab(-1))),
            _ => return process_global_command_cx(cx, command),
        }
        Action::Continue
    }

    fn resolve_binding(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        match resolve_binding_cx(cx, bindings::SETTINGS_LAYER, key) {
            BindingResolution::Action(command) => self.process_action(cx, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        let Some(settings) = cx.session.settings.clone() else {
            cx.set_error("settings session is no longer active");
            return Action::Continue;
        };
        let mut session = settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        session.sync_catalog(&cx.session.audio_devices);
        if session.input_picker.open || session.output_picker.open {
            drop(session);
            cx.send(CoreCommand::Settings(SettingsOp::PickerKey(key)));
            return Action::Continue;
        }
        if is_control_save_chord_cx(cx, &key) {
            drop(session);
            cx.send(CoreCommand::Settings(SettingsOp::Save));
            return Action::Continue;
        }
        let tab_delta = match key.code {
            KeyCode::Tab => Some(1),
            KeyCode::BackTab => Some(-1),
            _ => None,
        };
        if let Some(delta) = tab_delta {
            drop(session);
            cx.send(CoreCommand::Settings(SettingsOp::CycleTab(delta)));
            return Action::Continue;
        }
        if let Some(delta) = settings_tab_chord_cx(cx, &key) {
            drop(session);
            cx.send(CoreCommand::Settings(SettingsOp::CycleTab(delta)));
            return Action::Continue;
        }

        let kind = session.form.focused_kind();
        let text_focused = matches!(kind, FormFieldKind::Text | FormFieldKind::AdjustableText);
        let event = session.form.handle_key(key, kind);
        let live_list_edit = event.commit.as_ref().is_some_and(|(field, _)| {
            crate::ui::settings::is_list_add_field(&session.draft, *field)
        });
        drop(session);
        match event.action {
            FormAction::None if !text_focused => return self.resolve_binding(cx, key),
            FormAction::None => cx.send(CoreCommand::Settings(SettingsOp::Drive {
                intent: FieldIntent::None,
                commit: event.commit,
                focus_column: None,
            })),
            FormAction::Cancel => {
                cx.send(CoreCommand::Settings(SettingsOp::CancelOrClose));
            }
            FormAction::ActivateNextInsert => {
                // Moving focus clears and commits the active editor itself.
                // Keep this as one core operation so a render cannot expose an
                // intermediate frame with the completed list row still active.
                cx.send(CoreCommand::Settings(SettingsOp::MoveFocusInsert(1)));
            }
            FormAction::MoveFocus(delta) => {
                cx.send(CoreCommand::Settings(SettingsOp::MoveFocus(delta)));
            }
            FormAction::Activate if text_focused => {
                // MoveFocus commits the active editor and registers the next
                // field in one core update, avoiding an intermediate redraw.
                cx.send(CoreCommand::Settings(SettingsOp::MoveFocus(1)));
            }
            FormAction::Activate => cx.send(CoreCommand::Settings(SettingsOp::Drive {
                intent: FieldIntent::Activate,
                commit: event.commit,
                focus_column: None,
            })),
            FormAction::Adjust(delta) => cx.send(CoreCommand::Settings(SettingsOp::Drive {
                intent: FieldIntent::Adjust(delta),
                commit: event.commit,
                focus_column: None,
            })),
            FormAction::FocusMoved | FormAction::Scrolled => {
                cx.send(CoreCommand::Settings(SettingsOp::Drive {
                    intent: FieldIntent::None,
                    commit: event.commit,
                    focus_column: None,
                }));
            }
            FormAction::TextChanged => {
                if live_list_edit {
                    cx.send(CoreCommand::Settings(SettingsOp::Drive {
                        intent: FieldIntent::None,
                        commit: event.commit,
                        focus_column: None,
                    }));
                } else {
                    cx.send(CoreCommand::Settings(SettingsOp::MarkDirty));
                }
            }
        }
        Action::Continue
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if process_top_bar_mouse_cx(cx, mouse) {
            return Action::Continue;
        }
        let Some(settings) = cx.session.settings.clone() else {
            cx.set_error("settings session is no longer active");
            return Action::Continue;
        };
        let mut session = settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        session.sync_catalog(&cx.session.audio_devices);
        if matches!(
            mouse.kind,
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left)
        ) && let Some(tab) = session
            .tab_rects
            .iter()
            .zip(crate::ui::settings::SettingsTab::ALL)
            .find(|(rect, _)| crate::tui::form::rect_contains(**rect, mouse.column, mouse.row))
            .map(|(_, tab)| tab)
        {
            drop(session);
            cx.send(CoreCommand::Settings(SettingsOp::SetTab(tab)));
            return Action::Continue;
        }
        if (session.input_picker.open || session.output_picker.open)
            && matches!(
                mouse.kind,
                extui::event::MouseEventKind::ScrollDown | extui::event::MouseEventKind::ScrollUp
            )
        {
            drop(session);
            cx.send(CoreCommand::Settings(SettingsOp::PickerMouse(mouse)));
            return Action::Continue;
        }

        let event = session.form.handle_mouse(mouse);
        drop(session);
        match event.intent {
            FormMouseIntent::None => cx.send(CoreCommand::Settings(SettingsOp::Drive {
                intent: FieldIntent::None,
                commit: event.commit,
                focus_column: None,
            })),
            FormMouseIntent::Activate(_) => {
                cx.send(CoreCommand::Settings(SettingsOp::Drive {
                    intent: FieldIntent::Activate,
                    commit: event.commit,
                    focus_column: None,
                }));
            }
            FormMouseIntent::Adjust(_, delta) => {
                cx.send(CoreCommand::Settings(SettingsOp::Drive {
                    intent: FieldIntent::Adjust(delta),
                    commit: event.commit,
                    focus_column: None,
                }));
            }
            FormMouseIntent::Text(_, _, column) => {
                cx.send(CoreCommand::Settings(SettingsOp::Drive {
                    intent: FieldIntent::None,
                    commit: event.commit,
                    focus_column: Some(column),
                }));
            }
            FormMouseIntent::PickerItem(field, item_index) => {
                cx.send(CoreCommand::Settings(SettingsOp::ActivatePickerItem {
                    field,
                    item_index,
                }));
            }
        }
        Action::Continue
    }
}

impl AppMode for SettingsMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        let Some(settings) = cx.session.settings.clone() else {
            return;
        };
        settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .sync_catalog(&cx.session.audio_devices);
        let chrome = self.presentation(cx).chrome.expect("base mode has chrome");
        let mut render = crate::tui::render::RenderState::new(cx);
        crate::tui::render::draw_settings_screen(
            &mut render,
            self,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        self.process_mouse_cx(cx, mouse)
    }

    fn on_exit(&mut self, cx: &mut ViewCx<'_>, _reason: ExitReason) {
        cx.send(CoreCommand::Settings(SettingsOp::Finish));
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation::full_screen(ChromeSpec {
            theme_mode: theme::UiMode::Settings,
            status_label: "Settings",
            layer: bindings::SETTINGS_LAYER,
        })
    }
}

/// The chat state a frame was painted from. When the next frame's anchor
/// matches in everything but `top`, the visible rows merely shifted and the
/// renderer scrolls the chat rect instead of rewriting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChatScrollAnchor {
    pub room: Option<rpc::ids::RoomId>,
    pub width: u16,
    pub rect: Rect,
    pub top: usize,
    pub epoch: u64,
}

#[derive(Debug)]
pub(crate) struct RoomLayout {
    pub chat_width: u16,
    pub chat_height: u16,
    pub chat_rect: Rect,
    pub chat_scroll_anchor: Option<ChatScrollAnchor>,
    pub visible_chat_lines: Vec<VisibleLine>,
    pub top_bar_rect: Rect,
    pub key_preview_rect: Rect,
    pub workspace_rect: Rect,
    /// Lobby rows carved from the workspace top; part of the geometry
    /// backstop because it moves the lobby/chat split without moving the
    /// workspace rect itself.
    pub workspace_room_height: u16,
    pub room_list_rect: Rect,
    pub lobby_divider_rect: Rect,
    pub rooms_scrollbar: Option<ScrollbarLayout>,
    pub user_list_rect: Rect,
    /// Hit boxes of the room rows drawn in the room list.
    pub room_hits: Vec<(Rect, rpc::ids::RoomId)>,
    pub lobby_bar_rect: Rect,
    pub chat_log_bar_rect: Rect,
    pub composer_rect: Rect,
    /// Composer editor plus its optional half-block frame and side gutters.
    pub composer_frame_rect: Rect,
    pub composer_scrollbar: Option<ScrollbarLayout>,
    pub compose_bar_rect: Rect,
}

impl Default for RoomLayout {
    fn default() -> Self {
        Self {
            chat_width: 80,
            chat_height: 0,
            chat_rect: Rect::EMPTY,
            chat_scroll_anchor: None,
            visible_chat_lines: Vec::new(),
            top_bar_rect: Rect::EMPTY,
            key_preview_rect: Rect::EMPTY,
            workspace_rect: Rect::EMPTY,
            workspace_room_height: 0,
            room_list_rect: Rect::EMPTY,
            lobby_divider_rect: Rect::EMPTY,
            rooms_scrollbar: None,
            user_list_rect: Rect::EMPTY,
            room_hits: Vec::new(),
            lobby_bar_rect: Rect::EMPTY,
            chat_log_bar_rect: Rect::EMPTY,
            composer_rect: Rect::EMPTY,
            composer_frame_rect: Rect::EMPTY,
            composer_scrollbar: None,
            compose_bar_rect: Rect::EMPTY,
        }
    }
}

impl RoomLayout {
    fn room_hit(&self, column: u16, row: u16) -> Option<rpc::ids::RoomId> {
        self.room_hits
            .iter()
            .find(|(rect, _)| crate::tui::form::rect_contains(*rect, column, row))
            .map(|(_, room_id)| *room_id)
    }

    pub(crate) fn clear_chat(&mut self) {
        self.chat_height = 0;
        self.chat_rect = Rect::EMPTY;
        self.chat_scroll_anchor = None;
        self.visible_chat_lines.clear();
    }

    fn chat_line_at(&self, row: u16) -> Option<VisibleLine> {
        let index = row.checked_sub(self.chat_rect.y)? as usize;
        self.visible_chat_lines.get(index).copied()
    }
}

/// Sections a chat-log scroll or cursor move can touch: the log itself, the
/// key preview a completed chord clears, and the compose bar where handlers
/// surface status text. Handlers narrow to this only when chat focus did not
/// change, since a focus change restyles the section bars.
const CHAT_SCROLL_DIRTY: DirtySections = DirtySections::CHAT
    .union(DirtySections::KEY_PREVIEW)
    .union(DirtySections::COMPOSE_BAR);

/// Moves the composer cursor to the text boundary nearest a terminal click.
/// Frame and gutter coordinates clamp onto the closest editor edge.
fn move_composer_cursor_to_click(editor: &mut Editor, area: Rect, column: u16, row: u16) {
    if area.is_empty() {
        return;
    }
    let click_x = column.clamp(area.x, area.x.saturating_add(area.w).saturating_sub(1));
    let click_y = row.clamp(area.y, area.y.saturating_add(area.h).saturating_sub(1));
    let Some((_, cursor_y)) = editor.cursor_position(area) else {
        return;
    };
    let width = area.w.max(1);
    let tabstop = editor.tab_settings().tabstop.max(1);
    let text = editor.text();
    let cursor_visual_row =
        composer_visual_position(&text, editor.cursor_offset() as usize, width, tabstop).0;
    let visible_start = cursor_visual_row.saturating_sub(cursor_y.saturating_sub(area.y) as usize);
    let target_row = visible_start + click_y.saturating_sub(area.y) as usize;
    let target_col = click_x.saturating_sub(area.x);

    let mut best = (usize::MAX, 0usize);
    for offset in text
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
    {
        let (visual_row, visual_col) = composer_visual_position(&text, offset, width, tabstop);
        let distance = visual_row.abs_diff(target_row) * (width as usize + 1)
            + usize::from(visual_col.abs_diff(target_col));
        if distance < best.0 {
            best = (distance, offset);
        }
    }
    editor.set_cursor_offset(best.1 as u32);
}

/// Moves the cursor just far enough for extui-editor to establish `target` as
/// the viewport's first visible visual row on the next render.
fn scroll_composer_to_offset(
    editor: &mut Editor,
    area: Rect,
    current: u32,
    target: u32,
    viewport: u32,
) {
    if area.is_empty() || current == target {
        return;
    }
    let cursor_row = if target < current {
        target
    } else {
        target.saturating_add(viewport.saturating_sub(1))
    } as usize;
    let text = editor.text();
    let offset = composer_offset_at_visual_row(
        &text,
        cursor_row,
        area.w.max(1),
        editor.tab_settings().tabstop.max(1),
    );
    editor.set_cursor_offset(offset as u32);
}

/// An in-progress divider drag started on one of the inner status bars. The
/// anchor is the row the drag began on and the starting size lets each `Drag`
/// apply `start + delta` from that anchor, robust against the layout rects
/// shifting between frames as the split moves.
#[derive(Debug, Clone, Copy)]
enum DividerDrag {
    /// Dragging the Lobby/Rooms bar resizes the rooms/users list block above it.
    LobbyBar {
        anchor_row: u16,
        start_room_height: u16,
    },
    /// Dragging the Chat Log bar resizes the compose window below it.
    ChatLogBar { anchor_row: u16, start_rows: u16 },
}

#[derive(Debug)]
pub(crate) struct RoomMode {
    focus: ChatPanelFocus,
    lobby_list_focus: LobbyListFocus,
    layout: RoomLayout,
    divider_drag: Option<DividerDrag>,
    scrollbar_drag: Option<ScrollbarDrag>,
    history_search: Option<HistorySearch>,
    last_history_search: Option<HistorySearch>,
}

impl Default for RoomMode {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomMode {
    pub(crate) fn new() -> Self {
        Self {
            focus: ChatPanelFocus::Compose,
            lobby_list_focus: LobbyListFocus::Users,
            layout: RoomLayout::default(),
            divider_drag: None,
            scrollbar_drag: None,
            history_search: None,
            last_history_search: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_focus(focus: ChatPanelFocus) -> Self {
        Self {
            focus,
            lobby_list_focus: LobbyListFocus::Users,
            layout: RoomLayout::default(),
            divider_drag: None,
            scrollbar_drag: None,
            history_search: None,
            last_history_search: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn focus(&self) -> ChatPanelFocus {
        self.focus
    }

    #[cfg(test)]
    pub(crate) fn lobby_list_focus(&self) -> LobbyListFocus {
        self.lobby_list_focus
    }

    #[cfg(test)]
    pub(crate) fn layout(&self) -> &RoomLayout {
        &self.layout
    }

    fn set_focus_cx(&mut self, cx: &mut ViewCx<'_>, focus: ChatPanelFocus) {
        if self.focus == ChatPanelFocus::Compose && focus != ChatPanelFocus::Compose {
            cx.view.cancel_pending_edit();
        }
        self.focus = focus;
        match focus {
            ChatPanelFocus::Lobby if self.lobby_list_focus == LobbyListFocus::Users => {
                self.keep_selected_room_user_visible(cx);
            }
            ChatPanelFocus::Lobby => {}
            ChatPanelFocus::ChatLog => {
                cx.view.active.chat.ensure_cursor(self.layout.chat_width);
            }
            ChatPanelFocus::Compose => {}
        }
    }

    #[cfg(test)]
    pub(crate) fn set_focus(&mut self, app: &mut App, focus: ChatPanelFocus) {
        let mut cx = app.view_cx();
        self.set_focus_cx(&mut cx, focus);
        app.drain_core_commands();
    }

    fn enter_compose_insert_mode(&mut self, cx: &mut ViewCx<'_>) {
        cx.view.composer.enter_insert_mode();
        self.set_focus_cx(cx, ChatPanelFocus::Compose);
    }

    fn move_focus(&mut self, cx: &mut ViewCx<'_>, delta: isize) {
        self.set_focus_cx(cx, self.focus.moved(delta));
    }

    /// The binding layer for non-compose focus: the chat-visual overlay while
    /// the chat log holds a visual-line selection, the workspace otherwise.
    fn workspace_layer_cx(&self, cx: &ViewCx<'_>) -> LayerId {
        if self.focus == ChatPanelFocus::ChatLog && cx.view.active.chat.has_visual() {
            bindings::CHAT_VISUAL_LAYER
        } else {
            bindings::WORKSPACE_LAYER
        }
    }

    #[cfg(test)]
    fn workspace_layer(&self, app: &App) -> LayerId {
        if self.focus == ChatPanelFocus::ChatLog && app.view.active.chat.has_visual() {
            bindings::CHAT_VISUAL_LAYER
        } else {
            bindings::WORKSPACE_LAYER
        }
    }

    fn set_lobby_list_focus(&mut self, cx: &mut ViewCx<'_>, focus: LobbyListFocus) {
        self.lobby_list_focus = focus;
        self.set_focus_cx(cx, ChatPanelFocus::Lobby);
    }

    fn submit_input(&mut self, cx: &mut ViewCx<'_>) {
        let Some(submission) = cx.view.submit_composer() else {
            return;
        };
        match submission {
            crate::app::ComposerSubmission::Message(body) => cx.send(CoreCommand::SendChat {
                room_id: cx.view.viewed_room,
                body,
            }),
            crate::app::ComposerSubmission::Edit {
                room_id,
                target,
                body,
            } => cx.send(CoreCommand::SubmitEdit {
                room_id,
                target,
                body,
            }),
            crate::app::ComposerSubmission::Command(input) => {
                if !self.run_ui_slash(cx, &input) {
                    cx.send(CoreCommand::RunSlash {
                        room_id: cx.view.viewed_room,
                        input,
                    });
                }
            }
        }
    }

    fn run_ui_slash(&mut self, cx: &mut ViewCx<'_>, input: &str) -> bool {
        match input {
            "/quit" => cx.set_status("use Ctrl-C to quit"),
            "/stats" => {
                cx.view.lobby_details = !cx.view.lobby_details;
                cx.set_status(if cx.view.lobby_details {
                    "lobby detail on (jitter buffer stats)"
                } else {
                    "lobby detail off (latency estimate)"
                });
            }
            "/clear" => cx.view.clear_chat(),
            "/config" | "/settings" => cx.send(CoreCommand::OpenSettings),
            "/servers" if cx.session.active_server_label.is_some() => {
                cx.request_transition(ModeTransition::Pop);
            }
            "/servers" => {
                cx.request_transition(ModeTransition::Set(Box::new(ServerListMode::new())));
            }
            "/rooms" => self.open_room_switcher(cx),
            "/room-settings" => self.open_room_settings(cx),
            _ => return false,
        }
        true
    }

    fn open_room_switcher(&self, cx: &mut ViewCx<'_>) {
        cx.request_transition(ModeTransition::Push(Box::new(RoomSwitchMode::new())));
    }

    fn open_user_list(&self, cx: &mut ViewCx<'_>) {
        cx.request_transition(ModeTransition::Push(Box::new(UserListMode::new())));
    }

    fn open_room_settings(&self, cx: &mut ViewCx<'_>) {
        let Some(alias) = cx.session.active_server_label.as_ref() else {
            cx.set_error("connect to a server first");
            return;
        };
        let Some(room_id) = cx.view.viewed_room else {
            cx.set_error("view a room first");
            return;
        };
        let server = match cx.config.server(alias) {
            Ok(server) => server,
            Err(error) => {
                cx.set_error(error);
                return;
            }
        };
        let draft = RoomSettingsDraft::from_config(
            cx.config,
            server,
            room_id,
            cx.session
                .room_meta(room_id)
                .map(|room| room.name.clone())
                .unwrap_or_else(|| cx.session.room_name.clone()),
        );
        cx.request_transition(ModeTransition::Push(Box::new(RoomSettingsMode::new(draft))));
    }

    fn open_selected_user_volume(&self, cx: &mut ViewCx<'_>) {
        let participants = cx.session.participant_snapshot(cx.view.viewed_room);
        let Some(selected) = cx.view.selected_participant(&participants.entries) else {
            cx.set_status("no user selected");
            return;
        };
        if Some(selected.user_id) == cx.session.local_user {
            cx.set_status("select another user to adjust volume");
            return;
        }
        let user_id = selected.user_id;
        let display_name = selected.display_name().to_string();
        let value_db = cx.config.user_volume_db(&cx.session.server_alias, user_id);
        cx.send(CoreCommand::BeginVolumePreview { user_id, value_db });
        let dialog = UserVolumeDialog::new(user_id, display_name.clone(), value_db, &cx.view.theme);
        cx.request_transition(ModeTransition::Push(Box::new(DialogMode::new(dialog))));
        cx.set_status(format!("adjusting local volume for {display_name}"));
    }

    fn cycle_room(&self, cx: &mut ViewCx<'_>, delta: isize) {
        let rooms: Vec<_> = cx
            .session
            .room_metas()
            .map(|(room_id, _)| room_id)
            .collect();
        if rooms.is_empty() {
            cx.set_status("no rooms yet");
            return;
        }
        let current = cx
            .view
            .viewed_room
            .and_then(|viewed| rooms.iter().position(|room_id| *room_id == viewed));
        let next = match current {
            Some(current) => (current as isize + delta).rem_euclid(rooms.len() as isize) as usize,
            None if delta < 0 => rooms.len() - 1,
            None => 0,
        };
        cx.view
            .keep_room_index_visible(next, rooms.len(), self.layout.room_list_rect.h as usize);
        cx.send(CoreCommand::SetViewedRoom(rooms[next]));
    }

    fn process_global_command(&mut self, cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
        process_global_command_cx(cx, command)
    }

    fn process_action(&mut self, cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            EnterCompose => self.enter_compose_insert_mode(cx),
            EnterLog => self.set_focus_cx(cx, ChatPanelFocus::ChatLog),
            SubmitMessage => self.submit_input(cx),
            Cancel => {
                if self.focus == ChatPanelFocus::Compose && !cx.view.cancel_pending_edit() {
                    cx.view.composer.clear();
                }
                self.enter_compose_insert_mode(cx);
            }
            ScrollUp => self.scroll_focused_panel(cx, -1),
            ScrollDown => self.scroll_focused_panel(cx, 1),
            RoomScrollUp => self.move_user_selection_with_focus(cx, -1),
            RoomScrollDown => self.move_user_selection_with_focus(cx, 1),
            OpenSelectedUserVolume => {
                if self.focus != ChatPanelFocus::Lobby {
                    cx.set_status("focus lobby to adjust users");
                } else if self.lobby_list_focus == LobbyListFocus::Users {
                    self.open_selected_user_volume(cx);
                } else {
                    cx.set_status("focus users list to adjust volume");
                }
            }
            ToggleSelectedUserMute => {
                if self.focus != ChatPanelFocus::Lobby {
                    cx.set_status("focus lobby to mute users");
                } else if self.lobby_list_focus == LobbyListFocus::Users {
                    let participants = cx.session.participant_snapshot(cx.view.viewed_room);
                    let Some(selected) = cx.view.selected_participant(&participants.entries) else {
                        cx.set_status("no user selected");
                        return Action::Continue;
                    };
                    cx.send(CoreCommand::ToggleUserMute(selected.user_id));
                } else {
                    cx.set_status("focus users list to mute users");
                }
            }
            HalfPageUp => {
                self.scroll_chat_log_if_focused(cx, -(self.chat_half_page_rows() as isize));
            }
            HalfPageDown => {
                self.scroll_chat_log_if_focused(cx, self.chat_half_page_rows() as isize);
            }
            Top => self.select_chat_top(cx),
            Bottom => self.select_chat_bottom(cx),
            ParagraphBack => self.move_chat_cursor_paragraph(cx, -1),
            ParagraphForward => self.move_chat_cursor_paragraph(cx, 1),
            ToggleVisual => self.toggle_chat_visual_if_focused(cx),
            ClearSelection => {
                if self.focus == ChatPanelFocus::ChatLog {
                    cx.view.active.chat.clear_visual_anchor();
                }
            }
            CopySelection => self.copy_chat_selection_if_focused(cx),
            CopyLine => self.copy_cursor_line_if_focused(cx),
            CopyMessage => self.copy_cursor_message_if_focused(cx),
            CopyMessageRef => self.copy_message_ref_if_focused(cx),
            InsertMessageRef => self.insert_message_ref_if_focused(cx),
            OpenMessageRef => self.open_message_ref_if_focused(cx),
            EditMessage => self.edit_cursor_message_if_focused(cx),
            DeleteMessage => self.delete_selected_messages_if_focused(cx),
            ToggleExpand => self.toggle_chat_expand_if_focused(cx),
            FocusNext => self.move_focus(cx, 1),
            FocusPrev => self.move_focus(cx, -1),
            SelectNext if self.focus == ChatPanelFocus::Lobby => self.scroll_focused_panel(cx, 1),
            SelectPrev if self.focus == ChatPanelFocus::Lobby => self.scroll_focused_panel(cx, -1),
            AdjustLeft if self.focus == ChatPanelFocus::Lobby => {
                self.set_lobby_list_focus(cx, LobbyListFocus::Rooms);
            }
            AdjustRight if self.focus == ChatPanelFocus::Lobby => {
                self.set_lobby_list_focus(cx, LobbyListFocus::Users);
            }
            ClearChat if self.focus == ChatPanelFocus::ChatLog => cx.view.clear_chat(),
            PasteClipboard => {
                self.paste_from_clipboard(cx, &crate::clipboard_paste::HelperClipboard)
            }
            RoomSwitcher => self.open_room_switcher(cx),
            OpenUserList => self.open_user_list(cx),
            OpenRoomSettings => self.open_room_settings(cx),
            NextRoom => self.cycle_room(cx, 1),
            PrevRoom => self.cycle_room(cx, -1),
            _ => return self.process_global_command(cx, command),
        }
        Action::Continue
    }

    fn process_compose_key(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        // Keys the editor consumes touch only the composer band and the
        // chrome that tracks the editor mode and pending chords; bound
        // commands can reach anything, so they stay on the coarse path.
        const EDITOR_DIRTY: DirtySections = DirtySections::COMPOSER
            .union(DirtySections::COMPOSE_BAR)
            .union(DirtySections::KEY_PREVIEW);
        const CHORD_DIRTY: DirtySections =
            DirtySections::COMPOSE_BAR.union(DirtySections::KEY_PREVIEW);
        if cx.view.composer.mode() == EditorMode::Insert {
            if key.code == extui::event::KeyCode::Tab
                && key.modifiers.is_empty()
                && cx.view.complete_command()
            {
                cx.narrow_dirty(EDITOR_DIRTY);
                return Action::Continue;
            }
            if key.code != extui::event::KeyCode::Esc {
                match resolve_binding_cx(cx, bindings::INSERT_LAYER, key) {
                    BindingResolution::Action(command) => {
                        return self.process_action(cx, command);
                    }
                    BindingResolution::Consumed => {
                        cx.narrow_dirty(CHORD_DIRTY);
                        return Action::Continue;
                    }
                    BindingResolution::Unmatched => {}
                }
            }
            if cx.view.composer.send_key(&key) {
                maybe_auto_close_markdown_code_fence(&mut cx.view.composer, key);
            }
            cx.narrow_dirty(EDITOR_DIRTY);
            return Action::Continue;
        }

        match resolve_binding_cx(cx, bindings::COMPOSE_NORMAL_LAYER, key) {
            BindingResolution::Action(command) => return self.process_action(cx, command),
            BindingResolution::Consumed => {
                cx.narrow_dirty(CHORD_DIRTY);
                return Action::Continue;
            }
            BindingResolution::Unmatched => {}
        }
        let _ = cx.view.composer.send_key(&key);
        cx.narrow_dirty(EDITOR_DIRTY);
        Action::Continue
    }

    /// Reads the clipboard and either inserts text into the composer or opens
    /// the image upload dialog. Text focuses the composer first so the paste is
    /// visible from any room focus, while a command issued from compose Normal
    /// mode leaves the editor in Normal mode.
    fn paste_from_clipboard(
        &mut self,
        cx: &mut ViewCx<'_>,
        provider: &dyn crate::clipboard_paste::ClipboardPasteProvider,
    ) {
        use crate::clipboard_paste::PastePayload;
        match provider.read_paste() {
            Ok(PastePayload::Text(text)) => {
                let preserve_normal = self.focus == ChatPanelFocus::Compose
                    && cx.view.composer.mode() == EditorMode::Normal;
                if preserve_normal {
                    cx.view.put_paste_after_cursor(text);
                } else {
                    self.enter_compose_insert_mode(cx);
                    cx.view.insert_paste(text);
                }
            }
            Ok(PastePayload::Image(image)) => {
                let dialog = PasteImageUploadMode::new(image, &cx.view.theme);
                cx.request_transition(ModeTransition::Push(Box::new(dialog)));
            }
            Ok(PastePayload::Empty) => cx.set_status("clipboard is empty"),
            Ok(PastePayload::Unsupported(reason)) => cx.set_status(reason),
            Err(error) => cx.set_error(error.to_string()),
        }
    }

    fn process_chat_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        let rect = self.layout.chat_rect;
        let in_chat = crate::tui::form::rect_contains(rect, mouse.column, mouse.row);
        let in_chat_bar =
            crate::tui::form::rect_contains(self.layout.chat_log_bar_rect, mouse.column, mouse.row);
        let in_room_list =
            crate::tui::form::rect_contains(self.layout.room_list_rect, mouse.column, mouse.row);
        let in_user_list =
            crate::tui::form::rect_contains(self.layout.user_list_rect, mouse.column, mouse.row);
        let in_lobby_bar =
            crate::tui::form::rect_contains(self.layout.lobby_bar_rect, mouse.column, mouse.row);
        let in_composer = crate::tui::form::rect_contains(
            self.layout.composer_frame_rect,
            mouse.column,
            mouse.row,
        ) || crate::tui::form::rect_contains(
            self.layout.compose_bar_rect,
            mouse.column,
            mouse.row,
        );
        let scrollbar = self
            .layout
            .rooms_scrollbar
            .filter(|scrollbar| scrollbar.contains(mouse.column, mouse.row))
            .or_else(|| {
                self.layout
                    .composer_scrollbar
                    .filter(|scrollbar| scrollbar.contains(mouse.column, mouse.row))
            });
        let transfer_hit = cx
            .view
            .chrome
            .transfer_buttons
            .iter()
            .find(|(rect, _)| crate::tui::form::rect_contains(*rect, mouse.column, mouse.row))
            .map(|(_, id)| *id);

        match mouse.kind {
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left)
                if transfer_hit.is_some() =>
            {
                if let Some(transfer_id) = transfer_hit {
                    cx.send(CoreCommand::CancelTransfer(transfer_id));
                }
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left)
                if scrollbar.is_some() =>
            {
                self.start_scrollbar_drag(cx, scrollbar.unwrap(), mouse.column, mouse.row);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_composer => {
                self.enter_compose_insert_mode(cx);
                if crate::tui::form::rect_contains(
                    self.layout.composer_frame_rect,
                    mouse.column,
                    mouse.row,
                ) {
                    move_composer_cursor_to_click(
                        &mut cx.view.composer,
                        self.layout.composer_rect,
                        mouse.column,
                        mouse.row,
                    );
                }
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left)
                if crate::tui::form::rect_contains(
                    cx.view.chrome.lobby_bar.audio_reset,
                    mouse.column,
                    mouse.row,
                ) =>
            {
                cx.send(CoreCommand::AudioManualReset);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left)
                if crate::tui::form::rect_contains(
                    cx.view.chrome.lobby_bar.call_button,
                    mouse.column,
                    mouse.row,
                ) =>
            {
                if cx.session.voice_room.is_some() {
                    cx.send(CoreCommand::LeaveVoice);
                } else if let Some(room_id) = cx.view.viewed_room {
                    cx.send(CoreCommand::JoinVoice(room_id));
                } else {
                    cx.set_status("no room selected");
                }
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_lobby_bar => {
                self.set_focus_cx(cx, ChatPanelFocus::Lobby);
                self.divider_drag = Some(DividerDrag::LobbyBar {
                    anchor_row: mouse.row,
                    start_room_height: self.layout.room_list_rect.h.max(1),
                });
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_room_list => {
                self.set_lobby_list_focus(cx, LobbyListFocus::Rooms);
                if let Some(room_id) = self.layout.room_hit(mouse.column, mouse.row) {
                    cx.send(CoreCommand::SetViewedRoom(room_id));
                }
            }
            extui::event::MouseEventKind::ScrollUp if in_room_list => {
                self.set_lobby_list_focus(cx, LobbyListFocus::Rooms);
                self.cycle_room(cx, -1);
            }
            extui::event::MouseEventKind::ScrollDown if in_room_list => {
                self.set_lobby_list_focus(cx, LobbyListFocus::Rooms);
                self.cycle_room(cx, 1);
            }
            extui::event::MouseEventKind::ScrollUp if in_user_list => {
                self.move_user_selection_with_focus(cx, -1);
            }
            extui::event::MouseEventKind::ScrollDown if in_user_list => {
                self.move_user_selection_with_focus(cx, 1);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_user_list => {
                self.set_lobby_list_focus(cx, LobbyListFocus::Users);
                let row = mouse.row.saturating_sub(self.layout.user_list_rect.y) as usize;
                let visible_rows = self.visible_user_rows(cx);
                let participants = cx.session.participant_snapshot(cx.view.viewed_room);
                cx.view
                    .select_visible_participant(&participants.entries, row, visible_rows);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_chat_bar => {
                self.set_focus_cx(cx, ChatPanelFocus::ChatLog);
                self.divider_drag = Some(DividerDrag::ChatLogBar {
                    anchor_row: mouse.row,
                    start_rows: self.layout.composer_rect.h,
                });
            }
            extui::event::MouseEventKind::ScrollUp if in_chat => {
                let was_focused = self.focus == ChatPanelFocus::ChatLog;
                self.set_focus_cx(cx, ChatPanelFocus::ChatLog);
                self.scroll_chat_up(cx, 5);
                if was_focused {
                    cx.narrow_dirty(CHAT_SCROLL_DIRTY);
                }
            }
            extui::event::MouseEventKind::ScrollDown if in_chat => {
                let was_focused = self.focus == ChatPanelFocus::ChatLog;
                self.set_focus_cx(cx, ChatPanelFocus::ChatLog);
                cx.view.active.chat.scroll_down(5);
                if was_focused {
                    cx.narrow_dirty(CHAT_SCROLL_DIRTY);
                }
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_chat => {
                self.set_focus_cx(cx, ChatPanelFocus::ChatLog);
                match self.layout.chat_line_at(mouse.row) {
                    Some(line) => match line.kind {
                        LineKind::Heading | LineKind::Ellipsis => {
                            cx.view
                                .active
                                .chat
                                .toggle_expand(line.message, self.layout.chat_width);
                            cx.view
                                .active
                                .chat
                                .clamp_scroll(self.layout.chat_width, self.layout.chat_height);
                        }
                        LineKind::Body => {
                            cx.view.active.chat.begin_drag(ChatCursor {
                                message: line.message,
                                line: line.line,
                            });
                        }
                    },
                    _ => {
                        cx.view.active.chat.clear_visual_anchor();
                    }
                }
            }
            extui::event::MouseEventKind::Drag(extui::event::MouseButton::Left)
                if self.scrollbar_drag.is_some() =>
            {
                self.drag_scrollbar(cx, mouse.row);
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left)
                if self.scrollbar_drag.is_some() =>
            {
                self.scrollbar_drag = None;
            }
            extui::event::MouseEventKind::Drag(extui::event::MouseButton::Left)
                if self.divider_drag.is_some() =>
            {
                self.drag_divider(cx, mouse.row);
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left)
                if self.divider_drag.is_some() =>
            {
                self.divider_drag = None;
            }
            extui::event::MouseEventKind::Drag(extui::event::MouseButton::Left)
                if cx.view.active.chat.is_dragging() =>
            {
                self.set_focus_cx(cx, ChatPanelFocus::ChatLog);
                self.drag_chat_selection(cx, mouse.row);
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left) if in_chat => {
                // A click (press and release without a drag) over a message
                // reference jumps to it and over a URL opens it; a drag remains
                // a visual selection.
                if cx.view.active.chat.drag_is_click() {
                    if let Some(target) = self.chat_ref_at(cx, mouse.column, mouse.row) {
                        self.jump_to_ref(cx, target);
                    } else if let Some(url) = self.chat_link_at(cx, mouse.column, mouse.row) {
                        cx.view.request_open_url(url);
                    }
                }
                cx.view.active.chat.end_drag();
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left) => {
                cx.view.active.chat.end_drag();
            }
            _ => {}
        }
        Action::Continue
    }

    fn start_scrollbar_drag(
        &mut self,
        cx: &mut ViewCx<'_>,
        scrollbar: ScrollbarLayout,
        column: u16,
        row: u16,
    ) {
        let Some(press) = scrollbar.press(column, row) else {
            return;
        };
        match scrollbar.id {
            ScrollbarId::Rooms => self.set_lobby_list_focus(cx, LobbyListFocus::Rooms),
            ScrollbarId::Compose => self.enter_compose_insert_mode(cx),
        }
        self.scrollbar_drag = Some(press.drag);
        if let Some(target) = press.target {
            self.apply_scrollbar_target(cx, scrollbar, target);
        }
    }

    fn drag_scrollbar(&mut self, cx: &mut ViewCx<'_>, row: u16) {
        let Some(mut drag) = self.scrollbar_drag else {
            return;
        };
        let scrollbar = match drag.id {
            ScrollbarId::Rooms => self.layout.rooms_scrollbar,
            ScrollbarId::Compose => self.layout.composer_scrollbar,
        };
        let Some(scrollbar) = scrollbar else {
            self.scrollbar_drag = None;
            return;
        };
        let Some(target) = scrollbar.drag_target(drag, row) else {
            self.scrollbar_drag = None;
            return;
        };
        self.apply_scrollbar_target(cx, scrollbar, target);
        drag.current_offset = target;
        self.scrollbar_drag = Some(drag);
    }

    fn apply_scrollbar_target(
        &mut self,
        cx: &mut ViewCx<'_>,
        scrollbar: ScrollbarLayout,
        target: u32,
    ) {
        match scrollbar.id {
            ScrollbarId::Rooms => {
                cx.view.rooms_offset = target as usize;
                cx.view.clamp_rooms_offset(
                    scrollbar.state.total as usize,
                    scrollbar.state.viewport as usize,
                );
            }
            ScrollbarId::Compose => scroll_composer_to_offset(
                &mut cx.view.composer,
                self.layout.composer_rect,
                self.scrollbar_drag
                    .map_or(scrollbar.state.offset, |drag| drag.current_offset),
                target,
                scrollbar.state.viewport,
            ),
        }
    }

    /// Resolves a screen cell to the URL of a link under it, if any. Returns an
    /// owned string so the caller can mutably borrow the view to queue the open.
    fn chat_link_at(&self, cx: &ViewCx<'_>, column: u16, row: u16) -> Option<String> {
        let line = self.layout.chat_line_at(row)?;
        if line.kind != LineKind::Body {
            return None;
        }
        // Content starts one column right of the marker gutter (see render.rs).
        let content_x = self.layout.chat_rect.x.saturating_add(1);
        if column < content_x {
            return None;
        }
        let col_in_line = column - content_x;
        cx.view
            .active
            .chat
            .link_at(line.message, line.line, col_in_line)
            .map(str::to_owned)
    }

    /// Resolves a screen cell to the message reference under it, if any.
    fn chat_ref_at(
        &self,
        cx: &ViewCx<'_>,
        column: u16,
        row: u16,
    ) -> Option<rpc::msgref::MessageRef> {
        let line = self.layout.chat_line_at(row)?;
        if line.kind != LineKind::Body {
            return None;
        }
        let content_x = self.layout.chat_rect.x.saturating_add(1);
        if column < content_x {
            return None;
        }
        let col_in_line = column - content_x;
        cx.view
            .active
            .chat
            .ref_at(line.message, line.line, col_in_line)
    }

    /// Jumps to a reference's target: selects and scrolls to the message when
    /// present in the buffer, otherwise reports why not.
    fn jump_to_ref(&mut self, cx: &mut ViewCx<'_>, target: rpc::msgref::MessageRef) {
        match cx
            .view
            .jump_to_ref(target, self.layout.chat_width, self.layout.chat_height)
        {
            crate::app::room::RefJump::Jumped => self.set_focus_cx(cx, ChatPanelFocus::ChatLog),
            crate::app::room::RefJump::NotFound => {
                cx.set_status("referenced message is not in this room's history");
            }
            crate::app::room::RefJump::OtherRoom => {
                self.set_focus_cx(cx, ChatPanelFocus::ChatLog);
                cx.send(CoreCommand::OpenMessageRef {
                    target,
                    width: self.layout.chat_width,
                    height: self.layout.chat_height,
                });
            }
        }
    }

    fn copy_message_ref_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        match cx.view.copy_message_ref(self.layout.chat_width) {
            Some(code) => cx.set_status(format!("copied {code}")),
            None => cx.set_status("select a message to reference"),
        }
    }

    fn insert_message_ref_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        if cx.view.insert_message_ref(self.layout.chat_width).is_some() {
            self.enter_compose_insert_mode(cx);
        } else {
            cx.set_status("select a message to reference");
        }
    }

    fn open_message_ref_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        cx.view.active.chat.ensure_cursor(self.layout.chat_width);
        let Some(target) = cx.view.active.chat.cursor_ref() else {
            cx.set_status("selected message contains no reference");
            return;
        };
        self.jump_to_ref(cx, target);
    }

    fn drag_chat_selection(&mut self, cx: &mut ViewCx<'_>, row: u16) {
        let rect = self.layout.chat_rect;
        if row < rect.y {
            self.scroll_chat_up(cx, 1);
        } else if row >= rect.y.saturating_add(rect.h) {
            cx.view.active.chat.scroll_down(1);
        }
        let clamped = row.clamp(rect.y, rect.y.saturating_add(rect.h).saturating_sub(1));
        if let Some(line) = self.layout.chat_line_at(clamped)
            && line.kind == LineKind::Body
        {
            cx.view.active.chat.drag_to(ChatCursor {
                message: line.message,
                line: line.line,
            });
        }
    }

    /// Applies an in-progress divider drag, trading rows between the chat log
    /// and the neighboring region. Both dividers keep the chat log at a
    /// minimum of one row.
    fn drag_divider(&mut self, cx: &mut ViewCx<'_>, row: u16) {
        match self.divider_drag {
            Some(DividerDrag::LobbyBar {
                anchor_row,
                start_room_height,
            }) => {
                // Dragging down grows the rooms/users block; the block and the
                // chat log share their rows (the 1-row bar between is fixed).
                let delta = i32::from(row) - i32::from(anchor_row);
                let budget = i32::from(start_room_height) + i32::from(self.layout.chat_rect.h);
                // Keep at least one lobby row and one chat row.
                let height = (i32::from(start_room_height) + delta).clamp(1, (budget - 1).max(1));
                cx.send(CoreCommand::SetRoomHeight(height as u16));
            }
            Some(DividerDrag::ChatLogBar {
                anchor_row,
                start_rows,
            }) => {
                // Dragging up grows the compose window; it and the chat log
                // share their rows. The dragged height becomes the composer's
                // fixed viewport and is allowed past `max_composer_height`.
                let delta = i32::from(anchor_row) - i32::from(row);
                let budget = i32::from(start_rows) + i32::from(self.layout.chat_rect.h);
                let rows = (i32::from(start_rows) + delta).clamp(1, (budget - 1).max(1)) as u16;
                cx.view.composer_rows = Some(rows);
            }
            None => {}
        }
    }

    fn toggle_selected_log_collapse(&mut self, cx: &mut ViewCx<'_>) {
        let width = self.layout.chat_width;
        match cx.view.toggle_cursor_message_expand(width) {
            ToggleExpandResult::Toggled => {}
            ToggleExpandResult::NoMessages => {
                cx.set_status("no messages");
                return;
            }
            ToggleExpandResult::NotCollapsible => {
                cx.set_status("selected log is not collapsible");
            }
        }
        cx.view
            .active
            .chat
            .clamp_scroll(width, self.layout.chat_height);
        self.keep_chat_cursor_visible(cx);
    }

    fn scroll_chat_up(&mut self, cx: &mut ViewCx<'_>, rows: usize) {
        cx.view
            .active
            .chat
            .scroll_up(rows, self.layout.chat_width, self.layout.chat_height);
        if cx
            .view
            .active
            .chat
            .is_at_top(self.layout.chat_width, self.layout.chat_height)
            && let Some(room_id) = cx.view.viewed_room
        {
            cx.send(CoreCommand::RequestOlderHistory { room_id });
        }
    }

    fn copy_chat_selection(&mut self, cx: &mut ViewCx<'_>) {
        if cx
            .view
            .copy_chat_selection(self.layout.chat_width)
            .is_some()
        {
            cx.set_transient_status("copied to clipboard");
        }
    }

    fn select_chat_top(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus == ChatPanelFocus::ChatLog {
            cx.narrow_dirty(CHAT_SCROLL_DIRTY);
            cx.view
                .active
                .chat
                .top(self.layout.chat_width, self.layout.chat_height);
            cx.view.active.chat.cursor_to_first();
            self.keep_chat_cursor_visible(cx);
            if cx
                .view
                .active
                .chat
                .is_at_top(self.layout.chat_width, self.layout.chat_height)
                && let Some(room_id) = cx.view.viewed_room
            {
                cx.send(CoreCommand::RequestOlderHistory { room_id });
            }
        }
    }

    fn select_chat_bottom(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus == ChatPanelFocus::ChatLog {
            cx.narrow_dirty(CHAT_SCROLL_DIRTY);
            cx.view.active.chat.bottom();
            cx.view.active.chat.cursor_to_last(self.layout.chat_width);
            self.keep_chat_cursor_visible(cx);
        }
    }

    fn copy_chat_selection_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus == ChatPanelFocus::ChatLog {
            self.copy_chat_selection(cx);
        }
    }

    fn copy_cursor_line_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        if cx.view.copy_cursor_line(self.layout.chat_width).is_some() {
            cx.set_transient_status("copied line to clipboard");
        }
    }

    fn copy_cursor_message_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        if cx
            .view
            .copy_cursor_message(self.layout.chat_width)
            .is_some()
        {
            cx.set_transient_status("copied message to clipboard");
        }
    }

    fn move_chat_cursor_paragraph(&mut self, cx: &mut ViewCx<'_>, delta: isize) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        cx.narrow_dirty(CHAT_SCROLL_DIRTY);
        if cx
            .view
            .active
            .chat
            .move_cursor_paragraph(delta, self.layout.chat_width)
            .is_none()
        {
            cx.set_status("no messages");
            return;
        }
        self.keep_chat_cursor_visible(cx);
    }

    fn toggle_chat_visual_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        cx.view
            .active
            .chat
            .toggle_visual_anchor(self.layout.chat_width);
    }

    fn toggle_chat_expand_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus == ChatPanelFocus::ChatLog {
            self.toggle_selected_log_collapse(cx);
        }
    }

    /// Starts editing the cursor message: the composer takes the original
    /// body and focus moves to it without forcing insert mode, so Vim
    /// bindings begin in Normal mode.
    fn edit_cursor_message_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        match cx
            .view
            .begin_edit_cursor_message(cx.session, self.layout.chat_width)
        {
            Ok(()) => {
                self.set_focus_cx(cx, ChatPanelFocus::Compose);
                cx.set_status("editing message; submit to save");
            }
            Err(denied) => cx.set_status(denied.status()),
        }
    }

    fn delete_selected_messages_if_focused(&mut self, cx: &mut ViewCx<'_>) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        let selection = match cx.view.delete_selection(cx.session, self.layout.chat_width) {
            Ok(selection) => selection,
            Err(denied) => {
                cx.set_status(denied.status());
                return;
            }
        };
        let DeleteSelection {
            room_id,
            targets,
            skipped,
        } = selection;
        let count = targets.len();
        let noun = if count == 1 { "message" } else { "messages" };
        let mut prompt = format!("Delete {count} {noun}?");
        if skipped > 0 {
            prompt.push_str(&format!(" ({skipped} skipped)"));
        }
        kvlog::info!(
            "chat delete confirmation opened",
            room_id = room_id.0,
            target_count = count,
            skipped
        );
        cx.request_transition(ModeTransition::Push(Box::new(ConfirmMode::new(
            prompt,
            "Delete",
            "Cancel",
            move |cx| {
                cx.send(CoreCommand::DeleteMessages {
                    room_id,
                    targets,
                    skipped,
                });
                ConfirmDisposition::Close
            },
        ))));
    }

    fn scroll_focused_panel(&mut self, cx: &mut ViewCx<'_>, direction: isize) {
        match self.focus {
            ChatPanelFocus::ChatLog => {
                self.move_chat_cursor(cx, direction);
                cx.narrow_dirty(CHAT_SCROLL_DIRTY);
            }
            ChatPanelFocus::Lobby => match self.lobby_list_focus {
                LobbyListFocus::Rooms => self.move_room_view_with_focus(cx, direction),
                LobbyListFocus::Users => self.move_user_selection_with_focus(cx, direction),
            },
            ChatPanelFocus::Compose => {}
        }
    }

    fn scroll_chat_log_if_focused(&mut self, cx: &mut ViewCx<'_>, rows: isize) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        if rows < 0 {
            self.scroll_chat_up(cx, rows.unsigned_abs());
        } else {
            cx.view.active.chat.scroll_down(rows as usize);
        }
        cx.narrow_dirty(CHAT_SCROLL_DIRTY);
    }

    fn chat_half_page_rows(&self) -> usize {
        (self.layout.chat_height as usize / 2).max(1)
    }

    fn move_chat_cursor(&mut self, cx: &mut ViewCx<'_>, delta: isize) {
        self.set_focus_cx(cx, ChatPanelFocus::ChatLog);
        if !cx.view.move_chat_cursor(delta, self.layout.chat_width) {
            cx.set_status("no messages");
            return;
        }
        self.keep_chat_cursor_visible(cx);
    }

    fn keep_chat_cursor_visible(&mut self, cx: &mut ViewCx<'_>) {
        cx.view
            .active
            .chat
            .keep_cursor_visible(self.layout.chat_width, self.layout.chat_height);
    }

    fn move_room_view_with_focus(&mut self, cx: &mut ViewCx<'_>, delta: isize) {
        self.lobby_list_focus = LobbyListFocus::Rooms;
        self.set_focus_cx(cx, ChatPanelFocus::Lobby);
        self.cycle_room(cx, delta);
    }

    fn move_user_selection_with_focus(&mut self, cx: &mut ViewCx<'_>, delta: isize) {
        self.lobby_list_focus = LobbyListFocus::Users;
        self.set_focus_cx(cx, ChatPanelFocus::Lobby);
        self.move_user_selection(cx, delta);
    }

    fn move_user_selection(&mut self, cx: &mut ViewCx<'_>, delta: isize) {
        let visible_rows = self.visible_user_rows(cx);
        let participants = cx.session.participant_snapshot(cx.view.viewed_room);
        if cx
            .view
            .move_participant_selection(&participants.entries, delta, visible_rows)
            .is_none()
        {
            cx.set_status("no users in the current room yet");
        }
        self.focus = ChatPanelFocus::Lobby;
        self.lobby_list_focus = LobbyListFocus::Users;
    }

    fn visible_user_rows(&self, cx: &ViewCx<'_>) -> usize {
        // Before the first render the participant rect is empty; fall back to
        // the configured lobby height.
        let fallback = cx.config.ui.room_height.max(1);
        self.layout.user_list_rect.h.max(fallback) as usize
    }

    fn keep_selected_room_user_visible(&mut self, cx: &mut ViewCx<'_>) {
        let visible_rows = self.visible_user_rows(cx);
        let participants = cx.session.participant_snapshot(cx.view.viewed_room);
        cx.view
            .keep_participant_selection_visible(&participants.entries, visible_rows);
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if let Some(search) = &mut self.history_search {
            if search.process_key(
                &mut cx.view.active.chat,
                key,
                self.layout.chat_width,
                self.layout.chat_height,
            ) == SearchAction::Close
            {
                self.last_history_search = self.history_search.take();
            }
            return Action::Continue;
        }
        if self.focus == ChatPanelFocus::ChatLog
            && self.last_history_search.is_some()
            && !matches!(key.kind, KeyEventKind::Release)
            && key.code == KeyCode::Esc
            && key.modifiers.is_empty()
        {
            self.last_history_search = None;
            return Action::Continue;
        }
        if self.focus == ChatPanelFocus::ChatLog
            && self.last_history_search.is_some()
            && !matches!(key.kind, KeyEventKind::Release)
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Char('n' | 'N'))
        {
            if let Some(search) = &mut self.last_history_search {
                let delta = if key.code == KeyCode::Char('N') {
                    -1
                } else {
                    1
                };
                search.repeat(
                    &mut cx.view.active.chat,
                    delta,
                    self.layout.chat_width,
                    self.layout.chat_height,
                );
            }
            return Action::Continue;
        }
        if self.focus == ChatPanelFocus::ChatLog
            && !matches!(key.kind, KeyEventKind::Release)
            && key.code == KeyCode::Char('/')
            && key.modifiers.is_empty()
        {
            self.last_history_search = None;
            self.history_search = Some(HistorySearch::new(&cx.view.active.chat));
            return Action::Continue;
        }
        if self.focus == ChatPanelFocus::Compose {
            return self.process_compose_key(cx, key);
        }
        let layer = self.workspace_layer_cx(cx);
        match resolve_binding_cx(cx, layer, key) {
            BindingResolution::Action(command) => self.process_action(cx, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    fn process_top_bar_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> bool {
        process_top_bar_mouse_cx(cx, mouse)
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if self.process_top_bar_mouse(cx, mouse) {
            return Action::Continue;
        }
        self.process_chat_mouse(cx, mouse)
    }

    fn process_paste_cx(&mut self, cx: &mut ViewCx<'_>, text: String) {
        self.enter_compose_insert_mode(cx);
        cx.view.insert_paste(text);
    }
}

impl AppMode for RoomMode {
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, now_ms: u64, dirty: DirtySections) {
        let chrome = self.presentation(cx).chrome.expect("base mode has chrome");
        let mut render = crate::tui::render::RenderState::new(cx);
        crate::tui::render::draw_room_screen(
            &mut render,
            self.focus,
            self.lobby_list_focus,
            &mut self.layout,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            self.history_search.as_mut(),
            self.last_history_search.as_ref(),
            buf,
            now_ms,
            dirty,
        );
    }

    fn section_rendering(&self) -> bool {
        true
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        self.process_mouse_cx(cx, mouse)
    }

    fn process_paste(&mut self, cx: &mut ViewCx<'_>, text: String) {
        self.process_paste_cx(cx, text);
    }

    fn presentation(&self, cx: &ViewCx<'_>) -> ModePresentation {
        if self.history_search.is_some() {
            return ModePresentation::full_screen(ChromeSpec {
                theme_mode: theme::UiMode::Compose,
                status_label: "Search",
                layer: bindings::INSERT_LAYER,
            });
        }
        let theme_mode = if self.focus == ChatPanelFocus::Compose {
            theme::UiMode::Compose
        } else {
            theme::UiMode::Log
        };
        let layer = if self.focus != ChatPanelFocus::Compose {
            if self.focus == ChatPanelFocus::ChatLog && cx.view.active.chat.has_visual() {
                bindings::CHAT_VISUAL_LAYER
            } else {
                bindings::WORKSPACE_LAYER
            }
        } else if cx.view.composer.mode() == EditorMode::Insert {
            bindings::INSERT_LAYER
        } else {
            bindings::COMPOSE_NORMAL_LAYER
        };
        ModePresentation::full_screen(ChromeSpec {
            theme_mode,
            status_label: "Compose",
            layer,
        })
    }
}

#[cfg(test)]
macro_rules! app_mode_test_bridge {
    ($($mode:ty),+ $(,)?) => {
        $(
            #[allow(dead_code)]
            impl $mode {
                pub(crate) fn render(
                    &mut self,
                    app: &mut App,
                    buf: &mut Buffer,
                    now_ms: u64,
                ) {
                    {
                        let mut cx = app.view_cx();
                        AppMode::render(self, &mut cx, buf, now_ms, DirtySections::ALL);
                    }
                    app.drain_core_commands();
                }

                pub(crate) fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
                    let action = {
                        let mut cx = app.view_cx();
                        AppMode::process_input(self, &mut cx, key)
                    };
                    app.drain_core_commands();
                    action
                }

                pub(crate) fn process_mouse(
                    &mut self,
                    app: &mut App,
                    mouse: MouseEvent,
                ) -> Action {
                    let action = {
                        let mut cx = app.view_cx();
                        AppMode::process_mouse(self, &mut cx, mouse)
                    };
                    app.drain_core_commands();
                    action
                }

                pub(crate) fn process_paste(&mut self, app: &mut App, text: String) {
                    {
                        let mut cx = app.view_cx();
                        AppMode::process_paste(self, &mut cx, text);
                    }
                    app.drain_core_commands();
                }
            }
        )+
    };
}

#[cfg(test)]
app_mode_test_bridge!(
    WelcomeMode,
    ServerListMode,
    RoomSwitchMode,
    ServerEditMode,
    SettingsMode,
    RoomMode,
);

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use extui::{
        Buffer, Style,
        event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    };
    use extui_editor::Mode as EditorMode;
    use rpc::{
        control::{ChatMessage, ParticipantVoiceStatus},
        ids::{FileTransferId, MessageId, RoomId, UserId},
    };
    use toml_spanner::Arena;

    use super::*;
    use crate::{chat_buffer::LineKind, client_net::TerminalVerb, config::Config};

    fn test_app() -> App {
        App::new(Config::default(), None).expect("test app")
    }

    /// An app whose composer uses Vim bindings, for tests exercising the
    /// compose Normal mode.
    fn test_app_vim() -> App {
        let mut config = Config::default();
        config.ui.default_bindings = crate::config::DefaultBindings::Vim;
        App::new(config, None).expect("test app")
    }

    fn app_with_bindings(bindings: &str) -> App {
        let arena = Arena::new();
        let config: Config = toml_spanner::parse(bindings, &arena)
            .expect("bindings parse")
            .to()
            .expect("bindings deserialize");
        App::new(config, None).expect("test app")
    }

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::empty())
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    struct TextClipboard(&'static str);

    impl crate::clipboard_paste::ClipboardPasteProvider for TextClipboard {
        fn read_paste(
            &self,
        ) -> Result<crate::clipboard_paste::PastePayload, crate::clipboard_paste::ClipboardPasteError>
        {
            Ok(crate::clipboard_paste::PastePayload::Text(
                self.0.to_string(),
            ))
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    #[test]
    fn bracketed_paste_inserts_from_any_focus() {
        for focus in [
            ChatPanelFocus::Lobby,
            ChatPanelFocus::ChatLog,
            ChatPanelFocus::Compose,
        ] {
            let mut app = test_app();
            let mut room = RoomMode::with_focus(focus);
            room.process_paste(&mut app, "pasted".to_string());
            assert!(
                app.view.composer.text().contains("pasted"),
                "focus {focus:?} did not receive paste"
            );
        }
    }

    #[test]
    fn typed_markdown_code_fence_auto_closes() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "```");

        assert_eq!(app.view.composer.text(), "```\n```");
        assert_eq!(app.view.composer.cursor_offset(), 3);
    }

    #[test]
    fn typed_markdown_code_fence_language_inserts_before_closer() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "```rust");

        assert_eq!(app.view.composer.text(), "```rust\n```");
        assert_eq!(app.view.composer.cursor_offset(), "```rust".len() as u32);
    }

    #[test]
    fn typed_markdown_code_fence_after_prose_auto_closes() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "Hello ```rust");

        assert_eq!(app.view.composer.text(), "Hello ```rust\n```");
        assert_eq!(
            app.view.composer.cursor_offset(),
            "Hello ```rust".len() as u32
        );
    }

    #[test]
    fn typed_markdown_code_fence_inside_line_does_not_auto_close() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.view.composer.set_lines("``x");
        app.view.composer.set_cursor_offset(2);
        app.view.composer.enter_insert_mode();

        room.process_input(&mut app, key('`'));

        assert_eq!(app.view.composer.text(), "```x");
        assert_eq!(app.view.composer.cursor_offset(), 3);
    }

    #[test]
    fn pasted_markdown_code_fence_does_not_auto_close() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        room.process_paste(&mut app, "```".to_string());

        assert_eq!(app.view.composer.text(), "```");
        assert_eq!(app.view.composer.cursor_offset(), 3);
    }

    #[test]
    fn direct_insert_paste_markdown_code_fence_does_not_auto_close() {
        let mut app = test_app();

        app.view.insert_paste("```".to_string());

        assert_eq!(app.view.composer.text(), "```");
        assert_eq!(app.view.composer.cursor_offset(), 3);
    }

    #[test]
    fn paste_command_resolves_from_room_layers() {
        let mut app = test_app();
        let cases = [
            (bindings::WORKSPACE_LAYER, key('p')),
            (bindings::COMPOSE_NORMAL_LAYER, key('p')),
            (bindings::WORKSPACE_LAYER, ctrl('v')),
            (bindings::COMPOSE_NORMAL_LAYER, ctrl('v')),
            (bindings::INSERT_LAYER, ctrl('v')),
        ];
        for (layer, event) in cases {
            match resolve_binding(&mut app, layer, event) {
                BindingResolution::Action(BindCommand::PasteClipboard) => {}
                other => panic!("expected paste command, got {other:?}"),
            }
        }
    }

    #[test]
    fn paste_command_puts_after_cursor_and_preserves_vim_normal_mode() {
        let mut app = test_app_vim();
        let mut room = RoomMode::default();
        app.view.composer.set_lines("tail");
        app.view.composer.set_cursor_offset(4);
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);

        {
            let mut cx = app.view_cx();
            room.paste_from_clipboard(&mut cx, &TextClipboard(" pasted"));
        }

        assert_eq!(app.view.composer.text(), "tail pasted");
        assert_eq!(app.view.composer.cursor_offset(), 10);
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);
    }

    #[test]
    fn paste_command_appends_at_insert_cursor_in_standard_mode() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.view.composer.set_lines("tail");
        app.view.composer.set_cursor_offset(4);

        {
            let mut cx = app.view_cx();
            room.paste_from_clipboard(&mut cx, &TextClipboard(" pasted"));
        }

        assert_eq!(app.view.composer.text(), "tail pasted");
        assert_eq!(app.view.composer.cursor_offset(), 11);
        assert_eq!(app.view.composer.mode(), EditorMode::Insert);
    }

    #[test]
    fn insert_layer_p_stays_literal_text() {
        let mut app = test_app();
        match resolve_binding(&mut app, bindings::INSERT_LAYER, key('p')) {
            BindingResolution::Unmatched => {}
            other => panic!("expected unmatched, got {other:?}"),
        }
    }

    fn type_text(room: &mut RoomMode, app: &mut App, text: &str) {
        for ch in text.chars() {
            room.process_input(app, key(ch));
        }
    }

    fn render_room(app: &mut App, room: &mut RoomMode, buffer: &mut Buffer) {
        // The runtime loop catches the view up to the session before painting.
        app.view.sync_active(&app.room);
        room.render(app, buffer, 0);
    }

    fn render_room_sections(
        app: &mut App,
        room: &mut RoomMode,
        buffer: &mut Buffer,
        dirty: DirtySections,
    ) {
        app.view.sync_active(&app.room);
        {
            let mut cx = app.view_cx();
            AppMode::render(room, &mut cx, buffer, 0, dirty);
        }
        app.drain_core_commands();
    }

    /// Renders one gated frame into a retained buffer and returns the bytes
    /// it emitted, feeding them to `term` when given.
    fn gated_frame(
        app: &mut App,
        room: &mut RoomMode,
        buffer: &mut Buffer,
        dirty: DirtySections,
        term: Option<&mut vt100::Parser>,
    ) -> usize {
        render_room_sections(app, room, buffer, dirty);
        buffer.render_internal();
        if let Some(term) = term {
            term.process(buffer.write_buffer());
        }
        let emitted = buffer.write_buffer().len();
        buffer.buf.clear();
        emitted
    }

    fn cell_style(buffer: &mut Buffer, column: u16, row: u16) -> Style {
        let grid = buffer.current();
        grid.cells()[(row as usize * grid.width() as usize) + column as usize].style()
    }

    fn cell_text(buffer: &mut Buffer, column: u16, row: u16) -> String {
        let grid = buffer.current();
        let cell = grid.cells()[(row as usize * grid.width() as usize) + column as usize];
        if cell.is_handle() {
            String::from_utf8_lossy(grid.handle_text(cell).unwrap_or_default()).to_string()
        } else {
            cell.text_inline().unwrap_or_default().to_string()
        }
    }

    fn row_text(buffer: &mut Buffer, row: u16) -> String {
        let width = buffer.current().width();
        let mut text = String::new();
        for column in 0..width {
            text.push_str(&cell_text(buffer, column, row));
        }
        text
    }

    fn enter_room_one(app: &mut App) {
        if app.room.viewed_room.is_some() {
            return;
        }
        enter_rooms(app, &[1]);
    }

    /// Registers public rooms named `room-<id>`, viewing the first.
    fn enter_rooms(app: &mut App, ids: &[u32]) {
        let rooms: Vec<rpc::control::RoomInfo> = ids
            .iter()
            .map(|id| rpc::control::RoomInfo {
                room_id: RoomId(*id),
                name: if *id == 1 {
                    "lobby".to_string()
                } else {
                    format!("room-{id}")
                },
                kind: rpc::control::RoomKind::Public,
                head: None,
                voice_users: Vec::new(),
            })
            .collect();
        app.room
            .authenticated(&rooms, Vec::new(), RoomId(ids[0]), None, None);
        app.view.switch_room(RoomId(ids[0]), &app.room);
    }

    #[test]
    fn room_switcher_query_filters_and_enter_switches_view() {
        let mut app = test_app();
        enter_rooms(&mut app, &[1, 2, 3]);
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));

        let mut switcher = RoomSwitchMode::new();
        switcher.select.set_query("room-2");
        switcher.refresh(&app);
        assert_eq!(switcher.select.filtered_len(), 1);
        assert_eq!(switcher.selected_room(), Some(RoomId(2)));

        switcher.process_action(&mut app, BindCommand::Activate);
        assert_eq!(app.room.viewed_room, Some(RoomId(2)));
    }

    #[test]
    fn room_switcher_lists_every_room_without_a_query() {
        let mut app = test_app();
        enter_rooms(&mut app, &[1, 2, 3]);

        let mut switcher = RoomSwitchMode::new();
        switcher.refresh(&app);
        assert_eq!(switcher.select.filtered_len(), 3);
    }

    #[test]
    fn gated_idle_frame_emits_no_section_content() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        push_room_message(&mut app, 1, UserId(1), 0, "hello world");
        let mut buffer = Buffer::new(80, 24);
        buffer.set_swap(extui::Swap::Retained);

        let first = gated_frame(&mut app, &mut room, &mut buffer, DirtySections::ALL, None);
        assert!(first > 100, "the first frame paints the whole screen");

        // Nothing changed and nothing is dirty: the retained seed already
        // matches the previous frame, so only the style reset and cursor
        // bookkeeping are emitted.
        let idle = gated_frame(&mut app, &mut room, &mut buffer, DirtySections::EMPTY, None);
        assert!(
            idle < 32,
            "idle frame re-emitted section content ({idle} bytes)"
        );
    }

    #[test]
    fn compose_bar_only_frame_matches_full_redraw() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        push_room_message(&mut app, 1, UserId(1), 0, "hello world");
        push_room_message(&mut app, 2, UserId(2), 60_000, "second message");

        let mut gated = Buffer::new(80, 24);
        gated.set_swap(extui::Swap::Retained);
        let mut gated_term = vt100::Parser::new(24, 80, 0);
        gated_frame(
            &mut app,
            &mut room,
            &mut gated,
            DirtySections::ALL,
            Some(&mut gated_term),
        );

        app.view.set_status("fresh status text");
        gated_frame(
            &mut app,
            &mut room,
            &mut gated,
            DirtySections::COMPOSE_BAR,
            Some(&mut gated_term),
        );

        // Reference: a full redraw of the same state into a fresh buffer.
        let mut reference_room = RoomMode::new();
        let mut reference = Buffer::new(80, 24);
        let mut reference_term = vt100::Parser::new(24, 80, 0);
        render_room_sections(
            &mut app,
            &mut reference_room,
            &mut reference,
            DirtySections::ALL,
        );
        reference.render_internal();
        reference_term.process(reference.write_buffer());

        // The foreground of a blank cell is invisible and legitimately
        // differs between an explicit space write and an \x1b[K erase, so
        // blanks compare by background only (extui's fuzzers do the same).
        let visual = |cell: &vt100::Cell| {
            let text = cell.contents().to_owned();
            let fg = if text.trim().is_empty() {
                vt100::Color::Default
            } else {
                cell.fgcolor()
            };
            (text, fg, cell.bgcolor())
        };
        for row in 0..24 {
            for column in 0..80 {
                let gated_cell = gated_term.screen().cell(row, column).unwrap();
                let reference_cell = reference_term.screen().cell(row, column).unwrap();
                assert_eq!(
                    visual(gated_cell),
                    visual(reference_cell),
                    "gated frame diverged from full redraw at ({column},{row}): gated row {:?} vs reference row {:?}",
                    gated_term.screen().contents_between(row, 0, row, 80),
                    reference_term.screen().contents_between(row, 0, row, 80)
                );
            }
        }
    }

    #[test]
    fn skipped_sections_keep_room_layout_state() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        push_room_message(&mut app, 1, UserId(1), 0, "hello world");
        let mut buffer = Buffer::new(80, 24);
        buffer.set_swap(extui::Swap::Retained);

        gated_frame(&mut app, &mut room, &mut buffer, DirtySections::ALL, None);
        let chat_lines = room.layout.visible_chat_lines.len();
        assert!(chat_lines > 0, "the full frame laid out chat lines");
        let chat_rect = room.layout.chat_rect;
        let room_list_rect = room.layout.room_list_rect;

        app.view.set_status("status only");
        gated_frame(
            &mut app,
            &mut room,
            &mut buffer,
            DirtySections::COMPOSE_BAR,
            None,
        );
        assert_eq!(
            room.layout.visible_chat_lines.len(),
            chat_lines,
            "a skipped chat kept its laid-out lines for mouse hits"
        );
        assert_eq!(room.layout.chat_rect, chat_rect);
        assert_eq!(room.layout.room_list_rect, room_list_rect);
    }

    #[test]
    fn room_height_change_escalates_to_full_repaint() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        push_room_message(&mut app, 1, UserId(1), 0, "hello world");
        let mut buffer = Buffer::new(80, 24);
        buffer.set_swap(extui::Swap::Retained);
        gated_frame(&mut app, &mut room, &mut buffer, DirtySections::ALL, None);

        // The lobby split moved without any dirty section being reported;
        // the geometry backstop must repaint everything.
        app.config.ui.room_height += 3;
        let emitted = gated_frame(&mut app, &mut room, &mut buffer, DirtySections::EMPTY, None);
        assert!(
            emitted > 100,
            "geometry change did not trigger a full repaint ({emitted} bytes)"
        );
    }

    /// Renders one gated frame and returns the emitted bytes, feeding them to
    /// `term`. `frame_retained` marks the frame as diffing against a retained
    /// seeded previous frame, as the client loop does from the second room
    /// frame onward.
    fn frame_bytes(
        app: &mut App,
        room: &mut RoomMode,
        buffer: &mut Buffer,
        dirty: DirtySections,
        frame_retained: bool,
        term: &mut vt100::Parser,
    ) -> Vec<u8> {
        app.view.sync_active(&app.room);
        {
            let mut cx = app.view_cx();
            cx.frame_retained = frame_retained;
            AppMode::render(room, &mut cx, buffer, 0, dirty);
        }
        app.drain_core_commands();
        buffer.render_internal();
        let bytes = buffer.write_buffer().to_vec();
        term.process(&bytes);
        buffer.buf.clear();
        bytes
    }

    /// The DECSTBM sequence [`queue_chat_scroll`] produces for the chat rows.
    fn chat_scroll_escape(room: &RoomMode) -> Vec<u8> {
        let chat = room.layout.chat_rect;
        format!("\x1b[{};{}r", chat.y + 1, chat.y + chat.h).into_bytes()
    }

    fn bytes_contain(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|window| window == needle)
    }

    /// Renders the app state fresh (full redraw into a new mode and buffer)
    /// and asserts `gated_term` matches it cell-for-cell. Blank cells compare
    /// by background only: an explicit space write, an \x1b[K erase, and a
    /// never-touched cell legitimately disagree on the invisible foreground
    /// and on whether the cell holds " " or "".
    fn assert_matches_full_redraw(app: &mut App, gated_term: &vt100::Parser) {
        let mut room = RoomMode::new();
        let mut buffer = Buffer::new(80, 24);
        let mut term = vt100::Parser::new(24, 80, 0);
        render_room_sections(app, &mut room, &mut buffer, DirtySections::ALL);
        buffer.render_internal();
        term.process(buffer.write_buffer());
        let visual = |cell: &vt100::Cell| {
            let text = cell.contents().to_owned();
            let (text, fg) = if text.trim().is_empty() {
                (String::new(), vt100::Color::Default)
            } else {
                (text, cell.fgcolor())
            };
            (text, fg, cell.bgcolor())
        };
        for row in 0..24 {
            for column in 0..80 {
                let gated_cell = gated_term.screen().cell(row, column).unwrap();
                let reference_cell = term.screen().cell(row, column).unwrap();
                assert_eq!(
                    visual(gated_cell),
                    visual(reference_cell),
                    "gated frame diverged from full redraw at ({column},{row}): gated row {:?} vs reference row {:?}",
                    gated_term.screen().contents_between(row, 0, row, 80),
                    term.screen().contents_between(row, 0, row, 80)
                );
            }
        }
    }

    /// Seeds a room whose chat overflows the viewport (alternating senders,
    /// so each message carries a heading row) and paints the first full frame.
    fn scrolled_room_fixture(
        app: &mut App,
        room: &mut RoomMode,
        buffer: &mut Buffer,
        term: &mut vt100::Parser,
    ) -> usize {
        for id in 1..=20u64 {
            push_room_message(
                app,
                100 + id,
                UserId(1 + id % 2),
                0,
                format!("message {id}"),
            );
        }
        buffer.set_swap(extui::Swap::Retained);
        frame_bytes(app, room, buffer, DirtySections::ALL, false, term).len()
    }

    #[test]
    fn chat_scroll_frame_emits_region_scroll_and_matches_full_redraw() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        let mut buffer = Buffer::new(80, 24);
        let mut term = vt100::Parser::new(24, 80, 0);
        let full = scrolled_room_fixture(&mut app, &mut room, &mut buffer, &mut term);

        app.view
            .active
            .chat
            .scroll_up(3, room.layout.chat_width, room.layout.chat_height);
        let bytes = frame_bytes(
            &mut app,
            &mut room,
            &mut buffer,
            DirtySections::CHAT,
            true,
            &mut term,
        );

        let escape = chat_scroll_escape(&room);
        assert!(
            bytes_contain(&bytes, &escape),
            "expected chat scroll region escape {:?} in frame",
            String::from_utf8_lossy(&escape)
        );
        assert!(
            bytes.len() < full,
            "scrolled frame ({}) should emit less than the full repaint ({full})",
            bytes.len()
        );
        assert_matches_full_redraw(&mut app, &term);
    }

    #[test]
    fn append_while_pinned_scrolls_chat_region() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        let mut buffer = Buffer::new(80, 24);
        let mut term = vt100::Parser::new(24, 80, 0);
        scrolled_room_fixture(&mut app, &mut room, &mut buffer, &mut term);

        push_room_message(&mut app, 200, UserId(1), 0, "fresh tail message");
        // Runtime events wake every section, so the follow-bottom scroll must
        // engage even on an all-dirty retained frame.
        let bytes = frame_bytes(
            &mut app,
            &mut room,
            &mut buffer,
            DirtySections::ALL,
            true,
            &mut term,
        );

        assert!(
            bytes_contain(&bytes, &chat_scroll_escape(&room)),
            "append while pinned did not scroll the chat region"
        );
        assert_matches_full_redraw(&mut app, &term);
    }

    #[test]
    fn chat_scroll_skips_region_without_retained_frame() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        let mut buffer = Buffer::new(80, 24);
        let mut term = vt100::Parser::new(24, 80, 0);
        scrolled_room_fixture(&mut app, &mut room, &mut buffer, &mut term);

        app.view
            .active
            .chat
            .scroll_up(3, room.layout.chat_width, room.layout.chat_height);
        let bytes = frame_bytes(
            &mut app,
            &mut room,
            &mut buffer,
            DirtySections::CHAT,
            false,
            &mut term,
        );

        assert!(
            !bytes_contain(&bytes, &chat_scroll_escape(&room)),
            "a non-retained frame must not emit a region scroll"
        );
        assert_matches_full_redraw(&mut app, &term);
    }

    #[test]
    fn history_prepend_skips_region_scroll() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        let mut buffer = Buffer::new(80, 24);
        let mut term = vt100::Parser::new(24, 80, 0);
        scrolled_room_fixture(&mut app, &mut room, &mut buffer, &mut term);

        app.view.active.chat.prepend_chat(vec![(
            ChatMessage {
                message_id: MessageId(1),
                room_id: RoomId(1),
                sender: UserId(9),
                sender_name: "user9".to_string(),
                timestamp_ms: 0,
                body: "older history".to_string(),
                file_transfer_id: None,
                flags: rpc::control::MessageFlags::default(),
                target: None,
            },
            false,
        )]);
        let bytes = frame_bytes(
            &mut app,
            &mut room,
            &mut buffer,
            DirtySections::CHAT,
            true,
            &mut term,
        );

        assert!(
            !bytes_contain(&bytes, &chat_scroll_escape(&room)),
            "a prepend shifts every row and must repaint, not scroll"
        );
        assert_matches_full_redraw(&mut app, &term);
    }

    #[test]
    fn chat_jump_to_top_skips_region_scroll() {
        let mut app = test_app();
        let mut room = RoomMode::new();
        let mut buffer = Buffer::new(80, 24);
        let mut term = vt100::Parser::new(24, 80, 0);
        scrolled_room_fixture(&mut app, &mut room, &mut buffer, &mut term);

        app.view
            .active
            .chat
            .top(room.layout.chat_width, room.layout.chat_height);
        let bytes = frame_bytes(
            &mut app,
            &mut room,
            &mut buffer,
            DirtySections::CHAT,
            true,
            &mut term,
        );

        assert!(
            !bytes_contain(&bytes, &chat_scroll_escape(&room)),
            "a whole-viewport jump must repaint, not scroll"
        );
        assert_matches_full_redraw(&mut app, &term);
    }

    fn push_room_message(
        app: &mut App,
        message_id: u64,
        sender: UserId,
        timestamp_ms: u64,
        body: impl Into<String>,
    ) {
        enter_room_one(app);
        app.room.chat_received(
            ChatMessage {
                message_id: MessageId(message_id),
                room_id: RoomId(1),
                sender,
                sender_name: format!("user{}", sender.0),
                timestamp_ms,
                body: body.into(),
                file_transfer_id: None,
                flags: rpc::control::MessageFlags::default(),
                target: None,
            },
            None,
        );
        app.view.sync_active(&app.room);
    }

    fn push_file_message(app: &mut App, message_id: u64, transfer_id: FileTransferId) {
        enter_room_one(app);
        app.room.chat_received(
            ChatMessage {
                message_id: MessageId(message_id),
                room_id: RoomId(1),
                sender: UserId(2),
                sender_name: "user2".to_string(),
                timestamp_ms: 1_000,
                body: "sent file `photo.png` (1.0 KiB)".to_string(),
                file_transfer_id: Some(transfer_id),
                flags: rpc::control::MessageFlags::default(),
                target: None,
            },
            None,
        );
        app.view.sync_active(&app.room);
    }

    #[test]
    fn terminal_transfer_shows_label_and_no_button() {
        let mut app = test_app();
        let transfer_id = FileTransferId(1);
        push_file_message(&mut app, 1, transfer_id);
        app.room.end_transfer(
            RoomId(1),
            transfer_id,
            TerminalVerb::Skipped,
            Some("Automatic file receive disabled".to_string()),
        );

        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 12);
        render_room(&mut app, &mut room, &mut buffer);

        let screen: String = (0..12).map(|row| row_text(&mut buffer, row)).collect();
        assert!(
            screen.contains("skipped: Automatic file receive disabled"),
            "terminal label missing: {screen}"
        );
        assert!(
            app.view.chrome.transfer_buttons.is_empty(),
            "a terminal transfer must not offer a cancel/skip button"
        );
    }

    fn participant(user_id: UserId, display_name: &str) -> rpc::control::UserSummary {
        rpc::control::UserSummary {
            user_id,
            display_name: display_name.to_string(),
            identifier: display_name.to_string(),
            online: true,
            connected_at_ms: 0,
            voice_status: ParticipantVoiceStatus::default(),
        }
    }

    #[test]
    fn insert_chord_prefix_and_failed_second_key_are_not_inserted() {
        let mut app = app_with_bindings("[bindings.insert]\n\"x x\" = \"ToggleMute\"\n");
        let mut room = RoomMode::default();

        room.process_input(&mut app, key('x'));
        assert_eq!(app.view.composer.text(), "");
        assert!(app.view.chrome.binding.pending_chord.is_some());

        room.process_input(&mut app, key('z'));
        assert_eq!(app.view.composer.text(), "");
        assert!(app.view.chrome.binding.pending_chord.is_none());
    }

    #[test]
    fn unmatched_insert_key_is_inserted_once() {
        let mut app = app_with_bindings("[bindings.insert]\n\"x x\" = \"ToggleMute\"\n");
        let mut room = RoomMode::default();

        room.process_input(&mut app, key('q'));

        assert_eq!(app.view.composer.text(), "q");
    }

    #[test]
    fn tab_completes_lobby_slash_command() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "/rep");
        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));

        assert_eq!(app.view.composer.text(), "/report-bug");
        assert_eq!(app.view.composer.mode(), EditorMode::Insert);
    }

    #[test]
    fn repeated_tab_cycles_lobby_slash_commands() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "/s");
        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.view.composer.text(), "/servers");

        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.view.composer.text(), "/settings");

        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.view.composer.text(), "/sound");
    }

    #[test]
    fn compose_normal_chord_prefix_is_not_sent_to_editor() {
        let mut app = app_with_bindings(
            "[ui]\ndefault-bindings = \"vim\"\n[bindings.compose-normal]\n\"z z\" = \"ToggleMute\"\n",
        );
        let mut room = RoomMode::default();
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);

        room.process_input(&mut app, key('z'));

        assert_eq!(app.view.composer.text(), "");
        assert!(app.view.chrome.binding.pending_chord.is_some());
    }

    #[test]
    fn escape_leaves_compose_focused_in_vim_normal_mode() {
        let mut app = test_app_vim();
        let mut room = RoomMode::default();

        assert!(matches!(
            room.process_input(&mut app, key('a')),
            Action::Continue
        ));
        assert!(matches!(
            room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            Action::Continue
        ));

        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);

        assert!(matches!(
            room.process_input(&mut app, key('i')),
            Action::Continue
        ));
        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.view.composer.mode(), EditorMode::Insert);
    }

    #[test]
    fn compose_vim_text_object_commands_receive_i_key() {
        let mut app = test_app_vim();
        let mut room = RoomMode::default();
        app.view.composer.set_lines("alpha beta");
        app.view.composer.set_cursor_offset(2);

        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);

        for key in ['c', 'i', 'w'] {
            room.process_input(
                &mut app,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::empty()),
            );
        }

        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.view.composer.mode(), EditorMode::Insert);
        assert_eq!(app.view.composer.text(), " beta");
    }

    #[test]
    fn shifted_jk_wraps_chat_panel_focus() {
        let mut app = test_app_vim();
        let mut room = RoomMode::default();
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::empty()),
        );
        assert_eq!(room.focus(), ChatPanelFocus::ChatLog);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::empty()),
        );
        assert_eq!(room.focus(), ChatPanelFocus::Lobby);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('K'), KeyModifiers::empty()),
        );
        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('J'), KeyModifiers::empty()),
        );
        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
    }

    #[test]
    fn super_jk_move_chat_panel_focus_from_compose_insert_mode() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        assert_eq!(app.view.composer.mode(), EditorMode::Insert);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SUPER),
        );
        assert_eq!(room.focus(), ChatPanelFocus::ChatLog);

        room.set_focus(&mut app, ChatPanelFocus::Compose);
        assert_eq!(app.view.composer.mode(), EditorMode::Insert);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::SUPER),
        );
        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
    }

    #[test]
    fn chat_log_jk_moves_cursor_by_body_line() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        for index in 0..3 {
            push_room_message(
                &mut app,
                index + 1,
                UserId(index as u64 + 1),
                index * 120_000,
                format!("message {index}"),
            );
        }

        room.set_focus(&mut app, ChatPanelFocus::ChatLog);
        let cursor_message =
            |app: &crate::app::App| app.view.active.chat.cursor().map(|c| c.message);
        assert_eq!(cursor_message(&app), Some(2));

        room.process_input(&mut app, key('k'));
        assert_eq!(cursor_message(&app), Some(1));

        room.process_input(&mut app, key('j'));
        assert_eq!(cursor_message(&app), Some(2));
    }

    #[test]
    fn slash_opens_history_search_from_chat_log_and_preserves_composer() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(1), 1_000, "a Boat afloat");
        push_room_message(&mut app, 2, UserId(2), 2_000, "unrelated");
        app.view.composer.set_lines("draft /command");

        room.process_input(&mut app, key('/'));
        assert!(room.history_search.is_some());
        for ch in ['b', ' ', 't'] {
            room.process_input(&mut app, key(ch));
        }

        let search = room.history_search.as_ref().expect("search open");
        assert_eq!(search.query(), "b t");
        assert_eq!(search.matches().len(), 1);
        assert_eq!(search.selected_message(), Some(0));
        assert_eq!(
            app.view.active.chat.cursor().map(|cursor| cursor.message),
            Some(0)
        );
        assert_eq!(app.view.composer.text(), "draft /command");

        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(room.history_search.is_none());
        assert_eq!(app.view.composer.text(), "draft /command");
    }

    #[test]
    fn history_search_navigation_always_tracks_a_selected_result() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(1), 1_000, "first needle");
        push_room_message(&mut app, 2, UserId(2), 2_000, "skip");
        push_room_message(&mut app, 3, UserId(3), 3_000, "last NEEDLE");

        room.process_input(&mut app, key('/'));
        for ch in "needle".chars() {
            room.process_input(&mut app, key(ch));
        }
        assert_eq!(
            app.view.active.chat.cursor().map(|cursor| cursor.message),
            Some(2),
            "the nearest result starts selected"
        );

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(
            app.view.active.chat.cursor().map(|cursor| cursor.message),
            Some(0),
            "Ctrl-K selects and follows the previous result"
        );
        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_eq!(
            app.view.active.chat.cursor().map(|cursor| cursor.message),
            Some(2)
        );

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert!(room.history_search.is_none());
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        assert!(
            row_text(&mut buffer, room.layout.chat_log_bar_rect.y).contains("/needle"),
            "the closed search remains visible in the Chat Log bar"
        );
        room.process_input(&mut app, key('n'));
        assert_eq!(
            app.view.active.chat.cursor().map(|cursor| cursor.message),
            Some(0),
            "n repeats the closed search and wraps to its next result"
        );
        room.process_input(&mut app, key('N'));
        assert_eq!(
            app.view.active.chat.cursor().map(|cursor| cursor.message),
            Some(2)
        );
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(room.last_history_search.is_none());
    }

    #[test]
    fn chat_log_gg_and_g_move_cursor_to_edges() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        for index in 0..20 {
            push_room_message(
                &mut app,
                index + 1,
                UserId(index as u64 + 1),
                index * 120_000,
                format!("message {index}"),
            );
        }

        let mut buffer = Buffer::new(80, 12);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);

        room.process_input(&mut app, key('g'));
        room.process_input(&mut app, key('g'));
        assert_eq!(
            app.view.active.chat.cursor(),
            Some(ChatCursor {
                message: 0,
                line: 0
            })
        );
        assert!(app.view.active.chat.scroll_offset() > 0);

        room.process_input(&mut app, key('G'));
        assert_eq!(
            app.view.active.chat.cursor(),
            Some(ChatCursor {
                message: 19,
                line: 0,
            })
        );
        assert_eq!(app.view.active.chat.scroll_offset(), 0);
    }

    #[test]
    fn chat_log_cursor_move_scrolls_cursor_into_view() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        for index in 0..30 {
            push_room_message(
                &mut app,
                index + 1,
                UserId(2),
                1_000 + index * 1_000,
                format!("message {index}"),
            );
        }

        let mut buffer = Buffer::new(80, 14);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);
        // Scroll the viewport away, leaving the cursor on the newest message.
        app.view
            .active
            .chat
            .top(room.layout().chat_width, room.layout().chat_height);
        room.process_input(&mut app, key('k'));
        render_room(&mut app, &mut room, &mut buffer);

        let cursor = app.view.active.chat.cursor().expect("cursor present");
        assert_eq!(cursor.message, 28);
        assert!(
            room.layout()
                .visible_chat_lines
                .iter()
                .any(|line| line.kind == LineKind::Body
                    && line.message == cursor.message
                    && line.line == cursor.line),
            "cursor line must be visible after movement"
        );
    }

    #[test]
    fn tab_toggles_selected_chat_log_collapse() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(
            &mut app,
            1,
            UserId(2),
            1,
            "```\n0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n```",
        );

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);
        assert!(app.view.active.chat.is_collapsed(0));

        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert!(app.view.active.chat.is_expanded(0));
    }

    #[test]
    fn mouse_collapse_reclamps_scroll_and_fills_viewport() {
        // Regression: collapsing an expanded message via mouse must re-clamp the
        // scroll offset so the top-anchored chat log re-fills the viewport
        // instead of leaving blank rows where the expanded body used to be.
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);

        // A lone collapsible message (over COLLAPSE_LIMIT wrapped lines) at the
        // top, followed by enough short messages to keep the buffer taller than
        // the viewport once the long message collapses.
        let long_body = format!(
            "```\n{}\n```",
            (0..17).fold(String::new(), |mut acc, n| {
                use std::fmt::Write;
                let _ = writeln!(acc, "line {n}");
                acc
            })
        );
        push_room_message(&mut app, 1, UserId(2), 1_000, long_body);
        for index in 0..20u64 {
            push_room_message(
                &mut app,
                index + 2,
                UserId(2),
                2_000 + index * 1_000,
                "short",
            );
        }

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);

        let width = room.layout().chat_width;
        let height = room.layout().chat_height;
        assert!(
            app.view.active.chat.is_collapsed(0),
            "long message starts collapsed"
        );

        // Expand the long message and scroll to the very top so its heading sits
        // at the top of the viewport. Collapsing now removes fewer rows than the
        // viewport height, which lands the scroll offset in the window where
        // `visible_lines` does not self-correct.
        app.view.active.chat.toggle_expand(0, width);
        assert!(app.view.active.chat.is_expanded(0));
        app.view.active.chat.top(width, height);
        render_room(&mut app, &mut room, &mut buffer);

        let heading_row = room
            .layout()
            .visible_chat_lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.kind == LineKind::Heading && line.block_contains(0))
            .map(|(row_index, _)| room.layout().chat_rect.y + row_index as u16)
            .expect("expanded message heading visible at top");

        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: room.layout().chat_rect.x + 1,
                row: heading_row,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert!(
            app.view.active.chat.is_collapsed(0),
            "clicking the heading collapses it"
        );

        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(
            room.layout().visible_chat_lines.len(),
            height as usize,
            "viewport must stay full after collapse, with no trailing blank rows",
        );
    }

    #[test]
    fn y_copies_visual_selection_and_exits_visual() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        for (index, body) in ["first", "second"].into_iter().enumerate() {
            push_room_message(
                &mut app,
                index as u64 + 1,
                UserId(2),
                1_000 + index as u64 * 1_000,
                body,
            );
        }

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);
        // A mouse-style drag over both messages produces the visual range.
        app.view.active.chat.begin_drag(ChatCursor {
            message: 0,
            line: 0,
        });
        app.view.active.chat.drag_to(ChatCursor {
            message: 1,
            line: 0,
        });
        app.view.active.chat.end_drag();
        room.process_input(&mut app, key('y'));

        assert_eq!(
            app.view.take_pending_clipboard().as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(app.view.status.text(), "copied to clipboard");
        assert_eq!(app.view.status.kind(), crate::app::StatusKind::Info);
        assert!(!app.view.active.chat.has_visual(), "yank exits visual mode");
    }

    #[test]
    fn y_without_visual_selection_copies_nothing() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(2), 1_000, "first");

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);
        room.process_input(&mut app, key('y'));

        assert_eq!(app.view.take_pending_clipboard(), None);
    }

    #[test]
    fn v_then_jk_extends_visual_selection_and_y_copies_it() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        for (index, body) in ["first", "second", "third"].into_iter().enumerate() {
            push_room_message(
                &mut app,
                index as u64 + 1,
                UserId(2),
                1_000 + index as u64 * 1_000,
                body,
            );
        }

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);

        room.process_input(&mut app, key('v'));
        assert!(app.view.active.chat.has_visual());
        room.process_input(&mut app, key('k'));
        room.process_input(&mut app, key('k'));
        room.process_input(&mut app, key('y'));

        assert_eq!(
            app.view.take_pending_clipboard().as_deref(),
            Some("first\nsecond\nthird")
        );
        assert!(!app.view.active.chat.has_visual(), "yank exits visual");
    }

    #[test]
    fn esc_clears_visual_anchor() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(2), 1_000, "first");

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);

        room.process_input(&mut app, key('v'));
        assert!(app.view.active.chat.has_visual());
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(!app.view.active.chat.has_visual());
    }

    #[test]
    fn paragraph_keys_jump_between_sender_blocks() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(2), 1_000, "alice one");
        push_room_message(&mut app, 2, UserId(2), 2_000, "alice two");
        push_room_message(&mut app, 3, UserId(3), 3_000, "bob one");

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);

        room.process_input(&mut app, key('{'));
        assert_eq!(
            app.view.active.chat.cursor(),
            Some(ChatCursor {
                message: 0,
                line: 0
            }),
            "previous block start"
        );
        room.process_input(&mut app, key('}'));
        assert_eq!(
            app.view.active.chat.cursor(),
            Some(ChatCursor {
                message: 2,
                line: 0
            }),
            "next block start"
        );
    }

    #[test]
    fn yy_copies_cursor_line() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(2), 1_000, "alpha\nbeta");

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);

        room.process_input(&mut app, key('y'));
        room.process_input(&mut app, key('y'));

        assert_eq!(app.view.take_pending_clipboard().as_deref(), Some("beta"));
    }

    #[test]
    fn ym_copies_cursor_message_body() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(2), 1_000, "alpha\nbeta");

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);

        room.process_input(&mut app, key('y'));
        room.process_input(&mut app, key('m'));

        assert_eq!(
            app.view.take_pending_clipboard().as_deref(),
            Some("alpha\nbeta")
        );
    }

    #[test]
    fn copy_ref_targets_cursor_message_inside_a_block() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(2), 1_000, "block head");
        push_room_message(&mut app, 2, UserId(2), 2_000, "second in block");

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.set_focus(&mut app, ChatPanelFocus::ChatLog);
        // The default cursor sits on the newest message, mid-block.
        room.process_input(&mut app, key('Y'));

        let expected = rpc::msgref::MessageRef {
            room_id: RoomId(1),
            message_id: MessageId(2),
        }
        .encode();
        assert_eq!(
            app.view.take_pending_clipboard(),
            Some(format!("@@{expected}"))
        );
    }

    #[test]
    fn visual_layer_is_active_only_with_anchor() {
        let mut app = test_app();
        let room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        push_room_message(&mut app, 1, UserId(2), 1_000, "first");

        assert_eq!(room.workspace_layer(&app), bindings::WORKSPACE_LAYER);
        app.view.active.chat.ensure_cursor(40);
        app.view.active.chat.toggle_visual_anchor(40);
        assert_eq!(room.workspace_layer(&app), bindings::CHAT_VISUAL_LAYER);
        app.view.active.chat.clear_visual_anchor();
        assert_eq!(room.workspace_layer(&app), bindings::WORKSPACE_LAYER);
    }

    /// Guards the router hazard where a terminal binding and a chord prefix
    /// share a key: workspace `y` must open the yank chord while the inherited
    /// chat-visual override resolves `y` straight to CopySelection.
    #[test]
    fn y_is_a_chord_in_workspace_and_a_yank_in_visual() {
        let mut app = test_app();
        match resolve_binding(&mut app, bindings::WORKSPACE_LAYER, key('y')) {
            BindingResolution::Consumed => {}
            other => panic!("expected chord entry, got {other:?}"),
        }
        match resolve_binding(&mut app, bindings::WORKSPACE_LAYER, key('m')) {
            BindingResolution::Action(BindCommand::CopyMessage) => {}
            other => panic!("expected CopyMessage, got {other:?}"),
        }
        match resolve_binding(&mut app, bindings::CHAT_VISUAL_LAYER, key('y')) {
            BindingResolution::Action(BindCommand::CopySelection) => {}
            other => panic!("expected CopySelection, got {other:?}"),
        }
        match resolve_binding(&mut app, bindings::CHAT_VISUAL_LAYER, key('j')) {
            BindingResolution::Action(BindCommand::ScrollDown) => {}
            other => panic!("expected inherited motion, got {other:?}"),
        }
    }

    #[test]
    fn d_deletes_from_workspace_and_visual_layers() {
        let mut app = test_app();
        for layer in [bindings::WORKSPACE_LAYER, bindings::CHAT_VISUAL_LAYER] {
            match resolve_binding(&mut app, layer, key('d')) {
                BindingResolution::Action(BindCommand::DeleteMessage) => {}
                other => panic!("expected DeleteMessage, got {other:?}"),
            }
        }
    }

    #[test]
    fn mouse_down_on_chat_text_focuses_chat_log_and_moves_cursor() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        push_room_message(&mut app, 1, UserId(2), 1, "hello");

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let layout = room.layout();
        let (row_index, line) = layout
            .visible_chat_lines
            .iter()
            .copied()
            .enumerate()
            .find(|(_, line)| line.kind == LineKind::Body)
            .expect("body line rendered");
        let column = layout.chat_rect.x + 2;
        let row = layout.chat_rect.y + row_index as u16;

        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert_eq!(room.focus(), ChatPanelFocus::ChatLog);
        assert_eq!(
            app.view.active.chat.cursor(),
            Some(ChatCursor {
                message: line.message,
                line: line.line,
            })
        );
        assert!(app.view.active.chat.is_dragging());

        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert!(
            !app.view.active.chat.has_visual(),
            "a click is a cursor move, not a selection"
        );
    }

    #[test]
    fn mouse_down_on_lobby_row_focuses_lobby_and_selects_user() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.user_id = Some(UserId(1));
        app.room.authenticated(
            &[rpc::control::RoomInfo {
                room_id: RoomId(1),
                name: "lobby".to_string(),
                kind: rpc::control::RoomKind::Public,
                head: None,
                voice_users: vec![UserId(1), UserId(2)],
            }],
            vec![
                participant(UserId(1), "alice"),
                participant(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            Some(UserId(1)),
        );

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let column = room.layout().user_list_rect.x + 1;
        let row = room.layout().user_list_rect.y;
        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Users);
        assert_eq!(app.room.participants.selected_user, Some(UserId(1)));
    }

    #[test]
    fn drag_lobby_bar_trades_rows_with_chat_log() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let bar_row = room.layout().lobby_bar_rect.y;
        let column = room.layout().lobby_bar_rect.x;
        let start_height = app.config.ui.room_height;
        let budget = start_height + room.layout().chat_rect.h;

        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, bar_row),
        );
        // Dragging one row down grows the rooms/users block by one row.
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), column, bar_row + 1),
        );
        assert_eq!(app.config.ui.room_height, start_height + 1);

        // Dragging far down keeps the chat log at one row and never past it.
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), column, 1000),
        );
        assert_eq!(app.config.ui.room_height, budget - 1);
        render_room(&mut app, &mut room, &mut buffer);
        assert!(room.layout().chat_rect.h >= 1);

        // Dragging back above the anchor shrinks it again.
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                bar_row.saturating_sub(2),
            ),
        );
        assert_eq!(app.config.ui.room_height, start_height.saturating_sub(2));

        // Dragging all the way up never shrinks the lobby below a single row.
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), column, 0),
        );
        assert_eq!(app.config.ui.room_height, 1);

        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), column, bar_row),
        );
    }

    #[test]
    fn drag_lobby_bar_uses_rendered_height_when_config_is_too_large() {
        let mut app = test_app();
        app.config.ui.room_height = 100;
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 12);
        render_room(&mut app, &mut room, &mut buffer);

        let bar_row = room.layout().lobby_bar_rect.y;
        let column = room.layout().lobby_bar_rect.x;
        let rendered_lobby_height = room.layout().room_list_rect.h;
        let budget = rendered_lobby_height + room.layout().chat_rect.h;
        assert!(rendered_lobby_height < app.config.ui.room_height);
        assert!(room.layout().chat_rect.h >= 1);

        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, bar_row),
        );
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), column, 1000),
        );

        assert_eq!(app.config.ui.room_height, budget - 1);
        render_room(&mut app, &mut room, &mut buffer);
        assert!(room.layout().chat_rect.h >= 1);
    }

    #[test]
    fn drag_chat_log_bar_sets_composer_height_that_survives_send() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let bar_row = room.layout().chat_log_bar_rect.y;
        let column = room.layout().chat_log_bar_rect.x;
        // Drag up past `max_composer_height` (default 6) to prove the override
        // may exceed the content cap.
        let target = 9u16;
        assert!(target > app.config.ui.max_composer_height);

        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, bar_row),
        );
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                bar_row - (target - 1),
            ),
        );
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                column,
                bar_row - (target - 1),
            ),
        );

        assert_eq!(app.view.composer_rows, Some(target));
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(room.layout().composer_rect.h, target);

        // The chosen height persists once the message clears on send.
        type_text(&mut room, &mut app, "hello");
        app.view.submit_composer();
        assert!(app.view.composer.text().is_empty());
        assert_eq!(app.view.composer_rows, Some(target));
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(room.layout().composer_rect.h, target);
    }

    #[test]
    fn dragging_scrolled_composer_sets_exact_viewport_height() {
        let mut app = test_app();
        app.config.ui.max_composer_height = 2;
        app.view.composer.set_lines("one\ntwo\nthree\nfour\nfive");
        let end = app.view.composer.text_len();
        app.view.composer.set_cursor_offset(end);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(room.layout().composer_rect.h, 2);

        let bar_row = room.layout().chat_log_bar_rect.y;
        let column = room.layout().chat_log_bar_rect.x;
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, bar_row),
        );
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), column, bar_row - 1),
        );
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), column, bar_row - 1),
        );

        assert_eq!(app.view.composer_rows, Some(3));
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(room.layout().composer_rect.h, 3);
    }

    #[test]
    fn clicking_composer_border_focuses_and_places_cursor_at_nearest_text() {
        let mut app = test_app();
        app.view.composer.set_lines("alpha\nbravo");
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(40, 20);
        render_room(&mut app, &mut room, &mut buffer);

        let editor = room.layout().composer_rect;
        let frame = room.layout().composer_frame_rect;
        assert_eq!(frame.h, editor.h + 2);
        assert_eq!(editor.x, frame.x + 1);
        assert_eq!(editor.w, frame.w - 2);

        let bottom_border = frame.y + frame.h - 1;
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                editor.x + 2,
                bottom_border,
            ),
        );

        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.view.composer.mode(), EditorMode::Insert);
        assert_eq!(app.view.composer.cursor_offset(), 8);
    }

    #[test]
    fn padded_composer_uses_transparent_full_width_borders_and_gutters() {
        let mut app = test_app();
        app.view.theme.panel = app.view.theme.panel.with_bg_rgb(0xaa, 0xbb, 0xcc);
        app.view.theme.composer_border =
            app.view.theme.composer_border.with_bg_rgb(0x11, 0x22, 0x33);
        let mut room = RoomMode::default();
        let width = 40u16;
        let mut buffer = Buffer::new(width, 20);
        render_room(&mut app, &mut room, &mut buffer);

        let editor = room.layout().composer_rect;
        let frame = room.layout().composer_frame_rect;
        let top_y = frame.y;
        let bottom_y = frame.y + frame.h - 1;
        let cell = |grid: &extui::Grid, x: u16, y: u16| {
            grid.cells()[(usize::from(y) * usize::from(width)) + usize::from(x)]
        };
        let grid = buffer.current();

        for x in [frame.x, frame.x + frame.w - 1] {
            let top = cell(grid, x, top_y);
            let bottom = cell(grid, x, bottom_y);
            assert_eq!(top.text_inline(), Some("▀"));
            assert_eq!(bottom.text_inline(), Some("▄"));
            assert_eq!(top.style().bg(), None);
            assert_eq!(bottom.style().bg(), None);
        }

        let left_gutter = cell(grid, frame.x, editor.y);
        assert_eq!(left_gutter.text_inline(), Some(" "));
        assert_eq!(left_gutter.style().bg(), None);

        let placeholder = cell(grid, editor.x, editor.y);
        assert_eq!(placeholder.text_inline(), Some("M"));
        assert_eq!(placeholder.style().fg(), app.view.theme.subtle.fg());
        assert_eq!(placeholder.style().bg(), None);
    }

    #[test]
    fn padded_overflow_scrollbar_uses_right_gutter_and_border_rows() {
        let mut app = test_app();
        app.config.ui.max_composer_height = 2;
        app.view.composer.set_lines("one\ntwo\nthree\nfour");
        let mut room = RoomMode::default();
        let width = 40u16;
        let mut buffer = Buffer::new(width, 20);

        render_room(&mut app, &mut room, &mut buffer);

        let editor = room.layout().composer_rect;
        let frame = room.layout().composer_frame_rect;
        assert_eq!(editor.w, frame.w - 2);
        assert_eq!(editor.h, 2);
        let scrollbar_x = frame.x + frame.w - 1;
        let cell = |grid: &extui::Grid, x: u16, y: u16| {
            grid.cells()[(usize::from(y) * usize::from(width)) + usize::from(x)]
        };
        let grid = buffer.current();
        assert_ne!(cell(grid, scrollbar_x, frame.y).text_inline(), Some("▀"));
        assert_eq!(
            cell(grid, scrollbar_x, frame.y).style().bg(),
            app.view.theme.scrollbar.bg()
        );
        assert_eq!(
            cell(grid, scrollbar_x, frame.y + frame.h - 1).text_inline(),
            Some(" ")
        );

        let end = app.view.composer.text_len();
        app.view.composer.set_cursor_offset(end);
        render_room(&mut app, &mut room, &mut buffer);
        let grid = buffer.current();
        assert_eq!(cell(grid, scrollbar_x, frame.y).text_inline(), Some(" "));
        assert_ne!(
            cell(grid, scrollbar_x, frame.y + frame.h - 1).text_inline(),
            Some("▄")
        );
    }

    #[test]
    fn clicking_composer_scrollbar_track_jumps_to_that_end() {
        let mut app = test_app();
        app.config.ui.max_composer_height = 2;
        app.view
            .composer
            .set_lines("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight");
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(40, 20);
        render_room(&mut app, &mut room, &mut buffer);

        let scrollbar = room.layout().composer_scrollbar.unwrap();
        assert_eq!(scrollbar.state.offset, 0);
        let x = scrollbar.rect.x;
        let bottom = scrollbar.rect.y + scrollbar.rect.h - 1;
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), x, bottom),
        );
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), x, bottom),
        );
        render_room(&mut app, &mut room, &mut buffer);

        let scrollbar = room.layout().composer_scrollbar.unwrap();
        assert_eq!(
            scrollbar.state.offset,
            scrollbar.state.total - scrollbar.state.viewport
        );
    }

    #[test]
    fn clicking_large_composer_scrollbar_maps_across_full_buffer() {
        let mut app = test_app();
        app.config.ui.max_composer_height = 6;
        let text = (0..2000)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.view.composer.set_lines(&text);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let scrollbar = room.layout().composer_scrollbar.unwrap();
        let middle = scrollbar.rect.y + scrollbar.rect.h / 2;
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                scrollbar.rect.x - 1,
                middle,
            ),
        );
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                scrollbar.rect.x - 1,
                middle,
            ),
        );
        render_room(&mut app, &mut room, &mut buffer);

        let scrollbar = room.layout().composer_scrollbar.unwrap();
        assert!(
            scrollbar.state.offset > 500,
            "offset was {}",
            scrollbar.state.offset
        );
        assert!(
            scrollbar.state.offset < 1500,
            "offset was {}",
            scrollbar.state.offset
        );
    }

    #[test]
    fn dragging_composer_scrollbar_thumb_scrolls_to_bottom() {
        let mut app = test_app();
        app.config.ui.max_composer_height = 2;
        app.view
            .composer
            .set_lines("one\ntwo\nthree\nfour\nfive\nsix\nseven\neight");
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(40, 20);
        render_room(&mut app, &mut room, &mut buffer);

        let scrollbar = room.layout().composer_scrollbar.unwrap();
        let x = scrollbar.rect.x;
        let top = scrollbar.rect.y;
        let bottom = scrollbar.rect.y + scrollbar.rect.h - 1;
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), x, top),
        );
        assert_eq!(room.scrollbar_drag.unwrap().id, ScrollbarId::Compose);
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), x, bottom),
        );
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), x, bottom),
        );
        assert!(room.scrollbar_drag.is_none());
        render_room(&mut app, &mut room, &mut buffer);

        let scrollbar = room.layout().composer_scrollbar.unwrap();
        assert_eq!(
            scrollbar.state.offset,
            scrollbar.state.total - scrollbar.state.viewport
        );
    }

    #[test]
    fn borderless_overflow_reserves_one_column_only_while_needed() {
        let mut app = test_app();
        app.config.ui.composer_padding = false;
        app.config.ui.max_composer_height = 2;
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(40, 20);

        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(room.layout().composer_rect.w, 40);

        app.view.composer.set_lines("one\ntwo\nthree");
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(room.layout().composer_frame_rect.w, 40);
        assert_eq!(room.layout().composer_rect.w, 39);
        assert_eq!(room.layout().composer_rect.h, 2);
    }

    #[test]
    fn composer_height_is_clamped_by_current_terminal_height() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let mut tall = Buffer::new(80, 30);
        render_room(&mut app, &mut room, &mut tall);

        let bar_row = room.layout().chat_log_bar_rect.y;
        let column = room.layout().chat_log_bar_rect.x;
        let target = 12u16;
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, bar_row),
        );
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                bar_row - (target - 1),
            ),
        );
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                column,
                bar_row - (target - 1),
            ),
        );
        assert_eq!(app.view.composer_rows, Some(target));

        let mut short = Buffer::new(80, 12);
        render_room(&mut app, &mut room, &mut short);

        assert!(room.layout().composer_rect.h < target);
        assert_eq!(room.layout().chat_log_bar_rect.h, 1);
        assert!(room.layout().room_list_rect.h >= 1);
        assert_eq!(room.layout().lobby_bar_rect.h, 1);
        assert!(room.layout().chat_rect.h >= 1);
    }

    #[test]
    fn overlarge_resize_preserves_lobby_row_bar_and_chat_row() {
        let mut app = test_app();
        app.config.ui.room_height = 100;
        app.view.composer_rows = Some(100);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 12);

        render_room(&mut app, &mut room, &mut buffer);

        assert_eq!(room.layout().room_list_rect.h, 1);
        assert_eq!(room.layout().lobby_bar_rect.h, 1);
        assert_eq!(room.layout().chat_rect.h, 1);
        assert_eq!(room.layout().chat_log_bar_rect.h, 1);
    }

    #[test]
    fn manual_composer_height_caps_taller_content() {
        let mut app = test_app();
        app.config.ui.max_composer_height = 2;
        app.view.composer_rows = Some(3);
        app.view.composer.set_lines("a\nb\nc\nd\ne");
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);

        render_room(&mut app, &mut room, &mut buffer);

        assert_eq!(room.layout().composer_rect.h, 3);
    }

    #[test]
    fn click_on_lobby_bar_without_drag_keeps_sizes() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let bar_row = room.layout().lobby_bar_rect.y;
        let column = room.layout().lobby_bar_rect.x;
        let start_height = app.config.ui.room_height;

        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), column, bar_row),
        );
        room.process_mouse(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), column, bar_row),
        );

        assert_eq!(app.config.ui.room_height, start_height);
        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
    }

    #[test]
    fn lobby_layout_splits_rooms_and_users_full_height() {
        let mut app = test_app();
        app.config.ui.room_height = 4;
        enter_rooms(&mut app, &[1, 2, 3]);
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);
        let mut buffer = Buffer::new(90, 24);

        render_room(&mut app, &mut room, &mut buffer);
        let layout = room.layout();

        assert_eq!(
            layout.room_list_rect,
            Rect {
                x: 0,
                y: 1,
                w: 30,
                h: 4,
            }
        );
        assert_eq!(
            layout.lobby_divider_rect,
            Rect {
                x: 30,
                y: 1,
                w: 1,
                h: 4,
            }
        );
        assert_eq!(
            layout.user_list_rect,
            Rect {
                x: 31,
                y: 1,
                w: 59,
                h: 4,
            }
        );
        assert_eq!(layout.lobby_bar_rect.y, 5);
        assert_eq!(
            cell_text(
                &mut buffer,
                layout.room_list_rect.x + 1,
                layout.lobby_bar_rect.y
            ),
            "R",
        );
        assert_eq!(
            cell_text(
                &mut buffer,
                layout.user_list_rect.x + 1,
                layout.lobby_bar_rect.y
            ),
            "C",
        );
        assert_eq!(layout.room_hits.len(), 3);
        assert!(
            layout
                .room_hits
                .iter()
                .all(|(rect, _)| rect.x == 0 && rect.w == layout.room_list_rect.w && rect.h == 1)
        );
        assert_eq!(
            cell_style(
                &mut buffer,
                layout.lobby_divider_rect.x,
                layout.lobby_divider_rect.y
            ),
            app.view.theme.status_fill,
        );
        assert!(layout.rooms_scrollbar.is_none());
    }

    #[test]
    fn overflowing_rooms_use_divider_scrollbar_and_offset_aligned_hits() {
        let mut app = test_app();
        app.config.ui.room_height = 3;
        enter_rooms(&mut app, &[1, 2, 3, 4, 5, 6, 7, 8]);
        app.view.rooms_offset = 3;
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);
        let mut buffer = Buffer::new(80, 24);
        room.process_input(&mut app, key('h'));

        render_room(&mut app, &mut room, &mut buffer);
        let scrollbar = room.layout().rooms_scrollbar.unwrap();
        assert_eq!(scrollbar.id, ScrollbarId::Rooms);
        assert_eq!(scrollbar.state.offset, 3);
        assert_eq!(scrollbar.state.viewport, 3);
        assert_eq!(room.layout().room_hits[0].1, RoomId(4));
        assert_eq!(room.layout().room_hits[2].1, RoomId(6));
        assert_eq!(
            cell_style(&mut buffer, scrollbar.rect.x, scrollbar.rect.y).bg(),
            app.view.theme.scrollbar.bg()
        );
    }

    #[test]
    fn overflowing_rooms_hide_thumb_until_rooms_view_is_focused() {
        let mut app = test_app();
        app.config.ui.room_height = 3;
        enter_rooms(&mut app, &[1, 2, 3, 4, 5]);
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);
        let mut buffer = Buffer::new(80, 24);

        render_room(&mut app, &mut room, &mut buffer);
        let divider = room.layout().lobby_divider_rect;
        assert!(room.layout().rooms_scrollbar.is_some());
        assert_eq!(
            cell_style(&mut buffer, divider.x, divider.y),
            app.view.theme.status_fill
        );

        room.process_input(&mut app, key('h'));
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Rooms);
        assert_ne!(cell_text(&mut buffer, divider.x, divider.y), " ");
    }

    #[test]
    fn passive_rooms_divider_cannot_start_scrollbar_drag() {
        let mut app = test_app();
        app.config.ui.room_height = 3;
        enter_rooms(&mut app, &[1, 2, 3]);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        assert!(room.layout().rooms_scrollbar.is_none());
        let divider = room.layout().lobby_divider_rect;
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                divider.x,
                divider.y,
            ),
        );
        assert!(room.scrollbar_drag.is_none());
    }

    #[test]
    fn rooms_keyboard_selection_scrolls_only_enough_to_remain_visible() {
        let mut app = test_app();
        app.config.ui.room_height = 3;
        enter_rooms(&mut app, &[1, 2, 3, 4, 5]);
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        room.process_input(&mut app, key('h'));

        room.process_input(&mut app, key('j'));
        room.process_input(&mut app, key('j'));
        assert_eq!(app.view.rooms_offset, 0);
        room.process_input(&mut app, key('j'));
        assert_eq!(app.room.viewed_room, Some(RoomId(4)));
        assert_eq!(app.view.rooms_offset, 1);
        room.process_input(&mut app, key('k'));
        assert_eq!(app.view.rooms_offset, 1);
    }

    #[test]
    fn rooms_track_click_and_thumb_drag_map_across_complete_list() {
        let mut app = test_app();
        app.config.ui.room_height = 4;
        enter_rooms(&mut app, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let scrollbar = room.layout().rooms_scrollbar.unwrap();
        let bottom = scrollbar.rect.y + scrollbar.rect.h - 1;
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                scrollbar.rect.x,
                bottom,
            ),
        );
        assert_eq!(app.view.rooms_offset, 8);
        assert_eq!(room.scrollbar_drag.unwrap().id, ScrollbarId::Rooms);
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Up(MouseButton::Left),
                scrollbar.rect.x,
                bottom,
            ),
        );

        render_room(&mut app, &mut room, &mut buffer);
        let scrollbar = room.layout().rooms_scrollbar.unwrap();
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Down(MouseButton::Left),
                scrollbar.rect.x,
                bottom,
            ),
        );
        assert_eq!(app.view.rooms_offset, 8, "thumb press must not jump");
        room.process_mouse(
            &mut app,
            mouse(
                MouseEventKind::Drag(MouseButton::Left),
                scrollbar.rect.x,
                scrollbar.rect.y,
            ),
        );
        assert_eq!(app.view.rooms_offset, 0);
    }

    #[test]
    fn rooms_offset_clamps_after_resize_and_catalog_mutation() {
        let mut app = test_app();
        app.config.ui.room_height = 3;
        enter_rooms(&mut app, &[1, 2, 3, 4, 5, 6, 7, 8]);
        app.view.rooms_offset = 5;
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(app.view.rooms_offset, 5);

        app.config.ui.room_height = 6;
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(app.view.rooms_offset, 2);

        enter_rooms(&mut app, &[1, 2, 3, 4]);
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(app.view.rooms_offset, 0);
        assert!(room.layout().rooms_scrollbar.is_none());

        app.view.rooms_offset = 3;
        app.room
            .authenticated(&[], Vec::new(), RoomId(99), None, None);
        render_room(&mut app, &mut room, &mut buffer);
        assert_eq!(app.view.rooms_offset, 0);
        assert!(room.layout().room_hits.is_empty());
    }

    #[test]
    fn lobby_room_column_caps_and_narrow_layout_omits_divider() {
        let mut app = test_app();
        app.config.ui.room_height = 3;
        enter_rooms(&mut app, &[1, 2]);
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);
        let mut wide = Buffer::new(180, 18);

        render_room(&mut app, &mut room, &mut wide);
        assert_eq!(room.layout().room_list_rect.w, 50);
        assert_eq!(room.layout().lobby_divider_rect.w, 1);
        assert_eq!(room.layout().user_list_rect.w, 129);

        let mut narrow = Buffer::new(2, 12);
        render_room(&mut app, &mut room, &mut narrow);
        assert_eq!(room.layout().room_list_rect.w, 1);
        assert_eq!(room.layout().lobby_divider_rect, Rect::EMPTY);
        assert_eq!(room.layout().user_list_rect.w, 1);
    }

    #[test]
    fn lobby_h_l_switch_subfocus_and_jk_route_to_active_list() {
        let mut app = test_app();
        enter_rooms(&mut app, &[1, 2, 3]);
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);

        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Users);

        room.process_input(&mut app, key('h'));
        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Rooms);
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));

        room.process_input(&mut app, key('j'));
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Rooms);
        assert_eq!(app.room.viewed_room, Some(RoomId(2)));

        room.process_input(&mut app, key('k'));
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));

        room.process_input(&mut app, key('l'));
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Users);
        room.process_input(&mut app, key('j'));
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
        assert_eq!(app.view.status.text(), "no users in the current room yet");
    }

    #[test]
    fn workspace_deafen_uses_ctrl_h_after_h_moves_to_rooms() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);

        room.process_input(&mut app, key('h'));
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Rooms);
        assert!(!app.deafened.load(Ordering::Relaxed));

        room.process_input(&mut app, ctrl('h'));
        assert!(app.deafened.load(Ordering::Relaxed));
    }

    #[test]
    fn top_bar_uses_fixed_voice_buttons_without_compact_status() {
        let mut app = test_app();
        app.voice_tx_enabled.store(true, Ordering::Relaxed);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(100, 24);

        render_room(&mut app, &mut room, &mut buffer);
        let live_row = row_text(&mut buffer, 0);
        assert!(live_row.contains(" LIVE  MUTE  DEAF "));
        assert!(!live_row.contains("voice"));
        assert!(!live_row.contains("open"));
        assert!(!live_row.contains("kbps"));
        assert!(!live_row.contains("vad"));
        assert!(!live_row.contains(" MIC "));
        assert!(!live_row.contains(" HEAR "));

        app.voice_tx_enabled.store(false, Ordering::Relaxed);
        app.mic_muted.store(true, Ordering::Relaxed);
        render_room(&mut app, &mut room, &mut buffer);
        let muted_row = row_text(&mut buffer, 0);
        assert!(muted_row.contains(" LIVE  MUTE  DEAF "));
        assert!(!muted_row.contains(" MUTED "));

        app.deafened.store(true, Ordering::Relaxed);
        render_room(&mut app, &mut room, &mut buffer);
        let deaf_row = row_text(&mut buffer, 0);
        assert!(deaf_row.contains(" LIVE  MUTE  DEAF "));
        assert!(!deaf_row.contains(" HEAR "));
    }

    #[test]
    fn user_actions_require_users_list_subfocus() {
        let mut app = test_app();
        let mut room = RoomMode::with_focus(ChatPanelFocus::Lobby);
        room.process_input(&mut app, key('h'));

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        assert_eq!(app.view.status.text(), "focus users list to adjust volume");
    }

    #[test]
    fn mouse_down_on_room_list_full_row_focuses_rooms_and_switches_view() {
        let mut app = test_app();
        enter_rooms(&mut app, &[1, 2, 3]);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let (rect, room_id) = room.layout().room_hits[1];
        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + rect.w - 1,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert_eq!(room_id, RoomId(2));
        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Rooms);
        assert_eq!(app.room.viewed_room, Some(RoomId(2)));
    }

    #[test]
    fn mouse_wheel_on_room_list_cycles_rooms() {
        let mut app = test_app();
        enter_rooms(&mut app, &[1, 2, 3]);
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let rect = room.layout().room_list_rect;
        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
        assert_eq!(room.lobby_list_focus(), LobbyListFocus::Rooms);
        assert_eq!(app.room.viewed_room, Some(RoomId(2)));
    }

    #[test]
    fn shift_enter_inserts_newline_in_composer() {
        let mut app = test_app_vim();
        let mut room = RoomMode::default();

        assert!(matches!(
            room.process_input(&mut app, key('a')),
            Action::Continue
        ));
        assert!(matches!(
            room.process_input(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::Continue
        ));
        assert!(matches!(
            room.process_input(&mut app, key('b')),
            Action::Continue
        ));

        assert_eq!(app.view.composer.text(), "a\nb");
    }
}
