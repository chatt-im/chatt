use local_rpc::{
    ids::{RoomId, ServerId},
    model::RequestId,
    room_settings as wire,
};

use crate::config::{
    self, DownloadTarget, FileOverrides, HistoryOverrides, RoomOverrides, RuntimeSaveError,
};

use super::App;

impl App {
    pub(crate) fn rpc_open_room_settings(
        &self,
        request_id: RequestId,
        room_id: RoomId,
    ) -> Result<wire::RoomSettingsEvent, String> {
        let server_id = self
            .room
            .active_server_id
            .ok_or_else(|| "connect to a server first".to_string())?;
        let source = self.config.load_runtime_document()?;
        let document = self.rpc_room_settings_document(
            &source.config,
            source.revision,
            server_id,
            room_id,
        )?;
        Ok(wire::RoomSettingsEvent::Document {
            request_id,
            document,
        })
    }

    pub(crate) fn rpc_reload_room_settings(
        &self,
        request_id: RequestId,
        server_id: ServerId,
        room_id: RoomId,
    ) -> Result<wire::RoomSettingsEvent, String> {
        self.require_active_room_settings_target(server_id, room_id)?;
        let source = self.config.load_runtime_document()?;
        let document = self.rpc_room_settings_document(
            &source.config,
            source.revision,
            server_id,
            room_id,
        )?;
        Ok(wire::RoomSettingsEvent::Document {
            request_id,
            document,
        })
    }

    pub(crate) fn rpc_save_room_settings(
        &mut self,
        request_id: RequestId,
        server_id: ServerId,
        room_id: RoomId,
        expected_revision: u64,
        values: wire::RoomSettingsValues,
    ) -> Result<wire::RoomSettingsEvent, String> {
        self.require_active_room_settings_target(server_id, room_id)?;
        values.validate()?;
        let mut source = self.config.load_runtime_document()?;
        if source.revision != expected_revision {
            let latest = self.rpc_room_settings_document(
                &source.config,
                source.revision,
                server_id,
                room_id,
            )?;
            return Ok(wire::RoomSettingsEvent::Conflict { request_id, latest });
        }

        let overrides = room_overrides(room_id, values);
        let server = source
            .config
            .server_by_id_mut(server_id)
            .ok_or_else(|| "server is no longer configured".to_string())?;
        let previous_history = server
            .rooms
            .iter()
            .find(|room| room.room_id == room_id)
            .map(|room| room.history.clone())
            .unwrap_or_default();
        let history_changed = previous_history != overrides.history;
        server.rooms.retain(|room| room.room_id != room_id);
        if !overrides.is_empty() {
            server.rooms.push(overrides);
            server.rooms.sort_by_key(|room| room.room_id);
        }

        let (_, revision) = match source
            .config
            .save_runtime_at_revision(expected_revision, false)
        {
            Ok(saved) => saved,
            Err(RuntimeSaveError::Conflict { .. }) => {
                let latest_source = self.config.load_runtime_document()?;
                let latest = self.rpc_room_settings_document(
                    &latest_source.config,
                    latest_source.revision,
                    server_id,
                    room_id,
                )?;
                return Ok(wire::RoomSettingsEvent::Conflict { request_id, latest });
            }
            Err(RuntimeSaveError::Other(error)) => return Err(error),
        };

        let committed_rooms = source
            .config
            .server_by_id(server_id)
            .expect("room settings server validated above")
            .rooms
            .clone();
        self.config
            .server_by_id_mut(server_id)
            .ok_or_else(|| "server is no longer configured".to_string())?
            .rooms = committed_rooms;
        self.push_file_policy();
        let document = self.rpc_room_settings_document(
            &source.config,
            revision,
            server_id,
            room_id,
        )?;
        Ok(wire::RoomSettingsEvent::Saved {
            request_id,
            document,
            history_reconnect_required: history_changed && self.network.is_some(),
        })
    }

    fn require_active_room_settings_target(
        &self,
        server_id: ServerId,
        room_id: RoomId,
    ) -> Result<(), String> {
        if self.room.active_server_id != Some(server_id) {
            return Err("room settings belong to a different active server".into());
        }
        if self.room.room_name_of(room_id).is_none() {
            return Err("room is no longer available".into());
        }
        Ok(())
    }

