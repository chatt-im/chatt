use extui::{
    Buffer, Ellipsis, HAlign, Rect,
    event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent},
    vt::Modifier,
};

use crate::{
    app::{App, UserVolumeDialog},
    theme::Theme,
    tui::{
        Action,
        mode::{AppMode, ModePresentation, ModeTransition, is_quit_key},
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
    retry: bool,
}

impl PasswordPromptMode {
    pub(crate) fn new(retry: bool) -> Self {
        Self {
            input: String::new(),
            retry,
        }
    }
}

impl AppMode for PasswordPromptMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let theme = &app.theme;
        let area = buf.rect();
        if area.w < 24 || area.h < 6 {
            return;
        }
        let width = area.w.min(54).max(24);
        let height = area.h.min(6);
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        buf.clear_rect(panel, theme.dialog_panel);

        let mut rows = panel.inset(2, 1);
        let prompt = if self.retry {
            "Incorrect password, try again"
        } else {
            "Server password required"
        };
        rows.take_top(1)
            .with(theme.dialog_header | Modifier::BOLD)
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, prompt);
        rows.take_top(1);
        let masked = "*".repeat(self.input.chars().count());
        rows.take_top(1)
            .with(theme.dialog_panel.patch(theme.selected_focused))
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, &masked);
        rows.take_top(1);
        rows.take_top(1)
            .with(theme.dialog_panel.patch(theme.muted))
            .with(HAlign::Center)
            .with(Ellipsis(true))
            .text(buf, "Enter to submit · Esc to cancel");
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        match key.code {
            KeyCode::Esc => app.cancel_open_pairing(),
            KeyCode::Enter => {
                let password = std::mem::take(&mut self.input);
                app.submit_open_pair_password(password);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.push(ch);
            }
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

fn draw_button(area: Rect, buf: &mut Buffer, style: extui::Style, label: &str) {
    area.with(style).fill(buf);
    area.with(style)
        .with(HAlign::Center)
        .with(Ellipsis(true))
        .text(buf, &format!("[ {label} ]"));
}
