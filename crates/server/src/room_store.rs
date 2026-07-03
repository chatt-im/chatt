//! Server-side room message retention and durable room state.
//!
//! One [`RoomStore`] owns, per room, the retention backend selected by the
//! room's persistence config: nothing, a bounded in-memory ring, or an
//! append-only on-disk log. It also owns the durable server state that must
//! survive restarts: per-room message-id watermarks (so ids are never reused)
//! and the registry of runtime-created DM rooms.
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
    ids::{MessageId, RoomId, UserId},
};
use toml_spanner::Toml;

use crate::config::{FIRST_DYNAMIC_ROOM_ID, RoomConfig, RoomPersistenceConfig, atomic_write_toml};

/// Message-id block reserved per state write; a restart skips at most this
/// many ids per room.
pub const MESSAGE_ID_RESERVE: u64 = 1024;
/// Upper bound on one serialized log record; larger frames mark the log tail
/// corrupt.
const MAX_LOG_RECORD_BYTES: u32 = 128 * 1024;
/// Active durable log size that triggers rotation to the single `.1` backup.
const MAX_ACTIVE_LOG_BYTES: u64 = 4 * 1024 * 1024;
/// Cap on messages a durable room keeps resident for history fetches.
const MAX_DURABLE_RESIDENT_MESSAGES: usize = 8192;
const STATE_FILE: &str = "state.toml";

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
        messages: Vec<ChatMessage>,
        limit: usize,
    },
    Durable {
        messages: Vec<ChatMessage>,
        file: Option<File>,
        active_bytes: u64,
        path: Option<PathBuf>,
    },
}

impl Retention {
    fn messages(&self) -> &[ChatMessage] {
        match self {
            Retention::None => &[],
            Retention::Memory { messages, .. } => messages,
            Retention::Durable { messages, .. } => messages,
        }
    }
}

pub struct RoomStore {
    data_dir: Option<PathBuf>,
    next_room_id: u32,
    next_ids: HashMap<RoomId, u64>,
    watermarks: HashMap<RoomId, u64>,
    heads: HashMap<RoomId, MessageId>,
    dm_rooms: Vec<DmRoom>,
    rooms: HashMap<RoomId, Retention>,
}

