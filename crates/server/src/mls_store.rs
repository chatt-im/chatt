//! Durable MLS delivery storage.
//!
//! This is deliberately the only server module that knows about redb.  The
//! service above it deals in typed records and transactional operations, so a
//! different ordered KV implementation can replace this module without
//! leaking database keys or transactions into protocol and policy code.

use std::{fs, path::{Path, PathBuf}};

use chatt_mls::PublicGroupState;
use jsony::Jsony;
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, TableDefinition,
    backends::InMemoryBackend,
};
use rpc::{
    identity::SignedDeviceRoster,
    ids::{DeviceId, EventId, RoomId, UserId},
    mls::{EncryptedRoomDescriptor, MlsDeliveryEvent, MlsWelcome, MlsWelcomeBundle},
};

const SCHEMA_VERSION: u32 = 1;
const SCHEMA_KEY: &str = "schema-version";

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const GLOBAL: TableDefinition<u8, &[u8]> = TableDefinition::new("global");
const ROOMS: TableDefinition<u32, &[u8]> = TableDefinition::new("rooms");
const EVENTS: TableDefinition<(u32, u64), &[u8]> = TableDefinition::new("events");
const EVENT_IDS: TableDefinition<(u32, [u8; 16]), u64> = TableDefinition::new("event_ids");
const COMMIT_EPOCHS: TableDefinition<(u32, u64), u64> =
    TableDefinition::new("commit_epochs");
const DEVICE_CURSORS: TableDefinition<(u32, [u8; 16]), &[u8]> =
    TableDefinition::new("device_cursors");
const WELCOMES: TableDefinition<u64, &[u8]> = TableDefinition::new("welcomes");
const WELCOME_TARGET_COUNTS: TableDefinition<u64, u16> =
    TableDefinition::new("welcome_target_counts");
const DEVICE_WELCOMES: TableDefinition<([u8; 16], u64), ()> =
    TableDefinition::new("device_welcomes");
const WELCOME_CURSORS: TableDefinition<[u8; 16], u64> =
    TableDefinition::new("welcome_cursors");

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

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
struct StoredEvent {
    event: MlsDeliveryEvent,
    stored_at_unix_ms: u64,
}

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
struct StoredWelcome {
    delivery_id: u64,
    sequence: u64,
    stored_at_unix_ms: u64,
    bundle: MlsWelcomeBundle,
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
    Stored { sequence: u64, room: RoomRecord },
    AlreadyStored { sequence: u64 },
    Conflict,
    StaleEpoch { current_epoch: u64 },
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

pub(super) struct MlsStore {
    database: Database,
    path: Option<PathBuf>,
    cleanup_after_room: u32,
}

impl std::fmt::Debug for MlsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MlsStore").field("path", &self.path).finish()
    }
}

