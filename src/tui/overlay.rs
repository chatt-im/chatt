use extui::{
    Buffer, Ellipsis, HAlign, Rect,
    event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    vt::Modifier,
};
use extui_bindings::InputKey;

use crate::{
    app::{App, AppEvent, UserVolumeDialog},
    bindings::{self, BindCommand, Resolved},
    client_net::NetworkEvent,
    theme,
    theme::Theme,
    tui::{
        Action,
        form::rect_contains,
        mode::{AppMode, ChromeSpec, Coverage, ModePresentation, ModeTransition, is_quit_key},
    },
};

/// Overlay hosting the per-user local volume dialog.
///
/// The mode owns the [`UserVolumeDialog`]; event handling lives on [`App`] so it
/// can reach the audio and config state the dialog mutates.
pub(crate) struct DialogMode {
    dialog: UserVolumeDialog,
}

impl DialogMode {
    pub(crate) fn new(dialog: UserVolumeDialog) -> Self {
        Self { dialog }
    }
}

impl AppMode for DialogMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        self.dialog.render(buf.rect(), buf, &app.theme);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        let event = self.dialog.handle_key(key);
        if app.apply_volume_event(event, &mut self.dialog) {
            app.pop_mode();
        }
        Action::Continue
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
        ModePresentation::OVERLAY
    }
}

pub(crate) enum ConfirmDisposition {
    Close,
    Transition(ModeTransition),
}

/// Reusable yes/no confirmation overlay.
///
/// The confirm action runs the stored callback against [`App`], then pops the
/// overlay. Cancel just pops. The cancel button is selected by default so a
/// stray `Enter` does not trigger a destructive action.
pub(crate) struct ConfirmMode {
    prompt: String,
    confirm_label: String,
    cancel_label: String,
    selected_confirm: bool,
    on_confirm: Option<Box<dyn FnOnce(&mut App) -> ConfirmDisposition>>,
}

impl ConfirmMode {
    pub(crate) fn new(
        prompt: impl Into<String>,
        confirm_label: impl Into<String>,
        cancel_label: impl Into<String>,
        on_confirm: impl FnOnce(&mut App) -> ConfirmDisposition + 'static,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            confirm_label: confirm_label.into(),
            cancel_label: cancel_label.into(),
            selected_confirm: false,
            on_confirm: Some(Box::new(on_confirm)),
        }
    }

    fn confirm(&mut self, app: &mut App) {
        let disposition = self
            .on_confirm
            .take()
            .map(|callback| callback(app))
            .unwrap_or(ConfirmDisposition::Close);
        match disposition {
            ConfirmDisposition::Close => app.pop_mode(),
            ConfirmDisposition::Transition(transition) => {
                app.request_mode_transition(transition);
            }
        }
    }

    fn render_buttons(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let mut row = area;
        let cancel = row.take_left((row.w / 2) as i32);
        let confirm = row;
        let selected = theme.selected_focused;
        let idle = theme.dialog_panel.patch(theme.muted);
        let (cancel_style, confirm_style) = if self.selected_confirm {
            (idle, selected)
        } else {
            (selected, idle)
        };
        draw_button(cancel, buf, cancel_style, &self.cancel_label);
        draw_button(confirm, buf, confirm_style, &self.confirm_label);
    }
}

impl AppMode for ConfirmMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let theme = &app.theme;
        let area = buf.rect();
        if area.w < 24 || area.h < 5 {
            return;
        }
        let width = area.w.min(54).max(24);
        let height = area.h.min(5);
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        buf.clear_rect(panel, theme.dialog_panel);

        let mut rows = panel.inset(2, 1);
        rows.take_top(1)
            .with(theme.dialog_header | Modifier::BOLD)
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, &self.prompt);
        rows.take_top(1);
        self.render_buttons(rows.take_top(1), buf, theme);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => app.pop_mode(),
            KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm(app),
            KeyCode::Enter => {
                if self.selected_confirm {
                    self.confirm(app);
                } else {
                    app.pop_mode();
                }
            }
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('h')
            | KeyCode::Char('l') => self.selected_confirm = !self.selected_confirm,
            _ => {}
        }
        Action::Continue
    }

    fn process_mouse(&mut self, _app: &mut App, _mouse: MouseEvent) -> Action {
        Action::Continue
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
        ModePresentation::OVERLAY
    }
}

