//! Per-server local room catalog.
//!
//! One TOML file per server (`<data-dir>/<history-id>/rooms.toml`)
//! remembers every room the client has seen — id, name, kind, read watermark —
//! plus the last viewed and last voice rooms. It is what makes room names and
//! navigation available offline, alongside the per-room `room-<id>.kvlog`
//! message captures.

use std::{fs, io::Write, path::PathBuf};

use rpc::ids::{MessageId, RoomId, UserId};
use toml_spanner::Toml;

use crate::room_history;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RoomCatalog {
    pub(crate) last_viewed_room: Option<RoomId>,
    pub(crate) last_voice_room: Option<RoomId>,
    pub(crate) rooms: Vec<CatalogRoom>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRoom {
    pub(crate) room_id: RoomId,
    pub(crate) name: String,
    pub(crate) kind: CatalogRoomKind,
    pub(crate) last_read: Option<MessageId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CatalogRoomKind {
    Public,
    Private,
    /// A DM room, labeled offline by the peer's last-known display name.
    Dm {
        peer: UserId,
        peer_name: String,
    },
}

#[derive(Default, Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct CatalogFile {
    #[toml(default)]
    last_viewed_room: u32,
    #[toml(default)]
    last_voice_room: u32,
    #[toml(default)]
    rooms: Vec<CatalogFileRoom>,
}

#[derive(Toml)]
#[toml(FromToml, rename_all = "kebab-case")]
struct CatalogFileRoom {
    id: u32,
    name: String,
    #[toml(default)]
    kind: String,
    #[toml(default)]
    dm_peer: u64,
    #[toml(default)]
    dm_peer_name: String,
    #[toml(default)]
    last_read: u64,
}

fn catalog_path(history_id: &str) -> Option<PathBuf> {
    let dir = room_history::server_dir(history_id)?;
    Some(dir.join("rooms.toml"))
}

pub(crate) fn load(history_id: &str) -> RoomCatalog {
    let Some(path) = catalog_path(history_id) else {
        return RoomCatalog::default();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return RoomCatalog::default();
    };
    let arena = toml_spanner::Arena::new();
    let Ok(mut doc) = toml_spanner::parse(&content, &arena) else {
        kvlog::warn!(
            "room catalog unparseable; ignoring",
            path = path.display().to_string().as_str()
        );
        return RoomCatalog::default();
    };
    let Ok(file) = doc.to::<CatalogFile>() else {
        kvlog::warn!(
            "room catalog undeserializable; ignoring",
            path = path.display().to_string().as_str()
        );
        return RoomCatalog::default();
    };
    RoomCatalog {
        last_viewed_room: (file.last_viewed_room != 0).then_some(RoomId(file.last_viewed_room)),
        last_voice_room: (file.last_voice_room != 0).then_some(RoomId(file.last_voice_room)),
        rooms: file
            .rooms
            .into_iter()
            .map(|room| CatalogRoom {
                room_id: RoomId(room.id),
                name: room.name,
                kind: match room.kind.as_str() {
                    "private" => CatalogRoomKind::Private,
                    "dm" => CatalogRoomKind::Dm {
                        peer: UserId(room.dm_peer),
                        peer_name: room.dm_peer_name,
                    },
                    _ => CatalogRoomKind::Public,
                },
                last_read: (room.last_read != 0).then_some(MessageId(room.last_read)),
            })
            .collect(),
    }
}

/// Atomically rewrites the catalog file. Best-effort: failures are logged and
/// the in-memory state stays authoritative for the session.
pub(crate) fn save(history_id: &str, catalog: &RoomCatalog) {
    let Some(path) = catalog_path(history_id) else {
        return;
    };
    if let Some(parent) = path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        kvlog::warn!("room catalog dir create failed", error = %error);
        return;
    }
    let mut out = String::new();
    out.push_str("# chatt local room catalog. Managed by the client; do not edit.\n\n");
    if let Some(room) = catalog.last_viewed_room {
        out.push_str(&format!("last-viewed-room = {}\n", room.0));
    }
    if let Some(room) = catalog.last_voice_room {
        out.push_str(&format!("last-voice-room = {}\n", room.0));
    }
    for room in &catalog.rooms {
        out.push_str("\n[[rooms]]\n");
        out.push_str(&format!("id = {}\n", room.room_id.0));
        out.push_str(&format!("name = \"{}\"\n", toml_quote(&room.name)));
        match &room.kind {
            CatalogRoomKind::Public => out.push_str("kind = \"public\"\n"),
            CatalogRoomKind::Private => out.push_str("kind = \"private\"\n"),
            CatalogRoomKind::Dm { peer, peer_name } => {
                out.push_str("kind = \"dm\"\n");
                out.push_str(&format!("dm-peer = {}\n", peer.0));
                out.push_str(&format!("dm-peer-name = \"{}\"\n", toml_quote(peer_name)));
            }
        }
        if let Some(last_read) = room.last_read {
            out.push_str(&format!("last-read = {}\n", last_read.0));
        }
    }
    let tmp = path.with_extension("toml.tmp");
    let result = fs::File::create(&tmp)
        .and_then(|mut file| {
            file.write_all(out.as_bytes())?;
            file.flush()
        })
        .and_then(|()| fs::rename(&tmp, &path));
    if let Err(error) = result {
        let _ = fs::remove_file(&tmp);
        kvlog::warn!("room catalog write failed", error = %error);
    }
}

fn toml_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_round_trips_rooms_and_watermarks() {
        let history_id = "catalog-round-trip-test";
        let catalog = RoomCatalog {
            last_viewed_room: Some(RoomId(2)),
            last_voice_room: Some(RoomId(1)),
            rooms: vec![
                CatalogRoom {
                    room_id: RoomId(1),
                    name: "lobby".to_string(),
                    kind: CatalogRoomKind::Public,
                    last_read: Some(MessageId(42)),
                },
                CatalogRoom {
                    room_id: RoomId(3),
                    name: "secret".to_string(),
                    kind: CatalogRoomKind::Private,
                    last_read: None,
                },
                CatalogRoom {
                    room_id: RoomId(0x8000_0000),
                    name: "dm:1:2".to_string(),
                    kind: CatalogRoomKind::Dm {
                        peer: UserId(2),
                        peer_name: "Bob".to_string(),
                    },
                    last_read: Some(MessageId(7)),
                },
            ],
        };

        save(history_id, &catalog);
        let loaded = load(history_id);

        assert_eq!(loaded, catalog);
    }

    #[test]
    fn missing_catalog_loads_empty() {
        let loaded = load("catalog-missing-test");
        assert_eq!(loaded, RoomCatalog::default());
    }
}
