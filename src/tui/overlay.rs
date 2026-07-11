use extui::{
    Buffer, Ellipsis, HAlign, Rect,
    event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    vt::Modifier,
};
use extui_bindings::InputKey;
use extui_editor::{Editor, Mode as EditorMode, Span as EditorSpan};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{UserVolumeDialog, command::CoreCommand},
    bindings::{self, BindCommand, Resolved},
    clipboard_paste::{ImagePaste, ImagePasteOrigin, ImagePasteSource},
    theme,
    theme::Theme,
    tui::{
        Action,
        form::rect_contains,
        mode::{
            AppMode, ChromeSpec, Coverage, ModePresentation, ModeTransition, ViewCx, is_quit_key,
        },
        modes::process_global_command_cx,
        widgets::{RowPalette, button_label, draw_button, draw_labeled_editor_frame},
    },
};

#[cfg(test)]
use crate::app::App;

/// Overlay hosting the per-user local volume dialog.
///
/// The mode owns the [`UserVolumeDialog`]; event handling lives on [`App`] so it
/// can reach the audio and config state the dialog mutates.
pub(crate) struct DialogMode {
    dialog: Option<UserVolumeDialog>,
}

impl DialogMode {
    pub(crate) fn new(dialog: UserVolumeDialog) -> Self {
        Self {
            dialog: Some(dialog),
        }
    }
}

impl AppMode for DialogMode {
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, _now_ms: u64) {
        let app = crate::tui::render::RenderState::new(cx);
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.render(buf.rect(), buf, &app.view.theme);
        }
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        let Some(mut dialog) = self.dialog.take() else {
            return Action::Continue;
        };
        let event = dialog.handle_key(key);
        cx.send(CoreCommand::ApplyVolume { event, dialog });
        Action::Continue
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation::OVERLAY
    }
}

pub(crate) enum ConfirmDisposition {
    Close,
    Transition(ModeTransition),
}

/// Reusable yes/no confirmation overlay.
///
/// The confirm action runs the stored callback against [`ViewCx`], then pops the
/// overlay. Cancel just pops. The cancel button is selected by default so a
/// stray `Enter` does not trigger a destructive action.
pub(crate) struct ConfirmMode {
    prompt: String,
    confirm_label: String,
    cancel_label: String,
    selected_confirm: bool,
    cancel_button: Rect,
    confirm_button: Rect,
    on_confirm: Option<Box<dyn FnOnce(&mut ViewCx<'_>) -> ConfirmDisposition + Send>>,
}

impl ConfirmMode {
    pub(crate) fn new(
        prompt: impl Into<String>,
        confirm_label: impl Into<String>,
        cancel_label: impl Into<String>,
        on_confirm: impl FnOnce(&mut ViewCx<'_>) -> ConfirmDisposition + Send + 'static,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            confirm_label: confirm_label.into(),
            cancel_label: cancel_label.into(),
            selected_confirm: false,
            cancel_button: Rect::EMPTY,
            confirm_button: Rect::EMPTY,
            on_confirm: Some(Box::new(on_confirm)),
        }
    }

    fn confirm(&mut self, cx: &mut ViewCx<'_>) {
        let disposition = self
            .on_confirm
            .take()
            .map(|callback| callback(cx))
            .unwrap_or(ConfirmDisposition::Close);
        match disposition {
            ConfirmDisposition::Close => cx.request_transition(ModeTransition::Pop),
            ConfirmDisposition::Transition(transition) => {
                cx.request_transition(transition);
            }
        }
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                cx.request_transition(ModeTransition::Pop)
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm(cx),
            KeyCode::Enter => {
                if self.selected_confirm {
                    self.confirm(cx);
                } else {
                    cx.request_transition(ModeTransition::Pop);
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

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Continue;
        }
        if rect_contains(self.cancel_button, mouse.column, mouse.row) {
            cx.request_transition(ModeTransition::Pop);
        } else if rect_contains(self.confirm_button, mouse.column, mouse.row) {
            self.confirm(cx);
        }
        Action::Continue
    }

    fn render_buttons(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let mut row = area;
        self.cancel_button = row.take_left((row.w / 2) as i32);
        self.confirm_button = row;
        let selected = theme.selected_focused;
        let idle = theme.dialog_panel.patch(theme.muted);
        let (cancel_style, confirm_style) = if self.selected_confirm {
            (idle, selected)
        } else {
            (selected, idle)
        };
        draw_button(self.cancel_button, buf, cancel_style, &self.cancel_label);
        draw_button(self.confirm_button, buf, confirm_style, &self.confirm_label);
    }
}

impl AppMode for ConfirmMode {
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, _now_ms: u64) {
        let app = crate::tui::render::RenderState::new(cx);
        let theme = &app.view.theme;
        let area = buf.rect();
        if area.w < 24 || area.h < 5 {
            self.cancel_button = Rect::EMPTY;
            self.confirm_button = Rect::EMPTY;
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

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        self.process_mouse_cx(cx, mouse)
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation::OVERLAY
    }
}

/// Safety gate for connecting to a server that selected plaintext
/// ExternalSecureLink transport.
pub(crate) struct NativeEncryptionWarningMode {
    label: String,
    cancel_button: Rect,
    connect_button: Rect,
}

impl NativeEncryptionWarningMode {
    pub(crate) fn new(label: String) -> Self {
        Self {
            label,
            cancel_button: Rect::EMPTY,
            connect_button: Rect::EMPTY,
        }
    }

