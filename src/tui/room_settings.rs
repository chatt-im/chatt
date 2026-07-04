//! The room settings popup: per-room download and persistence overrides,
//! shown as a dialog overlay above the in-room view.

use extui::{
    Buffer,
    event::{KeyEvent, MouseEvent},
};

use crate::{
    app::{App, RoomSettingsDraft, RoomSettingsEvent},
    bindings, theme,
    tui::{
        Action,
        mode::{AppMode, ChromeSpec, Coverage, ModePresentation, is_quit_key},
    },
};

pub(crate) struct RoomSettingsMode {
    draft: RoomSettingsDraft,
}

impl RoomSettingsMode {
    pub(crate) fn new(draft: RoomSettingsDraft) -> Self {
        Self { draft }
    }

    fn handle_event(&mut self, app: &mut App, event: RoomSettingsEvent) {
        match event {
            RoomSettingsEvent::Consumed => {}
            RoomSettingsEvent::Cancel => app.pop_mode(),
            RoomSettingsEvent::Save => app.save_room_settings(&self.draft),
        }
    }
}

impl AppMode for RoomSettingsMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let area = buf.rect();
        let Some(panel) = crate::tui::render::form_dialog_panel(area, self.draft.form_height())
        else {
            return;
        };
        let body =
            crate::tui::render::draw_form_dialog_frame(panel, buf, &app.theme, &self.draft.title());
        self.draft.render(body, buf, &app.theme);
        crate::tui::render::draw_overlay_key_preview(app, bindings::FORM_LAYER, buf);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        let event = self.draft.handle_key(key, &app.theme);
        self.handle_event(app, event);
        Action::Continue
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        let event = self.draft.handle_mouse(mouse, &app.theme);
        self.handle_event(app, event);
        Action::Continue
    }

    fn presentation(&self, _app: &App) -> ModePresentation {
        ModePresentation {
            coverage: Coverage::Overlay,
            chrome: Some(ChromeSpec {
                theme_mode: theme::UiMode::ServerEdit,
                status_label: "Room",
                layer: bindings::FORM_LAYER,
            }),
        }
    }
}
