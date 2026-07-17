//! Server-side room message retention and durable room state.
//!
//! One [`RoomStore`] owns, per room, the retention backend selected by the
//! room's persistence config: nothing, a bounded in-memory ring, or an
//! append-only on-disk log. Every backend also keeps a small window of record
//! ids so mutation recency does not require retaining message contents. A
//! durable room keeps only the newest messages
//! resident; when the active log grows past its size cap it rotates into an
//! immutable segment named `<log>.<first-message-id>`, and segments are never
//! deleted, so older pages stay servable from disk (see
//! [`crate::history_reader`]). The store also owns the durable server state
//! that must survive restarts: per-room message-id watermarks (so ids are
//! never reused) and the registry of runtime-created DM rooms.
//!
//! # Durability scope
//!
//! Room identity, dynamic-room registration, and message-id reservations live
//! in a transactional `redb` database. Message log appends still use ordinary
//! writes with no fsync on the append path, so durable rooms survive a process
//! crash (the page cache persists) but not power loss or a kernel crash. After
//! such an unclean shutdown the loader truncates the torn log tail while the
//! database watermark keeps message ids advancing, so a client that saw the
//! lost messages observes a gap. Fsync-on-a-timer is the upgrade path if that
//! trade stops being acceptable.
//!
//! Message ids are per-room, monotonic from 1, and durable. Allocation uses a
//! block reservation: whenever the remaining reservation drops below half a
//! block, the watermark advances past the next [`MESSAGE_ID_RESERVE`]
//! boundary and the new value is queued to the background state writer, so
//! the write lands while ids from the still-reserved region are handed out.
//! A restart resumes from the watermark, skipping at most
//! `2 * MESSAGE_ID_RESERVE - 1` ids.

use hashbrown::HashMap;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
};

use rpc::{
    control::{ChatMessage, MUTATION_WINDOW_MESSAGES},
    history,
    ids::{MessageId, RoomId, SessionId, UserId},
};
use crate::config::{FIRST_DYNAMIC_ROOM_ID, RoomConfig, RoomPersistenceConfig};
use crate::event_queue::{EventNotifier, EventQueue, ROOM_LOG_EVENTS, ROOM_STATE_EVENTS};
use crate::history_reader::{HistoryReadRequest, Source};
use crate::room_state::{self, LoadedState, StateWriteEvent, StateWriter};

/// Message-id block reserved per state write; a restart skips at most twice
/// this many ids per room.
pub const MESSAGE_ID_RESERVE: u64 = 1024;
/// Active durable log size that triggers rotation into an immutable
/// id-named segment.
const MAX_ACTIVE_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Cap on messages a durable room keeps resident; older pages are served
/// from disk.
const MAX_DURABLE_RESIDENT_MESSAGES: usize = 8192;
const LOG_WRITE_QUEUE_CAPACITY: usize = 256;

/// Durable retention size limits, injectable so tests can force rotation and
/// resident trimming with a handful of small messages.
#[derive(Clone, Copy)]
pub(crate) struct StoreTuning {
    pub(crate) max_active_log_bytes: u64,
    pub(crate) max_resident_messages: usize,
}

impl Default for StoreTuning {
    fn default() -> Self {
        Self {
            max_active_log_bytes: MAX_ACTIVE_LOG_BYTES,
            max_resident_messages: MAX_DURABLE_RESIDENT_MESSAGES,
        }
    }
}

/// Which mutation [`RoomStore::validate_mutation`] is gating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationKind {
    Edit,
    Delete,
}

/// Why [`RoomStore::validate_mutation`] rejected a mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationDenied {
    /// The target is not among the newest window of record ids: unknown or
    /// frozen by newer traffic.
    TargetMissing,
    /// The target was sent by someone else.
    WrongSender,
    /// The target announces a file transfer, which cannot be edited.
    FileMessage,
    /// The target is itself an edit or delete record.
    MutationRecord,
    /// A delete already applies to the target.
    Deleted,
}

impl MutationDenied {
    pub fn as_str(self) -> &'static str {
        match self {
            MutationDenied::TargetMissing => "target_missing",
            MutationDenied::WrongSender => "wrong_sender",
            MutationDenied::FileMessage => "file_message",
            MutationDenied::MutationRecord => "mutation_record",
            MutationDenied::Deleted => "deleted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmRoom {
    pub room_id: RoomId,
    pub user_a: UserId,
    pub user_b: UserId,
    pub created_at_ms: u64,
}

struct PendingDm {
    operation_id: u64,
    room: DmRoom,
    previous_next_room_id: u32,
}

pub(crate) enum OpenDmResult {
    Existing(RoomId),
    Pending { operation_id: u64 },
}

pub(crate) struct DmCompletion {
    pub(crate) operation_id: u64,
    pub(crate) room: DmRoom,
    pub(crate) result: Result<(), String>,
}

impl DmRoom {
    pub fn pairs(&self, first: UserId, second: UserId) -> bool {
        (self.user_a == first && self.user_b == second)
            || (self.user_a == second && self.user_b == first)
    }
}

enum Retention {
    None,
    Memory {
        history: ResidentHistory,
        limit: usize,
    },
    Durable {
        history: ResidentHistory,
        file: ActiveLog,
        active_bytes: u64,
        path: Option<PathBuf>,
        active_first_id: Option<MessageId>,
        segments: Vec<Segment>,
    },
}

/// Write state of a durable room's active log file.
enum ActiveLog {
    Open(File),
    /// Rotation renamed the old log away but the fresh open failed; the path
    /// is free, so a later append retries the open.
    Reopen,
    /// Appending would place records after unrepaired corruption or a torn
    /// write; the room keeps only resident history for the rest of the
    /// process.
    Disabled,
}

impl ActiveLog {
    fn open(&self) -> Option<&File> {
        match self {
            ActiveLog::Open(file) => Some(file),
            ActiveLog::Reopen | ActiveLog::Disabled => None,
        }
    }
}

struct RoomLogWrite {
    room_id: RoomId,
    message_id: MessageId,
    record: Vec<u8>,
    max_active_log_bytes: u64,
    path: Option<PathBuf>,
}

enum LogFileUpdate {
    Keep,
    Open(File),
    Reopen,
    Disabled,
}

struct RoomLogEvent {
    room_id: RoomId,
    message_id: MessageId,
    active_bytes: u64,
    active_first_id: Option<MessageId>,
    rotated: Option<Segment>,
    file: LogFileUpdate,
    error: Option<String>,
}

struct RoomLogWorkerState {
    path: PathBuf,
    file: ActiveLog,
    /// The event-loop copy cannot page the active tail until the worker sends
    /// it a cloned read handle. Kept separately from `file` because the worker
    /// may still be able to append after cloning temporarily fails.
    needs_read_handle: bool,
    active_bytes: u64,
    active_first_id: Option<MessageId>,
}

struct RoomLogWriter {
    requests: Option<mpsc::SyncSender<RoomLogWrite>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RoomLogWriter {
    fn spawn(
        rooms: HashMap<RoomId, RoomLogWorkerState>,
        events: Arc<EventQueue<RoomLogEvent>>,
    ) -> Self {
        let (requests, request_rx) = mpsc::sync_channel(LOG_WRITE_QUEUE_CAPACITY);
        let thread = thread::Builder::new()
            .name("chatt-room-log-writer".to_string())
            .spawn(move || room_log_worker(request_rx, rooms, events))
            .expect("failed to spawn room log writer");
        Self {
            requests: Some(requests),
            thread: Some(thread),
        }
    }

    fn enqueue(&self, write: RoomLogWrite) -> Result<(), &'static str> {
        let Some(requests) = &self.requests else {
            return Err("writer gone");
        };
        requests.try_send(write).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => "queue full",
            mpsc::TrySendError::Disconnected(_) => "writer gone",
        })
    }
}

