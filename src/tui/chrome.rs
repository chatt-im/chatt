use extui::Rect;

use crate::{bindings::PendingChord, tui::render::KeyPreviewCache};

#[derive(Default)]
pub(crate) struct BindingState {
    pub(crate) pending_chord: Option<PendingChord>,
}

#[derive(Default)]
pub(crate) struct KeyPreviewState {
    pub(crate) expanded: bool,
    pub(crate) cache: KeyPreviewCache,
}

pub(crate) struct TopBarLayout {
    pub(crate) mute: Rect,
    pub(crate) deafen: Rect,
}

impl Default for TopBarLayout {
    fn default() -> Self {
        Self {
            mute: Rect::EMPTY,
            deafen: Rect::EMPTY,
        }
    }
}

#[derive(Default)]
pub(crate) struct ChromeState {
    pub(crate) binding: BindingState,
    pub(crate) key_preview: KeyPreviewState,
    pub(crate) top_bar: TopBarLayout,
}
