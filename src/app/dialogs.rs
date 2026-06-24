use extui_editor::Editor;
use rpc::ids::UserId;

use super::{format_volume_db_value, parse_user_volume_db, volume_input_editor};
use crate::config::{USER_VOLUME_DB_STEP, snap_user_volume_db};

pub(crate) struct UserVolumeDialog {
    pub(crate) user_id: UserId,
    pub(crate) user_name: String,
    pub(crate) original_db: f32,
    pub(crate) value_db: f32,
    pub(crate) editor: Editor,
    pub(crate) error: Option<String>,
}

impl UserVolumeDialog {
    pub(crate) fn new(user_id: UserId, user_name: String, value_db: f32) -> Self {
        let mut editor = volume_input_editor(value_db);
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

    pub(crate) fn adjust(&mut self, delta_steps: isize) {
        let next = self.value_db + delta_steps as f32 * USER_VOLUME_DB_STEP;
        self.value_db = snap_user_volume_db(next);
        self.editor
            .set_lines(&format_volume_db_value(self.value_db));
        self.editor.enter_insert_mode();
        self.error = None;
    }

    pub(crate) fn parse_editor_value(&self) -> Result<f32, String> {
        parse_user_volume_db(&self.editor.text())
    }

    pub(crate) fn apply_editor_value(&mut self) -> Result<f32, String> {
        let value = self.parse_editor_value()?;
        self.value_db = value;
        self.error = None;
        Ok(value)
    }
}
