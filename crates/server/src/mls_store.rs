//! In-memory MLS delivery storage backed by an append-only write-ahead log.
//!
//! The delivery worker validates a mutation, appends and syncs its compact
//! state delta, and only then publishes the delta to [`MemoryState`]. Periodic
//! snapshots bound replay time and discard payload bytes made dead by
//! retention cleanup. Delivery readers only touch the shared hot state.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use chatt_mls::PublicGroupState;
use jsony::Jsony;
use rpc::{
    identity::SignedDeviceRoster,
    ids::{DeviceId, EventId, RoomId, UserId},
    mls::{
        EncryptedRoomDescriptor, MAX_MLS_WELCOMES_PER_COMMIT, MlsDeliveryEvent, MlsWelcome,
        MlsWelcomeBundle,
    },
};

const SNAPSHOT_VERSION: u32 = 1;
const SNAPSHOT_MAGIC: &[u8; 8] = b"CHATTMLS";
const WAL_VERSION: u32 = 1;
const WAL_MAGIC: &[u8; 8] = b"CHATTWAL";
const WAL_HEADER_BYTES: u64 = 12;
const WAL_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
const WAL_CHECKPOINT_RECORDS: u64 = 16 * 1024;

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
pub(super) struct DeviceCredential {
    pub user_id: UserId,
    pub device_id: DeviceId,
    pub token_hash: String,
}

#[derive(Clone, Debug, Default, Jsony)]
#[jsony(Binary, version)]
pub(super) struct GlobalRecord {
    pub initialized_accounts: Vec<UserId>,
    pub rosters: Vec<(UserId, SignedDeviceRoster)>,
    pub key_packages: Vec<(DeviceId, Vec<Vec<u8>>)>,
    pub retired_key_packages: Vec<[u8; 32]>,
    pub issued_key_packages: Vec<Vec<u8>>,
    pub used_key_packages: Vec<Vec<u8>>,
    pub next_welcome_delivery_id: u64,
    pub credentials: Vec<DeviceCredential>,
    pub device_owners: Vec<(DeviceId, UserId)>,
}

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
pub(super) struct RoomRecord {
    pub descriptor: EncryptedRoomDescriptor,
    pub public_state: PublicGroupState,
    pub group_info: Vec<u8>,
    pub revocation_pending: bool,
    pub head_sequence: u64,
    pub oldest_available_sequence: u64,
    pub last_event_time_unix_ms: u64,
    pub retention_days: u16,
    pub required_devices: Vec<DeviceId>,
}

#[derive(Clone, Debug)]
struct StoredEvent {
    event: MlsDeliveryEvent,
    stored_at_unix_ms: u64,
    payload_len: u32,
}

#[derive(Clone, Debug)]
struct StoredWelcome {
    delivery_id: u64,
    sequence: u64,
    stored_at_unix_ms: u64,
    bundle: MlsWelcomeBundle,
    payload_len: u32,
}

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
struct DeviceCursor {
    highest_contiguous_sequence: u64,
    starting_sequence: u64,
    last_ack_unix_ms: u64,
    rejoin_required_through: Option<u64>,
}

pub(super) struct LoadedState {
    pub global: GlobalRecord,
    pub rooms: Vec<RoomRecord>,
}

#[derive(Debug)]
pub(super) enum AppendApplicationResult {
    Stored {
        sequence: u64,
        room: RoomRecord,
        event: MlsDeliveryEvent,
    },
    AlreadyStored {
        sequence: u64,
    },
    Conflict,
    StaleEpoch {
        current_epoch: u64,
    },
    RevocationPending,
    RejoinRequired,
}

pub(super) struct EventBatch {
    pub events: Vec<MlsDeliveryEvent>,
    pub oldest_available_sequence: u64,
    pub head_sequence: u64,
}

#[derive(Default, Debug)]
pub(super) struct CleanupReport {
    pub deleted_events: usize,
    pub lagging_devices: usize,
    pub more_work: bool,
}

#[derive(Debug)]
pub(super) struct StorageStatus {
    pub allocated_bytes: u64,
    pub stored_bytes: u64,
    pub fragmented_bytes: u64,
    pub file_bytes: Option<u64>,
}

#[derive(Clone, Default)]
struct MemoryState {
    global: GlobalRecord,
    rooms: BTreeMap<u32, RoomRecord>,
    events: BTreeMap<(u32, u64), StoredEvent>,
    event_ids: BTreeMap<(u32, [u8; 16]), u64>,
    commit_epochs: BTreeMap<(u32, u64), u64>,
    device_cursors: BTreeMap<(u32, [u8; 16]), DeviceCursor>,
    welcomes: BTreeMap<u64, StoredWelcome>,
    welcome_target_counts: BTreeMap<u64, u16>,
    device_welcomes: BTreeSet<([u8; 16], u64)>,
    welcome_cursors: BTreeMap<[u8; 16], u64>,
    retired_payload_bytes: u64,
}

#[derive(Clone, Jsony)]
#[jsony(Binary, version)]
struct WalEvent {
    room_id: RoomId,
    stored_at_unix_ms: u64,
    event: MlsDeliveryEvent,
}

#[derive(Clone, Jsony)]
#[jsony(Binary, version)]
struct WalCursor {
    room_id: RoomId,
    device_id: DeviceId,
    cursor: DeviceCursor,
}

#[derive(Clone, Jsony)]
#[jsony(Binary, version)]
struct WalWelcome {
    delivery_id: u64,
    sequence: u64,
    stored_at_unix_ms: u64,
    bundle: MlsWelcomeBundle,
}

#[derive(Clone, Copy, Jsony)]
#[jsony(Binary, zerocopy)]
#[repr(C)]
struct EventKey {
    sequence: u64,
    room_id: u32,
    reserved: u32,
}

#[derive(Clone, Copy, Jsony)]
#[jsony(Binary, zerocopy)]
#[repr(C)]
struct CursorKey {
    device_id: [u8; 16],
    room_id: u32,
    reserved: u32,
}

#[derive(Default, Jsony)]
#[jsony(Binary, version)]
struct StateDelta {
    global: Option<GlobalRecord>,
    rooms: Vec<RoomRecord>,
    events: Vec<WalEvent>,
    deleted_events: Vec<EventKey>,
    cursors: Vec<WalCursor>,
    deleted_cursors: Vec<CursorKey>,
    welcomes: Vec<WalWelcome>,
    deleted_welcome_targets: Vec<DeviceWelcomeCheckpoint>,
    welcome_cursors: Vec<WelcomeCursorCheckpoint>,
}

impl StateDelta {
    fn is_empty(&self) -> bool {
        self.global.is_none()
            && self.rooms.is_empty()
            && self.events.is_empty()
            && self.deleted_events.is_empty()
            && self.cursors.is_empty()
            && self.deleted_cursors.is_empty()
            && self.welcomes.is_empty()
            && self.deleted_welcome_targets.is_empty()
            && self.welcome_cursors.is_empty()
    }
}

#[derive(Clone, Copy, Jsony)]
#[jsony(Binary, zerocopy)]
#[repr(C)]
struct EventCheckpoint {
    sequence: u64,
    stored_at_unix_ms: u64,
    payload_offset: u64,
    room_id: u32,
    payload_len: u32,
}

#[derive(Clone, Copy, Jsony)]
#[jsony(Binary, zerocopy)]
#[repr(C)]
struct CursorCheckpoint {
    highest_contiguous_sequence: u64,
    starting_sequence: u64,
    last_ack_unix_ms: u64,
    rejoin_required_through: u64,
    device_id: [u8; 16],
    room_id: u32,
    reserved: u32,
}

#[derive(Clone, Copy, Jsony)]
#[jsony(Binary, zerocopy)]
#[repr(C)]
struct WelcomeCheckpoint {
    delivery_id: u64,
    sequence: u64,
    stored_at_unix_ms: u64,
    payload_offset: u64,
    payload_len: u32,
    reserved: u32,
}

#[derive(Clone, Copy, Jsony)]
#[jsony(Binary, zerocopy)]
#[repr(C)]
struct DeviceWelcomeCheckpoint {
    delivery_id: u64,
    device_id: [u8; 16],
}

#[derive(Clone, Copy, Jsony)]
#[jsony(Binary, zerocopy)]
#[repr(C)]
struct WelcomeCursorCheckpoint {
    delivery_id: u64,
    device_id: [u8; 16],
}

#[derive(Jsony)]
#[jsony(ToBinary)]
struct CheckpointEncode<'a> {
    global: &'a GlobalRecord,
    rooms: Vec<&'a RoomRecord>,
    events: Vec<EventCheckpoint>,
    cursors: Vec<CursorCheckpoint>,
    welcomes: Vec<WelcomeCheckpoint>,
    device_welcomes: Vec<DeviceWelcomeCheckpoint>,
    welcome_cursors: Vec<WelcomeCursorCheckpoint>,
    payloads: &'a [u8],
}

#[derive(Jsony)]
#[jsony(FromBinary)]
struct CheckpointDecode {
    global: GlobalRecord,
    rooms: Vec<RoomRecord>,
    events: Vec<EventCheckpoint>,
    cursors: Vec<CursorCheckpoint>,
    welcomes: Vec<WelcomeCheckpoint>,
    device_welcomes: Vec<DeviceWelcomeCheckpoint>,
    welcome_cursors: Vec<WelcomeCursorCheckpoint>,
    payloads: Vec<u8>,
}

pub(super) struct MlsStore {
    state: Arc<RwLock<MemoryState>>,
    persistence: Option<Arc<Mutex<Persistence>>>,
    snapshot_writer: Option<Arc<SnapshotWriter>>,
    path: Option<PathBuf>,
    cleanup_after_room: u32,
}

struct Persistence {
    checkpoint_path: PathBuf,
    wal_path: PathBuf,
    sealed_wal_path: PathBuf,
    _lock: File,
    wal: File,
    wal_bytes: u64,
    records_since_checkpoint: u64,
    poisoned: bool,
}

enum SnapshotCommand {
    Request,
    Flush(std::sync::mpsc::SyncSender<Result<(), String>>),
}

