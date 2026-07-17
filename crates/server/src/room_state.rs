//! Transactional storage for server-owned room recovery state.
//!
//! Room message logs remain append-only files, but their identity is owned by
//! this database. Creating a dynamic room atomically reserves its id and
//! records its participants; an empty log is derived state that can be
//! recreated after a crash.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::{
    config::FIRST_DYNAMIC_ROOM_ID,
    event_queue::EventQueue,
    room_store::DmRoom,
};
use rpc::ids::{RoomId, UserId};

pub(super) const FILE_NAME: &str = "state.redb";

const SCHEMA_VERSION: u64 = 1;
const SCHEMA_VERSION_KEY: &str = "schema_version";
const NEXT_ROOM_ID_KEY: &str = "next_room_id";
const DM_ROOM_BYTES: usize = 24;

const METADATA: TableDefinition<&str, u64> = TableDefinition::new("metadata");
const MESSAGE_ID_WATERMARKS: TableDefinition<u32, u64> =
    TableDefinition::new("message_id_watermarks");
const DM_ROOMS: TableDefinition<u32, &[u8]> = TableDefinition::new("dm_rooms");

pub(super) struct LoadedState {
    pub(super) next_room_id: u32,
    pub(super) watermarks: Vec<(RoomId, u64)>,
    pub(super) dm_rooms: Vec<DmRoom>,
}

impl Default for LoadedState {
    fn default() -> Self {
        Self {
            next_room_id: FIRST_DYNAMIC_ROOM_ID,
            watermarks: Vec::new(),
            dm_rooms: Vec::new(),
        }
    }
}

pub(super) fn open(path: &Path) -> Result<(LoadedState, StateWriter), String> {
    let database = Database::create(path).map_err(|error| error.to_string())?;
    initialize_schema(&database)?;
    let state = load(&database)?;
    Ok((state, StateWriter::spawn(database)))
}

