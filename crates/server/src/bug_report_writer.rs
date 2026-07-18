//! Bounded background writer for completed client bug reports.
//!
//! Upload chunks are accumulated by the control loop, but regular-file writes
//! must not run there. Completed bundles move to this worker and return through
//! a reply channel plus the server's shared [`mio::Waker`].

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::Path,
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

use rpc::ids::{BugReportId, SessionId};

use crate::event_queue::EventQueue;

const WRITE_QUEUE_CAPACITY: usize = 8;

pub(super) struct BugReportWriteRequest {
    pub(super) session_id: SessionId,
    pub(super) report_id: BugReportId,
    pub(super) dir: String,
    pub(super) username: String,
    pub(super) description: String,
    pub(super) metadata: String,
    pub(super) logs: Vec<u8>,
}

pub(super) struct BugReportWriteReply {
    pub(super) session_id: SessionId,
    pub(super) report_id: BugReportId,
    pub(super) description: String,
    pub(super) result: Result<String, String>,
}

#[derive(Debug)]
pub(super) enum EnqueueError {
    Full,
    Gone,
}

pub(super) struct BugReportWriter {
    requests: Option<mpsc::SyncSender<BugReportWriteRequest>>,
    thread: Option<JoinHandle<()>>,
}

impl BugReportWriter {
    pub(super) fn spawn(events: Arc<EventQueue<BugReportWriteReply>>) -> Self {
        let (requests, request_rx) =
            mpsc::sync_channel::<BugReportWriteRequest>(WRITE_QUEUE_CAPACITY);
        let thread = thread::Builder::new()
            .name("chatt-bug-report-writer".to_string())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let BugReportWriteRequest {
                        session_id,
                        report_id,
                        dir,
                        username,
                        description,
                        metadata,
                        logs,
                    } = request;
                    let result =
                        write_bug_report(Path::new(&dir), &username, report_id, &metadata, &logs);
                    events.push(BugReportWriteReply {
                        session_id,
                        report_id,
                        description,
                        result,
                    });
                }
            })
            .expect("failed to spawn bug report writer");
        Self {
            requests: Some(requests),
            thread: Some(thread),
        }
    }

    pub(super) fn enqueue(&self, request: BugReportWriteRequest) -> Result<(), EnqueueError> {
        let Some(requests) = &self.requests else {
            return Err(EnqueueError::Gone);
        };
        requests.try_send(request).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => EnqueueError::Full,
            mpsc::TrySendError::Disconnected(_) => EnqueueError::Gone,
        })
    }
}

impl Drop for BugReportWriter {
    fn drop(&mut self) {
        drop(self.requests.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Writes a bug report as two files sharing a unique prefix: the compressed
/// logs (`.log.zst`, already compressed by the client) and metadata (`.json`).
/// Returns the logs path.
fn write_bug_report(
    dir: &Path,
    username: &str,
    report_id: BugReportId,
    metadata: &str,
    logs: &[u8],
) -> Result<String, String> {
    fs::create_dir_all(dir).map_err(|error| format!("failed to create bug-report dir: {error}"))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let user = super::sanitize_file_name(if username.trim().is_empty() {
        "anon"
    } else {
        username
    });
    let base = format!("{timestamp}-{user}-{}", report_id.0);

    for index in 0u64.. {
        let prefix = if index == 0 {
            base.clone()
        } else {
            format!("{base}-{index}")
        };
        let logs_path = dir.join(format!("{prefix}.log.zst"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&logs_path)
        {
            Ok(mut file) => {
                file.write_all(logs)
                    .map_err(|error| format!("failed to write bug report logs: {error}"))?;
                let metadata_path = dir.join(format!("{prefix}.json"));
                fs::write(&metadata_path, metadata.as_bytes())
                    .map_err(|error| format!("failed to write bug report metadata: {error}"))?;
                return Ok(logs_path.display().to_string());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("failed to create bug report file: {error}")),
        }
    }
    unreachable!("u64 bug-report suffix space exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio::{Poll, Token, Waker};
    use std::time::{Duration, Instant};

    #[test]
    fn worker_writes_paired_files_and_replies() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-bug-writer-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(7)).unwrap());
        let notifier = Arc::new(crate::event_queue::EventNotifier::new(waker));
        let events = Arc::new(EventQueue::new(
            notifier,
            crate::event_queue::BUG_REPORT_EVENTS,
            "bug-report-test",
        ));
        let writer = BugReportWriter::spawn(Arc::clone(&events));
        writer
            .enqueue(BugReportWriteRequest {
                session_id: SessionId(3),
                report_id: BugReportId(7),
                dir: dir.display().to_string(),
                username: "Alice Tester".to_string(),
                description: "stuck mute".to_string(),
                metadata: "{\"version\":\"0.1.0\"}".to_string(),
                logs: vec![1, 2, 3, 4],
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let reply = loop {
            if let Some(reply) = events.drain().pop_front() {
                break reply;
            }
            assert!(Instant::now() < deadline, "bug report writer reply");
            thread::yield_now();
        };
        assert_eq!(reply.session_id, SessionId(3));
        assert_eq!(reply.report_id, BugReportId(7));
        assert_eq!(reply.description, "stuck mute");
        let logs_path = reply.result.unwrap();
        assert_eq!(fs::read(&logs_path).unwrap(), vec![1, 2, 3, 4]);
        let metadata_path = logs_path.replace(".log.zst", ".json");
        assert_eq!(
            fs::read_to_string(metadata_path).unwrap(),
            "{\"version\":\"0.1.0\"}"
        );

        drop(writer);
        let _ = fs::remove_dir_all(&dir);
    }
}
