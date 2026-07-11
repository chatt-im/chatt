use std::{
    collections::VecDeque,
    io,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

/// Core-to-render-thread signalling for one terminal.
pub(crate) struct ClientChannel {
    pub(crate) waker: Waker,
    terminate: AtomicBool,
    handoff: AtomicBool,
    resize_generation: AtomicU64,
    events: Mutex<VecDeque<TerminalEvent>>,
}

impl ClientChannel {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            waker: Waker::new()?,
            terminate: AtomicBool::new(false),
            handoff: AtomicBool::new(false),
            resize_generation: AtomicU64::new(0),
            events: Mutex::new(VecDeque::new()),
        })
    }

    pub(crate) fn wake(&self) {
        let _ = self.waker.wake();
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
        self.wake();
    }

    pub(crate) fn push(&self, event: TerminalEvent) {
        self.events
            .lock()
            .expect("client event mutex poisoned")
            .push_back(event);
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
}
