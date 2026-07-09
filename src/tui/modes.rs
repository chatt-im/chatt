use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use extui::{Buffer, Rect};
use extui_bindings::{InputKey, LayerId};
use extui_editor::{Editor, Mode as EditorMode, Span as EditorSpan};

use crate::{
    app::{App, ChatPanelFocus, PendingJoin, ServerEditDraft, ServerEditEvent, ToggleExpandResult},
    bindings::{self, BindCommand, Resolved},
    chat_buffer::{LineKind, VisibleLine},
    settings::{self, AudioInputPickerState, AudioOutputPickerState, SettingsDraft},
    theme,
    tui::{
        form::{FormAction, FormFieldKind, FormMouseIntent, FormState},
        mode::{
            AppMode, ChromeSpec, Coverage, ExitReason, ModePresentation, ModeTransition,
            is_quit_key,
        },
        overlay::{ConfirmDisposition, ConfirmMode},
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
        app: &mut App,
        intent: FieldIntent,
        commit: Option<(FieldId, String)>,
        focus_column: Option<u16>,
    ) {
        let output = welcome::welcome_logic(
            &mut self.form,
            &mut self.draft,
            &app.theme,
            &app.config.bindings,
            intent,
            commit,
            focus_column,
        );
        self.handle_output(app, output);
    }

    fn handle_output(&mut self, app: &mut App, output: WelcomeOutput) {
        if output.changed {
            let _ = self.form.set_bindings(self.draft.default_bindings);
            app.apply_theme(self.draft.theme.clone());
        }
        match output.button {
            Some(WelcomeButton::Save) if app.save_welcome(&self.draft) => {
                app.finish_welcome(self.pending_join.take());
            }
            Some(WelcomeButton::Exit) => app.request_quit(),
            Some(WelcomeButton::Save) | None => {}
        }
    }

    fn save_and_continue(&mut self, app: &mut App) {
        let commit = self.form.clear_text();
        self.drive(app, FieldIntent::None, commit, None);
        if app.save_welcome(&self.draft) {
            app.finish_welcome(self.pending_join.take());
        }
    }

    fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        match command {
            BindCommand::SaveSettings => self.save_and_continue(app),
            BindCommand::Cancel | BindCommand::CloseSettings => {
                app.set_status("save setup to continue");
            }
            BindCommand::Quit => return Action::Quit,
            _ => return app.process_global_command(command),
        }
        Action::Continue
    }

    fn resolve_binding(&mut self, app: &mut App, key: KeyEvent) -> Action {
        match resolve_binding(app, bindings::SETTINGS_LAYER, key) {
            BindingResolution::Action(command) => self.process_action(app, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    pub(crate) fn draw_body(&mut self, area: Rect, app: &App, buf: &mut Buffer) {
        welcome::draw_welcome(
            area,
            buf,
            &app.theme,
            &app.config.bindings,
            &mut self.draft,
            &mut self.form,
            &self.config_path_text,
            &self.data_dir_text,
        );
    }
}

impl AppMode for WelcomeMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let chrome = self.presentation(app).chrome.expect("base mode has chrome");
        crate::tui::render::draw_welcome_screen(
            app,
            self,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if is_control_save_chord(app, &key) {
            self.save_and_continue(app);
            return Action::Continue;
        }

        let kind = self.form.focused_kind();
        let text_focused = kind == FormFieldKind::Text;
        let event = self.form.handle_key(key, kind);
        match event.action {
            FormAction::None if !text_focused => return self.resolve_binding(app, key),
            FormAction::None => self.drive(app, FieldIntent::None, event.commit, None),
            FormAction::Cancel => {
                app.set_status("save setup to continue");
            }
            FormAction::Activate if text_focused => {
                self.drive(app, FieldIntent::None, event.commit, None);
                let commit = self.form.move_focus(1);
                self.drive(app, FieldIntent::None, commit, None);
            }
            FormAction::Activate => {
                self.drive(app, FieldIntent::Activate, event.commit, None);
            }
            FormAction::Adjust(delta) => {
                self.drive(app, FieldIntent::Adjust(delta), event.commit, None);
            }
            FormAction::FocusMoved | FormAction::Scrolled => {
                self.drive(app, FieldIntent::None, event.commit, None);
            }
            FormAction::TextChanged => {}
        }
        Action::Continue
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        let event = self.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::None | FormMouseIntent::PickerItem(_, _) => {
                self.drive(app, FieldIntent::None, event.commit, None);
            }
            FormMouseIntent::Activate(_) => {
                self.drive(app, FieldIntent::Activate, event.commit, None);
            }
            FormMouseIntent::Adjust(_, delta) => {
                self.drive(app, FieldIntent::Adjust(delta), event.commit, None);
            }
            FormMouseIntent::Text(_, _, column) => {
                self.drive(app, FieldIntent::None, event.commit, Some(column));
            }
        }
        Action::Continue
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
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

pub(crate) fn resolve_binding(app: &mut App, layer: LayerId, key: KeyEvent) -> BindingResolution {
    let Some(input) = InputKey::from_event(&key) else {
        return BindingResolution::Unmatched;
    };
    match bindings::resolve(
        &app.config.bindings.router,
        layer,
        &mut app.chrome.binding.pending_chord,
        input,
    ) {
        Resolved::Action(id) => {
            BindingResolution::Action(app.config.bindings.actions.get(id).clone())
        }
        Resolved::Consumed => BindingResolution::Consumed,
        Resolved::Unmatched => BindingResolution::Unmatched,
    }
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
    let line_start = before_cursor.rfind('\n').map_or(0, |index| index + 1);
    if &before_cursor[line_start..] != "```" {
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

fn is_control_save_chord(app: &App, key: &KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    let Some(input) = InputKey::from_event(key) else {
        return false;
    };
    let pending = None;
    bindings::reachable(&app.config.bindings, bindings::SETTINGS_LAYER, &pending)
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

    fn selected_label(&self, app: &App) -> Option<String> {
        self.select
            .current_item_index()
            .and_then(|index| app.server_items().get(index))
            .map(|item| item.label.clone())
    }

    pub(crate) fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            Activate => {
                let Some(label) = self.selected_label(app) else {
                    app.set_error("no server selected");
                    return Action::Continue;
                };
                if app.start_network(&label) {
                    app.push_mode(Box::new(RoomMode::default()));
                }
            }
            SelectNext => {
                self.select.move_selection(1);
            }
            SelectPrev => {
                self.select.move_selection(-1);
            }
            EditServer => {
                let Some(label) = self.selected_label(app) else {
                    app.set_error("no server selected");
                    return Action::Continue;
                };
                app.open_server_edit(&label);
            }
            DeleteServer => {
                let Some(label) = self.selected_label(app) else {
                    app.set_error("no server selected");
                    return Action::Continue;
                };
                let prompt = format!("Delete server '{label}'?");
                app.push_mode(Box::new(ConfirmMode::new(
                    prompt,
                    "Delete",
                    "Cancel",
                    move |app| {
                        app.delete_server(&label);
                        ConfirmDisposition::Transition(ModeTransition::Pop)
                    },
                )));
            }
            SearchServers => {
                self.searching = true;
                self.select.clear_query();
                self.select.refresh(app.server_items());
            }
            Cancel if app.network.is_some() => app.push_mode(Box::new(RoomMode::default())),
            _ => return app.process_global_command(command),
        }
        Action::Continue
    }
}

impl Default for ServerListMode {
    fn default() -> Self {
        Self::new()
    }
}

impl AppMode for ServerListMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        self.select.refresh(app.server_items());
        let chrome = self.presentation(app).chrome.expect("base mode has chrome");
        crate::tui::render::draw_server_select_screen(
            app,
            &mut self.select,
            self.searching,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
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
                _ if self.select.edit_query(key) => self.select.refresh(app.server_items()),
                _ => {}
            }
            return Action::Continue;
        }
        match resolve_binding(app, bindings::PICKER_LAYER, key) {
            BindingResolution::Action(command) => self.process_action(app, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
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

    fn refresh(&mut self, app: &App) {
        self.items = app.room_select_items();
        self.select.refresh(&self.items);
    }

    pub(crate) fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            Activate => {
                let Some(room_id) = self.selected_room() else {
                    app.set_error("no room selected");
                    return Action::Continue;
                };
                app.set_viewed_room(room_id);
                app.pop_mode();
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
                self.refresh(app);
            }
            Cancel | RoomSwitcher => app.pop_mode(),
            _ => return app.process_global_command(command),
        }
        Action::Continue
    }
}

