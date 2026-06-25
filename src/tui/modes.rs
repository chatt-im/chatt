use extui::event::{KeyCode, KeyEvent, KeyModifiers};
use extui_bindings::InputKey;
use extui_editor::Mode as EditorMode;

use crate::{
    app::{App, ChatPanelFocus},
    bindings::{self, Resolved},
    theme,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Continue,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModeKind {
    ServerSelect,
    ServerEdit,
    Workspace,
    Insert,
    Settings,
    Dialog,
}

impl ModeKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ModeKind::ServerSelect => "Servers",
            ModeKind::ServerEdit => "Server",
            ModeKind::Workspace => "Workspace",
            ModeKind::Insert => "Insert",
            ModeKind::Settings => "Settings",
            ModeKind::Dialog => "Dialog",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ModeStack {
    stack: Vec<ModeKind>,
}

impl ModeStack {
    pub(crate) fn new(initial: ModeKind) -> Self {
        Self {
            stack: vec![initial],
        }
    }

    pub(crate) fn top(&self) -> ModeKind {
        self.stack.last().copied().unwrap_or(ModeKind::Workspace)
    }

    pub(crate) fn set(&mut self, mode: ModeKind) {
        self.stack.clear();
        self.stack.push(mode);
    }

    pub(crate) fn push(&mut self, mode: ModeKind) {
        self.stack.push(mode);
    }

    pub(crate) fn pop_or(&mut self, fallback: ModeKind) {
        if self.stack.len() > 1 {
            self.stack.pop();
        } else {
            self.set(fallback);
        }
    }
}

impl From<theme::UiMode> for ModeKind {
    fn from(mode: theme::UiMode) -> Self {
        match mode {
            theme::UiMode::ServerSelect => ModeKind::ServerSelect,
            theme::UiMode::ServerEdit => ModeKind::ServerEdit,
            theme::UiMode::Compose => ModeKind::Insert,
            theme::UiMode::Log => ModeKind::Workspace,
            theme::UiMode::Settings => ModeKind::Settings,
        }
    }
}

pub(crate) fn process_key(app: &mut App, key: KeyEvent) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }

    if app.mode == theme::UiMode::ServerSelect {
        return app.process_server_select_key(key);
    }
    if app.mode == theme::UiMode::ServerEdit {
        return app.process_server_edit_key(key);
    }
    if app.mode == theme::UiMode::Settings {
        return app.process_settings_key(key);
    }

    if app.handle_volume_dialog_key(key) {
        return Action::Continue;
    }

    if matches!(app.mode, theme::UiMode::Compose | theme::UiMode::Log) {
        return process_chat_key(app, key);
    }

    Action::Continue
}

fn process_chat_key(app: &mut App, key: KeyEvent) -> Action {
    match app.chat_focus {
        ChatPanelFocus::Compose => process_compose_key(app, key),
        ChatPanelFocus::ChatLog | ChatPanelFocus::Lobby => {
            resolve_app_binding(app, bindings::WORKSPACE_LAYER, key).unwrap_or(Action::Continue)
        }
    }
}

fn process_compose_key(app: &mut App, key: KeyEvent) -> Action {
    if app.composer.mode() == EditorMode::Insert {
        if key.code != KeyCode::Esc
            && let Some(action) = resolve_app_binding(app, bindings::INSERT_LAYER, key)
        {
            return action;
        }

        let _ = app.composer.send_key(&key);
        app.refresh_mode_and_focus();
        return Action::Continue;
    }

    if let Some(action) = resolve_app_binding(app, bindings::COMPOSE_NORMAL_LAYER, key) {
        return action;
    }

    let _ = app.composer.send_key(&key);
    app.refresh_mode_and_focus();
    Action::Continue
}

fn resolve_app_binding(
    app: &mut App,
    base: extui_bindings::LayerId,
    key: KeyEvent,
) -> Option<Action> {
    if let Some(input) = InputKey::from_event(&key) {
        match bindings::resolve(
            &app.config.bindings.router,
            base,
            &mut app.pending_chord,
            input,
        ) {
            Resolved::Action(id) => {
                let command = app.config.bindings.actions.get(id).clone();
                return Some(app.process_command(command));
            }
            Resolved::Consumed => return Some(Action::Continue),
            Resolved::Unmatched => {}
        }
    }
    None
}
