//! Append-only per-room chat persistence on top of the kvlog binary format.
//!
//! Each `(server, room)` pair owns one kvlog file under the XDG data dir. A chat
//! message maps onto kvlog builtins (native record timestamp, [`StaticKey::msg`]
//! for the body, [`StaticKey::id`] for the message id, and so on), so the file
//! is a normal kvlog stream that existing tooling can decode.
//!
//! The store is authoritative: [`load`] is read at startup for offline viewing
//! and merged with the server's history on join. Records are deduped on the
//! composite key `(timestamp_ms, message_id)` because the server's `message_id`
//! is a per-process counter that resets on every boot, so the id alone is not
//! stable across server restarts.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kvlog::encoding::{Encoder, Key, StaticKey, Value, decode};
use kvlog::{Encode, LogLevel};
use rpc::control::ChatMessage;
use rpc::ids::{FileTransferId, MessageId, RoomId, UserId};

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
    /// File-detail records keyed by transfer id, for the web view.
    pub files: HashMap<FileTransferId, FileDetail>,
}

/// An open append handle to one room's history file.
pub(crate) struct RoomHistoryStore {
    file: File,
    path: PathBuf,
    encoder: Encoder,
}

impl RoomHistoryStore {
    /// Opens (creating the directory tree and file) the per-room log in append
    /// mode. Returns `None` on any filesystem error so chat continues without
    /// persistence.
    pub(crate) fn open(server_alias: &str, room_id: RoomId) -> Option<RoomHistoryStore> {
        let path = history_path(server_alias, room_id)?;
        Self::open_at(path)
    }

    fn open_at(path: PathBuf) -> Option<RoomHistoryStore> {
        if let Some(parent) = path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            kvlog::warn!(
                "room history dir create failed",
                path = parent.display().to_string(),
                err = error.to_string()
            );
            return None;
        }
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => Some(RoomHistoryStore {
                file,
                path,
                encoder: Encoder::with_capacity(512),
            }),
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

    /// Encodes one chat record and appends it. The body is clipped to 64 KiB by
    /// the kvlog string encoder. Errors are logged, never propagated.
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

    /// Appends a correlated file-detail record (level `Debug`, no `id`/`msg`
    /// keys, which is how [`load`] tells it apart from a chat record).
    pub(crate) fn append_file_detail(
        &mut self,
        transfer_id: FileTransferId,
        file_name: &str,
        length: u64,
        packed_dims: u64,
    ) {
        self.encoder.clear();
        {
            let mut field = self.encoder.append(LogLevel::Debug, now_nano());
            transfer_id
                .0
                .encode_log_value_into(field.static_key(StaticKey::object_id));
            file_name.encode_log_value_into(field.static_key(StaticKey::path));
            length.encode_log_value_into(field.static_key(StaticKey::length));
            packed_dims.encode_log_value_into(field.static_key(StaticKey::size));
        }
        self.write_record();
    }

    fn write_record(&mut self) {
        let result = self
            .file
            .write_all(self.encoder.bytes())
            .and_then(|()| self.file.flush());
        if let Err(error) = result {
            kvlog::warn!(
                "room history append failed",
                path = self.path.display().to_string(),
                err = error.to_string()
            );
        }
    }
}

/// Reads a room's history, deduped on `(timestamp_ms, message_id)` and sorted.
///
/// Missing files load as empty without warning. A torn trailing record (from a
/// process killed mid-append) stops decoding and keeps everything prior.
pub(crate) fn load(server_alias: &str, room_id: RoomId) -> LoadedHistory {
    let Some(path) = history_path(server_alias, room_id) else {
        return LoadedHistory::default();
    };
    load_path(&path, room_id.0)
}

fn load_path(path: &Path, default_room: u32) -> LoadedHistory {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedHistory::default();
        }
        Err(error) => {
            kvlog::warn!(
                "room history read failed",
                path = path.display().to_string(),
                err = error.to_string()
            );
            return LoadedHistory::default();
        }
    };
    load_bytes(&bytes, default_room)
}

fn load_bytes(bytes: &[u8], default_room: u32) -> LoadedHistory {
    let mut messages: HashMap<(u64, u64), ChatMessage> = HashMap::new();
    let mut files: HashMap<FileTransferId, FileDetail> = HashMap::new();

    for record in decode(bytes) {
        let Ok((timestamp_nano, _level, _span, fields)) = record else {
            break;
        };
        let mut id = None;
        let mut body = None;
        let mut user = None;
        let mut caller = None;
        let mut object = None;
        let mut room = None;
        let mut file_name = None;
        let mut length = None;
        let mut size = None;
        let mut torn = false;
        for field in fields {
            let Ok((key, value)) = field else {
                torn = true;
                break;
            };
            match key {
                Key::Static(StaticKey::id) => id = value_u64(value),
                Key::Static(StaticKey::msg) => body = value_string(value),
                Key::Static(StaticKey::user_id) => user = value_u32(value),
                Key::Static(StaticKey::caller) => caller = value_string(value),
                Key::Static(StaticKey::object_id) => object = value_u64(value),
                Key::Static(StaticKey::conn_id) => room = value_u32(value),
                Key::Static(StaticKey::path) => file_name = value_string(value),
                Key::Static(StaticKey::length) => length = value_u64(value),
                Key::Static(StaticKey::size) => size = value_u64(value),
                _ => {}
            }
        }
        if torn {
            break;
        }
        let timestamp_ms = timestamp_nano / 1_000_000;
        match (id, body) {
            (Some(id), Some(body)) => {
                let message = ChatMessage {
                    message_id: MessageId(id),
                    room_id: RoomId(room.unwrap_or(default_room)),
                    sender: UserId(user.unwrap_or(0)),
                    sender_name: caller.unwrap_or_default(),
                    timestamp_ms,
                    body,
                    file_transfer_id: object.map(FileTransferId),
                };
                messages.insert((timestamp_ms, id), message);
            }
            _ => {
                if let (Some(object), Some(file_name)) = (object, file_name) {
                    files.insert(
                        FileTransferId(object),
                        FileDetail {
                            file_name,
                            length: length.unwrap_or(0),
                            packed_dims: size.unwrap_or(0),
                        },
                    );
                }
            }
        }
    }

    let mut messages: Vec<ChatMessage> = messages.into_values().collect();
    messages.sort_by_key(|message| (message.timestamp_ms, message.message_id.0));
    LoadedHistory { messages, files }
}