    fn accept(&mut self, cx: &mut ViewCx<'_>) {
        cx.send(CoreCommand::AcceptNativeEncryption(self.label.clone()));
    }

    fn cancel(&mut self, cx: &mut ViewCx<'_>) {
        cx.send(CoreCommand::CancelNativeEncryption);
    }

    fn process_command(&mut self, cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            Cancel => self.cancel(cx),
            Activate => self.accept(cx),
            command => return process_global_command_cx(cx, command),
        }
        Action::Continue
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if let Some(input) = InputKey::from_event(&key) {
            match bindings::resolve(
                &cx.config.bindings.router,
                bindings::DIALOG_LAYER,
                &mut cx.view.chrome.binding.pending_chord,
                input,
            ) {
                Resolved::Action(id) => {
                    let command = cx.config.bindings.actions.get(id).clone();
                    return self.process_command(cx, command);
                }
                Resolved::Consumed => return Action::Continue,
                Resolved::Unmatched => {}
            }
        }
        Action::Continue
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Continue;
        }
        if rect_contains(self.cancel_button, mouse.column, mouse.row) {
            self.cancel(cx);
        } else if rect_contains(self.connect_button, mouse.column, mouse.row) {
            self.accept(cx);
        }
        Action::Continue
    }

    fn render_buttons(
        &mut self,
        area: Rect,
        app: &crate::tui::render::RenderState<'_>,
        buf: &mut Buffer,
        theme: &Theme,
    ) {
        if area.is_empty() {
            self.cancel_button = Rect::EMPTY;
            self.connect_button = Rect::EMPTY;
            return;
        }

        let mut row = area;
        let gap = u16::from(row.w > 20);
        let button_width = row.w.saturating_sub(gap) / 2;
        self.cancel_button = row.take_left(button_width as i32);
        if gap > 0 {
            row.take_left(gap as i32).with(theme.dialog_panel).fill(buf);
        }
        self.connect_button = row;

        let cancel = button_label(
            "Cancel",
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::DIALOG_LAYER,
                BindCommand::Cancel,
            ),
        );
        let connect = button_label(
            "Connect",
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::DIALOG_LAYER,
                BindCommand::Activate,
            ),
        );
        draw_button(
            self.cancel_button,
            buf,
            theme.dialog_panel.patch(theme.muted),
            &cancel,
        );
        draw_button(self.connect_button, buf, theme.selected_focused, &connect);
    }
}

