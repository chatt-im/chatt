//! Transactional storage for server-owned room recovery state.
//!
//! Room message logs remain append-only files, but their identity is owned by
//! this database. Creating a dynamic room atomically reserves its id and
//! records its participants; an empty log is derived state that can be
//! recreated after a crash.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::{config::FIRST_DYNAMIC_ROOM_ID, event_queue::EventQueue, room_store::DmRoom};
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
    ensure_private_file(path)?;
    let database = Database::create(path).map_err(|error| error.to_string())?;
    initialize_schema(&database)?;
    let state = load(&database)?;
    Ok((state, StateWriter::spawn(database, path.to_path_buf())))
}

fn ensure_private_file(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = fs::metadata(path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        let mode = metadata.mode() & 0o777;
        let private_mode = mode & 0o700;
        if mode != private_mode {
            fs::set_permissions(path, fs::Permissions::from_mode(private_mode))
                .map_err(|error| format!("failed to secure {}: {error}", path.display()))?;
        }
    }
    Ok(())
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

pub(super) enum StateWriteEvent {
    Dm {
        operation_id: u64,
        result: Result<(), String>,
    },
    Fatal {
        error: String,
    },
}

#[derive(Default)]
struct StateWriteSubmission {
    pending: Mutex<PendingWrite>,
    ready: Condvar,
}

impl StateWriteSubmission {
    fn submit_watermark(&self, room_id: RoomId, next: u64) -> Result<(), String> {
        let mut pending = self.pending.lock().unwrap();
        if pending.closed {
            return Err("room state writer is closed".to_string());
        }
        pending
            .watermarks
            .entry(room_id.0)
            .and_modify(|queued| *queued = (*queued).max(next))
            .or_insert(next);
        self.ready.notify_one();
        Ok(())
    }

    fn submit_dm(&self, insert: DmInsert, waiter: StateWriteWaiter) -> Result<(), String> {
        let mut pending = self.pending.lock().unwrap();
        if pending.closed {
            return Err("room state writer is closed".to_string());
        }
        pending.dm_inserts.push(insert);
        pending.waiters.push(waiter);
        self.ready.notify_one();
        Ok(())
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

    fn terminate(&self) -> Vec<StateWriteWaiter> {
        let mut pending = self.pending.lock().unwrap();
        pending.closed = true;
        pending.watermarks.clear();
        pending.dm_inserts.clear();
        let waiters = std::mem::take(&mut pending.waiters);
        self.ready.notify_one();
        waiters
    }
}

/// Single ordered writer for all room-state transactions. Watermark advances
/// coalesce by room while DM insert waiters retain commit acknowledgement.
#[derive(Default)]
struct StateWriteNotifications {
    events: Option<Arc<EventQueue<StateWriteEvent>>>,
    terminal_error: Option<String>,
}

pub(super) struct StateWriter {
    submission: Arc<StateWriteSubmission>,
    notifications: Arc<Mutex<StateWriteNotifications>>,
    thread: Option<thread::JoinHandle<()>>,
    #[cfg(test)]
    fail_next_write: Arc<std::sync::atomic::AtomicBool>,
}

impl StateWriter {
    fn spawn(database: Database, path: PathBuf) -> Self {
        let submission = Arc::new(StateWriteSubmission::default());
        let worker_submission = Arc::clone(&submission);
        let notifications = Arc::new(Mutex::new(StateWriteNotifications::default()));
        let worker_notifications = Arc::clone(&notifications);
        #[cfg(test)]
        let fail_next_write = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(test)]
        let worker_fail_next_write = Arc::clone(&fail_next_write);
        let thread = thread::Builder::new()
            .name("chatt-room-state-writer".to_string())
            .spawn(move || {
                let mut database = Some(database);
                while let Some(write) = worker_submission.receive() {
                    #[cfg(test)]
                    let forced_failure = worker_fail_next_write
                        .swap(false, std::sync::atomic::Ordering::Relaxed);
                    #[cfg(not(test))]
                    let forced_failure = false;
                    let result = if forced_failure {
                        Err(PersistError::Conflict(
                            "forced room-state write failure".to_string(),
                        ))
                    } else {
                        persist(database.as_ref().expect("database present"), &write)
                    };
                    let result = match result {
                        Ok(()) => Ok(()),
                        Err(PersistError::Storage(first_error)) => {
                            drop(database.take());
                            recover_and_persist(&path, &write).map(|reopened| {
                                database = Some(reopened);
                            }).map_err(|recovery_error| {
                                format!(
                                    "{first_error}; room-state database recovery failed: {recovery_error}"
                                )
                            })
                        }
                        Err(PersistError::Conflict(error)) => Err(error),
                    };
                    let terminal_error = result.as_ref().err().cloned();
                    if let Some(error) = &terminal_error {
                        kvlog::error!(
                            "room state writer failed; server must stop before allocating more ids",
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
                            } => events.push(StateWriteEvent::Dm {
                                operation_id,
                                result: result.clone(),
                            }),
                        }
                    }
                    let Some(error) = terminal_error else {
                        continue;
                    };
                    let fatal_events = {
                        let mut notifications = worker_notifications.lock().unwrap();
                        notifications.terminal_error = Some(error.clone());
                        notifications.events.clone()
                    };
                    for waiter in worker_submission.terminate() {
                        match waiter {
                            StateWriteWaiter::Sync(reply) => {
                                let _ = reply.send(Err(error.clone()));
                            }
                            StateWriteWaiter::Event {
                                operation_id,
                                events,
                            } => events.push(StateWriteEvent::Dm {
                                operation_id,
                                result: Err(error.clone()),
                            }),
                        }
                    }
                    if let Some(events) = fatal_events {
                        events.push(StateWriteEvent::Fatal { error });
                    }
                    break;
                }
            })
            .expect("failed to spawn room state writer");
        Self {
            submission,
            notifications,
            thread: Some(thread),
            #[cfg(test)]
            fail_next_write,
        }
    }

    pub(super) fn set_events(&self, events: Arc<EventQueue<StateWriteEvent>>) {
        let terminal_error = {
            let mut notifications = self.notifications.lock().unwrap();
            notifications.events = Some(Arc::clone(&events));
            notifications.terminal_error.clone()
        };
        if let Some(error) = terminal_error {
            events.push(StateWriteEvent::Fatal { error });
        }
    }

    pub(super) fn enqueue_watermark(&self, room_id: RoomId, next: u64) -> Result<(), String> {
        self.submission.submit_watermark(room_id, next)
    }

    pub(super) fn insert_dm_sync(&self, room: DmRoom, next_room_id: u32) -> Result<(), String> {
        let gone = || "room state writer thread is gone".to_string();
        let (reply, done) = mpsc::sync_channel(1);
        self.submission
            .submit_dm(
                DmInsert { room, next_room_id },
                StateWriteWaiter::Sync(reply),
            )
            .map_err(|_| gone())?;
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
            .map_err(|_| "room state writer thread is gone".to_string())
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

#[derive(Debug)]
enum PersistError {
    Storage(String),
    Conflict(String),
}

fn storage_error(error: impl std::fmt::Display) -> PersistError {
    PersistError::Storage(error.to_string())
}

fn recover_and_persist(path: &Path, write: &PendingWrite) -> Result<Database, String> {
    ensure_private_file(path)?;
    let database = Database::create(path).map_err(|error| error.to_string())?;
    initialize_schema(&database)?;
    persist(&database, write).map_err(|error| match error {
        PersistError::Storage(error) | PersistError::Conflict(error) => error,
    })?;
    Ok(database)
}

fn persist(database: &Database, write: &PendingWrite) -> Result<(), PersistError> {
    let transaction = database.begin_write().map_err(storage_error)?;
    {
        let mut watermarks = transaction
            .open_table(MESSAGE_ID_WATERMARKS)
            .map_err(storage_error)?;
        for (&room_id, &next) in &write.watermarks {
            let stored = watermarks
                .get(room_id)
                .map_err(storage_error)?
                .map_or(0, |value| value.value());
            if next > stored {
                watermarks.insert(room_id, next).map_err(storage_error)?;
            }
        }
    }
    if !write.dm_inserts.is_empty() {
        let mut metadata = transaction.open_table(METADATA).map_err(storage_error)?;
        let mut rooms = transaction.open_table(DM_ROOMS).map_err(storage_error)?;
        for insert in &write.dm_inserts {
            let existing = rooms
                .get(insert.room.room_id.0)
                .map_err(storage_error)?
                .map(|existing| decode_dm(insert.room.room_id, existing.value()))
                .transpose()
                .map_err(PersistError::Conflict)?;
            if let Some(existing) = existing {
                if existing != insert.room {
                    return Err(PersistError::Conflict(format!(
                        "direct-message room {} already exists with different participants",
                        insert.room.room_id.0
                    )));
                }
            } else {
                let encoded = encode_dm(insert.room);
                rooms
                    .insert(insert.room.room_id.0, encoded.as_slice())
                    .map_err(storage_error)?;
            }
            let stored_next = metadata
                .get(NEXT_ROOM_ID_KEY)
                .map_err(storage_error)?
                .map_or(u64::from(FIRST_DYNAMIC_ROOM_ID), |value| value.value());
            let next = stored_next.max(u64::from(insert.next_room_id));
            metadata
                .insert(NEXT_ROOM_ID_KEY, next)
                .map_err(storage_error)?;
        }
    }
    transaction.commit().map_err(storage_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_watermarks_coalesce_to_the_highest_value_per_room() {
        let submission = StateWriteSubmission::default();
        submission.submit_watermark(RoomId(3), 2048).unwrap();
        submission.submit_watermark(RoomId(3), 1024).unwrap();
        submission.submit_watermark(RoomId(3), 4096).unwrap();
        submission.submit_watermark(RoomId(7), 8192).unwrap();

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
        writer.enqueue_watermark(RoomId(1), 2048).unwrap();
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

    #[test]
    fn replaying_an_ambiguous_batch_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        ensure_private_file(&path).unwrap();
        let database = Database::create(&path).unwrap();
        initialize_schema(&database).unwrap();
        let room = DmRoom {
            room_id: RoomId(FIRST_DYNAMIC_ROOM_ID),
            user_a: UserId(7),
            user_b: UserId(11),
            created_at_ms: 1234,
        };
        let mut write = PendingWrite::default();
        write.watermarks.insert(1, 2048);
        write.dm_inserts.push(DmInsert {
            room,
            next_room_id: FIRST_DYNAMIC_ROOM_ID + 1,
        });

        persist(&database, &write).unwrap();
        drop(database);
        let database = recover_and_persist(&path, &write).unwrap();
        let state = load(&database).unwrap();

        assert_eq!(state.watermarks, vec![(RoomId(1), 2048)]);
        assert_eq!(state.next_room_id, FIRST_DYNAMIC_ROOM_ID + 1);
        assert_eq!(state.dm_rooms.len(), 1);
        assert_eq!(state.dm_rooms[0], room);
    }

    #[cfg(unix)]
    #[test]
    fn database_file_is_owner_only_on_create_and_reopen() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let (_, writer) = open(&path).unwrap();
        drop(writer);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        let (_, writer) = open(&path).unwrap();
        drop(writer);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