fn initialize_schema(database: &Database) -> Result<(), String> {
    let transaction = database.begin_write().map_err(|error| error.to_string())?;
    {
        let mut metadata = transaction
            .open_table(METADATA)
            .map_err(|error| error.to_string())?;
        let stored_schema_version = metadata
            .get(SCHEMA_VERSION_KEY)
            .map_err(|error| error.to_string())?
            .map(|value| value.value());
        match stored_schema_version {
            Some(SCHEMA_VERSION) => {}
            Some(found) => {
                return Err(format!(
                    "unsupported room-state schema version {found}; supported version is {SCHEMA_VERSION}"
                ));
            }
            None => {
                metadata
                    .insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)
                    .map_err(|error| error.to_string())?;
                metadata
                    .insert(NEXT_ROOM_ID_KEY, u64::from(FIRST_DYNAMIC_ROOM_ID))
                    .map_err(|error| error.to_string())?;
            }
        }
        transaction
            .open_table(MESSAGE_ID_WATERMARKS)
            .map_err(|error| error.to_string())?;
        transaction
            .open_table(DM_ROOMS)
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn load(database: &Database) -> Result<LoadedState, String> {
    let transaction = database.begin_read().map_err(|error| error.to_string())?;
    let metadata = transaction
        .open_table(METADATA)
        .map_err(|error| error.to_string())?;
    let raw_next_room_id = metadata
        .get(NEXT_ROOM_ID_KEY)
        .map_err(|error| error.to_string())?
        .map_or(u64::from(FIRST_DYNAMIC_ROOM_ID), |value| value.value());
    let next_room_id = u32::try_from(raw_next_room_id)
        .map_err(|_| format!("stored next room id {raw_next_room_id} exceeds u32::MAX"))?;

    let watermark_table = transaction
        .open_table(MESSAGE_ID_WATERMARKS)
        .map_err(|error| error.to_string())?;
    let mut watermarks = Vec::new();
    for entry in watermark_table.iter().map_err(|error| error.to_string())? {
        let (room_id, next) = entry.map_err(|error| error.to_string())?;
        watermarks.push((RoomId(room_id.value()), next.value()));
    }

    let dm_table = transaction
        .open_table(DM_ROOMS)
        .map_err(|error| error.to_string())?;
    let mut dm_rooms = Vec::new();
    for entry in dm_table.iter().map_err(|error| error.to_string())? {
        let (room_id, value) = entry.map_err(|error| error.to_string())?;
        dm_rooms.push(decode_dm(RoomId(room_id.value()), value.value())?);
    }
    Ok(LoadedState {
        next_room_id,
        watermarks,
        dm_rooms,
    })
}

fn encode_dm(room: DmRoom) -> [u8; DM_ROOM_BYTES] {
    let mut bytes = [0; DM_ROOM_BYTES];
    bytes[..8].copy_from_slice(&room.user_a.0.to_le_bytes());
    bytes[8..16].copy_from_slice(&room.user_b.0.to_le_bytes());
    bytes[16..].copy_from_slice(&room.created_at_ms.to_le_bytes());
    bytes
}

fn decode_dm(room_id: RoomId, bytes: &[u8]) -> Result<DmRoom, String> {
    let bytes: &[u8; DM_ROOM_BYTES] = bytes.try_into().map_err(|_| {
        format!(
            "room {} has a corrupt direct-message record length",
            room_id.0
        )
    })?;
    let user_a = UserId(u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")));
    let user_b = UserId(u64::from_le_bytes(
        bytes[8..16].try_into().expect("8 bytes"),
    ));
    if user_a == user_b {
        return Err(format!(
            "room {} has identical direct-message participants",
            room_id.0
        ));
    }
    Ok(DmRoom {
        room_id,
        user_a,
        user_b,
        created_at_ms: u64::from_le_bytes(bytes[16..].try_into().expect("8 bytes")),
    })
}

#[derive(Clone, Copy)]
struct DmInsert {
    room: DmRoom,
    next_room_id: u32,
}

#[derive(Default)]
struct PendingWrite {
    watermarks: BTreeMap<u32, u64>,
    dm_inserts: Vec<DmInsert>,
    waiters: Vec<StateWriteWaiter>,
    closed: bool,
}

impl PendingWrite {
    fn is_empty(&self) -> bool {
        self.watermarks.is_empty() && self.dm_inserts.is_empty()
    }
}

enum StateWriteWaiter {
    Sync(mpsc::SyncSender<Result<(), String>>),
    Event {
        operation_id: u64,
        events: Arc<EventQueue<StateWriteEvent>>,
    },
}

pub(super) struct StateWriteEvent {
    pub(super) operation_id: u64,
    pub(super) result: Result<(), String>,
}

#[derive(Default)]
struct StateWriteSubmission {
    pending: Mutex<PendingWrite>,
    ready: Condvar,
}

impl StateWriteSubmission {
    fn submit_watermark(&self, room_id: RoomId, next: u64) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if pending.closed {
            return false;
        }
        pending
            .watermarks
            .entry(room_id.0)
            .and_modify(|queued| *queued = (*queued).max(next))
            .or_insert(next);
        self.ready.notify_one();
        true
    }

    fn submit_dm(&self, insert: DmInsert, waiter: StateWriteWaiter) -> bool {
        let mut pending = self.pending.lock().unwrap();
        if pending.closed {
            return false;
        }
        pending.dm_inserts.push(insert);
        pending.waiters.push(waiter);
        self.ready.notify_one();
        true
    }

    fn receive(&self) -> Option<PendingWrite> {
        let mut pending = self.pending.lock().unwrap();
        loop {
            if !pending.is_empty() {
                let closed = pending.closed;
                let mut write = std::mem::take(&mut *pending);
                pending.closed = closed;
                write.closed = false;
                return Some(write);
            }
            if pending.closed {
                return None;
            }
            pending = self.ready.wait(pending).unwrap();
        }
    }

    fn close(&self) {
        self.pending.lock().unwrap().closed = true;
        self.ready.notify_one();
    }
}

/// Single ordered writer for all room-state transactions. Watermark advances
/// coalesce by room while DM insert waiters retain commit acknowledgement.
pub(super) struct StateWriter {
    submission: Arc<StateWriteSubmission>,
    thread: Option<thread::JoinHandle<()>>,
    #[cfg(test)]
    fail_next_write: Arc<std::sync::atomic::AtomicBool>,
}

impl StateWriter {
    fn spawn(database: Database) -> Self {
        let submission = Arc::new(StateWriteSubmission::default());
        let worker_submission = Arc::clone(&submission);
        #[cfg(test)]
        let fail_next_write = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(test)]
        let worker_fail_next_write = Arc::clone(&fail_next_write);
        let thread = thread::Builder::new()
            .name("chatt-room-state-writer".to_string())
            .spawn(move || {
                while let Some(write) = worker_submission.receive() {
                    #[cfg(test)]
                    let forced_failure = worker_fail_next_write
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    #[cfg(not(test))]
                    let forced_failure = false;
                    let result = if forced_failure {
                        Err("forced room-state write failure".to_string())
                    } else {
                        persist(&database, &write)
                    };
                    if let Err(error) = &result {
                        kvlog::error!(
                            "room state transaction failed; ids may repeat after restart",
                            error = error.as_str()
                        );
                    }
                    for waiter in write.waiters {
                        match waiter {
                            StateWriteWaiter::Sync(reply) => {
                                let _ = reply.send(result.clone());
                            }
                            StateWriteWaiter::Event {
                                operation_id,
                                events,
                            } => events.push(StateWriteEvent {
                                operation_id,
                                result: result.clone(),
                            }),
                        }
                    }
                }
            })
            .expect("failed to spawn room state writer");
        Self {
            submission,
            thread: Some(thread),
            #[cfg(test)]
            fail_next_write,
        }
    }

    pub(super) fn enqueue_watermark(&self, room_id: RoomId, next: u64) {
        if !self.submission.submit_watermark(room_id, next) {
            kvlog::error!("room state writer gone; ids may repeat after restart");
        }
    }

    pub(super) fn insert_dm_sync(&self, room: DmRoom, next_room_id: u32) -> Result<(), String> {
        let gone = || "room state writer thread is gone".to_string();
        let (reply, done) = mpsc::sync_channel(1);
        if !self.submission.submit_dm(
            DmInsert { room, next_room_id },
            StateWriteWaiter::Sync(reply),
        ) {
            return Err(gone());
        }
        done.recv().map_err(|_| gone())?
    }

    pub(super) fn insert_dm_async(
        &self,
        room: DmRoom,
        next_room_id: u32,
        operation_id: u64,
        events: Arc<EventQueue<StateWriteEvent>>,
    ) -> Result<(), String> {
        self.submission
            .submit_dm(
                DmInsert { room, next_room_id },
                StateWriteWaiter::Event {
                    operation_id,
                    events,
                },
            )
            .then_some(())
            .ok_or_else(|| "room state writer thread is gone".to_string())
    }

    #[cfg(test)]
    pub(super) fn fail_next_write(&self) {
        self.fail_next_write
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for StateWriter {
    fn drop(&mut self) {
        self.submission.close();
        let Some(thread) = self.thread.take() else {
            return;
        };
        let _ = thread.join();
    }
}

fn persist(database: &Database, write: &PendingWrite) -> Result<(), String> {
    let transaction = database.begin_write().map_err(|error| error.to_string())?;
    {
        let mut watermarks = transaction
            .open_table(MESSAGE_ID_WATERMARKS)
            .map_err(|error| error.to_string())?;
        for (&room_id, &next) in &write.watermarks {
            let stored = watermarks
                .get(room_id)
                .map_err(|error| error.to_string())?
                .map_or(0, |value| value.value());
            if next > stored {
                watermarks
                    .insert(room_id, next)
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    if !write.dm_inserts.is_empty() {
        let mut metadata = transaction
            .open_table(METADATA)
            .map_err(|error| error.to_string())?;
        let mut rooms = transaction
            .open_table(DM_ROOMS)
            .map_err(|error| error.to_string())?;
        for insert in &write.dm_inserts {
            if rooms
                .get(insert.room.room_id.0)
                .map_err(|error| error.to_string())?
                .is_some()
            {
                return Err(format!(
                    "direct-message room {} already exists",
                    insert.room.room_id.0
                ));
            }
            let encoded = encode_dm(insert.room);
            rooms
                .insert(insert.room.room_id.0, encoded.as_slice())
                .map_err(|error| error.to_string())?;
            metadata
                .insert(NEXT_ROOM_ID_KEY, u64::from(insert.next_room_id))
                .map_err(|error| error.to_string())?;
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_watermarks_coalesce_to_the_highest_value_per_room() {
        let submission = StateWriteSubmission::default();
        assert!(submission.submit_watermark(RoomId(3), 2048));
        assert!(submission.submit_watermark(RoomId(3), 1024));
        assert!(submission.submit_watermark(RoomId(3), 4096));
        assert!(submission.submit_watermark(RoomId(7), 8192));

        let write = submission.receive().unwrap();
        assert_eq!(write.watermarks.get(&3), Some(&4096));
        assert_eq!(write.watermarks.get(&7), Some(&8192));
    }

    #[test]
    fn database_round_trips_transactional_room_state() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        drop(file);
        let (state, writer) = open(&path).unwrap();
        assert_eq!(state.next_room_id, FIRST_DYNAMIC_ROOM_ID);
        let room = DmRoom {
            room_id: RoomId(FIRST_DYNAMIC_ROOM_ID),
            user_a: UserId(7),
            user_b: UserId(11),
            created_at_ms: 1234,
        };
        writer.enqueue_watermark(RoomId(1), 2048);
        writer
            .insert_dm_sync(room, FIRST_DYNAMIC_ROOM_ID + 1)
            .unwrap();
        drop(writer);

        let (state, writer) = open(&path).unwrap();
        assert_eq!(state.next_room_id, FIRST_DYNAMIC_ROOM_ID + 1);
        assert_eq!(state.watermarks, vec![(RoomId(1), 2048)]);
        assert_eq!(state.dm_rooms.len(), 1);
        assert_eq!(state.dm_rooms[0].room_id, room.room_id);
        assert_eq!(state.dm_rooms[0].user_a, room.user_a);
        assert_eq!(state.dm_rooms[0].user_b, room.user_b);
        assert_eq!(state.dm_rooms[0].created_at_ms, room.created_at_ms);
        drop(writer);
    }
}
