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
    app::{
        UserVolumeDialog,
        command::CoreCommand,
        device_pair::{
            DeviceLinkButton, DeviceLinkDialog, DevicePairDialog, DevicePairEvent,
        },
    },
    bindings::{self, BindCommand, Resolved},
    client_channel::{DirtySections, E2eIdentityOverlay, E2eIdentityTarget},
    clipboard_paste::{
        ClipboardPasteProvider, HelperClipboard, ImagePaste, ImagePasteOrigin, ImagePasteSource,
        PastePayload,
    },
    theme,
    theme::Theme,
    tui::{
        Action,
        form::rect_contains,
        mode::{
            AppMode, ChromeSpec, Coverage, ModePresentation, ModeTransition, ViewCx, is_quit_key,
        },
        modes::process_global_command_cx,
        widgets::{
            RowPalette, ScrollbarDrag, ScrollbarId, ScrollbarLayout, ScrollbarState, button_label,
            draw_button, draw_labeled_editor_frame, draw_scrollbar,
        },
    },
};

use crate::e2e_identity::E2ePublicIdentity;

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
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogButtonKey {
    Cancel,
    Activate,
    Previous,
    Next,
}

/// Common keyboard behavior for every hand-rendered dialog button row.
///
/// Keeping this here prevents new overlays from quietly losing the Vim keys
/// that the established confirmation dialogs support.
fn dialog_button_key(key: &KeyEvent) -> Option<DialogButtonKey> {
    match key.code {
        KeyCode::Esc => Some(DialogButtonKey::Cancel),
        KeyCode::Enter => Some(DialogButtonKey::Activate),
        KeyCode::Left | KeyCode::BackTab | KeyCode::Char('h') => Some(DialogButtonKey::Previous),
        KeyCode::Right | KeyCode::Tab | KeyCode::Char('l') => Some(DialogButtonKey::Next),
        _ => None,
    }
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
        match dialog_button_key(&key) {
            Some(DialogButtonKey::Cancel) => cx.request_transition(ModeTransition::Pop),
            Some(DialogButtonKey::Activate) => {
                if self.selected_confirm {
                    self.confirm(cx);
                } else {
                    cx.request_transition(ModeTransition::Pop);
                }
            }
            Some(DialogButtonKey::Previous | DialogButtonKey::Next) => {
                self.selected_confirm = !self.selected_confirm
            }
            None => match key.code {
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    cx.request_transition(ModeTransition::Pop)
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm(cx),
                _ => {}
            },
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
        let cancel = button_label(&self.cancel_label, Some("n".to_string()));
        let confirm = button_label(&self.confirm_label, Some("y".to_string()));
        draw_button(self.cancel_button, buf, cancel_style, &cancel);
        draw_button(self.confirm_button, buf, confirm_style, &confirm);
    }
}

impl AppMode for ConfirmMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
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
        let mut body = crate::tui::render::draw_dialog_frame(panel, buf, theme, &self.prompt);
        body.take_top(1);
        self.render_buttons(body.take_top(1), buf, theme);
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
    generation: u64,
    selected_connect: bool,
    cancel_button: Rect,
    connect_button: Rect,
}

impl NativeEncryptionWarningMode {
    pub(crate) fn new(label: String, generation: u64) -> Self {
        Self {
            label,
            generation,
            selected_connect: false,
            cancel_button: Rect::EMPTY,
            connect_button: Rect::EMPTY,
        }
    }

    fn accept(&mut self, cx: &mut ViewCx<'_>) {
        cx.send(CoreCommand::AcceptNativeEncryption {
            label: self.label.clone(),
            generation: self.generation,
        });
    }