struct SnapshotWriter {
    sender: Option<std::sync::mpsc::SyncSender<SnapshotCommand>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SnapshotWriter {
    fn spawn(
        checkpoint_path: PathBuf,
        sealed_wal_path: PathBuf,
        state: Arc<RwLock<MemoryState>>,
    ) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("chatt-mls-snapshot".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    let result = if sealed_wal_path.exists() {
                        snapshot_and_retire(&checkpoint_path, &sealed_wal_path, &state)
                    } else {
                        Ok(())
                    };
                    match command {
                        SnapshotCommand::Request => {
                            if let Err(error) = result {
                                kvlog::error!("MLS snapshot failed", error = error.as_str());
                            }
                        }
                        SnapshotCommand::Flush(reply) => {
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .expect("failed to spawn MLS snapshot writer");
        Self {
            sender: Some(sender),
            thread: Some(thread),
        }
    }

    fn request(&self) -> Result<(), String> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| "MLS snapshot writer is stopped".to_string())?;
        match sender.try_send(SnapshotCommand::Request) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                Err("MLS snapshot writer is gone".to_string())
            }
        }
    }

    fn flush(&self) -> Result<(), String> {
        let (reply, done) = std::sync::mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or_else(|| "MLS snapshot writer is stopped".to_string())?
            .send(SnapshotCommand::Flush(reply))
            .map_err(|_| "MLS snapshot writer is gone".to_string())?;
        done.recv()
            .map_err(|_| "MLS snapshot writer is gone".to_string())?
    }
}

impl Drop for SnapshotWriter {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn snapshot_and_retire(
    checkpoint_path: &Path,
    sealed_wal_path: &Path,
    state: &RwLock<MemoryState>,
) -> Result<(), String> {
    let snapshot = {
        let state = state.read().unwrap();
        state.clone()
    };
    let retired_payload_bytes = snapshot.retired_payload_bytes;
    let bytes = serialize_checkpoint(&snapshot);
    write_checkpoint(checkpoint_path, &bytes)?;
    let removed = match fs::remove_file(sealed_wal_path) {
        Ok(()) => sync_parent(sealed_wal_path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to remove {}: {error}",
            sealed_wal_path.display()
        )),
    };
    removed?;
    let mut state = state.write().unwrap();
    state.retired_payload_bytes = state
        .retired_payload_bytes
        .saturating_sub(retired_payload_bytes);
    Ok(())
}

impl std::fmt::Debug for MlsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MlsStore")
            .field("path", &self.path)
            .finish()
    }
}

impl MlsStore {
    pub fn open(data_dir: Option<&Path>) -> Result<Self, String> {
        let paths = data_dir
            .map(|data_dir| {
                fs::create_dir_all(data_dir)
                    .map_err(|error| format!("failed to create {}: {error}", data_dir.display()))?;
                Ok::<_, String>((
                    data_dir.join("mls-state.bin"),
                    data_dir.join("mls-state.wal"),
                ))
            })
            .transpose()?;
        let mut memory = match &paths {
            Some((path, _)) => load_checkpoint(path)?,
            None => MemoryState {
                global: GlobalRecord {
                    next_welcome_delivery_id: 1,
                    ..GlobalRecord::default()
                },
                ..MemoryState::default()
            },
        };
        let persistence = match &paths {
            Some((checkpoint_path, wal_path)) => Some(Arc::new(Mutex::new(Persistence::open(
                checkpoint_path.clone(),
                wal_path.clone(),
                &mut memory,
            )?))),
            None => None,
        };
        let state = Arc::new(RwLock::new(memory));
        let snapshot_writer = paths.as_ref().map(|(checkpoint_path, wal_path)| {
            let writer = Arc::new(SnapshotWriter::spawn(
                checkpoint_path.clone(),
                wal_path.with_extension("wal.sealed"),
                Arc::clone(&state),
            ));
            if wal_path.with_extension("wal.sealed").exists() {
                let _ = writer.request();
            }
            writer
        });
        Ok(Self {
            state,
            persistence,
            snapshot_writer,
            path: paths.map(|(path, _)| path),
            cleanup_after_room: 0,
        })
    }

