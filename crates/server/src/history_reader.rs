//! Background disk reader serving history pages below the resident window.
//!
//! The mio event loop must never block on disk, so [`HistoryReader`] runs one
//! worker thread: the loop enqueues a [`HistoryReadRequest`] built from the
//! room's segment index ([`crate::room_store::RoomStore::disk_fetch_request`]),
//! the worker reads the sources and encodes the page into wire chunks, and the
//! reply channel plus a [`mio::Waker`] hand the finished payloads back to the
//! loop. The single thread processes requests in order, so replies for one
//! session never reorder.

use std::{
    fs::{self, File},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
};

use rpc::{
    history,
    ids::{MessageId, RoomId, SessionId},
};

use crate::room_store::{LogRecords, ResidentHistory};

/// One disk source holding candidate records for a page.
pub(crate) enum Source {
    /// The room's live append log. The cloned handle pins the inode across
    /// concurrent rotations, and `valid_bytes` was snapshotted after the last
    /// completed record write, so positioned reads below it never see a torn
    /// record.
    ActiveTail { file: File, valid_bytes: u64 },
    /// A rotated segment, immutable once created and safe to read by path.
    Segment { path: PathBuf },
}

/// One disk history page for the reader thread to serve, built on the event
/// loop by [`crate::room_store::RoomStore::disk_fetch_request`].
pub(crate) struct HistoryReadRequest {
    pub(crate) session_id: SessionId,
    pub(crate) room_id: RoomId,
    /// Exclusive newest bound of the page; `None` pages from the newest
    /// record.
    pub(crate) before: Option<MessageId>,
    pub(crate) limit: usize,
    pub(crate) target_bytes: usize,
    /// Id of the oldest record the room retains anywhere on disk; the reply
    /// is `at_start` exactly when the page begins with this record.
    pub(crate) oldest_durable_id: MessageId,
    /// Sources ordered newest to oldest; scanning stops once `limit` records
    /// are gathered.
    pub(crate) sources: Vec<Source>,
}

/// A served page ready for the event loop to seal and queue.
pub(crate) struct HistoryReadReply {
    pub(crate) session_id: SessionId,
    pub(crate) room_id: RoomId,
    /// Encoded wire chunks in queue order, always ending with a `complete`
    /// chunk so the client's fetch state cannot wedge.
    pub(crate) payloads: Vec<Vec<u8>>,
}

/// Handle to the disk reader thread; dropping it stops the thread.
pub(crate) struct HistoryReader {
    requests: mpsc::Sender<HistoryReadRequest>,
}

impl HistoryReader {
    /// Starts the reader thread; it wakes `waker` after queueing each reply.
    pub(crate) fn spawn(waker: Arc<mio::Waker>) -> (Self, mpsc::Receiver<HistoryReadReply>) {
        let (requests, request_rx) = mpsc::channel::<HistoryReadRequest>();
        let (reply_tx, replies) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                let reply = execute(&request);
                if reply_tx.send(reply).is_err() {
                    return;
                }
                if let Err(error) = waker.wake() {
                    kvlog::warn!(
                        "history reader wake failed",
                        error = error.to_string().as_str()
                    );
                }
            }
        });
        (Self { requests }, replies)
    }

    /// Queues a request, returning false when the worker thread is gone; the
    /// caller must then still answer the client with a terminal chunk.
    pub(crate) fn enqueue(&self, request: HistoryReadRequest) -> bool {
        self.requests.send(request).is_ok()
    }
}

fn execute(request: &HistoryReadRequest) -> HistoryReadReply {
    let selected = select_records(request);
    let at_start = selected.first_message_id() == Some(request.oldest_durable_id);
    let payloads = match selected.chunk_payloads(request.room_id, at_start, request.target_bytes) {
        Ok(payloads) => payloads,
        Err(error) => {
            kvlog::error!(
                "history disk page encode failed",
                room_id = request.room_id.0,
                error = error.as_str()
            );
            vec![empty_chunk(request.room_id)]
        }
    };
    HistoryReadReply {
        session_id: request.session_id,
        room_id: request.room_id,
        payloads,
    }
}

