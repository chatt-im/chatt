//! Append-only per-room chat persistence on top of the kvlog binary format.
//!
//! Each `(server, room)` pair owns one active kvlog file under the XDG data
//! directory. The active file is validated before it is opened for append. A
//! provably incomplete tail is truncated; other corruption is archived and
//! only structurally validated records are copied into a clean active file.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kvlog::encoding::{
    Encoder, Key, LogFields, MunchError, SpanInfo, StaticKey, Value, log_len, munch_log_with_span,
};
use kvlog::{Encode, LogLevel};
use ring::digest::{SHA256, digest};
use rpc::control::{ChatMessage, MAX_CHAT_BODY_BYTES};
use rpc::ids::{FileTransferId, MessageId, RoomId, UserId};

const RECOVERY_MIN_RECORDS: usize = 2;
const MAX_ROOM_RECORD_BYTES: usize = 128 * 1024;

/// Rotation threshold for one room's active log. When an append crosses this
/// size the file is renamed to a single `.1` backup and a fresh file is opened,
/// bounding steady-state history to roughly twice this per room so a chatty peer
/// cannot grow it without limit.
const MAX_HISTORY_BYTES: u64 = 4 * 1024 * 1024;

/// Upper bound on bytes read from one history file. A larger file (left by an
/// older build, or tampered with) is read from its tail and resynced to a record
/// boundary, so load memory stays bounded regardless of the file's size.
const MAX_LOAD_BYTES: u64 = MAX_HISTORY_BYTES + 1024 * 1024;

/// Durable identity for file metadata. Server transfer ids restart at one, so
/// they are unique only when paired with the announcement timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileHistoryKey {
    pub timestamp_ms: u64,
    pub transfer_id: FileTransferId,
}

/// File-message metadata that is not part of [`ChatMessage`], captured when the
/// matching `FileReceived` event arrives and folded back in on load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileDetail {
    pub file_name: String,
    /// Byte size of the transferred file.
    pub length: u64,
    /// Image dimensions packed as `(height << 32) | width`, `0` when unknown.
    pub packed_dims: u64,
}

impl FileDetail {
    /// Unpacks [`packed_dims`](FileDetail::packed_dims) into `(width, height)`,
    /// or `None` when no dimensions were recorded.
    pub(crate) fn dimensions(&self) -> Option<(u32, u32)> {
        if self.packed_dims == 0 {
            return None;
        }
        let width = (self.packed_dims & 0xFFFF_FFFF) as u32;
        let height = (self.packed_dims >> 32) as u32;
        Some((width, height))
    }
}

/// Result of reading a per-room log back from disk.
#[derive(Debug, Default)]
pub(crate) struct LoadedHistory {
    /// Messages deduped by `(timestamp_ms, message_id)` and sorted by that pair.
    pub messages: Vec<ChatMessage>,
    /// File-detail records keyed by announcement timestamp and transfer id.
    pub files: HashMap<FileHistoryKey, FileDetail>,
}

/// A validated history load and its append handle, when the active file was
/// either clean or repaired successfully.
pub(crate) struct OpenedHistory {
    pub loaded: LoadedHistory,
    pub store: Option<RoomHistoryStore>,
}

/// An open append handle to one room's history file.
pub(crate) struct RoomHistoryStore {
    file: Option<File>,
    path: PathBuf,
    encoder: Encoder,
    /// Bytes in the active file, tracked to trigger size-based rotation.
    bytes_written: u64,
}

impl RoomHistoryStore {
    /// Encodes one chat record and appends it. Errors permanently disable this
    /// handle so no later record can be written behind a partial append.
    pub(crate) fn append_message(&mut self, message: &ChatMessage) {
        self.encoder.clear();
        let level = if message.file_transfer_id.is_some() {
            LogLevel::Warn
        } else {
            LogLevel::Info
        };
        let timestamp_nano = message.timestamp_ms.saturating_mul(1_000_000);
        {
            let mut field = self.encoder.append(level, timestamp_nano);
            message
                .message_id
                .0
                .encode_log_value_into(field.static_key(StaticKey::id));
            message
                .body
                .encode_log_value_into(field.static_key(StaticKey::msg));
            message
                .sender
                .0
                .encode_log_value_into(field.static_key(StaticKey::user_id));
            message
                .sender_name
                .encode_log_value_into(field.static_key(StaticKey::caller));
            message
                .room_id
                .0
                .encode_log_value_into(field.static_key(StaticKey::conn_id));
            if let Some(transfer_id) = message.file_transfer_id {
                transfer_id
                    .0
                    .encode_log_value_into(field.static_key(StaticKey::object_id));
            }
        }
        self.write_record();
    }