impl Drop for RoomLogWriter {
    fn drop(&mut self) {
        drop(self.requests.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn room_log_worker(
    requests: mpsc::Receiver<RoomLogWrite>,
    mut rooms: HashMap<RoomId, RoomLogWorkerState>,
    events: Arc<EventQueue<RoomLogEvent>>,
) {
    while let Ok(write) = requests.recv() {
        let mut new_room = false;
        if !rooms.contains_key(&write.room_id)
            && let Some(path) = write.path.clone()
        {
            let file = match open_active_log(&path) {
                Ok(file) => ActiveLog::Open(file),
                Err(error) => {
                    events.push(RoomLogEvent {
                        room_id: write.room_id,
                        message_id: write.message_id,
                        active_bytes: 0,
                        active_first_id: None,
                        rotated: None,
                        file: LogFileUpdate::Reopen,
                        error: Some(format!("active log open failed: {error}")),
                    });
                    continue;
                }
            };
            rooms.insert(
                write.room_id,
                RoomLogWorkerState {
                    path,
                    file,
                    needs_read_handle: true,
                    active_bytes: 0,
                    active_first_id: None,
                },
            );
            new_room = true;
        }
        let Some(state) = rooms.get_mut(&write.room_id) else {
            continue;
        };
        let mut file_update = LogFileUpdate::Keep;
        let mut rotated = None;
        let mut error = None;
        if matches!(state.file, ActiveLog::Reopen) {
            match open_active_log(&state.path) {
                Ok(file) => {
                    state.file = ActiveLog::Open(file);
                    state.needs_read_handle = true;
                }
                Err(open_error) => {
                    error = Some(format!("active log reopen failed: {open_error}"));
                }
            }
        }
        if state.needs_read_handle
            && let ActiveLog::Open(file) = &state.file
        {
            match file.try_clone() {
                Ok(read_file) => {
                    file_update = LogFileUpdate::Open(read_file);
                    state.needs_read_handle = false;
                }
                Err(clone_error) => {
                    error = Some(format!("active log clone failed: {clone_error}"));
                    file_update = LogFileUpdate::Reopen;
                }
            }
        } else if new_room {
            file_update = LogFileUpdate::Reopen;
        }
        if let ActiveLog::Open(file) = &mut state.file {
            match write_log_record(file, &write.record) {
                Ok(written) => {
                    if state.active_bytes == 0 {
                        state.active_first_id = Some(write.message_id);
                    }
                    state.active_bytes += written as u64;
                }
                Err(write_error) if write_error.kind() == std::io::ErrorKind::InvalidInput => {
                    error = Some("history record exceeds the log record cap".to_string());
                }
                Err(write_error) => {
                    error = Some(format!("durable room log append failed: {write_error}"));
                    state.file = ActiveLog::Disabled;
                    file_update = LogFileUpdate::Disabled;
                }
            }
        }
        if state.active_bytes > write.max_active_log_bytes
            && let Some(first_id) = state.active_first_id
            && matches!(state.file, ActiveLog::Open(_))
        {
            let segment_path = segment_path(&state.path, first_id);
            match fs::rename(&state.path, &segment_path) {
                Ok(()) => {
                    rotated = Some(Segment {
                        path: segment_path,
                        first_id,
                    });
                    state.active_bytes = 0;
                    state.active_first_id = None;
                    match open_active_log(&state.path) {
                        Ok(file) => {
                            state.needs_read_handle = true;
                            file_update = match file.try_clone() {
                                Ok(read_file) => {
                                    state.needs_read_handle = false;
                                    LogFileUpdate::Open(read_file)
                                }
                                Err(clone_error) => {
                                    error = Some(format!(
                                        "fresh active log clone failed: {clone_error}"
                                    ));
                                    LogFileUpdate::Reopen
                                }
                            };
                            state.file = ActiveLog::Open(file);
                        }
                        Err(open_error) => {
                            error = Some(format!("active log reopen after rotation failed: {open_error}"));
                            state.file = ActiveLog::Reopen;
                            state.needs_read_handle = true;
                            file_update = LogFileUpdate::Reopen;
                        }
                    }
                }
                Err(rename_error) => {
                    error = Some(format!("durable room log rotate failed: {rename_error}"));
                }
            }
        }
        events.push(RoomLogEvent {
            room_id: write.room_id,
            message_id: write.message_id,
            active_bytes: state.active_bytes,
            active_first_id: state.active_first_id,
            rotated,
            file: file_update,
            error,
        });
    }
}

/// An immutable rotated log file, named `<log>.<first-message-id>` so the
/// directory listing doubles as the id index for disk paging.
pub(crate) struct Segment {
    pub(crate) path: PathBuf,
    pub(crate) first_id: MessageId,
}

impl Retention {
    fn history(&self) -> Option<&ResidentHistory> {
        match self {
            Retention::None => None,
            Retention::Memory { history, .. } | Retention::Durable { history, .. } => Some(history),
        }
    }

    fn oldest_durable_id(&self) -> Option<MessageId> {
        let Retention::Durable {
            active_first_id,
            segments,
            ..
        } = self
        else {
            return None;
        };
        segments
            .first()
            .map(|segment| segment.first_id)
            .or(*active_first_id)
    }

    fn has_unresident_history(&self) -> bool {
        let Some(oldest_durable) = self.oldest_durable_id() else {
            return false;
        };
        match self.history().and_then(ResidentHistory::first_message_id) {
            Some(oldest_resident) => oldest_durable < oldest_resident,
            None => true,
        }
    }
}

pub struct HistoryFetchPlan {
    room_id: RoomId,
    before: Option<MessageId>,
    chunks: Vec<HistoryChunkSpec>,
    pub message_count: usize,
    pub at_start: bool,
}

impl HistoryFetchPlan {
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn before(&self) -> Option<MessageId> {
        self.before
    }
}

/// One planned wire chunk, anchored to the id of its oldest message so an
/// intervening resident trim is detected at encode time instead of silently
/// shifting the range onto newer entries.
struct HistoryChunkSpec {
    /// Id of the chunk's oldest message; `None` for the empty terminal chunk.
    first_id: Option<MessageId>,
    count: usize,
    records_bytes: usize,
    at_start: bool,
    complete: bool,
}

/// Decoded-on-demand message records held in memory: the retention window of
/// a live room, or the selected page of a background disk read.
#[derive(Default)]
pub(crate) struct ResidentHistory {
    bytes: Vec<u8>,
    entries: Vec<HistoryEntry>,
    first_entry: usize,
    first_byte: usize,
}

#[derive(Clone, Copy)]
struct HistoryEntry {
    message_id: MessageId,
    start: usize,
    len: usize,
}

impl ResidentHistory {
    pub(crate) fn len(&self) -> usize {
        self.entries.len().saturating_sub(self.first_entry)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn entries(&self) -> &[HistoryEntry] {
        &self.entries[self.first_entry..]
    }

    fn last(&self) -> Option<HistoryEntry> {
        self.entries().last().copied()
    }

    pub(crate) fn first_message_id(&self) -> Option<MessageId> {
        self.entries().first().map(|entry| entry.message_id)
    }

    fn record(&self, entry: HistoryEntry) -> &[u8] {
        &self.bytes[entry.start..entry.start + entry.len]
    }

    fn record_range(&self, start: usize, len: usize) -> &[u8] {
        &self.bytes[start..start + len]
    }

    fn append_message(&mut self, message: &ChatMessage) -> (usize, usize) {
        let start = self.bytes.len();
        history::write_message(message, &mut self.bytes);
        let len = self.bytes.len() - start;
        self.entries.push(HistoryEntry {
            message_id: message.message_id,
            start,
            len,
        });
        (start, len)
    }

    fn rollback_append(&mut self, start: usize) {
        let entry = self.entries.pop().expect("rollback follows append");
        debug_assert_eq!(entry.start, start);
        self.bytes.truncate(start);
    }

    fn append_record(&mut self, record: &[u8]) -> Result<(), String> {
        let parsed = history::parse_message(record)?;
        self.append_known_record(parsed.message_id, record);
        Ok(())
    }

    pub(crate) fn append_known_record(&mut self, message_id: MessageId, record: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(record);
        self.entries.push(HistoryEntry {
            message_id,
            start,
            len: record.len(),
        });
    }

    pub(crate) fn extend(&mut self, other: ResidentHistory) {
        for entry in other.entries() {
            self.append_known_record(entry.message_id, other.record(*entry));
        }
    }

    fn history_chunks_before(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: usize,
        target_bytes: usize,
        has_older_on_disk: bool,
    ) -> HistoryFetchPlan {
        let entries = self.entries();
        let end = match before {
            Some(before) => entries.partition_point(|entry| entry.message_id < before),
            None => entries.len(),
        };
        let start = end.saturating_sub(limit);
        let selected = &entries[start..end];
        let at_start = start == 0 && !has_older_on_disk;
        HistoryFetchPlan {
            room_id,
            before,
            chunks: self.chunk_specs(selected, at_start, target_bytes),
            message_count: selected.len(),
            at_start,
        }
    }

    fn chunk_specs(
        &self,
        entries: &[HistoryEntry],
        at_start: bool,
        target_bytes: usize,
    ) -> Vec<HistoryChunkSpec> {
        if entries.is_empty() {
            return vec![HistoryChunkSpec {
                first_id: None,
                count: 0,
                records_bytes: 0,
                at_start,
                complete: true,
            }];
        }

        let target_bytes = target_bytes.max(history::CHUNK_HEADER_BYTES);
        let mut ranges = Vec::new();
        let mut start = 0usize;
        let mut bytes = history::CHUNK_HEADER_BYTES;
        let mut records_bytes = 0usize;
        let mut count = 0usize;
        for (index, entry) in entries.iter().enumerate() {
            let would_exceed_bytes = count > 0 && bytes.saturating_add(entry.len) > target_bytes;
            let would_exceed_count = count == history::MAX_CHUNK_MESSAGES;
            if would_exceed_bytes || would_exceed_count {
                ranges.push((start, index, records_bytes));
                start = index;
                bytes = history::CHUNK_HEADER_BYTES;
                records_bytes = 0;
                count = 0;
            }
            bytes = bytes.saturating_add(entry.len);
            records_bytes = records_bytes.saturating_add(entry.len);
            count += 1;
        }
        ranges.push((start, entries.len(), records_bytes));

        let chunk_count = ranges.len();
        let mut chunks = Vec::with_capacity(chunk_count);
        for (send_index, (start, end, records_bytes)) in ranges.into_iter().rev().enumerate() {
            let complete = send_index + 1 == chunk_count;
            chunks.push(HistoryChunkSpec {
                first_id: Some(entries[start].message_id),
                count: end - start,
                records_bytes,
                at_start: complete && at_start,
                complete,
            });
        }
        chunks
    }

    fn encode_chunk(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        spec: &HistoryChunkSpec,
    ) -> Result<Vec<u8>, String> {
        let stale = || "history fetch plan no longer matches retained history".to_string();
        let entries = self.entries();
        let chunk_entries = match spec.first_id {
            None => &[] as &[HistoryEntry],
            Some(first_id) => {
                let start = entries.partition_point(|entry| entry.message_id < first_id);
                let Some(chunk_entries) = start
                    .checked_add(spec.count)
                    .and_then(|end| entries.get(start..end))
                else {
                    return Err(stale());
                };
                if chunk_entries.first().map(|entry| entry.message_id) != Some(first_id) {
                    return Err(stale());
                }
                chunk_entries
            }
        };
        let mut payload = Vec::with_capacity(history::CHUNK_HEADER_BYTES + spec.records_bytes);
        history::write_chunk_header(
            room_id,
            before,
            spec.at_start,
            spec.complete,
            chunk_entries.len(),
            &mut payload,
        )?;
        for entry in chunk_entries {
            payload.extend_from_slice(self.record(*entry));
        }
        Ok(payload)
    }

    /// Encodes the entire resident content as wire history chunks in queue
    /// order: newest range first, the final (oldest) chunk marked `complete`
    /// and carrying `at_start`.
    pub(crate) fn chunk_payloads(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        at_start: bool,
        target_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, String> {
        let specs = self.chunk_specs(self.entries(), at_start, target_bytes);
        let mut payloads = Vec::with_capacity(specs.len());
        for spec in &specs {
            payloads.push(self.encode_chunk(room_id, before, spec)?);
        }
        Ok(payloads)
    }

    pub(crate) fn trim_to_limit(&mut self, limit: usize) {
        while self.len() > limit {
            let entry = self.entries[self.first_entry];
            self.first_byte = entry.start + entry.len;
            self.first_entry += 1;
        }
        self.compact_if_worthwhile();
    }

    fn compact_if_worthwhile(&mut self) {
        if self.first_entry == 0 {
            return;
        }
        if self.first_entry == self.entries.len() {
            self.bytes.clear();
            self.entries.clear();
            self.first_entry = 0;
            self.first_byte = 0;
            return;
        }

        const COMPACT_DEAD_BYTES: usize = 64 * 1024;
        const COMPACT_DEAD_ENTRIES: usize = 1024;
        let dead_bytes = self.first_byte;
        let dead_entries = self.first_entry;
        let worthwhile_bytes =
            dead_bytes >= COMPACT_DEAD_BYTES && dead_bytes * 2 >= self.bytes.len();
        let worthwhile_entries =
            dead_entries >= COMPACT_DEAD_ENTRIES && dead_entries * 2 >= self.entries.len();
        if !worthwhile_bytes && !worthwhile_entries {
            return;
        }

        self.bytes.drain(..dead_bytes);
        for entry in &mut self.entries[self.first_entry..] {
            entry.start -= dead_bytes;
        }
        self.entries.drain(..self.first_entry);
        self.first_entry = 0;
        self.first_byte = 0;
    }
}

pub struct RoomStore {
    data_dir: Option<PathBuf>,
    state_writer: Option<StateWriter>,
    log_writer: Option<RoomLogWriter>,
    log_events: Option<Arc<EventQueue<RoomLogEvent>>>,
    pending_log_writes: HashMap<RoomId, usize>,
    state_events: Option<Arc<EventQueue<StateWriteEvent>>>,
    tuning: StoreTuning,
    next_room_id: u32,
    next_ids: HashMap<RoomId, u64>,
    watermarks: HashMap<RoomId, u64>,
    heads: HashMap<RoomId, MessageId>,
    recent_ids: HashMap<RoomId, VecDeque<MessageId>>,
    dm_rooms: Vec<DmRoom>,
    pending_dm: Option<PendingDm>,
    next_dm_operation: u64,
    rooms: HashMap<RoomId, Retention>,
}

impl RoomStore {
    /// Opens the store for the configured rooms: loads the transactional room
    /// state database, fills
    /// each durable room's resident window from its active log (repairing a
    /// corrupt tail) and newest segments, and registers persisted DM rooms.
    /// `data_dir = None` keeps everything in memory, so ids restart per
    /// process; real servers always have a data dir.
    pub fn open(data_dir: Option<PathBuf>, rooms: &[RoomConfig]) -> Self {
        Self::try_open_with_tuning(data_dir, rooms, StoreTuning::default())
            .unwrap_or_else(|error| panic!("failed to open room store: {error}"))
    }

    pub(crate) fn try_open(
        data_dir: Option<PathBuf>,
        rooms: &[RoomConfig],
    ) -> Result<Self, String> {
        Self::try_open_with_tuning(data_dir, rooms, StoreTuning::default())
    }

    #[cfg(test)]
    pub(crate) fn open_with_tuning(
        data_dir: Option<PathBuf>,
        rooms: &[RoomConfig],
        tuning: StoreTuning,
    ) -> Self {
        Self::try_open_with_tuning(data_dir, rooms, tuning)
            .unwrap_or_else(|error| panic!("failed to open room store: {error}"))
    }

    fn try_open_with_tuning(
        data_dir: Option<PathBuf>,
        rooms: &[RoomConfig],
        tuning: StoreTuning,
    ) -> Result<Self, String> {
        if let Some(dir) = &data_dir {
            fs::create_dir_all(dir.join("rooms")).map_err(|error| {
                format!(
                    "room store data directory {} is unavailable: {error}",
                    dir.display()
                )
            })?;
        }
        let (state, state_writer) = match data_dir.as_deref() {
            Some(dir) => {
                let path = dir.join(room_state::FILE_NAME);
                let opened = room_state::open(&path).map_err(|error| {
                    format!("room state database {} is unavailable: {error}", path.display())
                })?;
                (opened.0, Some(opened.1))
            }
            None => (LoadedState::default(), None),
        };
        let mut next_room_id = state.next_room_id.max(FIRST_DYNAMIC_ROOM_ID);
        for entry in &state.dm_rooms {
            next_room_id = next_room_id.max(entry.room_id.0.saturating_add(1));
        }
        if let Some(dir) = &data_dir {
            next_room_id = next_room_id.max(next_room_id_above_existing_logs(dir));
        }
        let mut store = Self {
            data_dir,
            state_writer,
            log_writer: None,
            log_events: None,
            pending_log_writes: HashMap::new(),
            state_events: None,
            tuning,
            next_room_id,
            next_ids: HashMap::new(),
            watermarks: HashMap::new(),
            heads: HashMap::new(),
            recent_ids: HashMap::new(),
            dm_rooms: Vec::new(),
            pending_dm: None,
            next_dm_operation: 1,
            rooms: HashMap::new(),
        };
        for (room_id, next) in state.watermarks {
            store.watermarks.insert(room_id, next);
            store.next_ids.insert(room_id, next);
        }
        for room in rooms {
            store.register_room(
                room.room_id(),
                room.persistence,
                room.memory_history_limit(),
            );
        }
        for dm in state.dm_rooms {
            let room_id = dm.room_id;
            store.register_room(room_id, RoomPersistenceConfig::Durable, 0);
            store.dm_rooms.push(dm);
        }
        Ok(store)
    }

    /// Test-only override of the durable retention limits; both caps are read
    /// at append time, so it takes effect for subsequent appends.
    #[cfg(test)]
    pub(crate) fn set_tuning(&mut self, tuning: StoreTuning) {
        self.tuning = tuning;
    }

    /// Transfers regular-file appends and rotations to one ordered worker.
    /// Resident history is still published immediately; the worker completion
    /// advances only the disk-history mirror used by paging.
    pub(crate) fn enable_async_log_writes(&mut self, notifier: Arc<EventNotifier>) {
        if self.log_writer.is_some() {
            return;
        }
        let events = Arc::new(EventQueue::new(notifier, ROOM_LOG_EVENTS, "room-log"));
        let mut worker_rooms = HashMap::new();
        let mut failed_rooms = Vec::new();
        for (room_id, retention) in &self.rooms {
            let Retention::Durable {
                file,
                active_bytes,
                path: Some(path),
                active_first_id,
                ..
            } = retention
            else {
                continue;
            };
            let worker_file = match file {
                ActiveLog::Open(file) => match file.try_clone() {
                    Ok(file) => ActiveLog::Open(file),
                    Err(error) => {
                        kvlog::error!(
                            "durable room log handoff failed; retention degraded to memory",
                            room_id = room_id.0,
                            error = error.to_string().as_str()
                        );
                        failed_rooms.push(*room_id);
                        continue;
                    }
                },
                ActiveLog::Reopen => ActiveLog::Reopen,
                ActiveLog::Disabled => continue,
            };
            worker_rooms.insert(
                *room_id,
                RoomLogWorkerState {
                    path: path.clone(),
                    file: worker_file,
                    needs_read_handle: matches!(file, ActiveLog::Reopen),
                    active_bytes: *active_bytes,
                    active_first_id: *active_first_id,
                },
            );
        }
        for room_id in failed_rooms {
            if let Some(Retention::Durable { file, .. }) = self.rooms.get_mut(&room_id) {
                *file = ActiveLog::Disabled;
            }
        }
        self.log_writer = Some(RoomLogWriter::spawn(worker_rooms, Arc::clone(&events)));
        self.log_events = Some(events);
    }

    pub(crate) fn enable_async_state_writes(&mut self, notifier: Arc<EventNotifier>) {
        if self.state_writer.is_some() && self.state_events.is_none() {
            let events = Arc::new(EventQueue::new(
                notifier,
                ROOM_STATE_EVENTS,
                "room-state",
            ));
            self.state_writer
                .as_ref()
                .expect("state writer checked")
                .set_events(Arc::clone(&events));
            self.state_events = Some(events);
        }
    }

    pub(crate) fn drain_log_events(&mut self) {
        let Some(events) = &self.log_events else {
            return;
        };
        let mut completed = events.drain_up_to(64);
        while let Some(event) = completed.pop_front() {
            let Some(Retention::Durable {
                history,
                file,
                active_bytes,
                active_first_id,
                segments,
                ..
            }) = self.rooms.get_mut(&event.room_id)
            else {
                continue;
            };
            if let Some(error) = event.error {
                kvlog::error!(
                    "asynchronous durable room log operation failed",
                    room_id = event.room_id.0,
                    message_id = event.message_id.0,
                    error = error.as_str()
                );
            }
            if let Some(pending) = self.pending_log_writes.get_mut(&event.room_id) {
                *pending = pending.saturating_sub(1);
                if *pending == 0 {
                    self.pending_log_writes.remove(&event.room_id);
                }
            }
            if matches!(file, ActiveLog::Disabled) {
                if !self.pending_log_writes.contains_key(&event.room_id) {
                    history.trim_to_limit(self.tuning.max_resident_messages);
                }
                continue;
            }
            *active_bytes = event.active_bytes;
            *active_first_id = event.active_first_id;
            if let Some(segment) = event.rotated {
                segments.push(segment);
            }
            match event.file {
                LogFileUpdate::Keep => {}
                LogFileUpdate::Open(read_file) => *file = ActiveLog::Open(read_file),
                LogFileUpdate::Reopen => *file = ActiveLog::Reopen,
                LogFileUpdate::Disabled => *file = ActiveLog::Disabled,
            }
            if !self.pending_log_writes.contains_key(&event.room_id) {
                history.trim_to_limit(self.tuning.max_resident_messages);
            }
        }
    }

    fn register_room(
        &mut self,
        room_id: RoomId,
        persistence: RoomPersistenceConfig,
        memory_limit: usize,
    ) {
        let retention = match persistence {
            RoomPersistenceConfig::None => Retention::None,
            RoomPersistenceConfig::Memory => Retention::Memory {
                history: ResidentHistory::default(),
                limit: memory_limit,
            },
            RoomPersistenceConfig::Durable => self.open_durable(room_id),
        };
        let recent_ids = retention
            .history()
            .map(|history| {
                history
                    .entries()
                    .iter()
                    .rev()
                    .take(MUTATION_WINDOW_MESSAGES)
                    .rev()
                    .map(|entry| entry.message_id)
                    .collect()
            })
            .unwrap_or_default();
        if let Retention::Durable { history, .. } = &retention
            && let Some(last) = history.last()
        {
            let next = self.next_ids.entry(room_id).or_insert(1);
            *next = (*next).max(last.message_id.0 + 1);
            self.heads.insert(room_id, last.message_id);
        }
        self.recent_ids.insert(room_id, recent_ids);
        self.rooms.insert(room_id, retention);
    }

    fn open_durable(&self, room_id: RoomId) -> Retention {
        let Some(dir) = &self.data_dir else {
            return Retention::Durable {
                history: ResidentHistory::default(),
                file: ActiveLog::Disabled,
                active_bytes: 0,
                path: None,
                active_first_id: None,
                segments: Vec::new(),
            };
        };
        let path = dir.join("rooms").join(format!("{}.log", room_id.0));
        let segments = list_segments(&path);
        let active = load_log_repairing(&path);
        let active_bytes = active.valid_bytes;
        let active_first_id = active.history.first_message_id();
        let mut resident_total = active.history.len();
        let mut newest_first = Vec::new();
        for segment in segments.iter().rev() {
            if resident_total >= self.tuning.max_resident_messages {
                break;
            }
            let loaded = load_log(&segment.path, LogRepair::ReadOnly).history;
            resident_total += loaded.len();
            newest_first.push(loaded);
        }
        let mut history;
        if newest_first.is_empty() {
            history = active.history;
        } else {
            history = ResidentHistory::default();
            for older in newest_first.into_iter().rev() {
                history.extend(older);
            }
            history.extend(active.history);
        }
        history.trim_to_limit(self.tuning.max_resident_messages);
        // Appending to a log whose corrupt tail could not be truncated would
        // place records after the corruption, unreadable by every future
        // startup while ids keep advancing.
        let file = if active.tail_repair_failed {
            kvlog::error!(
                "durable room log tail unrepaired; retention degraded to memory",
                room_id = room_id.0
            );
            ActiveLog::Disabled
        } else {
            match open_active_log(&path) {
                Ok(open) => ActiveLog::Open(open),
                Err(error) => {
                    kvlog::error!(
                        "durable room log open failed; retention degraded to memory",
                        room_id = room_id.0,
                        error = error.to_string().as_str()
                    );
                    ActiveLog::Disabled
                }
            }
        };
        Retention::Durable {
            history,
            file,
            active_bytes,
            path: Some(path),
            active_first_id,
            segments,
        }
    }

    /// Hands out the next message id for the room, queueing a new watermark
    /// block to the background state writer when the remaining reservation
    /// drops below half a block.
    ///
    /// The half-block headroom lets the write land off the event loop while
    /// ids from the still-reserved region keep flowing; the persisted
    /// watermark only ever advances, and durable rooms additionally recover
    /// their next id from the log itself.
    pub fn allocate_message_id(&mut self, room_id: RoomId) -> MessageId {
        let next = self.next_ids.entry(room_id).or_insert(1);
        let id = *next;
        *next += 1;
        let watermark = self.watermarks.entry(room_id).or_insert(0);
        let persist = if id + MESSAGE_ID_RESERVE / 2 >= *watermark {
            *watermark = (id + 1 + MESSAGE_ID_RESERVE).next_multiple_of(MESSAGE_ID_RESERVE);
            Some(*watermark)
        } else {
            None
        };
        if let Some(watermark) = persist {
            self.queue_persist_watermark(room_id, watermark);
        }
        MessageId(id)
    }

    /// Records the message per the room's retention and returns how many
    /// messages the room now retains. This compatibility wrapper is used by
    /// store-level callers that do not run with the bounded async writer.
    pub fn append(&mut self, room_id: RoomId, message: &ChatMessage) -> usize {
        self.try_append(room_id, message).unwrap_or_else(|_| {
            self.rooms
                .get(&room_id)
                .and_then(Retention::history)
                .map_or(0, ResidentHistory::len)
        })
    }

    /// Records a message without weakening durability when the background
    /// writer is saturated. The caller can reject this one operation and
    /// retry later; subsequent room writes remain durable.
    pub fn try_append(
        &mut self,
        room_id: RoomId,
        message: &ChatMessage,
    ) -> Result<usize, String> {
        let StoreTuning {
            max_active_log_bytes,
            max_resident_messages,
        } = self.tuning;
        let Some(retention) = self.rooms.get_mut(&room_id) else {
            return Ok(0);
        };
        let history_len = match retention {
            Retention::None => 0,
            Retention::Memory { history, limit } => {
                history.append_message(message);
                history.trim_to_limit(*limit);
                history.len()
            }
            Retention::Durable {
                history,
                file,
                active_bytes,
                path,
                active_first_id,
                segments,
            } => {
                let (start, len) = history.append_message(message);
                if let Some(writer) = &self.log_writer
                    && !matches!(file, ActiveLog::Disabled)
                {
                    let write = RoomLogWrite {
                        room_id,
                        message_id: message.message_id,
                        record: history.record_range(start, len).to_vec(),
                        max_active_log_bytes,
                        path: path.clone(),
                    };
                    if let Err(error) = writer.enqueue(write) {
                        history.rollback_append(start);
                        kvlog::warn!(
                            "durable room log append deferred by writer backpressure",
                            room_id = room_id.0,
                            error
                        );
                        return Err(format!("durable history writer is {error}; retry shortly"));
                    }
                    *self.pending_log_writes.entry(room_id).or_default() += 1;
                    let history_len = history.len();
                    self.heads.insert(room_id, message.message_id);
                    let recent_ids = self.recent_ids.entry(room_id).or_default();
                    if recent_ids.len() == MUTATION_WINDOW_MESSAGES {
                        recent_ids.pop_front();
                    }
                    recent_ids.push_back(message.message_id);
                    return Ok(history_len);
                }
                if matches!(file, ActiveLog::Reopen)
                    && let Some(path) = path.as_deref()
                    && let Ok(fresh) = open_active_log(path)
                {
                    kvlog::info!(
                        "durable room log reopened after failed rotation",
                        room_id = room_id.0
                    );
                    *file = ActiveLog::Open(fresh);
                }
                if let ActiveLog::Open(open) = file {
                    let record = history.record_range(start, len);
                    match write_log_record(open, record) {
                        Ok(written) => {
                            if *active_bytes == 0 {
                                *active_first_id = Some(message.message_id);
                            }
                            *active_bytes += written as u64;
                        }
                        // An InvalidInput record was rejected before any byte
                        // reached the file, so the log framing stays intact;
                        // only this record is dropped from the durable copy.
                        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {
                            kvlog::error!(
                                "history record exceeds the log record cap; message not persisted",
                                room_id = room_id.0,
                                message_id = message.message_id.0
                            );
                        }
                        Err(error) => {
                            kvlog::error!(
                                "durable room log append failed; retention degraded to memory",
                                room_id = room_id.0,
                                error = error.to_string().as_str()
                            );
                            *file = ActiveLog::Disabled;
                        }
                    }
                }
                if *active_bytes > max_active_log_bytes
                    && let Some(path) = path
                {
                    rotate_log(path, file, active_bytes, active_first_id, segments);
                }
                history.trim_to_limit(max_resident_messages);
                history.len()
            }
        };
        self.heads.insert(room_id, message.message_id);
        let recent_ids = self.recent_ids.entry(room_id).or_default();
        if recent_ids.len() == MUTATION_WINDOW_MESSAGES {
            recent_ids.pop_front();
        }
        recent_ids.push_back(message.message_id);
        Ok(history_len)
    }

    /// Test-only: the most recent `max` retained messages, oldest first,
    /// served through the live fetch-plan path.
    #[cfg(test)]
    pub(crate) fn recent(&self, room_id: RoomId, max: usize) -> Vec<ChatMessage> {
        self.messages_before(room_id, None, max).0
    }

    /// Latest message id assigned in the room, `None` before the first message.
    pub fn head(&self, room_id: RoomId) -> Option<MessageId> {
        self.heads.get(&room_id).copied()
    }

    /// Whether `sender` may edit or delete `target` in the room right now.
    ///
    /// The target must be one of the newest [`MUTATION_WINDOW_MESSAGES`] record
    /// ids. When its full record is resident, ownership and target semantics
    /// are checked too. Otherwise only recency is knowable and clients validate
    /// the broadcast mutation against their copy of the target.
    pub fn validate_mutation(
        &self,
        room_id: RoomId,
        target: MessageId,
        sender: UserId,
        kind: MutationKind,
    ) -> Result<(), MutationDenied> {
        if !self
            .recent_ids
            .get(&room_id)
            .is_some_and(|ids| ids.contains(&target))
        {
            return Err(MutationDenied::TargetMissing);
        }
        let Some(history) = self.rooms.get(&room_id).and_then(Retention::history) else {
            return Ok(());
        };
        let mut deleted = false;
        for entry in history
            .entries()
            .iter()
            .rev()
            .take(MUTATION_WINDOW_MESSAGES)
        {
            let Ok(record) = history::parse_message(history.record(*entry)) else {
                return Err(MutationDenied::TargetMissing);
            };
            if record.target == Some(target) && record.flags.deleted() {
                deleted = true;
            }
            if record.message_id != target {
                continue;
            }
            if record.target.is_some() {
                return Err(MutationDenied::MutationRecord);
            }
            if record.sender != sender {
                return Err(MutationDenied::WrongSender);
            }
            if deleted {
                return Err(MutationDenied::Deleted);
            }
            if kind == MutationKind::Edit && record.file_transfer_id.is_some() {
                return Err(MutationDenied::FileMessage);
            }
            return Ok(());
        }
        Ok(())
    }

    /// Test-only view of one resident history page, decoded from the same
    /// plan-and-encode path production uses so pagination has a single
    /// implementation. The bool is the page's `at_start`.
    #[cfg(test)]
    pub(crate) fn messages_before(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: usize,
    ) -> (Vec<ChatMessage>, bool) {
        let plan = self.history_fetch_plan_before(room_id, before, limit, usize::MAX);
        let mut messages = Vec::new();
        for chunk_index in 0..plan.chunk_count() {
            let payload = self
                .encode_history_chunk(&plan, chunk_index)
                .expect("plan encodes before any retention change");
            let chunk = history::decode_chunk(&payload)
                .expect("planned chunk decodes")
                .expect("planned chunk carries the magic");
            messages.splice(0..0, chunk.messages);
        }
        (messages, plan.at_start)
    }

    pub fn history_fetch_plan_before(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: usize,
        target_bytes: usize,
    ) -> HistoryFetchPlan {
        let retention = self.rooms.get(&room_id);
        let has_older = retention.is_some_and(Retention::has_unresident_history);
        let resident = retention
            .and_then(Retention::history)
            .filter(|history| !history.is_empty());
        let mut plan = match resident {
            Some(history) => {
                history.history_chunks_before(room_id, before, limit, target_bytes, has_older)
            }
            None => HistoryFetchPlan {
                room_id,
                before,
                chunks: vec![HistoryChunkSpec {
                    first_id: None,
                    count: 0,
                    records_bytes: 0,
                    at_start: false,
                    complete: true,
                }],
                message_count: 0,
                at_start: false,
            },
        };
        if plan.message_count == 0 {
            // An empty page is the true start only when nothing older than
            // `before` survives anywhere. Residency alone would claim older
            // history that `disk_fetch_request` then refuses to serve,
            // sending clients into a re-request loop.
            let before_bound = before.unwrap_or(MessageId(u64::MAX));
            let at_start = match retention.and_then(Retention::oldest_durable_id) {
                Some(oldest_durable) => oldest_durable >= before_bound,
                None => true,
            };
            plan.at_start = at_start;
            for chunk in &mut plan.chunks {
                chunk.at_start = at_start;
            }
        }
        plan
    }

    /// Builds the background read request serving history strictly before
    /// `before` from rotated segments and the unresident active-log prefix,
    /// or `None` when the room retains nothing on disk older than `before`.
    pub(crate) fn disk_fetch_request(
        &self,
        session_id: SessionId,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: usize,
        target_bytes: usize,
    ) -> Option<HistoryReadRequest> {
        let retention = self.rooms.get(&room_id)?;
        let oldest_durable_id = retention.oldest_durable_id()?;
        let Retention::Durable {
            file,
            active_bytes,
            active_first_id,
            segments,
            ..
        } = retention
        else {
            return None;
        };
        let before_bound = before.unwrap_or(MessageId(u64::MAX));
        if oldest_durable_id >= before_bound {
            return None;
        }
        let sources = 'sources: {
            let mut sources = Vec::new();
            if active_first_id.is_some_and(|first| first < before_bound) {
                let clone = file.open().and_then(|open| {
                    open.try_clone()
                        .map_err(|error| {
                            kvlog::warn!(
                                "active log clone for history paging failed",
                                room_id = room_id.0,
                                error = error.to_string().as_str()
                            );
                        })
                        .ok()
                });
                let Some(clone) = clone else {
                    // A page missing the newest matching records would make
                    // the client's cursor skip them forever; serve nothing
                    // instead.
                    break 'sources Vec::new();
                };
                sources.push(Source::ActiveTail {
                    file: clone,
                    valid_bytes: *active_bytes,
                });
            }
            for segment in segments.iter().rev() {
                if segment.first_id < before_bound {
                    sources.push(Source::Segment {
                        path: segment.path.clone(),
                    });
                }
            }
            sources
        };
        Some(HistoryReadRequest {
            session_id,
            room_id,
            before,
            limit,
            target_bytes,
            oldest_durable_id,
            sources,
        })
    }

    pub fn encode_history_chunk(
        &self,
        plan: &HistoryFetchPlan,
        chunk_index: usize,
    ) -> Result<Vec<u8>, String> {
        let Some(spec) = plan.chunks.get(chunk_index) else {
            return Err("history chunk index is out of range".to_string());
        };
        if spec.first_id.is_none() {
            let mut payload = Vec::with_capacity(history::CHUNK_HEADER_BYTES);
            history::write_chunk_header(
                plan.room_id,
                plan.before,
                spec.at_start,
                spec.complete,
                0,
                &mut payload,
            )?;
            return Ok(payload);
        }
        self.resident(plan.room_id)
            .ok_or_else(|| "history fetch plan no longer has retained messages".to_string())?
            .encode_chunk(plan.room_id, plan.before, spec)
    }

    fn resident(&self, room_id: RoomId) -> Option<&ResidentHistory> {
        self.rooms
            .get(&room_id)
            .and_then(Retention::history)
            .filter(|history| !history.is_empty())
    }

    pub fn dm_rooms(&self) -> &[DmRoom] {
        &self.dm_rooms
    }

    pub fn dm_room_for(&self, first: UserId, second: UserId) -> Option<RoomId> {
        self.dm_rooms
            .iter()
            .find(|dm| dm.pairs(first, second))
            .map(|dm| dm.room_id)
    }

    /// Creates (or returns) the DM room for the unordered user pair. DM rooms
    /// are always durable, and creation fails (rolling the allocation back)
    /// when the registry cannot be persisted: a registry entry that exists
    /// only in memory would let a later process reuse the room id — and its
    /// surviving log — for a different pair.
    pub fn open_dm(
        &mut self,
        first: UserId,
        second: UserId,
        now_ms: u64,
    ) -> Result<RoomId, String> {
        if first == second {
            return Err("cannot open a direct message with yourself".to_string());
        }
        if let Some(existing) = self.dm_room_for(first, second) {
            return Ok(existing);
        }
        let room_id = RoomId(self.next_room_id);
        let next = self
            .next_room_id
            .checked_add(1)
            .ok_or_else(|| "no dynamic room ids are available".to_string())?;
        self.next_room_id = next;
        let room = DmRoom {
            room_id,
            user_a: first,
            user_b: second,
            created_at_ms: now_ms,
        };
        if let Err(error) = self.try_persist_dm(room) {
            self.next_room_id = room_id.0;
            return Err(format!("dm room registry write failed: {error}"));
        }
        self.register_room(room_id, RoomPersistenceConfig::Durable, 0);
        self.dm_rooms.push(room);
        Ok(room_id)
    }

    /// Starts first-time DM persistence without waiting on the event-loop
    /// thread. A concurrent request for the same unordered pair joins the
    /// existing operation and receives the same operation id.
    pub(crate) fn begin_open_dm(
        &mut self,
        first: UserId,
        second: UserId,
        now_ms: u64,
    ) -> Result<OpenDmResult, String> {
        if first == second {
            return Err("cannot open a direct message with yourself".to_string());
        }
        if let Some(existing) = self.dm_room_for(first, second) {
            return Ok(OpenDmResult::Existing(existing));
        }
        if let Some(pending) = &self.pending_dm {
            if pending.room.pairs(first, second) {
                return Ok(OpenDmResult::Pending {
                    operation_id: pending.operation_id,
                });
            }
            return Err("another direct-message room is being persisted; retry shortly".to_string());
        }
        let (Some(writer), Some(events)) = (&self.state_writer, &self.state_events) else {
            return self.open_dm(first, second, now_ms).map(OpenDmResult::Existing);
        };
        if self.data_dir.is_none() {
            return Err("dm room log path is unavailable".to_string());
        }
        let previous_next_room_id = self.next_room_id;
        let room_id = RoomId(previous_next_room_id);
        self.next_room_id = previous_next_room_id
            .checked_add(1)
            .ok_or_else(|| "no dynamic room ids are available".to_string())?;
        let operation_id = self.next_dm_operation;
        self.next_dm_operation = self.next_dm_operation.wrapping_add(1).max(1);
        self.pending_dm = Some(PendingDm {
            operation_id,
            room: DmRoom {
                room_id,
                user_a: first,
                user_b: second,
                created_at_ms: now_ms,
            },
            previous_next_room_id,
        });
        if let Err(error) = writer.insert_dm_async(
            self.pending_dm.as_ref().expect("pending DM inserted").room,
            self.next_room_id,
            operation_id,
            Arc::clone(events),
        ) {
            self.pending_dm = None;
            self.next_room_id = previous_next_room_id;
            return Err(format!("dm room registry write failed: {error}"));
        }
        Ok(OpenDmResult::Pending {
            operation_id,
        })
    }

    pub(crate) fn drain_dm_completions(&mut self) -> Result<VecDeque<DmCompletion>, String> {
        let Some(events) = &self.state_events else {
            return Ok(VecDeque::new());
        };
        let mut replies = events.drain_up_to(16);
        let mut completed = VecDeque::new();
        while let Some(reply) = replies.pop_front() {
            let (operation_id, result) = match reply {
                StateWriteEvent::Dm {
                    operation_id,
                    result,
                } => (operation_id, result),
                StateWriteEvent::Fatal { error } => return Err(error),
            };
            let Some(pending) = self.pending_dm.take() else {
                kvlog::warn!(
                    "unexpected room state completion",
                    operation_id
                );
                continue;
            };
            if pending.operation_id != operation_id {
                kvlog::error!(
                    "room state completion id mismatch",
                    expected = pending.operation_id,
                    actual = operation_id
                );
            }
            if result.is_ok() {
                let room_id = pending.room.room_id;
                let path = self
                    .data_dir
                    .as_ref()
                    .map(|dir| dir.join("rooms").join(format!("{}.log", room_id.0)));
                self.rooms.insert(
                    room_id,
                    Retention::Durable {
                        history: ResidentHistory::default(),
                        file: ActiveLog::Reopen,
                        active_bytes: 0,
                        path,
                        active_first_id: None,
                        segments: Vec::new(),
                    },
                );
                self.recent_ids.insert(room_id, VecDeque::new());
                self.dm_rooms.push(pending.room);
            } else {
                self.next_room_id = pending.previous_next_room_id;
            }
            completed.push_back(DmCompletion {
                operation_id: pending.operation_id,
                room: pending.room,
                result: result
                    .map_err(|error| format!("dm room registry write failed: {error}")),
            });
        }
        Ok(completed)
    }

    /// Queues one keyed watermark update without waiting for the transaction;
    /// used on the message hot path where an fsync stall must not block the
    /// event loop.
    fn queue_persist_watermark(&self, room_id: RoomId, next: u64) {
        let Some(writer) = &self.state_writer else {
            return;
        };
        if let Err(error) = writer.enqueue_watermark(room_id, next) {
            kvlog::error!(
                "room state watermark rejected; server must stop before allocating more ids",
                error = error.as_str()
            );
        }
    }

    /// Atomically reserves the dynamic id and records the DM participants,
    /// waiting so the in-memory creation can roll back on failure.
    fn try_persist_dm(&self, room: DmRoom) -> Result<(), String> {
        let Some(writer) = &self.state_writer else {
            return Ok(());
        };
        writer.insert_dm_sync(room, self.next_room_id)
    }
}

/// The lowest dynamic room id above every room log already on disk. Guards id
/// allocation when the state database is lost or corrupt: reusing an id whose log
/// survives would seed the new room with the old room's private history.
fn next_room_id_above_existing_logs(dir: &Path) -> u32 {
    let mut next = FIRST_DYNAMIC_ROOM_ID;
    let Ok(entries) = fs::read_dir(dir.join("rooms")) else {
        return next;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((room_id, _)) = name.split_once('.') else {
            continue;
        };
        let Ok(room_id) = room_id.parse::<u32>() else {
            continue;
        };
        if room_id >= FIRST_DYNAMIC_ROOM_ID {
            next = next.max(room_id.saturating_add(1));
        }
    }
    next
}


/// Appends one `len:u32 | record` frame, rejecting records the loader would
/// treat as a corrupt tail. Without the write-time check an oversized record
/// would append fine and the next startup's tail repair would silently drop
/// it and every later message.
fn write_log_record(file: &mut File, payload: &[u8]) -> std::io::Result<usize> {
    if payload.len() > history::MAX_LOG_RECORD_BYTES as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "history record exceeds the log record cap",
        ));
    }
    let len = payload.len() as u32;
    file.write_all(&len.to_le_bytes())?;
    file.write_all(payload)?;
    Ok(4 + payload.len())
}

enum LogRepair {
    /// Truncate the file to the last valid record boundary.
    Truncate,
    /// Keep the valid prefix in memory without touching the file.
    ReadOnly,
}

struct LoadedLog {
    history: ResidentHistory,
    valid_bytes: u64,
    tail_repair_failed: bool,
}

fn load_log_repairing(path: &Path) -> LoadedLog {
    load_log(path, LogRepair::Truncate)
}

/// Walks the `len:u32 | record` framing of a log byte buffer, stopping at the
/// first invalid frame; [`LogRecords::offset`] then marks the valid prefix.
pub(crate) struct LogRecords<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> LogRecords<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// True when every byte was consumed as well-framed records.
    pub(crate) fn exhausted(&self) -> bool {
        self.offset == self.bytes.len()
    }

