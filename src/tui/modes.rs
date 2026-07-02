use extui::event::{KeyEvent, KeyEventKind, MouseEvent};
use extui::{Buffer, Rect};
use extui_bindings::{InputKey, LayerId};
use extui_editor::Mode as EditorMode;

use crate::{
    app::{App, ChatPanelFocus, ServerEditDraft, ServerEditEvent, ToggleExpandResult},
    bindings::{self, BindCommand, Resolved},
    chat_buffer::{LineKind, VisibleLine},
    settings::{self, AudioInputPickerState, AudioOutputPickerState, SettingsDraft},
    theme,
    tui::{
        form::{FormAction, FormFieldKind, FormMouseIntent, FormState},
        mode::{AppMode, ChromeSpec, ExitReason, ModePresentation, ModeTransition, is_quit_key},
        overlay::{ConfirmDisposition, ConfirmMode},
    },
    ui::{
        select::FuzzySelect,
        settings::{FieldId, FieldIntent},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Continue,
    Quit,
}

#[derive(Debug)]
enum BindingResolution {
    Action(BindCommand),
    Consumed,
    Unmatched,
}

fn resolve_binding(app: &mut App, layer: LayerId, key: KeyEvent) -> BindingResolution {
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
        let chrome = self.presentation(app).chrome.expect("base mode has chrome");
        crate::tui::render::draw_server_edit_screen(
            app,
            &mut self.draft,
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
        ModePresentation::full_screen(ChromeSpec {
            theme_mode: theme::UiMode::ServerEdit,
            status_label: "Server",
            layer: bindings::FORM_LAYER,
        })
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
        draft.set_form_bindings_from_config(app.config.ui.form_bindings);
        draft.set_theme_from_config(app.config.ui.theme);
        draft.set_web_from_config(&app.config.web);
        draft.set_notifications_from_config(&app.config.notifications);
        let input_items = settings::audio_input_items(app.audio_devices.input_devices());
        let output_items = settings::audio_output_items(app.audio_devices.output_devices());
        let mut input_picker = AudioInputPickerState::default();
        input_picker.reset(&input_items, draft.input_selection());
        let mut output_picker = AudioOutputPickerState::default();
        output_picker.reset(&output_items, draft.output_selection());
        Self {
            form: FormState::new(
                crate::ui::settings::initial_focus(),
                app.config.ui.form_bindings,
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
    pub(crate) chat_width: u16,
    pub(crate) chat_height: u16,
    pub(crate) chat_rect: Rect,
    pub(crate) visible_chat_lines: Vec<VisibleLine>,
    pub(crate) room_rect: Rect,
    pub(crate) lobby_bar_rect: Rect,
    pub(crate) chat_log_bar_rect: Rect,
    pub(crate) composer_rect: Rect,
    pub(crate) compose_bar_rect: Rect,
}

impl Default for RoomLayout {
    fn default() -> Self {
        Self {
            chat_width: 80,
            chat_height: 0,
            chat_rect: Rect::EMPTY,
            visible_chat_lines: Vec::new(),
            room_rect: Rect::EMPTY,
            lobby_bar_rect: Rect::EMPTY,
            chat_log_bar_rect: Rect::EMPTY,
            composer_rect: Rect::EMPTY,
            compose_bar_rect: Rect::EMPTY,
        }
    }
}

impl RoomLayout {
    pub(crate) fn clear_workspace(&mut self) {
        self.room_rect = Rect::EMPTY;
        self.lobby_bar_rect = Rect::EMPTY;
        self.chat_log_bar_rect = Rect::EMPTY;
        self.composer_rect = Rect::EMPTY;
        self.compose_bar_rect = Rect::EMPTY;
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
            layout: RoomLayout::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_focus(focus: ChatPanelFocus) -> Self {
        Self {
            focus,
            layout: RoomLayout::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn focus(&self) -> ChatPanelFocus {
        self.focus
    }

    #[cfg(test)]
    pub(crate) fn layout(&self) -> &RoomLayout {
        &self.layout
    }

    pub(crate) fn set_focus(&mut self, app: &mut App, focus: ChatPanelFocus) {
        self.focus = focus;
        match focus {
            ChatPanelFocus::Lobby => self.keep_selected_room_user_visible(app),
            ChatPanelFocus::ChatLog => {
                app.room.chat.ensure_selected_header(self.layout.chat_width);
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
            RoomScrollUp => self.move_room_selection_with_focus(app, -1),
            RoomScrollDown => self.move_room_selection_with_focus(app, 1),
            OpenSelectedUserVolume => {
                if self.focus == ChatPanelFocus::Lobby {
                    app.open_selected_user_volume();
                } else {
                    app.set_status("focus lobby to adjust users");
                }
            }
            ToggleSelectedUserMute => {
                if self.focus == ChatPanelFocus::Lobby {
                    app.toggle_selected_user_mute();
                } else {
                    app.set_status("focus lobby to mute users");
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
            ToggleExpand => self.toggle_chat_expand_if_focused(app),
            FocusNext => self.move_focus(app, 1),
            FocusPrev => self.move_focus(app, -1),
            SelectNext if self.focus == ChatPanelFocus::Lobby => {
                self.move_room_selection_with_focus(app, 1);
            }
            SelectPrev if self.focus == ChatPanelFocus::Lobby => {
                self.move_room_selection_with_focus(app, -1);
            }
            ClearChat if self.focus == ChatPanelFocus::ChatLog => app.room.clear_chat(),
            PasteClipboard => {
                self.paste_from_clipboard(app, &crate::clipboard_paste::HelperClipboard)
            }
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
            let _ = app.room.composer.send_key(&key);
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
        let in_room =
            crate::tui::form::rect_contains(self.layout.room_rect, mouse.column, mouse.row);
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
            extui::event::MouseEventKind::ScrollUp if in_room => {
                self.move_room_selection_with_focus(app, -1);
            }
            extui::event::MouseEventKind::ScrollDown if in_room => {
                self.move_room_selection_with_focus(app, 1);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_room => {
                self.set_focus(app, ChatPanelFocus::Lobby);
                let row = mouse.row.saturating_sub(self.layout.room_rect.y) as usize;
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
                app.room.chat.scroll_down(5);
            }
            extui::event::MouseEventKind::Down(extui::event::MouseButton::Left) if in_chat => {
                self.set_focus(app, ChatPanelFocus::ChatLog);
                match self.layout.chat_line_at(mouse.row) {
                    Some(line) => match line.kind {
                        LineKind::Heading | LineKind::Ellipsis => {
                            app.room
                                .chat
                                .select_header_containing(line.message, self.layout.chat_width);
                            app.room
                                .chat
                                .toggle_expand(line.message, self.layout.chat_width);
                            self.keep_selected_chat_header_visible(app);
                            app.room.chat.clear_selection();
                        }
                        LineKind::Body => {
                            app.room.chat.begin_selection((line.message, line.line));
                        }
                    },
                    _ => app.room.chat.clear_selection(),
                }
            }
            extui::event::MouseEventKind::Drag(extui::event::MouseButton::Left)
                if app.room.chat.is_selecting() =>
            {
                self.set_focus(app, ChatPanelFocus::ChatLog);
                self.drag_chat_selection(app, mouse.row);
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left) if in_chat => {
                // A collapsed selection (press and release without a drag) over a
                // URL opens it; a drag remains a text selection.
                if app.room.chat.selection_is_click()
                    && let Some(url) = self.chat_link_at(app, mouse.column, mouse.row)
                {
                    app.room.request_open_url(url);
                }
                app.room.chat.end_selection();
            }
            extui::event::MouseEventKind::Up(extui::event::MouseButton::Left) => {
                app.room.chat.end_selection();
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
            .chat
            .link_at(line.message, line.line, col_in_line)
            .map(str::to_owned)
    }

    fn drag_chat_selection(&mut self, app: &mut App, row: u16) {
        let rect = self.layout.chat_rect;
        if row < rect.y {
            self.scroll_chat_up(app, 1);
        } else if row >= rect.y.saturating_add(rect.h) {
            app.room.chat.scroll_down(1);
        }
        let clamped = row.clamp(rect.y, rect.y.saturating_add(rect.h).saturating_sub(1));
        if let Some(line) = self.layout.chat_line_at(clamped)
            && line.kind == LineKind::Body
        {
            app.room.chat.extend_selection((line.message, line.line));
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
            .chat
            .scroll_up(rows, self.layout.chat_width, self.layout.chat_height);
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
                .chat
                .top(self.layout.chat_width, self.layout.chat_height);
            app.room.chat.select_first_header();
            app.room.chat.clear_selection();
            self.keep_selected_chat_header_visible(app);
        }
    }

    fn select_chat_bottom(&mut self, app: &mut App) {
        if self.focus == ChatPanelFocus::ChatLog {
            app.room.chat.bottom();
            app.room.chat.select_last_header(self.layout.chat_width);
            app.room.chat.clear_selection();
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
            ChatPanelFocus::Lobby => self.move_room_selection_with_focus(app, direction),
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
            app.room.chat.scroll_down(rows as usize);
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
            .chat
            .keep_selected_header_visible(self.layout.chat_width, self.layout.chat_height);
    }

    fn move_room_selection_with_focus(&mut self, app: &mut App, delta: isize) {
        self.set_focus(app, ChatPanelFocus::Lobby);
        self.move_room_selection(app, delta);
    }

    fn move_room_selection(&mut self, app: &mut App, delta: isize) {
        if app.room.move_participant_selection(delta).is_none() {
            app.set_status("no users in the current room yet");
            return;
        }
        self.keep_selected_room_user_visible(app);
        self.focus = ChatPanelFocus::Lobby;
    }

    fn keep_selected_room_user_visible(&mut self, app: &mut App) {
        let visible_rows = self.layout.room_rect.h.max(app.config.ui.room_height) as usize;
        app.room.keep_selected_participant_visible(visible_rows);
    }
}

impl AppMode for RoomMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64) {
        let chrome = self.presentation(app).chrome.expect("base mode has chrome");
        crate::tui::render::draw_room_screen(
            app,
            self.focus,
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
    use extui::{
        Buffer,
        event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    };
    use extui_editor::Mode as EditorMode;
    use rpc::{
        control::{ChatMessage, ParticipantInfo, ParticipantVoiceStatus},
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

    fn push_room_message(
        app: &mut App,
        message_id: u64,
        sender: UserId,
        timestamp_ms: u64,
        body: impl Into<String>,
    ) {
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

    fn participant(user_id: UserId, display_name: &str) -> ParticipantInfo {
        ParticipantInfo {
            user_id,
            display_name: display_name.to_string(),
            identifier: display_name.to_string(),
            in_call: true,
            voice_status: ParticipantVoiceStatus::default(),
            joined_at_ms: 0,
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
        assert_eq!(app.room.chat.selected_message(), Some(2));

        room.process_input(&mut app, key('k'));
        assert_eq!(app.room.chat.selected_message(), Some(1));

        room.process_input(&mut app, key('j'));
        assert_eq!(app.room.chat.selected_message(), Some(2));
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
        assert_eq!(app.room.chat.selected_message(), Some(0));
        assert!(app.room.chat.scroll_offset() > 0);

        room.process_input(&mut app, key('G'));
        assert_eq!(app.room.chat.selected_message(), Some(19));
        assert_eq!(app.room.chat.scroll_offset(), 0);
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

        let selected = app.room.chat.selected_message().expect("selected header");
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
        assert!(app.room.chat.is_collapsed(0));

        room.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert!(app.room.chat.is_expanded(0));
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
            app.room.chat.is_collapsed(0),
            "long message starts collapsed"
        );

        // Expand the long message and scroll to the very top so its heading sits
        // at the top of the viewport. Collapsing now removes fewer rows than the
        // viewport height, which lands the scroll offset in the window where
        // `visible_lines` does not self-correct.
        app.room.chat.toggle_expand(0, width);
        assert!(app.room.chat.is_expanded(0));
        app.room.chat.top(width, height);
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
            app.room.chat.is_collapsed(0),
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
        app.room.chat.select_first_header();
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
        assert_eq!(app.room.chat.selected_message(), Some(line.message));
        assert!(app.room.chat.is_selecting());
    }

    #[test]
    fn mouse_down_on_lobby_row_focuses_lobby_and_selects_user() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.room.joined(
            RoomId(1),
            vec![
                participant(UserId(1), "alice"),
                participant(UserId(2), "bob"),
            ],
            Vec::new(),
            Some(UserId(1)),
        );

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let column = room.layout().room_rect.x + 1;
        let row = room.layout().room_rect.y;
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
        assert_eq!(app.room.participants.selected_user, Some(UserId(1)));
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