impl AppMode for RoomSwitchMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        self.refresh(app);
        let chrome = self.presentation(app).chrome.expect("base mode has chrome");
        crate::tui::render::draw_room_select_screen(
            app,
            &mut self.select,
            &self.items,
            self.searching,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
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
                _ if self.select.edit_query(key) => self.refresh(app),
                _ => {}
            }
            return Action::Continue;
        }
        match resolve_binding(app, bindings::PICKER_LAYER, key) {
            BindingResolution::Action(command) => self.process_action(app, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
        ModePresentation::full_screen(ChromeSpec {
            theme_mode: theme::UiMode::ServerSelect,
            status_label: "Rooms",
            layer: bindings::PICKER_LAYER,
        })
    }
}

pub(crate) struct ServerEditMode {
    draft: ServerEditDraft,
}

impl ServerEditMode {
    pub(crate) fn new(draft: ServerEditDraft) -> Self {
        Self { draft }
    }

    fn handle_event(&mut self, app: &mut App, event: ServerEditEvent) {
        match event {
            ServerEditEvent::Consumed => {}
            ServerEditEvent::Cancel => app.pop_mode(),
            ServerEditEvent::Save { join_after_save } => {
                app.save_server_edit_with(&self.draft, join_after_save);
            }
        }
    }
}

impl AppMode for ServerEditMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        crate::tui::render::draw_server_edit_overlay(app, &mut self.draft, buf);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        let event = self.draft.handle_key(key, &app.theme);
        self.handle_event(app, event);
        Action::Continue
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        let event = self.draft.handle_mouse(mouse, &app.theme);
        self.handle_event(app, event);
        Action::Continue
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
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
    catalog_generation: u64,
}

impl SettingsSession {
    fn new(app: &App) -> Self {
        let mut draft = SettingsDraft::from_audio(&app.config.audio);
        draft.set_form_bindings_from_config(app.config.ui.default_bindings);
        draft.set_theme_from_config(
            app.config.ui.theme.clone(),
            app.config.ui.themes.resolved.keys().cloned().collect(),
        );
        draft.set_web_from_config(&app.config.web);
        draft.set_notifications_from_config(&app.config.notifications);
        draft.set_files_from_config(&app.config.files);
        draft.set_p2p_from_config(&app.config.p2p);
        draft.set_history_from_config(&app.config.history);
        let input_items = settings::audio_input_items(app.audio_devices.input_devices());
        let output_items = settings::audio_output_items(app.audio_devices.output_devices());
        let mut input_picker = AudioInputPickerState::default();
        input_picker.reset(&input_items, draft.input_selection());
        let mut output_picker = AudioOutputPickerState::default();
        output_picker.reset(&output_items, draft.output_selection());
        Self {
            form: FormState::new(
                crate::ui::settings::initial_focus(),
                app.config.ui.default_bindings,
            ),
            draft,
            input_items,
            output_items,
            input_picker,
            output_picker,
            dirty: false,
            catalog_generation: app.audio_devices.generation(),
        }
    }

    pub(crate) fn sync_catalog(&mut self, app: &App) {
        if self.catalog_generation == app.audio_devices.generation() {
            return;
        }
        self.catalog_generation = app.audio_devices.generation();
        self.input_items = settings::audio_input_items(app.audio_devices.input_devices());
        if self.input_picker.open {
            self.input_picker
                .refresh_items(&self.input_items, self.draft.input_selection());
        } else {
            self.input_picker
                .reset(&self.input_items, self.draft.input_selection());
        }
        self.output_items = settings::audio_output_items(app.audio_devices.output_devices());
        if self.output_picker.open {
            self.output_picker
                .refresh_items(&self.output_items, self.draft.output_selection());
        } else {
            self.output_picker
                .reset(&self.output_items, self.draft.output_selection());
        }
    }
}