/// Gathers the newest `limit` records with ids below the request bound,
/// oldest first. A source that fails to read or parse cleanly ends the scan
/// without contributing: keeping its partial run under newer sources would
/// leave a hole in the page that the client's paging cursor then skips
/// forever, so the page is truncated at the newest contiguous run instead.
fn select_records(request: &HistoryReadRequest) -> ResidentHistory {
    let before = request.before.unwrap_or(MessageId(u64::MAX));
    let mut newest_first = Vec::new();
    let mut total = 0usize;
    for source in &request.sources {
        if total >= request.limit {
            break;
        }
        let bytes = match read_source(source) {
            Ok(bytes) => bytes,
            Err(error) => {
                kvlog::warn!(
                    "history page source read failed",
                    room_id = request.room_id.0,
                    error = error.to_string().as_str()
                );
                break;
            }
        };
        let Some(mut run) = scan_records_before(&bytes, before) else {
            kvlog::warn!(
                "history page source corrupt; page truncated",
                room_id = request.room_id.0
            );
            break;
        };
        total += run.len();
        // Only the newest `limit` records of a run can survive the final
        // trim, so shed the rest before it is copied again below.
        run.trim_to_limit(request.limit);
        newest_first.push(run);
    }
    let mut selected = ResidentHistory::default();
    for run in newest_first.into_iter().rev() {
        selected.extend(run);
    }
    selected.trim_to_limit(request.limit);
    selected
}

fn read_source(source: &Source) -> std::io::Result<Vec<u8>> {
    match source {
        Source::Segment { path } => fs::read(path),
        Source::ActiveTail { file, valid_bytes } => {
            let mut bytes = vec![0u8; *valid_bytes as usize];
            file.read_exact_at(&mut bytes, 0)?;
            Ok(bytes)
        }
    }
}

/// One source's records below `before`, or `None` when the source has a
/// corrupt frame or record. Records within a source ascend, so those below
/// `before` form a prefix and the scan stops at the first id at or past it.
fn scan_records_before(bytes: &[u8], before: MessageId) -> Option<ResidentHistory> {
    let mut run = ResidentHistory::default();
    let mut records = LogRecords::new(bytes);
    while let Some(record) = records.next_record() {
        let Ok(message) = history::parse_message(record) else {
            return None;
        };
        if message.message_id >= before {
            return Some(run);
        }
        run.append_known_record(message.message_id, record);
    }
    if records.exhausted() { Some(run) } else { None }
}