    /// Appends file metadata with the same native timestamp and transfer id as
    /// its chat announcement.
    pub(crate) fn append_file_detail(
        &mut self,
        key: FileHistoryKey,
        file_name: &str,
        length: u64,
        packed_dims: u64,
    ) {
        self.encoder.clear();
        {
            let mut field = self
                .encoder
                .append(LogLevel::Debug, key.timestamp_ms.saturating_mul(1_000_000));
            key.transfer_id
                .0
                .encode_log_value_into(field.static_key(StaticKey::object_id));
            key.timestamp_ms
                .encode_log_value_into(field.static_key(StaticKey::timestamp));
            file_name.encode_log_value_into(field.static_key(StaticKey::path));
            length.encode_log_value_into(field.static_key(StaticKey::length));
            packed_dims.encode_log_value_into(field.static_key(StaticKey::size));
        }
        self.write_record();
    }

    fn write_record(&mut self) {
        let Some(file) = &mut self.file else {
            return;
        };
        let written = self.encoder.bytes().len() as u64;
        let result = file
            .write_all(self.encoder.bytes())
            .and_then(|()| file.flush());
        if let Err(error) = result {
            self.file = None;
            kvlog::warn!(
                "room history append failed; persistence disabled",
                path = self.path.display().to_string(),
                err = error.to_string()
            );
            return;
        }
        self.bytes_written = self.bytes_written.saturating_add(written);
        if self.bytes_written >= MAX_HISTORY_BYTES {
            self.rotate();
        }
    }

    /// Renames the full active file to a single `.1` backup and opens a fresh
    /// active file. A failure leaves the handle appending to the current file and
    /// resets the counter so rotation is retried only after more growth, never on
    /// every record.
    fn rotate(&mut self) {
        let backup = backup_path(&self.path);
        if let Err(error) = fs::rename(&self.path, &backup) {
            kvlog::warn!(
                "room history rotate failed",
                path = self.path.display().to_string(),
                err = error.to_string()
            );
            self.bytes_written = 0;
            return;
        }
        self.bytes_written = 0;
        match open_append_file(&self.path) {
            Some(file) => self.file = Some(file),
            None => {
                self.file = None;
                kvlog::warn!(
                    "room history reopen after rotate failed; persistence disabled",
                    path = self.path.display().to_string()
                );
            }
        }
    }

    #[cfg(test)]
    fn rotate_now(&mut self) {
        self.rotate();
    }
}

/// Loads and validates one room's history, repairing its active file before
/// returning an append handle. Filesystem failures degrade to read-only history.
/// `history_id` is the stable per-server id from [`derive_server_id`], empty when
/// not connected.
pub(crate) fn open(history_id: &str, room_id: RoomId) -> OpenedHistory {
    let Some(path) = history_path(history_id, room_id) else {
        return OpenedHistory {
            loaded: LoadedHistory::default(),
            store: None,
        };
    };
    open_path(&path, room_id.0)
}

fn open_path(path: &Path, default_room: u32) -> OpenedHistory {
    if let Some(parent) = path.parent()
        && let Err(error) = create_private_dir(parent)
    {
        kvlog::warn!(
            "room history dir create failed",
            path = parent.display().to_string(),
            err = error.to_string()
        );
        return OpenedHistory {
            loaded: LoadedHistory::default(),
            store: None,
        };
    }

    // A pre-existing oversized active file (from a build without rotation, or
    // tampering) is moved aside so the full read below and the append handle stay
    // bounded. Its tail is recovered through the backup path that follows.
    let active_len = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if active_len > MAX_LOAD_BYTES {
        if let Err(error) = fs::rename(path, backup_path(path)) {
            kvlog::warn!(
                "room history oversize rotate failed; loading read-only",
                path = path.display().to_string(),
                err = error.to_string()
            );
            let records = read_archived_records(path, default_room);
            return OpenedHistory {
                loaded: fold_records(&records),
                store: None,
            };
        }
        kvlog::warn!(
            "room history oversized; rotated to backup",
            path = path.display().to_string(),
            bytes = active_len
        );
    }

    let mut records = read_archived_records(&backup_path(path), default_room);

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            kvlog::warn!(
                "room history read failed",
                path = path.display().to_string(),
                err = error.to_string()
            );
            return OpenedHistory {
                loaded: fold_records(&records),
                store: None,
            };
        }
    };
    let scan = scan_bytes(&bytes, default_room);
    let append_safe = match scan.repair {
        Repair::Clean => true,
        Repair::IncompleteTail { valid_end } => truncate_tail(path, bytes.len(), valid_end),
        Repair::Corrupt => rotate_corrupt(path, &bytes, &scan.records),
    };
    records.extend(scan.records);
    let loaded = fold_records(&records);
    let store = append_safe.then(|| open_append(path)).flatten();
    OpenedHistory { loaded, store }
}

fn open_append(path: &Path) -> Option<RoomHistoryStore> {
    let file = open_append_file(path)?;
    let bytes_written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    Some(RoomHistoryStore {
        file: Some(file),
        path: path.to_path_buf(),
        encoder: Encoder::with_capacity(512),
        bytes_written,
    })
}

