use extui::Buffer;
use extui::event::{KeyEvent, KeyEventKind, MouseEvent};
use extui_bindings::{InputKey, LayerId};
use extui_editor::Mode as EditorMode;

use crate::{
    app::{App, ChatPanelFocus, ServerEditDraft, ServerEditEvent},
    bindings::{self, BindCommand, Resolved},
    settings::SettingsFocus,
    theme,
    tui::{
        form::{FormAction, FormMouseIntent, FormState},
        mode::{AppMode, is_quit_key},
        overlay::ConfirmMode,
    },
    ui::select::FuzzySelect,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Continue,
    Quit,
}

fn resolve_binding(app: &mut App, layer: LayerId, key: KeyEvent) -> Option<BindCommand> {
    let input = InputKey::from_event(&key)?;
    match bindings::resolve(
        &app.config.bindings.router,
        layer,
        &mut app.pending_chord,
        input,
    ) {
        Resolved::Action(id) => Some(app.config.bindings.actions.get(id).clone()),
        Resolved::Consumed | Resolved::Unmatched => None,
    }
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

    fn selected_alias(&self, app: &App) -> Option<String> {
        self.select
            .current_item_index()
            .and_then(|index| app.server_items.get(index))
            .map(|item| item.alias.clone())
    }

    pub(crate) fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            Activate => {
                let Some(alias) = self.selected_alias(app) else {
                    app.set_error("no server selected");
                    return Action::Continue;
                };
                if app.start_network(&alias) {
                    app.push_mode(Box::new(RoomMode));
                }
            }
            SelectNext => {
                self.select.move_selection(1);
            }
            SelectPrev => {
                self.select.move_selection(-1);
            }
            EditServer => {
                let Some(alias) = self.selected_alias(app) else {
                    app.set_error("no server selected");
                    return Action::Continue;
                };
                app.open_server_edit(&alias);
            }
            DeleteServer => {
                let Some(alias) = self.selected_alias(app) else {
                    app.set_error("no server selected");
                    return Action::Continue;
                };
                let prompt = format!("Delete server '{alias}'?");
                app.push_mode(Box::new(ConfirmMode::new(
                    prompt,
                    "Delete",
                    "Cancel",
                    move |app| app.delete_server(&alias),
                )));
            }
            SearchServers => {
                self.searching = true;
                self.select.clear_query();
                self.select.refresh(&app.server_items);
            }
            Cancel if app.network.is_some() => app.push_mode(Box::new(RoomMode)),
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
        self.select.refresh(&app.server_items);
        let mode = self.theme_mode(app);
        let label = self.status_label(app);
        let layer = self.layer_id(app);
        crate::tui::render::draw_server_select_screen(
            app,
            &mut self.select,
            self.searching,
            mode,
            label,
            layer,
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
                _ if self.select.edit_query(key) => self.select.refresh(&app.server_items),
                _ => {}
            }
            return Action::Continue;
        }
        resolve_binding(app, bindings::PICKER_LAYER, key)
            .map(|command| self.process_action(app, command))
            .unwrap_or(Action::Continue)
    }

    fn theme_mode(&self, _app: &App) -> theme::UiMode {
        theme::UiMode::ServerSelect
    }

    fn status_label(&self, _app: &App) -> &'static str {
        "Servers"
    }

    fn layer_id(&self, _app: &App) -> LayerId {
        bindings::PICKER_LAYER
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
        let mode = self.theme_mode(app);
        let label = self.status_label(app);
        let layer = self.layer_id(app);
        crate::tui::render::draw_server_edit_screen(app, &mut self.draft, mode, label, layer, buf);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        let event = self.draft.handle_key(key);
        self.handle_event(app, event);
        Action::Continue
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        let event = self.draft.handle_mouse(mouse);
        self.handle_event(app, event);
        Action::Continue
    }

    fn theme_mode(&self, _app: &App) -> theme::UiMode {
        theme::UiMode::ServerEdit
    }

    fn status_label(&self, _app: &App) -> &'static str {
        "Server"
    }

    fn layer_id(&self, _app: &App) -> LayerId {
        bindings::FORM_LAYER
    }
}