impl AppMode for NativeEncryptionWarningMode {
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, _now_ms: u64) {
        let mut app = crate::tui::render::RenderState::new(cx);
        let theme = &app.view.theme;
        let area = buf.rect();
        if area.w < 34 || area.h < 11 {
            return;
        }
        let width = area.w.min(68).max(34);
        let height = area.h.min(12);
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        buf.clear_rect(panel, theme.dialog_panel);

        let mut rows = panel;
        rows.take_top(1)
            .with(native_encryption_header_style(theme) | Modifier::BOLD)
            .fill(buf)
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, "No Native Encryption to Server");

        let mut body = rows.inset(2, 1);
        body.take_top(1)
            .with(theme.dialog_panel.patch(theme.error | Modifier::BOLD))
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, "Chatt will not encrypt this connection");
        body.take_top(1);
        body.take_top(1)
            .with(theme.dialog_panel.patch(theme.text))
            .with(Ellipsis(true))
            .text(
                buf,
                "The server selected plaintext ExternalSecureLink transport.",
            );
        body.take_top(1)
            .with(theme.dialog_panel.patch(theme.text))
            .with(Ellipsis(true))
            .text(
                buf,
                "Connect only when another secure link already protects it.",
            );
        body.take_top(1)
            .with(theme.dialog_panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(
                buf,
                "Examples: WireGuard, SSH tunnel, or a private trusted network.",
            );
        body.take_top(1);
        body.take_top(1)
            .with(theme.dialog_panel.patch(theme.subtle))
            .with(Ellipsis(true))
            .text(
                buf,
                &format!(
                    "Accepting saves require-native-encryption = false for {}.",
                    self.label
                ),
            );
        if body.h > 0 {
            body.take_top(1);
        }
        if body.h > 0 {
            self.render_buttons(body.take_top(1), &app, buf, theme);
        }
        crate::tui::render::draw_overlay_key_preview(&mut app, bindings::DIALOG_LAYER, buf);
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
                status_label: "Security",
                layer: bindings::DIALOG_LAYER,
            }),
        }
    }
}