    pub(crate) fn next_record(&mut self) -> Option<&'a [u8]> {
        if self.bytes.len() - self.offset < 4 {
            return None;
        }
        let len = u32::from_le_bytes(
            self.bytes[self.offset..self.offset + 4]
                .try_into()
                .expect("4 bytes"),
        );
        if len > history::MAX_LOG_RECORD_BYTES {
            return None;
        }
        let len = len as usize;
        let payload = self.bytes.get(self.offset + 4..self.offset + 4 + len)?;
        self.offset += 4 + len;
        Some(payload)
    }
}

fn load_log(path: &Path, repair: LogRepair) -> LoadedLog {
    let Ok(bytes) = fs::read(path) else {
        return LoadedLog {
            history: ResidentHistory::default(),
            valid_bytes: 0,
            tail_repair_failed: false,
        };
    };
    let mut history = ResidentHistory::default();
    let mut records = LogRecords::new(&bytes);
    let mut offset = 0usize;
    while let Some(record) = records.next_record() {
        if history.append_record(record).is_err() {
            break;
        }
        offset = records.offset();
    }
    let mut tail_repair_failed = false;
    if offset < bytes.len() {
        match repair {
            LogRepair::Truncate => {
                kvlog::warn!(
                    "durable room log tail corrupt; truncating",
                    path = path.display().to_string().as_str(),
                    valid_bytes = offset,
                    total_bytes = bytes.len()
                );
                if let Err(error) = truncate_log(path, offset as u64) {
                    kvlog::error!(
                        "durable room log tail truncation failed",
                        path = path.display().to_string().as_str(),
                        error = error.to_string().as_str()
                    );
                    tail_repair_failed = true;
                }
            }
            LogRepair::ReadOnly => {
                kvlog::warn!(
                    "durable room log backup tail corrupt; loading valid prefix",
                    path = path.display().to_string().as_str(),
                    valid_bytes = offset,
                    total_bytes = bytes.len()
                );
            }
        }
    }
    LoadedLog {
        history,
        valid_bytes: offset as u64,
        tail_repair_failed,
    }
}

