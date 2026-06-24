use extui::event::{KeyCode, KeyEvent, KeyModifiers};
use extui_bindings::{InputKey, LayerId};

use crate::{
    app::App,
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
            ModeKind::ServerSelect => "SERVERS",
            ModeKind::ServerEdit => "SERVER",
            ModeKind::Workspace => "WORKSPACE",
            ModeKind::Insert => "INSERT",
            ModeKind::Settings => "SETTINGS",
            ModeKind::Dialog => "DIALOG",
        }
    }

    pub(crate) fn binding_layer(self) -> LayerId {
        match self {
            ModeKind::Insert => bindings::INSERT_LAYER,
            ModeKind::Settings => bindings::SETTINGS_LAYER,
            ModeKind::ServerSelect => bindings::PICKER_LAYER,
            ModeKind::ServerEdit => bindings::FORM_LAYER,
            ModeKind::Dialog => bindings::DIALOG_LAYER,
            ModeKind::Workspace => bindings::WORKSPACE_LAYER,
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

    if app.handle_volume_dialog_key(key) {
        return Action::Continue;
    }

    if app.handle_settings_search_key(key) {
        return Action::Continue;
    }

    if app.handle_settings_buffer_key(key) {
        return Action::Continue;
    }

    let base = ModeKind::from(app.mode).binding_layer();
    if let Some(input) = InputKey::from_event(&key) {
        match bindings::resolve(
            &app.config.bindings.router,
            base,
            &mut app.pending_chord,
            input,
        ) {
            Resolved::Action(id) => {
                let command = app.config.bindings.actions.get(id).clone();
                return app.process_command(command);
            }
            Resolved::Consumed => return Action::Continue,
            Resolved::Unmatched => {}
        }
    }

    match app.mode {
        theme::UiMode::Compose => {
            let _ = app.composer.send_key(&key);
        }
        theme::UiMode::Log
        | theme::UiMode::Settings
        | theme::UiMode::ServerSelect
        | theme::UiMode::ServerEdit => {}
    }
    Action::Continue
}