    pub(super) fn read_handle(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            persistence: None,
            snapshot_writer: None,
            path: self.path.clone(),
            cleanup_after_room: 0,
        }
    }

    pub fn snapshot_state(&self) -> LoadedState {
        let state = self.state.read().unwrap();
        LoadedState {
            global: state.global.clone(),
            rooms: state.rooms.values().cloned().collect(),
        }
    }

    pub fn replace_global(&self, global: &GlobalRecord) -> Result<(), String> {
        self.replace_global_and_rooms(global, &[])
    }

    pub fn replace_global_and_rooms(
        &self,
        global: &GlobalRecord,
        rooms: &[RoomRecord],
    ) -> Result<(), String> {
        let mut delta = StateDelta {
            global: Some(global.clone()),
            rooms: rooms.to_vec(),
            ..StateDelta::default()
        };
        let state = self.state.read().unwrap();
        for room in rooms {
            let previous = state
                .rooms
                .get(&room.descriptor.room_id.0)
                .map(|room| room.required_devices.as_slice())
                .unwrap_or_default();
            plan_cursor_reconciliation(
                &state,
                room.descriptor.room_id,
                previous,
                room,
                room.head_sequence,
                &mut delta,
            )?;
        }
        drop(state);
        self.commit(delta, true)
    }

    pub fn create_room(
        &self,
        global: &GlobalRecord,
        room: &RoomRecord,
        event: &MlsDeliveryEvent,
        welcome: Option<(u64, &MlsWelcomeBundle)>,
    ) -> Result<(), String> {
        let delta = {
            let state = self.state.read().unwrap();
            let room_id = room.descriptor.room_id;
            if state.rooms.contains_key(&room_id.0) {
                return Err("encrypted room already exists in MLS storage".to_string());
            }
            ensure_event_available(&state, room_id, event)?;
            ensure_welcome_available(&state, welcome)?;
            let mut delta = StateDelta {
                global: Some(global.clone()),
                rooms: vec![room.clone()],
                events: vec![WalEvent {
                    room_id,
                    stored_at_unix_ms: room.last_event_time_unix_ms,
                    event: event.clone(),
                }],
                ..StateDelta::default()
            };
            plan_cursor_reconciliation(&state, room_id, &[], room, room.head_sequence, &mut delta)?;
            if let Some((delivery_id, bundle)) = welcome {
                delta.welcomes.push(WalWelcome {
                    delivery_id,
                    sequence: room.head_sequence,
                    stored_at_unix_ms: room.last_event_time_unix_ms,
                    bundle: bundle.clone(),
                });
            }
            delta
        };
        self.commit(delta, true)
    }

    pub fn append_commit(
        &self,
        global: &GlobalRecord,
        previous: &RoomRecord,
        room: &RoomRecord,
        event: &MlsDeliveryEvent,
        welcome: Option<(u64, &MlsWelcomeBundle)>,
    ) -> Result<(), String> {
        let delta = {
            let state = self.state.read().unwrap();
            let room_id = room.descriptor.room_id;
            let durable = state
                .rooms
                .get(&room_id.0)
                .ok_or_else(|| "encrypted room does not exist in MLS storage".to_string())?;
            if durable.public_state.epoch != previous.public_state.epoch
                || durable.head_sequence != previous.head_sequence
            {
                return Err("MLS room changed during commit validation".to_string());
            }
            ensure_event_available(&state, room_id, event)?;
            ensure_welcome_available(&state, welcome)?;
            let mut delta = StateDelta {
                global: Some(global.clone()),
                rooms: vec![room.clone()],
                events: vec![WalEvent {
                    room_id,
                    stored_at_unix_ms: room.last_event_time_unix_ms,
                    event: event.clone(),
                }],
                ..StateDelta::default()
            };
            plan_cursor_reconciliation(
                &state,
                room_id,
                &previous.required_devices,
                room,
                room.head_sequence,
                &mut delta,
            )?;
            if let Some((delivery_id, bundle)) = welcome {
                delta.welcomes.push(WalWelcome {
                    delivery_id,
                    sequence: room.head_sequence,
                    stored_at_unix_ms: room.last_event_time_unix_ms,
                    bundle: bundle.clone(),
                });
            }
            delta
        };
        self.commit(delta, true)
    }

    pub fn find_commit(
        &self,
        room_id: RoomId,
        parent_epoch: u64,
        commit: &[u8],
    ) -> Result<Option<(u64, u64)>, String> {
        let state = self.state.read().unwrap();
        let Some(sequence) = state.commit_epochs.get(&(room_id.0, parent_epoch)).copied() else {
            return Ok(None);
        };
        let stored = state
            .events
            .get(&(room_id.0, sequence))
            .ok_or_else(|| "MLS commit index references a missing event".to_string())?;
        match &stored.event {
            MlsDeliveryEvent::Commit {
                epoch,
                commit: stored,
                ..
            } if stored == commit => Ok(Some((sequence, *epoch))),
            _ => Ok(None),
        }
    }

    pub fn append_application(
        &self,
        room_id: RoomId,
        device_id: DeviceId,
        expected_epoch: u64,
        event_id: EventId,
        rosters: Vec<SignedDeviceRoster>,
        ciphertext: &[u8],
        now_unix_ms: u64,
    ) -> Result<AppendApplicationResult, String> {
        let (result, delta) = {
            let state = self.state.read().unwrap();
            if let Some(sequence) = state.event_ids.get(&(room_id.0, event_id.0)).copied() {
                let stored = state
                    .events
                    .get(&(room_id.0, sequence))
                    .ok_or_else(|| "MLS event id references a missing event".to_string())?;
                match &stored.event {
                    MlsDeliveryEvent::Application {
                        epoch,
                        event_id: stored_id,
                        ciphertext: stored_ciphertext,
                        ..
                    } if *epoch == expected_epoch
                        && *stored_id == event_id
                        && stored_ciphertext == ciphertext =>
                    {
                        (AppendApplicationResult::AlreadyStored { sequence }, None)
                    }
                    _ => (AppendApplicationResult::Conflict, None),
                }
            } else if state
                .device_cursors
                .get(&(room_id.0, device_id.0))
                .is_some_and(|cursor| cursor.rejoin_required_through.is_some())
            {
                (AppendApplicationResult::RejoinRequired, None)
            } else {
                let mut room = state
                    .rooms
                    .get(&room_id.0)
                    .cloned()
                    .ok_or_else(|| "encrypted room does not exist".to_string())?;
                if room.revocation_pending {
                    (AppendApplicationResult::RevocationPending, None)
                } else if room.public_state.epoch != expected_epoch {
                    (
                        AppendApplicationResult::StaleEpoch {
                            current_epoch: room.public_state.epoch,
                        },
                        None,
                    )
                } else {
                    let sequence = room
                        .head_sequence
                        .checked_add(1)
                        .filter(|sequence| *sequence < u64::MAX)
                        .ok_or_else(|| "MLS delivery sequence is exhausted".to_string())?;
                    let stored_at = now_unix_ms.max(room.last_event_time_unix_ms);
                    let event = MlsDeliveryEvent::Application {
                        sequence,
                        epoch: expected_epoch,
                        event_id,
                        rosters,
                        ciphertext: ciphertext.to_vec(),
                    };
                    room.head_sequence = sequence;
                    room.last_event_time_unix_ms = stored_at;
                    let delta = StateDelta {
                        rooms: vec![room.clone()],
                        events: vec![WalEvent {
                            room_id,
                            stored_at_unix_ms: stored_at,
                            event: event.clone(),
                        }],
                        ..StateDelta::default()
                    };
                    (
                        AppendApplicationResult::Stored {
                            sequence,
                            room,
                            event,
                        },
                        Some(delta),
                    )
                }
            }
        };
        if let Some(delta) = delta {
            self.commit(delta, true)?;
        }
        Ok(result)
    }

    pub fn application_retry(
        &self,
        room_id: RoomId,
        event_id: EventId,
        epoch: u64,
        ciphertext: &[u8],
    ) -> Result<Option<Result<u64, ()>>, String> {
        let state = self.state.read().unwrap();
        let Some(sequence) = state.event_ids.get(&(room_id.0, event_id.0)).copied() else {
            return Ok(None);
        };
        let stored = state
            .events
            .get(&(room_id.0, sequence))
            .ok_or_else(|| "MLS event id references a missing event".to_string())?;
        let exact = matches!(
            &stored.event,
            MlsDeliveryEvent::Application {
                epoch: stored_epoch,
                event_id: stored_id,
                ciphertext: stored_ciphertext,
                ..
            } if *stored_epoch == epoch && *stored_id == event_id && stored_ciphertext == ciphertext
        );
        Ok(Some(if exact { Ok(sequence) } else { Err(()) }))
    }

    pub fn events(&self, room_id: RoomId, after: u64, limit: usize) -> Result<EventBatch, String> {
        let state = self.state.read().unwrap();
        let room = state
            .rooms
            .get(&room_id.0)
            .ok_or_else(|| "encrypted room does not exist".to_string())?;
        let events = match after.checked_add(1) {
            Some(start) => state
                .events
                .range((room_id.0, start.max(room.oldest_available_sequence))..=(room_id.0, u64::MAX))
                .take(limit)
                .map(|(_, stored)| stored.event.clone())
                .collect(),
            None => Vec::new(),
        };
        Ok(EventBatch {
            events,
            oldest_available_sequence: room.oldest_available_sequence,
            head_sequence: room.head_sequence,
        })
    }

    pub fn acknowledge(
        &self,
        room_id: RoomId,
        device_id: DeviceId,
        sequence: u64,
        now_unix_ms: u64,
    ) -> Result<bool, String> {
        let delta = {
            let state = self.state.read().unwrap();
            let room = state
                .rooms
                .get(&room_id.0)
                .ok_or_else(|| "encrypted room does not exist".to_string())?;
            if sequence > room.head_sequence {
                return Err("MLS acknowledgement is beyond the room head".to_string());
            }
            let mut cursor = state
                .device_cursors
                .get(&(room_id.0, device_id.0))
                .cloned()
                .ok_or_else(|| "MLS device has no cursor for this room".to_string())?;
            if sequence < cursor.starting_sequence {
                return Err("MLS acknowledgement predates the device's room membership".to_string());
            }
            if sequence <= cursor.highest_contiguous_sequence {
                None
            } else {
                cursor.highest_contiguous_sequence = sequence;
                cursor.last_ack_unix_ms = now_unix_ms;
                if cursor
                    .rejoin_required_through
                    .is_some_and(|watermark| sequence > watermark)
                {
                    cursor.rejoin_required_through = None;
                }
                Some(StateDelta {
                    cursors: vec![WalCursor {
                        room_id,
                        device_id,
                        cursor,
                    }],
                    ..StateDelta::default()
                })
            }
        };
        if let Some(delta) = delta {
            // Cursor progress is advisory. As with redb's former
            // `Durability::None`, a crash may replay events but cannot delete
            // an event the client has not seen.
            self.commit(delta, false)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn persist_acknowledgement(
        &self,
        room_id: RoomId,
        device_id: DeviceId,
    ) -> Result<(), String> {
        let cursor = self
            .state
            .read()
            .unwrap()
            .device_cursors
            .get(&(room_id.0, device_id.0))
            .cloned()
            .ok_or_else(|| "MLS device has no cursor for this room".to_string())?;
        self.commit(
            StateDelta {
                cursors: vec![WalCursor {
                    room_id,
                    device_id,
                    cursor,
                }],
                ..StateDelta::default()
            },
            false,
        )
    }

    pub fn requires_rejoin(&self, room_id: RoomId, device_id: DeviceId) -> Result<bool, String> {
        Ok(self
            .state
            .read()
            .unwrap()
            .device_cursors
            .get(&(room_id.0, device_id.0))
            .is_some_and(|cursor| cursor.rejoin_required_through.is_some()))
    }

    pub fn welcomes(&self, device_id: DeviceId, after: u64) -> Result<Vec<MlsWelcome>, String> {
        let Some(start) = after.checked_add(1) else {
            return Ok(Vec::new());
        };
        let state = self.state.read().unwrap();
        state
            .device_welcomes
            .range((device_id.0, start)..=(device_id.0, u64::MAX))
            .map(|(_, delivery_id)| {
                let stored = state
                    .welcomes
                    .get(delivery_id)
                    .ok_or_else(|| "MLS Welcome target references a missing value".to_string())?;
                Ok(MlsWelcome {
                    delivery_id: *delivery_id,
                    sequence: stored.sequence,
                    device_id,
                    descriptor: stored.bundle.descriptor.clone(),
                    welcome: stored.bundle.welcome.clone(),
                })
            })
            .collect()
    }

    pub fn welcome_head(&self, device_id: DeviceId) -> Result<u64, String> {
        Ok(welcome_head_memory(
            &self.state.read().unwrap(),
            device_id,
        ))
    }

    pub fn acknowledge_welcome(
        &self,
        device_id: DeviceId,
        delivery_id: u64,
    ) -> Result<bool, String> {
        {
            let state = self.state.read().unwrap();
            let current = state
                .welcome_cursors
                .get(&device_id.0)
                .copied()
                .unwrap_or_default();
            if delivery_id <= current {
                return Ok(false);
            }
            if delivery_id > welcome_head_memory(&state, device_id) {
                return Err("MLS Welcome acknowledgement is beyond the inbox head".to_string());
            }
        }
        self.commit(
            StateDelta {
                welcome_cursors: vec![WelcomeCursorCheckpoint {
                    delivery_id,
                    device_id: device_id.0,
                }],
                ..StateDelta::default()
            },
            false,
        )?;
        Ok(true)
    }

    pub fn persist_welcome_acknowledgement(&self, device_id: DeviceId) -> Result<(), String> {
        let delivery_id = self
            .state
            .read()
            .unwrap()
            .welcome_cursors
            .get(&device_id.0)
            .copied()
            .ok_or_else(|| "MLS device has no Welcome acknowledgement".to_string())?;
        self.commit(
            StateDelta {
                welcome_cursors: vec![WelcomeCursorCheckpoint {
                    delivery_id,
                    device_id: device_id.0,
                }],
                ..StateDelta::default()
            },
            false,
        )
    }

    pub fn cleanup(
        &mut self,
        now_unix_ms: u64,
        batch_limit: usize,
    ) -> Result<CleanupReport, String> {
        if batch_limit == 0 {
            return Ok(CleanupReport::default());
        }
        let (report, delta) = {
            let state = self.state.read().unwrap();
            let mut room_ids = state.rooms.keys().copied().collect::<Vec<_>>();
            room_ids.sort_by_key(|room_id| (*room_id <= self.cleanup_after_room, *room_id));
            let mut selected = None;
            for room_id in room_ids {
                if let Some((report, delta)) =
                    plan_prune_room(&state, RoomId(room_id), now_unix_ms, batch_limit)?
                {
                    self.cleanup_after_room = room_id;
                    selected = Some((report, delta));
                    break;
                }
            }
            match selected {
                Some((mut report, mut delta)) => {
                    let (_, welcomes) = plan_cleanup_welcomes(&state, now_unix_ms, batch_limit)?;
                    report.more_work = true;
                    delta.deleted_welcome_targets = welcomes;
                    (report, delta)
                }
                None => {
                    let (more_work, deleted_welcome_targets) =
                        plan_cleanup_welcomes(&state, now_unix_ms, batch_limit)?;
                    (
                        CleanupReport {
                            more_work,
                            ..CleanupReport::default()
                        },
                        StateDelta {
                            deleted_welcome_targets,
                            ..StateDelta::default()
                        },
                    )
                }
            }
        };
        if !delta.is_empty() {
            self.commit(delta, true)?;
        }
        Ok(report)
    }

    pub fn status(&self) -> Result<StorageStatus, String> {
        match &self.persistence {
            Some(persistence) => {
                let persistence = persistence.lock().unwrap();
                let checkpoint_bytes = fs::metadata(&persistence.checkpoint_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or_default();
                let sealed_wal_bytes = fs::metadata(&persistence.sealed_wal_path)
                    .map(|metadata| metadata.len())
                    .unwrap_or_default();
                let state = self.state.read().unwrap();
                let fragmented_bytes = fragmented_payload_bytes(&state);
                let file_bytes = checkpoint_bytes
                    .saturating_add(sealed_wal_bytes)
                    .saturating_add(persistence.wal_bytes);
                Ok(StorageStatus {
                    allocated_bytes: file_bytes,
                    stored_bytes: file_bytes.saturating_sub(fragmented_bytes),
                    fragmented_bytes,
                    file_bytes: Some(file_bytes),
                })
            }
            None => Ok(StorageStatus {
                allocated_bytes: 0,
                stored_bytes: 0,
                fragmented_bytes: 0,
                file_bytes: None,
            }),
        }
    }

    pub fn compact(&mut self) -> Result<bool, String> {
        match &self.persistence {
            Some(persistence) => {
                if let Some(writer) = &self.snapshot_writer {
                    writer.flush()?;
                }
                let mut persistence = persistence.lock().unwrap();
                let mut state = self.state.write().unwrap();
                persistence.checkpoint(&state)?;
                reset_fragmentation_accounting(&mut state);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub fn checkpoint_if_needed(&self) -> Result<bool, String> {
        let Some(persistence) = &self.persistence else {
            return Ok(false);
        };
        let mut persistence = persistence.lock().unwrap();
        if persistence.sealed_wal_path.exists() {
            if let Some(writer) = &self.snapshot_writer {
                writer.request()?;
            }
            return Ok(false);
        }
        if persistence.wal_bytes < WAL_CHECKPOINT_BYTES
            && persistence.records_since_checkpoint < WAL_CHECKPOINT_RECORDS
        {
            return Ok(false);
        }
        persistence.seal()?;
        drop(persistence);
        if let Some(writer) = &self.snapshot_writer {
            writer.request()?;
        }
        Ok(true)
    }

    fn commit(&self, delta: StateDelta, durable: bool) -> Result<(), String> {
        if let Some(persistence) = &self.persistence {
            let mut persistence = persistence.lock().unwrap();
            persistence.append(&delta, durable)?;
            apply_delta(&mut self.state.write().unwrap(), delta)?;
        } else {
            apply_delta(&mut self.state.write().unwrap(), delta)?;
        }
        Ok(())
    }
}

fn reset_fragmentation_accounting(state: &mut MemoryState) {
    state.retired_payload_bytes = 0;
}

fn apply_delta(state: &mut MemoryState, delta: StateDelta) -> Result<(), String> {
    for key in delta.deleted_events {
        remove_event_memory(state, RoomId(key.room_id), key.sequence);
    }
    for key in delta.deleted_cursors {
        state.device_cursors.remove(&(key.room_id, key.device_id));
    }
    for target in delta.deleted_welcome_targets {
        let key = (target.device_id, target.delivery_id);
        if !state.device_welcomes.remove(&key) {
            continue;
        }
        let Some(count) = state.welcome_target_counts.get_mut(&target.delivery_id) else {
            return Err("MLS Welcome target count is missing".to_string());
        };
        if *count == 1 {
            state.welcome_target_counts.remove(&target.delivery_id);
            if let Some(welcome) = state.welcomes.remove(&target.delivery_id) {
                state.retired_payload_bytes = state
                    .retired_payload_bytes
                    .saturating_add(u64::from(welcome.payload_len));
            }
        } else {
            *count -= 1;
        }
    }
    for stored in delta.events {
        insert_event_memory(
            state,
            stored.room_id,
            stored.event,
            stored.stored_at_unix_ms,
        );
    }
    for stored in delta.welcomes {
        insert_welcome_memory(
            state,
            Some((stored.delivery_id, &stored.bundle)),
            stored.sequence,
            stored.stored_at_unix_ms,
        );
    }
    for room in delta.rooms {
        state.rooms.insert(room.descriptor.room_id.0, room);
    }
    for stored in delta.cursors {
        state
            .device_cursors
            .insert((stored.room_id.0, stored.device_id.0), stored.cursor);
    }
    for cursor in delta.welcome_cursors {
        state
            .welcome_cursors
            .insert(cursor.device_id, cursor.delivery_id);
    }
    if let Some(global) = delta.global {
        state.global = global;
    }
    Ok(())
}

fn remove_event_memory(state: &mut MemoryState, room_id: RoomId, sequence: u64) {
    let Some(stored) = state.events.remove(&(room_id.0, sequence)) else {
        return;
    };
    state.retired_payload_bytes = state
        .retired_payload_bytes
        .saturating_add(u64::from(stored.payload_len));
    match stored.event {
        MlsDeliveryEvent::Application { event_id, .. } => {
            state.event_ids.remove(&(room_id.0, event_id.0));
        }
        MlsDeliveryEvent::Commit { parent_epoch, .. } => {
            state.commit_epochs.remove(&(room_id.0, parent_epoch));
        }
    }
}

fn fragmented_payload_bytes(state: &MemoryState) -> u64 {
    state.retired_payload_bytes
}

fn welcome_head_memory(state: &MemoryState, device_id: DeviceId) -> u64 {
    let pending = state
        .device_welcomes
        .range((device_id.0, 0)..=(device_id.0, u64::MAX))
        .next_back()
        .map_or(0, |(_, delivery_id)| *delivery_id);
    pending.max(
        state
            .welcome_cursors
            .get(&device_id.0)
            .copied()
            .unwrap_or_default(),
    )
}

fn ensure_event_available(
    state: &MemoryState,
    room_id: RoomId,
    event: &MlsDeliveryEvent,
) -> Result<(), String> {
    if state.events.contains_key(&(room_id.0, event.sequence())) {
        return Err("MLS delivery sequence unexpectedly already exists".to_string());
    }
    match event {
        MlsDeliveryEvent::Application { event_id, .. }
            if state.event_ids.contains_key(&(room_id.0, event_id.0)) =>
        {
            Err("MLS application event id unexpectedly already exists".to_string())
        }
        MlsDeliveryEvent::Commit { parent_epoch, .. }
            if state
                .commit_epochs
                .contains_key(&(room_id.0, *parent_epoch)) =>
        {
            Err("MLS commit parent epoch unexpectedly already exists".to_string())
        }
        _ => Ok(()),
    }
}

fn insert_event_memory(
    state: &mut MemoryState,
    room_id: RoomId,
    event: MlsDeliveryEvent,
    stored_at_unix_ms: u64,
) {
    let sequence = event.sequence();
    if state
        .events
        .get(&(room_id.0, sequence))
        .is_some_and(|stored| {
            stored.event == event && stored.stored_at_unix_ms == stored_at_unix_ms
        })
    {
        return;
    }
    remove_event_memory(state, room_id, sequence);
    let payload_len = match &event {
        MlsDeliveryEvent::Application { ciphertext, .. } => ciphertext.len(),
        MlsDeliveryEvent::Commit { commit, .. } => commit.len(),
    };
    let payload_len =
        u32::try_from(payload_len).expect("MLS event exceeds the binary snapshot limit");
    match &event {
        MlsDeliveryEvent::Application { event_id, .. } => {
            state.event_ids.insert((room_id.0, event_id.0), sequence);
        }
        MlsDeliveryEvent::Commit { parent_epoch, .. } => {
            state
                .commit_epochs
                .insert((room_id.0, *parent_epoch), sequence);
        }
    }
    state.events.insert(
        (room_id.0, sequence),
        StoredEvent {
            event,
            stored_at_unix_ms,
            payload_len,
        },
    );
}

fn plan_cursor_reconciliation(
    state: &MemoryState,
    room_id: RoomId,
    previous: &[DeviceId],
    room: &RoomRecord,
    sequence: u64,
    delta: &mut StateDelta,
) -> Result<(), String> {
    for removed in previous
        .iter()
        .filter(|device| !room.required_devices.contains(device))
    {
        delta.deleted_cursors.push(CursorKey {
            device_id: removed.0,
            room_id: room_id.0,
            reserved: 0,
        });
    }
    for added in room
        .required_devices
        .iter()
        .filter(|device| !previous.contains(device))
    {
        if state.device_cursors.contains_key(&(room_id.0, added.0)) {
            return Err("new MLS room member unexpectedly already has a cursor".to_string());
        }
        delta.cursors.push(WalCursor {
            room_id,
            device_id: *added,
            cursor: DeviceCursor {
                highest_contiguous_sequence: sequence,
                starting_sequence: sequence,
                last_ack_unix_ms: room.last_event_time_unix_ms,
                rejoin_required_through: None,
            },
        });
    }
    Ok(())
}

fn ensure_welcome_available(
    state: &MemoryState,
    welcome: Option<(u64, &MlsWelcomeBundle)>,
) -> Result<(), String> {
    let Some((delivery_id, bundle)) = welcome else {
        return Ok(());
    };
    if state.welcomes.contains_key(&delivery_id) {
        return Err("MLS Welcome delivery id unexpectedly already exists".to_string());
    }
    if bundle
        .device_ids
        .iter()
        .any(|device| state.device_welcomes.contains(&(device.0, delivery_id)))
    {
        return Err("MLS Welcome target unexpectedly already exists".to_string());
    }
    Ok(())
}

fn insert_welcome_memory(
    state: &mut MemoryState,
    welcome: Option<(u64, &MlsWelcomeBundle)>,
    sequence: u64,
    stored_at_unix_ms: u64,
) {
    let Some((delivery_id, bundle)) = welcome else {
        return;
    };
    if state.welcomes.get(&delivery_id).is_some_and(|stored| {
        stored.sequence == sequence
            && stored.stored_at_unix_ms == stored_at_unix_ms
            && stored.bundle == *bundle
    }) {
        return;
    }
    let old_targets = state
        .device_welcomes
        .iter()
        .filter(|(_, stored_delivery_id)| *stored_delivery_id == delivery_id)
        .copied()
        .collect::<Vec<_>>();
    for target in old_targets {
        state.device_welcomes.remove(&target);
    }
    state.welcome_target_counts.remove(&delivery_id);
    if let Some(previous) = state.welcomes.remove(&delivery_id) {
        state.retired_payload_bytes = state
            .retired_payload_bytes
            .saturating_add(u64::from(previous.payload_len));
    }
    let payload_len =
        u32::try_from(bundle.welcome.len()).expect("MLS Welcome exceeds the binary snapshot limit");
    state.welcomes.insert(
        delivery_id,
        StoredWelcome {
            delivery_id,
            sequence,
            stored_at_unix_ms,
            bundle: bundle.clone(),
            payload_len,
        },
    );
    for device in &bundle.device_ids {
        state.device_welcomes.insert((device.0, delivery_id));
    }
    state
        .welcome_target_counts
        .insert(delivery_id, bundle.device_ids.len() as u16);
}

fn plan_prune_room(
    state: &MemoryState,
    room_id: RoomId,
    now_unix_ms: u64,
    batch_limit: usize,
) -> Result<Option<(CleanupReport, StateDelta)>, String> {
    let Some(mut room) = state.rooms.get(&room_id.0).cloned() else {
        return Ok(None);
    };
    if room.oldest_available_sequence > room.head_sequence {
        return Ok(None);
    }
    let mut required = Vec::new();
    for device in &room.required_devices {
        let cursor = state
            .device_cursors
            .get(&(room_id.0, device.0))
            .cloned()
            .ok_or_else(|| "required MLS device is missing its cursor".to_string())?;
        required.push((*device, cursor));
    }
    let safe = required
        .iter()
        .map(|(_, cursor)| cursor.highest_contiguous_sequence)
        .min()
        .unwrap_or(room.head_sequence);
    let retention_ms = u64::from(room.retention_days).saturating_mul(24 * 60 * 60 * 1000);
    let cutoff = now_unix_ms.saturating_sub(retention_ms);
    let mut expiry = room.oldest_available_sequence.saturating_sub(1);
    for (_, stored) in state
        .events
        .range((room_id.0, room.oldest_available_sequence)..=(room_id.0, room.head_sequence))
    {
        if stored.stored_at_unix_ms >= cutoff {
            break;
        }
        expiry = stored.event.sequence();
    }
    let prune_through = safe.max(expiry).min(room.head_sequence);
    if prune_through < room.oldest_available_sequence {
        return Ok(None);
    }
    let mut report = CleanupReport::default();
    let mut delta = StateDelta::default();
    if expiry > safe {
        for (device, mut cursor) in required {
            if cursor.highest_contiguous_sequence < expiry {
                cursor.rejoin_required_through = Some(expiry);
                delta.cursors.push(WalCursor {
                    room_id,
                    device_id: device,
                    cursor,
                });
                report.lagging_devices += 1;
            }
        }
    }
    let end = prune_through.min(
        room.oldest_available_sequence
            .saturating_add(batch_limit as u64)
            .saturating_sub(1),
    );
    for sequence in room.oldest_available_sequence..=end {
        state
            .events
            .get(&(room_id.0, sequence))
            .ok_or_else(|| "MLS cleanup found a hole in the delivery log".to_string())?;
        delta.deleted_events.push(EventKey {
            sequence,
            room_id: room_id.0,
            reserved: 0,
        });
        report.deleted_events += 1;
    }
    room.oldest_available_sequence = end.saturating_add(1);
    report.more_work = end < prune_through;
    delta.rooms.push(room);
    Ok(Some((report, delta)))
}

fn plan_cleanup_welcomes(
    state: &MemoryState,
    now_unix_ms: u64,
    batch_limit: usize,
) -> Result<(bool, Vec<DeviceWelcomeCheckpoint>), String> {
    let mut remove_targets = Vec::new();
    let mut more_work = false;
    for &(device, delivery_id) in &state.device_welcomes {
        let acknowledged = state
            .welcome_cursors
            .get(&device)
            .is_some_and(|cursor| *cursor >= delivery_id);
        let welcome = state
            .welcomes
            .get(&delivery_id)
            .ok_or_else(|| "MLS Welcome target references a missing value".to_string())?;
        let room = state
            .rooms
            .get(&welcome.bundle.descriptor.room_id.0)
            .ok_or_else(|| "MLS Welcome references a missing room".to_string())?;
        let expired = welcome.stored_at_unix_ms
            < now_unix_ms
                .saturating_sub(u64::from(room.retention_days).saturating_mul(24 * 60 * 60 * 1000));
        if acknowledged || expired {
            if remove_targets.len() == batch_limit {
                more_work = true;
                break;
            }
            remove_targets.push(DeviceWelcomeCheckpoint {
                delivery_id,
                device_id: device,
            });
        }
    }
    Ok((more_work, remove_targets))
}

fn validate_memory(state: &MemoryState) -> Result<(), String> {
    if state.global.next_welcome_delivery_id == 0 {
        return Err("persisted MLS Welcome delivery head is invalid".to_string());
    }
    for room in state.rooms.values() {
        let id = room.descriptor.room_id;
        let Some(after_head) = room.head_sequence.checked_add(1) else {
            return Err("persisted MLS room has exhausted its delivery sequence".to_string());
        };
        if room.oldest_available_sequence == 0
            || room.oldest_available_sequence > after_head
            || room
                .required_devices
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err("persisted MLS room has an invalid retained range".to_string());
        }
        let mut expected = room.oldest_available_sequence;
        if room.oldest_available_sequence <= room.head_sequence {
            for ((_, sequence), stored) in state
                .events
                .range((id.0, room.oldest_available_sequence)..=(id.0, room.head_sequence))
            {
                if *sequence != expected || stored.event.sequence() != expected {
                    return Err("persisted MLS delivery sequence is not contiguous".to_string());
                }
                expected = expected
                    .checked_add(1)
                    .ok_or_else(|| "persisted MLS delivery sequence is exhausted".to_string())?;
            }
        }
        if expected != after_head {
            return Err("persisted MLS delivery head does not match its log".to_string());
        }
        for device in &room.required_devices {
            if !state.device_cursors.contains_key(&(id.0, device.0)) {
                return Err("persisted MLS required device has no cursor".to_string());
            }
        }
    }

    let mut application_events = 0usize;
    let mut commit_events = 0usize;
    for (&(room_id, sequence), stored) in &state.events {
        let room = state
            .rooms
            .get(&room_id)
            .ok_or_else(|| "persisted MLS event references a missing room".to_string())?;
        if sequence < room.oldest_available_sequence
            || sequence > room.head_sequence
            || stored.event.sequence() != sequence
        {
            return Err("persisted MLS event lies outside its room log".to_string());
        }
        match &stored.event {
            MlsDeliveryEvent::Application { event_id, .. } => {
                application_events += 1;
                if state.event_ids.get(&(room_id, event_id.0)) != Some(&sequence) {
                    return Err("persisted MLS application index is inconsistent".to_string());
                }
            }
            MlsDeliveryEvent::Commit { parent_epoch, .. } => {
                commit_events += 1;
                if state.commit_epochs.get(&(room_id, *parent_epoch)) != Some(&sequence) {
                    return Err("persisted MLS commit index is inconsistent".to_string());
                }
            }
        }
    }
    if application_events != state.event_ids.len() || commit_events != state.commit_epochs.len() {
        return Err("persisted MLS delivery indexes contain orphan entries".to_string());
    }

    for (&(room_id, device_id), cursor) in &state.device_cursors {
        let room = state
            .rooms
            .get(&room_id)
            .ok_or_else(|| "persisted MLS cursor references a missing room".to_string())?;
        if room
            .required_devices
            .binary_search(&DeviceId(device_id))
            .is_err()
        {
            return Err("persisted MLS cursor references a non-required device".to_string());
        }
        if cursor.starting_sequence == 0
            || cursor.starting_sequence > cursor.highest_contiguous_sequence
            || cursor.highest_contiguous_sequence > room.head_sequence
            || cursor.rejoin_required_through.is_some_and(|watermark| {
                watermark < cursor.highest_contiguous_sequence || watermark > room.head_sequence
            })
        {
            return Err("persisted MLS device cursor is inconsistent".to_string());
        }
    }

    for (&delivery_id, welcome) in &state.welcomes {
        if delivery_id == 0 || delivery_id >= state.global.next_welcome_delivery_id {
            return Err("persisted MLS Welcome delivery id is inconsistent".to_string());
        }
        let room = state
            .rooms
            .get(&welcome.bundle.descriptor.room_id.0)
            .ok_or_else(|| "persisted MLS Welcome references a missing room".to_string())?;
        if welcome.bundle.descriptor != room.descriptor
            || welcome.sequence == 0
            || welcome.sequence > room.head_sequence
            || welcome.bundle.device_ids.is_empty()
            || welcome.bundle.device_ids.len() > MAX_MLS_WELCOMES_PER_COMMIT
            || welcome
                .bundle
                .device_ids
                .iter()
                .enumerate()
                .any(|(index, device)| welcome.bundle.device_ids[index + 1..].contains(device))
        {
            return Err("persisted MLS Welcome metadata is inconsistent".to_string());
        }
        let Some(target_count) = state.welcome_target_counts.get(&delivery_id) else {
            return Err("persisted MLS Welcome targets are inconsistent".to_string());
        };
        if *target_count == 0 || usize::from(*target_count) > welcome.bundle.device_ids.len() {
            return Err("persisted MLS Welcome target count is inconsistent".to_string());
        }
        if welcome.sequence >= room.oldest_available_sequence
            && !state
                .events
                .get(&(room.descriptor.room_id.0, welcome.sequence))
                .is_some_and(|stored| matches!(&stored.event, MlsDeliveryEvent::Commit { .. }))
        {
            return Err("persisted MLS Welcome does not reference a retained commit".to_string());
        }
    }
    let mut actual_target_counts = BTreeMap::<u64, u16>::new();
    for &(device_id, delivery_id) in &state.device_welcomes {
        let welcome = state
            .welcomes
            .get(&delivery_id)
            .ok_or_else(|| "persisted MLS Welcome target references a missing value".to_string())?;
        if !welcome.bundle.device_ids.contains(&DeviceId(device_id)) {
            return Err("persisted MLS Welcome target is inconsistent".to_string());
        }
        let count = actual_target_counts.entry(delivery_id).or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| "persisted MLS Welcome target count is too large".to_string())?;
    }
    if actual_target_counts != state.welcome_target_counts {
        return Err("persisted MLS Welcome target counts are inconsistent".to_string());
    }
    if state.welcome_cursors.values().any(|delivery_id| {
        *delivery_id == 0 || *delivery_id >= state.global.next_welcome_delivery_id
    }) {
        return Err("persisted MLS Welcome cursor is inconsistent".to_string());
    }
    Ok(())
}

impl Persistence {
    fn open(
        checkpoint_path: PathBuf,
        wal_path: PathBuf,
        state: &mut MemoryState,
    ) -> Result<Self, String> {
        let sealed_wal_path = wal_path.with_extension("wal.sealed");
        let lock_path = checkpoint_path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| format!("failed to open {}: {error}", lock_path.display()))?;
        lock_store(&lock, &lock_path)?;
        let (_, sealed_records) = replay_wal(&sealed_wal_path, state)?;
        let (valid_bytes, active_records) = replay_wal(&wal_path, state)?;
        let records_since_checkpoint = sealed_records.saturating_add(active_records);
        validate_memory(state)?;
        let mut wal = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&wal_path)
            .map_err(|error| format!("failed to open {}: {error}", wal_path.display()))?;
        let wal_bytes = if valid_bytes == 0 {
            wal.write_all(WAL_MAGIC)
                .and_then(|()| wal.write_all(&WAL_VERSION.to_le_bytes()))
                .and_then(|()| wal.sync_data())
                .map_err(|error| format!("failed to initialize {}: {error}", wal_path.display()))?;
            sync_parent(&wal_path)?;
            WAL_HEADER_BYTES
        } else {
            valid_bytes
        };
        Ok(Self {
            checkpoint_path,
            wal_path,
            sealed_wal_path,
            _lock: lock,
            wal,
            wal_bytes,
            records_since_checkpoint,
            poisoned: false,
        })
    }

    fn append(&mut self, delta: &StateDelta, durable: bool) -> Result<(), String> {
        if self.poisoned {
            return Err("MLS WAL is unavailable after an earlier write failure".to_string());
        }
        let body = jsony::to_binary(delta);
        let len = u32::try_from(body.len())
            .map_err(|_| "MLS WAL record exceeds the binary format limit".to_string())?;
        let original_len = self.wal_bytes;
        let mut frame = Vec::with_capacity(12 + body.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&wal_checksum(&body).to_le_bytes());
        frame.extend_from_slice(&body);
        let result = self.wal.write_all(&frame).and_then(|()| {
            if durable {
                self.wal.sync_data()
            } else {
                Ok(())
            }
        });
        if let Err(error) = result {
            if self
                .wal
                .set_len(original_len)
                .and_then(|()| self.wal.sync_data())
                .is_err()
            {
                self.poisoned = true;
            }
            return Err(format!(
                "failed to append {}: {error}",
                self.wal_path.display()
            ));
        }
        self.wal_bytes = self.wal_bytes.saturating_add(frame.len() as u64);
        self.records_since_checkpoint = self.records_since_checkpoint.saturating_add(1);
        Ok(())
    }

    fn checkpoint(&mut self, state: &MemoryState) -> Result<(), String> {
        let bytes = serialize_checkpoint(state);
        write_checkpoint(&self.checkpoint_path, &bytes)?;
        self.wal
            .set_len(0)
            .and_then(|()| self.wal.seek(SeekFrom::Start(0)).map(|_| ()))
            .and_then(|()| self.wal.write_all(WAL_MAGIC))
            .and_then(|()| self.wal.write_all(&WAL_VERSION.to_le_bytes()))
            .and_then(|()| self.wal.sync_data())
            .map_err(|error| format!("failed to reset {}: {error}", self.wal_path.display()))?;
        self.wal_bytes = WAL_HEADER_BYTES;
        self.records_since_checkpoint = 0;
        self.poisoned = false;
        match fs::remove_file(&self.sealed_wal_path) {
            Ok(()) => sync_parent(&self.sealed_wal_path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to remove {}: {error}",
                    self.sealed_wal_path.display()
                ));
            }
        }
        Ok(())
    }

    fn seal(&mut self) -> Result<(), String> {
        if self.poisoned {
            return Err("MLS WAL is unavailable after an earlier write failure".to_string());
        }
        if self.sealed_wal_path.exists() {
            return Ok(());
        }
        fs::rename(&self.wal_path, &self.sealed_wal_path).map_err(|error| {
            format!(
                "failed to rotate {} to {}: {error}",
                self.wal_path.display(),
                self.sealed_wal_path.display()
            )
        })?;
        let replacement = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .append(true)
                .read(true)
                .open(&self.wal_path)?;
            file.write_all(WAL_MAGIC)?;
            file.write_all(&WAL_VERSION.to_le_bytes())?;
            file.sync_data()?;
            Ok::<_, std::io::Error>(file)
        })();
        let replacement = match replacement {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::rename(&self.sealed_wal_path, &self.wal_path);
                return Err(format!(
                    "failed to create rotated {}: {error}",
                    self.wal_path.display()
                ));
            }
        };
        self.wal = replacement;
        self.wal_bytes = WAL_HEADER_BYTES;
        self.records_since_checkpoint = 0;
        sync_parent(&self.wal_path)?;
        Ok(())
    }
}