fn native_encryption_header_style(theme: &Theme) -> extui::Style {
    if let Some(error) = theme.error.fg() {
        theme.dialog_header.with_bg(error).with_fg_rgb(0, 0, 0)
    } else {
        theme.dialog_header.patch(theme.error).with_fg_rgb(0, 0, 0)
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

    fn submit(&mut self, cx: &mut ViewCx<'_>) {
        if self.submitting {
            return;
        }
        let password = std::mem::take(&mut self.input);
        self.submitting = true;
        self.feedback = PasswordFeedback::Info("Checking password...".to_string());
        cx.send(CoreCommand::SubmitPairPassword(password));
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

    fn process_command(&mut self, cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            Cancel => cx.send(CoreCommand::CancelPairing),
            SubmitPassword | Activate => self.submit(cx),
            TogglePasswordVisibility => self.show_password = !self.show_password,
            command => return process_global_command_cx(cx, command),
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
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, _now_ms: u64) {
        let mut app = crate::tui::render::RenderState::new(cx);
        let theme = &app.view.theme;
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
            self.render_buttons(body.take_top(1), &app, buf, theme);
        }
        crate::tui::render::draw_overlay_key_preview(&mut app, bindings::PASSWORD_LAYER, buf);
    }

    fn process_client_event(&mut self, event: crate::client_channel::ClientEvent) {
        use crate::client_channel::ClientEvent;

        match event {
            ClientEvent::SetError(_) => {}
            ClientEvent::OpenPairingPasswordChallenge { retry } => {
                self.apply_password_challenge(retry);
            }
            ClientEvent::PairingPasswordChallenge { retry, .. } => {
                self.apply_password_challenge(retry);
            }
            ClientEvent::PairingSucceeded => self.submitting = false,
            ClientEvent::PairingFailed(error) => self.apply_error(error),
        }
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if let Some(input) = InputKey::from_event(&key) {
            match bindings::resolve(
                &cx.config.bindings.router,
                bindings::PASSWORD_LAYER,
                &mut cx.view.chrome.binding.pending_chord,
                input,
            ) {
                Resolved::Action(id) => {
                    let command = cx.config.bindings.actions.get(id).clone();
                    return self.process_command(cx, command);
                }
                Resolved::Consumed => return Action::Continue,
                Resolved::Unmatched => {}
            }
        }
        self.push_char(key);
        Action::Continue
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Continue;
        }
        if rect_contains(self.cancel_button, mouse.column, mouse.row) {
            cx.send(CoreCommand::CancelPairing);
        } else if rect_contains(self.submit_button, mouse.column, mouse.row) {
            self.submit(cx);
        }
        Action::Continue
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
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

    fn render_buttons(
        &mut self,
        area: Rect,
        app: &crate::tui::render::RenderState<'_>,
        buf: &mut Buffer,
        theme: &Theme,
    ) {
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
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::PASSWORD_LAYER,
                BindCommand::Cancel,
            ),
        );
        let submit = button_label(
            "Submit",
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::PASSWORD_LAYER,
                BindCommand::SubmitPassword,
            ),
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

/// Confirmation dialog for uploading a pasted image or file.
///
/// The filename field is a real single-line [`Editor`] seeded with a default
/// name. `Enter` uploads under the edited name, `Esc` cancels and removes any
/// staged temp file. The upload pipeline takes over once confirmed.
pub(crate) struct PasteImageUploadMode {
    editor: Editor,
    source: Option<ImagePasteSource>,
    origin: ImagePasteOrigin,
    dimensions: Option<(u32, u32)>,
    size: Option<u64>,
    default_name: String,
    error: Option<String>,
    cancel_button: Rect,
    upload_button: Rect,
}

impl PasteImageUploadMode {
    pub(crate) fn new(image: ImagePaste, theme: &Theme) -> Self {
        let size = std::fs::metadata(image.source.path())
            .ok()
            .map(|metadata| metadata.len());
        let editor = filename_editor(theme);
        Self {
            editor,
            source: Some(image.source),
            origin: image.origin,
            dimensions: image.dimensions,
            size,
            default_name: image.default_name,
            error: None,
            cancel_button: Rect::EMPTY,
            upload_button: Rect::EMPTY,
        }
    }

    /// The upload name: the edited field, or the default when left blank, with
    /// the default extension appended when the typed name has none.
    fn finalize_name(&self) -> String {
        let typed = self.editor.text();
        let trimmed = typed.trim();
        let base = if trimmed.is_empty() {
            self.default_name.trim()
        } else {
            trimmed
        };
        let mut name = base.to_string();
        if std::path::Path::new(&name).extension().is_none() {
            if let Some(extension) = std::path::Path::new(&self.default_name)
                .extension()
                .and_then(|extension| extension.to_str())
            {
                name = format!("{name}.{extension}");
            }
        }
        name
    }

    /// The muted suffix shown after the typed text: the full default name when
    /// the field is empty, or the extension that would be appended, or nothing
    /// when the user already typed an extension.
    fn ghost_text(&self, typed: &str) -> String {
        if typed.is_empty() {
            return self.default_name.clone();
        }
        if std::path::Path::new(typed).extension().is_some() {
            return String::new();
        }
        match std::path::Path::new(&self.default_name)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some(extension) => format!(".{extension}"),
            None => String::new(),
        }
    }

    fn submit(&mut self, cx: &mut ViewCx<'_>) {
        if cx.session.active_server_label.is_none() {
            self.error = Some("select a server before uploading files".to_string());
            return;
        }
        let name = crate::client_net::sanitize_file_name(&self.finalize_name());
        if name.len() > rpc::control::MAX_FILE_NAME_BYTES {
            self.error = Some("file name is too long".to_string());
            return;
        }
        let Some(source) = self.source.take() else {
            return;
        };
        cx.send(CoreCommand::UploadPastedImage {
            room_id: cx.view.viewed_room,
            source,
            raw_name: name,
        });
        cx.request_transition(ModeTransition::Pop);
    }

    fn cancel(&mut self, cx: &mut ViewCx<'_>) {
        if let Some(source) = self.source.as_ref()
            && source.is_staged()
        {
            let _ = std::fs::remove_file(source.path());
        }
        cx.request_transition(ModeTransition::Pop);
    }

    fn process_command(&mut self, cx: &mut ViewCx<'_>, command: BindCommand) -> Action {
        match command {
            BindCommand::Cancel => self.cancel(cx),
            BindCommand::Activate => self.submit(cx),
            command => return process_global_command_cx(cx, command),
        }
        Action::Continue
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if key.code == KeyCode::Esc && self.editor.mode() == EditorMode::Insert {
            self.editor.send_key(&key);
            return Action::Continue;
        }
        if let Some(input) = InputKey::from_event(&key) {
            match bindings::resolve(
                &cx.config.bindings.router,
                bindings::PASTE_LAYER,
                &mut cx.view.chrome.binding.pending_chord,
                input,
            ) {
                Resolved::Action(id) => {
                    let command = cx.config.bindings.actions.get(id).clone();
                    return self.process_command(cx, command);
                }
                Resolved::Consumed => return Action::Continue,
                Resolved::Unmatched => {}
            }
        }
        if self.editor.send_key(&key) {
            self.error = None;
        }
        Action::Continue
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Continue;
        }
        if rect_contains(self.cancel_button, mouse.column, mouse.row) {
            self.cancel(cx);
        } else if rect_contains(self.upload_button, mouse.column, mouse.row) {
            self.submit(cx);
        }
        Action::Continue
    }

    fn metadata_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some((width, height)) = self.dimensions {
            parts.push(format!("{width}x{height}"));
        }
        if let Some(size) = self.size {
            parts.push(format_size(size));
        }
        parts.push(self.origin.label().to_string());
        parts.join("  ·  ")
    }

    fn render_buttons(
        &mut self,
        area: Rect,
        app: &crate::tui::render::RenderState<'_>,
        buf: &mut Buffer,
        theme: &Theme,
    ) {
        if area.is_empty() {
            self.cancel_button = Rect::EMPTY;
            self.upload_button = Rect::EMPTY;
            return;
        }

        let mut row = area;
        let gap = u16::from(row.w > 20);
        let button_width = row.w.saturating_sub(gap) / 2;
        self.cancel_button = row.take_left(button_width as i32);
        if gap > 0 {
            row.take_left(gap as i32).with(theme.dialog_panel).fill(buf);
        }
        self.upload_button = row;

        let cancel = button_label(
            "Cancel",
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::PASTE_LAYER,
                BindCommand::Cancel,
            ),
        );
        let upload = button_label(
            "Upload",
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::PASTE_LAYER,
                BindCommand::Activate,
            ),
        );
        draw_button(
            self.cancel_button,
            buf,
            theme.dialog_panel.patch(theme.muted),
            &cancel,
        );
        draw_button(self.upload_button, buf, theme.selected_focused, &upload);
    }
}

