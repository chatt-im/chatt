use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use mio::Waker;

use super::MlsReply;

/// Completed storage events waiting to be applied by the mio event loop.
/// Producers wake the loop directly; the loop never waits on a channel.
pub(super) struct MlsEventQueue {
    queue: Mutex<VecDeque<MlsReply>>,
    waker: Arc<Waker>,
}

impl MlsEventQueue {
    pub(super) fn new(waker: Arc<Waker>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            waker,
        }
    }

    pub(super) fn push(&self, event: MlsReply) {
        self.queue.lock().unwrap().push_back(event);
        if let Err(error) = self.waker.wake() {
            kvlog::warn!("MLS delivery event wake failed", error = %error);
        }
    }

    pub(super) fn drain(&self) -> VecDeque<MlsReply> {
        std::mem::take(&mut *self.queue.lock().unwrap())
    }
}