pub(crate) struct SettingsMode {
    session: SettingsSession,
}

impl SettingsMode {
    pub(crate) fn new(app: &App) -> Self {
        Self {
            session: SettingsSession::new(app),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_form_for_test(form: FormState<FieldId>, app: &App) -> Self {
        let mut mode = Self::new(app);
        mode.session.form = form;
        mode
    }

    pub(crate) fn session_mut(&mut self) -> &mut SettingsSession {
        &mut self.session
    }

    fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            SaveSettings => app.save_settings(&mut self.session),
            Activate => app.drive_settings(&mut self.session, FieldIntent::Activate, None, None),
            FocusNext => app.move_settings_focus(&mut self.session, 1),
            FocusPrev => app.move_settings_focus(&mut self.session, -1),
            SelectNext => app.move_settings_selection(&mut self.session, 1),
            SelectPrev => app.move_settings_selection(&mut self.session, -1),
            AdjustLeft => {
                app.drive_settings(&mut self.session, FieldIntent::Adjust(-1), None, None)
            }
            AdjustRight => {
                app.drive_settings(&mut self.session, FieldIntent::Adjust(1), None, None)
            }
            Cancel | CloseSettings => {
                if !app.cancel_open_audio_picker(&mut self.session) {
                    app.close_settings(&mut self.session);
                }
            }
            RefreshDevices => app.refresh_audio_devices_for_settings(&self.session),
            _ => return app.process_global_command(command),
        }
        Action::Continue
    }

    fn resolve_binding(&mut self, app: &mut App, key: KeyEvent) -> Action {
        match resolve_binding(app, bindings::SETTINGS_LAYER, key) {
            BindingResolution::Action(command) => self.process_action(app, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }
}

impl AppMode for SettingsMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        self.session.sync_catalog(app);
        let chrome = self.presentation(app).chrome.expect("base mode has chrome");
        crate::tui::render::draw_settings_screen(
            app,
            self,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
        );
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        self.session.sync_catalog(app);
        if app.handle_open_settings_picker_key(&mut self.session, key) {
            return Action::Continue;
        }
        if is_control_save_chord(app, &key) {
            app.save_settings(&mut self.session);
            return Action::Continue;
        }

        let kind = self.session.form.focused_kind();
        let text_focused = kind == FormFieldKind::Text;
        let event = self.session.form.handle_key(key, kind);
        match event.action {
            FormAction::None if !text_focused => return self.resolve_binding(app, key),
            FormAction::None => {
                app.drive_settings(&mut self.session, FieldIntent::None, event.commit, None);
            }
            FormAction::Cancel => {
                if !app.cancel_open_audio_picker(&mut self.session) {
                    app.close_settings(&mut self.session);
                }
            }
            FormAction::Activate if text_focused => {
                // Enter in a text field commits the edit then advances focus,
                // matching the previous buffer/web-bind behavior.
                app.drive_settings(&mut self.session, FieldIntent::None, event.commit, None);
                app.move_settings_focus(&mut self.session, 1);
            }
            FormAction::Activate => {
                app.drive_settings(&mut self.session, FieldIntent::Activate, event.commit, None);
            }
            FormAction::Adjust(delta) => {
                app.drive_settings(
                    &mut self.session,
                    FieldIntent::Adjust(delta),
                    event.commit,
                    None,
                );
            }
            FormAction::FocusMoved | FormAction::Scrolled => {
                app.drive_settings(&mut self.session, FieldIntent::None, event.commit, None);
            }
            FormAction::TextChanged => app.mark_settings_dirty(&mut self.session),
        }
        Action::Continue
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        if app.process_top_bar_mouse(mouse) {
            return Action::Continue;
        }
        self.session.sync_catalog(app);
        if app.handle_open_settings_picker_mouse(&mut self.session, mouse) {
            return Action::Continue;
        }

        let event = self.session.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::None => {
                app.drive_settings(&mut self.session, FieldIntent::None, event.commit, None);
            }
            FormMouseIntent::Activate(_) => {
                app.drive_settings(&mut self.session, FieldIntent::Activate, event.commit, None);
            }
            FormMouseIntent::Adjust(_, delta) => {
                app.drive_settings(
                    &mut self.session,
                    FieldIntent::Adjust(delta),
                    event.commit,
                    None,
                );
            }
            FormMouseIntent::Text(_, _, column) => {
                app.drive_settings(
                    &mut self.session,
                    FieldIntent::None,
                    event.commit,
                    Some(column),
                );
            }
            FormMouseIntent::PickerItem(field, item_index) => {
                app.activate_settings_picker_item(&mut self.session, field, item_index);
            }
        }
        Action::Continue
    }

    fn on_exit(&mut self, app: &mut App, _reason: ExitReason) {
        app.finish_settings_session(&mut self.session);
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
        ModePresentation::full_screen(ChromeSpec {
            theme_mode: theme::UiMode::Settings,
            status_label: "Settings",
            layer: bindings::SETTINGS_LAYER,
        })
    }
}

#[derive(Debug)]
pub(crate) struct RoomLayout {
    pub chat_width: u16,
    pub chat_height: u16,
    pub chat_rect: Rect,
    pub visible_chat_lines: Vec<VisibleLine>,
    pub room_list_rect: Rect,
    pub lobby_divider_rect: Rect,
    pub user_list_rect: Rect,
    /// Hit boxes of the room rows drawn in the room list.
    pub room_hits: Vec<(Rect, rpc::ids::RoomId)>,
    pub lobby_bar_rect: Rect,
    pub chat_log_bar_rect: Rect,
    pub composer_rect: Rect,
    pub compose_bar_rect: Rect,
}

