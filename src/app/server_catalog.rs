//! Transactional mutation boundary for server records.
//!
//! Every durable change to `config.servers` goes through one operation here:
//! locate the entry by its immutable [`ServerId`], stage the change, save the
//! runtime config, and roll back on a refused write. A failed save therefore
//! never leaves memory ahead of disk, and callers apply session or UI effects
//! only from a returned success.

use std::path::PathBuf;

use rpc::ids::ServerId;

use crate::config::{Config, ServerEntry, validate_server_entry};

use super::server::{ServerEditDraft, ServerEditFields};

pub(super) enum EditCommit {
    Saved {
        server: ServerEntry,
        path: PathBuf,
        /// Whether a field the live session was established with changed, so
        /// the caller can report that the edit lands on the next connect.
        connection_fields_changed: bool,
    },
    /// The entry changed under the draft; present this reload of it instead.
    Conflict(Box<ServerEditDraft>),
    /// The entry is gone. A draft may never re-create it.
    Missing,
}

/// Applies `draft` onto the entry it was opened over.
///
/// # Errors
///
/// Returns the message to show when a field does not parse, the new label is
/// already taken, the merged entry fails validation, or the write is refused.
/// The configuration is unchanged on every error.
pub(super) fn commit_edit(
    config: &mut Config,
    draft: &ServerEditDraft,
) -> Result<EditCommit, String> {
    let fields = draft.fields()?;
    if let Some(base) = draft.new_server() {
        if let Some(current) = config.server_by_id(base.id) {
            return Ok(EditCommit::Conflict(Box::new(
                ServerEditDraft::from_server(current, config),
            )));
        }
        let mut candidate = base.clone();
        fields.apply_to(&mut candidate);
        let (server, path) = insert_server(config, candidate)?;
        return Ok(EditCommit::Saved {
            server,
            path,
            connection_fields_changed: false,
        });
    }
    let Some(index) = config
        .servers
        .iter()
        .position(|server| server.id == draft.server_id())
    else {
        return Ok(EditCommit::Missing);
    };
    let current = &config.servers[index];
    if Some(&ServerEditFields::of(current)) != draft.original_fields() {
        let reload = ServerEditDraft::from_server(current, config);
        return Ok(EditCommit::Conflict(Box::new(reload)));
    }
    if config
        .servers
        .iter()
        .any(|existing| existing.id != draft.server_id() && existing.label == fields.label)
    {
        return Err(format!("server label {} already exists", fields.label));
    }
    let mut candidate = current.clone();
    fields.apply_to(&mut candidate);
    validate_server_entry(&candidate)?;
    let connection_fields_changed = !current.connection_fields_eq(&candidate);
    let previous = std::mem::replace(&mut config.servers[index], candidate);
    let path = persist(config, |config| {
        config.servers[index] = previous;
    })?;
    Ok(EditCommit::Saved {
        server: config.servers[index].clone(),
        path,
        connection_fields_changed,
    })
}

/// Inserts one fully paired server. Pairing calls this before opening the
/// editor; a username-retry editor calls it only after its retry succeeds.
/// No incomplete recovery state is accepted by this boundary.
pub(super) fn insert_server(
    config: &mut Config,
    server: ServerEntry,
) -> Result<(ServerEntry, PathBuf), String> {
    validate_server_entry(&server)?;
    if config.server_by_id(server.id).is_some() {
        return Err(format!("server id {} already exists", server.id));
    }
    if config
        .servers
        .iter()
        .any(|existing| existing.label == server.label)
    {
        return Err(format!("server label {} already exists", server.label));
    }
    let id = server.id;
    config.servers.push(server);
    let path = persist(config, |config| {
        config.servers.pop();
    })?;
    Ok((
        config
            .server_by_id(id)
            .expect("new server persisted above")
            .clone(),
        path,
    ))
}

/// Removes the entry and the audio preferences it keys.
///
/// # Errors
///
/// Returns the message to show when no entry has this id or the write is
/// refused; a refused write leaves the entry and its preferences configured.
pub(super) fn delete(config: &mut Config, server_id: ServerId) -> Result<PathBuf, String> {
    let Some(index) = config
        .servers
        .iter()
        .position(|server| server.id == server_id)
    else {
        return Err("server is no longer configured".to_string());
    };
    let server = config.servers.remove(index);
    let user_audio = std::mem::take(&mut config.user_audio);
    config.user_audio = user_audio
        .iter()
        .filter(|preference| preference.server_id != server_id)
        .cloned()
        .collect();
    persist(config, |config| {
        config.servers.insert(index, server);
        config.user_audio = user_audio;
    })
}

