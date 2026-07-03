use extui::{
    Buffer, Ellipsis, HAlign, Rect,
    event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    vt::Modifier,
};
use extui_bindings::{InputKey, LayerId};
use extui_editor::{Editor, Mode as EditorMode, Span as EditorSpan};
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, AppEvent, UserVolumeDialog},
    bindings::{self, BindCommand, Resolved},
    client_net::NetworkEvent,
    clipboard_paste::{ImagePaste, ImagePasteOrigin, ImagePasteSource},
    theme,
    theme::Theme,
    tui::{
        Action,
        form::rect_contains,
        mode::{AppMode, ChromeSpec, Coverage, ModePresentation, ModeTransition, is_quit_key},
        widgets::draw_labeled_editor_frame,
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
            command_key_hint(
                &app.config.bindings,
                bindings::PASSWORD_LAYER,
                BindCommand::Cancel,
            ),
        );
        let submit = button_label(
            "Submit",
            command_key_hint(
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
    source: ImagePasteSource,
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
            source: image.source,
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

    fn submit(&mut self, app: &mut App) {
        let name = self.finalize_name();
        match app.confirm_paste_image_upload(&self.source, name) {
            Ok(()) => app.pop_mode(),
            Err(error) => self.error = Some(error),
        }
    }

    fn cancel(&mut self, app: &mut App) {
        if self.source.is_staged() {
            let _ = std::fs::remove_file(self.source.path());
        }
        app.pop_mode();
    }

    fn process_command(&mut self, app: &mut App, command: BindCommand) -> Action {
        match command {
            BindCommand::Cancel => self.cancel(app),
            BindCommand::Activate => self.submit(app),
            command => return app.process_global_command(command),
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

    fn render_buttons(&mut self, area: Rect, app: &App, buf: &mut Buffer, theme: &Theme) {
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
            command_key_hint(
                &app.config.bindings,
                bindings::PASTE_LAYER,
                BindCommand::Cancel,
            ),
        );
        let upload = button_label(
            "Upload",
            command_key_hint(
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
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let theme = &app.theme;
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
            self.render_buttons(body.take_top(1), app, buf, theme);
        }
        crate::tui::render::draw_overlay_key_preview(app, bindings::PASTE_LAYER, buf);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        // In insert mode, Esc leaves the editor's insert mode rather than
        // cancelling the dialog. Cancel is reachable with a second Esc from
        // normal mode.
        if key.code == KeyCode::Esc && self.editor.mode() == EditorMode::Insert {
            self.editor.send_key(&key);
            return Action::Continue;
        }
        if let Some(input) = InputKey::from_event(&key) {
            match bindings::resolve(
                &app.config.bindings.router,
                bindings::PASTE_LAYER,
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
        if self.editor.send_key(&key) {
            self.error = None;
        }
        Action::Continue
    }

    fn process_paste(&mut self, _app: &mut App, text: String) {
        let span = EditorSpan::empty_at(self.editor.cursor_offset());
        self.editor.replace_range(span, text.trim());
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Action::Continue;
        }
        if rect_contains(self.cancel_button, mouse.column, mouse.row) {
            self.cancel(app);
        } else if rect_contains(self.upload_button, mouse.column, mouse.row) {
            self.submit(app);
        }
        Action::Continue
    }

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

fn command_key_hint(
    bindings: &bindings::BindingRuntime,
    layer: LayerId,
    target: BindCommand,
) -> Option<String> {
    let pending = None;
    for reachable in bindings::reachable(bindings, layer, &pending) {
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
            &app.theme,
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
            &app.theme,
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
            &app.theme,
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
            &app.theme,
        );
        mode.submit(&mut app);
        assert!(mode.error.is_some());
        assert!(app.pending_transition.is_empty());
    }

    #[test]
    fn paste_dialog_esc_leaves_insert_before_cancelling() {
        let mut app = test_app();
        let mut mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::ExistingPath("/tmp/pic.png".into()),
                "pic.png",
            ),
            &app.theme,
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());

        // First Esc: editor leaves insert mode, dialog stays open.
        mode.process_input(&mut app, esc);
        assert_eq!(mode.editor.mode(), EditorMode::Normal);
        assert!(app.pending_transition.is_empty());

        // Second Esc: from normal mode this cancels the dialog.
        mode.process_input(&mut app, esc);
        assert!(!app.pending_transition.is_empty());
    }

    #[test]
    fn paste_dialog_cancel_removes_staged_file() {
        let mut app = test_app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staged.png");
        std::fs::write(&path, b"bytes").unwrap();
        let mut mode = PasteImageUploadMode::new(
            image_paste(ImagePasteSource::StagedFile(path.clone()), "staged.png"),
            &app.theme,
        );
        mode.cancel(&mut app);
        assert!(!path.exists());
        assert!(!app.pending_transition.is_empty());
    }

    #[test]
    fn paste_dialog_uses_paste_layer_and_binding_hints() {
        let app = test_app();
        let mode = PasteImageUploadMode::new(
            image_paste(
                ImagePasteSource::ExistingPath("/tmp/pic.png".into()),
                "pic.png",
            ),
            &app.theme,
        );
        let chrome = mode.presentation(&app).chrome.expect("dialog chrome");
        assert!(chrome.layer == bindings::PASTE_LAYER);
        assert_eq!(
            command_key_hint(
                &app.config.bindings,
                bindings::PASTE_LAYER,
                BindCommand::Cancel
            ),
            Some("Esc".to_string())
        );
        assert_eq!(
            command_key_hint(
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
                command_key_hint(
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
                command_key_hint(
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