impl RoomStore {
    /// Opens the store for the configured rooms: loads `state.toml` and every
    /// durable room's log (repairing a corrupt tail), and registers persisted
    /// DM rooms. `data_dir = None` keeps everything in memory, so ids restart
    /// per process; real servers always have a data dir.
    pub fn open(data_dir: Option<PathBuf>, rooms: &[RoomConfig]) -> Self {
        if let Some(dir) = &data_dir {
            if let Err(error) = fs::create_dir_all(dir.join("rooms")) {
                kvlog::error!(
                    "room store data dir unavailable; durable state disabled",
                    dir = dir.display().to_string().as_str(),
                    error = error.to_string().as_str()
                );
                return Self::open(None, rooms);
            }
        }
        let state = data_dir
            .as_deref()
            .map(load_state)
            .unwrap_or_else(StoreState::default);
        let mut store = Self {
            data_dir,
            next_room_id: state.next_room_id.max(FIRST_DYNAMIC_ROOM_ID),
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

    fn register_room(
        &mut self,
        room_id: RoomId,
        persistence: RoomPersistenceConfig,
        memory_limit: usize,
    ) {
        let retention = match persistence {
            RoomPersistenceConfig::None => Retention::None,
            RoomPersistenceConfig::Memory => Retention::Memory {
                messages: Vec::new(),
                limit: memory_limit,
            },
            RoomPersistenceConfig::Durable => self.open_durable(room_id),
        };
        if let Retention::Durable { messages, .. } = &retention
            && let Some(last) = messages.last()
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
                messages: Vec::new(),
                file: None,
                active_bytes: 0,
                path: None,
            };
        };
        let path = dir.join("rooms").join(format!("{}.log", room_id.0));
        let mut messages = load_log(&backup_path(&path), LogRepair::ReadOnly);
        let active = load_log_repairing(&path);
        messages.extend(active.messages);
        trim_resident(&mut messages);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| {
                kvlog::error!(
                    "durable room log open failed; retention degraded to memory",
                    room_id = room_id.0,
                    error = error.to_string().as_str()
                );
            })
            .ok();
        Retention::Durable {
            messages,
            file,
            active_bytes: active.valid_bytes,
            path: Some(path),
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
        let Some(retention) = self.rooms.get_mut(&room_id) else {
            return 0;
        };
        self.heads.insert(room_id, message.message_id);
        match retention {
            Retention::None => 0,
            Retention::Memory { messages, limit } => {
                messages.push(message.clone());
                if messages.len() > *limit {
                    let excess = messages.len() - *limit;
                    messages.drain(..excess);
                }
                messages.len()
            }
            Retention::Durable {
                messages,
                file,
                active_bytes,
                path,
            } => {
                if let Some(open) = file {
                    let record = encode_record(message);
                    match open.write_all(&record) {
                        Ok(()) => *active_bytes += record.len() as u64,
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
                if *active_bytes > MAX_ACTIVE_LOG_BYTES
                    && let Some(path) = path
                {
                    rotate_log(path, file, active_bytes);
                }
                messages.push(message.clone());
                trim_resident(messages);
                messages.len()
            }
        }
    }

    /// The most recent `max` retained messages, oldest first.
    pub fn recent(&self, room_id: RoomId, max: usize) -> Vec<ChatMessage> {
        let retained = self.retained(room_id);
        let skip = retained.len().saturating_sub(max);
        retained[skip..].to_vec()
    }

    /// Latest message id assigned in the room, `None` before the first message.
    pub fn head(&self, room_id: RoomId) -> Option<MessageId> {
        self.heads.get(&room_id).copied()
    }

    /// Retained messages strictly before `before` (or the newest when `None`),
    /// oldest first, at most `limit`. The bool is true when the returned slice
    /// reaches the oldest retained message.
    pub fn messages_before(
        &self,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: usize,
    ) -> (Vec<ChatMessage>, bool) {
        let retained = self.retained(room_id);
        let end = match before {
            Some(before) => retained.partition_point(|message| message.message_id < before),
            None => retained.len(),
        };
        let start = end.saturating_sub(limit);
        (retained[start..end].to_vec(), start == 0)
    }

    fn retained(&self, room_id: RoomId) -> &[ChatMessage] {
        self.rooms
            .get(&room_id)
            .map(Retention::messages)
            .unwrap_or(&[])
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

    /// Creates (or returns) the DM room for the unordered user pair, persisting
    /// the registry. DM rooms are always durable.
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
        self.persist_state();
        Ok(room_id)
    }

    fn persist_state(&self) {
        let Some(dir) = &self.data_dir else {
            return;
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
        if let Err(error) = atomic_write_toml(&dir.join(STATE_FILE), &out) {
            kvlog::error!(
                "room state write failed; ids may repeat after restart",
                error = error.as_str()
            );
        }
    }
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

fn encode_record(message: &ChatMessage) -> Vec<u8> {
    let payload = jsony::to_binary(message);
    let mut record = Vec::with_capacity(4 + payload.len());
    record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    record.extend_from_slice(&payload);
    record
}

enum LogRepair {
    /// Truncate the file to the last valid record boundary.
    Truncate,
    /// Keep the valid prefix in memory without touching the file.
    ReadOnly,
}

struct LoadedLog {
    messages: Vec<ChatMessage>,
    valid_bytes: u64,
}

fn load_log_repairing(path: &Path) -> LoadedLog {
    let messages = load_log(path, LogRepair::Truncate);
    let valid_bytes = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    LoadedLog {
        messages,
        valid_bytes,
    }
}

fn load_log(path: &Path, repair: LogRepair) -> Vec<ChatMessage> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    let mut messages = Vec::new();
    let mut offset = 0usize;
    while bytes.len() - offset >= 4 {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("4 bytes"));
        if len > MAX_LOG_RECORD_BYTES {
            break;
        }
        let len = len as usize;
        let Some(payload) = bytes.get(offset + 4..offset + 4 + len) else {
            break;
        };
        let Ok(message) = jsony::from_binary::<ChatMessage>(payload) else {
            break;
        };
        messages.push(message);
        offset += 4 + len;
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
    messages
}

fn rotate_log(path: &Path, file: &mut Option<File>, active_bytes: &mut u64) {
    let backup = backup_path(path);
    if let Err(error) = fs::rename(path, &backup) {
        kvlog::error!(
            "durable room log rotate failed",
            path = path.display().to_string().as_str(),
            error = error.to_string().as_str()
        );
        return;
    }
    match OpenOptions::new().create(true).append(true).open(path) {
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

fn trim_resident(messages: &mut Vec<ChatMessage>) {
    if messages.len() > MAX_DURABLE_RESIDENT_MESSAGES {
        let excess = messages.len() - MAX_DURABLE_RESIDENT_MESSAGES;
        messages.drain(..excess);
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut raw = path.as_os_str().to_owned();
    raw.push(".1");
    PathBuf::from(raw)
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
}
