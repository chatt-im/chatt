use extui::{
    Buffer,
    event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent},
};
use extui_bindings::LayerId;
use std::sync::mpsc::Sender;

use crate::{
    app::{RoomSession, command::CoreCommand},
    client_channel::{ClientEvent, ClientId},
    config::Config,
    theme,
    tui::{Action, view::ClientView},
};

#[cfg(test)]
use crate::app::App;

pub(crate) enum CommandSink<'a> {
    Local(&'a mut Vec<CoreCommand>),
    Channel {
        client_id: ClientId,
        sender: &'a Sender<(ClientId, CoreCommand)>,
    },
}

/// The state and command sink available to render-thread mode code.
///
/// The session is deliberately read-only. Once clients render on their own
/// threads this reference will be backed by a scoped `RwLock` read guard.
#[allow(dead_code)]
pub(crate) struct ViewCx<'a> {
    pub(crate) view: &'a mut ClientView,
    pub(crate) session: &'a RoomSession,
    pub(crate) config: &'a Config,
    pub(crate) commands: CommandSink<'a>,
}

#[allow(dead_code)]
impl ViewCx<'_> {
    pub(crate) fn send(&mut self, command: CoreCommand) {
        match &mut self.commands {
            CommandSink::Local(commands) => commands.push(command),
            CommandSink::Channel { client_id, sender } => {
                let _ = sender.send((*client_id, command));
            }
        }
    }

    pub(crate) fn set_status(&mut self, status: impl Into<String>) {
        self.view.set_status(status);
    }

    pub(crate) fn set_error(&mut self, status: impl Into<String>) {
        self.view.set_error(status);
    }

    pub(crate) fn set_transient_status(&mut self, status: impl Into<String>) {
        self.view.set_transient_status(status);
    }

    pub(crate) fn request_transition(&mut self, transition: ModeTransition) {
        self.view.pending_transition.request(transition);
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
pub(crate) trait AppMode: Send {
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, now_ms: u64);

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action;

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        let _ = (cx, mouse);
        Action::Continue
    }

    fn process_paste(&mut self, cx: &mut ViewCx<'_>, text: String) {
        let _ = (cx, text);
    }

    fn process_client_event(&mut self, event: ClientEvent) {
        let _ = event;
    }

    fn on_enter(&mut self, _cx: &mut ViewCx<'_>) {}

    fn on_exit(&mut self, _cx: &mut ViewCx<'_>, _reason: ExitReason) {}

    fn presentation(&self, cx: &ViewCx<'_>) -> ModePresentation;
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
    #[cfg(test)]
    pub(crate) fn new(mut root: Box<dyn AppMode>, app: &mut App) -> Self {
        {
            let mut cx = app.view_cx();
            root.on_enter(&mut cx);
        }
        app.drain_core_commands();
        Self { modes: vec![root] }
    }

    pub(crate) fn new_with_cx(mut root: Box<dyn AppMode>, cx: &mut ViewCx<'_>) -> Self {
        root.on_enter(cx);
        Self { modes: vec![root] }
    }

    pub(crate) fn active_mut(&mut self) -> &mut dyn AppMode {
        self.modes
            .last_mut()
            .map(Box::as_mut)
            .expect("mode stack always has a root")
    }

    pub(crate) fn process_client_event(&mut self, event: ClientEvent) {
        self.active_mut().process_client_event(event);
    }

    #[cfg(test)]
    pub(crate) fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action {
        let action = {
            let mut cx = app.view_cx();
            self.process_input_cx(&mut cx, key)
        };
        app.drain_core_commands();
        action
    }

    pub(crate) fn process_input_cx(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action {
        self.active_mut().process_input(cx, key)
    }

    pub(crate) fn process_mouse_cx(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        self.active_mut().process_mouse(cx, mouse)
    }

    pub(crate) fn process_paste_cx(&mut self, cx: &mut ViewCx<'_>, text: String) {
        self.active_mut().process_paste(cx, text);
    }

    /// Applies at most one requested transition. A root pop is an explicit
    /// no-op; navigation must use `Set` to replace the root.
    #[cfg(test)]
    pub(crate) fn apply_pending(&mut self, app: &mut App) {
        {
            let mut cx = app.view_cx();
            self.apply_pending_cx(&mut cx);
        }
        app.drain_core_commands();
    }

    pub(crate) fn apply_pending_cx(&mut self, cx: &mut ViewCx<'_>) {
        let Some(transition) = cx.view.pending_transition.take() else {
            return;
        };

        // Chords never cross a navigation boundary, including overlays.
        cx.view.chrome.binding.pending_chord = None;

        match transition {
            ModeTransition::Set(mut mode) => {
                for mut removed in self.modes.drain(..).rev() {
                    removed.on_exit(cx, ExitReason::Reset);
                }
                mode.on_enter(cx);
                self.modes.push(mode);
            }
            ModeTransition::Push(mut mode) => {
                mode.on_enter(cx);
                self.modes.push(mode);
            }
            ModeTransition::Replace(mut mode) => {
                if let Some(mut removed) = self.modes.pop() {
                    removed.on_exit(cx, ExitReason::Replaced);
                }
                mode.on_enter(cx);
                self.modes.push(mode);
            }
            ModeTransition::Pop if self.modes.len() > 1 => {
                let mut removed = self.modes.pop().expect("checked non-root mode");
                removed.on_exit(cx, ExitReason::Popped);
            }
            ModeTransition::Pop => {}
        }
    }

    /// Renders the highest full-screen mode and overlays above it. Covered
    /// full-screen modes retain state without mutating active layout caches.
    #[cfg(test)]
    pub(crate) fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64) {
        {
            let mut cx = app.view_cx();
            self.render_cx(&mut cx, buf, now_ms);
        }
        app.drain_core_commands();
    }

    pub(crate) fn render_cx(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, now_ms: u64) {
        let start = self
            .modes
            .iter()
            .rposition(|mode| mode.presentation(cx).coverage == Coverage::FullScreen)
            .unwrap_or(0);
        for mode in &mut self.modes[start..] {
            mode.render(cx, buf, now_ms);
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
    pub(crate) fn overlay_active(&self, app: &mut App) -> bool {
        let cx = app.view_cx();
        self.modes
            .last()
            .is_some_and(|mode| mode.presentation(&cx).coverage == Coverage::Overlay)
    }

    #[cfg(test)]
    pub(crate) fn top_presentation(&self, app: &mut App) -> ModePresentation {
        let cx = app.view_cx();
        self.modes
            .last()
            .expect("mode stack always has a root")
            .presentation(&cx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use extui::event::{KeyEvent, MouseEvent};

    use super::*;
    use crate::{bindings, config::Config, tui::modes::ServerListMode};

    struct OverlayMode;

    impl AppMode for OverlayMode {
        fn render(&mut self, _cx: &mut ViewCx<'_>, _buf: &mut Buffer, _now_ms: u64) {}

        fn process_input(&mut self, _cx: &mut ViewCx<'_>, _key: KeyEvent) -> Action {
            Action::Continue
        }

        fn process_mouse(&mut self, _cx: &mut ViewCx<'_>, _mouse: MouseEvent) -> Action {
            Action::Continue
        }

        fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
            ModePresentation::OVERLAY
        }
    }

    struct RecordingMode {
        label: &'static str,
        presentation: ModePresentation,
        rendered: Arc<Mutex<Vec<&'static str>>>,
    }

    impl RecordingMode {
        fn full_screen(label: &'static str, rendered: Arc<Mutex<Vec<&'static str>>>) -> Self {
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

        fn overlay(label: &'static str, rendered: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                label,
                rendered,
                presentation: ModePresentation::OVERLAY,
            }
        }
    }

    impl AppMode for RecordingMode {
        fn render(&mut self, _cx: &mut ViewCx<'_>, _buf: &mut Buffer, _now_ms: u64) {
            self.rendered
                .lock()
                .expect("render log mutex")
                .push(self.label);
        }

        fn process_input(&mut self, _cx: &mut ViewCx<'_>, _key: KeyEvent) -> Action {
            Action::Continue
        }

        fn presentation(&self, _cx: &ViewCx<'_>) -> ModePresentation {
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
    #[should_panic(expected = "multiple mode transitions")]
    fn requesting_two_transitions_in_one_dispatch_panics() {
        let mut pending = PendingTransition::default();
        pending.request(ModeTransition::Pop);
        pending.request(ModeTransition::Pop);
    }

    #[test]
    fn render_skips_full_screen_modes_covered_by_a_later_full_screen_mode() {
        let mut app = app();
        let rendered = Arc::new(Mutex::new(Vec::new()));
        let mut stack = ModeStack::new(
            Box::new(RecordingMode::full_screen("list", Arc::clone(&rendered))),
            &mut app,
        );
        app.push_mode(Box::new(RecordingMode::full_screen(
            "room",
            Arc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);
        app.push_mode(Box::new(RecordingMode::full_screen(
            "settings",
            Arc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);

        stack.render(&mut app, &mut Buffer::new(80, 24), 0);

        assert_eq!(&*rendered.lock().expect("render log mutex"), &["settings"]);
    }

    #[test]
    fn render_draws_highest_full_screen_then_overlays_bottom_to_top() {
        let mut app = app();
        let rendered = Arc::new(Mutex::new(Vec::new()));
        let mut stack = ModeStack::new(
            Box::new(RecordingMode::full_screen("list", Arc::clone(&rendered))),
            &mut app,
        );
        app.push_mode(Box::new(RecordingMode::full_screen(
            "room",
            Arc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);
        app.push_mode(Box::new(RecordingMode::overlay(
            "confirm",
            Arc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);
        app.push_mode(Box::new(RecordingMode::overlay(
            "volume",
            Arc::clone(&rendered),
        )));
        stack.apply_pending(&mut app);

        stack.render(&mut app, &mut Buffer::new(80, 24), 0);

        assert_eq!(
            &*rendered.lock().expect("render log mutex"),
            &["room", "confirm", "volume"]
        );
    }
}