fn replay_wal(path: &Path, state: &mut MemoryState) -> Result<(u64, u64), String> {
    let mut bytes = Vec::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_end(&mut bytes)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(format!("failed to open {}: {error}", path.display())),
    }
    if bytes.is_empty() {
        return Ok((0, 0));
    }
    if bytes.len() < WAL_HEADER_BYTES as usize {
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|error| format!("failed to repair {}: {error}", path.display()))?;
        file.set_len(0)
            .and_then(|()| file.sync_data())
            .map_err(|error| format!("failed to repair {}: {error}", path.display()))?;
        return Ok((0, 0));
    }
    if &bytes[..8] != WAL_MAGIC {
        return Err("invalid MLS WAL header".to_string());
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != WAL_VERSION {
        return Err(format!(
            "unsupported MLS WAL version {version}; expected {WAL_VERSION}"
        ));
    }
    let mut offset = WAL_HEADER_BYTES as usize;
    let mut records = 0u64;
    while bytes.len().saturating_sub(offset) >= 12 {
        let len = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let checksum = u64::from_le_bytes(bytes[offset + 4..offset + 12].try_into().unwrap());
        let Some(end) = offset
            .checked_add(12)
            .and_then(|start| start.checked_add(len))
        else {
            break;
        };
        let Some(body) = bytes.get(offset + 12..end) else {
            break;
        };
        if wal_checksum(body) != checksum {
            return Err(format!(
                "{} contains a corrupt MLS WAL record",
                path.display()
            ));
        }
        let delta = jsony::from_binary::<StateDelta>(body)
            .map_err(|error| format!("failed to decode {}: {error}", path.display()))?;
        apply_delta(state, delta)?;
        offset = end;
        records = records.saturating_add(1);
    }
    if offset != bytes.len() {
        kvlog::warn!(
            "MLS WAL tail incomplete; truncating",
            path = path.display().to_string().as_str(),
            valid_bytes = offset as u64,
            total_bytes = bytes.len() as u64,
        );
        let file = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|error| format!("failed to repair {}: {error}", path.display()))?;
        file.set_len(offset as u64)
            .and_then(|()| file.sync_data())
            .map_err(|error| format!("failed to repair {}: {error}", path.display()))?;
    }
    Ok((offset as u64, records))
}