fn truncate_log(path: &Path, valid_bytes: u64) -> std::io::Result<()> {
    #[cfg(test)]
    if path.with_extension("fail-truncate").exists() {
        return Err(std::io::Error::other("forced log truncation failure"));
    }
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(valid_bytes)?;
    file.sync_all()
}

fn rotate_log(
    path: &Path,
    file: &mut ActiveLog,
    active_bytes: &mut u64,
    active_first_id: &mut Option<MessageId>,
    segments: &mut Vec<Segment>,
) {
    let Some(first_id) = *active_first_id else {
        return;
    };
    let segment = segment_path(path, first_id);
    if let Err(error) = fs::rename(path, &segment) {
        kvlog::error!(
            "durable room log rotate failed",
            path = path.display().to_string().as_str(),
            error = error.to_string().as_str()
        );
        return;
    }
    segments.push(Segment {
        path: segment,
        first_id,
    });
    *active_first_id = None;
    // The rename already freed the path, so the fresh log starts empty either
    // way; resetting `active_bytes` on failure keeps later appends from
    // re-entering rotation, and `Reopen` retries the open on a later append
    // once a transient error (EMFILE, ENOSPC) clears.
    *active_bytes = 0;
    match open_active_log(path) {
        Ok(fresh) => {
            *file = ActiveLog::Open(fresh);
        }
        Err(error) => {
            kvlog::error!(
                "durable room log reopen after rotate failed; retrying on a later append",
                path = path.display().to_string().as_str(),
                error = error.to_string().as_str()
            );
            *file = ActiveLog::Reopen;
        }
    }
}

