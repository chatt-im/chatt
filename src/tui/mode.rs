use std::collections::VecDeque;

use extui::{
    Buffer,
    event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent},
};
use extui_bindings::LayerId;

use crate::{
    app::{RoomSession, command::CoreCommand},
    client_channel::{
        BaseScreen, DirtySections, NavigationEvent, OverlaySpec, ScreenSpec, TerminalEvent,
    },
    config::Config,
    theme,
    tui::{Action, view::ClientView},
};

#[cfg(test)]
use crate::app::App;

/// The state and command sink available to render-thread mode code.
///
/// The session is deliberately read-only. Commands land in a local queue,
/// never directly in a channel: a channel send could block while this context
/// holds the session read guard the core needs before it can drain commands.
/// The context's owner transmits the queue once the guards are dropped.
#[allow(dead_code)]
pub(crate) struct ViewCx<'a> {
    pub(crate) view: &'a mut ClientView,
    pub(crate) session: &'a RoomSession,
    pub(crate) config: &'a Config,
    pub(crate) commands: &'a mut Vec<CoreCommand>,
    pub(crate) navigation: &'a mut VecDeque<ModeTransition>,
    /// Sections the current input dispatch may have touched. Starts at
    /// [`DirtySections::ALL`] so unaudited paths stay safe; only dispatch
    /// paths audited to touch a known section set narrow it.
    pub(crate) dirty_hint: DirtySections,
    /// This render frame diffs against a retained, seeded previous frame, so
    /// scroll-region optimizations may queue terminal scrolls. `false` on
    /// input-dispatch contexts, which never render.
    pub(crate) frame_retained: bool,
}