impl Default for RoomLayout {
    fn default() -> Self {
        Self {
            chat_width: 80,
            chat_height: 0,
            chat_rect: Rect::EMPTY,
            visible_chat_lines: Vec::new(),
            room_list_rect: Rect::EMPTY,
            lobby_divider_rect: Rect::EMPTY,
            user_list_rect: Rect::EMPTY,
            room_hits: Vec::new(),
            lobby_bar_rect: Rect::EMPTY,
            chat_log_bar_rect: Rect::EMPTY,
            composer_rect: Rect::EMPTY,
            compose_bar_rect: Rect::EMPTY,
        }
    }
}

impl RoomLayout {
    pub(crate) fn clear_workspace(&mut self) {
        self.room_list_rect = Rect::EMPTY;
        self.lobby_divider_rect = Rect::EMPTY;
        self.user_list_rect = Rect::EMPTY;
        self.room_hits.clear();
        self.lobby_bar_rect = Rect::EMPTY;
        self.chat_log_bar_rect = Rect::EMPTY;
        self.composer_rect = Rect::EMPTY;
        self.compose_bar_rect = Rect::EMPTY;
    }

    fn room_hit(&self, column: u16, row: u16) -> Option<rpc::ids::RoomId> {
        self.room_hits
            .iter()
            .find(|(rect, _)| crate::tui::form::rect_contains(*rect, column, row))
            .map(|(_, room_id)| *room_id)
    }

    pub(crate) fn clear_chat(&mut self) {
        self.chat_height = 0;
        self.chat_rect = Rect::EMPTY;
        self.visible_chat_lines.clear();
    }

    fn chat_line_at(&self, row: u16) -> Option<VisibleLine> {
        let index = row.checked_sub(self.chat_rect.y)? as usize;
        self.visible_chat_lines.get(index).copied()
    }
}

