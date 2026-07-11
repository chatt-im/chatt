//! The room settings popup: per-room download and persistence overrides,
//! shown as a dialog overlay above the in-room view.

use extui::{
    Buffer,
    event::{KeyEvent, MouseEvent},
};

use crate::{
    app::{App, RoomSettingsDraft, RoomSettingsEvent, command::CoreCommand},
    bindings, theme,
    tui::{
        Action,
        mode::{
            AppMode, ChromeSpec, Coverage, ModePresentation, ModeTransition, ViewCx, is_quit_key,
        },
    },
};

pub(crate) struct RoomSettingsMode {
    draft: Option<RoomSettingsDraft>,
}

impl RoomSettingsMode {
    pub(crate) fn new(draft: RoomSettingsDraft) -> Self {
        Self { draft: Some(draft) }
    }

    fn handle_event(&mut self, cx: &mut ViewCx<'_>, event: RoomSettingsEvent) {
        match event {
            RoomSettingsEvent::Consumed => {}
            RoomSettingsEvent::Cancel => cx.request_transition(ModeTransition::Pop),
            RoomSettingsEvent::Save => {
                if let Some(draft) = self.draft.take() {
                    cx.send(CoreCommand::SaveRoomSettings(draft));
                }
            }
        }
    }

    fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        if is_quit_key(&key) {
            return Action::Quit;
        }
        if let Some(draft) = self.draft.as_mut() {
            let event = draft.handle_key(key, &cx.view.theme);
            self.handle_event(cx, event);
        }
        Action::Continue
    }

    fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        if let Some(draft) = self.draft.as_mut() {
            let event = draft.handle_mouse(mouse, &cx.view.theme);
            self.handle_event(cx, event);
        }
        Action::Continue
    }
}

impl AppMode for RoomSettingsMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, _now_ms: u64) {
        let area = buf.rect();
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let Some(panel) = crate::tui::render::form_dialog_panel(area, draft.form_height()) else {
            return;
        };
        let body =
            crate::tui::render::draw_form_dialog_frame(panel, buf, &app.view.theme, &draft.title());
        draft.render(body, buf, &app.view.theme);
        crate::tui::render::draw_overlay_key_preview(app, bindings::FORM_LAYER, buf);
    }

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        let action = {
            let mut cx = app.view_cx();
            self.process_input_cx(&mut cx, key)
        };
        app.drain_core_commands();
        action
    }

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        let action = {
            let mut cx = app.view_cx();
            self.process_mouse_cx(&mut cx, mouse)
        };
        app.drain_core_commands();
        action
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