#[allow(dead_code)]
impl ViewCx<'_> {
    pub(crate) fn send(&mut self, command: CoreCommand) {
        self.commands.push(command);
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

    pub(crate) fn queue_clipboard(&mut self, text: String) {
        self.view.queue_clipboard(text);
        self.view.set_transient_status("copied to clipboard");
    }

    pub(crate) fn request_transition(&mut self, transition: ModeTransition) {
        self.navigation.push_back(transition);
    }

    /// Replaces the input dirty hint with an audited section set. Call only
    /// from dispatch paths whose entire effect on render state is known.
    pub(crate) fn narrow_dirty(&mut self, sections: DirtySections) {
        self.dirty_hint = sections;
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
    /// Draws the mode. `dirty` names the sections whose inputs changed since
    /// the last frame; only modes reporting [`Self::section_rendering`] gate
    /// their drawing on it — every other mode repaints fully and ignores it.
    fn render(&mut self, cx: &mut ViewCx<'_>, buf: &mut Buffer, now_ms: u64, dirty: DirtySections);

    /// Whether this mode redraws only dirty sections and declares the
    /// rectangles it drew as damage on the buffer. The render thread keeps
    /// the buffer in [`extui::Swap::Retained`] while such a mode is on top;
    /// for everything else it reverts to classic blank-and-repaint frames.
    fn section_rendering(&self) -> bool {
        false
    }

    fn process_input(&mut self, cx: &mut ViewCx<'_>, key: KeyEvent) -> Action;

    fn process_mouse(&mut self, cx: &mut ViewCx<'_>, mouse: MouseEvent) -> Action {
        let _ = (cx, mouse);
        Action::Continue
    }

    fn process_paste(&mut self, cx: &mut ViewCx<'_>, text: String) {
        let _ = (cx, text);
    }

    fn process_client_event(&mut self, event: TerminalEvent) {
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

/// Owns navigation invariants and lifecycle for all screens and overlays.
pub(crate) struct ModeStack {
    modes: Vec<Box<dyn AppMode>>,
    pending: VecDeque<ModeTransition>,
    /// Incremented whenever the stack's composition actually changes, so the
    /// render thread can detect transitions and force a full repaint.
    composition_generation: u64,
}

impl ModeStack {
    #[cfg(test)]
    pub(crate) fn new(mut root: Box<dyn AppMode>, app: &mut App) -> Self {
        {
            let mut cx = app.view_cx();
            root.on_enter(&mut cx);
        }
        app.drain_core_commands();
        Self {
            modes: vec![root],
            pending: VecDeque::new(),
            composition_generation: 0,
        }
    }

    pub(crate) fn new_with_cx(mut root: Box<dyn AppMode>, cx: &mut ViewCx<'_>) -> Self {
        root.on_enter(cx);
        Self {
            modes: vec![root],
            pending: VecDeque::new(),
            composition_generation: 0,
        }
    }

    /// The current composition generation; changes exactly when a transition
    /// alters which modes are on the stack.
    pub(crate) fn composition_generation(&self) -> u64 {
        self.composition_generation
    }

    /// Whether the mode on top of the stack draws with section gating.
    pub(crate) fn section_rendering_active(&self) -> bool {
        self.modes
            .last()
            .is_some_and(|mode| mode.section_rendering())
    }

    pub(crate) fn active_mut(&mut self) -> &mut dyn AppMode {
        self.modes
            .last_mut()
            .map(Box::as_mut)
            .expect("mode stack always has a root")
    }

    pub(crate) fn process_terminal_event(&mut self, cx: &mut ViewCx<'_>, event: TerminalEvent) {
        if let TerminalEvent::Navigation(event) = event {
            let transition = match event {
                NavigationEvent::ResetBase(base) => ModeTransition::Set(base.into_mode()),
                NavigationEvent::OpenScreen(screen) => ModeTransition::Push(screen.into_mode()),
                NavigationEvent::ReplaceScreen(screen) => {
                    ModeTransition::Replace(screen.into_mode())
                }
                NavigationEvent::CloseScreen => ModeTransition::Pop,
                NavigationEvent::ShowOverlay(overlay) => {
                    ModeTransition::Push(overlay.into_mode(&cx.view.theme))
                }
                NavigationEvent::ReplaceOverlay(overlay) => {
                    ModeTransition::Replace(overlay.into_mode(&cx.view.theme))
                }
                NavigationEvent::CloseOverlay => ModeTransition::Pop,
            };
            self.apply_transition(cx, transition);
            self.apply_pending_cx(cx);
            return;
        }
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

    /// Applies every queued transition in request order. A root pop is an
    /// explicit no-op; navigation must use `Set` to replace the root.
    #[cfg(test)]
    pub(crate) fn apply_pending(&mut self, app: &mut App) {
        {
            let mut cx = app.view_cx();
            self.apply_pending_cx(&mut cx);
        }
        app.drain_core_commands();
    }

    pub(crate) fn apply_pending_cx(&mut self, cx: &mut ViewCx<'_>) {
        loop {
            self.pending.append(cx.navigation);
            let Some(transition) = self.pending.pop_front() else {
                break;
            };
            self.apply_transition(cx, transition);
        }
    }

    fn apply_transition(&mut self, cx: &mut ViewCx<'_>, transition: ModeTransition) {
        // Chords never cross a navigation boundary, including overlays.
        cx.view.chrome.binding.pending_chord = None;

        match transition {
            ModeTransition::Set(mut mode) => {
                for mut removed in self.modes.drain(..).rev() {
                    removed.on_exit(cx, ExitReason::Reset);
                }
                mode.on_enter(cx);
                self.modes.push(mode);
                self.composition_generation += 1;
            }
            ModeTransition::Push(mut mode) => {
                mode.on_enter(cx);
                self.modes.push(mode);
                self.composition_generation += 1;
            }
            ModeTransition::Replace(mut mode) => {
                if let Some(mut removed) = self.modes.pop() {
                    removed.on_exit(cx, ExitReason::Replaced);
                }
                mode.on_enter(cx);
                self.modes.push(mode);
                self.composition_generation += 1;
            }
            ModeTransition::Pop if self.modes.len() > 1 => {
                let mut removed = self.modes.pop().expect("checked non-root mode");
                removed.on_exit(cx, ExitReason::Popped);
                self.composition_generation += 1;
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
            self.render_cx(&mut cx, buf, now_ms, DirtySections::ALL);
        }
        app.drain_core_commands();
    }

    pub(crate) fn render_cx(
        &mut self,
        cx: &mut ViewCx<'_>,
        buf: &mut Buffer,
        now_ms: u64,
        dirty: DirtySections,
    ) {
        let start = self
            .modes
            .iter()
            .rposition(|mode| mode.presentation(cx).coverage == Coverage::FullScreen)
            .unwrap_or(0);
        for mode in &mut self.modes[start..] {
            mode.render(cx, buf, now_ms, dirty);
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

impl BaseScreen {
    fn into_mode(self) -> Box<dyn AppMode> {
        use crate::tui::modes::{RoomMode, ServerListMode};
        match self {
            Self::Room => Box::new(RoomMode::default()),
            Self::Servers { query: Some(query) } => Box::new(ServerListMode::with_query(query)),
            Self::Servers { query: None } => Box::new(ServerListMode::new()),
        }
    }
}

impl ScreenSpec {
    fn into_mode(self) -> Box<dyn AppMode> {
        use crate::tui::{
            modes::{RoomSwitchMode, ServerEditMode, SettingsMode},
            room_settings::RoomSettingsMode,
            user_list::UserListMode,
        };
        match self {
            Self::Settings => Box::new(SettingsMode::new()),
            Self::ServerEditor(draft) => Box::new(ServerEditMode::new(draft)),
            Self::RoomSwitcher => Box::new(RoomSwitchMode::new()),
            Self::UserList => Box::new(UserListMode::new()),
            Self::RoomSettings(draft) => Box::new(RoomSettingsMode::new(draft)),
        }
    }
}

impl OverlaySpec {
    fn into_mode(self, theme: &crate::theme::Theme) -> Box<dyn AppMode> {
        use crate::tui::overlay::{
            DialogMode, E2eIdentityMode, NativeEncryptionWarningMode, PasswordPromptMode,
            PasteImageUploadMode,
        };
        match self {
            Self::UserVolume(dialog) => Box::new(DialogMode::new(dialog)),
            Self::NativeEncryptionWarning { label, generation } => {
                Box::new(NativeEncryptionWarningMode::new(label, generation))
            }
            Self::E2eIdentity(dialog) => Box::new(E2eIdentityMode::new(dialog, theme)),
            Self::PairingPassword { retry } => Box::new(PasswordPromptMode::new(retry)),
            Self::PasteUpload(image) => Box::new(PasteImageUploadMode::new(image, theme)),
        }
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
        fn render(
            &mut self,
            _cx: &mut ViewCx<'_>,
            _buf: &mut Buffer,
            _now_ms: u64,
            _dirty: DirtySections,
        ) {
        }

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
        fn render(
            &mut self,
            _cx: &mut ViewCx<'_>,
            _buf: &mut Buffer,
            _now_ms: u64,
            _dirty: DirtySections,
        ) {
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
        assert!(app.test_navigation.is_empty());
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
    fn queued_transitions_apply_once_in_request_order() {
        let mut app = app();
        let mut stack = ModeStack::new(Box::new(ServerListMode::new()), &mut app);

        app.push_mode(Box::new(OverlayMode));
        app.pop_mode();
        stack.apply_pending(&mut app);

        assert_eq!(stack.depth(), 1);
        assert!(app.test_navigation.is_empty());
    }

    #[test]
    fn batched_base_reset_then_native_warning_keeps_warning_active() {
        let mut app = app();
        let mut stack = ModeStack::new(Box::new(ServerListMode::new()), &mut app);
        {
            let mut cx = app.view_cx();
            stack.process_terminal_event(
                &mut cx,
                TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Servers {
                    query: None,
                })),
            );
            stack.process_terminal_event(
                &mut cx,
                TerminalEvent::Navigation(NavigationEvent::ShowOverlay(
                    OverlaySpec::NativeEncryptionWarning {
                        label: "legacy".to_string(),
                        generation: 17,
                    },
                )),
            );
        }

        assert_eq!(stack.depth(), 2);
        assert!(stack.overlay_active(&mut app));

        {
            let mut cx = app.view_cx();
            stack.process_input_cx(
                &mut cx,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            );
        }
        assert!(matches!(
            app.take_queued_core_command(),
            Some(CoreCommand::CancelNativeEncryption { generation: 17 })
        ));
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