/// A terminal empty chunk: `complete` so the client's fetch state cannot
/// wedge, and not `at_start` because the start was not reached, the fetch
/// merely failed.
pub(crate) fn empty_chunk(room_id: RoomId) -> Vec<u8> {
    let mut payload = Vec::with_capacity(history::CHUNK_HEADER_BYTES);
    history::write_chunk_header(room_id, false, true, 0, &mut payload)
        .expect("empty history chunk header should encode");
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::control::ChatMessage;
    use rpc::ids::UserId;
    use std::path::Path;

    const ROOM: RoomId = RoomId(1);
    const TARGET_BYTES: usize = 64 * 1024;

    fn test_message(id: u64) -> ChatMessage {
        ChatMessage {
            message_id: MessageId(id),
            room_id: ROOM,
            sender: UserId(1),
            sender_name: "alice".to_string(),
            timestamp_ms: 1_000 + id,
            body: format!("message {id}"),
            file_transfer_id: None,
        }
    }

    fn encode_log(ids: &[u64]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for id in ids {
            let record = history::encode_message(&test_message(*id));
            bytes.extend_from_slice(&(record.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&record);
        }
        bytes
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chatt-history-reader-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn segment_source(dir: &Path, name: &str, ids: &[u64]) -> Source {
        let path = dir.join(name);
        fs::write(&path, encode_log(ids)).unwrap();
        Source::Segment { path }
    }

    fn active_source(dir: &Path, ids: &[u64]) -> Source {
        let path = dir.join("active.log");
        let bytes = encode_log(ids);
        fs::write(&path, &bytes).unwrap();
        Source::ActiveTail {
            file: File::open(&path).unwrap(),
            valid_bytes: bytes.len() as u64,
        }
    }

    fn request(
        before: Option<u64>,
        limit: usize,
        oldest_durable_id: u64,
        sources: Vec<Source>,
    ) -> HistoryReadRequest {
        HistoryReadRequest {
            session_id: SessionId(1),
            room_id: ROOM,
            before: before.map(MessageId),
            limit,
            target_bytes: TARGET_BYTES,
            oldest_durable_id: MessageId(oldest_durable_id),
            sources,
        }
    }

    fn page_of(reply: &HistoryReadReply) -> (Vec<u64>, bool) {
        let mut ids = Vec::new();
        let mut at_start = false;
        let mut completed = false;
        for payload in &reply.payloads {
            assert!(!completed, "chunks after the complete chunk");
            let chunk = history::decode_chunk(payload)
                .expect("decode history chunk")
                .expect("history chunk payload");
            assert_eq!(chunk.room_id, ROOM);
            let chunk_ids: Vec<u64> = chunk
                .messages
                .iter()
                .map(|message| message.message_id.0)
                .collect();
            ids.splice(0..0, chunk_ids);
            completed = chunk.complete;
            at_start = chunk.at_start;
        }
        assert!(completed, "reply must end with a complete chunk");
        (ids, at_start)
    }

    #[test]
    fn pages_across_segments_and_active_tail() {
        let dir = temp_dir("across-sources");
        let sources = vec![
            active_source(&dir, &[11, 12, 13, 14, 15]),
            segment_source(&dir, "log.6", &[6, 7, 8, 9, 10]),
            segment_source(&dir, "log.1", &[1, 2, 3, 4, 5]),
        ];

        let reply = execute(&request(Some(13), 4, 1, sources));

        let (ids, at_start) = page_of(&reply);
        assert_eq!(ids, vec![9, 10, 11, 12]);
        assert!(!at_start);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_reaching_oldest_record_sets_at_start() {
        let dir = temp_dir("at-start");
        let sources = vec![segment_source(&dir, "log.1", &[1, 2, 3, 4, 5])];

        let reply = execute(&request(Some(3), 10, 1, sources));

        let (ids, at_start) = page_of(&reply);
        assert_eq!(ids, vec![1, 2]);
        assert!(at_start);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn before_none_pages_from_the_newest_record() {
        let dir = temp_dir("before-none");
        let sources = vec![active_source(&dir, &[11, 12, 13])];

        let reply = execute(&request(None, 2, 11, sources));

        let (ids, at_start) = page_of(&reply);
        assert_eq!(ids, vec![12, 13]);
        assert!(!at_start);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn id_gaps_from_watermark_blocks_page_exactly() {
        let dir = temp_dir("id-gaps");
        let sources = vec![
            active_source(&dir, &[2048, 2049]),
            segment_source(&dir, "log.1", &[1, 2, 1024, 1025]),
        ];

        let reply = execute(&request(Some(2049), 3, 1, sources));

        let (ids, at_start) = page_of(&reply);
        assert_eq!(ids, vec![1024, 1025, 2048]);
        assert!(!at_start);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_source_truncates_page_before_the_gap() {
        let dir = temp_dir("corrupt-source");
        let corrupt_path = dir.join("log.6");
        let mut bytes = encode_log(&[6, 7, 8, 9, 10]);
        bytes.truncate(bytes.len() - 3);
        fs::write(&corrupt_path, &bytes).unwrap();
        let sources = vec![
            active_source(&dir, &[11, 12, 13]),
            Source::Segment { path: corrupt_path },
            segment_source(&dir, "log.1", &[1, 2, 3, 4, 5]),
        ];

        let reply = execute(&request(Some(12), 10, 1, sources));

        let (ids, at_start) = page_of(&reply);
        assert_eq!(
            ids,
            vec![11],
            "records past the corrupt source would leave a hole the paging cursor skips"
        );
        assert!(!at_start);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_tail_ignores_bytes_beyond_the_snapshot() {
        let dir = temp_dir("tail-snapshot");
        let path = dir.join("active.log");
        let snapshot = encode_log(&[11, 12]);
        let mut bytes = snapshot.clone();
        bytes.extend_from_slice(&encode_log(&[13, 14]));
        fs::write(&path, &bytes).unwrap();
        let sources = vec![Source::ActiveTail {
            file: File::open(&path).unwrap(),
            valid_bytes: snapshot.len() as u64,
        }];

        let reply = execute(&request(None, 10, 11, sources));

        let (ids, at_start) = page_of(&reply);
        assert_eq!(ids, vec![11, 12]);
        assert!(at_start);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_sources_reply_is_terminal_but_not_at_start() {
        let reply = execute(&request(Some(5), 4, 1, Vec::new()));

        let (ids, at_start) = page_of(&reply);
        assert!(ids.is_empty());
        assert!(!at_start);
    }
}