/// Opens (creating it private) a room file for append. Files are mode `0600` so
/// chat contents are not exposed to other local users.
fn open_append_file(path: &Path) -> Option<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(path) {
        Ok(file) => Some(file),
        Err(error) => {
            kvlog::warn!(
                "room history open failed",
                path = path.display().to_string(),
                err = error.to_string()
            );
            None
        }
    }
}

/// Creates `dir` and any missing parents with mode `0700` so the history tree is
/// readable only by its owner.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)
}

/// The single rotation backup beside `path`, named by appending `.1`.
fn backup_path(path: &Path) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(".1");
    PathBuf::from(raw)
}

/// Reads a history file read-only and returns its structurally valid records.
/// Missing files yield none. An oversized file is read from its tail and
/// resynced to a record boundary, bounding memory at the cost of the oldest
/// records.
fn read_archived_records(path: &Path, default_room: u32) -> Vec<ValidatedRecord> {
    let Some((bytes, trimmed)) = read_capped(path) else {
        return Vec::new();
    };
    if !trimmed {
        return scan_bytes(&bytes, default_room).records;
    }
    match find_resync(&bytes, 0, default_room) {
        Some(start) => scan_bytes(&bytes[start..], default_room).records,
        None => Vec::new(),
    }
}

/// Reads a file, capping memory at [`MAX_LOAD_BYTES`]. Returns the bytes and
/// whether the read was a tail of an oversized file. A missing file returns
/// `None`.
fn read_capped(path: &Path) -> Option<(Vec<u8>, bool)> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            kvlog::warn!(
                "room history read failed",
                path = path.display().to_string(),
                err = error.to_string()
            );
            return None;
        }
    };
    let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let trimmed = len > MAX_LOAD_BYTES;
    if trimmed && let Err(error) = file.seek(SeekFrom::Start(len - MAX_LOAD_BYTES)) {
        kvlog::warn!(
            "room history seek failed",
            path = path.display().to_string(),
            err = error.to_string()
        );
        return None;
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_LOAD_BYTES).read_to_end(&mut bytes) {
        kvlog::warn!(
            "room history read failed",
            path = path.display().to_string(),
            err = error.to_string()
        );
        return None;
    }
    if trimmed {
        kvlog::warn!(
            "room history oversized on load; tail read",
            path = path.display().to_string(),
            dropped = len - MAX_LOAD_BYTES
        );
    }
    Some((bytes, trimmed))
}

#[derive(Clone, Debug)]
enum ParsedRecord {
    Message(ChatMessage),
    FileDetail {
        key: FileHistoryKey,
        detail: FileDetail,
    },
}