fn wal_checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(unix)]
fn lock_store(file: &File, path: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(format!(
            "failed to lock {}; another server may be using this MLS store",
            path.display()
        ))
    }
}

#[cfg(not(unix))]
fn lock_store(_file: &File, _path: &Path) -> Result<(), String> {
    Ok(())
}

fn serialize_checkpoint(state: &MemoryState) -> Vec<u8> {
    let rooms = state.rooms.values().collect::<Vec<_>>();
    let mut payloads = Vec::new();
    let events = state
        .events
        .iter()
        .map(|(&(room_id, sequence), stored)| {
            let payload_offset = payloads.len() as u64;
            let encoded = jsony::to_binary(&stored.event);
            let payload_len =
                u32::try_from(encoded.len()).expect("MLS event exceeds the binary snapshot limit");
            payloads.extend_from_slice(&encoded);
            EventCheckpoint {
                sequence,
                stored_at_unix_ms: stored.stored_at_unix_ms,
                payload_offset,
                room_id,
                payload_len,
            }
        })
        .collect::<Vec<_>>();
    let cursors = state
        .device_cursors
        .iter()
        .map(|(&(room_id, device_id), cursor)| CursorCheckpoint {
            highest_contiguous_sequence: cursor.highest_contiguous_sequence,
            starting_sequence: cursor.starting_sequence,
            last_ack_unix_ms: cursor.last_ack_unix_ms,
            rejoin_required_through: cursor.rejoin_required_through.unwrap_or(u64::MAX),
            device_id,
            room_id,
            reserved: 0,
        })
        .collect::<Vec<_>>();
    let welcomes = state
        .welcomes
        .values()
        .map(|stored| {
            let payload_offset = payloads.len() as u64;
            let encoded = jsony::to_binary(&stored.bundle);
            let payload_len = u32::try_from(encoded.len())
                .expect("MLS Welcome exceeds the binary snapshot limit");
            payloads.extend_from_slice(&encoded);
            WelcomeCheckpoint {
                delivery_id: stored.delivery_id,
                sequence: stored.sequence,
                stored_at_unix_ms: stored.stored_at_unix_ms,
                payload_offset,
                payload_len,
                reserved: 0,
            }
        })
        .collect::<Vec<_>>();
    let device_welcomes = state
        .device_welcomes
        .iter()
        .map(|&(device_id, delivery_id)| DeviceWelcomeCheckpoint {
            delivery_id,
            device_id,
        })
        .collect::<Vec<_>>();
    let welcome_cursors = state
        .welcome_cursors
        .iter()
        .map(|(&device_id, &delivery_id)| WelcomeCursorCheckpoint {
            delivery_id,
            device_id,
        })
        .collect::<Vec<_>>();

    let mut bytes = jsony::to_binary(&CheckpointEncode {
        global: &state.global,
        rooms,
        events,
        cursors,
        welcomes,
        device_welcomes,
        welcome_cursors,
        payloads: payloads.as_slice(),
    });
    let body_len = bytes.len();
    bytes.reserve(12);
    bytes.resize(body_len + 12, 0);
    bytes.copy_within(0..body_len, 12);
    bytes[..8].copy_from_slice(SNAPSHOT_MAGIC);
    bytes[8..12].copy_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    bytes
}

