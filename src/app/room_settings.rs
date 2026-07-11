use extui::{Buffer, Rect, event::KeyEvent, event::MouseEvent};
use rpc::ids::RoomId;

use crate::{
    config::{Config, DownloadMode, FileOverrides, HistoryOverrides, RoomOverrides, ServerEntry},
    settings::{
        DownloadChoice, OverrideToggle, download_path_error, mb_limit_error, mb_limit_text,
        parse_mb_limit,
    },
    theme::Theme,
    tui::form::{FormAction, FormFieldKind, FormMouseIntent},
    ui::form::{
        self, ActionButton, Commit, DetailForm, FieldIntent, Form, FormSurface,
        State as UiFormState,
    },
};

const LABEL_WIDTH: u16 = 12;
const DOWNLOADS_SECTION: &str = "Downloads";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoomSettingsButton {
    Save,
    Cancel,
}

const ACTIONS: [ActionButton<'static, RoomSettingsButton>; 2] = [
    ActionButton {
        key: "Save",
        label: "Save",
        value: RoomSettingsButton::Save,
        help: "Persist these room overrides to chatt.toml.",
    },
    ActionButton {
        key: "Cancel",
        label: "Cancel",
        value: RoomSettingsButton::Cancel,
        help: "Discard this edit and return to the room.",
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoomSettingsEvent {
    Consumed,
    Cancel,
    Save,
}

/// Editable per-room download and persistence overrides for one room of one
/// server, shown in the room settings popup. Unset (`inherit`) fields resolve
/// through the server overrides to the global config.
pub(crate) struct RoomSettingsDraft {
    server_label: String,
    room_id: RoomId,
    room_name: String,
    download_choice: DownloadChoice,
    download_path: String,
    receive_limit: String,
    history_choice: OverrideToggle,
    history_location: String,
    /// Server-level effective values, shown as what `inherit` resolves to.
    inherited_download_mode: DownloadMode,
    inherited_receive_limit: String,
    inherited_history_on: bool,
    form: UiFormState,
}

impl RoomSettingsDraft {
    pub(crate) fn from_config(
        config: &Config,
        server: &ServerEntry,
        room_id: RoomId,
        room_name: String,
    ) -> Self {
        let overrides = server
            .rooms
            .iter()
            .find(|room| room.room_id == room_id)
            .cloned()
            .unwrap_or(RoomOverrides {
                room_id,
                ..Default::default()
            });
        let download_choice = DownloadChoice::from_override(overrides.files.download);
        let download_path = overrides.files.download_dir.clone().unwrap_or_default();
        let inherited_files = config.effective_files(server, None);
        Self {
            server_label: server.label.clone(),
            room_id,
            room_name,
            download_choice,
            download_path,
            receive_limit: mb_limit_text(overrides.files.max_download_mb),
            history_choice: OverrideToggle::from_option(overrides.history.enabled),
            history_location: overrides.history.location.clone().unwrap_or_default(),
            inherited_download_mode: inherited_files.target.mode(),
            inherited_receive_limit: mb_limit_text(Some(
                inherited_files.max_download_bytes / crate::config::MIB,
            )),
            inherited_history_on: config.effective_history(server, None).enabled,
            form: form::state_with_focus(
                config.ui.default_bindings,
                DOWNLOADS_SECTION,
                "Downloads",
            ),
        }
    }

    pub(crate) fn server_label(&self) -> &str {
        &self.server_label
    }

    pub(crate) fn title(&self) -> String {
        format!("Room Settings — {}", self.room_name)
    }

    /// The number of form rows the dialog body currently lays out.
    pub(crate) fn form_height(&self) -> u16 {
        9 + u16::from(self.download_choice.shows_path())
    }

    pub(crate) fn to_overrides(&self) -> Result<RoomOverrides, String> {
        let mut draft = self.clone_values();
        if let Some(field) = self.form.active_text() {
            draft.drive(
                &Theme::tomorrow_night(),
                FieldIntent::None,
                Some((field, self.form.text())),
                None,
            );
        }
        let download_dir = if draft.download_choice == DownloadChoice::Persistent {
            let path = draft.download_path.trim();
            if path.is_empty() {
                return Err(
                    "download path cannot be empty while downloads are saved to disk".to_string(),
                );
            }
            Some(path.to_string())
        } else {
            None
        };
        Ok(RoomOverrides {
            room_id: self.room_id,
            files: FileOverrides {
                download: draft.download_choice.to_override(),
                download_dir,
                max_download_mb: parse_mb_limit(&draft.receive_limit)?,
            },
            history: HistoryOverrides {
                enabled: draft.history_choice.to_option(),
                location: {
                    let location = draft.history_location.trim();
                    (!location.is_empty()).then(|| location.to_string())
                },
            },
        })
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent, theme: &Theme) -> RoomSettingsEvent {
        let kind = self.form.focused_kind();
        let text_focused = kind == FormFieldKind::Text;
        let event = self.form.handle_key(key, kind);
        match event.action {
            FormAction::None | FormAction::TextChanged | FormAction::Scrolled => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                RoomSettingsEvent::Consumed
            }
            FormAction::Cancel => RoomSettingsEvent::Cancel,
            FormAction::FocusMoved => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                RoomSettingsEvent::Consumed
            }
            FormAction::Adjust(delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
                RoomSettingsEvent::Consumed
            }
            FormAction::ActivateNextInsert => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                self.move_focus(theme, 1);
                self.form.enter_insert_mode();
                RoomSettingsEvent::Consumed
            }
            FormAction::MoveFocus(delta) => {
                self.move_focus(theme, delta);
                RoomSettingsEvent::Consumed
            }
            FormAction::Activate if text_focused => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                self.move_focus(theme, 1);
                RoomSettingsEvent::Consumed
            }
            FormAction::Activate => self
                .drive(theme, FieldIntent::Activate, event.commit, None)
                .map(room_settings_button_event)
                .unwrap_or(RoomSettingsEvent::Consumed),
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent, theme: &Theme) -> RoomSettingsEvent {
        let event = self.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::None => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                RoomSettingsEvent::Consumed
            }
            FormMouseIntent::Activate(_) => self
                .drive(theme, FieldIntent::Activate, event.commit, None)
                .map(room_settings_button_event)
                .unwrap_or(RoomSettingsEvent::Consumed),
            FormMouseIntent::Adjust(_, delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
                RoomSettingsEvent::Consumed
            }
            FormMouseIntent::Text(_, _, column) => {
                self.drive(theme, FieldIntent::None, event.commit, Some(column));
                RoomSettingsEvent::Consumed
            }
            FormMouseIntent::PickerItem(_, _) => RoomSettingsEvent::Consumed,
        }
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        let mut body = area;
        let detail_area = form::take_detail_area(&mut body, buf, theme, FormSurface::Dialog);
        self.form.begin_frame(body);
        let detail = {
            let core = Form::new(
                &mut self.form,
                Some(buf),
                theme,
                false,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(LABEL_WIDTH)
            .with_surface(FormSurface::Dialog);
            let mut form = DetailForm::new(core);
            let values = RoomSettingsValues {
                download_choice: &mut self.download_choice,
                download_path: &mut self.download_path,
                receive_limit: &mut self.receive_limit,
                history_choice: &mut self.history_choice,
                history_location: &mut self.history_location,
                inherited_download_mode: self.inherited_download_mode,
                inherited_receive_limit: &self.inherited_receive_limit,
                inherited_history_on: self.inherited_history_on,
            };
            room_settings_ui(&mut form, values);
            form.detail().cloned()
        };
        self.form.finish_frame();
        if let Some(area) = detail_area {
            form::draw_detail(area, buf, theme, detail.as_ref());
        }
    }

    fn move_focus(&mut self, theme: &Theme, delta: isize) {
        let commit = self.form.move_focus(delta);
        self.drive(theme, FieldIntent::None, commit, None);
    }

    fn drive(
        &mut self,
        theme: &Theme,
        intent: FieldIntent,
        commit: Option<Commit>,
        focus_column: Option<u16>,
    ) -> Option<RoomSettingsButton> {
        let viewport = self.form.viewport();
        self.form.begin_frame(viewport);
        let activated = {
            let core = Form::new(
                &mut self.form,
                None,
                theme,
                false,
                intent,
                commit,
                focus_column,
            )
            .with_label_width(LABEL_WIDTH)
            .with_surface(FormSurface::Dialog);
            let mut form = DetailForm::new(core);
            let values = RoomSettingsValues {
                download_choice: &mut self.download_choice,
                download_path: &mut self.download_path,
                receive_limit: &mut self.receive_limit,
                history_choice: &mut self.history_choice,
                history_location: &mut self.history_location,
                inherited_download_mode: self.inherited_download_mode,
                inherited_receive_limit: &self.inherited_receive_limit,
                inherited_history_on: self.inherited_history_on,
            };
            room_settings_ui(&mut form, values)
        };
        self.form.finish_frame();
        activated
    }

    fn clone_values(&self) -> Self {
        Self {
            server_label: self.server_label.clone(),
            room_id: self.room_id,
            room_name: self.room_name.clone(),
            download_choice: self.download_choice,
            download_path: self.download_path.clone(),
            receive_limit: self.receive_limit.clone(),
            history_choice: self.history_choice,
            history_location: self.history_location.clone(),
            inherited_download_mode: self.inherited_download_mode,
            inherited_receive_limit: self.inherited_receive_limit.clone(),
            inherited_history_on: self.inherited_history_on,
            form: form::state_with_focus(
                crate::config::FormBindings::Standard,
                DOWNLOADS_SECTION,
                "Downloads",
            ),
        }
    }
}

