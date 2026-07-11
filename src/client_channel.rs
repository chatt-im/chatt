use std::{
    io,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use extui::event::polling::Waker;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClientId(pub(crate) u32);

impl ClientId {
    pub(crate) const PRIMARY: Self = Self(0);
}

/// Events whose state belongs to an open render-thread mode (the pairing
/// password prompt); everything else reaches a terminal through its view.
#[derive(Debug)]
pub(crate) enum ClientEvent {
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
    events: Mutex<Vec<ClientEvent>>,
}

impl ClientChannel {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            waker: Waker::new()?,
            terminate: AtomicBool::new(false),
            handoff: AtomicBool::new(false),
            resize_generation: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
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

    pub(crate) fn push(&self, event: ClientEvent) {
        self.events
            .lock()
            .expect("client event mutex poisoned")
            .push(event);
        self.wake();
    }

    pub(crate) fn drain_events(&self) -> Vec<ClientEvent> {
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