/// Persists the transport-encryption requirement for one entry, returning the
/// committed entry.
///
/// # Errors
///
/// Returns the message to show when no entry has this id or the write is
/// refused; a refused write leaves the previous requirement in force.
pub(super) fn commit_transport_policy(
    config: &mut Config,
    server_id: ServerId,
    require_encryption: bool,
) -> Result<(ServerEntry, PathBuf), String> {
    let Some(server) = config.server_by_id_mut(server_id) else {
        return Err("server is no longer configured".to_string());
    };
    let previous = std::mem::replace(&mut server.require_transport_encryption, require_encryption);
    let path = persist(config, |config| {
        if let Some(server) = config.server_by_id_mut(server_id) {
            server.require_transport_encryption = previous;
        }
    })?;
    let server = config
        .server_by_id(server_id)
        .expect("entry persisted above")
        .clone();
    Ok((server, path))
}

/// Commits reissued credentials onto the current record by id.
///
/// # Errors
///
/// Returns the message to show when the record is missing, its credentials
/// changed while repair was running, the repaired record is invalid, or the
/// write is refused. Non-credential edits made during repair are preserved.
pub(super) fn commit_repaired_credentials(
    config: &mut Config,
    server_id: ServerId,
    expected_token: &str,
    expected_server_public_key: &str,
    token: String,
    server_public_key: String,
) -> Result<(ServerEntry, PathBuf), String> {
    let Some(index) = config
        .servers
        .iter()
        .position(|server| server.id == server_id)
    else {
        return Err("server is no longer configured".to_string());
    };
    let current = &config.servers[index];
    if current.token != expected_token || current.server_public_key != expected_server_public_key {
        return Err("server credentials changed while they were being repaired".to_string());
    }
    let mut ready = current.clone();
    ready.token = token;
    ready.server_public_key = server_public_key;
    validate_server_entry(&ready)?;
    let previous = std::mem::replace(&mut config.servers[index], ready);
    let path = persist(config, |config| {
        config.servers[index] = previous;
    })?;
    let server = config.servers[index].clone();
    Ok((server, path))
}

/// Persists one DM identity pin onto its entry, replacing any pin for the
/// same peer.
///
/// # Errors
///
/// Returns the message to show when no entry has this id or the write is
/// refused; a refused write leaves the previous pins in force.
pub(super) fn commit_e2e_pin(
    config: &mut Config,
    server_id: ServerId,
    pin: crate::config::E2ePeerPin,
) -> Result<(), String> {
    let Some(entry) = config.server_by_id_mut(server_id) else {
        return Err("server is no longer configured".to_string());
    };
    let previous = std::mem::take(&mut entry.e2e_peer_pins);
    entry.e2e_peer_pins = previous
        .iter()
        .filter(|stored| stored.room_id != pin.room_id && stored.user_id != pin.user_id)
        .cloned()
        .collect();
    entry.e2e_peer_pins.push(pin);
    persist(config, |config| {
        if let Some(entry) = config.server_by_id_mut(server_id) {
            entry.e2e_peer_pins = previous;
        }
    })
    .map(|_| ())
}

/// Persists one room's overrides onto its entry, returning whether the room's
/// history configuration changed.
///
/// # Errors
///
/// Returns the message to show when no entry has this id or the write is
/// refused; a refused write leaves the previous overrides in force.
pub(super) fn commit_room_overrides(
    config: &mut Config,
    server_id: ServerId,
    overrides: crate::config::RoomOverrides,
) -> Result<(bool, PathBuf), String> {
    let Some(entry) = config.server_by_id_mut(server_id) else {
        return Err("server is no longer configured".to_string());
    };
    let previous = entry.rooms.clone();
    let history_changed = previous
        .iter()
        .find(|room| room.room_id == overrides.room_id)
        .map(|room| room.history.clone())
        .unwrap_or_default()
        != overrides.history;
    entry.rooms.retain(|room| room.room_id != overrides.room_id);
    if !overrides.is_empty() {
        entry.rooms.push(overrides);
        entry.rooms.sort_by_key(|room| room.room_id);
    }
    let path = persist(config, |config| {
        if let Some(entry) = config.server_by_id_mut(server_id) {
            entry.rooms = previous;
        }
    })?;
    Ok((history_changed, path))
}

/// Saves the already-staged config, running `rollback` when the write is
/// refused so memory never gets ahead of disk.
fn persist(config: &mut Config, rollback: impl FnOnce(&mut Config)) -> Result<PathBuf, String> {
    match config.save_runtime() {
        Ok(path) => {
            config.config_path = Some(path.clone());
            Ok(path)
        }
        Err(error) => {
            rollback(config);
            Err(error)
        }
    }
}
