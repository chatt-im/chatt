//! Server-side room message retention and durable room state.
//!
//! One [`RoomStore`] owns, per room, the retention backend selected by the
//! room's persistence config: nothing, a bounded in-memory ring, or an
//! append-only on-disk log. A durable room keeps only the newest messages
//! resident; when the active log grows past its size cap it rotates into an
//! immutable segment named `<log>.<first-message-id>`, and segments are never
//! deleted, so older pages stay servable from disk (see
//! [`crate::history_reader`]). The store also owns the durable server state
//! that must survive restarts: per-room message-id watermarks (so ids are
//! never reused) and the registry of runtime-created DM rooms.
//!
//! Message ids are per-room, monotonic from 1, and durable. Allocation uses a
//! block reservation: the persisted watermark is rounded up to the next
//! [`MESSAGE_ID_RESERVE`] boundary whenever an allocation crosses it, so rooms
//! without durable logs cost one state write per block instead of one per
//! message. A restart resumes from the watermark, skipping at most
//! `MESSAGE_ID_RESERVE - 1` ids.

use hashbrown::HashMap;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use rpc::{
    control::ChatMessage,
    history,
    ids::{MessageId, RoomId, SessionId, UserId},
};
use toml_spanner::Toml;

use crate::config::{FIRST_DYNAMIC_ROOM_ID, RoomConfig, RoomPersistenceConfig, atomic_write_toml};
use crate::history_reader::{HistoryReadRequest, Source};

/// Message-id block reserved per state write; a restart skips at most this
/// many ids per room.
pub const MESSAGE_ID_RESERVE: u64 = 1024;
/// Upper bound on one serialized log record; larger frames mark the log tail
/// corrupt.
const MAX_LOG_RECORD_BYTES: u32 = 128 * 1024;
/// Active durable log size that triggers rotation into an immutable
/// id-named segment.
const MAX_ACTIVE_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Cap on messages a durable room keeps resident; older pages are served
/// from disk.
const MAX_DURABLE_RESIDENT_MESSAGES: usize = 8192;
const STATE_FILE: &str = "state.toml";

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

pub struct DmRoom {
    pub room_id: RoomId,
    pub user_a: UserId,
    pub user_b: UserId,
    pub created_at_ms: u64,
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
        file: Option<File>,
        active_bytes: u64,
        path: Option<PathBuf>,
        active_first_id: Option<MessageId>,
        segments: Vec<Segment>,
    },
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
    chunks: Vec<HistoryChunkSpec>,
    pub message_count: usize,
    pub at_start: bool,
}

impl HistoryFetchPlan {
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

struct HistoryChunkSpec {
    start: usize,
    end: usize,
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

    fn recent(&self, max: usize) -> Vec<ChatMessage> {
        let entries = self.entries();
        let skip = entries.len().saturating_sub(max);
        self.decode_entries(&entries[skip..])
    }

    fn messages_before(
        &self,
        before: Option<MessageId>,
        limit: usize,
        has_older_on_disk: bool,
    ) -> (Vec<ChatMessage>, bool) {
        let entries = self.entries();
        let end = match before {
            Some(before) => entries.partition_point(|entry| entry.message_id < before),
            None => entries.len(),
        };
        let start = end.saturating_sub(limit);
        let at_start = start == 0 && !has_older_on_disk;
        (self.decode_entries(&entries[start..end]), at_start)
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
        let mut chunks = self.chunk_specs(selected, at_start, target_bytes);
        for chunk in &mut chunks {
            chunk.start += start;
            chunk.end += start;
        }
        HistoryFetchPlan {
            room_id,
            chunks,
            message_count: selected.len(),
            at_start,
        }
    }

    fn decode_entries(&self, entries: &[HistoryEntry]) -> Vec<ChatMessage> {
        entries
            .iter()
            .map(|entry| {
                history::decode_message(self.record(*entry))
                    .expect("resident history record should remain valid")
            })
            .collect()
    }