impl AppMode for PasteImageUploadMode {
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, _now_ms: u64) {
        let mut app = crate::tui::render::RenderState::new(cx);
        let theme = &app.view.theme;
        let area = buf.rect();
        if area.w < 28 || area.h < 7 {
            return;
        }
        let width = area.w.min(58).max(28);
        let height = area.h.min(7);
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        buf.clear_rect(panel, theme.dialog_panel);

        let mut rows = panel;
        rows.take_top(1)
            .with(theme.dialog_header | Modifier::BOLD)
            .fill(buf)
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, "Upload image");

        let mut body = rows.inset(2, 1);
        body.take_top(1)
            .with(theme.dialog_panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, &self.metadata_line());

        let field = draw_labeled_editor_frame(
            body.take_top(1),
            buf,
            theme,
            RowPalette::dialog(theme),
            6,
            "Name",
            true,
            self.error.is_some(),
        );
        self.editor.render(field, buf);
        let typed = self.editor.text();
        let ghost = self.ghost_text(&typed);
        if !ghost.is_empty() {
            let mut rest = field;
            rest.take_left(UnicodeWidthStr::width(typed.as_str()) as i32);
            rest.with(theme.join_input_active.patch(theme.muted))
                .with(Ellipsis(true))
                .text(buf, &ghost);
        }

        // One padding line above the buttons, reused to show a validation error.
        if body.h > 0 {
            let row = body.take_top(1);
            if let Some(error) = &self.error {
                row.with(theme.dialog_panel.patch(theme.error))
                    .with(Ellipsis(true))
                    .text(buf, error);
            }
        }
        if body.h > 0 {
            self.render_buttons(body.take_top(1), &app, buf, theme);
        }
        crate::tui::render::draw_overlay_key_preview(&mut app, bindings::PASTE_LAYER, buf);
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.process_input_cx(cx, key)
    }

    fn process_paste(&mut self, _cx: &mut ViewCx<'_>, text: String) {
        let span = EditorSpan::empty_at(self.editor.cursor_offset());
        self.editor.replace_range(span, text.trim());
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        self.process_mouse_cx(cx, mouse)
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        ModePresentation {
            coverage: Coverage::Overlay,
            chrome: Some(ChromeSpec {
                theme_mode: theme::UiMode::Compose,
                status_label: "Upload",
                layer: bindings::PASTE_LAYER,
            }),
        }
    }
}