fn open_active_log(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
}

fn segment_path(path: &Path, first_id: MessageId) -> PathBuf {
    let mut raw = path.as_os_str().to_owned();
    raw.push(format!(".{}", first_id.0));
    PathBuf::from(raw)
}

fn list_segments(active_path: &Path) -> Vec<Segment> {
    let Some(dir) = active_path.parent() else {
        return Vec::new();
    };
    let Some(name) = active_path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let prefix = format!("{name}.");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut segments = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(suffix) = file_name.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(first_id) = suffix.parse::<u64>() else {
            continue;
        };
        segments.push(Segment {
            path: dir.join(file_name),
            first_id: MessageId(first_id),
        });
    }
    segments.sort_unstable_by_key(|segment| segment.first_id);
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::ids::UserId;

    #[test]
    fn async_log_reopen_publishes_a_read_handle() {
        let dir = temp_dir("async-reopen-handle");
        fs::create_dir_all(dir.join("rooms")).unwrap();
        let room_id = RoomId(1);
        let path = dir.join("rooms").join("1.log");
        let poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(1)).unwrap());
        let notifier = Arc::new(EventNotifier::new(waker));
        let events = Arc::new(EventQueue::new(
            notifier,
            ROOM_LOG_EVENTS,
            "room-log-test",
        ));
        let writer = RoomLogWriter::spawn(
            HashMap::from([(
                room_id,
                RoomLogWorkerState {
                    path,
                    file: ActiveLog::Reopen,
                    needs_read_handle: true,
                    active_bytes: 0,
                    active_first_id: None,
                },
            )]),
            Arc::clone(&events),
        );
        let message = test_message(room_id, 1);
        let mut record = Vec::new();
        history::write_message(&message, &mut record);
        writer
            .enqueue(RoomLogWrite {
                room_id,
                message_id: message.message_id,
                record,
                max_active_log_bytes: u64::MAX,
                path: None,
            })
            .unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let event = loop {
            if let Some(event) = events.drain().pop_front() {
                break event;
            }
            assert!(std::time::Instant::now() < deadline, "room log reply");
            std::thread::yield_now();
        };
        assert!(event.error.is_none());
        assert!(matches!(event.file, LogFileUpdate::Open(_)));
    }

    fn test_message(room_id: RoomId, id: u64) -> ChatMessage {
        ChatMessage {
            message_id: MessageId(id),
            room_id,
            sender: UserId(1),
            sender_name: "alice".to_string(),
            timestamp_ms: 1_000 + id,
            body: format!("message {id}"),
            file_transfer_id: None,
            flags: rpc::control::MessageFlags::default(),
            target: None,
        }
    }

    fn durable_room(id: u32) -> RoomConfig {
        RoomConfig {
            id,
            name: format!("room-{id}"),
            members: None,
            persistence: RoomPersistenceConfig::Durable,
            memory_limit: None,
            mls_retention_days: None,
            is_default: false,
        }
    }

    fn memory_room(id: u32, limit: u64) -> RoomConfig {
        RoomConfig {
            id,
            name: format!("room-{id}"),
            members: None,
            persistence: RoomPersistenceConfig::Memory,
            memory_limit: Some(limit),
            mls_retention_days: None,
            is_default: false,
        }
    }

    fn none_room(id: u32) -> RoomConfig {
        RoomConfig {
            id,
            name: format!("room-{id}"),
            members: None,
            persistence: RoomPersistenceConfig::None,
            memory_limit: None,
            mls_retention_days: None,
            is_default: false,
        }
    }

    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "chatt-room-store-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(dir.as_path());
            Scratch { dir }
        }
    }

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.dir
        }
    }

    impl AsRef<Path> for Scratch {
        fn as_ref(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_dir(tag: &str) -> Scratch {
        Scratch::new(tag)
    }

    #[test]
    fn durable_ids_resume_after_reload() {
        let dir = temp_dir("resume");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        for _ in 0..3 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        let next = store.allocate_message_id(room);
        assert!(next.0 > 3, "id {} must not reuse 1..=3", next.0);
        assert_eq!(store.recent(room, 10).len(), 3);
        assert_eq!(store.head(room), Some(MessageId(3)));
    }

    #[test]
    fn allocation_does_not_advance_head_until_append() {
        let mut store = RoomStore::open(None, &[memory_room(1, 16)]);
        let room = RoomId(1);

        let id = store.allocate_message_id(room);
        assert_eq!(store.head(room), None);

        store.append(room, &test_message(room, id.0));
        assert_eq!(store.head(room), Some(id));
    }

    #[test]
    fn restart_head_tracks_retained_messages_not_reserved_ids() {
        let durable_dir = temp_dir("durable-head");
        let room = RoomId(1);
        let mut store = RoomStore::open(Some(durable_dir.to_path_buf()), &[durable_room(1)]);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(durable_dir.to_path_buf()), &[durable_room(1)]);
        assert_eq!(store.head(room), Some(MessageId(1)));
        drop(store);

        let memory_dir = temp_dir("memory-head");
        let mut store = RoomStore::open(Some(memory_dir.to_path_buf()), &[memory_room(1, 16)]);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(memory_dir.to_path_buf()), &[memory_room(1, 16)]);
        assert_eq!(store.head(room), None);
    }

    #[test]
    fn watermark_blocks_never_reuse_ids() {
        let dir = temp_dir("watermark");
        let rooms = [memory_room(1, 16)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        assert_eq!(store.allocate_message_id(room), MessageId(1));
        assert_eq!(store.allocate_message_id(room), MessageId(2));
        drop(store);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        assert_eq!(
            store.allocate_message_id(room),
            MessageId(2 * MESSAGE_ID_RESERVE),
            "restart resumes at the persisted watermark, one headroom block past the used ids"
        );
    }

    #[test]
    fn memory_ring_honors_limit() {
        let mut store = RoomStore::open(None, &[memory_room(1, 2)]);
        let room = RoomId(1);
        for _ in 0..5 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        let recent = store.recent(room, 10);
        let ids: Vec<u64> = recent.iter().map(|message| message.message_id.0).collect();
        assert_eq!(ids, vec![4, 5]);
    }

    #[test]
    fn none_rooms_retain_nothing() {
        let mut store = RoomStore::open(
            None,
            &[RoomConfig {
                id: 1,
                name: "lobby".to_string(),
                members: None,
                persistence: RoomPersistenceConfig::None,
                memory_limit: None,
                mls_retention_days: None,
                is_default: true,
            }],
        );
        let room = RoomId(1);
        let id = store.allocate_message_id(room);
        assert_eq!(store.append(room, &test_message(room, id.0)), 0);
        assert!(store.recent(room, 10).is_empty());
        let (messages, at_start) = store.messages_before(room, None, 10);
        assert!(messages.is_empty());
        assert!(at_start);
    }

    #[test]
    fn dm_registry_round_trips() {
        let dir = temp_dir("dm");
        let rooms = [durable_room(1)];

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        let dm = store.open_dm(UserId(3), UserId(7), 1_234).unwrap();
        assert!(dm.0 >= FIRST_DYNAMIC_ROOM_ID);
        assert_eq!(store.open_dm(UserId(7), UserId(3), 9_999).unwrap(), dm);
        let id = store.allocate_message_id(dm);
        store.append(dm, &test_message(dm, id.0));
        drop(store);

        let store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        assert_eq!(store.dm_room_for(UserId(3), UserId(7)), Some(dm));
        assert_eq!(store.dm_rooms().len(), 1);
        assert_eq!(store.recent(dm, 10).len(), 1);
    }

    #[test]
    fn empty_dm_recovers_from_the_database_without_a_log_prerequisite() {
        let dir = temp_dir("empty-dm-database-recovery");
        fs::create_dir_all(dir.join("rooms")).unwrap();
        let room = DmRoom {
            room_id: RoomId(FIRST_DYNAMIC_ROOM_ID),
            user_a: UserId(3),
            user_b: UserId(7),
            created_at_ms: 1_234,
        };
        let (_, writer) = room_state::open(&dir.join(room_state::FILE_NAME)).unwrap();
        writer
            .insert_dm_sync(room, FIRST_DYNAMIC_ROOM_ID + 1)
            .unwrap();
        drop(writer);
        let log_path = dir.join("rooms").join(format!("{}.log", room.room_id.0));
        assert!(!log_path.exists());

        let store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        assert_eq!(
            store.dm_room_for(UserId(3), UserId(7)),
            Some(room.room_id)
        );
        assert!(log_path.exists(), "startup must recreate the derived empty log");
    }

    #[test]
    fn corrupt_state_database_prevents_durable_store_startup() {
        let dir = temp_dir("corrupt-state-database");
        fs::create_dir_all(dir.as_ref()).unwrap();
        fs::write(dir.join(room_state::FILE_NAME), b"not a redb database").unwrap();

        let result = RoomStore::try_open(Some(dir.to_path_buf()), &[]);
        assert!(result.is_err());
    }

    #[test]
    fn async_dm_creation_commits_registry_before_enrolling_log_writer() {
        let dir = temp_dir("async-dm-transaction");
        let mut poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(1)).unwrap());
        let notifier = Arc::new(EventNotifier::new(waker));
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        store.enable_async_state_writes(Arc::clone(&notifier));
        store.enable_async_log_writes(Arc::clone(&notifier));

        let OpenDmResult::Pending { .. } = store
            .begin_open_dm(UserId(1), UserId(2), 1_000)
            .unwrap()
        else {
            panic!("first async DM unexpectedly already existed");
        };
        let mut events = mio::Events::with_capacity(2);
        poll.poll(&mut events, Some(std::time::Duration::from_secs(2)))
            .unwrap();
        assert_ne!(notifier.take_ready() & ROOM_STATE_EVENTS, 0);
        let completion = store
            .drain_dm_completions()
            .unwrap()
            .pop_front()
            .unwrap();
        completion.result.unwrap();
        let dm = completion.room.room_id;
        let log_path = dir.join("rooms").join(format!("{}.log", dm.0));
        assert!(dir.join(room_state::FILE_NAME).exists());
        assert!(
            !log_path.exists(),
            "an empty log is derived state, not a transaction prerequisite"
        );

        let id = store.allocate_message_id(dm);
        store.try_append(dm, &test_message(dm, id.0)).unwrap();
        poll.poll(&mut events, Some(std::time::Duration::from_secs(2)))
            .unwrap();
        assert_ne!(notifier.take_ready() & ROOM_LOG_EVENTS, 0);
        store.drain_log_events();
        assert!(log_path.exists());
        drop(store);

        fs::remove_file(dir.join(room_state::FILE_NAME)).unwrap();
        let mut reopened = RoomStore::open(Some(dir.to_path_buf()), &[]);
        let fresh = reopened.open_dm(UserId(3), UserId(4), 2_000).unwrap();
        assert_ne!(fresh, dm, "the async room id tombstone was not recovered");
    }

    #[test]
    fn terminal_state_writer_failure_wakes_and_stops_the_event_loop() {
        let dir = temp_dir("terminal-state-writer");
        let mut poll = mio::Poll::new().unwrap();
        let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(1)).unwrap());
        let notifier = Arc::new(EventNotifier::new(waker));
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        store.enable_async_state_writes(Arc::clone(&notifier));
        store
            .state_writer
            .as_ref()
            .expect("persistent store")
            .fail_next_write();

        let OpenDmResult::Pending { .. } = store
            .begin_open_dm(UserId(1), UserId(2), 1_000)
            .unwrap()
        else {
            panic!("first async DM unexpectedly already existed");
        };
        let mut events = mio::Events::with_capacity(2);
        poll.poll(&mut events, Some(std::time::Duration::from_secs(2)))
            .unwrap();
        assert_ne!(notifier.take_ready() & ROOM_STATE_EVENTS, 0);

        let error = match store.drain_dm_completions() {
            Ok(_) => panic!("terminal state failure was not surfaced"),
            Err(error) => error,
        };
        assert!(error.contains("forced room-state write failure"));
        assert!(store.begin_open_dm(UserId(1), UserId(2), 2_000).is_err());
    }

    #[test]
    fn dm_rooms_allocate_distinct_ids() {
        let dir = temp_dir("dm-distinct");
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[durable_room(1)]);

        let first = store.open_dm(UserId(1), UserId(2), 0).unwrap();
        let second = store.open_dm(UserId(1), UserId(3), 0).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn dm_room_id_never_reused_after_state_loss() {
        let dir = temp_dir("dm-state-loss");
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        let dm = store.open_dm(UserId(1), UserId(2), 1_000).unwrap();
        let id = store.allocate_message_id(dm);
        store.append(dm, &test_message(dm, id.0));
        drop(store);

        fs::remove_file(dir.join(room_state::FILE_NAME)).unwrap();
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        let fresh = store.open_dm(UserId(3), UserId(4), 2_000).unwrap();
        assert_ne!(fresh, dm, "id with a surviving log was handed out again");
        assert!(store.recent(fresh, 10).is_empty());
    }

    #[test]
    fn dm_room_id_floors_over_backup_logs() {
        let dir = temp_dir("dm-backup-floor");
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        let dm = store.open_dm(UserId(1), UserId(2), 1_000).unwrap();
        let id = store.allocate_message_id(dm);
        store.append(dm, &test_message(dm, id.0));
        drop(store);

        fs::remove_file(dir.join(room_state::FILE_NAME)).unwrap();
        let log = dir.join("rooms").join(format!("{}.log", dm.0));
        fs::rename(&log, segment_path(&log, MessageId(1))).unwrap();
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        let fresh = store.open_dm(UserId(3), UserId(4), 2_000).unwrap();
        assert!(fresh.0 > dm.0, "backup log {} vs fresh {}", dm.0, fresh.0);
    }

    #[test]
    fn open_dm_rejects_the_same_user_on_both_ends() {
        let mut store = RoomStore::open(None, &[]);

        assert!(store.open_dm(UserId(1), UserId(1), 1_000).is_err());
        assert!(store.dm_rooms().is_empty());
        assert_eq!(store.dm_room_for(UserId(1), UserId(1)), None);
    }

    #[test]
    fn open_dm_rolls_back_and_closes_writer_when_state_persist_fails() {
        let dir = temp_dir("dm-rollback");
        let mut store = RoomStore::open(Some(dir.to_path_buf()), &[]);
        store
            .state_writer
            .as_ref()
            .expect("persistent store")
            .fail_next_write();
        let result = store.open_dm(UserId(1), UserId(2), 1_000);
        assert!(result.is_err(), "unpersistable dm creation must fail");
        assert_eq!(store.dm_room_for(UserId(1), UserId(2)), None);

        assert!(store.open_dm(UserId(1), UserId(2), 1_000).is_err());
        assert_eq!(store.dm_room_for(UserId(1), UserId(2)), None);
    }

    #[test]
    fn corrupt_log_tail_is_truncated() {
        let dir = temp_dir("corrupt");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        for _ in 0..3 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let log = dir.join("rooms").join("1.log");
        let mut bytes = fs::read(&log).unwrap();
        bytes.truncate(bytes.len() - 3);
        bytes.extend_from_slice(&[0xFF, 0xFF]);
        fs::write(&log, &bytes).unwrap();

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        let recent = store.recent(room, 10);
        assert_eq!(recent.len(), 2);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        assert_eq!(store.recent(room, 10).len(), 3);
    }

    #[test]
    fn unrepairable_corrupt_tail_degrades_to_memory() {
        let dir = temp_dir("corrupt-unrepairable");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        for _ in 0..3 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let log = dir.join("rooms").join("1.log");
        let mut bytes = fs::read(&log).unwrap();
        bytes.truncate(bytes.len() - 3);
        bytes.extend_from_slice(&[0xFF, 0xFF]);
        fs::write(&log, &bytes).unwrap();
        let fail_truncate = log.with_extension("fail-truncate");
        fs::write(&fail_truncate, []).unwrap();

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        fs::remove_file(fail_truncate).unwrap();
        assert_eq!(store.recent(room, 10).len(), 2);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        assert_eq!(
            store.recent(room, 10).len(),
            3,
            "appends must continue in memory"
        );
        drop(store);

        assert_eq!(
            fs::read(&log).unwrap(),
            bytes,
            "nothing may be written after the unrepaired corruption"
        );
    }

    #[test]
    fn messages_before_paginates_backwards() {
        let mut store = RoomStore::open(None, &[memory_room(1, 100)]);
        let room = RoomId(1);
        for _ in 0..9 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        let (newest, at_start) = store.messages_before(room, None, 4);
        let ids: Vec<u64> = newest.iter().map(|message| message.message_id.0).collect();
        assert_eq!(ids, vec![6, 7, 8, 9]);
        assert!(!at_start);

        let (older, at_start) = store.messages_before(room, Some(MessageId(6)), 4);
        let ids: Vec<u64> = older.iter().map(|message| message.message_id.0).collect();
        assert_eq!(ids, vec![2, 3, 4, 5]);
        assert!(!at_start);

        let (oldest, at_start) = store.messages_before(room, Some(MessageId(2)), 4);
        let ids: Vec<u64> = oldest.iter().map(|message| message.message_id.0).collect();
        assert_eq!(ids, vec![1]);
        assert!(at_start);
    }

    #[test]
    fn rotation_preserves_all_records_as_segments() {
        let dir = temp_dir("rotate-preserve");
        let rooms = [durable_room(1)];
        let room = RoomId(1);
        let tuning = StoreTuning {
            max_active_log_bytes: 256,
            max_resident_messages: 4,
        };

        let mut store = RoomStore::open_with_tuning(Some(dir.to_path_buf()), &rooms, tuning);
        for _ in 0..20 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        let Some(Retention::Durable { segments, .. }) = store.rooms.get(&room) else {
            panic!("room 1 must be durable");
        };
        assert!(
            segments.len() >= 2,
            "rotations must produce segments, got {}",
            segments.len()
        );
        for segment in segments {
            let loaded = load_log(&segment.path, LogRepair::ReadOnly).history;
            assert_eq!(
                loaded.first_message_id(),
                Some(segment.first_id),
                "segment name must match its first record"
            );
        }
        drop(store);

        let store = RoomStore::open_with_tuning(
            Some(dir.to_path_buf()),
            &rooms,
            StoreTuning {
                max_active_log_bytes: 256,
                max_resident_messages: 1024,
            },
        );
        let ids: Vec<u64> = store
            .recent(room, 100)
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, (1..=20).collect::<Vec<u64>>());
    }

    #[test]
    fn startup_fills_resident_from_newest_segments() {
        let dir = temp_dir("resident-fill");
        let rooms = [durable_room(1)];
        let room = RoomId(1);
        let tuning = StoreTuning {
            max_active_log_bytes: 256,
            max_resident_messages: 8,
        };

        let mut store = RoomStore::open_with_tuning(Some(dir.to_path_buf()), &rooms, tuning);
        for _ in 0..30 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let store = RoomStore::open_with_tuning(Some(dir.to_path_buf()), &rooms, tuning);
        let ids: Vec<u64> = store
            .recent(room, 100)
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(
            ids,
            (23..=30).collect::<Vec<u64>>(),
            "resident must be the newest suffix filled from segments"
        );
        let (_, at_start) = store.messages_before(room, None, 100);
        assert!(!at_start, "older messages remain on disk");
    }

    #[test]
    fn at_start_defers_to_unresident_disk_history() {
        let dir = temp_dir("at-start-disk");
        let rooms = [durable_room(1)];
        let room = RoomId(1);
        let tuning = StoreTuning {
            max_active_log_bytes: 1 << 20,
            max_resident_messages: 4,
        };

        let mut store = RoomStore::open_with_tuning(Some(dir.to_path_buf()), &rooms, tuning);
        for _ in 0..10 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        let (messages, at_start) = store.messages_before(room, None, 10);
        let ids: Vec<u64> = messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![7, 8, 9, 10]);
        assert!(!at_start, "unresident records remain in the active log");

        let plan = store.history_fetch_plan_before(room, Some(MessageId(7)), 4, 4096);
        assert_eq!(plan.message_count, 0);
        assert!(!plan.at_start);

        let request = store
            .disk_fetch_request(SessionId(1), room, Some(MessageId(7)), 4, 4096)
            .expect("disk records exist below the resident window");
        assert_eq!(request.oldest_durable_id, MessageId(1));
        assert_eq!(request.sources.len(), 1, "active tail is the only source");
    }

    #[test]
    fn disk_fetch_request_is_none_at_true_start() {
        let dir = temp_dir("disk-fetch-none");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        for _ in 0..3 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        assert!(
            store
                .disk_fetch_request(SessionId(1), room, Some(MessageId(1)), 4, 4096)
                .is_none()
        );

        let mut memory = RoomStore::open(None, &[memory_room(2, 16)]);
        let memory_room_id = RoomId(2);
        let id = memory.allocate_message_id(memory_room_id);
        memory.append(memory_room_id, &test_message(memory_room_id, id.0));
        assert!(
            memory
                .disk_fetch_request(SessionId(1), memory_room_id, None, 4, 4096)
                .is_none()
        );
    }

    #[test]
    fn oversized_record_is_rejected_at_write_and_later_messages_survive() {
        let dir = temp_dir("oversized-record");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        let id = store.allocate_message_id(room);
        let mut oversized = test_message(room, id.0);
        oversized.body = "x".repeat(history::MAX_LOG_RECORD_BYTES as usize + 1);
        store.append(room, &oversized);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        let ids: Vec<u64> = store
            .recent(room, 10)
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(
            ids,
            vec![1, 3],
            "an oversized record must not take later messages with it"
        );
    }

    #[test]
    fn smaller_records_still_round_trip_under_the_cap() {
        let dir = temp_dir("cap-round-trip");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        let id = store.allocate_message_id(room);
        let mut large = test_message(room, id.0);
        large.body = "x".repeat(64 * 1024);
        store.append(room, &large);
        drop(store);

        let store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        assert_eq!(store.recent(room, 10).len(), 1);
    }

    #[test]
    fn stale_fetch_plan_errors_instead_of_serving_shifted_messages() {
        let mut store = RoomStore::open(None, &[memory_room(1, 4)]);
        let room = RoomId(1);
        for _ in 0..4 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        let plan = store.history_fetch_plan_before(room, None, 4, usize::MAX);
        assert_eq!(plan.message_count, 4);
        for _ in 0..2 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        assert!(
            store.encode_history_chunk(&plan, 0).is_err(),
            "a plan trimmed out of residency must error, not serve newer ids"
        );
    }

    #[test]
    fn fetch_plan_is_stable_across_new_appends() {
        let mut store = RoomStore::open(None, &[memory_room(1, 100)]);
        let room = RoomId(1);
        for _ in 0..4 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        let plan = store.history_fetch_plan_before(room, None, 4, usize::MAX);
        for _ in 0..2 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        let payload = store.encode_history_chunk(&plan, 0).unwrap();
        let chunk = history::decode_chunk(&payload).unwrap().unwrap();
        let ids: Vec<u64> = chunk
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(
            ids,
            vec![1, 2, 3, 4],
            "appends must not shift a planned page"
        );
    }

    #[test]
    fn reopen_retry_after_failed_rotation_restores_durability() {
        let dir = temp_dir("reopen-retry");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        for _ in 0..2 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        // Simulate the state rotate_log leaves after the rename succeeded but
        // the fresh open failed: the log became a segment and the path is
        // free.
        let log = dir.join("rooms").join("1.log");
        let segment = segment_path(&log, MessageId(1));
        fs::rename(&log, &segment).unwrap();
        let Some(Retention::Durable {
            file,
            active_bytes,
            active_first_id,
            segments,
            ..
        }) = store.rooms.get_mut(&room)
        else {
            panic!("room 1 must be durable");
        };
        *file = ActiveLog::Reopen;
        *active_bytes = 0;
        *active_first_id = None;
        segments.push(Segment {
            path: segment,
            first_id: MessageId(1),
        });

        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        let ids: Vec<u64> = store
            .recent(room, 10)
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(
            ids,
            vec![1, 2, 3],
            "the append after a failed reopen must recover durability"
        );
    }

    #[test]
    fn empty_fetch_at_durable_floor_reports_at_start() {
        let dir = temp_dir("empty-at-floor");
        let rooms = [durable_room(1)];
        let room = RoomId(1);
        let tuning = StoreTuning {
            max_active_log_bytes: 1 << 20,
            max_resident_messages: 4,
        };

        let mut store = RoomStore::open_with_tuning(Some(dir.to_path_buf()), &rooms, tuning);
        for _ in 0..10 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }

        let plan = store.history_fetch_plan_before(room, Some(MessageId(1)), 4, 4096);
        assert_eq!(plan.message_count, 0);
        assert!(
            plan.at_start,
            "nothing older than the durable floor exists to fetch"
        );
        let payload = store.encode_history_chunk(&plan, 0).unwrap();
        let chunk = history::decode_chunk(&payload).unwrap().unwrap();
        assert!(chunk.at_start);
        assert!(
            store
                .disk_fetch_request(SessionId(1), room, Some(MessageId(1)), 4, 4096)
                .is_none(),
            "the plan and the disk fetch must agree there is nothing older"
        );
    }

    #[test]
    fn legacy_backup_log_loads_as_segment() {
        let dir = temp_dir("legacy-backup");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        for _ in 0..3 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let log = dir.join("rooms").join("1.log");
        fs::rename(&log, segment_path(&log, MessageId(1))).unwrap();

        let mut store = RoomStore::open(Some(dir.to_path_buf()), &rooms);
        assert_eq!(store.recent(room, 10).len(), 3);
        let id = store.allocate_message_id(room);
        assert!(id.0 > 3, "ids must resume above segment records");
    }

    fn append_next(store: &mut RoomStore, room: RoomId) -> MessageId {
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        id
    }

    fn append_mutation(
        store: &mut RoomStore,
        room: RoomId,
        target: MessageId,
        kind: MutationKind,
    ) -> MessageId {
        let id = store.allocate_message_id(room);
        let mut message = test_message(room, id.0);
        match kind {
            MutationKind::Edit => message.flags.set_edited(),
            MutationKind::Delete => {
                message.flags.set_deleted();
                message.body = String::new();
            }
        }
        message.target = Some(target);
        store.append(room, &message);
        id
    }

    #[test]
    fn accepts_mutation_within_window() {
        let mut store = RoomStore::open(None, &[memory_room(1, 600)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);
        for _ in 0..MUTATION_WINDOW_MESSAGES - 1 {
            append_next(&mut store, room);
        }

        assert_eq!(
            store.validate_mutation(room, target, UserId(1), MutationKind::Edit),
            Ok(())
        );
        assert_eq!(
            store.validate_mutation(room, target, UserId(1), MutationKind::Delete),
            Ok(())
        );
    }

    #[test]
    fn stateless_room_accepts_recent_mutation_without_sender_metadata() {
        let mut store = RoomStore::open(None, &[none_room(1)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);

        assert_eq!(
            store.validate_mutation(room, target, UserId(2), MutationKind::Edit),
            Ok(())
        );
        assert_eq!(
            store.validate_mutation(room, target, UserId(2), MutationKind::Delete),
            Ok(())
        );
        assert!(store.recent(room, 1).is_empty());
    }

    #[test]
    fn recency_fallback_survives_a_short_memory_history() {
        let mut store = RoomStore::open(None, &[memory_room(1, 1)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);
        append_next(&mut store, room);

        assert_eq!(
            store.validate_mutation(room, target, UserId(2), MutationKind::Edit),
            Ok(())
        );
    }

    #[test]
    fn rejects_target_beyond_window() {
        let mut store = RoomStore::open(None, &[memory_room(1, 600)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);
        for _ in 0..MUTATION_WINDOW_MESSAGES {
            append_next(&mut store, room);
        }

        assert_eq!(
            store.validate_mutation(room, target, UserId(1), MutationKind::Edit),
            Err(MutationDenied::TargetMissing)
        );
    }

    #[test]
    fn rejects_missing_target_and_wrong_sender() {
        let mut store = RoomStore::open(None, &[memory_room(1, 16)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);

        assert_eq!(
            store.validate_mutation(room, MessageId(999), UserId(1), MutationKind::Delete),
            Err(MutationDenied::TargetMissing)
        );
        assert_eq!(
            store.validate_mutation(room, target, UserId(2), MutationKind::Edit),
            Err(MutationDenied::WrongSender)
        );
    }

    #[test]
    fn rejects_edit_of_file_message_but_allows_delete() {
        let mut store = RoomStore::open(None, &[memory_room(1, 16)]);
        let room = RoomId(1);
        let id = store.allocate_message_id(room);
        let mut message = test_message(room, id.0);
        message.file_transfer_id = Some(rpc::ids::FileTransferId(9));
        store.append(room, &message);

        assert_eq!(
            store.validate_mutation(room, id, UserId(1), MutationKind::Edit),
            Err(MutationDenied::FileMessage)
        );
        assert_eq!(
            store.validate_mutation(room, id, UserId(1), MutationKind::Delete),
            Ok(())
        );
    }

    #[test]
    fn rejects_mutating_a_mutation_record() {
        let mut store = RoomStore::open(None, &[memory_room(1, 16)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);
        let edit = append_mutation(&mut store, room, target, MutationKind::Edit);

        assert_eq!(
            store.validate_mutation(room, edit, UserId(1), MutationKind::Edit),
            Err(MutationDenied::MutationRecord)
        );
    }

    #[test]
    fn rejects_mutations_after_delete_but_allows_delete_after_edit() {
        let mut store = RoomStore::open(None, &[memory_room(1, 16)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);
        append_mutation(&mut store, room, target, MutationKind::Edit);
        assert_eq!(
            store.validate_mutation(room, target, UserId(1), MutationKind::Delete),
            Ok(())
        );

        append_mutation(&mut store, room, target, MutationKind::Delete);
        assert_eq!(
            store.validate_mutation(room, target, UserId(1), MutationKind::Edit),
            Err(MutationDenied::Deleted)
        );
        assert_eq!(
            store.validate_mutation(room, target, UserId(1), MutationKind::Delete),
            Err(MutationDenied::Deleted)
        );
    }

    #[test]
    fn mutation_append_advances_head() {
        let mut store = RoomStore::open(None, &[memory_room(1, 16)]);
        let room = RoomId(1);
        let target = append_next(&mut store, room);
        let mutation = append_mutation(&mut store, room, target, MutationKind::Edit);

        assert!(mutation > target);
        assert_eq!(store.head(room), Some(mutation));
    }
}