/// Transient overlay prompting for the open-pairing password.
///
/// The entered password is held verbatim but rendered as `*` characters. Enter
/// submits it to the pairing worker, Esc cancels pairing.
pub(crate) struct PasswordPromptMode {
    input: String,
    show_password: bool,
    submitting: bool,
    feedback: PasswordFeedback,
    cancel_button: Rect,
    submit_button: Rect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PasswordFeedback {
    Info(String),
    Error(String),
}

impl PasswordPromptMode {
    pub(crate) fn new(retry: bool) -> Self {
        let mut mode = Self {
            input: String::new(),
            show_password: false,
            submitting: false,
            feedback: PasswordFeedback::Info("Enter the server password".to_string()),
            cancel_button: Rect::EMPTY,
            submit_button: Rect::EMPTY,
        };
        mode.apply_password_challenge(retry);
        mode
    }

    fn submit(&mut self, app: &mut App) {
        if self.submitting {
            return;
        }
        let password = std::mem::take(&mut self.input);
        self.submitting = true;
        self.feedback = PasswordFeedback::Info("Checking password...".to_string());
        app.submit_open_pair_password(password);
    }

    fn apply_password_challenge(&mut self, retry: bool) {
        self.submitting = false;
        self.input.clear();
        self.feedback = if retry {
            PasswordFeedback::Error("Incorrect password; try again".to_string())
        } else {
            PasswordFeedback::Info("Enter the server password".to_string())
        };
    }

    fn apply_error(&mut self, error: String) {
        self.submitting = false;
        self.input.clear();
        self.feedback = PasswordFeedback::Error(error);
    }

    fn feedback_line(&self) -> (&str, bool) {
        match &self.feedback {
            PasswordFeedback::Info(message) => (message, false),
            PasswordFeedback::Error(message) => (message, true),
        }
    }

    fn process_command(&mut self, app: &mut App, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            Cancel => app.cancel_open_pairing(),
            SubmitPassword | Activate => self.submit(app),
            TogglePasswordVisibility => self.show_password = !self.show_password,
            command => return app.process_global_command(command),
        }
        Action::Continue
    }

    fn push_char(&mut self, key: KeyEvent) {
        if self.submitting {
            return;
        }
        let mut modifiers = key.modifiers;
        modifiers.remove(KeyModifiers::SHIFT);
        if !modifiers.is_empty() {
            return;
        }
        match key.code {
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(ch) if !ch.is_control() => self.input.push(ch),
            _ => {}
        }
    }
}

