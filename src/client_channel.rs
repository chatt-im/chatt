use std::{
    io,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use extui::event::polling::Waker;

const TERMINATE: u64 = 1 << 63;
const RESIZE_MASK: u64 = !TERMINATE;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ClientId(pub(crate) u32);

impl ClientId {
    pub(crate) const PRIMARY: Self = Self(0);
}

/// Events whose state belongs to a render-thread mode rather than the shared
/// session. The pairing variants replace the last App-owned mode callback.
#[derive(Debug)]
pub(crate) enum ClientEvent {
    PairingPasswordChallenge { retry: bool },
    PairingSucceeded,
    PairingFailed(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ClientActions {
    pub(crate) terminate: bool,
    pub(crate) resized: bool,
}

/// Core-to-render-thread signalling for one terminal.
pub(crate) struct ClientChannel {
    pub(crate) waker: Waker,
    state: AtomicU64,
    events: Mutex<Vec<ClientEvent>>,
}

impl ClientChannel {
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            waker: Waker::new()?,
            state: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn wake(&self) {
        let _ = self.waker.wake();
    }

    pub(crate) fn terminate(&self) {
        self.state.fetch_or(TERMINATE, Ordering::Release);
        self.wake();
    }

    pub(crate) fn resize(&self) {
        self.state.fetch_add(1, Ordering::Release);
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
        let state = self.state.load(Ordering::Acquire);
        let resize = state & RESIZE_MASK;
        let resized = resize != *previous_resize;
        *previous_resize = resize;
        ClientActions {
            terminate: state & TERMINATE != 0,
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
                resized: true,
            }
        );
        assert_eq!(channel.actions(&mut resize), ClientActions::default());

        channel.terminate();
        assert!(channel.actions(&mut resize).terminate);
        assert!(channel.actions(&mut resize).terminate);
    }
}
