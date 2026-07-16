use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}, mpsc},
    thread,
};

use mio::Waker;

use super::{MlsReadRequest, MlsReply, mls_store};

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

/// Independent redb read owners. Requests are distributed round-robin so a
/// slow room scan cannot serialize unrelated delivery reads.
pub(super) struct MlsReadPool {
    requests: Vec<mpsc::Sender<MlsReadRequest>>,
    threads: Vec<thread::JoinHandle<()>>,
    next: AtomicUsize,
}

impl MlsReadPool {
    pub(super) fn spawn(
        handles: Vec<mls_store::MlsStore>,
        events: Arc<MlsEventQueue>,
    ) -> Self {
        assert!(handles.len() >= 2, "MLS delivery reads require fan-out");
        let mut requests = Vec::with_capacity(handles.len());
        let mut threads = Vec::with_capacity(handles.len());
        for (index, store) in handles.into_iter().enumerate() {
            let (tx, rx) = mpsc::channel();
            let events = Arc::clone(&events);
            let thread = thread::Builder::new()
                .name(format!("chatt-mls-read-{index}"))
                .spawn(move || {
                    while let Ok(request) = rx.recv() {
                        match request {
                            MlsReadRequest::Events {
                                token,
                                room_id,
                                after_sequence,
                                limit,
                            } => events.push(MlsReply::EventBatch {
                                token,
                                room_id,
                                result: store.events(room_id, after_sequence, limit),
                            }),
                            MlsReadRequest::Welcomes {
                                token,
                                device_id,
                                after_sequence,
                            } => events.push(MlsReply::Welcomes {
                                token,
                                result: (|| {
                                    Ok((
                                        store.welcomes(device_id, after_sequence)?,
                                        store.welcome_head(device_id)?,
                                    ))
                                })(),
                            }),
                        }
                    }
                })
                .expect("failed to spawn MLS delivery reader");
            requests.push(tx);
            threads.push(thread);
        }
        Self {
            requests,
            threads,
            next: AtomicUsize::new(0),
        }
    }

    pub(super) fn enqueue(&self, request: MlsReadRequest) -> bool {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % self.requests.len();
        self.requests[index].send(request).is_ok()
    }
}

impl Drop for MlsReadPool {
    fn drop(&mut self) {
        self.requests.clear();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