pub(crate) struct SettingsMode {
    form: FormState<SettingsFocus>,
}

impl SettingsMode {
    pub(crate) fn new(form: FormState<SettingsFocus>) -> Self {
        Self { form }
    }

    pub(crate) fn form_mut(&mut self) -> &mut FormState<SettingsFocus> {
        &mut self.form
    }

    fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            SaveSettings => app.save_settings(&mut self.form),
            Activate => app.activate_settings_focus(&mut self.form),
            FocusNext => app.move_settings_focus(&mut self.form, 1),
            FocusPrev => app.move_settings_focus(&mut self.form, -1),
            SelectNext => app.move_settings_selection(&mut self.form, 1),
            SelectPrev => app.move_settings_selection(&mut self.form, -1),
            AdjustLeft => app.adjust_settings_focus(&mut self.form, -1),
            AdjustRight => app.adjust_settings_focus(&mut self.form, 1),
            Cancel | CloseSettings => {
                if !app.cancel_open_audio_picker() {
                    app.close_settings(&mut self.form);
                }
            }
            RefreshDevices => app.refresh_audio_devices(),
            _ => return app.process_global_command(command),
        }
        Action::Continue
    }

    fn resolve_binding(&mut self, app: &mut App, key: KeyEvent) -> Action {
        resolve_binding(app, bindings::SETTINGS_LAYER, key)
            .map(|command| self.process_action(app, command))
            .unwrap_or(Action::Continue)
    }
}

impl AppMode for SettingsMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let mode = self.theme_mode(app);
        let label = self.status_label(app);
        let layer = self.layer_id(app);
        crate::tui::render::draw_settings_screen(app, self, mode, label, layer, buf);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if app.handle_open_settings_picker_key(self.form.focus(), key) {
            return Action::Continue;
        }

        let focus = self.form.focus();
        let kind = app.settings.field_kind(focus);
        let text_focused = kind == crate::tui::form::FormFieldKind::Text;
        let event = self.form.handle_key(key, kind);
        app.apply_settings_commit(&mut self.form, event.commit);
        match event.action {
            FormAction::None if !text_focused => return self.resolve_binding(app, key),
            FormAction::None => {}
            FormAction::Cancel => {
                if !app.cancel_open_audio_picker() {
                    app.close_settings(&mut self.form);
                }
            }
            FormAction::Activate => app.activate_settings_focus(&mut self.form),
            FormAction::Adjust(delta) => app.adjust_settings_focus(&mut self.form, delta),
            FormAction::FocusMoved | FormAction::Scrolled => {}
            FormAction::TextChanged => app.mark_settings_dirty(),
        }
        Action::Continue
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        if app.process_top_bar_mouse(mouse) {
            return Action::Continue;
        }
        if app.handle_open_settings_picker_mouse(self.form.focus(), mouse) {
            return Action::Continue;
        }

        let event = self.form.handle_mouse(mouse);
        app.apply_settings_commit(&mut self.form, event.commit);
        match event.intent {
            FormMouseIntent::None => {}
            FormMouseIntent::Activate(field) => {
                let _ = self.form.set_focus(field);
                app.activate_settings_focus(&mut self.form);
            }
            FormMouseIntent::Adjust(field, delta) => {
                let commit = self.form.set_focus(field);
                app.apply_settings_commit(&mut self.form, commit);
                app.adjust_settings_focus(&mut self.form, delta);
            }
            FormMouseIntent::Text(field, area, column) => {
                let value = app.settings.buffer_text(field);
                let commit = self.form.focus_text_at(field, &value, area, column, true);
                app.apply_settings_commit(&mut self.form, commit);
            }
            FormMouseIntent::PickerItem(field, item_index) => {
                app.activate_settings_picker_item(field, item_index);
            }
        }
        Action::Continue
    }

    fn theme_mode(&self, _app: &App) -> theme::UiMode {
        theme::UiMode::Settings
    }

    fn status_label(&self, _app: &App) -> &'static str {
        "Settings"
    }

    fn layer_id(&self, _app: &App) -> LayerId {
        bindings::SETTINGS_LAYER
    }
}

pub(crate) struct RoomMode;

