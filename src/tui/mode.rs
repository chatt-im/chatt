use extui::{
    Buffer,
    event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent},
};
use extui_bindings::LayerId;

use crate::{
    app::{App, AppEvent, RoomSession, command::CoreCommand},
    config::Config,
    theme,
    tui::{Action, view::ClientView},
};

/// The state and command sink available to render-thread mode code.
///
/// The session is deliberately read-only. Once clients render on their own
/// threads this reference will be backed by a scoped `RwLock` read guard.
#[allow(dead_code)]
pub(crate) struct ViewCx<'a> {
    pub(crate) view: &'a mut ClientView,
    pub(crate) session: &'a RoomSession,
    pub(crate) config: &'a Config,
    pub(crate) commands: &'a mut Vec<CoreCommand>,
}

#[allow(dead_code)]
impl ViewCx<'_> {
    pub(crate) fn send(&mut self, command: CoreCommand) {
        self.commands.push(command);
    }
}

/// Whether `key` is the global quit chord (Ctrl-C). Overlay modes check this so
/// quit is never trapped by a modal.
pub(crate) fn is_quit_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Coverage {
    FullScreen,
    Overlay,
}

#[derive(Clone, Copy)]
pub(crate) struct ChromeSpec {
    pub(crate) theme_mode: theme::UiMode,
    pub(crate) status_label: &'static str,
    pub(crate) layer: LayerId,
}

#[derive(Clone, Copy)]
pub(crate) struct ModePresentation {
    pub(crate) coverage: Coverage,
    pub(crate) chrome: Option<ChromeSpec>,
}

impl ModePresentation {
    pub(crate) fn full_screen(chrome: ChromeSpec) -> Self {
        Self {
            coverage: Coverage::FullScreen,
            chrome: Some(chrome),
        }
    }

    pub(crate) const OVERLAY: Self = Self {
        coverage: Coverage::Overlay,
        chrome: None,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitReason {
    Popped,
    Replaced,
    Reset,
}

/// A screen or overlay on the mode stack.
///
/// One dispatch may request at most one transition. The stack applies that
/// transition only after the borrowed active mode returns.
pub(crate) trait AppMode {
    fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64);

    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action;

    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        let _ = (app, mouse);
        Action::Continue
    }

    fn process_paste(&mut self, app: &mut App, text: String) {
        let _ = (app, text);
    }

    fn process_app_event(&mut self, app: &mut App, event: AppEvent) -> Option<AppEvent> {
        let _ = app;
        Some(event)
    }

    fn on_enter(&mut self, _app: &mut App) {}

    fn on_exit(&mut self, _app: &mut App, _reason: ExitReason) {}

    fn presentation(&self, app: &App) -> ModePresentation;
}

pub(crate) enum ModeTransition {
    Set(Box<dyn AppMode>),
    Push(Box<dyn AppMode>),
    Replace(Box<dyn AppMode>),
    Pop,
}

/// The single deferred transition slot.
///
/// Requesting twice during one dispatch is a programming error. Keeping this a
/// slot rather than a queue makes event ordering explicit at the runtime loop.
#[derive(Default)]
pub(crate) struct PendingTransition(Option<ModeTransition>);

impl PendingTransition {
    pub(crate) fn request(&mut self, transition: ModeTransition) {
        debug_assert!(
            self.0.is_none(),
            "a dispatch requested multiple mode transitions"
        );
        if self.0.is_none() {
            self.0 = Some(transition);
        }
    }

    pub(crate) fn take(&mut self) -> Option<ModeTransition> {
        self.0.take()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_none()
    }
}

/// Owns navigation invariants and lifecycle for all screens and overlays.
pub(crate) struct ModeStack {
    modes: Vec<Box<dyn AppMode>>,
}

impl ModeStack {
    pub(crate) fn new(mut root: Box<dyn AppMode>, app: &mut App) -> Self {
        root.on_enter(app);
        Self { modes: vec![root] }
    }

    pub(crate) fn active_mut(&mut self) -> &mut dyn AppMode {
        self.modes
            .last_mut()
            .map(Box::as_mut)
            .expect("mode stack always has a root")
    }

