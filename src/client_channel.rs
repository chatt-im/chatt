use std::{
    collections::VecDeque,
    io,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
    },
};

use extui::event::polling::Waker;

use crate::{
    app::{RoomSettingsDraft, ServerEditDraft, UserVolumeDialog},
    clipboard_paste::ImagePaste,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClientId(pub(crate) u32);

impl ClientId {
    pub(crate) const PRIMARY: Self = Self(0);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BaseScreen {
    Room,
    Servers { query: Option<String> },
}

pub(crate) enum ScreenSpec {
    Settings,
    ServerEditor(ServerEditDraft),
    RoomSwitcher,
    UserList,
    RoomSettings(RoomSettingsDraft),
}

pub(crate) enum OverlaySpec {
    UserVolume(UserVolumeDialog),
    NativeEncryptionWarning { label: String, generation: u64 },
    PairingPassword { retry: bool },
    PasteUpload(ImagePaste),
}

pub(crate) enum NavigationEvent {
    ResetBase(BaseScreen),
    OpenScreen(ScreenSpec),
    ReplaceScreen(ScreenSpec),
    CloseScreen,
    ShowOverlay(OverlaySpec),
    ReplaceOverlay(OverlaySpec),
    CloseOverlay,
}

/// Ordered, data-only effects published by the core to one terminal.
pub(crate) enum TerminalEvent {
    Navigation(NavigationEvent),
    PairingPasswordChallenge { retry: bool },
    PairingFailed(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientActions {
    pub(crate) terminate: bool,
    pub(crate) handoff: bool,
    pub(crate) resized: bool,
}

/// Room-screen sections invalidated by a state change, ORed across changes.
///
/// The render thread redraws (and declares as damage to `extui`) only the
/// sections present in the accumulated mask; everything it cannot attribute
/// to a section escalates to [`DirtySections::ALL`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DirtySections(u16);

impl DirtySections {
    pub(crate) const EMPTY: Self = Self(0);
    pub(crate) const TOP_BAR: Self = Self(1 << 0);
    pub(crate) const ROOM_LIST: Self = Self(1 << 1);
    pub(crate) const USER_LIST: Self = Self(1 << 2);
    pub(crate) const LOBBY_BAR: Self = Self(1 << 3);
    pub(crate) const CHAT: Self = Self(1 << 4);
    pub(crate) const CHAT_LOG_BAR: Self = Self(1 << 5);
    pub(crate) const COMPOSER: Self = Self(1 << 6);
    pub(crate) const COMPOSE_BAR: Self = Self(1 << 7);
    pub(crate) const KEY_PREVIEW: Self = Self(1 << 8);
    pub(crate) const ALL: Self = Self(u16::MAX);

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) fn contains(self, sections: Self) -> bool {
        self.0 & sections.0 == sections.0
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl std::ops::BitOr for DirtySections {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for DirtySections {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Core-to-render-thread signalling for one terminal.
pub(crate) struct ClientChannel {
    pub(crate) waker: Waker,
    terminate: AtomicBool,
    handoff: AtomicBool,
    resize_generation: AtomicU64,
    dirty: AtomicU16,
    events: Mutex<VecDeque<TerminalEvent>>,
}

impl ClientChannel {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            waker: Waker::new()?,
            terminate: AtomicBool::new(false),
            handoff: AtomicBool::new(false),
            resize_generation: AtomicU64::new(0),
            dirty: AtomicU16::new(0),
            events: Mutex::new(VecDeque::new()),
        })
    }

    pub(crate) fn wake(&self) {
        let _ = self.waker.wake();
    }

    /// Accumulates `sections` into the dirty mask and wakes the render
    /// thread. A no-op when `sections` is empty.
    pub(crate) fn wake_sections(&self, sections: DirtySections) {
        if sections.is_empty() {
            return;
        }
        self.dirty.fetch_or(sections.0, Ordering::Release);
        self.wake();
    }

    /// Returns and clears the accumulated dirty mask.
    pub(crate) fn take_dirty(&self) -> DirtySections {
        DirtySections(self.dirty.swap(0, Ordering::Acquire))
    }

    pub(crate) fn terminate(&self) {
        self.terminate.store(true, Ordering::Release);
        self.wake();
    }

    pub(crate) fn handoff(&self) {
        self.handoff.store(true, Ordering::Release);
        self.wake();
    }

    pub(crate) fn resize(&self) {
        self.resize_generation.fetch_add(1, Ordering::Release);
        self.dirty.fetch_or(DirtySections::ALL.0, Ordering::Release);
        self.wake();
    }

    pub(crate) fn push(&self, event: TerminalEvent) {
        self.events
            .lock()
            .expect("client event mutex poisoned")
            .push_back(event);
        self.dirty.fetch_or(DirtySections::ALL.0, Ordering::Release);
        self.wake();
    }

    pub(crate) fn drain_events(&self) -> VecDeque<TerminalEvent> {
        std::mem::take(&mut *self.events.lock().expect("client event mutex poisoned"))
    }

    pub(crate) fn actions(&self, previous_resize: &mut u64) -> ClientActions {
        let resize = self.resize_generation.load(Ordering::Acquire);
        let resized = resize != *previous_resize;
        *previous_resize = resize;
        ClientActions {
            terminate: self.terminate.load(Ordering::Acquire),
            handoff: self.handoff.load(Ordering::Acquire),
            resized,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_report_resize_edges_and_sticky_termination() {
        let channel = ClientChannel::new().expect("channel");
        let mut resize = 0;
        assert_eq!(channel.actions(&mut resize), ClientActions::default());

        channel.resize();
        assert_eq!(
            channel.actions(&mut resize),
            ClientActions {
                terminate: false,
                handoff: false,
                resized: true,
            }
        );
        assert_eq!(channel.actions(&mut resize), ClientActions::default());

        channel.terminate();
        assert!(channel.actions(&mut resize).terminate);
        assert!(channel.actions(&mut resize).terminate);
    }

    #[test]
    fn handoff_is_sticky_and_distinct_from_termination() {
        let channel = ClientChannel::new().expect("channel");
        let mut resize = 0;
        channel.handoff();
        let actions = channel.actions(&mut resize);
        assert!(actions.handoff);
        assert!(!actions.terminate);
    }

    #[test]
    fn dirty_sections_accumulate_until_taken() {
        let channel = ClientChannel::new().expect("channel");
        assert_eq!(channel.take_dirty(), DirtySections::EMPTY);

        channel.wake_sections(DirtySections::TOP_BAR);
        channel.wake_sections(DirtySections::USER_LIST | DirtySections::CHAT);
        let taken = channel.take_dirty();
        assert!(taken.contains(DirtySections::TOP_BAR | DirtySections::USER_LIST));
        assert!(taken.contains(DirtySections::CHAT));
        assert!(!taken.contains(DirtySections::COMPOSER));

        assert_eq!(channel.take_dirty(), DirtySections::EMPTY);
    }

    #[test]
    fn resize_and_events_imply_all_sections_dirty() {
        let channel = ClientChannel::new().expect("channel");
        channel.resize();
        assert_eq!(channel.take_dirty(), DirtySections::ALL);

        channel.push(TerminalEvent::PairingFailed(String::new()));
        assert_eq!(channel.take_dirty(), DirtySections::ALL);
        channel.drain_events();
    }
}
