use extui::{Buffer, Ellipsis, Rect};
use extui_editor::Editor;
use rpc::ids::UserId;
use unicode_width::UnicodeWidthStr;

use super::volume_db_label;
use crate::{
    config::{MAX_USER_VOLUME_DB, MIN_USER_VOLUME_DB, USER_VOLUME_DB_STEP, snap_user_volume_db},
    theme::Theme,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum UserVolumeEvent {
    Consumed,
    Preview {
        user_id: UserId,
        value_db: f32,
    },
    Cancel {
        user_id: UserId,
        username: String,
        original_db: f32,
    },
    Save {
        user_id: UserId,
        username: String,
        value_db: f32,
    },
    Invalid(String),
}

pub(crate) struct UserVolumeDialog {
    user_id: UserId,
    user_name: String,
    original_db: f32,
    value_db: f32,
    editor: Editor,
    error: Option<String>,
}

impl UserVolumeDialog {
    pub(crate) fn new(user_id: UserId, user_name: String, value_db: f32, theme: &Theme) -> Self {
        let mut editor = volume_input_editor(value_db, theme);
        editor.enter_insert_mode();
        Self {
            user_id,
            user_name,
            original_db: value_db,
            value_db,
            editor,
            error: None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: extui::event::KeyEvent) -> UserVolumeEvent {
        use extui::event::{KeyCode, KeyEventKind};

        if matches!(key.kind, KeyEventKind::Release) {
            return UserVolumeEvent::Consumed;
        }

        match key.code {
            KeyCode::Esc => UserVolumeEvent::Cancel {
                user_id: self.user_id,
                username: self.user_name.clone(),
                original_db: self.original_db,
            },
            KeyCode::Enter => match self.commit_editor() {
                Ok(value_db) => UserVolumeEvent::Save {
                    user_id: self.user_id,
                    username: self.user_name.clone(),
                    value_db,
                },
                Err(error) => UserVolumeEvent::Invalid(error),
            },
            KeyCode::Left | KeyCode::Down => self.adjust(-1),
            KeyCode::Right | KeyCode::Up => self.adjust(1),
            _ if self.editor.send_key(&key) => match self.commit_editor() {
                Ok(value_db) => UserVolumeEvent::Preview {
                    user_id: self.user_id,
                    value_db,
                },
                Err(error) => {
                    self.error = Some(error.clone());
                    UserVolumeEvent::Invalid(error)
                }
            },
            _ => UserVolumeEvent::Consumed,
        }
    }

    pub(crate) fn mark_save_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if area.w < 24 || area.h < 6 {
            return;
        }

        let width = area.w.min(58);
        let height = area.h.min(7);
        let panel = Rect {
            x: area.x + area.w.saturating_sub(width) / 2,
            y: area.y + area.h.saturating_sub(height) / 2,
            w: width,
            h: height,
        };
        let mut body = crate::tui::render::draw_dialog_frame(
            panel,
            buf,
            theme,
            &format!("Local volume: {}", self.user_name),
        );
        body.take_top(1)
            .with(theme.dialog_panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(
                buf,
                &format!(
                    "User {}  saved {}",
                    self.user_id.0,
                    volume_db_label(self.original_db)
                ),
            );

        self.render_slider(body.take_top(1), buf, theme);
        self.render_editor_row(body.take_top(1), buf, theme);
        self.render_footer(body.take_top(1), buf, theme);
    }

    fn adjust(&mut self, delta_steps: isize) -> UserVolumeEvent {
        let next = self.value_db + delta_steps as f32 * USER_VOLUME_DB_STEP;
        self.value_db = snap_user_volume_db(next);
        self.editor
            .set_lines(&format_volume_db_value(self.value_db));
        self.editor.enter_insert_mode();
        self.error = None;
        UserVolumeEvent::Preview {
            user_id: self.user_id,
            value_db: self.value_db,
        }
    }

    fn parse_editor_value(&self) -> Result<f32, String> {
        parse_user_volume_db(&self.editor.text())
    }

    fn commit_editor(&mut self) -> Result<f32, String> {
        let value = self.parse_editor_value()?;
        self.value_db = value;
        self.error = None;
        Ok(value)
    }

    fn render_slider(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let mut row = area;
        let label = volume_db_label(self.value_db);
        let label_width = label.width() as u16 + 1;
        let slider_width = row.w.saturating_sub(label_width).max(8);
        row.take_left(slider_width as i32)
            .with(theme.dialog_panel.patch(theme.good))
            .with(Ellipsis(true))
            .text(buf, &volume_slider(self.value_db, slider_width));
        row.with(theme.dialog_panel.patch(theme.text))
            .with(Ellipsis(true))
            .text(buf, &format!(" {label}"));
    }

    fn render_editor_row(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let mut row = area;
        row.take_left(8)
            .with(theme.dialog_panel.patch(theme.muted))
            .text(buf, "Offset");
        let field_width = row.w.min(14);
        let mut field = row.take_left(field_width as i32);
        field.with(theme.join_input_boundary_active).fill(buf);
        if field.w > 2 {
            field
                .take_left(1)
                .with(theme.join_input_boundary_active)
                .text(buf, " ");
            field
                .take_right(1)
                .with(theme.join_input_boundary_active)
                .text(buf, " ");
        }
        field.with(theme.join_input_active).fill(buf);
        self.editor.render(field, buf);
        row.with(theme.dialog_panel.patch(theme.muted))
            .with(Ellipsis(true))
            .text(buf, " dB");
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if let Some(error) = &self.error {
            area.with(theme.dialog_panel.patch(theme.error))
                .with(Ellipsis(true))
                .text(buf, error);
        } else {
            area.with(theme.dialog_panel.patch(theme.subtle))
                .with(Ellipsis(true))
                .text(buf, &format!("Pending {}", volume_db_label(self.value_db)));
        }
    }
}

fn volume_input_editor(value_db: f32, theme: &Theme) -> Editor {
    let mut editor = Editor::new();
    editor.set_single_line(true);
    editor.set_wrap(false);
    editor.set_height_bounds(1, 1);
    editor.set_theme(theme.join_input_editor_theme());
    editor.set_lines(&format_volume_db_value(value_db));
    editor.enter_insert_mode();
    editor
}

fn parse_user_volume_db(value: &str) -> Result<f32, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("volume dB value is empty".to_string());
    }
    let parsed = value
        .parse::<f32>()
        .map_err(|_| "volume dB value must be a number".to_string())?;
    if !(MIN_USER_VOLUME_DB..=MAX_USER_VOLUME_DB).contains(&parsed) {
        return Err(format!(
            "volume dB value must be between {:.1} and {:.1}",
            MIN_USER_VOLUME_DB, MAX_USER_VOLUME_DB
        ));
    }
    Ok(snap_user_volume_db(parsed))
}

fn format_volume_db_value(value_db: f32) -> String {
    format!("{value_db:.1}")
}

fn volume_slider(value_db: f32, width: u16) -> String {
    let inner = width.saturating_sub(2).max(1) as usize;
    let span = MAX_USER_VOLUME_DB - MIN_USER_VOLUME_DB;
    let value_ratio = ((value_db - MIN_USER_VOLUME_DB) / span).clamp(0.0, 1.0);
    let zero_ratio = ((0.0 - MIN_USER_VOLUME_DB) / span).clamp(0.0, 1.0);
    let value_index = (value_ratio * inner.saturating_sub(1) as f32).round() as usize;
    let zero_index = (zero_ratio * inner.saturating_sub(1) as f32).round() as usize;

    let mut out = String::with_capacity(inner + 2);
    out.push('[');
    for index in 0..inner {
        if index == value_index {
            out.push('|');
        } else if index == zero_index {
            out.push('0');
        } else if index < value_index {
            out.push('=');
        } else {
            out.push('-');
        }
    }
    out.push(']');
    out
}