#[cfg(test)]
macro_rules! app_mode_test_bridge {
    ($($mode:ty),+ $(,)?) => {
        $(
            #[allow(dead_code)]
            impl $mode {
                fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64) {
                    {
                        let mut cx = app.view_cx();
                        AppMode::render(self, &mut cx, buf, now_ms);
                    }
                    app.drain_core_commands();
                }

                fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
                    let action = {
                        let mut cx = app.view_cx();
                        AppMode::process_input(self, &mut cx, key)
                    };
                    app.drain_core_commands();
                    action
                }

                fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
                    let action = {
                        let mut cx = app.view_cx();
                        AppMode::process_mouse(self, &mut cx, mouse)
                    };
                    app.drain_core_commands();
                    action
                }
            }
        )+
    };
}

#[cfg(test)]
app_mode_test_bridge!(
    DialogMode,
    ConfirmMode,
    NativeEncryptionWarningMode,
    PasswordPromptMode,
    PasteImageUploadMode,
);

#[cfg(test)]
impl PasteImageUploadMode {
    fn presentation(&self, _app: &App) -> ModePresentation {
        ModePresentation {
            coverage: Coverage::Overlay,
            chrome: Some(ChromeSpec {
                theme_mode: theme::UiMode::Compose,
                status_label: "Upload",
                layer: bindings::PASTE_LAYER,
            }),
        }
    }
}

fn filename_editor(theme: &Theme) -> Editor {
    let mut editor = Editor::new();
    editor.set_single_line(true);
    editor.set_wrap(false);
    editor.set_height_bounds(1, 1);
    editor.set_theme(theme.join_input_editor_theme());
    editor.enter_insert_mode();
    editor
}