    fn chunk_specs(
        &self,
        entries: &[HistoryEntry],
        at_start: bool,
        target_bytes: usize,
    ) -> Vec<HistoryChunkSpec> {
        if entries.is_empty() {
            return vec![HistoryChunkSpec {
                start: 0,
                end: 0,
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
            let would_exceed_count = count == u16::MAX as usize;
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
                start,
                end,
                records_bytes,
                at_start: complete && at_start,
                complete,
            });
        }
        chunks
    }

    fn encode_chunk(&self, room_id: RoomId, spec: &HistoryChunkSpec) -> Result<Vec<u8>, String> {
        let entries = self.entries();
        let Some(chunk_entries) = entries.get(spec.start..spec.end) else {
            return Err("history fetch plan no longer matches retained history".to_string());
        };
        let mut payload = Vec::with_capacity(history::CHUNK_HEADER_BYTES + spec.records_bytes);
        history::write_chunk_header(
            room_id,
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
        at_start: bool,
        target_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, String> {
        let specs = self.chunk_specs(self.entries(), at_start, target_bytes);
        let mut payloads = Vec::with_capacity(specs.len());
        for spec in &specs {
            payloads.push(self.encode_chunk(room_id, spec)?);
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
    tuning: StoreTuning,
    next_room_id: u32,
    next_ids: HashMap<RoomId, u64>,
    watermarks: HashMap<RoomId, u64>,
    heads: HashMap<RoomId, MessageId>,
    dm_rooms: Vec<DmRoom>,
    rooms: HashMap<RoomId, Retention>,
}

impl RoomStore {
    /// Opens the store for the configured rooms: loads `state.toml`, fills
    /// each durable room's resident window from its active log (repairing a
    /// corrupt tail) and newest segments, and registers persisted DM rooms.
    /// `data_dir = None` keeps everything in memory, so ids restart per
    /// process; real servers always have a data dir.
    pub fn open(data_dir: Option<PathBuf>, rooms: &[RoomConfig]) -> Self {
        Self::open_with_tuning(data_dir, rooms, StoreTuning::default())
    }

    pub(crate) fn open_with_tuning(
        data_dir: Option<PathBuf>,
        rooms: &[RoomConfig],
        tuning: StoreTuning,
    ) -> Self {
        if let Some(dir) = &data_dir {
            if let Err(error) = fs::create_dir_all(dir.join("rooms")) {
                kvlog::error!(
                    "room store data dir unavailable; durable state disabled",
                    dir = dir.display().to_string().as_str(),
                    error = error.to_string().as_str()
                );
                return Self::open_with_tuning(None, rooms, tuning);
            }
        }
        let state = data_dir
            .as_deref()
            .map(load_state)
            .unwrap_or_else(StoreState::default);
        let mut next_room_id = state.next_room_id.max(FIRST_DYNAMIC_ROOM_ID);
        for entry in &state.dm_rooms {
            next_room_id = next_room_id.max(entry.room_id.saturating_add(1));
        }
        if let Some(dir) = &data_dir {
            next_room_id = next_room_id.max(next_room_id_above_existing_logs(dir));
        }
        let mut store = Self {
            data_dir,
            tuning,
            next_room_id,
            next_ids: HashMap::new(),
            watermarks: HashMap::new(),
            heads: HashMap::new(),
            dm_rooms: Vec::new(),
            rooms: HashMap::new(),
        };
        for entry in state.message_id_watermarks {
            store.watermarks.insert(RoomId(entry.room_id), entry.next);
            store.next_ids.insert(RoomId(entry.room_id), entry.next);
        }
        for room in rooms {
            store.register_room(
                room.room_id(),
                room.persistence,
                room.memory_history_limit(),
            );
        }
        for entry in state.dm_rooms {
            let room_id = RoomId(entry.room_id);
            store.register_room(room_id, RoomPersistenceConfig::Durable, 0);
            store.dm_rooms.push(DmRoom {
                room_id,
                user_a: entry.user_a,
                user_b: entry.user_b,
                created_at_ms: entry.created_at_ms,
            });
        }
        store
    }

    /// Test-only override of the durable retention limits; both caps are read
    /// at append time, so it takes effect for subsequent appends.
    #[cfg(test)]
    pub(crate) fn set_tuning(&mut self, tuning: StoreTuning) {
        self.tuning = tuning;
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
        if let Retention::Durable { history, .. } = &retention
            && let Some(last) = history.last()
        {
            let next = self.next_ids.entry(room_id).or_insert(1);
            *next = (*next).max(last.message_id.0 + 1);
            self.heads.insert(room_id, last.message_id);
        }
        self.rooms.insert(room_id, retention);
    }

    fn open_durable(&self, room_id: RoomId) -> Retention {
        let Some(dir) = &self.data_dir else {
            return Retention::Durable {
                history: ResidentHistory::default(),
                file: None,
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
        let file = open_active_log(&path)
            .map_err(|error| {
                kvlog::error!(
                    "durable room log open failed; retention degraded to memory",
                    room_id = room_id.0,
                    error = error.to_string().as_str()
                );
            })
            .ok();
        Retention::Durable {
            history,
            file,
            active_bytes,
            path: Some(path),
            active_first_id,
            segments,
        }
    }

    /// Hands out the next message id for the room, persisting a new watermark
    /// block when the reservation is exhausted.
    pub fn allocate_message_id(&mut self, room_id: RoomId) -> MessageId {
        let next = self.next_ids.entry(room_id).or_insert(1);
        let id = *next;
        *next += 1;
        let watermark = self.watermarks.entry(room_id).or_insert(0);
        if id >= *watermark {
            *watermark = (id + 1).next_multiple_of(MESSAGE_ID_RESERVE);
            self.persist_state();
        }
        MessageId(id)
    }

    /// Records the message per the room's retention and returns how many
    /// messages the room now retains.
    pub fn append(&mut self, room_id: RoomId, message: &ChatMessage) -> usize {
        let StoreTuning {
            max_active_log_bytes,
            max_resident_messages,
        } = self.tuning;
        let Some(retention) = self.rooms.get_mut(&room_id) else {
            return 0;
        };
        self.heads.insert(room_id, message.message_id);
        match retention {
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
                if let Some(open) = file {
                    let record = history.record_range(start, len);
                    match write_log_record(open, record) {
                        Ok(written) => {
                            if *active_bytes == 0 {
                                *active_first_id = Some(message.message_id);
                            }
                            *active_bytes += written as u64;
                        }
                        Err(error) => {
                            kvlog::error!(
                                "durable room log append failed; retention degraded to memory",
                                room_id = room_id.0,
                                error = error.to_string().as_str()
                            );
                            *file = None;
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
        }
    }

    /// The most recent `max` retained messages, oldest first.
    pub fn recent(&self, room_id: RoomId, max: usize) -> Vec<ChatMessage> {
        self.resident(room_id)
            .map(|history| history.recent(max))
            .unwrap_or_default()
    }

    /// Latest message id assigned in the room, `None` before the first message.
    pub fn head(&self, room_id: RoomId) -> Option<MessageId> {
        self.heads.get(&room_id).copied()
    }

    /// Resident messages strictly before `before` (or the newest when
    /// `None`), oldest first, at most `limit`. The bool is true when the
    /// returned slice reaches the oldest message retained anywhere, including
    /// on disk.
    pub fn messages_before(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: usize,
    ) -> (Vec<ChatMessage>, bool) {
        let Some(retention) = self.rooms.get(&room_id) else {
            return (Vec::new(), true);
        };
        let has_older = retention.has_unresident_history();
        match retention.history().filter(|history| !history.is_empty()) {
            Some(history) => history.messages_before(before, limit, has_older),
            None => (Vec::new(), !has_older),
        }
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
        match resident {
            Some(history) => {
                history.history_chunks_before(room_id, before, limit, target_bytes, has_older)
            }
            None => HistoryFetchPlan {
                room_id,
                chunks: vec![HistoryChunkSpec {
                    start: 0,
                    end: 0,
                    records_bytes: 0,
                    at_start: !has_older,
                    complete: true,
                }],
                message_count: 0,
                at_start: !has_older,
            },
        }
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
                let clone = file.as_ref().and_then(|open| {
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
        if spec.start == spec.end {
            let mut payload = Vec::with_capacity(history::CHUNK_HEADER_BYTES);
            history::write_chunk_header(
                plan.room_id,
                spec.at_start,
                spec.complete,
                0,
                &mut payload,
            )?;
            return Ok(payload);
        }
        self.resident(plan.room_id)
            .ok_or_else(|| "history fetch plan no longer has retained messages".to_string())?
            .encode_chunk(plan.room_id, spec)
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
        if let Some(existing) = self.dm_room_for(first, second) {
            return Ok(existing);
        }
        let room_id = RoomId(self.next_room_id);
        let next = self
            .next_room_id
            .checked_add(1)
            .ok_or_else(|| "no dynamic room ids are available".to_string())?;
        self.next_room_id = next;
        self.register_room(room_id, RoomPersistenceConfig::Durable, 0);
        self.dm_rooms.push(DmRoom {
            room_id,
            user_a: first,
            user_b: second,
            created_at_ms: now_ms,
        });
        if let Err(error) = self.try_persist_state() {
            self.dm_rooms.pop();
            self.rooms.remove(&room_id);
            self.next_ids.remove(&room_id);
            self.watermarks.remove(&room_id);
            self.heads.remove(&room_id);
            self.next_room_id = room_id.0;
            return Err(format!("dm room registry write failed: {error}"));
        }
        Ok(room_id)
    }

    fn persist_state(&self) {
        if let Err(error) = self.try_persist_state() {
            kvlog::error!(
                "room state write failed; ids may repeat after restart",
                error = error.as_str()
            );
        }
    }

    fn try_persist_state(&self) -> Result<(), String> {
        let Some(dir) = &self.data_dir else {
            return Ok(());
        };
        let mut watermarks: Vec<(u32, u64)> = self
            .watermarks
            .iter()
            .map(|(room_id, next)| (room_id.0, *next))
            .collect();
        watermarks.sort_unstable();
        let mut out = String::new();
        out.push_str("# chatt server room state. Managed by the server; do not edit.\n\n");
        out.push_str(&format!("next-room-id = {}\n", self.next_room_id));
        for (room_id, next) in watermarks {
            out.push_str("\n[[message-id-watermarks]]\n");
            out.push_str(&format!("room-id = {room_id}\nnext = {next}\n"));
        }
        for dm in &self.dm_rooms {
            out.push_str("\n[[dm-rooms]]\n");
            out.push_str(&format!(
                "room-id = {}\nuser-a = {}\nuser-b = {}\ncreated-at-ms = {}\n",
                dm.room_id.0, dm.user_a.0, dm.user_b.0, dm.created_at_ms
            ));
        }
        atomic_write_toml(&dir.join(STATE_FILE), &out)
    }
}

/// The lowest dynamic room id above every room log already on disk. Guards id
/// allocation when `state.toml` is lost or corrupt: reusing an id whose log
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

#[derive(Default, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct StoreState {
    #[toml(default = FIRST_DYNAMIC_ROOM_ID)]
    next_room_id: u32,
    #[toml(default)]
    message_id_watermarks: Vec<WatermarkEntry>,
    #[toml(default)]
    dm_rooms: Vec<DmRoomEntry>,
}

#[derive(Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct WatermarkEntry {
    room_id: u32,
    next: u64,
}

#[derive(Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct DmRoomEntry {
    room_id: u32,
    user_a: UserId,
    user_b: UserId,
    created_at_ms: u64,
}

fn load_state(dir: &Path) -> StoreState {
    let path = dir.join(STATE_FILE);
    let Ok(content) = fs::read_to_string(&path) else {
        return StoreState::default();
    };
    let arena = toml_spanner::Arena::new();
    let Ok(mut doc) = toml_spanner::parse(&content, &arena) else {
        kvlog::error!(
            "room state unparseable; starting fresh",
            path = path.display().to_string().as_str()
        );
        return StoreState::default();
    };
    match doc.to() {
        Ok(state) => state,
        Err(_) => {
            kvlog::error!(
                "room state undeserializable; starting fresh",
                path = path.display().to_string().as_str()
            );
            StoreState::default()
        }
    }
}

fn write_log_record(file: &mut File, payload: &[u8]) -> std::io::Result<usize> {
    let len = u32::try_from(payload.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "history record exceeds log length field",
        )
    })?;
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
        if len > MAX_LOG_RECORD_BYTES {
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
    if offset < bytes.len() {
        match repair {
            LogRepair::Truncate => {
                kvlog::warn!(
                    "durable room log tail corrupt; truncating",
                    path = path.display().to_string().as_str(),
                    valid_bytes = offset,
                    total_bytes = bytes.len()
                );
                if let Ok(file) = OpenOptions::new().write(true).open(path) {
                    let _ = file.set_len(offset as u64);
                    let _ = file.sync_all();
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
    }
}

fn rotate_log(
    path: &Path,
    file: &mut Option<File>,
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
    match open_active_log(path) {
        Ok(fresh) => {
            *file = Some(fresh);
            *active_bytes = 0;
        }
        Err(error) => {
            kvlog::error!(
                "durable room log reopen after rotate failed; retention degraded to memory",
                path = path.display().to_string().as_str(),
                error = error.to_string().as_str()
            );
            *file = None;
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

    fn test_message(room_id: RoomId, id: u64) -> ChatMessage {
        ChatMessage {
            message_id: MessageId(id),
            room_id,
            sender: UserId(1),
            sender_name: "alice".to_string(),
            timestamp_ms: 1_000 + id,
            body: format!("message {id}"),
            file_transfer_id: None,
        }
    }

    fn durable_room(id: u32) -> RoomConfig {
        RoomConfig {
            id,
            name: format!("room-{id}"),
            members: None,
            persistence: RoomPersistenceConfig::Durable,
            memory_limit: None,
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
            is_default: false,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chatt-room-store-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn durable_ids_resume_after_reload() {
        let dir = temp_dir("resume");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        for _ in 0..3 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        let next = store.allocate_message_id(room);
        assert!(next.0 > 3, "id {} must not reuse 1..=3", next.0);
        assert_eq!(store.recent(room, 10).len(), 3);
        assert_eq!(store.head(room), Some(MessageId(3)));
        let _ = fs::remove_dir_all(&dir);
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
        let mut store = RoomStore::open(Some(durable_dir.clone()), &[durable_room(1)]);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(durable_dir.clone()), &[durable_room(1)]);
        assert_eq!(store.head(room), Some(MessageId(1)));
        drop(store);

        let memory_dir = temp_dir("memory-head");
        let mut store = RoomStore::open(Some(memory_dir.clone()), &[memory_room(1, 16)]);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(memory_dir.clone()), &[memory_room(1, 16)]);
        assert_eq!(store.head(room), None);

        let _ = fs::remove_dir_all(durable_dir);
        let _ = fs::remove_dir_all(memory_dir);
    }

    #[test]
    fn watermark_blocks_never_reuse_ids() {
        let dir = temp_dir("watermark");
        let rooms = [memory_room(1, 16)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        assert_eq!(store.allocate_message_id(room), MessageId(1));
        assert_eq!(store.allocate_message_id(room), MessageId(2));
        drop(store);

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        assert_eq!(
            store.allocate_message_id(room),
            MessageId(MESSAGE_ID_RESERVE)
        );
        let _ = fs::remove_dir_all(&dir);
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

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        let dm = store.open_dm(UserId(3), UserId(7), 1_234).unwrap();
        assert!(dm.0 >= FIRST_DYNAMIC_ROOM_ID);
        assert_eq!(store.open_dm(UserId(7), UserId(3), 9_999).unwrap(), dm);
        let id = store.allocate_message_id(dm);
        store.append(dm, &test_message(dm, id.0));
        drop(store);

        let store = RoomStore::open(Some(dir.clone()), &rooms);
        assert_eq!(store.dm_room_for(UserId(3), UserId(7)), Some(dm));
        assert_eq!(store.dm_rooms().len(), 1);
        assert_eq!(store.recent(dm, 10).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dm_rooms_allocate_distinct_ids() {
        let dir = temp_dir("dm-distinct");
        let mut store = RoomStore::open(Some(dir.clone()), &[durable_room(1)]);

        let first = store.open_dm(UserId(1), UserId(2), 0).unwrap();
        let second = store.open_dm(UserId(1), UserId(3), 0).unwrap();
        assert_ne!(first, second);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dm_room_id_never_reused_after_state_loss() {
        let dir = temp_dir("dm-state-loss");
        let mut store = RoomStore::open(Some(dir.clone()), &[]);
        let dm = store.open_dm(UserId(1), UserId(2), 1_000).unwrap();
        let id = store.allocate_message_id(dm);
        store.append(dm, &test_message(dm, id.0));
        drop(store);

        fs::remove_file(dir.join(STATE_FILE)).unwrap();
        let mut store = RoomStore::open(Some(dir.clone()), &[]);
        let fresh = store.open_dm(UserId(3), UserId(4), 2_000).unwrap();
        assert_ne!(fresh, dm, "id with a surviving log was handed out again");
        assert!(store.recent(fresh, 10).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dm_room_id_floors_over_backup_logs() {
        let dir = temp_dir("dm-backup-floor");
        let mut store = RoomStore::open(Some(dir.clone()), &[]);
        let dm = store.open_dm(UserId(1), UserId(2), 1_000).unwrap();
        let id = store.allocate_message_id(dm);
        store.append(dm, &test_message(dm, id.0));
        drop(store);

        fs::remove_file(dir.join(STATE_FILE)).unwrap();
        let log = dir.join("rooms").join(format!("{}.log", dm.0));
        fs::rename(&log, segment_path(&log, MessageId(1))).unwrap();
        let mut store = RoomStore::open(Some(dir.clone()), &[]);
        let fresh = store.open_dm(UserId(3), UserId(4), 2_000).unwrap();
        assert!(fresh.0 > dm.0, "backup log {} vs fresh {}", dm.0, fresh.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_dm_rolls_back_when_state_persist_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("dm-rollback");
        let mut store = RoomStore::open(Some(dir.clone()), &[]);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let result = store.open_dm(UserId(1), UserId(2), 1_000);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "unpersistable dm creation must fail");
        assert_eq!(store.dm_room_for(UserId(1), UserId(2)), None);

        let retry = store.open_dm(UserId(1), UserId(2), 1_000).unwrap();
        assert_eq!(store.dm_room_for(UserId(1), UserId(2)), Some(retry));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_log_tail_is_truncated() {
        let dir = temp_dir("corrupt");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
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

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        let recent = store.recent(room, 10);
        assert_eq!(recent.len(), 2);
        let id = store.allocate_message_id(room);
        store.append(room, &test_message(room, id.0));
        drop(store);

        let store = RoomStore::open(Some(dir.clone()), &rooms);
        assert_eq!(store.recent(room, 10).len(), 3);
        let _ = fs::remove_dir_all(&dir);
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

        let mut store = RoomStore::open_with_tuning(Some(dir.clone()), &rooms, tuning);
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
            Some(dir.clone()),
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
        let _ = fs::remove_dir_all(&dir);
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

        let mut store = RoomStore::open_with_tuning(Some(dir.clone()), &rooms, tuning);
        for _ in 0..30 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let store = RoomStore::open_with_tuning(Some(dir.clone()), &rooms, tuning);
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
        let _ = fs::remove_dir_all(&dir);
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

        let mut store = RoomStore::open_with_tuning(Some(dir.clone()), &rooms, tuning);
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
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_fetch_request_is_none_at_true_start() {
        let dir = temp_dir("disk-fetch-none");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
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
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_backup_log_loads_as_segment() {
        let dir = temp_dir("legacy-backup");
        let rooms = [durable_room(1)];
        let room = RoomId(1);

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        for _ in 0..3 {
            let id = store.allocate_message_id(room);
            store.append(room, &test_message(room, id.0));
        }
        drop(store);

        let log = dir.join("rooms").join("1.log");
        fs::rename(&log, segment_path(&log, MessageId(1))).unwrap();

        let mut store = RoomStore::open(Some(dir.clone()), &rooms);
        assert_eq!(store.recent(room, 10).len(), 3);
        let id = store.allocate_message_id(room);
        assert!(id.0 > 3, "ids must resume above segment records");
        let _ = fs::remove_dir_all(&dir);
    }
}
