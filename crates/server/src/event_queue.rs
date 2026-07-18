use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use mio::Waker;

/// Worker-to-event-loop delivery queue. Producers hold the mutex only while
/// appending; the event loop swaps the whole queue out before processing and is
/// woken when the shared ready word transitions from empty to non-empty.
pub(super) const VOICE_EVENTS: u64 = 1 << 0;
pub(super) const ROOM_LOG_EVENTS: u64 = 1 << 1;
pub(super) const ROOM_STATE_EVENTS: u64 = 1 << 2;
pub(super) const HISTORY_EVENTS: u64 = 1 << 3;
pub(super) const MLS_EVENTS: u64 = 1 << 4;
pub(super) const IDENTITY_EVENTS: u64 = 1 << 5;
pub(super) const BUG_REPORT_EVENTS: u64 = 1 << 6;
pub(super) const ADMIN_EVENTS: u64 = 1 << 7;

pub(super) struct EventNotifier {
    ready: AtomicU64,
    waker: Arc<Waker>,
}

impl EventNotifier {
    pub(super) fn new(waker: Arc<Waker>) -> Self {
        Self {
            ready: AtomicU64::new(0),
            waker,
        }
    }

    pub(super) fn signal(&self, flag: u64, producer: &'static str) {
        if self.ready.fetch_or(flag, Ordering::AcqRel) == 0
            && let Err(error) = self.waker.wake()
        {
            kvlog::warn!("worker event wake failed", producer, error = %error);
        }
    }

    pub(super) fn take_ready(&self) -> u64 {
        self.ready.swap(0, Ordering::AcqRel)
    }
}

pub(super) struct EventQueue<T> {
    pending: Mutex<VecDeque<T>>,
    notifier: Arc<EventNotifier>,
    flag: u64,
    producer: &'static str,
}

impl<T> EventQueue<T> {
    pub(super) fn new(notifier: Arc<EventNotifier>, flag: u64, producer: &'static str) -> Self {
        Self {
            pending: Mutex::new(VecDeque::new()),
            notifier,
            flag,
            producer,
        }
    }

    pub(super) fn push(&self, event: T) {
        self.pending.lock().unwrap().push_back(event);
        self.notifier.signal(self.flag, self.producer);
    }

    #[cfg(test)]
    pub(super) fn drain(&self) -> VecDeque<T> {
        std::mem::take(&mut *self.pending.lock().unwrap())
    }

    pub(super) fn drain_up_to(&self, limit: usize) -> VecDeque<T> {
        let mut pending = self.pending.lock().unwrap();
        let count = limit.min(pending.len());
        let drained = pending.drain(..count).collect();
        let remains = !pending.is_empty();
        drop(pending);
        if remains {
            self.notifier.signal(self.flag, self.producer);
        }
        drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::{Poll, Token};

    #[test]
    fn bounded_drain_resignals_when_items_remain() {
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(1)).unwrap());
        let notifier = Arc::new(EventNotifier::new(waker));
        let queue = EventQueue::new(Arc::clone(&notifier), HISTORY_EVENTS, "test");
        queue.push(1);
        queue.push(2);
        queue.push(3);
        assert_eq!(notifier.take_ready(), HISTORY_EVENTS);

        assert_eq!(queue.drain_up_to(2), VecDeque::from([1, 2]));
        assert_eq!(notifier.take_ready(), HISTORY_EVENTS);
        assert_eq!(queue.drain_up_to(2), VecDeque::from([3]));
        assert_eq!(notifier.take_ready(), 0);
    }
}