impl RoomMode {
    fn process_action(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            EnterCompose => app.enter_compose_insert_mode(),
            EnterLog => app.set_chat_panel_focus(ChatPanelFocus::ChatLog),
            SubmitMessage => app.submit_input(),
            Cancel => {
                if app.chat_focus == ChatPanelFocus::Compose {
                    app.composer.clear();
                }
                app.enter_compose_insert_mode();
            }
            ScrollUp => app.scroll_focused_panel(-1, 1),
            ScrollDown => app.scroll_focused_panel(1, 1),
            RoomScrollUp => app.move_room_selection_with_focus(-1),
            RoomScrollDown => app.move_room_selection_with_focus(1),
            OpenSelectedUserVolume => {
                if app.chat_focus == ChatPanelFocus::Lobby {
                    app.open_selected_user_volume();
                } else {
                    app.set_status("focus lobby to adjust users");
                }
            }
            ToggleSelectedUserMute => {
                if app.chat_focus == ChatPanelFocus::Lobby {
                    app.toggle_selected_user_mute();
                } else {
                    app.set_status("focus lobby to mute users");
                }
            }
            HalfPageUp => {
                app.scroll_chat_log_if_focused(-(app.chat_half_page_rows() as isize));
            }
            HalfPageDown => {
                app.scroll_chat_log_if_focused(app.chat_half_page_rows() as isize);
            }
            Top => app.select_chat_top(),
            Bottom => app.select_chat_bottom(),
            CopySelection => app.copy_chat_selection_if_focused(),
            ToggleExpand => app.toggle_chat_expand_if_focused(),
            FocusNext => app.move_chat_panel_focus(1),
            FocusPrev => app.move_chat_panel_focus(-1),
            SelectNext if app.chat_focus == ChatPanelFocus::Lobby => {
                app.move_room_selection_with_focus(1);
            }
            SelectPrev if app.chat_focus == ChatPanelFocus::Lobby => {
                app.move_room_selection_with_focus(-1);
            }
            ClearChat if app.chat_focus == ChatPanelFocus::ChatLog => app.chat.clear(),
            _ => return app.process_global_command(command),
        }
        Action::Continue
    }

    fn process_compose_key(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if app.composer.mode() == EditorMode::Insert {
            if key.code != extui::event::KeyCode::Esc
                && let Some(command) = resolve_binding(app, bindings::INSERT_LAYER, key)
            {
                return self.process_action(app, command);
            }
            let _ = app.composer.send_key(&key);
            return Action::Continue;
        }

        if let Some(command) = resolve_binding(app, bindings::COMPOSE_NORMAL_LAYER, key) {
            return self.process_action(app, command);
        }
        let _ = app.composer.send_key(&key);
        Action::Continue
    }
}

impl AppMode for RoomMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64) {
        let mode = self.theme_mode(app);
        let label = self.status_label(app);
        let layer = self.layer_id(app);
        crate::tui::render::draw_room_screen(app, mode, label, layer, buf, now_ms);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if app.chat_focus == ChatPanelFocus::Compose {
            return self.process_compose_key(app, key);
        }
        resolve_binding(app, bindings::WORKSPACE_LAYER, key)
            .map(|command| self.process_action(app, command))
            .unwrap_or(Action::Continue)
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        if app.process_top_bar_mouse(mouse) {
            return Action::Continue;
        }
        app.process_chat_mouse(mouse)
    }

    fn process_paste(&mut self, app: &mut App, text: String) {
        if app.chat_focus == ChatPanelFocus::Compose {
            app.insert_composer_paste(text);
        }
    }

    fn theme_mode(&self, app: &App) -> theme::UiMode {
        if app.chat_focus == ChatPanelFocus::Compose {
            theme::UiMode::Compose
        } else {
            theme::UiMode::Log
        }
    }

    fn status_label(&self, _app: &App) -> &'static str {
        "Compose"
    }

    fn layer_id(&self, app: &App) -> LayerId {
        if app.chat_focus != ChatPanelFocus::Compose {
            bindings::WORKSPACE_LAYER
        } else if app.composer.mode() == EditorMode::Insert {
            bindings::INSERT_LAYER
        } else {
            bindings::COMPOSE_NORMAL_LAYER
        }
    }
}
