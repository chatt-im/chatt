use extui::{
    Buffer,
    event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent},
};
use extui_bindings::LayerId;

use crate::{app::App, theme, tui::Action};

/// Whether `key` is the global quit chord (Ctrl-C). Overlay modes check this so
/// quit is never trapped by a modal.
pub(crate) fn is_quit_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// A screen or overlay on the mode stack.
///
/// A mode owns its transient interaction state and bundles input handling with
/// rendering. The active (top) mode receives input. The whole stack renders
/// bottom-to-top so an overlay paints over the base mode beneath it.
pub(crate) trait AppMode {
    /// Renders this mode.
    ///
    /// Base modes draw the full screen and chrome. Overlay modes draw only their
    /// panel on top of the mode beneath them.
    fn render(&mut self, app: &mut App, buf: &mut Buffer, now_ms: u64);

    /// Handles a key event routed to the active mode.
    fn process_input(&mut self, app: &mut App, key: KeyEvent) -> Action;

    /// Handles a mouse event routed to the active mode.
    fn process_mouse(&mut self, app: &mut App, mouse: MouseEvent) -> Action {
        let _ = (app, mouse);
        Action::Continue
    }

    /// Handles pasted text routed to the active mode.
    fn process_paste(&mut self, app: &mut App, text: String) {
        let _ = (app, text);
    }

    /// Runs once after this mode becomes the active mode on the stack.
    fn init(&mut self, app: &mut App) {
        let _ = app;
    }

    /// Whether this mode overlays the mode beneath it rather than replacing it.
    #[cfg_attr(not(test), allow(dead_code))]
    fn is_overlay(&self) -> bool {
        false
    }

    /// Styling token for this mode's chrome.
    fn theme_mode(&self, app: &App) -> theme::UiMode;

    /// Short label displayed in this mode's status chrome.
    fn status_label(&self, app: &App) -> &'static str;

    /// Binding layer displayed in this mode's key-preview chrome.
    fn layer_id(&self, app: &App) -> LayerId;
}

/// A deferred edit to the mode stack.
///
/// A mode requests a transition while it is borrowed as a stack element. The
/// event loop applies it afterward via [`apply_mode_transition`], which avoids
/// mutating the stack while a mode borrowed from it runs.
#[derive(Default)]
pub(crate) enum ModeTransition {
    #[default]
    None,
    Set(Box<dyn AppMode>),
    Push(Box<dyn AppMode>),
    Replace(Box<dyn AppMode>),
    Pop,
}

/// Applies a pending [`ModeTransition`] to the stack and initializes the new top.
pub(crate) fn apply_mode_transition(app: &mut App, stack: &mut Vec<Box<dyn AppMode>>) {
    match std::mem::take(&mut app.mode_transition) {
        ModeTransition::None => return,
        ModeTransition::Set(mode) => {
            stack.clear();
            stack.push(mode);
        }
        ModeTransition::Push(mode) => stack.push(mode),
        ModeTransition::Replace(mode) => {
            stack.pop();
            stack.push(mode);
        }
        ModeTransition::Pop => {
            stack.pop();
            if stack.is_empty() {
                stack.push(app.base_mode());
            }
        }
    }
    if let Some(top) = stack.last_mut() {
        top.init(app);
    }
}