struct RoomSettingsValues<'a> {
    download_choice: &'a mut DownloadChoice,
    download_path: &'a mut String,
    receive_limit: &'a mut String,
    history_choice: &'a mut OverrideToggle,
    history_location: &'a mut String,
    inherited_download_mode: DownloadMode,
    inherited_receive_limit: &'a str,
    inherited_history_on: bool,
}

fn room_settings_ui(
    form: &mut DetailForm<'_>,
    values: RoomSettingsValues<'_>,
) -> Option<RoomSettingsButton> {
    form.section(DOWNLOADS_SECTION);
    let inherited_download_mode = values.inherited_download_mode;
    if form
        .choice_value(
            "Downloads",
            values.download_choice,
            &DownloadChoice::ALL,
            |choice| choice.label(inherited_download_mode),
        )
        .is_focus()
    {
        form.set_help("How files in this room are handled: inherited from server settings, off, kept in memory, or saved to disk.");
    }
    if values.download_choice.shows_path()
        && form
            .text("Path", values.download_path, |value| {
                download_path_error(true, value)
            })
            .is_focus()
    {
        form.set_help("Directory where files received in this room are saved.");
    }
    if form
        .text_with_placeholder(
            "Limit",
            values.receive_limit,
            Some(values.inherited_receive_limit),
            |value| mb_limit_error(value),
        )
        .is_focus()
    {
        form.set_help("Maximum file size accepted in this room, in MiB. Empty inherits the server-effective limit shown in the field.");
    }
    form.section("Persistence");
    let inherited_history_on = values.inherited_history_on;
    if form
        .choice_value(
            "Persistence",
            values.history_choice,
            &OverrideToggle::ALL,
            |choice| choice.label(inherited_history_on),
        )
        .is_focus()
    {
        form.set_help("Controls whether chat history for this room is persisted, inherited, or disabled here.");
    }
    if form
        .text("Location", values.history_location, |_| None)
        .is_focus()
    {
        form.set_help("Base directory for this room's persisted chat log. Empty inherits the server-effective location.");
    }
    form.spacer(1);
    form.actions(&ACTIONS).activated
}