impl MlsStore {
    pub fn open(data_dir: Option<&Path>) -> Result<Self, String> {
        let (database, path) = match data_dir {
            Some(data_dir) => {
                fs::create_dir_all(data_dir).map_err(|error| {
                    format!("failed to create {}: {error}", data_dir.display())
                })?;
                let path = data_dir.join("mls.redb");
                let database = Database::create(&path)
                    .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
                (database, Some(path))
            }
            None => {
                let database = Database::builder()
                    .create_with_backend(InMemoryBackend::new())
                    .map_err(|error| format!("failed to create in-memory MLS database: {error}"))?;
                (database, None)
            }
        };
        let store = Self {
            database,
            path,
            cleanup_after_room: 0,
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), String> {
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        {
            let mut meta = tx.open_table(META).map_err(db_error)?;
            let existing = meta
                .get(SCHEMA_KEY)
                .map_err(db_error)?
                .map(|value| value.value().to_vec());
            match existing {
                Some(value) => {
                    let bytes: [u8; 4] = value.as_slice().try_into().map_err(|_| {
                        "MLS database schema version has an invalid length".to_string()
                    })?;
                    let version = u32::from_be_bytes(bytes);
                    if version != SCHEMA_VERSION {
                        return Err(format!(
                            "unsupported MLS database schema version {version}; expected {SCHEMA_VERSION}"
                        ));
                    }
                }
                None => {
                    meta.insert(SCHEMA_KEY, SCHEMA_VERSION.to_be_bytes().as_slice())
                        .map_err(db_error)?;
                }
            }
            tx.open_table(GLOBAL).map_err(db_error)?;
            tx.open_table(ROOMS).map_err(db_error)?;
            tx.open_table(EVENTS).map_err(db_error)?;
            tx.open_table(EVENT_IDS).map_err(db_error)?;
            tx.open_table(COMMIT_EPOCHS).map_err(db_error)?;
            tx.open_table(DEVICE_CURSORS).map_err(db_error)?;
            tx.open_table(WELCOMES).map_err(db_error)?;
            tx.open_table(WELCOME_TARGET_COUNTS).map_err(db_error)?;
            tx.open_table(DEVICE_WELCOMES).map_err(db_error)?;
            tx.open_table(WELCOME_CURSORS).map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }

    pub fn load(&self) -> Result<LoadedState, String> {
        let tx = self.database.begin_read().map_err(db_error)?;
        let global = {
            let table = tx.open_table(GLOBAL).map_err(db_error)?;
            match table.get(0).map_err(db_error)? {
                Some(value) => decode_global(value.value())?,
                None => GlobalRecord {
                    next_welcome_delivery_id: 1,
                    ..GlobalRecord::default()
                },
            }
        };
        let rooms = {
            let table = tx.open_table(ROOMS).map_err(db_error)?;
            table
                .iter()
                .map_err(db_error)?
                .map(|entry| {
                    let (_, value) = entry.map_err(db_error)?;
                    decode_room(value.value())
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        self.validate(&tx, &rooms)?;
        Ok(LoadedState { global, rooms })
    }

    pub fn replace_global_and_rooms(
        &self,
        global: &GlobalRecord,
        rooms: &[RoomRecord],
    ) -> Result<(), String> {
        let global_bytes = jsony::to_binary(global);
        let room_bytes = rooms
            .iter()
            .map(|room| (room.descriptor.room_id.0, jsony::to_binary(room)))
            .collect::<Vec<_>>();
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        {
            tx.open_table(GLOBAL)
                .map_err(db_error)?
                .insert(0, global_bytes.as_slice())
                .map_err(db_error)?;
            let mut table = tx.open_table(ROOMS).map_err(db_error)?;
            for (room_id, bytes) in room_bytes {
                let previous = table
                    .get(room_id)
                    .map_err(db_error)?
                    .map(|value| decode_room(value.value()))
                    .transpose()?;
                if let Some(previous) = previous {
                    let room = rooms
                        .iter()
                        .find(|room| room.descriptor.room_id.0 == room_id)
                        .expect("room bytes were built from the same slice");
                    reconcile_cursors(
                        &tx,
                        RoomId(room_id),
                        &previous.required_devices,
                        room,
                        room.head_sequence,
                    )?;
                }
                table.insert(room_id, bytes.as_slice()).map_err(db_error)?;
            }
        }
        tx.commit().map_err(db_error)
    }

    pub fn create_room(
        &self,
        global: &GlobalRecord,
        room: &RoomRecord,
        event: &MlsDeliveryEvent,
        welcome: Option<(u64, &MlsWelcomeBundle)>,
    ) -> Result<(), String> {
        let global_bytes = jsony::to_binary(global);
        let room_bytes = jsony::to_binary(room);
        let stored = StoredEvent {
            event: event.clone(),
            stored_at_unix_ms: room.last_event_time_unix_ms,
        };
        let event_bytes = jsony::to_binary(&stored);
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        {
            let mut rooms = tx.open_table(ROOMS).map_err(db_error)?;
            if rooms.get(room.descriptor.room_id.0).map_err(db_error)?.is_some() {
                return Err("encrypted room already exists in MLS storage".to_string());
            }
            rooms
                .insert(room.descriptor.room_id.0, room_bytes.as_slice())
                .map_err(db_error)?;
            insert_event(&tx, room.descriptor.room_id, event, &event_bytes)?;
            reconcile_cursors(&tx, room.descriptor.room_id, &[], room, room.head_sequence)?;
            insert_welcome(
                &tx,
                welcome,
                room.head_sequence,
                room.last_event_time_unix_ms,
            )?;
            tx.open_table(GLOBAL)
                .map_err(db_error)?
                .insert(0, global_bytes.as_slice())
                .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }

    pub fn append_commit(
        &self,
        global: &GlobalRecord,
        previous: &RoomRecord,
        room: &RoomRecord,
        event: &MlsDeliveryEvent,
        welcome: Option<(u64, &MlsWelcomeBundle)>,
    ) -> Result<(), String> {
        let global_bytes = jsony::to_binary(global);
        let room_bytes = jsony::to_binary(room);
        let event_bytes = jsony::to_binary(&StoredEvent {
            event: event.clone(),
            stored_at_unix_ms: room.last_event_time_unix_ms,
        });
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        {
            let mut rooms = tx.open_table(ROOMS).map_err(db_error)?;
            let durable = {
                let value = rooms
                    .get(room.descriptor.room_id.0)
                    .map_err(db_error)?
                    .ok_or_else(|| "encrypted room does not exist in MLS storage".to_string())?;
                decode_room(value.value())?
            };
            if durable.public_state.epoch != previous.public_state.epoch
                || durable.head_sequence != previous.head_sequence
            {
                return Err("MLS room changed during commit validation".to_string());
            }
            rooms
                .insert(room.descriptor.room_id.0, room_bytes.as_slice())
                .map_err(db_error)?;
            insert_event(&tx, room.descriptor.room_id, event, &event_bytes)?;
            reconcile_cursors(
                &tx,
                room.descriptor.room_id,
                &previous.required_devices,
                room,
                room.head_sequence,
            )?;
            insert_welcome(
                &tx,
                welcome,
                room.head_sequence,
                room.last_event_time_unix_ms,
            )?;
            tx.open_table(GLOBAL)
                .map_err(db_error)?
                .insert(0, global_bytes.as_slice())
                .map_err(db_error)?;
        }
        tx.commit().map_err(db_error)
    }

    pub fn find_commit(
        &self,
        room_id: RoomId,
        parent_epoch: u64,
        commit: &[u8],
    ) -> Result<Option<(u64, u64)>, String> {
        let tx = self.database.begin_read().map_err(db_error)?;
        let index = tx.open_table(COMMIT_EPOCHS).map_err(db_error)?;
        let Some(sequence) = index
            .get((room_id.0, parent_epoch))
            .map_err(db_error)?
            .map(|value| value.value())
        else {
            return Ok(None);
        };
        let events = tx.open_table(EVENTS).map_err(db_error)?;
        let Some(value) = events.get((room_id.0, sequence)).map_err(db_error)? else {
            return Err("MLS commit index references a missing event".to_string());
        };
        let stored = decode_event(value.value())?;
        match stored.event {
            MlsDeliveryEvent::Commit {
                epoch,
                commit: stored,
                ..
            } if stored == commit => Ok(Some((sequence, epoch))),
            _ => Ok(None),
        }
    }

    pub fn append_application(
        &self,
        room_id: RoomId,
        device_id: DeviceId,
        expected_epoch: u64,
        event_id: EventId,
        ciphertext: &[u8],
        now_unix_ms: u64,
    ) -> Result<AppendApplicationResult, String> {
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        let result = {
            let existing = {
                let event_ids = tx.open_table(EVENT_IDS).map_err(db_error)?;
                event_ids
                    .get((room_id.0, event_id.0))
                    .map_err(db_error)?
                    .map(|value| value.value())
            };
            if let Some(sequence) = existing {
                let events = tx.open_table(EVENTS).map_err(db_error)?;
                let value = events
                    .get((room_id.0, sequence))
                    .map_err(db_error)?
                    .ok_or_else(|| "MLS event id references a missing event".to_string())?;
                let stored = decode_event(value.value())?;
                match stored.event {
                    MlsDeliveryEvent::Application {
                        epoch,
                        event_id: stored_id,
                        ciphertext: stored_ciphertext,
                        ..
                    } if epoch == expected_epoch
                        && stored_id == event_id
                        && stored_ciphertext == ciphertext =>
                    {
                        AppendApplicationResult::AlreadyStored { sequence }
                    }
                    _ => AppendApplicationResult::Conflict,
                }
            } else {
                let cursors = tx.open_table(DEVICE_CURSORS).map_err(db_error)?;
                if cursors
                    .get((room_id.0, device_id.0))
                    .map_err(db_error)?
                    .map(|value| decode_cursor(value.value()))
                    .transpose()?
                    .is_some_and(|cursor| cursor.rejoin_required_through.is_some())
                {
                    AppendApplicationResult::RejoinRequired
                } else {
                    let mut rooms = tx.open_table(ROOMS).map_err(db_error)?;
                    let mut room = {
                        let value = rooms
                            .get(room_id.0)
                            .map_err(db_error)?
                            .ok_or_else(|| "encrypted room does not exist".to_string())?;
                        decode_room(value.value())?
                    };
                    if room.revocation_pending {
                        AppendApplicationResult::RevocationPending
                    } else if room.public_state.epoch != expected_epoch {
                        AppendApplicationResult::StaleEpoch {
                            current_epoch: room.public_state.epoch,
                        }
                    } else {
                        let sequence = room
                            .head_sequence
                            .checked_add(1)
                            .ok_or_else(|| "MLS delivery sequence is exhausted".to_string())?;
                        let stored_at = now_unix_ms.max(room.last_event_time_unix_ms);
                        let event = MlsDeliveryEvent::Application {
                            sequence,
                            epoch: expected_epoch,
                            event_id,
                            ciphertext: ciphertext.to_vec(),
                        };
                        let event_bytes = jsony::to_binary(&StoredEvent {
                            event: event.clone(),
                            stored_at_unix_ms: stored_at,
                        });
                        insert_event(&tx, room_id, &event, &event_bytes)?;
                        room.head_sequence = sequence;
                        room.last_event_time_unix_ms = stored_at;
                        let room_bytes = jsony::to_binary(&room);
                        rooms
                            .insert(room_id.0, room_bytes.as_slice())
                            .map_err(db_error)?;
                        AppendApplicationResult::Stored { sequence, room }
                    }
                }
            }
        };
        match result {
            AppendApplicationResult::Stored { .. } => tx.commit().map_err(db_error)?,
            _ => tx.abort().map_err(db_error)?,
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
        let tx = self.database.begin_read().map_err(db_error)?;
        let ids = tx.open_table(EVENT_IDS).map_err(db_error)?;
        let Some(sequence) = ids
            .get((room_id.0, event_id.0))
            .map_err(db_error)?
            .map(|value| value.value())
        else {
            return Ok(None);
        };
        let events = tx.open_table(EVENTS).map_err(db_error)?;
        let value = events
            .get((room_id.0, sequence))
            .map_err(db_error)?
            .ok_or_else(|| "MLS event id references a missing event".to_string())?;
        let stored = decode_event(value.value())?;
        let exact = matches!(
            stored.event,
            MlsDeliveryEvent::Application {
                epoch: stored_epoch,
                event_id: stored_id,
                ciphertext: stored_ciphertext,
                ..
            } if stored_epoch == epoch && stored_id == event_id && stored_ciphertext == ciphertext
        );
        Ok(Some(if exact { Ok(sequence) } else { Err(()) }))
    }

    pub fn events(&self, room_id: RoomId, after: u64, limit: usize) -> Result<EventBatch, String> {
        let tx = self.database.begin_read().map_err(db_error)?;
        let rooms = tx.open_table(ROOMS).map_err(db_error)?;
        let room = rooms
            .get(room_id.0)
            .map_err(db_error)?
            .ok_or_else(|| "encrypted room does not exist".to_string())?;
        let room = decode_room(room.value())?;
        let start = after.saturating_add(1).max(room.oldest_available_sequence);
        let events = tx.open_table(EVENTS).map_err(db_error)?;
        let values = events
            .range((room_id.0, start)..=(room_id.0, u64::MAX))
            .map_err(db_error)?
            .take(limit)
            .map(|entry| {
                let (_, value) = entry.map_err(db_error)?;
                decode_event(value.value()).map(|stored| stored.event)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EventBatch {
            events: values,
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
    ) -> Result<(), String> {
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        let changed = {
            let rooms = tx.open_table(ROOMS).map_err(db_error)?;
            let room = rooms
                .get(room_id.0)
                .map_err(db_error)?
                .ok_or_else(|| "encrypted room does not exist".to_string())?;
            let room = decode_room(room.value())?;
            if sequence > room.head_sequence {
                return Err("MLS acknowledgement is beyond the room head".to_string());
            }
            let mut cursors = tx.open_table(DEVICE_CURSORS).map_err(db_error)?;
            let mut cursor = {
                let value = cursors
                    .get((room_id.0, device_id.0))
                    .map_err(db_error)?
                    .ok_or_else(|| "MLS device has no cursor for this room".to_string())?;
                decode_cursor(value.value())?
            };
            if sequence < cursor.starting_sequence {
                return Err("MLS acknowledgement predates the device's room membership".to_string());
            }
            if sequence <= cursor.highest_contiguous_sequence {
                false
            } else {
                cursor.highest_contiguous_sequence = sequence;
                cursor.last_ack_unix_ms = now_unix_ms;
                if cursor
                    .rejoin_required_through
                    .is_some_and(|watermark| sequence > watermark)
                {
                    cursor.rejoin_required_through = None;
                }
                let bytes = jsony::to_binary(&cursor);
                cursors
                    .insert((room_id.0, device_id.0), bytes.as_slice())
                    .map_err(db_error)?;
                true
            }
        };
        if changed {
            tx.commit().map_err(db_error)
        } else {
            tx.abort().map_err(db_error)
        }
    }

    pub fn requires_rejoin(&self, room_id: RoomId, device_id: DeviceId) -> Result<bool, String> {
        let tx = self.database.begin_read().map_err(db_error)?;
        let cursors = tx.open_table(DEVICE_CURSORS).map_err(db_error)?;
        Ok(cursors
            .get((room_id.0, device_id.0))
            .map_err(db_error)?
            .map(|value| decode_cursor(value.value()))
            .transpose()?
            .is_some_and(|cursor| cursor.rejoin_required_through.is_some()))
    }

    pub fn welcomes(&self, device_id: DeviceId, after: u64) -> Result<Vec<MlsWelcome>, String> {
        let tx = self.database.begin_read().map_err(db_error)?;
        let targets = tx.open_table(DEVICE_WELCOMES).map_err(db_error)?;
        let welcomes = tx.open_table(WELCOMES).map_err(db_error)?;
        targets
            .range((device_id.0, after.saturating_add(1))..=(device_id.0, u64::MAX))
            .map_err(db_error)?
            .map(|entry| {
                let (key, _) = entry.map_err(db_error)?;
                let delivery_id = key.value().1;
                let value = welcomes
                    .get(delivery_id)
                    .map_err(db_error)?
                    .ok_or_else(|| "MLS Welcome target references a missing value".to_string())?;
                let stored = decode_welcome(value.value())?;
                Ok(MlsWelcome {
                    delivery_id,
                    sequence: stored.sequence,
                    device_id,
                    descriptor: stored.bundle.descriptor,
                    welcome: stored.bundle.welcome,
                })
            })
            .collect()
    }

    pub fn welcome_head(&self, device_id: DeviceId) -> Result<u64, String> {
        let tx = self.database.begin_read().map_err(db_error)?;
        let targets = tx.open_table(DEVICE_WELCOMES).map_err(db_error)?;
        let mut range = targets
            .range((device_id.0, 0)..=(device_id.0, u64::MAX))
            .map_err(db_error)?;
        Ok(range
            .next_back()
            .transpose()
            .map_err(db_error)?
            .map_or(0, |(key, _)| key.value().1))
    }

    pub fn acknowledge_welcome(&self, device_id: DeviceId, delivery_id: u64) -> Result<(), String> {
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        let changed = {
            let mut cursors = tx.open_table(WELCOME_CURSORS).map_err(db_error)?;
            let current = cursors
                .get(device_id.0)
                .map_err(db_error)?
                .map_or(0, |value| value.value());
            if delivery_id <= current {
                false
            } else {
                cursors.insert(device_id.0, delivery_id).map_err(db_error)?;
                true
            }
        };
        if changed {
            tx.commit().map_err(db_error)
        } else {
            tx.abort().map_err(db_error)
        }
    }

    pub fn cleanup(&mut self, now_unix_ms: u64, batch_limit: usize) -> Result<CleanupReport, String> {
        if batch_limit == 0 {
            return Ok(CleanupReport::default());
        }
        let loaded = self.load()?;
        let mut rooms = loaded.rooms;
        rooms.sort_by_key(|room| room.descriptor.room_id.0);
        let ordered = rooms
            .iter()
            .filter(|room| room.descriptor.room_id.0 > self.cleanup_after_room)
            .chain(rooms.iter().filter(|room| room.descriptor.room_id.0 <= self.cleanup_after_room));
        for room in ordered {
            if let Some(report) = self.prune_room(room.descriptor.room_id, now_unix_ms, batch_limit)? {
                self.cleanup_after_room = room.descriptor.room_id.0;
                return Ok(report);
            }
        }
        let more_work = self.cleanup_welcomes_only(now_unix_ms, batch_limit)?;
        Ok(CleanupReport {
            more_work,
            ..CleanupReport::default()
        })
    }

    fn cleanup_welcomes_only(
        &self,
        now_unix_ms: u64,
        batch_limit: usize,
    ) -> Result<bool, String> {
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        let more = cleanup_welcomes(&tx, now_unix_ms, batch_limit)?;
        tx.commit().map_err(db_error)?;
        Ok(more)
    }

    fn prune_room(
        &self,
        room_id: RoomId,
        now_unix_ms: u64,
        batch_limit: usize,
    ) -> Result<Option<CleanupReport>, String> {
        let mut tx = self.database.begin_write().map_err(db_error)?;
        tx.set_durability(Durability::Immediate).map_err(db_error)?;
        let mut report = CleanupReport::default();
        {
            let mut rooms = tx.open_table(ROOMS).map_err(db_error)?;
            let Some(mut room) = rooms
                .get(room_id.0)
                .map_err(db_error)?
                .map(|value| decode_room(value.value()))
                .transpose()?
            else {
                return Ok(None);
            };
            if room.oldest_available_sequence > room.head_sequence {
                return Ok(None);
            }
            let mut cursors = tx.open_table(DEVICE_CURSORS).map_err(db_error)?;
            let mut required = Vec::new();
            for device in &room.required_devices {
                let value = cursors
                    .get((room_id.0, device.0))
                    .map_err(db_error)?
                    .ok_or_else(|| "required MLS device is missing its cursor".to_string())?;
                required.push((*device, decode_cursor(value.value())?));
            }
            let safe = required
                .iter()
                .map(|(_, cursor)| cursor.highest_contiguous_sequence)
                .min()
                .unwrap_or(room.head_sequence);
            let retention_ms = u64::from(room.retention_days)
                .saturating_mul(24 * 60 * 60 * 1000);
            let cutoff = now_unix_ms.saturating_sub(retention_ms);
            let mut expiry = room.oldest_available_sequence.saturating_sub(1);
            let events = tx.open_table(EVENTS).map_err(db_error)?;
            for entry in events
                .range((room_id.0, room.oldest_available_sequence)..=(room_id.0, room.head_sequence))
                .map_err(db_error)?
            {
                let (_, value) = entry.map_err(db_error)?;
                let stored = decode_event(value.value())?;
                if stored.stored_at_unix_ms >= cutoff {
                    break;
                }
                expiry = stored.event.sequence();
            }
            let prune_through = safe.max(expiry).min(room.head_sequence);
            if prune_through < room.oldest_available_sequence {
                return Ok(None);
            }
            if expiry > safe {
                for (device, mut cursor) in required {
                    if cursor.highest_contiguous_sequence < expiry {
                        cursor.rejoin_required_through = Some(expiry);
                        let bytes = jsony::to_binary(&cursor);
                        cursors
                            .insert((room_id.0, device.0), bytes.as_slice())
                            .map_err(db_error)?;
                        report.lagging_devices += 1;
                    }
                }
            }
            drop(events);
            let mut events = tx.open_table(EVENTS).map_err(db_error)?;
            let mut event_ids = tx.open_table(EVENT_IDS).map_err(db_error)?;
            let mut commit_epochs = tx.open_table(COMMIT_EPOCHS).map_err(db_error)?;
            let end = prune_through.min(
                room.oldest_available_sequence
                    .saturating_add(batch_limit as u64)
                    .saturating_sub(1),
            );
            for sequence in room.oldest_available_sequence..=end {
                let value = events
                    .remove((room_id.0, sequence))
                    .map_err(db_error)?
                    .ok_or_else(|| "MLS cleanup found a hole in the delivery log".to_string())?;
                match decode_event(value.value())?.event {
                    MlsDeliveryEvent::Application { event_id, .. } => {
                        event_ids.remove((room_id.0, event_id.0)).map_err(db_error)?;
                    }
                    MlsDeliveryEvent::Commit { parent_epoch, .. } => {
                        commit_epochs
                            .remove((room_id.0, parent_epoch))
                            .map_err(db_error)?;
                    }
                }
                report.deleted_events += 1;
            }
            room.oldest_available_sequence = end.saturating_add(1);
            report.more_work = end < prune_through;
            let room_bytes = jsony::to_binary(&room);
            rooms
                .insert(room_id.0, room_bytes.as_slice())
                .map_err(db_error)?;
            drop(commit_epochs);
            drop(event_ids);
            drop(events);
            drop(cursors);
            drop(rooms);
            report.more_work |= cleanup_welcomes(&tx, now_unix_ms, batch_limit)?;
        }
        tx.commit().map_err(db_error)?;
        Ok(Some(report))
    }

    pub fn status(&self) -> Result<StorageStatus, String> {
        let tx = self.database.begin_write().map_err(db_error)?;
        let stats = tx.stats().map_err(db_error)?;
        let allocated_bytes = stats
            .allocated_pages()
            .saturating_mul(stats.page_size() as u64);
        let stored_bytes = stats.stored_bytes();
        let status = StorageStatus {
            allocated_bytes,
            stored_bytes,
            fragmented_bytes: stats.fragmented_bytes(),
            file_bytes: self
                .path
                .as_ref()
                .and_then(|path| fs::metadata(path).ok())
                .map(|meta| meta.len()),
        };
        tx.abort().map_err(db_error)?;
        Ok(status)
    }

    pub fn compact(&mut self) -> Result<bool, String> {
        self.database.compact().map_err(db_error)
    }

    fn validate(&self, tx: &redb::ReadTransaction, rooms: &[RoomRecord]) -> Result<(), String> {
        let events = tx.open_table(EVENTS).map_err(db_error)?;
        let event_ids = tx.open_table(EVENT_IDS).map_err(db_error)?;
        let commit_epochs = tx.open_table(COMMIT_EPOCHS).map_err(db_error)?;
        let cursors = tx.open_table(DEVICE_CURSORS).map_err(db_error)?;
        for room in rooms {
            let id = room.descriptor.room_id;
            if room.oldest_available_sequence == 0
                || room.oldest_available_sequence > room.head_sequence.saturating_add(1)
            {
                return Err("persisted MLS room has an invalid retained range".to_string());
            }
            let mut expected = room.oldest_available_sequence;
            for entry in events
                .range((id.0, room.oldest_available_sequence)..=(id.0, room.head_sequence))
                .map_err(db_error)?
            {
                let (key, value) = entry.map_err(db_error)?;
                if key.value().1 != expected {
                    return Err("persisted MLS delivery sequence is not contiguous".to_string());
                }
                let stored = decode_event(value.value())?;
                if stored.event.sequence() != expected {
                    return Err("persisted MLS event key does not match its sequence".to_string());
                }
                match stored.event {
                    MlsDeliveryEvent::Application { event_id, .. } => {
                        if event_ids
                            .get((id.0, event_id.0))
                            .map_err(db_error)?
                            .map(|value| value.value())
                            != Some(expected)
                        {
                            return Err("persisted MLS application index is inconsistent".to_string());
                        }
                    }
                    MlsDeliveryEvent::Commit { parent_epoch, .. } => {
                        if commit_epochs
                            .get((id.0, parent_epoch))
                            .map_err(db_error)?
                            .map(|value| value.value())
                            != Some(expected)
                        {
                            return Err("persisted MLS commit index is inconsistent".to_string());
                        }
                    }
                }
                expected = expected.saturating_add(1);
            }
            if expected != room.head_sequence.saturating_add(1) {
                return Err("persisted MLS delivery head does not match its log".to_string());
            }
            for device in &room.required_devices {
                if cursors
                    .get((id.0, device.0))
                    .map_err(db_error)?
                    .is_none()
                {
                    return Err("persisted MLS required device has no cursor".to_string());
                }
            }
        }
        Ok(())
    }
}

fn insert_event(
    tx: &redb::WriteTransaction,
    room_id: RoomId,
    event: &MlsDeliveryEvent,
    event_bytes: &[u8],
) -> Result<(), String> {
    let sequence = event.sequence();
    let mut events = tx.open_table(EVENTS).map_err(db_error)?;
    if events
        .insert((room_id.0, sequence), event_bytes)
        .map_err(db_error)?
        .is_some()
    {
        return Err("MLS delivery sequence unexpectedly already exists".to_string());
    }
    match event {
        MlsDeliveryEvent::Application { event_id, .. } => {
            let mut ids = tx.open_table(EVENT_IDS).map_err(db_error)?;
            if ids
                .insert((room_id.0, event_id.0), sequence)
                .map_err(db_error)?
                .is_some()
            {
                return Err("MLS application event id unexpectedly already exists".to_string());
            }
        }
        MlsDeliveryEvent::Commit { parent_epoch, .. } => {
            let mut commits = tx.open_table(COMMIT_EPOCHS).map_err(db_error)?;
            if commits
                .insert((room_id.0, *parent_epoch), sequence)
                .map_err(db_error)?
                .is_some()
            {
                return Err("MLS commit parent epoch unexpectedly already exists".to_string());
            }
        }
    }
    Ok(())
}

fn reconcile_cursors(
    tx: &redb::WriteTransaction,
    room_id: RoomId,
    previous: &[DeviceId],
    room: &RoomRecord,
    sequence: u64,
) -> Result<(), String> {
    let mut cursors = tx.open_table(DEVICE_CURSORS).map_err(db_error)?;
    for removed in previous
        .iter()
        .filter(|device| !room.required_devices.contains(device))
    {
        cursors.remove((room_id.0, removed.0)).map_err(db_error)?;
    }
    for added in room
        .required_devices
        .iter()
        .filter(|device| !previous.contains(device))
    {
        let bytes = jsony::to_binary(&DeviceCursor {
            highest_contiguous_sequence: sequence,
            starting_sequence: sequence,
            last_ack_unix_ms: room.last_event_time_unix_ms,
            rejoin_required_through: None,
        });
        if cursors
            .insert((room_id.0, added.0), bytes.as_slice())
            .map_err(db_error)?
            .is_some()
        {
            return Err("new MLS room member unexpectedly already has a cursor".to_string());
        }
    }
    Ok(())
}

fn insert_welcome(
    tx: &redb::WriteTransaction,
    welcome: Option<(u64, &MlsWelcomeBundle)>,
    sequence: u64,
    stored_at_unix_ms: u64,
) -> Result<(), String> {
    let Some((delivery_id, bundle)) = welcome else {
        return Ok(());
    };
    let bytes = jsony::to_binary(&StoredWelcome {
        delivery_id,
        sequence,
        stored_at_unix_ms,
        bundle: bundle.clone(),
    });
    let mut welcomes = tx.open_table(WELCOMES).map_err(db_error)?;
    if welcomes
        .insert(delivery_id, bytes.as_slice())
        .map_err(db_error)?
        .is_some()
    {
        return Err("MLS Welcome delivery id unexpectedly already exists".to_string());
    }
    let mut targets = tx.open_table(DEVICE_WELCOMES).map_err(db_error)?;
    for device in &bundle.device_ids {
        if targets
            .insert((device.0, delivery_id), ())
            .map_err(db_error)?
            .is_some()
        {
            return Err("MLS Welcome target unexpectedly already exists".to_string());
        }
    }
    tx.open_table(WELCOME_TARGET_COUNTS)
        .map_err(db_error)?
        .insert(delivery_id, bundle.device_ids.len() as u16)
        .map_err(db_error)?;
    Ok(())
}

fn cleanup_welcomes(
    tx: &redb::WriteTransaction,
    now_unix_ms: u64,
    batch_limit: usize,
) -> Result<bool, String> {
    let rooms = tx.open_table(ROOMS).map_err(db_error)?;
    let cursors = tx.open_table(WELCOME_CURSORS).map_err(db_error)?;
    let targets = tx.open_table(DEVICE_WELCOMES).map_err(db_error)?;
    let welcomes = tx.open_table(WELCOMES).map_err(db_error)?;
    let mut remove_targets = Vec::new();
    let mut more_work = false;
    for entry in targets.iter().map_err(db_error)? {
        let (key, _) = entry.map_err(db_error)?;
        let (device, delivery_id) = key.value();
        let acknowledged = cursors
            .get(device)
            .map_err(db_error)?
            .is_some_and(|cursor| cursor.value() >= delivery_id);
        let welcome = welcomes
            .get(delivery_id)
            .map_err(db_error)?
            .ok_or_else(|| "MLS Welcome target references a missing value".to_string())?;
        let welcome = decode_welcome(welcome.value())?;
        let room = rooms
            .get(welcome.bundle.descriptor.room_id.0)
            .map_err(db_error)?
            .ok_or_else(|| "MLS Welcome references a missing room".to_string())?;
        let room = decode_room(room.value())?;
        let expired = welcome.stored_at_unix_ms
            < now_unix_ms.saturating_sub(
                u64::from(room.retention_days).saturating_mul(24 * 60 * 60 * 1000),
            );
        if acknowledged || expired {
            if remove_targets.len() == batch_limit {
                more_work = true;
                break;
            }
            remove_targets.push((device, delivery_id));
        }
    }
    drop(welcomes);
    drop(targets);
    let mut targets = tx.open_table(DEVICE_WELCOMES).map_err(db_error)?;
    let mut welcomes = tx.open_table(WELCOMES).map_err(db_error)?;
    let mut counts = tx.open_table(WELCOME_TARGET_COUNTS).map_err(db_error)?;
    for key @ (_, delivery_id) in remove_targets {
        targets.remove(key).map_err(db_error)?;
        let count = counts
            .get(delivery_id)
            .map_err(db_error)?
            .ok_or_else(|| "MLS Welcome target count is missing".to_string())?
            .value();
        if count == 1 {
            counts.remove(delivery_id).map_err(db_error)?;
            welcomes.remove(delivery_id).map_err(db_error)?;
        } else {
            counts.insert(delivery_id, count - 1).map_err(db_error)?;
        }
    }
    Ok(more_work)
}

fn decode_global(bytes: &[u8]) -> Result<GlobalRecord, String> {
    jsony::from_binary(bytes).map_err(|error| format!("failed to decode MLS global state: {error}"))
}

fn decode_room(bytes: &[u8]) -> Result<RoomRecord, String> {
    jsony::from_binary(bytes).map_err(|error| format!("failed to decode MLS room state: {error}"))
}

fn decode_event(bytes: &[u8]) -> Result<StoredEvent, String> {
    jsony::from_binary(bytes).map_err(|error| format!("failed to decode MLS event: {error}"))
}

fn decode_welcome(bytes: &[u8]) -> Result<StoredWelcome, String> {
    jsony::from_binary(bytes).map_err(|error| format!("failed to decode MLS Welcome: {error}"))
}

fn decode_cursor(bytes: &[u8]) -> Result<DeviceCursor, String> {
    jsony::from_binary(bytes).map_err(|error| format!("failed to decode MLS device cursor: {error}"))
}

fn db_error(error: impl std::fmt::Display) -> String {
    format!("MLS database operation failed: {error}")
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
                    commit: vec![9],
                },
                None,
            )
            .unwrap();
    }

    #[test]
    fn schema_version_is_created_and_reopened() {
        let temp = tempfile::tempdir().unwrap();
        MlsStore::open(Some(temp.path())).unwrap();
        let loaded = MlsStore::open(Some(temp.path())).unwrap().load().unwrap();
        assert!(loaded.rooms.is_empty());
        assert_eq!(loaded.global.next_welcome_delivery_id, 1);
    }

    #[test]
    fn unsupported_schema_version_is_rejected_clearly() {
        let temp = tempfile::tempdir().unwrap();
        let store = MlsStore::open(Some(temp.path())).unwrap();
        let tx = store.database.begin_write().unwrap();
        {
            let mut meta = tx.open_table(META).unwrap();
            meta.insert(SCHEMA_KEY, 99u32.to_be_bytes().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
        drop(store);

        let error = MlsStore::open(Some(temp.path())).unwrap_err();
        assert!(error.contains("unsupported MLS database schema version 99"));
    }

    #[test]
    fn ordered_scans_and_application_retry_index_are_room_bounded() {
        let store = MlsStore::open(None).unwrap();
        let device = DeviceId([7; 16]);
        create(&store, &room(10, vec![device], 100));
        create(&store, &room(11, vec![device], 100));

        for (room_id, byte) in [(RoomId(10), 1), (RoomId(10), 2), (RoomId(11), 3)] {
            let event_id = EventId([byte; 16]);
            assert!(matches!(
                store
                    .append_application(room_id, device, 1, event_id, &[byte], 200)
                    .unwrap(),
                AppendApplicationResult::Stored { sequence: 2 | 3, .. }
            ));
        }

        let batch = store.events(RoomId(10), 0, 10).unwrap();
        assert_eq!(
            batch
                .events
                .iter()
                .map(MlsDeliveryEvent::sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(batch.head_sequence, 3);
        assert_eq!(
            store
                .application_retry(RoomId(10), EventId([1; 16]), 1, &[1])
                .unwrap(),
            Some(Ok(2))
        );
        assert_eq!(
            store
                .application_retry(RoomId(10), EventId([1; 16]), 1, &[9])
                .unwrap(),
            Some(Err(()))
        );
        assert!(matches!(
            store
                .append_application(
                    RoomId(10),
                    device,
                    1,
                    EventId([1; 16]),
                    &[9],
                    300,
                )
                .unwrap(),
            AppendApplicationResult::Conflict
        ));
        assert_eq!(store.events(RoomId(10), 0, 10).unwrap().head_sequence, 3);
        assert_eq!(store.events(RoomId(11), 0, 10).unwrap().events.len(), 2);
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
        assert!(!second.more_work);
        let batch = store.events(RoomId(20), 0, 10).unwrap();
        assert_eq!(batch.oldest_available_sequence, 4);
        assert_eq!(batch.head_sequence, 3);
        assert!(batch.events.is_empty());
        assert_eq!(
            store
                .application_retry(RoomId(20), EventId([1; 16]), 1, &[1])
                .unwrap(),
            None
        );
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
                &[1],
                100,
            )
            .unwrap();

        let day = 24 * 60 * 60 * 1000;
        let early = store.cleanup(day + 150, 10).unwrap();
        assert_eq!(early.deleted_events, 1); // safe cursor only, not forced expiry
        assert_eq!(early.lagging_devices, 0);
        assert!(!store.requires_rejoin(RoomId(30), device).unwrap());

        let expired = store.cleanup(day + 201, 10).unwrap();
        assert_eq!(expired.deleted_events, 1);
        assert_eq!(expired.lagging_devices, 1);
        assert!(store.requires_rejoin(RoomId(30), device).unwrap());
    }
}