#[derive(Debug)]
pub(crate) struct RoomMode {
    focus: ChatPanelFocus,
    lobby_list_focus: LobbyListFocus,
    layout: RoomLayout,
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
        }
    }

    #[cfg(test)]
    pub(crate) fn with_focus(focus: ChatPanelFocus) -> Self {
        Self {
            focus,
            lobby_list_focus: LobbyListFocus::Users,
            layout: RoomLayout::default(),
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

    pub(crate) fn set_focus(&mut self, app: &mut App, focus: ChatPanelFocus) {
        self.focus = focus;
        match focus {
            ChatPanelFocus::Lobby if self.lobby_list_focus == LobbyListFocus::Users => {
                self.keep_selected_room_user_visible(app);
            }
            ChatPanelFocus::Lobby => {}
            ChatPanelFocus::ChatLog => {
                app.room
                    .active
                    .chat
                    .ensure_selected_header(self.layout.chat_width);
            }
            ChatPanelFocus::Compose => {}
        }
    }

    fn enter_compose_insert_mode(&mut self, app: &mut App) {
        app.room.composer.enter_insert_mode();
        self.set_focus(app, ChatPanelFocus::Compose);
    }

    fn move_focus(&mut self, app: &mut App, delta: isize) {
        self.set_focus(app, self.focus.moved(delta));
    }

    fn set_lobby_list_focus(&mut self, app: &mut App, focus: LobbyListFocus) {
        self.lobby_list_focus = focus;
        self.set_focus(app, ChatPanelFocus::Lobby);
    }

    fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            EnterCompose => self.enter_compose_insert_mode(app),
            EnterLog => self.set_focus(app, ChatPanelFocus::ChatLog),
            SubmitMessage => app.submit_input(),
            Cancel => {
                if self.focus == ChatPanelFocus::Compose {
                    app.room.composer.clear();
                }
                self.enter_compose_insert_mode(app);
            }
            ScrollUp => self.scroll_focused_panel(app, -1),
            ScrollDown => self.scroll_focused_panel(app, 1),
            RoomScrollUp => self.move_user_selection_with_focus(app, -1),
            RoomScrollDown => self.move_user_selection_with_focus(app, 1),
            OpenSelectedUserVolume => {
                if self.focus != ChatPanelFocus::Lobby {
                    app.set_status("focus lobby to adjust users");
                } else if self.lobby_list_focus == LobbyListFocus::Users {
                    app.open_selected_user_volume();
                } else {
                    app.set_status("focus users list to adjust volume");
                }
            }
            ToggleSelectedUserMute => {
                if self.focus != ChatPanelFocus::Lobby {
                    app.set_status("focus lobby to mute users");
                } else if self.lobby_list_focus == LobbyListFocus::Users {
                    app.toggle_selected_user_mute();
                } else {
                    app.set_status("focus users list to mute users");
                }
            }
            HalfPageUp => {
                self.scroll_chat_log_if_focused(app, -(self.chat_half_page_rows() as isize));
            }
            HalfPageDown => {
                self.scroll_chat_log_if_focused(app, self.chat_half_page_rows() as isize);
            }
            Top => self.select_chat_top(app),
            Bottom => self.select_chat_bottom(app),
            CopySelection => self.copy_chat_selection_if_focused(app),
            CopyMessageRef => self.copy_message_ref_if_focused(app),
            InsertMessageRef => self.insert_message_ref_if_focused(app),
            OpenMessageRef => self.open_message_ref_if_focused(app),
            ToggleExpand => self.toggle_chat_expand_if_focused(app),
            FocusNext => self.move_focus(app, 1),
            FocusPrev => self.move_focus(app, -1),
            SelectNext if self.focus == ChatPanelFocus::Lobby => self.scroll_focused_panel(app, 1),
            SelectPrev if self.focus == ChatPanelFocus::Lobby => self.scroll_focused_panel(app, -1),
            AdjustLeft if self.focus == ChatPanelFocus::Lobby => {
                self.set_lobby_list_focus(app, LobbyListFocus::Rooms);
            }
            AdjustRight if self.focus == ChatPanelFocus::Lobby => {
                self.set_lobby_list_focus(app, LobbyListFocus::Users);
            }
            ClearChat if self.focus == ChatPanelFocus::ChatLog => app.room.clear_chat(),
            PasteClipboard => {
                self.paste_from_clipboard(app, &crate::clipboard_paste::HelperClipboard)
            }
            RoomSwitcher => app.open_room_switcher(),
            OpenUserList => app.open_user_list(),
            OpenRoomSettings => app.open_room_settings(),
            NextRoom => app.cycle_room(1),
            PrevRoom => app.cycle_room(-1),
            _ => return app.process_global_command(command),
        }
        Action::Continue
    }

    fn process_compose_key(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if app.room.composer.mode() == EditorMode::Insert {
            if key.code == extui::event::KeyCode::Tab
                && key.modifiers.is_empty()
                && app.room.complete_command()
            {
                return Action::Continue;
            }
            if key.code != extui::event::KeyCode::Esc {
                match resolve_binding(app, bindings::INSERT_LAYER, key) {
                    BindingResolution::Action(command) => {
                        return self.process_action(app, command);
                    }
                    BindingResolution::Consumed => return Action::Continue,
                    BindingResolution::Unmatched => {}
                }
            }
            if app.room.composer.send_key(&key) {
                maybe_auto_close_markdown_code_fence(&mut app.room.composer, key);
            }
            return Action::Continue;
        }

        match resolve_binding(app, bindings::COMPOSE_NORMAL_LAYER, key) {
            BindingResolution::Action(command) => return self.process_action(app, command),
            BindingResolution::Consumed => return Action::Continue,
            BindingResolution::Unmatched => {}
        }
        let _ = app.room.composer.send_key(&key);
        Action::Continue
    }

    /// Reads the clipboard and either inserts text into the composer or opens
    /// the image upload dialog. Text focuses the composer first so the paste is
    /// visible from any room focus.
    fn paste_from_clipboard(
        &mut self,
        app: &mut App,
        provider: &dyn crate::clipboard_paste::ClipboardPasteProvider,
    ) {
        use crate::clipboard_paste::PastePayload;
        match provider.read_paste() {
            Ok(PastePayload::Text(text)) => {
                self.enter_compose_insert_mode(app);
                app.room.insert_paste(text);
            }
            Ok(PastePayload::Image(image)) => app.open_paste_image_dialog(image),
            Ok(PastePayload::Empty) => app.set_status("clipboard is empty"),
            Ok(PastePayload::Unsupported(reason)) => app.set_status(reason),
            Err(error) => app.set_error(error.to_string()),
        }
    }

    fn process_chat_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
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
        let in_composer =
            crate::tui::form::rect_contains(self.layout.composer_rect, mouse.column, mouse.row)
                || crate::tui::form::rect_contains(
                    self.layout.compose_bar_rect,
                    mouse.column,
                    mouse.row,
                );

        match mouse.kind {
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_composer => {
                self.enter_compose_insert_mode(app);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left)
                if crate::tui::form::rect_contains(
                    app.chrome.lobby_bar.audio_reset,
                    mouse.column,
                    mouse.row,
                ) =>
            {
                app.audio_manual_reset();
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_lobby_bar => {
                self.set_focus(app, ChatPanelFocus::Lobby);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_room_list => {
                self.set_lobby_list_focus(app, LobbyListFocus::Rooms);
                if let Some(room_id) = self.layout.room_hit(mouse.column, mouse.row) {
                    app.set_viewed_room(room_id);
                }
            }
            extui::event::MouseEventKind::ScrollUp if in_room_list => {
                self.set_lobby_list_focus(app, LobbyListFocus::Rooms);
                app.cycle_room(-1);
            }
            extui::event::MouseEventKind::ScrollDown if in_room_list => {
                self.set_lobby_list_focus(app, LobbyListFocus::Rooms);
                app.cycle_room(1);
            }
            extui::event::MouseEventKind::ScrollUp if in_user_list => {
                self.move_user_selection_with_focus(app, -1);
            }
            extui::event::MouseEventKind::ScrollDown if in_user_list => {
                self.move_user_selection_with_focus(app, 1);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_user_list => {
                self.set_lobby_list_focus(app, LobbyListFocus::Users);
                let row = mouse.row.saturating_sub(self.layout.user_list_rect.y) as usize;
                if app.room.select_visible_participant(row).is_some() {
                    self.keep_selected_room_user_visible(app);
                }
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_chat_bar => {
                self.set_focus(app, ChatPanelFocus::ChatLog);
            }
            extui::event::MouseEventKind::ScrollUp if in_chat => {
                self.set_focus(app, ChatPanelFocus::ChatLog);
                self.scroll_chat_up(app, 5);
            }
            extui::event::MouseEventKind::ScrollDown if in_chat => {
                self.set_focus(app, ChatPanelFocus::ChatLog);
                app.room.active.chat.scroll_down(5);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_chat => {
                self.set_focus(app, ChatPanelFocus::ChatLog);
                match self.layout.chat_line_at(mouse.row) {
                    Some(line) => match line.kind {
                        LineKind::Heading | LineKind::Ellipsis => {
                            app.room
                                .active
                                .chat
                                .select_header_containing(line.message, self.layout.chat_width);
                            app.room
                                .active
                                .chat
                                .toggle_expand(line.message, self.layout.chat_width);
                            self.keep_selected_chat_header_visible(app);
                            app.room.active.chat.clear_selection();
                        }
                        LineKind::Body => {
                            app.room
                                .active
                                .chat
                                .begin_selection((line.message, line.line));
                        }
                    },
                    _ => app.room.active.chat.clear_selection(),
                }
            }
            extui::event::MouseEventKind::Drag(extui::event::MouseButton::Left)
                if app.room.active.chat.is_selecting() =>
            {
                self.set_focus(app, ChatPanelFocus::ChatLog);
                self.drag_chat_selection(app, mouse.row);
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left) if in_chat => {
                // A collapsed selection (press and release without a drag) over a
                // message reference jumps to it and over a URL opens it; a drag
                // remains a text selection.
                if app.room.active.chat.selection_is_click() {
                    if let Some(target) = self.chat_ref_at(app, mouse.column, mouse.row) {
                        self.jump_to_ref(app, target);
                    } else if let Some(url) = self.chat_link_at(app, mouse.column, mouse.row) {
                        app.room.request_open_url(url);
                    }
                }
                app.room.active.chat.end_selection();
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left) => {
                app.room.active.chat.end_selection();
            }
            _ => {}
        }
        Action::Continue
    }

    /// Resolves a screen cell to the URL of a link under it, if any. Returns an
    /// owned string so the caller can mutably borrow `app` to queue the open.
    fn chat_link_at(&self, app: &App, column: u16, row: u16) -> Option<String> {
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
        app.room
            .active
            .chat
            .link_at(line.message, line.line, col_in_line)
            .map(str::to_owned)
    }

    /// Resolves a screen cell to the message reference under it, if any.
    fn chat_ref_at(&self, app: &App, column: u16, row: u16) -> Option<rpc::msgref::MessageRef> {
        let line = self.layout.chat_line_at(row)?;
        if line.kind != LineKind::Body {
            return None;
        }
        let content_x = self.layout.chat_rect.x.saturating_add(1);
        if column < content_x {
            return None;
        }
        let col_in_line = column - content_x;
        app.room
            .active
            .chat
            .ref_at(line.message, line.line, col_in_line)
    }

    /// Jumps to a reference's target: selects and scrolls to the message when
    /// present in the buffer, otherwise reports why not.
    fn jump_to_ref(&mut self, app: &mut App, target: rpc::msgref::MessageRef) {
        match app
            .room
            .jump_to_ref(target, self.layout.chat_width, self.layout.chat_height)
        {
            crate::app::room::RefJump::Jumped => self.set_focus(app, ChatPanelFocus::ChatLog),
            crate::app::room::RefJump::NotFound => {
                app.set_status("referenced message is not in this room's history");
            }
            crate::app::room::RefJump::OtherRoom => {
                if app.set_viewed_room(target.room_id) {
                    match app.room.jump_to_ref(
                        target,
                        self.layout.chat_width,
                        self.layout.chat_height,
                    ) {
                        crate::app::room::RefJump::Jumped => {
                            self.set_focus(app, ChatPanelFocus::ChatLog)
                        }
                        _ => app.set_status("referenced message is not in this room's history"),
                    }
                    return;
                }
                match app.room.cross_room_ref_preview(target) {
                    Some(preview) => app.set_status(preview),
                    None => app.set_status("reference points to another room"),
                }
            }
        }
    }

    fn copy_message_ref_if_focused(&mut self, app: &mut App) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        match app.room.copy_message_ref(self.layout.chat_width) {
            Some(code) => app.set_status(format!("copied {code}")),
            None => app.set_status("select a message to reference"),
        }
    }

    fn insert_message_ref_if_focused(&mut self, app: &mut App) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        if app
            .room
            .insert_message_ref(self.layout.chat_width)
            .is_some()
        {
            self.enter_compose_insert_mode(app);
        } else {
            app.set_status("select a message to reference");
        }
    }

    fn open_message_ref_if_focused(&mut self, app: &mut App) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        let Some(target) = app.room.active.chat.selected_ref(self.layout.chat_width) else {
            app.set_status("selected message contains no reference");
            return;
        };
        self.jump_to_ref(app, target);
    }

    fn drag_chat_selection(&mut self, app: &mut App, row: u16) {
        let rect = self.layout.chat_rect;
        if row < rect.y {
            self.scroll_chat_up(app, 1);
        } else if row >= rect.y.saturating_add(rect.h) {
            app.room.active.chat.scroll_down(1);
        }
        let clamped = row.clamp(rect.y, rect.y.saturating_add(rect.h).saturating_sub(1));
        if let Some(line) = self.layout.chat_line_at(clamped)
            && line.kind == LineKind::Body
        {
            app.room
                .active
                .chat
                .extend_selection((line.message, line.line));
        }
    }

    fn toggle_selected_log_collapse(&mut self, app: &mut App) {
        let width = self.layout.chat_width;
        match app.room.toggle_selected_message_expand(width) {
            ToggleExpandResult::Toggled => {}
            ToggleExpandResult::NoMessages => {
                app.set_status("no messages");
                return;
            }
            ToggleExpandResult::NotCollapsible => {
                app.set_status("selected log is not collapsible");
            }
        }
        self.keep_selected_chat_header_visible(app);
    }

    fn scroll_chat_up(&mut self, app: &mut App, rows: usize) {
        app.room
            .active
            .chat
            .scroll_up(rows, self.layout.chat_width, self.layout.chat_height);
        app.request_older_history_if_at_top(self.layout.chat_width, self.layout.chat_height);
    }

    fn copy_chat_selection(&mut self, app: &mut App) {
        if app
            .room
            .copy_chat_selection(self.layout.chat_width)
            .is_some()
        {
            app.set_transient_status("copied to clipboard");
        }
    }

    fn select_chat_top(&mut self, app: &mut App) {
        if self.focus == ChatPanelFocus::ChatLog {
            app.room
                .active
                .chat
                .top(self.layout.chat_width, self.layout.chat_height);
            app.room.active.chat.select_first_header();
            app.room.active.chat.clear_selection();
            self.keep_selected_chat_header_visible(app);
            app.request_older_history_if_at_top(self.layout.chat_width, self.layout.chat_height);
        }
    }

    fn select_chat_bottom(&mut self, app: &mut App) {
        if self.focus == ChatPanelFocus::ChatLog {
            app.room.active.chat.bottom();
            app.room
                .active
                .chat
                .select_last_header(self.layout.chat_width);
            app.room.active.chat.clear_selection();
            self.keep_selected_chat_header_visible(app);
        }
    }

    fn copy_chat_selection_if_focused(&mut self, app: &mut App) {
        if self.focus == ChatPanelFocus::ChatLog {
            self.copy_chat_selection(app);
        }
    }

    fn toggle_chat_expand_if_focused(&mut self, app: &mut App) {
        if self.focus == ChatPanelFocus::ChatLog {
            self.toggle_selected_log_collapse(app);
        }
    }

    fn scroll_focused_panel(&mut self, app: &mut App, direction: isize) {
        match self.focus {
            ChatPanelFocus::ChatLog => self.move_chat_log_selection(app, direction),
            ChatPanelFocus::Lobby => match self.lobby_list_focus {
                LobbyListFocus::Rooms => self.move_room_view_with_focus(app, direction),
                LobbyListFocus::Users => self.move_user_selection_with_focus(app, direction),
            },
            ChatPanelFocus::Compose => {}
        }
    }

    fn scroll_chat_log_if_focused(&mut self, app: &mut App, rows: isize) {
        if self.focus != ChatPanelFocus::ChatLog {
            return;
        }
        if rows < 0 {
            self.scroll_chat_up(app, rows.unsigned_abs());
        } else {
            app.room.active.chat.scroll_down(rows as usize);
        }
    }

    fn chat_half_page_rows(&self) -> usize {
        (self.layout.chat_height as usize / 2).max(1)
    }

    fn move_chat_log_selection(&mut self, app: &mut App, delta: isize) {
        self.set_focus(app, ChatPanelFocus::ChatLog);
        if !app
            .room
            .move_selected_message(delta, self.layout.chat_width)
        {
            app.set_status("no messages");
            return;
        }
        self.keep_selected_chat_header_visible(app);
    }

    fn keep_selected_chat_header_visible(&mut self, app: &mut App) {
        app.room
            .active
            .chat
            .keep_selected_header_visible(self.layout.chat_width, self.layout.chat_height);
    }

    fn move_room_view_with_focus(&mut self, app: &mut App, delta: isize) {
        self.lobby_list_focus = LobbyListFocus::Rooms;
        self.set_focus(app, ChatPanelFocus::Lobby);
        app.cycle_room(delta);
    }

    fn move_user_selection_with_focus(&mut self, app: &mut App, delta: isize) {
        self.lobby_list_focus = LobbyListFocus::Users;
        self.set_focus(app, ChatPanelFocus::Lobby);
        self.move_user_selection(app, delta);
    }

    fn move_user_selection(&mut self, app: &mut App, delta: isize) {
        if app.room.move_participant_selection(delta).is_none() {
            app.set_status("no users in the current room yet");
            return;
        }
        self.keep_selected_room_user_visible(app);
        self.focus = ChatPanelFocus::Lobby;
        self.lobby_list_focus = LobbyListFocus::Users;
    }

    fn keep_selected_room_user_visible(&mut self, app: &mut App) {
        // Before the first render the participant rect is empty; fall back to
        // the configured lobby height.
        let fallback = app.config.ui.room_height.max(1);
        let visible_rows = self.layout.user_list_rect.h.max(fallback) as usize;
        app.room.keep_selected_participant_visible(visible_rows);
    }
}