fn write_checkpoint(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("bin.tmp");
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("failed to remove {}: {error}", tmp.display())),
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)
        .map_err(|error| format!("failed to create {}: {error}", tmp.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("failed to write {}: {error}", tmp.display()))?;
    file.sync_data()
        .map_err(|error| format!("failed to sync {}: {error}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path)
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))?;
    sync_parent(path)
}

fn sync_parent(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("failed to sync MLS directory {}: {error}", parent.display()))
}

fn load_checkpoint(path: &Path) -> Result<MemoryState, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MemoryState {
                global: GlobalRecord {
                    next_welcome_delivery_id: 1,
                    ..GlobalRecord::default()
                },
                ..MemoryState::default()
            });
        }
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    if bytes.len() < 12 || &bytes[..8] != SNAPSHOT_MAGIC {
        return Err("invalid MLS checkpoint header".to_string());
    }
    let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if version != SNAPSHOT_VERSION {
        return Err(format!(
            "unsupported MLS checkpoint version {version}; expected {SNAPSHOT_VERSION}"
        ));
    }
    let decoded = jsony::from_binary::<CheckpointDecode>(&bytes[12..])
        .map_err(|error| format!("failed to decode MLS checkpoint: {error}"))?;
    let CheckpointDecode {
        global,
        rooms,
        events,
        cursors,
        welcomes,
        device_welcomes,
        welcome_cursors,
        payloads,
    } = decoded;
    let mut state = MemoryState {
        global,
        ..MemoryState::default()
    };
    for room in rooms {
        let room_id = room.descriptor.room_id.0;
        if state.rooms.insert(room_id, room).is_some() {
            return Err("MLS checkpoint contains a duplicate room".to_string());
        }
    }
    for row in events {
        let event: MlsDeliveryEvent =
            decode_payload(&payloads, row.payload_offset, row.payload_len, "event")?;
        if event.sequence() != row.sequence {
            return Err("MLS checkpoint event metadata is inconsistent".to_string());
        }
        match &event {
            MlsDeliveryEvent::Application { event_id, .. } => {
                if state
                    .event_ids
                    .insert((row.room_id, event_id.0), row.sequence)
                    .is_some()
                {
                    return Err("MLS checkpoint contains a duplicate event id".to_string());
                }
            }
            MlsDeliveryEvent::Commit { parent_epoch, .. } => {
                if state
                    .commit_epochs
                    .insert((row.room_id, *parent_epoch), row.sequence)
                    .is_some()
                {
                    return Err("MLS checkpoint contains a duplicate commit epoch".to_string());
                }
            }
        }
        if state
            .events
            .insert(
                (row.room_id, row.sequence),
                StoredEvent {
                    event,
                    stored_at_unix_ms: row.stored_at_unix_ms,
                    payload_len: row.payload_len,
                },
            )
            .is_some()
        {
            return Err("MLS checkpoint contains a duplicate delivery event".to_string());
        }
    }
    for row in cursors {
        let cursor = DeviceCursor {
            highest_contiguous_sequence: row.highest_contiguous_sequence,
            starting_sequence: row.starting_sequence,
            last_ack_unix_ms: row.last_ack_unix_ms,
            rejoin_required_through: (row.rejoin_required_through != u64::MAX)
                .then_some(row.rejoin_required_through),
        };
        if state
            .device_cursors
            .insert((row.room_id, row.device_id), cursor)
            .is_some()
        {
            return Err("MLS checkpoint contains a duplicate device cursor".to_string());
        }
    }
    for row in welcomes {
        let bundle: MlsWelcomeBundle =
            decode_payload(&payloads, row.payload_offset, row.payload_len, "Welcome")?;
        if state
            .welcomes
            .insert(
                row.delivery_id,
                StoredWelcome {
                    delivery_id: row.delivery_id,
                    sequence: row.sequence,
                    stored_at_unix_ms: row.stored_at_unix_ms,
                    bundle,
                    payload_len: row.payload_len,
                },
            )
            .is_some()
        {
            return Err("MLS checkpoint contains a duplicate Welcome".to_string());
        }
    }
    for row in device_welcomes {
        if !state
            .device_welcomes
            .insert((row.device_id, row.delivery_id))
        {
            return Err("MLS checkpoint contains a duplicate Welcome target".to_string());
        }
        let count = state
            .welcome_target_counts
            .entry(row.delivery_id)
            .or_default();
        *count = count
            .checked_add(1)
            .ok_or_else(|| "MLS checkpoint Welcome target count is too large".to_string())?;
    }
    for row in welcome_cursors {
        if state
            .welcome_cursors
            .insert(row.device_id, row.delivery_id)
            .is_some()
        {
            return Err("MLS checkpoint contains a duplicate Welcome cursor".to_string());
        }
    }
    Ok(state)
}