    /// Gives the active mode first chance to consume an application event. Most
    /// events are global and fall through to [`App`], while modal flows can keep
    /// their own transient UI state local.
    pub(crate) fn process_app_event(&mut self, app: &mut App, event: AppEvent) {
        if let Some(event) = self.active_mut().process_app_event(app, event) {
            app.handle_app_event(event);
        }
    }

    /// Applies at most one requested transition. A root pop is an explicit
    /// no-op; navigation must use `Set` to replace the root.
    pub(crate) fn apply_pending(&mut self, app: &mut App) {
        let Some(transition) = app.take_mode_transition() else {
            return;
        };

        // Chords never cross a navigation boundary, including overlays.
        app.view.chrome.binding.pending_chord = None;

        match transition {
            ModeTransition::Set(mut mode) => {
                for mut removed in self.modes.drain(..).rev() {
                    removed.on_exit(app, ExitReason::Reset);
                }
                mode.on_enter(app);
                self.modes.push(mode);
            }
            ModeTransition::Push(mut mode) => {
                mode.on_enter(app);
                self.modes.push(mode);
            }
            ModeTransition::Replace(mut mode) => {
                if let Some(mut removed) = self.modes.pop() {
                    removed.on_exit(app, ExitReason::Replaced);
                }
                mode.on_enter(app);
                self.modes.push(mode);
            }
            ModeTransition::Pop if self.modes.len() > 1 => {
                let mut removed = self.modes.pop().expect("checked non-root mode");
                removed.on_exit(app, ExitReason::Popped);
            }
            ModeTransition::Pop => {}
        }
    }

