use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use super::{
    MlsReply,
    event_queue::{EventNotifier, MLS_EVENTS},
};

/// Completed storage events waiting to be applied by the mio event loop.
/// Producers wake the loop directly; the loop never waits on a channel.
pub(super) struct MlsEventQueue {
    queue: Mutex<VecDeque<MlsReply>>,
    notifier: Arc<EventNotifier>,
}

impl MlsEventQueue {
    pub(super) fn new(notifier: Arc<EventNotifier>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            notifier,
        }
    }

    pub(super) fn push(&self, event: MlsReply) {
        self.queue.lock().unwrap().push_back(event);
        self.notifier.signal(MLS_EVENTS, "mls");
    }

    #[cfg(test)]
    pub(super) fn drain(&self) -> VecDeque<MlsReply> {
        std::mem::take(&mut *self.queue.lock().unwrap())
    }

    pub(super) fn drain_up_to(&self, limit: usize) -> VecDeque<MlsReply> {
        let mut queue = self.queue.lock().unwrap();
        let count = limit.min(queue.len());
        let drained = queue.drain(..count).collect();
        let remains = !queue.is_empty();
        drop(queue);
        if remains {
            self.notifier.signal(MLS_EVENTS, "mls");
        }
        drained
    }
}