impl AppMode for RoomMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64) {
        let chrome = self.presentation(app).chrome.expect("base mode has chrome");
        crate::tui::render::draw_room_screen(
            app,
            self.focus,
            self.lobby_list_focus,
            &mut self.layout,
            chrome.theme_mode,
            chrome.status_label,
            chrome.layer,
            buf,
            now_ms,
        );
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if self.focus == ChatPanelFocus::Compose {
            return self.process_compose_key(app, key);
        }
        match resolve_binding(app, bindings::WORKSPACE_LAYER, key) {
            BindingResolution::Action(command) => self.process_action(app, command),
            BindingResolution::Consumed | BindingResolution::Unmatched => Action::Continue,
        }
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        if app.process_top_bar_mouse(mouse) {
            return Action::Continue;
        }
        self.process_chat_mouse(app, mouse)
    }

    fn process_paste(&mut self, app: &mut App, text: String) {
        self.enter_compose_insert_mode(app);
        app.room.insert_paste(text);
    }

    fn presentation(&self, app: &App) -> ModePresentation {
        let theme_mode = if self.focus == ChatPanelFocus::Compose {
            theme::UiMode::Compose
        } else {
            theme::UiMode::Log
        };
        let layer = if self.focus != ChatPanelFocus::Compose {
            bindings::WORKSPACE_LAYER
        } else if app.room.composer.mode() == EditorMode::Insert {
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
mod tests {
    use std::sync::atomic::Ordering;

    use extui::{
        Buffer, Style,
        event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    };
    use extui_editor::Mode as EditorMode;
    use rpc::{
        control::{ChatMessage, ParticipantVoiceStatus},
        ids::{MessageId, RoomId, UserId},
    };
    use toml_spanner::Arena;

    use super::*;
    use crate::{chat_buffer::LineKind, config::Config};

    fn test_app() -> App {
        App::new(Config::default(), None).expect("test app")
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
                app.room.composer.text().contains("pasted"),
                "focus {focus:?} did not receive paste"
            );
        }
    }

    #[test]
    fn typed_markdown_code_fence_auto_closes() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "```");

        assert_eq!(app.room.composer.text(), "```\n```");
        assert_eq!(app.room.composer.cursor_offset(), 3);
    }

    #[test]
    fn typed_markdown_code_fence_language_inserts_before_closer() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "```rust");

        assert_eq!(app.room.composer.text(), "```rust\n```");
        assert_eq!(app.room.composer.cursor_offset(), "```rust".len() as u32);
    }

    #[test]
    fn typed_markdown_code_fence_inside_line_does_not_auto_close() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.room.composer.set_lines("``x");
        app.room.composer.set_cursor_offset(2);
        app.room.composer.enter_insert_mode();

        room.process_input(&mut app, key('`'));

        assert_eq!(app.room.composer.text(), "```x");
        assert_eq!(app.room.composer.cursor_offset(), 3);
    }

    #[test]
    fn pasted_markdown_code_fence_does_not_auto_close() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        room.process_paste(&mut app, "```".to_string());

        assert_eq!(app.room.composer.text(), "```");
        assert_eq!(app.room.composer.cursor_offset(), 3);
    }

    #[test]
    fn direct_insert_paste_markdown_code_fence_does_not_auto_close() {
        let mut app = test_app();

        app.room.insert_paste("```".to_string());

        assert_eq!(app.room.composer.text(), "```");
        assert_eq!(app.room.composer.cursor_offset(), 3);
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
        room.render(app, buffer, 0);
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
            },
            None,
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
        assert_eq!(app.room.composer.text(), "");
        assert!(app.chrome.binding.pending_chord.is_some());

        room.process_input(&mut app, key('z'));
        assert_eq!(app.room.composer.text(), "");
        assert!(app.chrome.binding.pending_chord.is_none());
    }

    #[test]
    fn unmatched_insert_key_is_inserted_once() {
        let mut app = app_with_bindings("[bindings.insert]\n\"x x\" = \"ToggleMute\"\n");
        let mut room = RoomMode::default();

        room.process_input(&mut app, key('q'));

        assert_eq!(app.room.composer.text(), "q");
    }

    #[test]
    fn tab_completes_lobby_slash_command() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "/rep");
        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));

        assert_eq!(app.room.composer.text(), "/report-bug");
        assert_eq!(app.room.composer.mode(), EditorMode::Insert);
    }

    #[test]
    fn repeated_tab_cycles_lobby_slash_commands() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        type_text(&mut room, &mut app, "/s");
        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.room.composer.text(), "/servers");

        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.room.composer.text(), "/settings");

        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(app.room.composer.text(), "/sound");
    }

    #[test]
    fn compose_normal_chord_prefix_is_not_sent_to_editor() {
        let mut app = app_with_bindings("[bindings.compose-normal]\n\"z z\" = \"ToggleMute\"\n");
        let mut room = RoomMode::default();
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.room.composer.mode(), EditorMode::Normal);

        room.process_input(&mut app, key('z'));

        assert_eq!(app.room.composer.text(), "");
        assert!(app.chrome.binding.pending_chord.is_some());
    }

    #[test]
    fn escape_leaves_compose_focused_in_vim_normal_mode() {
        let mut app = test_app();
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
        assert_eq!(app.room.composer.mode(), EditorMode::Normal);

        assert!(matches!(
            room.process_input(&mut app, key('i')),
            Action::Continue
        ));
        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.room.composer.mode(), EditorMode::Insert);
    }

    #[test]
    fn compose_vim_text_object_commands_receive_i_key() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.room.composer.set_lines("alpha beta");
        app.room.composer.set_cursor_offset(2);

        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.room.composer.mode(), EditorMode::Normal);

        for key in ['c', 'i', 'w'] {
            room.process_input(
                &mut app,
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::empty()),
            );
        }

        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.room.composer.mode(), EditorMode::Insert);
        assert_eq!(app.room.composer.text(), " beta");
    }

    #[test]
    fn shifted_jk_wraps_chat_panel_focus() {
        let mut app = test_app();
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
        assert_eq!(app.room.composer.mode(), EditorMode::Normal);

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
        assert_eq!(app.room.composer.mode(), EditorMode::Insert);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SUPER),
        );
        assert_eq!(room.focus(), ChatPanelFocus::ChatLog);

        room.set_focus(&mut app, ChatPanelFocus::Compose);
        assert_eq!(app.room.composer.mode(), EditorMode::Insert);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::SUPER),
        );
        assert_eq!(room.focus(), ChatPanelFocus::Lobby);
    }

    #[test]
    fn chat_log_jk_moves_selected_message() {
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
        assert_eq!(app.room.active.chat.selected_message(), Some(2));

        room.process_input(&mut app, key('k'));
        assert_eq!(app.room.active.chat.selected_message(), Some(1));

        room.process_input(&mut app, key('j'));
        assert_eq!(app.room.active.chat.selected_message(), Some(2));
    }

    #[test]
    fn chat_log_gg_and_g_select_edge_headers() {
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
        assert_eq!(app.room.active.chat.selected_message(), Some(0));
        assert!(app.room.active.chat.scroll_offset() > 0);

        room.process_input(&mut app, key('G'));
        assert_eq!(app.room.active.chat.selected_message(), Some(19));
        assert_eq!(app.room.active.chat.scroll_offset(), 0);
    }

    #[test]
    fn chat_log_selection_change_scrolls_selected_header_into_view() {
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
        room.process_input(&mut app, key('k'));
        render_room(&mut app, &mut room, &mut buffer);

        let selected = app
            .room
            .active
            .chat
            .selected_message()
            .expect("selected header");
        assert_eq!(selected, 12);
        assert!(
            room.layout()
                .visible_chat_lines
                .iter()
                .any(|line| line.kind == LineKind::Heading && line.block_contains(selected)),
            "selected header must be visible after movement"
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
        assert!(app.room.active.chat.is_collapsed(0));

        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert!(app.room.active.chat.is_expanded(0));
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
            app.room.active.chat.is_collapsed(0),
            "long message starts collapsed"
        );

        // Expand the long message and scroll to the very top so its heading sits
        // at the top of the viewport. Collapsing now removes fewer rows than the
        // viewport height, which lands the scroll offset in the window where
        // `visible_lines` does not self-correct.
        app.room.active.chat.toggle_expand(0, width);
        assert!(app.room.active.chat.is_expanded(0));
        app.room.active.chat.top(width, height);
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
            app.room.active.chat.is_collapsed(0),
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
    fn y_copies_selected_log_when_no_lines_are_selected() {
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
        app.room.active.chat.select_first_header();
        room.process_input(&mut app, key('y'));

        assert_eq!(
            app.room.take_pending_clipboard().as_deref(),
            Some("first\nsecond")
        );
        assert_eq!(app.status.text(), "copied to clipboard");
        assert_eq!(app.status.kind(), crate::app::StatusKind::Info);
    }

    #[test]
    fn mouse_down_on_chat_text_focuses_chat_log_and_selects_message() {
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
        assert_eq!(app.room.active.chat.selected_message(), Some(line.message));
        assert!(app.room.active.chat.is_selecting());
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
            "L",
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
            app.theme.status_fill,
        );
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
        assert_eq!(app.status.text(), "no users in the current room yet");
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

        assert_eq!(app.status.text(), "focus users list to adjust volume");
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
        let mut app = test_app();
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

        assert_eq!(app.room.composer.text(), "a\nb");
    }
}