    fn cancel(&mut self, cx: &mut ViewCx<'_>) {
        cx.send(CoreCommand::CancelNativeEncryption {
            generation: self.generation,
        });
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
        match dialog_button_key(&key) {
            Some(DialogButtonKey::Cancel) => {
                self.cancel(cx);
                return Action::Continue;
            }
            Some(DialogButtonKey::Activate) => {
                if self.selected_connect {
                    self.accept(cx);
                } else {
                    self.cancel(cx);
                }
                return Action::Continue;
            }
            Some(DialogButtonKey::Previous | DialogButtonKey::Next) => {
                self.selected_connect = !self.selected_connect;
                return Action::Continue;
            }
            None => match key.code {
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.cancel(cx);
                    return Action::Continue;
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.accept(cx);
                    return Action::Continue;
                }
                _ => {}
            },
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

    fn render_buttons(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
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

        let cancel = button_label("Cancel", Some("n".to_string()));
        let connect = button_label("Connect", Some("y".to_string()));
        let (cancel_style, connect_style) = if self.selected_connect {
            (
                theme.dialog_panel.patch(theme.muted),
                theme.selected_focused,
            )
        } else {
            (
                theme.selected_focused,
                theme.dialog_panel.patch(theme.muted),
            )
        };
        draw_button(self.cancel_button, buf, cancel_style, &cancel);
        draw_button(self.connect_button, buf, connect_style, &connect);
    }
}

impl AppMode for NativeEncryptionWarningMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
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
        let mut body = crate::tui::render::draw_dialog_frame_with_header(
            panel,
            buf,
            theme,
            "No Native Encryption to Server",
            native_encryption_header_style(theme),
        );
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
            self.render_buttons(body.take_top(1), buf, theme);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecurityLineKind {
    Lead,
    Section,
    Body,
    Key,
    Words,
    Muted,
    Success,
    Warning,
    Danger,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SecurityDialogLine {
    text: String,
    kind: SecurityLineKind,
    trailing: Option<String>,
}

impl SecurityDialogLine {
    fn new(text: impl Into<String>, kind: SecurityLineKind) -> Self {
        Self {
            text: text.into(),
            kind,
            trailing: None,
        }
    }

    fn section(text: impl Into<String>) -> Self {
        Self::new(text, SecurityLineKind::Section)
    }

    fn section_with_metadata(text: impl Into<String>, metadata: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: SecurityLineKind::Section,
            trailing: Some(metadata.into()),
        }
    }

    fn blank() -> Self {
        Self::new(String::new(), SecurityLineKind::Body)
    }
}

fn append_identity_block(
    lines: &mut Vec<SecurityDialogLine>,
    heading: impl Into<String>,
    identity: &crate::config::E2ePeerIdentity,
    width: usize,
) {
    lines.push(SecurityDialogLine::section_with_metadata(
        heading,
        format!(
            "User ID {} | Room ID {:#x}",
            identity.user_id, identity.room_id
        ),
    ));
    lines.push(SecurityDialogLine::blank());
    lines.push(SecurityDialogLine::section("X25519 public key"));
    append_grouped_key_lines(lines, &identity.public_key, width);
    lines.push(SecurityDialogLine::blank());
    lines.push(SecurityDialogLine::section(
        "Identity words (compare all 24)",
    ));
    append_public_identity_word_lines(lines, &identity.public_key, width);
}

fn append_grouped_key_lines(lines: &mut Vec<SecurityDialogLine>, public_key: &str, width: usize) {
    let groups: Vec<_> = public_key.as_bytes().chunks(8).collect();
    let mut row = String::new();
    for group in groups {
        let group = String::from_utf8_lossy(group);
        if !row.is_empty() && row.len() + group.len() + 1 > width.max(8) {
            lines.push(SecurityDialogLine::new(
                std::mem::take(&mut row),
                SecurityLineKind::Key,
            ));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(&group);
    }
    if !row.is_empty() {
        lines.push(SecurityDialogLine::new(row, SecurityLineKind::Key));
    }
}

fn append_public_identity_word_lines(
    lines: &mut Vec<SecurityDialogLine>,
    public_key: &str,
    width: usize,
) {
    let words = E2ePublicIdentity::from_hex(public_key)
        .expect("worker identity is validated")
        .words();
    let column_width = words.iter().map(|word| word.len()).max().unwrap_or(1) + 2;
    let columns = (width.max(column_width) / column_width).clamp(1, 6);
    for row in words.chunks(columns) {
        let mut text = String::with_capacity(columns * column_width);
        for column in 0..columns {
            if let Some(word) = row.get(column) {
                text.push_str(&format!("{word:<column_width$}"));
            } else {
                text.push_str(&" ".repeat(column_width));
            }
        }
        lines.push(SecurityDialogLine::new(text, SecurityLineKind::Words));
    }
}

fn append_wrapped_security_lines(
    lines: &mut Vec<SecurityDialogLine>,
    text: &str,
    width: usize,
    kind: SecurityLineKind,
) {
    let width = width.max(12);
    let mut row = String::new();
    for word in text.split_whitespace() {
        if word.is_ascii() && word.len() > width {
            if !row.is_empty() {
                lines.push(SecurityDialogLine::new(std::mem::take(&mut row), kind));
            }
            for chunk in word.as_bytes().chunks(width) {
                lines.push(SecurityDialogLine::new(
                    String::from_utf8_lossy(chunk).into_owned(),
                    kind,
                ));
            }
            continue;
        }
        if !row.is_empty() && row.len() + word.len() + 1 > width {
            lines.push(SecurityDialogLine::new(std::mem::take(&mut row), kind));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(word);
    }
    if !row.is_empty() {
        lines.push(SecurityDialogLine::new(row, kind));
    }
}

fn security_line_style(line: &SecurityDialogLine, theme: &Theme) -> extui::Style {
    let style = match line.kind {
        SecurityLineKind::Lead => theme.text | Modifier::BOLD,
        SecurityLineKind::Section => theme.accent | Modifier::BOLD,
        SecurityLineKind::Body | SecurityLineKind::Key | SecurityLineKind::Words => theme.text,
        SecurityLineKind::Muted => theme.muted,
        SecurityLineKind::Success => theme.good | Modifier::BOLD,
        SecurityLineKind::Warning => theme.warn | Modifier::BOLD,
        SecurityLineKind::Danger => theme.error | Modifier::BOLD,
    };
    theme.dialog_panel.patch(style)
}

fn render_security_lines(
    mut area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    lines: &[SecurityDialogLine],
    scroll: usize,
) {
    for line in lines.iter().skip(scroll).take(area.h as usize) {
        let row = area.take_top(1);
        if let Some(metadata) = &line.trailing {
            row.with(security_line_style(line, theme))
                .with(Ellipsis(true))
                .text(buf, &line.text)
                .with(HAlign::Right)
                .with(theme.dialog_panel.patch(theme.muted))
                .text(buf, metadata);
        } else if matches!(line.kind, SecurityLineKind::Key | SecurityLineKind::Words) {
            row.with(security_line_style(line, theme))
                .with(HAlign::Center)
                .with(Ellipsis(true))
                .text(buf, &line.text);
        } else {
            row.with(security_line_style(line, theme))
                .with(Ellipsis(true))
                .text(buf, &line.text);
        }
    }
}

fn draw_identity_input_border(
    area: Rect,
    glyph: &str,
    input_style: extui::Style,
    theme: &Theme,
    buf: &mut Buffer,
) {
    if area.is_empty() {
        return;
    }
    let style = input_style.bg().map_or(theme.dialog_panel, |background| {
        theme.dialog_panel.with_fg(background)
    });
    area.with(style).text(buf, &glyph.repeat(area.w as usize));
}

pub(crate) struct E2eIdentityMode {
    dialog: E2eIdentityOverlay,
    editor: Editor,
    identity_view: IdentityView,
    scroll: usize,
    focus: IdentityFocus,
    close_button: Rect,
    primary_button: Rect,
    verification_input_frame: Rect,
    verification_input: Rect,
    words_button: Rect,
    copy_card_button: Rect,
    copy_key_button: Rect,
    copy_words_button: Rect,
    their_identity_button: Rect,
    your_identity_button: Rect,
    previous_identity_button: Rect,
    words_confirmed: bool,
    forget_confirmation: bool,
    scrollbar: Option<ScrollbarLayout>,
    scrollbar_drag: Option<ScrollbarDrag>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum IdentityFocus {
    Words,
    #[default]
    VerificationInput,
    Cancel,
    Primary,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum IdentityView {
    #[default]
    Their,
    Yours,
    Previous,
}

impl IdentityView {
    const ALL: [Self; 3] = [Self::Their, Self::Yours, Self::Previous];

    fn cycle(self, delta: isize) -> Self {
        let index = Self::ALL
            .iter()
            .position(|view| *view == self)
            .unwrap_or_default();
        let index = (index as isize + delta).rem_euclid(Self::ALL.len() as isize) as usize;
        Self::ALL[index]
    }
}

impl E2eIdentityMode {
    pub(crate) fn new(dialog: E2eIdentityOverlay, theme: &Theme) -> Self {
        let mut editor = filename_editor(theme);
        if !dialog.pasted_verification_text.is_empty() {
            editor.replace_range(EditorSpan::empty_at(0), &dialog.pasted_verification_text);
        }
        let mut mode = Self {
            dialog,
            editor,
            identity_view: IdentityView::Their,
            scroll: 0,
            focus: IdentityFocus::VerificationInput,
            close_button: Rect::EMPTY,
            primary_button: Rect::EMPTY,
            verification_input_frame: Rect::EMPTY,
            verification_input: Rect::EMPTY,
            words_button: Rect::EMPTY,
            copy_card_button: Rect::EMPTY,
            copy_key_button: Rect::EMPTY,
            copy_words_button: Rect::EMPTY,
            their_identity_button: Rect::EMPTY,
            your_identity_button: Rect::EMPTY,
            previous_identity_button: Rect::EMPTY,
            words_confirmed: false,
            forget_confirmation: false,
            scrollbar: None,
            scrollbar_drag: None,
        };
        if !mode.editor.text().trim().is_empty() {
            mode.refresh_verification_result();
        }
        if !mode.can_verify() {
            mode.focus = IdentityFocus::Cancel;
        }
        mode
    }

    fn has_previous_identity(&self) -> bool {
        false
    }

    fn show_identity(&mut self, view: IdentityView) {
        if view != IdentityView::Previous || self.has_previous_identity() {
            self.identity_view = view;
            self.scroll = 0;
            if self.focus == IdentityFocus::Words && view != IdentityView::Their {
                self.focus = if self.can_verify() {
                    IdentityFocus::VerificationInput
                } else {
                    IdentityFocus::Cancel
                };
            }
        }
    }

    fn cycle_identity(&mut self, delta: isize) {
        let mut view = self.identity_view;
        for _ in 0..IdentityView::ALL.len() {
            view = view.cycle(delta);
            if view != IdentityView::Previous || self.has_previous_identity() {
                self.show_identity(view);
                return;
            }
        }
    }

    fn close(&self, cx: &mut ViewCx<'_>) {
        cx.send(CoreCommand::CloseE2eIdentity);
    }

    fn confirm(&self, cx: &mut ViewCx<'_>) {
        if self.verification_passed() {
            cx.send(CoreCommand::ConfirmE2eIdentity(self.dialog.target.clone()));
        }
    }

    fn forget(&mut self, cx: &mut ViewCx<'_>) {
        let identity = self.dialog.target.accepted.clone();
        if self.forget_confirmation {
            cx.send(CoreCommand::ForgetE2eIdentity(identity));
        } else {
            self.forget_confirmation = true;
        }
    }

    fn confirm_words(&mut self) {
        if !self.has_verification_error() {
            self.words_confirmed = true;
        }
    }

    fn can_verify(&self) -> bool {
        self.dialog.target.accepted.trust_level != crate::config::E2eTrustLevel::Verified
    }

    fn has_verification_error(&self) -> bool {
        self.dialog.result.as_ref().is_some_and(Result::is_err)
    }

    fn verification_passed(&self) -> bool {
        !self.has_verification_error()
            && (self.words_confirmed || self.dialog.result == Some(Ok(())))
    }

    fn refresh_verification_result(&mut self) {
        let text = self.editor.text().trim().to_string();
        self.dialog.pasted_verification_text = text.clone();
        self.dialog.result = validate_verification_text(
            &self.dialog.local_verification_text,
            &self.dialog.target,
            &text,
        );
        self.forget_confirmation = false;
    }

    fn replace_verification_text(&mut self, text: &str) {
        self.focus = IdentityFocus::VerificationInput;
        let all = self.editor.text();
        self.editor
            .replace_range(EditorSpan::new(0, all.len() as u32), text.trim());
        self.refresh_verification_result();
    }

    fn paste_from_clipboard(&mut self, cx: &mut ViewCx<'_>, provider: &dyn ClipboardPasteProvider) {
        match provider.read_paste() {
            Ok(PastePayload::Text(text)) => self.replace_verification_text(&text),
            Ok(PastePayload::Image(image)) => {
                if image.source.is_staged() {
                    let _ = std::fs::remove_file(image.source.path());
                }
                cx.set_status("clipboard does not contain verification text");
            }
            Ok(PastePayload::Empty) => cx.set_status("clipboard is empty"),
            Ok(PastePayload::Unsupported(reason)) => cx.set_status(reason),
            Err(error) => cx.set_error(error.to_string()),
        }
    }

    fn process_clipboard_binding(
        &mut self,
        cx: &mut ViewCx<'_>,
        key: &KeyEvent,
        provider: &dyn ClipboardPasteProvider,
    ) -> bool {
        if !self.can_verify() {
            return false;
        }
        let Some(input) = InputKey::from_event(key) else {
            return false;
        };
        let layer = if self.editor.mode() == EditorMode::Insert {
            bindings::INSERT_LAYER
        } else {
            bindings::COMPOSE_NORMAL_LAYER
        };
        match bindings::resolve(
            &cx.config.bindings.router,
            layer,
            &mut cx.view.chrome.binding.pending_chord,
            input,
        ) {
            Resolved::Action(id)
                if matches!(
                    cx.config.bindings.actions.get(id),
                    BindCommand::PasteClipboard
                ) =>
            {
                self.paste_from_clipboard(cx, provider);
                true
            }
            Resolved::Consumed => true,
            Resolved::Action(_) | Resolved::Unmatched => false,
        }
    }

    fn activate_primary(&mut self, cx: &mut ViewCx<'_>) {
        if self.verification_passed() {
            self.confirm(cx);
        } else if self.dialog.target.accepted.trust_level == crate::config::E2eTrustLevel::Verified
        {
            self.forget(cx);
        }
    }

    fn move_focus(&mut self, direction: isize) {
        let mut available = Vec::with_capacity(4);
        if self.identity_view == IdentityView::Their && self.can_verify() {
            available.push(IdentityFocus::Words);
        }
        if self.can_verify() {
            available.push(IdentityFocus::VerificationInput);
        }
        available.push(IdentityFocus::Cancel);
        if self.verification_passed()
            || self.dialog.target.accepted.trust_level == crate::config::E2eTrustLevel::Verified
        {
            available.push(IdentityFocus::Primary);
        }
        let current = available
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or_default();
        let next = (current as isize + direction).rem_euclid(available.len() as isize) as usize;
        self.focus = available[next];
    }

    fn identity_lines(&self, width: usize) -> Vec<SecurityDialogLine> {
        let username =
            displayed_identity_name(&self.dialog.target.username, self.dialog.target.user_id.0);
        let mut lines = Vec::new();
        let (status, status_kind) = match (
            self.dialog.target.accepted.trust_level,
            self.dialog.target.accepted.change_from,
        ) {
            (
                crate::config::E2eTrustLevel::Accepted,
                Some(crate::config::E2eTrustLevel::Verified),
            ) => (
                "VERIFIED IDENTITY CHANGED: possible interception; verify through another channel",
                SecurityLineKind::Danger,
            ),
            (crate::config::E2eTrustLevel::Accepted, Some(_)) => (
                "IDENTITY CHANGED: verify through another channel",
                SecurityLineKind::Danger,
            ),
            (crate::config::E2eTrustLevel::Accepted, None) => (
                "UNVERIFIED: identity not independently confirmed",
                SecurityLineKind::Warning,
            ),
            (crate::config::E2eTrustLevel::Verified, _) => (
                "VERIFIED: all identity words or verification text matched",
                SecurityLineKind::Lead,
            ),
        };
        lines.push(SecurityDialogLine::new(status, status_kind));
        lines.push(SecurityDialogLine::blank());
        append_wrapped_security_lines(
            &mut lines,
            "An encryption identity is the public key Chatt uses to protect this DM. Confirming it means you compared it outside this DM.",
            width,
            SecurityLineKind::Body,
        );
        lines.push(SecurityDialogLine::blank());

        match self.identity_view {
            IdentityView::Their => self.append_their_identity_lines(&mut lines, width, &username),
            IdentityView::Yours => self.append_your_identity_lines(&mut lines, width),
            IdentityView::Previous => self.append_previous_identity_lines(&mut lines, width),
        }
        if self.identity_view == IdentityView::Their {
            lines.push(SecurityDialogLine::section("How to confirm this identity"));
            append_wrapped_security_lines(
                &mut lines,
                &format!(
                    "Call {username}, meet in person, or use another trusted service. Compare every one of the 24 identity words. This DM is not an independent verification channel."
                ),
                width,
                SecurityLineKind::Body,
            );
            lines.push(SecurityDialogLine::blank());
            lines.push(SecurityDialogLine::section("Verification text"));
            append_wrapped_security_lines(
                &mut lines,
                &format!(
                    "Verification text is a copyable form of the same public identity, bound to this server and account. Paste {username}'s text below to check it automatically. If clipboard sharing is unavailable, compare the 24 words instead."
                ),
                width,
                SecurityLineKind::Muted,
            );
            if let Some(error) = &self.dialog.error {
                lines.push(SecurityDialogLine::blank());
                append_wrapped_security_lines(&mut lines, error, width, SecurityLineKind::Danger);
            }
        }
        lines
    }

    fn append_their_identity_lines(
        &self,
        lines: &mut Vec<SecurityDialogLine>,
        width: usize,
        username: &str,
    ) {
        let label = format!("{username}'s public identity");
        let target_identity = crate::config::E2ePeerIdentity {
            room_id: self.dialog.target.room_id.0,
            user_id: self.dialog.target.user_id.0,
            username: username.to_string(),
            public_key: self.dialog.target.public_key.clone(),
            trust_level: self.dialog.target.accepted.trust_level,
        };
        append_identity_block(lines, label, &target_identity, width);
        if self.words_confirmed {
            lines.push(SecurityDialogLine::new(
                "✓ I confirmed that all 24 identity words matched.",
                SecurityLineKind::Success,
            ));
        }
        lines.push(SecurityDialogLine::blank());
    }

    fn append_your_identity_lines(&self, lines: &mut Vec<SecurityDialogLine>, width: usize) {
        match crate::e2e_identity::VerificationText::parse(&self.dialog.local_verification_text) {
            Ok(local) => {
                lines.push(SecurityDialogLine::section_with_metadata(
                    "Your public identity",
                    format!(
                        "User ID {} | Room ID {:#x}",
                        local.user_id(),
                        self.dialog.target.room_id.0
                    ),
                ));
                lines.push(SecurityDialogLine::new(
                    "The other person compares these exact words with the identity they see for you.",
                    SecurityLineKind::Muted,
                ));
                lines.push(SecurityDialogLine::blank());
                lines.push(SecurityDialogLine::section("X25519 public key"));
                append_grouped_key_lines(lines, &local.identity().hex(), width);
                lines.push(SecurityDialogLine::blank());
                lines.push(SecurityDialogLine::section(
                    "Your identity words (the other person compares all 24)",
                ));
                append_public_identity_word_lines(lines, &local.identity().hex(), width);
            }
            Err(_) => {
                lines.push(SecurityDialogLine::section("Your public identity"));
                lines.push(SecurityDialogLine::new(
                    "Local public identity is unavailable.",
                    SecurityLineKind::Danger,
                ));
            }
        }
    }

    fn append_previous_identity_lines(&self, lines: &mut Vec<SecurityDialogLine>, width: usize) {
        let _ = width;
        lines.push(SecurityDialogLine::new(
            "Previous keys are retained only to authenticate older messages.",
            SecurityLineKind::Muted,
        ));
    }
}

fn displayed_identity_name(username: &str, user_id: u64) -> String {
    let username = username.trim();
    if username.is_empty() {
        format!("User {user_id}")
    } else {
        username.to_string()
    }
}

fn identity_dialog_height(screen_height: u16) -> u16 {
    screen_height
        .saturating_sub(16)
        .max(screen_height.min(32))
        .max(10)
        .min(screen_height)
}

fn validate_verification_text(
    local_text: &str,
    target: &E2eIdentityTarget,
    verification_text: &str,
) -> Option<Result<(), String>> {
    use crate::e2e_identity::{
        VerificationText, VerificationTextError, VerificationTextMatchError,
    };

    if verification_text.trim().is_empty() {
        return None;
    }
    let result = (|| {
        let local = VerificationText::parse(local_text)
            .map_err(|_| "Your local verification context is unavailable.".to_string())?;
        let text = VerificationText::parse(verification_text).map_err(|error| match error {
            VerificationTextError::UnsupportedVersion | VerificationTextError::Malformed => {
                "Verification text is incomplete or malformed.".to_string()
            }
            VerificationTextError::ChecksumMismatch => {
                "Verification text has an invalid checksum.".to_string()
            }
            VerificationTextError::InvalidServerKey
            | VerificationTextError::InvalidUserId
            | VerificationTextError::InvalidPublicKey
            | VerificationTextError::NonCanonical => "Verification text is invalid.".to_string(),
        })?;
        let expected = E2ePublicIdentity::from_hex(&target.public_key)
            .expect("worker target contains a validated key");
        text.match_context(
            local.server_public_key(),
            local.user_id(),
            target.user_id.0,
            expected.public_key(),
        )
        .map_err(|error| match error {
            VerificationTextMatchError::WrongServer => {
                "Verification text belongs to a different Chatt server.".to_string()
            }
            VerificationTextMatchError::SelfText => {
                "That is your verification text, not the other person's.".to_string()
            }
            VerificationTextMatchError::WrongUser {
                presented,
                expected,
            } => format!("Verification text belongs to user {presented}, not user {expected}."),
            VerificationTextMatchError::KeyMismatch => {
                "DANGER: verification text contains a different public key.".to_string()
            }
        })
    })();
    Some(result)
}

impl AppMode for E2eIdentityMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        let app = crate::tui::render::RenderState::new(cx);
        let theme = &app.view.theme;
        let area = buf.rect();
        if area.w < 30 || area.h < 10 {
            return;
        }
        let width = area.w.min(92).max(30);
        let height = identity_dialog_height(area.h);
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        let username =
            displayed_identity_name(&self.dialog.target.username, self.dialog.target.user_id.0);
        let mut body = crate::tui::render::draw_dialog_frame(
            panel,
            buf,
            theme,
            &format!("Encryption identity: {username}"),
        );
        let can_verify = self.can_verify();
        let button_row = body.take_bottom(1);
        let action_area = if can_verify {
            body.take_bottom(4)
        } else {
            Rect::EMPTY
        };
        let mut copy_buttons = body.take_bottom(1);
        if body.h >= 5 {
            body.take_bottom(1);
        }
        let third = copy_buttons.w / 3;
        self.copy_card_button = copy_buttons.take_left(third as i32);
        self.copy_key_button = copy_buttons.take_left(third as i32);
        self.copy_words_button = copy_buttons;
        let mut identity_tabs = body.take_top(1);
        let third = identity_tabs.w / 3;
        self.their_identity_button = identity_tabs.take_left(third as i32);
        self.your_identity_button = identity_tabs.take_left(third as i32);
        self.previous_identity_button = identity_tabs;
        let idle_tab = theme.dialog_panel.patch(theme.muted);
        draw_button(
            self.their_identity_button,
            buf,
            if self.identity_view == IdentityView::Their {
                theme.selected_focused
            } else {
                idle_tab
            },
            "Their identity",
        );
        draw_button(
            self.your_identity_button,
            buf,
            if self.identity_view == IdentityView::Yours {
                theme.selected_focused
            } else {
                idle_tab
            },
            "Your identity",
        );
        draw_button(
            self.previous_identity_button,
            buf,
            if self.identity_view == IdentityView::Previous {
                theme.selected_focused
            } else if !self.has_previous_identity() {
                theme.dialog_panel.patch(theme.subtle)
            } else {
                idle_tab
            },
            if self.has_previous_identity() {
                "Previous identity"
            } else {
                "No previous identity"
            },
        );
        let identity_lines = self.identity_lines(body.w.saturating_sub(2) as usize);
        let max_scroll = identity_lines.len().saturating_sub(body.h as usize);
        self.scroll = self.scroll.min(max_scroll);
        let mut content = body;
        if max_scroll > 0 {
            let scrollbar_rect = content.take_right(1);
            content.take_right(1);
            let scrollbar = ScrollbarLayout {
                id: ScrollbarId::Identity,
                rect: scrollbar_rect,
                state: ScrollbarState {
                    total: identity_lines.len() as u32,
                    viewport: content.h as u32,
                    offset: self.scroll as u32,
                },
            };
            draw_scrollbar(scrollbar, *theme, buf);
            self.scrollbar = Some(scrollbar);
        } else {
            self.scrollbar = None;
            self.scrollbar_drag = None;
        }
        render_security_lines(content, buf, theme, &identity_lines, self.scroll);

        self.words_button = Rect::EMPTY;
        if self.identity_view == IdentityView::Their
            && can_verify
            && let Some(index) = identity_lines
                .iter()
                .position(|line| line.text == "Identity words (compare all 24)")
            && index >= self.scroll
            && index < self.scroll + content.h as usize
        {
            let mut header = Rect {
                x: content.x,
                y: content.y + (index - self.scroll) as u16,
                w: content.w,
                h: 1,
            };
            let label = if self.words_confirmed {
                "Words matched ✓"
            } else {
                "I checked all 24 words"
            };
            let button_width =
                (UnicodeWidthStr::width(label) as u16 + 2).min((header.w / 2).max(1));
            self.words_button = header.take_right(button_width as i32);
            let style = if self.words_confirmed {
                theme.mode_compose | Modifier::BOLD
            } else if self.focus == IdentityFocus::Words {
                theme.selected_focused
            } else {
                theme.status_section_inactive
            };
            draw_button(self.words_button, buf, style, label);
        }
        draw_button(
            self.copy_card_button,
            buf,
            theme.dialog_panel.patch(theme.muted),
            "My verification text [M-c]",
        );
        draw_button(
            self.copy_key_button,
            buf,
            theme.dialog_panel.patch(theme.muted),
            "Their key [M-k]",
        );
        draw_button(
            self.copy_words_button,
            buf,
            theme.dialog_panel.patch(theme.muted),
            "Their words [M-w]",
        );
        self.verification_input_frame = Rect::EMPTY;
        self.verification_input = Rect::EMPTY;
        if can_verify {
            let mut field = action_area;
            self.verification_input_frame = field.take_top(3);
            let mut framed_input = self.verification_input_frame;
            let top_border = framed_input.take_top(1);
            let bottom_border = framed_input.take_bottom(1);
            let input_focused = self.focus == IdentityFocus::VerificationInput;
            let mut input_style = if input_focused {
                theme.join_input_active
            } else {
                theme.join_input_inactive
            };
            if self.has_verification_error() {
                input_style = input_style.patch(theme.error.without_bg());
            }
            draw_identity_input_border(top_border, "▄", input_style, theme, buf);
            draw_identity_input_border(bottom_border, "▀", input_style, theme, buf);
            framed_input.with(input_style).fill(buf);
            framed_input.take_left(1);
            framed_input.take_right(1);
            self.verification_input = framed_input;
            self.editor.set_theme(if input_focused {
                theme.join_input_editor_theme()
            } else {
                theme.join_input_inactive_editor_theme()
            });
            self.editor.render(self.verification_input, buf);
            if !input_focused {
                buf.hide_cursor();
            }
            if self.editor.text().is_empty() {
                self.verification_input
                    .with(input_style.patch(theme.muted.without_bg()))
                    .with(Ellipsis(true))
                    .text(buf, "paste their verification text here");
            }
            let status = field.take_top(1);
            if let Some(result) = &self.dialog.result {
                let (label, style) = match result {
                    Ok(()) => (
                        "✓ Verification text matches this identity.",
                        theme.good | Modifier::BOLD,
                    ),
                    Err(error) => (error.as_str(), theme.error | Modifier::BOLD),
                };
                status
                    .with(theme.dialog_panel.patch(style))
                    .with(Ellipsis(true))
                    .text(buf, label);
            }
        }

        let has_primary = true;
        let mut buttons = button_row;
        if has_primary {
            self.close_button = buttons.take_left((buttons.w / 2) as i32);
            self.primary_button = buttons;
        } else {
            self.close_button = buttons;
            self.primary_button = Rect::EMPTY;
        }
        let idle = theme.dialog_panel.patch(theme.muted);
        draw_button(
            self.close_button,
            buf,
            if self.focus == IdentityFocus::Cancel {
                theme.selected_focused
            } else {
                idle
            },
            "Cancel",
        );
        if has_primary {
            let (label, style) = if self.verification_passed() {
                (
                    "Verify identity [Enter]",
                    theme.dialog_panel.patch(theme.good | Modifier::BOLD),
                )
            } else if self.has_verification_error() {
                ("Clear verification text to continue", idle)
            } else if self.forget_confirmation {
                (
                    "Press again to forget verification",
                    theme.dialog_panel.patch(theme.warn | Modifier::BOLD),
                )
            } else if self.dialog.target.accepted.trust_level
                == crate::config::E2eTrustLevel::Verified
            {
                (
                    "Forget verification",
                    if self.focus == IdentityFocus::Primary {
                        theme.selected_focused
                    } else {
                        idle
                    },
                )
            } else {
                (
                    "Independent verification required",
                    theme.dialog_panel.patch(theme.subtle),
                )
            };
            draw_button(self.primary_button, buf, style, label);
        }
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        match key.code {
            KeyCode::Tab => {
                self.cycle_identity(1);
                return Action::Continue;
            }
            KeyCode::BackTab => {
                self.cycle_identity(-1);
                return Action::Continue;
            }
            _ => {}
        }
        if key.code == KeyCode::Esc
            && self.focus == IdentityFocus::VerificationInput
            && self.editor.mode() != EditorMode::Normal
        {
            self.editor.send_key(&key);
            return Action::Continue;
        }
        if self.process_clipboard_binding(cx, &key, &HelperClipboard) {
            return Action::Continue;
        }
        match key.code {
            KeyCode::Esc => self.close(cx),
            KeyCode::Char('c') if key.modifiers == KeyModifiers::ALT => {
                cx.queue_clipboard(self.dialog.local_verification_text.clone())
            }
            KeyCode::Char('k') if key.modifiers == KeyModifiers::ALT => {
                cx.queue_clipboard(self.dialog.target.public_key.clone())
            }
            KeyCode::Char('w') if key.modifiers == KeyModifiers::ALT => {
                let words = E2ePublicIdentity::from_hex(&self.dialog.target.public_key)
                    .expect("validated verification target")
                    .words_string();
                cx.queue_clipboard(words);
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up => self.move_focus(-1),
            KeyCode::Down => self.move_focus(1),
            KeyCode::Left
                if matches!(self.focus, IdentityFocus::Cancel | IdentityFocus::Primary) =>
            {
                self.focus = IdentityFocus::Cancel;
            }
            KeyCode::Right
                if matches!(self.focus, IdentityFocus::Cancel | IdentityFocus::Primary) =>
            {
                self.focus = IdentityFocus::Primary;
            }
            KeyCode::Home if self.focus != IdentityFocus::VerificationInput => self.scroll = 0,
            KeyCode::Enter => match self.focus {
                IdentityFocus::Words => self.confirm_words(),
                IdentityFocus::VerificationInput if self.verification_passed() => self.confirm(cx),
                IdentityFocus::Cancel => self.close(cx),
                IdentityFocus::Primary => self.activate_primary(cx),
                IdentityFocus::VerificationInput => {}
            },
            KeyCode::Char('k') if self.focus != IdentityFocus::VerificationInput => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            KeyCode::Char('j') if self.focus != IdentityFocus::VerificationInput => {
                self.scroll = self.scroll.saturating_add(1)
            }
            _ if self.focus == IdentityFocus::VerificationInput && self.can_verify() => {
                self.editor.send_key(&key);
                self.refresh_verification_result();
            }
            _ => {}
        }
        Action::Continue
    }

    fn process_paste(&mut self, _cx: &mut ViewCx<'_>, text: String) {
        if self.can_verify() {
            self.replace_verification_text(&text);
        }
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(1);
                return Action::Continue;
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(1);
                return Action::Continue;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.scrollbar_drag.is_some() => {
                if let (Some(mut drag), Some(scrollbar)) = (self.scrollbar_drag, self.scrollbar)
                    && let Some(target) = scrollbar.drag_target(drag, mouse.row)
                {
                    self.scroll = target as usize;
                    drag.current_offset = target;
                    self.scrollbar_drag = Some(drag);
                }
                return Action::Continue;
            }
            MouseEventKind::Up(MouseButton::Left) if self.scrollbar_drag.is_some() => {
                self.scrollbar_drag = None;
                return Action::Continue;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return Action::Continue,
        }
        if let Some(scrollbar) = self
            .scrollbar
            .filter(|scrollbar| scrollbar.contains(mouse.column, mouse.row))
            && let Some(press) = scrollbar.press(mouse.column, mouse.row)
        {
            self.scrollbar_drag = Some(press.drag);
            if let Some(target) = press.target {
                self.scroll = target as usize;
            }
        } else if rect_contains(self.their_identity_button, mouse.column, mouse.row) {
            self.show_identity(IdentityView::Their);
        } else if rect_contains(self.your_identity_button, mouse.column, mouse.row) {
            self.show_identity(IdentityView::Yours);
        } else if rect_contains(self.previous_identity_button, mouse.column, mouse.row) {
            self.show_identity(IdentityView::Previous);
        } else if rect_contains(self.words_button, mouse.column, mouse.row) {
            self.focus = IdentityFocus::Words;
            self.confirm_words();
        } else if rect_contains(self.verification_input_frame, mouse.column, mouse.row) {
            self.focus = IdentityFocus::VerificationInput;
        } else if rect_contains(self.close_button, mouse.column, mouse.row) {
            self.focus = IdentityFocus::Cancel;
            self.close(cx);
        } else if rect_contains(self.primary_button, mouse.column, mouse.row) {
            self.focus = IdentityFocus::Primary;
            self.activate_primary(cx);
        } else if rect_contains(self.copy_card_button, mouse.column, mouse.row) {
            cx.queue_clipboard(self.dialog.local_verification_text.clone());
        } else if rect_contains(self.copy_key_button, mouse.column, mouse.row) {
            cx.queue_clipboard(self.dialog.target.public_key.clone());
        } else if rect_contains(self.copy_words_button, mouse.column, mouse.row) {
            let words = E2ePublicIdentity::from_hex(&self.dialog.target.public_key)
                .expect("validated verification target")
                .words_string();
            cx.queue_clipboard(words);
        }
        Action::Continue
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        security_dialog_presentation()
    }
}

fn security_dialog_presentation() -> ModePresentation {
    ModePresentation {
        coverage: Coverage::Overlay,
        chrome: Some(ChromeSpec {
            theme_mode: theme::UiMode::ServerSelect,
            status_label: "Security",
            layer: bindings::DIALOG_LAYER,
        }),
    }
}

fn native_encryption_header_style(theme: &Theme) -> extui::Style {
    if let Some(error) = theme.error.fg() {
        theme.dialog_header.with_bg(error).with_fg_rgb(0, 0, 0)
    } else {
        theme.dialog_header.patch(theme.error).with_fg_rgb(0, 0, 0)
    }
}

pub(crate) struct DevicePairMode {
    dialog: DevicePairDialog,
}

impl DevicePairMode {
    pub(crate) fn new(dialog: DevicePairDialog) -> Self {
        Self { dialog }
    }

    fn handle_event(&mut self, cx: &mut ViewCx<'_>, event: DevicePairEvent) {
        match event {
            DevicePairEvent::Consumed => {}
            DevicePairEvent::Cancel => cx.send(CoreCommand::CancelPairing),
            DevicePairEvent::Submit {
                pairing_string,
                transfer_password,
                device_name,
                overwrite_existing,
            } => cx.send(CoreCommand::SubmitDevicePair {
                pairing_string,
                transfer_password,
                device_name,
                overwrite_existing,
            }),
        }
    }
}

impl AppMode for DevicePairMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        let mut app = crate::tui::render::RenderState::new(cx);
        let area = buf.rect();
        let form_height = self.dialog.form_height(area.w);
        let Some(panel) =
            crate::tui::render::form_dialog_panel(area, form_height)
        else {
            return;
        };
        let body = crate::tui::render::draw_dialog_frame(
            panel,
            buf,
            &app.view.theme,
            "Pair a device",
        );
        self.dialog.render(body, buf, &app.view.theme);
        crate::tui::render::draw_overlay_key_preview(&mut app, bindings::FORM_LAYER, buf);
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        let event = self.dialog.handle_key(key, &cx.view.theme);
        self.handle_event(cx, event);
        Action::Continue
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        let event = self.dialog.handle_mouse(mouse, &cx.view.theme);
        self.handle_event(cx, event);
        Action::Continue
    }

    fn process_paste(&mut self, cx: &mut ViewCx<'_>, text: String) {
        self.dialog.paste(&text, &cx.view.theme);
    }

    fn process_client_event(&mut self, event: crate::client_channel::TerminalEvent) {
        match event {
            crate::client_channel::TerminalEvent::PairingFailed(error) => {
                self.dialog.pairing_failed(error);
            }
            crate::client_channel::TerminalEvent::DevicePairingIdentityExists {
                message,
                transfer_password,
            } => self.dialog.identity_exists(message, transfer_password),
            _ => {}
        }
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        form_dialog_presentation("Pair")
    }
}

pub(crate) struct DeviceLinkMode {
    dialog: DeviceLinkDialog,
}

impl DeviceLinkMode {
    pub(crate) fn new(dialog: DeviceLinkDialog) -> Self {
        Self { dialog }
    }

    fn handle_button(&mut self, cx: &mut ViewCx<'_>, button: DeviceLinkButton) {
        if let Some(value) = self.dialog.value(button) {
            cx.queue_clipboard(value.to_string());
        } else {
            cx.request_transition(ModeTransition::Pop);
        }
    }
}

impl AppMode for DeviceLinkMode {
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
        let mut app = crate::tui::render::RenderState::new(cx);
        let area = buf.rect();
        let form_height = self.dialog.form_height(area.w);
        let Some(panel) = crate::tui::render::form_dialog_panel(area, form_height) else {
            return;
        };
        let body = crate::tui::render::draw_dialog_frame(
            panel,
            buf,
            &app.view.theme,
            "Device link created",
        );
        self.dialog.render(body, buf, &app.view.theme);
        crate::tui::render::draw_overlay_key_preview(&mut app, bindings::FORM_LAYER, buf);
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if let Some(button) = self.dialog.handle_key(key, &cx.view.theme) {
            self.handle_button(cx, button);
        }
        Action::Continue
    }

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if let Some(button) = self.dialog.handle_mouse(mouse, &cx.view.theme) {
            self.handle_button(cx, button);
        }
        Action::Continue
    }

    fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
        form_dialog_presentation("Device link")
    }
}

fn form_dialog_presentation(status_label: &'static str) -> ModePresentation {
    ModePresentation {
        coverage: Coverage::Overlay,
        chrome: Some(ChromeSpec {
            theme_mode: theme::UiMode::ServerEdit,
            status_label,
            layer: bindings::FORM_LAYER,
        }),
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
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
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
        let mut body =
            crate::tui::render::draw_dialog_frame(panel, buf, theme, "Server password required");
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

    fn process_client_event(&mut self, event: crate::client_channel::TerminalEvent) {
        use crate::client_channel::TerminalEvent;

        match event {
            TerminalEvent::PairingPasswordChallenge { retry, .. } => {
                self.apply_password_challenge(retry);
            }
            TerminalEvent::PairingFailed(error) => self.apply_error(error),
            TerminalEvent::DevicePairingIdentityExists { .. } => {}
            TerminalEvent::Navigation(_) => {}
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
        parts.join(" | ")
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
    fn render(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        _now_ms: u64,
        _dirty: DirtySections,
    ) {
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
        let mut body = crate::tui::render::draw_dialog_frame(panel, buf, theme, "Upload image");
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
                        AppMode::render(self, &mut cx, buf, now_ms, DirtySections::ALL);
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

                fn process_paste(&mut self, app: &mut App, text: String) {
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
    DialogMode,
    ConfirmMode,
    NativeEncryptionWarningMode,
    E2eIdentityMode,
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

    struct TextClipboard(String);

    impl ClipboardPasteProvider for TextClipboard {
        fn read_paste(&self) -> Result<PastePayload, crate::clipboard_paste::ClipboardPasteError> {
            Ok(PastePayload::Text(self.0.clone()))
        }
    }

    fn buffer_text(buffer: &mut Buffer) -> String {
        let grid = buffer.current();
        let mut text = String::new();
        for cell in grid.cells() {
            if cell.is_handle() {
                text.push_str(&String::from_utf8_lossy(
                    grid.handle_text(*cell).unwrap_or_default(),
                ));
            } else {
                text.push_str(cell.text_inline().unwrap_or_default());
            }
        }
        text
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
        assert!(!app.test_navigation.is_empty());
    }

    fn accepted_identity() -> crate::e2e::AcceptedPeerIdentity {
        crate::e2e::AcceptedPeerIdentity {
            room_id: rpc::ids::RoomId(0x8000_0001),
            user_id: rpc::ids::UserId(42),
            identity: crate::config::E2ePeerIdentity {
                room_id: 0x8000_0001,
                user_id: 42,
                username: "Bob".to_string(),
                public_key: "11".repeat(32),
                trust_level: crate::config::E2eTrustLevel::Accepted,
            },
            trust_level: crate::config::E2eTrustLevel::Accepted,
            change_from: None,
            verified_keys: Vec::new(),
        }
    }

    #[test]
    fn verification_mismatch_renders_no_accept_action() {
        let app = test_app();
        let local = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            1,
            &[0x44; 32],
        )
        .unwrap()
        .encode();
        let mismatched = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            42,
            &[0x22; 32],
        )
        .unwrap()
        .encode();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: "Bob".to_string(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: local,
                pasted_verification_text: mismatched,
                result: None,
                error: None,
            },
            &app.view.theme,
        );
        let mut app = app;
        let mut buffer = Buffer::new(80, 24);

        mode.render(&mut app, &mut buffer, 0);
        let screen = buffer_text(&mut buffer);

        assert!(mode.has_verification_error());
        assert!(!mode.verification_passed());
        assert!(screen.contains("different public key"));
        assert!(screen.contains("Clear verification text to continue"));
        assert!(!screen.contains("Verify identity [Enter]"));
    }

    #[test]
    fn identity_tabs_show_complete_peer_and_local_word_sets() {
        let mut app = test_app();
        let local = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            1,
            &[0x22; 32],
        )
        .unwrap()
        .encode();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: String::new(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: local,
                pasted_verification_text: String::new(),
                result: None,
                error: None,
            },
            &app.view.theme,
        );

        let their_identity = mode
            .identity_lines(44)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        let prose = their_identity.replace('\n', " ");
        assert!(their_identity.contains("User 42's public identity"));
        let identity_header = mode
            .identity_lines(44)
            .into_iter()
            .find(|line| line.text == "User 42's public identity")
            .expect("identity header");
        assert_eq!(identity_header.kind, SecurityLineKind::Section);
        assert_eq!(
            identity_header.trailing.as_deref(),
            Some("User ID 42 | Room ID 0x80000001")
        );
        let mut header_buffer = Buffer::new(80, 1);
        let header_area = header_buffer.rect();
        render_security_lines(
            header_area,
            &mut header_buffer,
            &app.view.theme,
            std::slice::from_ref(&identity_header),
            0,
        );
        let rendered_header = buffer_text(&mut header_buffer);
        assert!(rendered_header.starts_with("User 42's public identity"));
        assert!(rendered_header.ends_with("User ID 42 | Room ID 0x80000001"));
        let grid = header_buffer.current();
        let metadata_x = usize::from(grid.width())
            - identity_header
                .trailing
                .as_deref()
                .expect("identity metadata")
                .len();
        assert_eq!(grid.cells()[0].style().fg(), app.view.theme.accent.fg());
        assert_eq!(
            grid.cells()[metadata_x].style().fg(),
            app.view.theme.muted.fg()
        );
        assert!(prose.contains("Verification text is a copyable form"));
        assert!(prose.contains("If clipboard sharing is unavailable"));
        assert!(!prose.contains("unlocks messages"));
        assert!(!their_identity.to_lowercase().contains("card"));
        let key_line = mode
            .identity_lines(44)
            .into_iter()
            .find(|line| line.kind == SecurityLineKind::Key)
            .expect("public key line");
        assert_eq!(
            security_line_style(&key_line, &app.view.theme).fg(),
            app.view.theme.text.fg()
        );
        for word in E2ePublicIdentity::from_hex(&"11".repeat(32))
            .unwrap()
            .words()
        {
            assert!(their_identity.split_whitespace().any(|shown| shown == word));
        }
        let mut buffer = Buffer::new(86, 32);
        mode.render(&mut app, &mut buffer, 0);
        let grid = buffer.current();
        let disabled_previous = grid.cells()[usize::from(mode.previous_identity_button.y)
            * usize::from(grid.width())
            + usize::from(mode.previous_identity_button.x)];
        assert_eq!(disabled_previous.style().fg(), app.view.theme.subtle.fg());

        mode.show_identity(IdentityView::Yours);
        let your_identity = mode
            .identity_lines(44)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(your_identity.contains("Your public identity"));
        let your_identity_header = mode
            .identity_lines(44)
            .into_iter()
            .find(|line| line.text == "Your public identity")
            .expect("local identity header");
        assert_eq!(
            your_identity_header.trailing.as_deref(),
            Some("User ID 1 | Room ID 0x80000001")
        );
        for word in E2ePublicIdentity::from_hex(&"22".repeat(32))
            .unwrap()
            .words()
        {
            assert!(your_identity.split_whitespace().any(|shown| shown == word));
        }
    }

    #[test]
    fn identity_tabs_keep_your_identity_visible_at_screenshot_size() {
        let mut app = test_app();
        let local = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            1,
            &[0x22; 32],
        )
        .unwrap()
        .encode();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: "Alice".to_string(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: local,
                pasted_verification_text: String::new(),
                result: None,
                error: None,
            },
            &app.view.theme,
        );
        let mut buffer = Buffer::new(86, 32);

        mode.render(&mut app, &mut buffer, 0);
        let their_frame = buffer_text(&mut buffer);
        assert!(their_frame.contains("Their identity"));
        assert!(their_frame.contains("Your identity"));
        assert!(their_frame.contains("Alice's public identity"));
        assert!(!their_frame.contains("Previously saved identity"));
        assert!(their_frame.contains("paste their verification text here"));
        assert!(their_frame.contains("Independent verification required"));
        assert!(their_frame.contains("I checked all 24 words"));
        assert!(!their_frame.contains("[F2]"));
        assert!(!their_frame.contains("[F3]"));
        assert!(!their_frame.contains("[F4]"));
        assert!(!their_frame.contains("[F5]"));
        assert!(!their_frame.contains("[F6]"));
        assert!(mode.scrollbar.is_some());

        let words_button = mode.words_button;
        assert!(!words_button.is_empty());
        let grid = buffer.current();
        let disabled_primary = grid.cells()[usize::from(mode.primary_button.y)
            * usize::from(grid.width())
            + usize::from(mode.primary_button.x)];
        assert_eq!(disabled_primary.style().fg(), app.view.theme.subtle.fg());
        let idle_cell = grid.cells()
            [usize::from(words_button.y) * usize::from(grid.width()) + usize::from(words_button.x)];
        assert_eq!(
            idle_cell.style().bg(),
            app.view.theme.status_section_inactive.bg()
        );
        mode.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: words_button.x,
                row: words_button.y,
                modifiers: KeyModifiers::empty(),
            },
        );
        mode.render(&mut app, &mut buffer, 0);
        let confirmed_frame = buffer_text(&mut buffer);
        assert!(mode.words_confirmed);
        assert!(confirmed_frame.contains("Words matched ✓"));
        assert!(confirmed_frame.contains("I confirmed that all 24 identity words matched"));
        assert!(confirmed_frame.contains("Verify identity [Enter]"));
        let confirmed_button = mode.words_button;
        let grid = buffer.current();
        let cell = grid.cells()[usize::from(confirmed_button.y) * usize::from(grid.width())
            + usize::from(confirmed_button.x)];
        assert_eq!(cell.style().bg(), app.view.theme.mode_compose.bg());

        mode.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(mode.identity_view, IdentityView::Yours);
        mode.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(mode.identity_view, IdentityView::Their);
        mode.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(mode.identity_view, IdentityView::Yours);
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        );
        assert_eq!(mode.identity_view, IdentityView::Their);

        mode.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(mode.identity_view, IdentityView::Yours);
        mode.render(&mut app, &mut buffer, 0);
        let your_frame = buffer_text(&mut buffer);
        assert!(your_frame.contains("Your public identity"));
        assert!(your_frame.contains("Your identity words"));
    }

    #[test]
    fn verification_focus_moves_between_input_and_the_two_footer_actions() {
        let mut app = test_app();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: "Bob".to_string(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: "chatt-e2e:v2:test".to_string(),
                pasted_verification_text: String::new(),
                result: None,
                error: None,
            },
            &app.view.theme,
        );

        assert_eq!(mode.focus, IdentityFocus::VerificationInput);
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_eq!(mode.focus, IdentityFocus::Cancel);
        let mut buffer = Buffer::new(86, 32);
        mode.render(&mut app, &mut buffer, 0);
        let frame = mode.verification_input_frame;
        let input = mode.verification_input;
        assert_eq!(frame.h, 3);
        assert_eq!(mode.copy_card_button.y + 1, frame.y);
        assert_eq!(frame.y + frame.h + 1, mode.close_button.y);
        assert_eq!(input.x, frame.x + 1);
        assert_eq!(input.w, frame.w - 2);
        let grid = buffer.current();
        let width = usize::from(grid.width());
        let top_border = grid.cells()[usize::from(frame.y) * width + usize::from(frame.x)];
        let bottom_border =
            grid.cells()[usize::from(frame.y + frame.h - 1) * width + usize::from(frame.x)];
        assert_eq!(top_border.text_inline(), Some("▄"));
        assert_eq!(bottom_border.text_inline(), Some("▀"));
        assert_eq!(
            top_border.style().fg(),
            app.view.theme.join_input_inactive.bg()
        );
        assert_eq!(top_border.style().bg(), app.view.theme.dialog_panel.bg());
        let left_padding = grid.cells()[usize::from(input.y) * width + usize::from(frame.x)];
        assert_eq!(
            left_padding.style().bg(),
            app.view.theme.join_input_inactive.bg()
        );
        let placeholder = grid.cells()[usize::from(input.y) * width + usize::from(input.x)];
        let empty_input = grid.cells()
            [usize::from(input.y) * width + usize::from(input.x + input.w.saturating_sub(1))];
        assert_eq!(placeholder.style().bg(), empty_input.style().bg());
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
        );
        assert_eq!(mode.focus, IdentityFocus::Primary);
        mode.process_input(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::empty()));
        mode.process_input(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(mode.focus, IdentityFocus::VerificationInput);

        mode.focus = IdentityFocus::Cancel;
        mode.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: frame.x + 1,
                row: frame.y,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert_eq!(mode.focus, IdentityFocus::VerificationInput);
        mode.render(&mut app, &mut buffer, 0);
        let grid = buffer.current();
        let active_top = grid.cells()[usize::from(frame.y) * width + usize::from(frame.x)];
        let active_padding = grid.cells()[usize::from(input.y) * width + usize::from(frame.x)];
        assert_eq!(
            active_top.style().fg(),
            app.view.theme.join_input_active.bg()
        );
        assert_eq!(active_top.style().bg(), app.view.theme.dialog_panel.bg());
        assert_eq!(
            active_padding.style().bg(),
            app.view.theme.join_input_active.bg()
        );
    }

    #[test]
    fn identity_esc_leaves_insert_mode_before_closing_the_dialog() {
        let mut app = test_app();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: "Bob".to_string(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: String::new(),
                pasted_verification_text: String::new(),
                result: None,
                error: None,
            },
            &app.view.theme,
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());

        assert_eq!(mode.editor.mode(), EditorMode::Insert);
        mode.process_input(&mut app, esc);
        assert_eq!(mode.editor.mode(), EditorMode::Normal);
        assert!(app.test_navigation.is_empty());

        mode.process_input(&mut app, esc);
        assert_eq!(mode.editor.mode(), EditorMode::Normal);
    }

    #[test]
    fn identity_visual_selection_returns_to_normal_before_closing_the_dialog() {
        let mut app = test_app();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: "Bob".to_string(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: String::new(),
                pasted_verification_text: "abcdef".to_string(),
                result: None,
                error: None,
            },
            &app.view.theme,
        );
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::empty());

        mode.process_input(&mut app, esc);
        assert_eq!(mode.editor.mode(), EditorMode::Normal);
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::empty()),
        );
        assert_eq!(mode.editor.mode(), EditorMode::Visual);
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::empty()),
        );

        let mut buffer = Buffer::new(86, 32);
        mode.render(&mut app, &mut buffer, 0);
        let input = mode.verification_input;
        let selection_background = app.view.theme.editor_selection_charwise.bg();
        let grid = buffer.current();
        let width = usize::from(grid.width());
        assert!((input.x..input.x + input.w).any(|x| {
            grid.cells()[usize::from(input.y) * width + usize::from(x)]
                .style()
                .bg()
                == selection_background
        }));

        mode.process_input(&mut app, esc);
        assert_eq!(mode.editor.mode(), EditorMode::Normal);
        assert!(app.test_navigation.is_empty());
    }

    #[test]
    fn identity_ctrl_v_uses_the_composer_clipboard_binding() {
        let mut app = test_app();
        let local = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            1,
            &[0x44; 32],
        )
        .unwrap()
        .encode();
        let matching = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            42,
            &[0x11; 32],
        )
        .unwrap()
        .encode();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: "Bob".to_string(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: local,
                pasted_verification_text: String::new(),
                result: None,
                error: None,
            },
            &app.view.theme,
        );
        mode.focus = IdentityFocus::Cancel;
        let ctrl_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);

        let handled = {
            let mut cx = app.view_cx();
            mode.process_clipboard_binding(&mut cx, &ctrl_v, &TextClipboard(matching.clone()))
        };

        assert!(handled);
        assert_eq!(mode.focus, IdentityFocus::VerificationInput);
        assert_eq!(mode.editor.text(), matching);
        assert_eq!(mode.dialog.result, Some(Ok(())));
    }

    #[test]
    fn verification_text_is_checked_on_paste_and_tab_never_enters_the_field() {
        let mut app = test_app();
        let local = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            1,
            &[0x44; 32],
        )
        .unwrap()
        .encode();
        let matching = crate::e2e_identity::VerificationText::new(
            &rpc::crypto::dev_server_public_key(),
            42,
            &[0x11; 32],
        )
        .unwrap()
        .encode();
        let mut mode = E2eIdentityMode::new(
            E2eIdentityOverlay {
                target: E2eIdentityTarget {
                    room_id: rpc::ids::RoomId(0x8000_0001),
                    user_id: rpc::ids::UserId(42),
                    username: "Bob".to_string(),
                    public_key: "11".repeat(32),
                    accepted: accepted_identity(),
                },
                local_verification_text: local,
                pasted_verification_text: String::new(),
                result: None,
                error: None,
            },
            &app.view.theme,
        );

        mode.process_paste(&mut app, matching.clone());
        assert_eq!(mode.dialog.result, Some(Ok(())));
        assert!(mode.verification_passed());
        mode.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(mode.identity_view, IdentityView::Yours);
        assert_eq!(mode.editor.text(), matching);

        let mut buffer = Buffer::new(86, 32);
        mode.render(&mut app, &mut buffer, 0);
        let screen = buffer_text(&mut buffer);
        assert!(screen.contains("Verification text matches this identity"));
        assert!(screen.contains("Verify identity [Enter]"));
    }

    #[test]
    fn identity_dialog_grows_beyond_the_old_height_with_eight_row_margins() {
        assert_eq!(identity_dialog_height(32), 32);
        assert_eq!(identity_dialog_height(60), 44);
        assert_eq!(identity_dialog_height(100), 84);
    }

    #[test]
    fn key_and_word_rows_are_centered() {
        let theme = test_app().view.theme;
        let mut buffer = Buffer::new(20, 2);
        let lines = vec![
            SecurityDialogLine::new("abcd", SecurityLineKind::Key),
            SecurityDialogLine::new("one  two", SecurityLineKind::Words),
        ];

        render_security_lines(buffer.rect(), &mut buffer, &theme, &lines, 0);

        let screen = buffer_text(&mut buffer);
        assert_eq!(screen.find("abcd"), Some(8));
        assert_eq!(screen.find("one  two"), Some(20 + 6));
    }

    #[test]
    fn generated_identity_word_rows_keep_one_fixed_grid_width() {
        let mut lines = Vec::new();
        append_public_identity_word_lines(&mut lines, &"11".repeat(32), 50);
        let widths = lines
            .iter()
            .map(|line| UnicodeWidthStr::width(line.text.as_str()))
            .collect::<Vec<_>>();

        assert!(widths.len() > 1);
        assert!(widths.iter().all(|width| *width == widths[0]));
        assert!(widths[0] <= 50);
    }

    #[test]
    fn confirmation_buttons_show_yes_and_no_shortcuts() {
        let mut app = test_app();
        let mut mode = ConfirmMode::new("Delete message?", "Delete", "Cancel", |_| {
            ConfirmDisposition::Close
        });
        let mut buffer = Buffer::new(80, 24);

        mode.render(&mut app, &mut buffer, 0);

        let screen: String = buffer
            .current()
            .cells()
            .iter()
            .filter_map(|cell| cell.text_inline())
            .collect();
        assert!(
            screen.contains("Cancel [n]"),
            "cancel hint missing: {screen}"
        );
        assert!(
            screen.contains("Delete [y]"),
            "confirm hint missing: {screen}"
        );
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
        assert!(!app.test_navigation.is_empty());
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
        assert!(app.test_navigation.is_empty());
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
        assert!(app.test_navigation.is_empty());

        // Second Esc: from normal mode this cancels the dialog.
        mode.process_input(&mut app, esc);
        assert!(!app.test_navigation.is_empty());
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
        assert!(!app.test_navigation.is_empty());
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
            open_password: String::new(),
            pairing_code: None,
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
        assert!(matches!(
            app.take_terminal_event(),
            Some(crate::client_channel::TerminalEvent::Navigation(
                crate::client_channel::NavigationEvent::CloseOverlay
            ))
        ));
    }

    #[test]
    fn password_prompt_consumes_retry_as_local_feedback() {
        let mut prompt = PasswordPromptMode::new(false);
        prompt.input = "bad".to_string();
        prompt.submitting = true;

        prompt.process_client_event(
            crate::client_channel::TerminalEvent::PairingPasswordChallenge { retry: true },
        );

        assert_eq!(
            prompt.feedback,
            PasswordFeedback::Error("Incorrect password; try again".to_string())
        );
        assert!(!prompt.submitting);
        assert!(prompt.input.is_empty());
    }
}