    fn rpc_room_settings_document(
        &self,
        config: &config::Config,
        revision: u64,
        server_id: ServerId,
        room_id: RoomId,
    ) -> Result<wire::RoomSettingsDocument, String> {
        self.require_active_room_settings_target(server_id, room_id)?;
        let server = config
            .server_by_id(server_id)
            .ok_or_else(|| "server is no longer configured".to_string())?;
        let values = server
            .rooms
            .iter()
            .find(|room| room.room_id == room_id)
            .map(values_from_overrides)
            .unwrap_or_else(default_values);
        Ok(wire::RoomSettingsDocument {
            server_id,
            server_name: server.label.clone(),
            room_id,
            room_name: self
                .room
                .room_name_of(room_id)
                .expect("room settings target validated above")
                .to_string(),
            revision,
            values,
            inherited: effective_settings(config, server, None),
            effective: effective_settings(config, server, Some(room_id)),
        })
    }
}

fn default_values() -> wire::RoomSettingsValues {
    wire::RoomSettingsValues {
        download: wire::DownloadOverride::Inherit,
        download_path: None,
        max_download_mb: None,
        history: wire::ToggleOverride::Inherit,
        history_location: None,
    }
}

fn values_from_overrides(overrides: &RoomOverrides) -> wire::RoomSettingsValues {
    let download = match overrides.files.download {
        None => wire::DownloadOverride::Inherit,
        Some(config::DownloadMode::Off) => wire::DownloadOverride::Off,
        Some(config::DownloadMode::Memory) => wire::DownloadOverride::Memory,
        Some(config::DownloadMode::Persistent) => wire::DownloadOverride::Persistent,
    };
    wire::RoomSettingsValues {
        download,
        download_path: (download == wire::DownloadOverride::Persistent)
            .then(|| overrides.files.download_dir.clone())
            .flatten(),
        max_download_mb: overrides.files.max_download_mb,
        history: match overrides.history.enabled {
            None => wire::ToggleOverride::Inherit,
            Some(true) => wire::ToggleOverride::On,
            Some(false) => wire::ToggleOverride::Off,
        },
        history_location: overrides.history.location.clone(),
    }
}

fn room_overrides(room_id: RoomId, values: wire::RoomSettingsValues) -> RoomOverrides {
    RoomOverrides {
        room_id,
        files: FileOverrides {
            download: match values.download {
                wire::DownloadOverride::Inherit => None,
                wire::DownloadOverride::Off => Some(config::DownloadMode::Off),
                wire::DownloadOverride::Memory => Some(config::DownloadMode::Memory),
                wire::DownloadOverride::Persistent => Some(config::DownloadMode::Persistent),
            },
            download_dir: values.download_path,
            max_download_mb: values.max_download_mb,
        },
        history: HistoryOverrides {
            enabled: match values.history {
                wire::ToggleOverride::Inherit => None,
                wire::ToggleOverride::On => Some(true),
                wire::ToggleOverride::Off => Some(false),
            },
            location: values.history_location,
        },
    }
}

fn effective_settings(
    config: &config::Config,
    server: &config::ServerEntry,
    room_id: Option<RoomId>,
) -> wire::EffectiveRoomSettings {
    let files = config.effective_files(server, room_id);
    let (download, download_path) = match files.target {
        DownloadTarget::Off => (wire::DownloadMode::Off, None),
        DownloadTarget::Memory => (wire::DownloadMode::Memory, None),
        DownloadTarget::Persistent(path) => (
            wire::DownloadMode::Persistent,
            Some(path.to_string_lossy().into_owned()),
        ),
    };
    let history = config.effective_history(server, room_id);
    wire::EffectiveRoomSettings {
        download,
        download_path,
        max_download_mb: files.max_download_bytes / config::MIB,
        history_enabled: history.enabled,
        history_location: history
            .location
            .map(|path| path.to_string_lossy().into_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_override_values_preserve_inheritance() {
        let values = default_values();
        let overrides = room_overrides(RoomId(7), values.clone());
        assert!(overrides.is_empty());
        assert_eq!(values_from_overrides(&overrides), values);
    }
}