fn decode_payload<T: for<'a> jsony::FromBinary<'a>>(
    payloads: &[u8],
    offset: u64,
    len: u32,
    kind: &str,
) -> Result<T, String> {
    let bytes = payload_slice(payloads, offset, len, kind)?;
    jsony::from_binary(bytes)
        .map_err(|error| format!("failed to decode MLS checkpoint {kind}: {error}"))
}

fn payload_slice<'a>(
    payloads: &'a [u8],
    offset: u64,
    len: u32,
    kind: &str,
) -> Result<&'a [u8], String> {
    let start = usize::try_from(offset)
        .map_err(|_| format!("MLS checkpoint {kind} offset is too large"))?;
    let end = start
        .checked_add(len as usize)
        .filter(|end| *end <= payloads.len())
        .ok_or_else(|| format!("MLS checkpoint {kind} payload is out of bounds"))?;
    Ok(&payloads[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::ids::AccountId;

    fn room(room_id: u32, required_devices: Vec<DeviceId>, event_time: u64) -> RoomRecord {
        let descriptor = EncryptedRoomDescriptor::new(
            RoomId(room_id),
            AccountId([1; 32]),
            vec![AccountId([1; 32]), AccountId([2; 32])],
            1,
        )
        .unwrap();
        RoomRecord {
            public_state: PublicGroupState {
                epoch: 1,
                group_id: descriptor.mls_group_id.clone(),
                tree_hash: vec![1],
                transcript_hash: vec![2],
                member_client_ids: Vec::new(),
                snapshot: vec![3],
            },
            descriptor,
            group_info: vec![4],
            revocation_pending: false,
            head_sequence: 1,
            oldest_available_sequence: 1,
            last_event_time_unix_ms: event_time,
            retention_days: 1,
            required_devices,
        }
    }

    fn create(store: &MlsStore, room: &RoomRecord) {
        store
            .create_room(
                &GlobalRecord {
                    next_welcome_delivery_id: 1,
                    ..GlobalRecord::default()
                },
                room,
                &MlsDeliveryEvent::Commit {
                    sequence: 1,
                    parent_epoch: 0,
                    epoch: 1,
                    rosters: Vec::new(),
                    commit: vec![9],
                },
                None,
            )
            .unwrap();
    }

    fn create_with_welcome(store: &MlsStore, room: &RoomRecord, devices: Vec<DeviceId>) {
        let bundle = MlsWelcomeBundle {
            device_ids: devices,
            descriptor: room.descriptor.clone(),
            welcome: vec![8],
        };
        store
            .create_room(
                &GlobalRecord {
                    next_welcome_delivery_id: 2,
                    ..GlobalRecord::default()
                },
                room,
                &MlsDeliveryEvent::Commit {
                    sequence: 1,
                    parent_epoch: 0,
                    epoch: 1,
                    rosters: Vec::new(),
                    commit: vec![9],
                },
                Some((1, &bundle)),
            )
            .unwrap();
    }

    #[test]
    fn schema_version_is_created_and_reopened() {
        let temp = tempfile::tempdir().unwrap();
        let store = MlsStore::open(Some(temp.path())).unwrap();
        drop(store);
        let loaded = MlsStore::open(Some(temp.path())).unwrap().snapshot_state();
        assert!(loaded.rooms.is_empty());
        assert_eq!(loaded.global.next_welcome_delivery_id, 1);
    }

    #[test]
    fn read_handles_share_hot_state() {
        let store = MlsStore::open(None).unwrap();
        let reader = store.read_handle();
        create(&store, &room(1, Vec::new(), 100));
        assert_eq!(reader.events(RoomId(1), 0, 10).unwrap().events.len(), 1);
    }

    #[test]
    fn read_handle_acknowledgement_does_not_touch_wal() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([23; 16]);
        let store = MlsStore::open(Some(temp.path())).unwrap();
        let reader = store.read_handle();
        create(&store, &room(23, vec![device], 100));
        store
            .append_application(
                RoomId(23),
                device,
                1,
                EventId([23; 16]),
                Vec::new(),
                &[1],
                150,
            )
            .unwrap();
        let wal_path = temp.path().join("mls-state.wal");
        let before = fs::metadata(&wal_path).unwrap().len();

        assert!(reader.acknowledge(RoomId(23), device, 2, 200).unwrap());
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), before);

        store.persist_acknowledgement(RoomId(23), device).unwrap();
        assert!(fs::metadata(wal_path).unwrap().len() > before);
    }

    #[test]
    fn ordered_scans_and_application_retry_index_are_room_bounded() {
        let store = MlsStore::open(None).unwrap();
        let device = DeviceId([7; 16]);
        create(&store, &room(10, vec![device], 100));
        create(&store, &room(11, vec![device], 100));
        for (room_id, byte) in [(RoomId(10), 1), (RoomId(10), 2), (RoomId(11), 3)] {
            store
                .append_application(
                    room_id,
                    device,
                    1,
                    EventId([byte; 16]),
                    Vec::new(),
                    &[byte],
                    200,
                )
                .unwrap();
        }
        assert_eq!(
            store
                .events(RoomId(10), 0, 10)
                .unwrap()
                .events
                .iter()
                .map(MlsDeliveryEvent::sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            store
                .application_retry(RoomId(10), EventId([1; 16]), 1, &[1])
                .unwrap(),
            Some(Ok(2))
        );
        assert_eq!(store.events(RoomId(11), 0, 10).unwrap().events.len(), 2);
    }

    #[test]
    fn maximum_fetch_cursors_return_empty_and_sequence_maximum_is_reserved() {
        let store = MlsStore::open(None).unwrap();
        let device = DeviceId([25; 16]);
        create_with_welcome(&store, &room(25, vec![device], 100), vec![device]);

        assert!(store.events(RoomId(25), u64::MAX, 10).unwrap().events.is_empty());
        assert!(store.welcomes(device, u64::MAX).unwrap().is_empty());

        store
            .state
            .write()
            .unwrap()
            .rooms
            .get_mut(&25)
            .unwrap()
            .head_sequence = u64::MAX - 1;
        let error = store
            .append_application(
                RoomId(25),
                device,
                1,
                EventId([25; 16]),
                Vec::new(),
                &[1],
                200,
            )
            .unwrap_err();
        assert!(error.contains("sequence is exhausted"));

        store
            .state
            .write()
            .unwrap()
            .rooms
            .get_mut(&25)
            .unwrap()
            .head_sequence = u64::MAX;
        assert!(validate_memory(&store.state.read().unwrap())
            .unwrap_err()
            .contains("exhausted its delivery sequence"));
    }

    #[test]
    fn welcome_acknowledgement_is_bounded_and_head_survives_cleanup() {
        let mut store = MlsStore::open(None).unwrap();
        let device = DeviceId([26; 16]);
        create_with_welcome(&store, &room(26, vec![device], 100), vec![device]);

        let error = store.acknowledge_welcome(device, 2).unwrap_err();
        assert!(error.contains("beyond the inbox head"));
        assert_eq!(store.welcome_head(device).unwrap(), 1);
        assert!(store.acknowledge_welcome(device, 1).unwrap());
        store.cleanup(100, 10).unwrap();

        assert!(store.welcomes(device, 0).unwrap().is_empty());
        assert_eq!(store.welcome_head(device).unwrap(), 1);
        assert!(!store.acknowledge_welcome(device, 1).unwrap());
    }

    #[test]
    fn final_checkpoint_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([8; 16]);
        let store = MlsStore::open(Some(temp.path())).unwrap();
        create(&store, &room(14, vec![device], 100));
        store
            .append_application(
                RoomId(14),
                device,
                1,
                EventId([1; 16]),
                Vec::new(),
                &[1],
                200,
            )
            .unwrap();
        store.acknowledge(RoomId(14), device, 2, 300).unwrap();
        drop(store);
        let reopened = MlsStore::open(Some(temp.path())).unwrap();
        assert_eq!(reopened.events(RoomId(14), 0, 10).unwrap().events.len(), 2);
    }

    #[test]
    fn empty_retained_room_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([24; 16]);
        let mut store = MlsStore::open(Some(temp.path())).unwrap();
        create(&store, &room(24, vec![device], 100));
        store.acknowledge(RoomId(24), device, 1, 200).unwrap();
        let report = store.cleanup(200, 10).unwrap();
        assert_eq!(report.deleted_events, 1);
        drop(store);

        let reopened = MlsStore::open(Some(temp.path())).unwrap();
        assert!(reopened.events(RoomId(24), 0, 10).unwrap().events.is_empty());
    }

    #[test]
    fn partially_acknowledged_welcome_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let acknowledged = DeviceId([27; 16]);
        let pending = DeviceId([28; 16]);
        let mut store = MlsStore::open(Some(temp.path())).unwrap();
        create_with_welcome(
            &store,
            &room(27, vec![acknowledged, pending], 100),
            vec![acknowledged, pending],
        );
        store.acknowledge_welcome(acknowledged, 1).unwrap();
        store
            .persist_welcome_acknowledgement(acknowledged)
            .unwrap();
        store.cleanup(100, 10).unwrap();
        drop(store);

        let reopened = MlsStore::open(Some(temp.path())).unwrap();
        assert!(reopened.welcomes(acknowledged, 0).unwrap().is_empty());
        assert_eq!(reopened.welcome_head(acknowledged).unwrap(), 1);
        assert_eq!(reopened.welcomes(pending, 0).unwrap().len(), 1);
    }

    #[test]
    fn startup_rejects_orphaned_welcome_target() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([29; 16]);
        let store = MlsStore::open(None).unwrap();
        create_with_welcome(&store, &room(29, vec![device], 100), vec![device]);
        let mut state = store.state.read().unwrap().clone();
        state.welcomes.clear();
        write_checkpoint(
            &temp.path().join("mls-state.bin"),
            &serialize_checkpoint(&state),
        )
        .unwrap();

        let error = MlsStore::open(Some(temp.path())).unwrap_err();
        assert!(error.contains("target references a missing value"));
    }

    #[test]
    fn validation_rejects_cross_index_inconsistencies() {
        let device = DeviceId([30; 16]);
        let store = MlsStore::open(None).unwrap();
        create_with_welcome(&store, &room(30, vec![device], 100), vec![device]);
        let valid = store.state.read().unwrap().clone();
        validate_memory(&valid).unwrap();

        let mut orphan_event = valid.clone();
        orphan_event.rooms.clear();
        assert!(validate_memory(&orphan_event)
            .unwrap_err()
            .contains("event references a missing room"));

        let mut orphan_index = valid.clone();
        orphan_index.event_ids.insert((30, [7; 16]), 1);
        assert!(validate_memory(&orphan_index)
            .unwrap_err()
            .contains("indexes contain orphan entries"));

        let mut orphan_cursor = valid.clone();
        orphan_cursor.device_cursors.insert(
            (31, device.0),
            DeviceCursor {
                highest_contiguous_sequence: 1,
                starting_sequence: 1,
                last_ack_unix_ms: 100,
                rejoin_required_through: None,
            },
        );
        assert!(validate_memory(&orphan_cursor)
            .unwrap_err()
            .contains("cursor references a missing room"));

        let mut missing_welcome_room = valid;
        missing_welcome_room.rooms.clear();
        missing_welcome_room.events.clear();
        missing_welcome_room.commit_epochs.clear();
        missing_welcome_room.device_cursors.clear();
        assert!(validate_memory(&missing_welcome_room)
            .unwrap_err()
            .contains("Welcome references a missing room"));
    }

    #[test]
    fn authoritative_mutations_replay_from_wal_without_a_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([18; 16]);
        let store = MlsStore::open(Some(temp.path())).unwrap();
        create(&store, &room(16, vec![device], 100));
        store
            .append_application(
                RoomId(16),
                device,
                1,
                EventId([1; 16]),
                Vec::new(),
                &[7; 32],
                200,
            )
            .unwrap();
        assert!(!temp.path().join("mls-state.bin").exists());
        assert!(
            fs::metadata(temp.path().join("mls-state.wal"))
                .unwrap()
                .len()
                > WAL_HEADER_BYTES
        );
        drop(store);

        let reopened = MlsStore::open(Some(temp.path())).unwrap();
        assert_eq!(reopened.events(RoomId(16), 0, 10).unwrap().events.len(), 2);
    }

    #[test]
    fn torn_wal_tail_is_truncated_after_replaying_whole_records() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([19; 16]);
        let store = MlsStore::open(Some(temp.path())).unwrap();
        create(&store, &room(17, vec![device], 100));
        drop(store);
        let path = temp.path().join("mls-state.wal");
        let valid_len = fs::metadata(&path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[4, 3, 2, 1, 0]).unwrap();
        file.sync_data().unwrap();
        drop(file);

        let reopened = MlsStore::open(Some(temp.path())).unwrap();
        assert_eq!(reopened.events(RoomId(17), 0, 10).unwrap().events.len(), 1);
        assert_eq!(fs::metadata(path).unwrap().len(), valid_len);
    }

    #[test]
    fn unrepaired_wal_failure_blocks_later_mutations() {
        let temp = tempfile::tempdir().unwrap();
        let store = MlsStore::open(Some(temp.path())).unwrap();
        {
            let mut persistence = store.persistence.as_ref().unwrap().lock().unwrap();
            let wal_path = persistence.wal_path.clone();
            persistence.wal = File::open(wal_path).unwrap();
        }

        let global = GlobalRecord {
            next_welcome_delivery_id: 2,
            ..GlobalRecord::default()
        };
        assert!(store.replace_global(&global).is_err());
        let error = store.replace_global(&global).unwrap_err();
        assert!(error.contains("earlier write failure"));
        assert_eq!(store.snapshot_state().global.next_welcome_delivery_id, 1);
    }

    #[test]
    fn store_excludes_a_second_writer_for_the_same_directory() {
        let temp = tempfile::tempdir().unwrap();
        let first = MlsStore::open(Some(temp.path())).unwrap();
        let error = MlsStore::open(Some(temp.path())).unwrap_err();
        assert!(error.contains("another server"));
        drop(first);
        MlsStore::open(Some(temp.path())).unwrap();
    }

    #[test]
    fn background_snapshot_preserves_active_wal_records_during_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([20; 16]);
        let store = MlsStore::open(Some(temp.path())).unwrap();
        create(&store, &room(18, vec![device], 100));
        store
            .persistence
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .seal()
            .unwrap();
        store
            .append_application(
                RoomId(18),
                device,
                1,
                EventId([2; 16]),
                Vec::new(),
                &[9; 32],
                200,
            )
            .unwrap();
        let writer = store.snapshot_writer.as_ref().unwrap();
        writer.request().unwrap();
        writer.flush().unwrap();
        assert!(temp.path().join("mls-state.bin").exists());
        assert!(!temp.path().join("mls-state.wal.sealed").exists());
        drop(store);

        let reopened = MlsStore::open(Some(temp.path())).unwrap();
        assert_eq!(reopened.events(RoomId(18), 0, 10).unwrap().events.len(), 2);
    }

    #[test]
    fn wal_threshold_rotates_and_snapshots_in_background() {
        let temp = tempfile::tempdir().unwrap();
        let store = MlsStore::open(Some(temp.path())).unwrap();
        create(&store, &room(22, Vec::new(), 100));
        store
            .persistence
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .wal_bytes = WAL_CHECKPOINT_BYTES;

        assert!(store.checkpoint_if_needed().unwrap());
        store.snapshot_writer.as_ref().unwrap().flush().unwrap();
        assert!(temp.path().join("mls-state.bin").exists());
        assert!(!temp.path().join("mls-state.wal.sealed").exists());
    }

    #[test]
    fn payload_compaction_clears_retired_byte_accounting() {
        let mut store = MlsStore::open(None).unwrap();
        let device = DeviceId([8; 16]);
        create(&store, &room(15, vec![device], 100));
        store
            .append_application(
                RoomId(15),
                device,
                1,
                EventId([1; 16]),
                Vec::new(),
                &[1; 1024],
                200,
            )
            .unwrap();
        store.acknowledge(RoomId(15), device, 2, 300).unwrap();
        store.cleanup(300, 10).unwrap();

        let before = store.state.read().unwrap().retired_payload_bytes;
        assert!(before > 0);
        reset_fragmentation_accounting(&mut store.state.write().unwrap());
        let state = store.state.read().unwrap();
        assert!(state.events.is_empty());
        assert_eq!(state.retired_payload_bytes, 0);
    }

    #[test]
    fn durable_compaction_shrinks_pruned_payloads_and_resets_fragmentation() {
        let temp = tempfile::tempdir().unwrap();
        let device = DeviceId([21; 16]);
        let mut store = MlsStore::open(Some(temp.path())).unwrap();
        create(&store, &room(19, vec![device], 100));
        store
            .append_application(
                RoomId(19),
                device,
                1,
                EventId([3; 16]),
                Vec::new(),
                &[5; 1024 * 1024],
                200,
            )
            .unwrap();
        store.compact().unwrap();
        let before = store.status().unwrap().file_bytes.unwrap();
        store.acknowledge(RoomId(19), device, 2, 300).unwrap();
        store.cleanup(300, 10).unwrap();
        assert!(store.status().unwrap().fragmented_bytes >= 1024 * 1024);

        store.compact().unwrap();
        let status = store.status().unwrap();
        assert_eq!(status.fragmented_bytes, 0);
        assert!(status.file_bytes.unwrap() < before / 2);
    }

    #[test]
    fn acknowledgement_prunes_in_bounded_contiguous_passes_and_drops_retry_index() {
        let mut store = MlsStore::open(None).unwrap();
        let device = DeviceId([8; 16]);
        create(&store, &room(20, vec![device], 100));
        for byte in [1, 2] {
            store
                .append_application(
                    RoomId(20),
                    device,
                    1,
                    EventId([byte; 16]),
                    Vec::new(),
                    &[byte],
                    200,
                )
                .unwrap();
        }
        store.acknowledge(RoomId(20), device, 3, 300).unwrap();
        let first = store.cleanup(300, 2).unwrap();
        assert_eq!(first.deleted_events, 2);
        assert!(first.more_work);
        let second = store.cleanup(300, 2).unwrap();
        assert_eq!(second.deleted_events, 1);
        assert!(second.more_work);
        let final_pass = store.cleanup(300, 2).unwrap();
        assert!(!final_pass.more_work);
        assert!(store.events(RoomId(20), 0, 10).unwrap().events.is_empty());
        assert_eq!(
            store
                .application_retry(RoomId(20), EventId([1; 16]), 1, &[1])
                .unwrap(),
            None
        );
    }

    #[test]
    fn cleanup_drains_multiple_rooms_before_reporting_idle() {
        let mut store = MlsStore::open(None).unwrap();
        create(&store, &room(40, Vec::new(), 100));
        create(&store, &room(41, Vec::new(), 100));

        let first = store.cleanup(100, 10).unwrap();
        assert_eq!(first.deleted_events, 1);
        assert!(first.more_work);
        let second = store.cleanup(100, 10).unwrap();
        assert_eq!(second.deleted_events, 1);
        assert!(second.more_work);
        let final_pass = store.cleanup(100, 10).unwrap();
        assert_eq!(final_pass.deleted_events, 0);
        assert!(!final_pass.more_work);
    }

    #[test]
    fn backward_clock_does_not_make_newer_event_expire_before_its_prefix() {
        let mut store = MlsStore::open(None).unwrap();
        let device = DeviceId([9; 16]);
        create(&store, &room(30, vec![device], 200));
        store
            .append_application(
                RoomId(30),
                device,
                1,
                EventId([1; 16]),
                Vec::new(),
                &[1],
                100,
            )
            .unwrap();
        let day = 24 * 60 * 60 * 1000;
        let early = store.cleanup(day + 150, 10).unwrap();
        assert_eq!(early.deleted_events, 1);
        assert_eq!(early.lagging_devices, 0);
        assert!(!store.requires_rejoin(RoomId(30), device).unwrap());
        let expired = store.cleanup(day + 201, 10).unwrap();
        assert_eq!(expired.deleted_events, 1);
        assert_eq!(expired.lagging_devices, 1);
        assert!(store.requires_rejoin(RoomId(30), device).unwrap());
    }
}