impl AppMode for PasswordPromptMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let theme = &app.theme;
        let area = buf.rect();
        if area.w < 28 || area.h < 9 {
            return;
        }
        let width = area.w.min(58).max(28);
        let height = area.h.min(9);
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        buf.clear_rect(panel, theme.dialog_panel);

        let mut rows = panel;
        let prompt = "Server password required";
        rows.take_top(1)
            .with(theme.dialog_header | Modifier::BOLD)
            .fill(buf)
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, prompt);
        let mut body = rows.inset(2, 1);
        body.take_top(1);
        self.render_input_row(body.take_top(1), buf, theme);
        body.take_top(1);
        let (feedback, is_error) = self.feedback_line();
        let feedback_style = if is_error { theme.error } else { theme.subtle };
        body.take_top(1)
            .with(theme.dialog_panel.patch(feedback_style))
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, feedback);
        let visibility = if self.show_password {
            "Password visible"
        } else {
            "Password hidden"
        };
        if body.h > 0 {
            body.take_top(1)
                .with(theme.dialog_panel.patch(theme.subtle))
                .with(HAlign::Center)
                .with(Ellipsis(true))
                .text(buf, visibility);
        }
        if body.h > 0 {
            self.render_buttons(body.take_top(1), app, buf, theme);
        }
        crate::tui::render::draw_overlay_key_preview(app, bindings::PASSWORD_LAYER, buf);
    }

    fn process_app_event(&mut self, app: &mut App, event: AppEvent) -> Option<AppEvent> {
        match event {
            AppEvent::Network(NetworkEvent::OpenPairingNeedsPassword {
                retry,
                server_public_key,
            }) => {
                match app.accept_open_pairing_password_challenge(server_public_key) {
                    Ok(()) => self.apply_password_challenge(retry),
                    Err(error) => self.apply_error(error),
                }
                None
            }
            AppEvent::Network(NetworkEvent::OpenPairingSucceeded {
                token,
                server_public_key,
                udp_addr,
                udp_probe_addr,
            }) => {
                self.submitting = false;
                app.complete_open_pairing_from_password_prompt(
                    token,
                    server_public_key,
                    udp_addr,
                    udp_probe_addr,
                );
                None
            }
            AppEvent::Network(NetworkEvent::PairingFailed(error)) => {
                self.apply_error(error);
                None
            }
            event => Some(event),
        }
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if let Some(input) = InputKey::from_event(&key) {
            match bindings::resolve(
                &app.config.bindings.router,
                bindings::PASSWORD_LAYER,
                &mut app.chrome.binding.pending_chord,
                input,
            ) {
                Resolved::Action(id) => {
                    let command = app.config.bindings.actions.get(id).clone();
                    return self.process_command(app, command);
                }
                Resolved::Consumed => return Action::Continue,
                Resolved::Unmatched => {}
            }
        }
        self.push_char(key);
        Action::Continue
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Continue;
        }
        if rect_contains(self.cancel_button, mouse.column, mouse.row) {
            app.cancel_open_pairing();
        } else if rect_contains(self.submit_button, mouse.column, mouse.row) {
            self.submit(app);
        }
        Action::Continue
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
        ModePresentation {
            coverage: Coverage::Overlay,
            chrome: Some(ChromeSpec {
                theme_mode: theme::UiMode::ServerSelect,
                status_label: "Password",
                layer: bindings::PASSWORD_LAYER,
            }),
        }
    }
}

impl PasswordPromptMode {
    fn render_input_row(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            return;
        }

        let mut row = area;
        row.take_left(10)
            .with(theme.dialog_panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, "Password");

        let field = row;
        field.with(theme.join_input_active).fill(buf);
        let shown = if self.show_password {
            self.input.clone()
        } else {
            "*".repeat(self.input.chars().count())
        };
        field
            .with(theme.join_input_active)
            .with(Ellipsis(true))
            .text(buf, &shown);
    }

    fn render_buttons(&mut self, area: Rect, app: &App, buf: &mut Buffer, theme: &Theme) {
        if area.is_empty() {
            self.cancel_button = Rect::EMPTY;
            self.submit_button = Rect::EMPTY;
            return;
        }

        let mut row = area;
        let gap = u16::from(row.w > 20);
        let button_width = row.w.saturating_sub(gap) / 2;
        self.cancel_button = row.take_left(button_width as i32);
        if gap > 0 {
            row.take_left(gap as i32).with(theme.dialog_panel).fill(buf);
        }
        self.submit_button = row;

        let cancel = button_label(
            "Cancel",
            command_key_hint(&app.config.bindings, BindCommand::Cancel),
        );
        let submit = button_label(
            "Submit",
            command_key_hint(&app.config.bindings, BindCommand::SubmitPassword),
        );
        draw_button(
            self.cancel_button,
            buf,
            theme.dialog_panel.patch(theme.muted),
            &cancel,
        );
        draw_button(self.submit_button, buf, theme.selected_focused, &submit);
    }
}