    /// Renders the highest full-screen mode and overlays above it. Covered
    /// full-screen modes retain state without mutating active layout caches.
    pub(crate) fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64) {
        let start = self
            .modes
            .iter()
            .rposition(|mode| mode.presentation(app).coverage == Coverage::FullScreen)
            .unwrap_or(0);
        for mode in &mut self.modes[start..] {
            mode.render(app, buf, now_ms);
        }
    }

    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.modes.len()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.depth()
    }

    #[cfg(test)]
    pub(crate) fn overlay_active(&self, app: &App) -> bool {
        self.modes
            .last()
            .is_some_and(|mode| mode.presentation(app).coverage == Coverage::Overlay)
    }

    #[cfg(test)]
    pub(crate) fn top_presentation(&self, app: &App) -> ModePresentation {
        self.modes
            .last()
            .expect("mode stack always has a root")
            .presentation(app)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use extui::event::{KeyEvent, MouseEvent};

    use super::*;
    use crate::{bindings, config::Config, tui::modes::ServerListMode};

    struct OverlayMode;

    impl AppMode for OverlayMode {
        fn render(&mut self, _app: &mut App, _buf: &mut Buffer, _now_ms: u64) {}

        fn process_input(&mut self, _app: &mut App, _key: KeyEvent) -> Action {
            Action::Continue
        }

        fn process_mouse(&mut self, _app: &mut App, _mouse: MouseEvent) -> Action {
            Action::Continue
        }

        fn presentation(&self, _app: &App) -> ModePresentation {
            ModePresentation::OVERLAY
        }
    }

    struct RecordingMode {
        label: &'static str,
        presentation: ModePresentation,
        rendered: Rc<RefCell<Vec<&'static str>>>,
    }

    impl RecordingMode {
        fn full_screen(label: &'static str, rendered: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                label,
                rendered,
                presentation: ModePresentation::full_screen(ChromeSpec {
                    theme_mode: theme::UiMode::Log,
                    status_label: label,
                    layer: bindings::WORKSPACE_LAYER,
                }),
            }
        }

        fn overlay(label: &'static str, rendered: Rc<RefCell<Vec<&'static str>>>) -> Self {
            Self {
                label,
                rendered,
                presentation: ModePresentation::OVERLAY,
            }
        }
    }

    impl AppMode for RecordingMode {
        fn render(&mut self, _app: &mut App, _buf: &mut Buffer, _now_ms: u64) {
            self.rendered.borrow_mut().push(self.label);
        }

        fn process_input(&mut self, _app: &mut App, _key: KeyEvent) -> Action {
            Action::Continue
        }

        fn presentation(&self, _app: &App) -> ModePresentation {
            self.presentation
        }
    }

    fn app() -> App {
        App::new(Config::default(), None).expect("test app")
    }

    #[test]
    fn popping_root_is_an_explicit_noop() {
        let mut app = app();
        let mut stack = ModeStack::new(Box::new(ServerListMode::new()), &mut app);

        app.pop_mode();
        stack.apply_pending(&mut app);

        assert_eq!(stack.depth(), 1);
        assert!(app.view.pending_transition.is_empty());
    }

    #[test]
    fn transition_cancels_pending_chord() {
        let mut app = app();
        let mut stack = ModeStack::new(Box::new(ServerListMode::new()), &mut app);
        let input = extui_bindings::InputKey::from_event(&KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::empty(),
        ))
        .expect("input key");
        let _ = crate::bindings::resolve(
            &app.config.bindings.router,
            crate::bindings::WORKSPACE_LAYER,
            &mut app.view.chrome.binding.pending_chord,
            input,
        );
        assert!(app.view.chrome.binding.pending_chord.is_some());

        app.push_mode(Box::new(OverlayMode));
        stack.apply_pending(&mut app);

        assert!(app.view.chrome.binding.pending_chord.is_none());
        assert_eq!(stack.depth(), 2);
    }

    #[test]
    fn active_mode_can_consume_app_event_before_global_app_handler() {
        struct EventConsumer {
            consumed: Rc<Cell<bool>>,
        }

        impl AppMode for EventConsumer {
            fn render(&mut self, _app: &mut App, _buf: &mut Buffer, _now_ms: u64) {}

            fn process_input(&mut self, _app: &mut App, _key: KeyEvent) -> Action {
                Action::Continue
            }

            fn process_app_event(&mut self, _app: &mut App, event: AppEvent) -> Option<AppEvent> {
                if matches!(event, AppEvent::ReportBug(_)) {
                    self.consumed.set(true);
                    None
                } else {
                    Some(event)
                }
            }

            fn presentation(&self, _app: &App) -> ModePresentation {
                ModePresentation::OVERLAY
            }
        }

        let mut app = app();
        let status_before = app.view.status.text().to_string();
        let consumed = Rc::new(Cell::new(false));
        let mut stack = ModeStack::new(
            Box::new(EventConsumer {
                consumed: Rc::clone(&consumed),
            }),
            &mut app,
        );

        stack.process_app_event(&mut app, AppEvent::ReportBug("details".to_string()));

        assert!(consumed.get());
        assert_eq!(app.view.status.text(), status_before);
    }

    #[test]
    #[should_panic(expected = "multiple mode transitions")]
    fn requesting_two_transitions_in_one_dispatch_panics() {
        let mut pending = PendingTransition::default();
        pending.request(ModeTransition::Pop);
        pending.request(ModeTransition::Pop);
    }

    #[test]
    fn render_skips_full_screen_modes_covered_by_a_later_full_screen_mode() {
        let mut app = app();
        let rendered = Rc::new(RefCell::new(Vec::new()));
        let mut stack = ModeStack::new(
            Box::new(RecordingMode::full_screen("list", Rc::clone(&rendered))),
            &mut app,
        );
        app.push_mode(Box::new(RecordingMode::full_screen(
            "room",
            Rc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);
        app.push_mode(Box::new(RecordingMode::full_screen(
            "settings",
            Rc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);

        stack.render(&mut app, &mut Buffer::new(80, 24), 0);

        assert_eq!(&*rendered.borrow(), &["settings"]);
    }

    #[test]
    fn render_draws_highest_full_screen_then_overlays_bottom_to_top() {
        let mut app = app();
        let rendered = Rc::new(RefCell::new(Vec::new()));
        let mut stack = ModeStack::new(
            Box::new(RecordingMode::full_screen("list", Rc::clone(&rendered))),
            &mut app,
        );
        app.push_mode(Box::new(RecordingMode::full_screen(
            "room",
            Rc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);
        app.push_mode(Box::new(RecordingMode::overlay(
            "confirm",
            Rc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);
        app.push_mode(Box::new(RecordingMode::overlay(
            "volume",
            Rc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);

        stack.render(&mut app, &mut Buffer::new(80, 24), 0);

        assert_eq!(&*rendered.borrow(), &["room", "confirm", "volume"]);
    }
}