#[derive(Clone, Debug)]
struct ValidatedRecord {
    range: Range<usize>,
    parsed: ParsedRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Repair {
    Clean,
    IncompleteTail { valid_end: usize },
    Corrupt,
}

struct ScanResult {
    records: Vec<ValidatedRecord>,
    repair: Repair,
}

fn scan_bytes(bytes: &[u8], default_room: u32) -> ScanResult {
    let mut records = Vec::new();
    let mut offset = 0;
    let mut valid_run = 0;
    let mut recovered_gap = false;

    while offset < bytes.len() {
        match parse_record_at(bytes, offset, default_room) {
            Ok(record) => {
                offset = record.range.end;
                records.push(record);
                valid_run += 1;
            }
            Err(_) => {
                let candidate = (valid_run >= RECOVERY_MIN_RECORDS)
                    .then(|| find_resync(bytes, offset.saturating_add(1), default_room))
                    .flatten();
                if let Some(candidate) = candidate {
                    recovered_gap = true;
                    offset = candidate;
                    valid_run = 0;
                    continue;
                }
                let repair = if !recovered_gap && is_incomplete_tail(bytes, offset) {
                    Repair::IncompleteTail { valid_end: offset }
                } else {
                    Repair::Corrupt
                };
                return ScanResult { records, repair };
            }
        }
    }

    ScanResult {
        records,
        repair: if recovered_gap {
            Repair::Corrupt
        } else {
            Repair::Clean
        },
    }
}

fn find_resync(bytes: &[u8], from: usize, default_room: u32) -> Option<usize> {
    let mut candidate = from;
    while candidate.saturating_add(4) <= bytes.len() {
        if bytes[candidate + 3] == kvlog::encoding::MAGIC_BYTE as u8
            && candidate_run_is_valid(bytes, candidate, default_room)
        {
            return Some(candidate);
        }
        candidate += 1;
    }
    None
}

fn candidate_run_is_valid(bytes: &[u8], start: usize, default_room: u32) -> bool {
    let mut offset = start;
    let mut count = 0;
    loop {
        match parse_record_at(bytes, offset, default_room) {
            Ok(record) => {
                count += 1;
                offset = record.range.end;
                if offset == bytes.len() {
                    return true;
                }
                if count >= RECOVERY_MIN_RECORDS {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
}

fn parse_record_at(
    bytes: &[u8],
    offset: usize,
    default_room: u32,
) -> Result<ValidatedRecord, MunchError> {
    let remaining = bytes.get(offset..).ok_or(MunchError::Eof)?;
    let length = log_len(remaining).ok_or(MunchError::MissingMagicByte)?;
    if !(13..=MAX_ROOM_RECORD_BYTES).contains(&length) {
        return Err(MunchError::InvalidValue);
    }
    let record_bytes = remaining.get(..length).ok_or(MunchError::EofOnFields)?;
    let mut input = record_bytes;
    let (timestamp_nano, level, span, fields) = munch_log_with_span(&mut input)?;
    if !input.is_empty() || span != SpanInfo::None {
        return Err(MunchError::InvalidValue);
    }
    let parsed = parse_fields(timestamp_nano, level, fields, default_room)?;
    Ok(ValidatedRecord {
        range: offset..offset + length,
        parsed,
    })
}

fn parse_fields(
    timestamp_nano: u64,
    level: LogLevel,
    fields: LogFields<'_>,
    default_room: u32,
) -> Result<ParsedRecord, MunchError> {
    let mut id = None;
    let mut body = None;
    let mut user = None;
    let mut caller = None;
    let mut object = None;
    let mut room = None;
    let mut file_name = None;
    let mut length = None;
    let mut size = None;
    let mut correlation_timestamp = None;

    for field in fields {
        let (key, value) = field?;
        match key {
            Key::Static(StaticKey::id) => set_once(&mut id, exact_u64(value)?)?,
            Key::Static(StaticKey::msg) => set_once(&mut body, exact_string(value)?)?,
            Key::Static(StaticKey::user_id) => set_once(&mut user, exact_u64(value)?)?,
            Key::Static(StaticKey::caller) => set_once(&mut caller, exact_string(value)?)?,
            Key::Static(StaticKey::object_id) => set_once(&mut object, exact_u64(value)?)?,
            Key::Static(StaticKey::conn_id) => set_once(&mut room, exact_u32(value)?)?,
            Key::Static(StaticKey::path) => set_once(&mut file_name, exact_string(value)?)?,
            Key::Static(StaticKey::length) => set_once(&mut length, exact_u64(value)?)?,
            Key::Static(StaticKey::size) => set_once(&mut size, exact_u64(value)?)?,
            Key::Static(StaticKey::timestamp) => {
                set_once(&mut correlation_timestamp, exact_u64(value)?)?
            }
            _ => {}
        }
    }

    let timestamp_ms = timestamp_nano / 1_000_000;
    match (id, body) {
        (Some(id), Some(body)) => {
            let user = user.ok_or(MunchError::InvalidValue)?;
            let caller = caller.ok_or(MunchError::InvalidValue)?;
            let room = room.ok_or(MunchError::InvalidValue)?;
            if room != default_room
                || timestamp_nano % 1_000_000 != 0
                || body.len() > MAX_CHAT_BODY_BYTES
                || level
                    != if object.is_some() {
                        LogLevel::Warn
                    } else {
                        LogLevel::Info
                    }
                || file_name.is_some()
                || length.is_some()
                || size.is_some()
                || correlation_timestamp.is_some()
            {
                return Err(MunchError::InvalidValue);
            }
            Ok(ParsedRecord::Message(ChatMessage {
                message_id: MessageId(id),
                room_id: RoomId(room),
                sender: UserId(user),
                sender_name: caller,
                timestamp_ms,
                body,
                file_transfer_id: object.map(FileTransferId),
            }))
        }
        (None, None) => {
            if level != LogLevel::Debug || user.is_some() || caller.is_some() || room.is_some() {
                return Err(MunchError::InvalidValue);
            }
            let transfer_id = FileTransferId(object.ok_or(MunchError::InvalidValue)?);
            let file_name = file_name.ok_or(MunchError::InvalidValue)?;
            let length = length.ok_or(MunchError::InvalidValue)?;
            let packed_dims = size.ok_or(MunchError::InvalidValue)?;
            let correlation_timestamp = correlation_timestamp.ok_or(MunchError::InvalidValue)?;
            if timestamp_nano % 1_000_000 != 0 || correlation_timestamp != timestamp_ms {
                return Err(MunchError::InvalidValue);
            }
            Ok(ParsedRecord::FileDetail {
                key: FileHistoryKey {
                    timestamp_ms: correlation_timestamp,
                    transfer_id,
                },
                detail: FileDetail {
                    file_name,
                    length,
                    packed_dims,
                },
            })
        }
        _ => Err(MunchError::InvalidValue),
    }
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), MunchError> {
    if slot.is_some() {
        return Err(MunchError::InvalidValue);
    }
    *slot = Some(value);
    Ok(())
}

fn exact_u64(value: Value<'_>) -> Result<u64, MunchError> {
    match value {
        Value::U64(inner) => Ok(inner),
        _ => Err(MunchError::InvalidValue),
    }
}

fn exact_u32(value: Value<'_>) -> Result<u32, MunchError> {
    match value {
        Value::U32(inner) => Ok(inner),
        _ => Err(MunchError::InvalidValue),
    }
}

fn exact_string(value: Value<'_>) -> Result<String, MunchError> {
    match value {
        Value::String(bytes) => std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| MunchError::InvalidString),
        _ => Err(MunchError::InvalidValue),
    }
}

fn fold_records(records: &[ValidatedRecord]) -> LoadedHistory {
    let mut messages: HashMap<(u64, u64), ChatMessage> = HashMap::new();
    let mut details = Vec::new();
    for record in records {
        match &record.parsed {
            ParsedRecord::Message(message) => {
                messages.insert(
                    (message.timestamp_ms, message.message_id.0),
                    message.clone(),
                );
            }
            ParsedRecord::FileDetail { key, detail } => {
                details.push((*key, detail.clone()));
            }
        }
    }

    let mut message_keys: HashSet<(FileTransferId, u64)> = HashSet::new();
    for message in messages.values() {
        if let Some(transfer_id) = message.file_transfer_id {
            message_keys.insert((transfer_id, message.timestamp_ms));
        }
    }

    let mut files = HashMap::new();
    for (key, detail) in details {
        if message_keys.contains(&(key.transfer_id, key.timestamp_ms)) {
            files.insert(key, detail);
        }
    }

    let mut messages: Vec<ChatMessage> = messages.into_values().collect();
    messages.sort_by_key(|message| (message.timestamp_ms, message.message_id.0));
    LoadedHistory { messages, files }
}

fn is_incomplete_tail(bytes: &[u8], offset: usize) -> bool {
    let Some(remaining) = bytes.get(offset..) else {
        return false;
    };
    if remaining.len() < 4 {
        return true;
    }
    match log_len(remaining) {
        Some(length) => remaining.len() < 13 || length > remaining.len(),
        None => false,
    }
}

fn truncate_tail(path: &Path, observed_len: usize, valid_end: usize) -> bool {
    let unchanged = fs::metadata(path)
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        == Some(observed_len);
    if !unchanged {
        kvlog::warn!(
            "room history changed during tail repair; persistence disabled",
            path = path.display().to_string()
        );
        return false;
    }
    let result = OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_len(valid_end as u64));
    match result {
        Ok(()) => {
            kvlog::warn!(
                "room history incomplete tail truncated",
                path = path.display().to_string(),
                removed = observed_len.saturating_sub(valid_end)
            );
            true
        }
        Err(error) => {
            kvlog::warn!(
                "room history tail repair failed; persistence disabled",
                path = path.display().to_string(),
                err = error.to_string()
            );
            false
        }
    }
}

fn rotate_corrupt(path: &Path, bytes: &[u8], records: &[ValidatedRecord]) -> bool {
    let unchanged = fs::metadata(path)
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        == Some(bytes.len());
    if !unchanged {
        kvlog::warn!(
            "room history changed during corruption recovery; persistence disabled",
            path = path.display().to_string()
        );
        return false;
    }

    let Some((archive_path, mut archive)) = create_unique_sibling(path, "corrupt") else {
        return false;
    };
    if let Err(error) = archive.write_all(bytes).and_then(|()| archive.sync_all()) {
        let _ = fs::remove_file(&archive_path);
        kvlog::warn!(
            "room history archive failed; persistence disabled",
            path = path.display().to_string(),
            err = error.to_string()
        );
        return false;
    }
    drop(archive);

    let Some((temporary_path, mut temporary)) = create_unique_sibling(path, "recovering") else {
        return false;
    };
    let rewrite = records
        .iter()
        .try_for_each(|record| temporary.write_all(&bytes[record.range.clone()]));
    if let Err(error) = rewrite.and_then(|()| temporary.sync_all()) {
        let _ = fs::remove_file(&temporary_path);
        kvlog::warn!(
            "room history rewrite failed; persistence disabled",
            path = path.display().to_string(),
            err = error.to_string()
        );
        return false;
    }
    drop(temporary);

    match fs::rename(&temporary_path, path) {
        Ok(()) => {
            kvlog::warn!(
                "room history corruption rotated",
                path = path.display().to_string(),
                archive = archive_path.display().to_string(),
                recovered = records.len()
            );
            true
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary_path);
            kvlog::warn!(
                "room history replacement failed; persistence disabled",
                path = path.display().to_string(),
                archive = archive_path.display().to_string(),
                err = error.to_string()
            );
            false
        }
    }
}

fn create_unique_sibling(path: &Path, kind: &str) -> Option<(PathBuf, File)> {
    let parent = path.parent()?;
    let stem = path.file_stem()?.to_string_lossy();
    let timestamp = now_millis();
    for counter in 0..10_000 {
        let candidate = parent.join(format!("{stem}.{kind}-{timestamp}-{counter}.kvlog"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&candidate) {
            Ok(file) => return Some((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                kvlog::warn!(
                    "room history recovery file create failed",
                    path = candidate.display().to_string(),
                    err = error.to_string()
                );
                return None;
            }
        }
    }
    None
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// `$XDG_DATA_HOME/chatt`, else `$HOME/.local/share/chatt`. Tests redirect this
/// to a per-process temp dir so they never touch the real data directory.
fn data_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        Some(test_data_dir())
    }
    #[cfg(not(test))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            Some(PathBuf::from(xdg).join("chatt"))
        } else {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/chatt"))
        }
    }
}

#[cfg(test)]
fn test_data_dir() -> PathBuf {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::temp_dir().join(format!("chatt-history-tests-{}", std::process::id()))
    })
    .clone()
}