fn command_key_hint(bindings: &bindings::BindingRuntime, target: BindCommand) -> Option<String> {
    let pending = None;
    for reachable in bindings::reachable(bindings, bindings::PASSWORD_LAYER, &pending) {
        let bindings::ReachableKind::Action(command) = reachable.kind else {
            continue;
        };
        if std::mem::discriminant(&command) == std::mem::discriminant(&target) {
            return Some(reachable.key.to_string());
        }
    }
    None
}

fn button_label(label: &str, key: Option<String>) -> String {
    match key {
        Some(key) => format!("{label} [{key}]"),
        None => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{PairCompletion, PendingPair},
        config::{Config, ServerEntry},
    };
    use extui::event::{KeyModifiers, MouseButton, MouseEventKind};

    fn test_app() -> App {
        App::new(Config::default(), None).expect("test app")
    }

    fn pending_open_pair() -> PendingPair {
        PendingPair {
            server: ServerEntry {
                label: "public".to_string(),
                tcp_addr: "chat.example.com:443".to_string(),
                udp_addr: String::new(),
                udp_probe_addr: None,
                username: "Zoe".to_string(),
                token: String::new(),
                server_public_key: String::new(),
                room_id: 1,
            },
            open: Some(String::new()),
            completion: PairCompletion::OpenEditor,
        }
    }

    #[test]
    fn password_prompt_treats_picker_shortcuts_as_text() {
        let mut app = test_app();
        let mut prompt = PasswordPromptMode::new(false);

        prompt.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()),
        );
        prompt.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::empty()),
        );

        assert_eq!(prompt.input, "q/");
    }

    #[test]
    fn password_prompt_visibility_toggle_uses_password_binding() {
        let mut app = test_app();
        let mut prompt = PasswordPromptMode::new(false);

        prompt.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );

        assert!(prompt.show_password);
    }

    #[test]
    fn password_prompt_buttons_use_password_bindings() {
        let app = test_app();

        assert_eq!(
            button_label(
                "Cancel",
                command_key_hint(&app.config.bindings, BindCommand::Cancel)
            ),
            "Cancel [Esc]"
        );
        assert_eq!(
            button_label(
                "Submit",
                command_key_hint(&app.config.bindings, BindCommand::SubmitPassword)
            ),
            "Submit [Enter]"
        );
    }

    #[test]
    fn password_prompt_cancel_button_is_clickable() {
        let mut app = test_app();
        app.pending_pair = Some(pending_open_pair());
        let mut prompt = PasswordPromptMode::new(false);
        prompt.cancel_button = Rect {
            x: 2,
            y: 3,
            w: 12,
            h: 1,
        };

        prompt.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: 3,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert!(app.pending_pair.is_none());
        assert!(!app.pending_transition.is_empty());
    }

    #[test]
    fn password_prompt_consumes_retry_as_local_feedback() {
        let mut app = test_app();
        app.pending_pair = Some(pending_open_pair());
        let mut prompt = PasswordPromptMode::new(false);
        prompt.input = "bad".to_string();
        prompt.submitting = true;
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        let unhandled = prompt.process_app_event(
            &mut app,
            AppEvent::Network(NetworkEvent::OpenPairingNeedsPassword {
                retry: true,
                server_public_key: key.to_string(),
            }),
        );

        assert!(unhandled.is_none());
        assert_eq!(
            prompt.feedback,
            PasswordFeedback::Error("Incorrect password; try again".to_string())
        );
        assert!(!prompt.submitting);
        assert!(prompt.input.is_empty());
        assert_eq!(
            app.pending_pair.as_ref().unwrap().server.server_public_key,
            key
        );
        assert!(app.pending_transition.is_empty());
    }
}

fn draw_button(area: Rect, buf: &mut Buffer, style: extui::Style, label: &str) {
    area.with(style).fill(buf);
    area.with(style)
        .with(HAlign::Center)
        .with(Ellipsis(true))
        .text(buf, label);
}