/// Formats a byte count as a short human-readable size for dialog metadata.
fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use super::*;
    use crate::{
        app::{PairCompletion, PendingPair},
        config::{Config, ServerEntry},
    };
    use extui::event::{KeyModifiers, MouseButton, MouseEventKind};

    fn test_app() -> App {
        App::new(Config::default(), None).expect("test app")
    }

    #[test]
    fn confirmation_buttons_are_clickable() {
        let mut app = test_app();
        let confirmed = Arc::new(AtomicBool::new(false));
        let confirmed_by_callback = Arc::clone(&confirmed);
        let mut mode = ConfirmMode::new("Delete message?", "Delete", "Cancel", move |_| {
            confirmed_by_callback.store(true, Ordering::Relaxed);
            ConfirmDisposition::Close
        });
        mode.render(&mut app, &mut Buffer::new(80, 24), 0);
        let confirm = mode.confirm_button;

        mode.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: confirm.x,
                row: confirm.y,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert!(confirmed.load(Ordering::Relaxed));
        assert!(!app.view.pending_transition.is_empty());
    }

    #[test]
    fn confirmation_cancel_button_does_not_run_callback() {
        let mut app = test_app();
        let confirmed = Arc::new(AtomicBool::new(false));
        let confirmed_by_callback = Arc::clone(&confirmed);
        let mut mode = ConfirmMode::new("Delete message?", "Delete", "Cancel", move |_| {
            confirmed_by_callback.store(true, Ordering::Relaxed);
            ConfirmDisposition::Close
        });
        mode.render(&mut app, &mut Buffer::new(80, 24), 0);
        let cancel = mode.cancel_button;

        mode.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: cancel.x,
                row: cancel.y,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert!(!confirmed.load(Ordering::Relaxed));
        assert!(!app.view.pending_transition.is_empty());
    }

    fn image_paste(source: ImagePasteSource, default_name: &str) -> ImagePaste {
        ImagePaste {
            source,
            default_name: default_name.to_string(),
            dimensions: Some((2, 2)),
            origin: ImagePasteOrigin::ClipboardImageData,
        }
    }

    #[test]
    fn paste_dialog_finalize_name_defaults_when_blank() {
        let app = test_app();
        let mut mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::ExistingPath("/tmp/pic.png".into()),
                "pic.png",
            ),
            &app.view.theme,
        );
        mode.editor.set_lines("");
        assert_eq!(mode.finalize_name(), "pic.png");
    }

    #[test]
    fn paste_dialog_appends_missing_extension() {
        let app = test_app();
        let mut mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::StagedFile("/tmp/staged.png".into()),
                "clipboard-image.png",
            ),
            &app.view.theme,
        );
        mode.editor.set_lines("holiday");
        assert_eq!(mode.finalize_name(), "holiday.png");
        mode.editor.set_lines("holiday.jpg");
        assert_eq!(mode.finalize_name(), "holiday.jpg");
    }

    #[test]
    fn paste_dialog_ghost_text_shows_default_then_extension() {
        let app = test_app();
        let mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::StagedFile("/tmp/staged.png".into()),
                "clipboard.png",
            ),
            &app.view.theme,
        );
        assert_eq!(mode.ghost_text(""), "clipboard.png");
        assert_eq!(mode.ghost_text("cat"), ".png");
        assert_eq!(mode.ghost_text("cat.apng"), "");
    }

    #[test]
    fn paste_dialog_submit_without_network_keeps_open() {
        let mut app = test_app();
        let mut mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::ExistingPath("/tmp/pic.png".into()),
                "pic.png",
            ),
            &app.view.theme,
        );
        {
            let mut cx = app.view_cx();
            mode.submit(&mut cx);
        }
        assert!(mode.error.is_some());
        assert!(app.view.pending_transition.is_empty());
    }

    #[test]
    fn paste_dialog_esc_leaves_insert_before_cancelling() {
        let mut app = test_app();
        let mut mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::ExistingPath("/tmp/pic.png".into()),
                "pic.png",
            ),
            &app.view.theme,
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());

        // First Esc: editor leaves insert mode, dialog stays open.
        mode.process_input(&mut app, esc);
        assert_eq!(mode.editor.mode(), EditorMode::Normal);
        assert!(app.view.pending_transition.is_empty());

        // Second Esc: from normal mode this cancels the dialog.
        mode.process_input(&mut app, esc);
        assert!(!app.view.pending_transition.is_empty());
    }

    #[test]
    fn paste_dialog_cancel_removes_staged_file() {
        let mut app = test_app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staged.png");
        std::fs::write(&path, b"bytes").unwrap();
        let mut mode = PasteImageUploadMode::new(
            image_paste(ImagePasteSource::StagedFile(path.clone()), "staged.png"),
            &app.view.theme,
        );
        {
            let mut cx = app.view_cx();
            mode.cancel(&mut cx);
        }
        assert!(!path.exists());
        assert!(!app.view.pending_transition.is_empty());
    }

    #[test]
    fn paste_dialog_uses_paste_layer_and_binding_hints() {
        let app = test_app();
        let mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::ExistingPath("/tmp/pic.png".into()),
                "pic.png",
            ),
            &app.view.theme,
        );
        let chrome = mode.presentation(&app).chrome.expect("dialog chrome");
        assert!(chrome.layer == bindings::PASTE_LAYER);
        assert_eq!(
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::PASTE_LAYER,
                BindCommand::Cancel
            ),
            Some("Esc".to_string())
        );
        assert_eq!(
            bindings::command_key_hint(
                &app.config.bindings,
                bindings::PASTE_LAYER,
                BindCommand::Activate
            ),
            Some("Enter".to_string())
        );
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
                ..ServerEntry::default()
            },
            open: Some(String::new()),
            completion: PairCompletion::OpenEditor,
        }
    }

    #[test]
    fn native_encryption_warning_header_uses_black_on_error_fill() {
        let black = extui::Style::DEFAULT.with_fg_rgb(0, 0, 0).fg();
        for theme in [
            Theme::tomorrow_night(),
            Theme::base16_dark(),
            Theme::base16_light(),
        ] {
            let style = native_encryption_header_style(&theme);
            assert_eq!(style.bg(), theme.error.fg());
            assert_eq!(style.fg(), black);
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
                bindings::command_key_hint(
                    &app.config.bindings,
                    bindings::PASSWORD_LAYER,
                    BindCommand::Cancel
                )
            ),
            "Cancel [Esc]"
        );
        assert_eq!(
            button_label(
                "Submit",
                bindings::command_key_hint(
                    &app.config.bindings,
                    bindings::PASSWORD_LAYER,
                    BindCommand::SubmitPassword
                )
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
        assert!(!app.view.pending_transition.is_empty());
    }

    #[test]
    fn password_prompt_consumes_retry_as_local_feedback() {
        let mut prompt = PasswordPromptMode::new(false);
        prompt.input = "bad".to_string();
        prompt.submitting = true;

        prompt.process_client_event(
            crate::client_channel::ClientEvent::PairingPasswordChallenge { retry: true },
        );

        assert_eq!(
            prompt.feedback,
            PasswordFeedback::Error("Incorrect password; try again".to_string())
        );
        assert!(!prompt.submitting);
        assert!(prompt.input.is_empty());
    }
}