fn sanitize_alias(alias: &str) -> String {
    let mut out: String = alias
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out.truncate(64);
    out
}

/// Stable per-server storage id derived from the server token. It survives alias
/// renames and endpoint edits, and a re-paired server that reuses an alias gets a
/// fresh token and id, so unrelated servers never share a history directory.
pub(crate) fn derive_server_id(token: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hash = digest(&SHA256, token.as_bytes());
    let mut id = String::with_capacity(32);
    for byte in &hash.as_ref()[..16] {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    id
}

fn history_path(history_id: &str, room_id: RoomId) -> Option<PathBuf> {
    if history_id.is_empty() {
        return None;
    }
    Some(room_file_path(&data_dir()?, history_id, room_id))
}

fn room_file_path(base: &Path, history_id: &str, room_id: RoomId) -> PathBuf {
    base.join(sanitize_alias(history_id))
        .join(format!("room-{}.kvlog", room_id.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owns a private scratch directory holding one history file. Dropping it
    /// removes the whole directory, so the file, its backup, and any recovery
    /// siblings (`.corrupt-*`, `.recovering-*`) are cleaned up even on panic.
    struct Scratch {
        dir: PathBuf,
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("chatt-history-{}-{}", std::process::id(), label));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("scratch dir");
            let path = dir.join("room-7.kvlog");
            Scratch { dir, path }
        }
    }

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<Path> for Scratch {
        fn as_ref(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn scratch(label: &str) -> Scratch {
        Scratch::new(label)
    }

    fn fresh_store(path: &Path) -> RoomHistoryStore {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(backup_path(path));
        open_path(path, 7).store.expect("open")
    }

    fn text_message(id: u64, timestamp_ms: u64, body: &str) -> ChatMessage {
        ChatMessage {
            message_id: MessageId(id),
            room_id: RoomId(7),
            sender: UserId(3),
            sender_name: "alice".to_string(),
            timestamp_ms,
            body: body.to_string(),
            file_transfer_id: None,
        }
    }

    fn append_messages(path: &Path, ids: Range<u64>) {
        let mut store = fresh_store(path);
        for id in ids {
            store.append_message(&text_message(id, id * 1_000, &format!("message-{id}")));
        }
    }

    fn record_ranges(bytes: &[u8]) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            let len = log_len(&bytes[offset..]).expect("record length");
            ranges.push(offset..offset + len);
            offset += len;
        }
        ranges
    }

    #[test]
    fn round_trip_text_message() {
        let path = scratch("round-trip-text");
        let mut store = fresh_store(&path);
        let original = text_message(11, 1_700_000_000_000, "hello world");
        store.append_message(&original);
        drop(store);

        let opened = open_path(&path, 7);
        assert_eq!(opened.loaded.messages, vec![original]);
    }

    #[test]
    fn same_transfer_id_at_different_timestamps_keeps_both_details() {
        let path = scratch("file-identity");
        let mut store = fresh_store(&path);
        for (id, timestamp, name) in [(20, 1_000, "old.png"), (21, 2_000, "new.png")] {
            let mut message = text_message(id, timestamp, name);
            message.file_transfer_id = Some(FileTransferId(99));
            store.append_message(&message);
            store.append_file_detail(
                FileHistoryKey {
                    timestamp_ms: timestamp,
                    transfer_id: FileTransferId(99),
                },
                name,
                4096,
                ((480u64) << 32) | 640u64,
            );
        }
        drop(store);

        let loaded = open_path(&path, 7).loaded;
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.files.len(), 2);
        assert_eq!(
            loaded.files[&FileHistoryKey {
                timestamp_ms: 1_000,
                transfer_id: FileTransferId(99)
            }]
                .file_name,
            "old.png"
        );
        assert_eq!(
            loaded.files[&FileHistoryKey {
                timestamp_ms: 2_000,
                transfer_id: FileTransferId(99)
            }]
                .file_name,
            "new.png"
        );
    }

    #[test]
    fn detail_is_kept_only_when_a_message_shares_its_transfer_id_and_timestamp() {
        let mut records = Vec::new();
        let mut matched = text_message(1, 1_000, "matched");
        matched.file_transfer_id = Some(FileTransferId(5));
        records.push(ValidatedRecord {
            range: 0..0,
            parsed: ParsedRecord::Message(matched),
        });
        records.push(ValidatedRecord {
            range: 0..0,
            parsed: ParsedRecord::FileDetail {
                key: FileHistoryKey {
                    timestamp_ms: 1_000,
                    transfer_id: FileTransferId(5),
                },
                detail: FileDetail {
                    file_name: "matched.png".to_string(),
                    length: 1,
                    packed_dims: 0,
                },
            },
        });
        // A detail whose timestamp does not match any message of its transfer id
        // is dropped.
        let mut other = text_message(2, 2_000, "other");
        other.file_transfer_id = Some(FileTransferId(6));
        records.push(ValidatedRecord {
            range: 0..0,
            parsed: ParsedRecord::Message(other),
        });
        records.push(ValidatedRecord {
            range: 0..0,
            parsed: ParsedRecord::FileDetail {
                key: FileHistoryKey {
                    timestamp_ms: 9_000,
                    transfer_id: FileTransferId(6),
                },
                detail: FileDetail {
                    file_name: "stale.png".to_string(),
                    length: 1,
                    packed_dims: 0,
                },
            },
        });

        let loaded = fold_records(&records);
        assert!(loaded.files.contains_key(&FileHistoryKey {
            timestamp_ms: 1_000,
            transfer_id: FileTransferId(5)
        }));
        assert!(
            !loaded
                .files
                .keys()
                .any(|key| key.transfer_id == FileTransferId(6))
        );
    }

    #[test]
    fn incomplete_tail_is_truncated_before_future_appends() {
        let path = scratch("torn-tail-repair");
        append_messages(&path, 1..4);
        let full = fs::metadata(&path).expect("metadata").len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen")
            .set_len(full - 3)
            .expect("truncate");

        let mut opened = open_path(&path, 7);
        assert_eq!(opened.loaded.messages.len(), 2);
        opened
            .store
            .as_mut()
            .expect("repaired append store")
            .append_message(&text_message(4, 4_000, "after-repair"));
        drop(opened);

        let loaded = open_path(&path, 7).loaded;
        let ids: Vec<u64> = loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2, 4]);
    }

    #[test]
    fn partial_header_tail_is_truncated() {
        let path = scratch("partial-header");
        append_messages(&path, 1..3);
        let valid_len = fs::metadata(&path).expect("metadata").len();
        let mut file = OpenOptions::new().append(true).open(&path).expect("append");
        file.write_all(&[1, 2]).expect("partial header");
        drop(file);

        let opened = open_path(&path, 7);
        assert!(opened.store.is_some());
        assert_eq!(fs::metadata(&path).expect("metadata").len(), valid_len);
    }

    #[test]
    fn middle_corruption_rotates_and_recovers_valid_suffix() {
        let path = scratch("middle-corruption");
        append_messages(&path, 1..6);
        let bytes = fs::read(&path).expect("read");
        let ranges = record_ranges(&bytes);
        let mut corrupt = bytes;
        corrupt[ranges[2].start + 3] = 0;
        fs::write(&path, corrupt).expect("corrupt");

        let mut opened = open_path(&path, 7);
        let ids: Vec<u64> = opened
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2, 4, 5]);
        opened
            .store
            .as_mut()
            .expect("rotated append store")
            .append_message(&text_message(6, 6_000, "after-rotation"));
        drop(opened);

        let ids: Vec<u64> = open_path(&path, 7)
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2, 4, 5, 6]);
        let parent = path.parent().expect("parent");
        let stem = path.file_stem().expect("stem").to_string_lossy();
        assert!(fs::read_dir(parent).expect("dir").any(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{stem}.corrupt-"))
        }));
    }

    #[test]
    fn invalid_utf8_record_is_corruption_and_suffix_is_recovered() {
        let path = scratch("invalid-utf8");
        append_messages(&path, 1..6);
        let mut bytes = fs::read(&path).expect("read");
        let body = b"message-3";
        let body_offset = bytes
            .windows(body.len())
            .position(|window| window == body)
            .expect("third body");
        bytes[body_offset] = 0xff;
        fs::write(&path, bytes).expect("corrupt utf8");

        let ids: Vec<u64> = open_path(&path, 7)
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2, 4, 5]);
    }

    #[test]
    fn complete_invalid_tail_rotates_instead_of_truncating_in_place() {
        let path = scratch("invalid-tail");
        append_messages(&path, 1..4);
        let mut bytes = fs::read(&path).expect("read");
        let body = b"message-3";
        let body_offset = bytes
            .windows(body.len())
            .position(|window| window == body)
            .expect("third body");
        bytes[body_offset] = 0xff;
        fs::write(&path, bytes).expect("corrupt utf8");

        let opened = open_path(&path, 7);
        assert!(opened.store.is_some());
        let ids: Vec<u64> = opened
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2]);
        let stem = path.file_stem().expect("stem").to_string_lossy();
        assert!(
            fs::read_dir(path.parent().expect("parent"))
                .expect("dir")
                .any(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{stem}.corrupt-"))
                })
        );
    }

    #[test]
    fn embedded_magic_does_not_create_a_false_record_boundary() {
        let path = scratch("embedded-magic");
        let mut store = fresh_store(&path);
        for id in 1..6 {
            let body = if id == 3 {
                format!("noise-\u{1000}-message-{id}")
            } else {
                format!("message-{id}")
            };
            store.append_message(&text_message(id, id * 1_000, &body));
        }
        drop(store);
        let mut bytes = fs::read(&path).expect("read");
        let ranges = record_ranges(&bytes);
        bytes[ranges[2].start + 3] = 0;
        fs::write(&path, bytes).expect("corrupt header");

        let ids: Vec<u64> = open_path(&path, 7)
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2, 4, 5]);
    }

    #[test]
    fn clean_eof_accepts_short_recovered_suffix() {
        let path = scratch("eof-recovery");
        append_messages(&path, 1..5);
        let bytes = fs::read(&path).expect("read");
        let ranges = record_ranges(&bytes);
        let mut corrupt = bytes;
        corrupt[ranges[2].start + 3] = 0;
        fs::write(&path, corrupt).expect("corrupt");

        let ids: Vec<u64> = open_path(&path, 7)
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2, 4]);
    }

    #[test]
    fn insufficient_prefix_does_not_resynchronize() {
        let path = scratch("short-prefix");
        append_messages(&path, 1..5);
        let bytes = fs::read(&path).expect("read");
        let ranges = record_ranges(&bytes);
        let mut corrupt = bytes;
        corrupt[ranges[1].start + 3] = 0;
        fs::write(&path, corrupt).expect("corrupt");

        let ids: Vec<u64> = open_path(&path, 7)
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn dedup_and_sort_on_load() {
        let path = scratch("dedup-sort");
        let mut store = fresh_store(&path);
        store.append_message(&text_message(2, 3_000, "third"));
        store.append_message(&text_message(1, 1_000, "first"));
        store.append_message(&text_message(1, 1_000, "first-updated"));
        store.append_message(&text_message(1, 2_000, "post-restart"));
        drop(store);

        let loaded = open_path(&path, 7).loaded;
        let bodies: Vec<&str> = loaded
            .messages
            .iter()
            .map(|message| message.body.as_str())
            .collect();
        assert_eq!(bodies, vec!["first-updated", "post-restart", "third"]);
    }

    #[test]
    fn sanitize_alias_strips_separators() {
        assert_eq!(sanitize_alias("a/b..c"), "a_b__c");
        assert_eq!(sanitize_alias(""), "_");
        assert_eq!(sanitize_alias("good-Name_1"), "good-Name_1");
    }

    #[test]
    fn room_file_path_layout() {
        let path = room_file_path(Path::new("/data/chatt"), "srv", RoomId(5));
        assert_eq!(path, PathBuf::from("/data/chatt/srv/room-5.kvlog"));
        assert_eq!(history_path("", RoomId(5)), None);
    }

    #[test]
    fn derive_server_id_is_stable_and_distinct() {
        assert_eq!(derive_server_id("token-a"), derive_server_id("token-a"));
        assert_ne!(derive_server_id("token-a"), derive_server_id("token-b"));
        assert_eq!(derive_server_id("token-a").len(), 32);
    }

    #[test]
    fn rotation_splits_into_backup_and_merges_on_load() {
        let path = scratch("rotation");
        let mut store = fresh_store(&path);
        for id in 1..4 {
            store.append_message(&text_message(id, id * 1_000, &format!("message-{id}")));
        }
        store.rotate_now();
        for id in 4..7 {
            store.append_message(&text_message(id, id * 1_000, &format!("message-{id}")));
        }
        drop(store);

        assert!(backup_path(&path).exists());
        let ids: Vec<u64> = open_path(&path, 7)
            .loaded
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6]);
    }

    #[cfg(unix)]
    #[test]
    fn history_dir_and_file_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = data_dir().expect("data dir");
        let dir = base.join("private-perms-test");
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("room-9.kvlog");
        let mut store = open_path(&file, 9).store.expect("store");
        store.append_message(&text_message(1, 1_000, "private"));
        drop(store);

        let dir_mode = fs::metadata(&dir).expect("dir meta").permissions().mode() & 0o777;
        let file_mode = fs::metadata(&file).expect("file meta").permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        let _ = fs::remove_dir_all(&base);
    }
}