fn value_u64(value: Value) -> Option<u64> {
    match value {
        Value::U64(inner) => Some(inner),
        Value::U32(inner) => Some(inner as u64),
        _ => None,
    }
}

fn value_u32(value: Value) -> Option<u32> {
    match value {
        Value::U32(inner) => Some(inner),
        Value::U64(inner) => Some(inner as u32),
        _ => None,
    }
}

fn value_string(value: Value) -> Option<String> {
    match value {
        Value::String(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

fn now_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
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

/// Replaces filesystem-unsafe characters in a server alias. Aliases already pass
/// `validate_server_alias`, so this is defensive and normally a no-op.
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

/// Resolves the on-disk log path. Returns `None` for an empty alias, which marks
/// "not connected to a server", so no history is read or written in that state.
fn history_path(server_alias: &str, room_id: RoomId) -> Option<PathBuf> {
    if server_alias.is_empty() {
        return None;
    }
    Some(room_file_path(&data_dir()?, server_alias, room_id))
}

fn room_file_path(base: &Path, server_alias: &str, room_id: RoomId) -> PathBuf {
    base.join(sanitize_alias(server_alias))
        .join(format!("room-{}.kvlog", room_id.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "chatt-history-{}-{}.kvlog",
            std::process::id(),
            label
        ))
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

    #[test]
    fn round_trip_text_message() {
        let path = scratch_path("round-trip-text");
        let _ = fs::remove_file(&path);
        let mut store = RoomHistoryStore::open_at(path.clone()).expect("open");
        let original = text_message(11, 1_700_000_000_000, "hello world");
        store.append_message(&original);
        drop(store);

        let loaded = load_path(&path, 7);
        assert_eq!(loaded.messages, vec![original]);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn round_trip_file_message_with_detail() {
        let path = scratch_path("round-trip-file");
        let _ = fs::remove_file(&path);
        let mut store = RoomHistoryStore::open_at(path.clone()).expect("open");
        let mut message = text_message(20, 1_700_000_001_000, "photo.png");
        message.file_transfer_id = Some(FileTransferId(99));
        store.append_message(&message);
        let packed = ((480u64) << 32) | 640u64;
        store.append_file_detail(FileTransferId(99), "photo.png", 4096, packed);
        drop(store);

        let loaded = load_path(&path, 7);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(
            loaded.messages[0].file_transfer_id,
            Some(FileTransferId(99))
        );
        let detail = loaded.files.get(&FileTransferId(99)).expect("detail");
        assert_eq!(detail.file_name, "photo.png");
        assert_eq!(detail.length, 4096);
        let width = (detail.packed_dims & 0xFFFF_FFFF) as u32;
        let height = (detail.packed_dims >> 32) as u32;
        assert_eq!((width, height), (640, 480));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn dedup_and_sort_on_load() {
        let path = scratch_path("dedup-sort");
        let _ = fs::remove_file(&path);
        let mut store = RoomHistoryStore::open_at(path.clone()).expect("open");
        // Out of order, a duplicate composite key, and a same-id-different-time
        // pair that a server restart would produce.
        store.append_message(&text_message(2, 3_000, "third"));
        store.append_message(&text_message(1, 1_000, "first"));
        store.append_message(&text_message(1, 1_000, "first-updated"));
        store.append_message(&text_message(1, 2_000, "post-restart"));
        drop(store);

        let loaded = load_path(&path, 7);
        let bodies: Vec<&str> = loaded
            .messages
            .iter()
            .map(|message| message.body.as_str())
            .collect();
        assert_eq!(bodies, vec!["first-updated", "post-restart", "third"]);
    }

    #[test]
    fn torn_tail_is_ignored() {
        let path = scratch_path("torn-tail");
        let _ = fs::remove_file(&path);
        let mut store = RoomHistoryStore::open_at(path.clone()).expect("open");
        store.append_message(&text_message(1, 1_000, "kept"));
        store.append_message(&text_message(2, 2_000, "torn"));
        drop(store);

        let full = fs::metadata(&path).expect("metadata").len();
        let file = OpenOptions::new().write(true).open(&path).expect("reopen");
        file.set_len(full - 3).expect("truncate");
        drop(file);

        let loaded = load_path(&path, 7);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].body, "kept");
        let _ = fs::remove_file(&path);
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
        // Empty alias resolves to no path (treated as "not connected").
        assert_eq!(history_path("", RoomId(5)), None);
    }
}