fn room_settings_button_event(button: RoomSettingsButton) -> RoomSettingsEvent {
    match button {
        RoomSettingsButton::Save => RoomSettingsEvent::Save,
        RoomSettingsButton::Cancel => RoomSettingsEvent::Cancel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with_override() -> ServerEntry {
        let mut server = ServerEntry::default();
        server.rooms = vec![RoomOverrides {
            room_id: RoomId(3),
            files: FileOverrides {
                download: Some(DownloadMode::Persistent),
                download_dir: Some("/room/dl".to_string()),
                max_download_mb: Some(300),
            },
            history: HistoryOverrides {
                enabled: Some(true),
                location: Some("/tmp/.chatt-data".to_string()),
            },
        }];
        server
    }

    #[test]
    fn draft_round_trips_inherit_and_explicit_values() {
        let config = Config::default();
        let server = server_with_override();

        let draft = RoomSettingsDraft::from_config(&config, &server, RoomId(3), "dev".to_string());
        let overrides = draft.to_overrides().unwrap();
        assert_eq!(overrides, server.rooms[0]);

        let draft =
            RoomSettingsDraft::from_config(&config, &server, RoomId(9), "other".to_string());
        let overrides = draft.to_overrides().unwrap();
        assert_eq!(overrides.room_id, RoomId(9));
        assert!(overrides.is_empty());
    }

    #[test]
    fn empty_limit_uses_server_effective_limit_placeholder() {
        let config = Config::default();
        let mut server = ServerEntry::default();
        server.files.max_download_mb = Some(75);

        let draft =
            RoomSettingsDraft::from_config(&config, &server, RoomId(9), "other".to_string());

        assert!(draft.receive_limit.is_empty());
        assert_eq!(draft.inherited_receive_limit, "75");
        assert!(draft.to_overrides().unwrap().is_empty());
    }

    #[test]
    fn invalid_receive_limit_is_rejected() {
        let config = Config::default();
        let server = ServerEntry::default();
        let mut draft =
            RoomSettingsDraft::from_config(&config, &server, RoomId(1), "lobby".to_string());
        draft.receive_limit = "fast".to_string();

        assert!(draft.to_overrides().is_err());
    }
}
