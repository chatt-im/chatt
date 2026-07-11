use extui::Rect;

use crate::{bindings::PendingChord, tui::render::KeyPreviewCache};
use rpc::ids::FileTransferId;

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
    pub(crate) live: Rect,
    pub(crate) mute: Rect,
    pub(crate) deafen: Rect,
    pub(crate) video: Rect,
}

impl Default for TopBarLayout {
    fn default() -> Self {
        Self {
            live: Rect::EMPTY,
            mute: Rect::EMPTY,
            deafen: Rect::EMPTY,
            video: Rect::EMPTY,
        }
    }
}

pub(crate) struct LobbyBarLayout {
    /// The audio health widget on the right of the lobby bar.
    pub(crate) audio_widget: Rect,
    /// The `[reset]` button; `Rect::EMPTY` while audio is healthy (hidden).
    pub(crate) audio_reset: Rect,
    /// The `JOIN` / `LEAVE` voice-call button at the far right.
    pub(crate) call_button: Rect,
}

impl Default for LobbyBarLayout {
    fn default() -> Self {
        Self {
            audio_widget: Rect::EMPTY,
            audio_reset: Rect::EMPTY,
            call_button: Rect::EMPTY,
        }
    }
}

#[derive(Default)]
pub(crate) struct ChromeState {
    pub(crate) binding: BindingState,
    pub(crate) key_preview: KeyPreviewState,
    pub(crate) top_bar: TopBarLayout,
    pub(crate) lobby_bar: LobbyBarLayout,
    /// Cancel/skip button hit-boxes for the transfers rendered this frame, keyed
    /// by server transfer id. Rebuilt every frame by `draw_transfer_progress`.
    pub(crate) transfer_buttons: Vec<(Rect, FileTransferId)>,
}
