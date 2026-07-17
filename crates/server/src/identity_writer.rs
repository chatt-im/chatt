//! Ordered background persistence for authentication-time identity changes.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
};

use mio::Token;
use rpc::ids::UserId;

use crate::{config::atomic_write_toml, event_queue::EventQueue, username_registry};

pub(super) enum IdentityWrite {
    UsersToml {
        path: PathBuf,
        snapshot: String,
    },
    DynamicUsername {
        path: PathBuf,
        user_id: UserId,
        username: String,
    },
}

pub(super) struct IdentityWriteRequest {
    pub(super) token: Token,
    pub(super) write: IdentityWrite,
}

pub(super) struct IdentityWriteReply {
    pub(super) token: Token,
    pub(super) result: Result<(), String>,
}

#[derive(Debug)]
pub(super) enum EnqueueError {
    Full,
    Gone,
}

pub(super) struct IdentityWriter {
    requests: Option<mpsc::SyncSender<IdentityWriteRequest>>,
    thread: Option<JoinHandle<()>>,
}

impl IdentityWriter {
    pub(super) fn spawn(events: Arc<EventQueue<IdentityWriteReply>>) -> Self {
        // The control loop admits only one identity transaction at a time. A
        // capacity-one channel makes accidental future over-admission fail
        // without ever blocking that loop.
        let (requests, request_rx) = mpsc::sync_channel::<IdentityWriteRequest>(1);
        let thread = thread::Builder::new()
            .name("chatt-identity-writer".to_string())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let result = execute(request.write);
                    events.push(IdentityWriteReply {
                        token: request.token,
                        result,
                    });
                }
            })
            .expect("failed to spawn identity writer");
        Self {
            requests: Some(requests),
            thread: Some(thread),
        }
    }

    pub(super) fn enqueue(&self, request: IdentityWriteRequest) -> Result<(), EnqueueError> {
        let Some(requests) = &self.requests else {
            return Err(EnqueueError::Gone);
        };
        requests.try_send(request).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => EnqueueError::Full,
            mpsc::TrySendError::Disconnected(_) => EnqueueError::Gone,
        })
    }
}

fn execute(write: IdentityWrite) -> Result<(), String> {
    match write {
        IdentityWrite::UsersToml { path, snapshot } => {
            create_parent(&path)?;
            atomic_write_toml(&path, &snapshot)
        }
        IdentityWrite::DynamicUsername {
            path,
            user_id,
            username,
        } => username_registry::persist_dynamic_claim(&path, user_id, &username),
    }
}

fn create_parent(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))
}

impl Drop for IdentityWriter {
    fn drop(&mut self) {
        drop(self.requests.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
