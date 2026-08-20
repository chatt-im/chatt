use jsony::Jsony;

use crate::{
    ids::{RoomId, ServerId},
    model::RequestId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DownloadMode {
    Off,
    Memory,
    Persistent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DownloadOverride {
    Inherit,
    Off,
    Memory,
    Persistent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ToggleOverride {
    Inherit,
    On,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RoomSettingsValues {
    pub download: DownloadOverride,
    /// A custom directory for a persistent override. `None` uses the inherited
    /// or platform-default persistent directory.
    pub download_path: Option<String>,
    pub max_download_mb: Option<u64>,
    pub history: ToggleOverride,
    pub history_location: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct EffectiveRoomSettings {
    pub download: DownloadMode,
    pub download_path: Option<String>,
    pub max_download_mb: u64,
    pub history_enabled: bool,
    pub history_location: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RoomSettingsDocument {
    pub server_id: ServerId,
    pub server_name: String,
    pub room_id: RoomId,
    pub room_name: String,
    pub revision: u64,
    pub values: RoomSettingsValues,
    pub inherited: EffectiveRoomSettings,
    pub effective: EffectiveRoomSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum RoomSettingsCommand {
    Open {
        room_id: RoomId,
    },
    Reload {
        server_id: ServerId,
        room_id: RoomId,
    },
    Save {
        server_id: ServerId,
        room_id: RoomId,
        expected_revision: u64,
        values: RoomSettingsValues,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum RoomSettingsEvent {
    Document {
        request_id: RequestId,
        document: RoomSettingsDocument,
    },
    Saved {
        request_id: RequestId,
        document: RoomSettingsDocument,
        history_reconnect_required: bool,
    },
    Conflict {
        request_id: RequestId,
        latest: RoomSettingsDocument,
    },
}

impl RoomSettingsValues {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_download_mb == Some(0) {
            return Err("room download limit must be a positive MiB count".into());
        }
        if self.download == DownloadOverride::Persistent {
            if let Some(path) = self.download_path.as_deref() {
                crate::model::check_nonempty_string(path)?;
            }
        } else if self.download_path.is_some() {
            return Err("room download path is only valid for persistent downloads".into());
        }
        if let Some(location) = &self.history_location {
            crate::model::check_nonempty_string(location)?;
        }
        Ok(())
    }
}

impl RoomSettingsDocument {
    pub fn validate(&self) -> Result<(), String> {
        if self.server_id.0 == [0; 16] || self.room_id.0 == 0 {
            return Err("room settings identity is invalid".into());
        }
        crate::model::check_string(&self.server_name)?;
        crate::model::check_string(&self.room_name)?;
        self.values.validate()?;
        self.inherited.validate()?;
        self.effective.validate()
    }
}

impl EffectiveRoomSettings {
    fn validate(&self) -> Result<(), String> {
        if self.max_download_mb == 0 {
            return Err("effective room download limit must be positive".into());
        }
        match (&self.download, &self.download_path) {
            (DownloadMode::Persistent, Some(path)) => crate::model::check_nonempty_string(path)?,
            (DownloadMode::Persistent, None) => {
                return Err("effective persistent downloads require a path".into());
            }
            (_, Some(_)) => return Err("effective download path requires persistent mode".into()),
            _ => {}
        }
        if let Some(location) = &self.history_location {
            crate::model::check_nonempty_string(location)?;
        }
        Ok(())
    }
}

impl RoomSettingsCommand {
    pub fn validate(&self) -> Result<(), String> {
        let (server_id, room_id) = match self {
            Self::Open { room_id } => {
                if room_id.0 == 0 {
                    return Err("room settings room id must be nonzero".into());
                }
                return Ok(());
            }
            Self::Reload { server_id, room_id } => (*server_id, *room_id),
            Self::Save {
                server_id,
                room_id,
                values,
                ..
            } => {
                values.validate()?;
                (*server_id, *room_id)
            }
        };
        if server_id.0 == [0; 16] || room_id.0 == 0 {
            return Err("room settings identity is invalid".into());
        }
        Ok(())
    }
}

impl RoomSettingsEvent {
    pub fn request_id(&self) -> RequestId {
        match self {
            Self::Document { request_id, .. }
            | Self::Saved { request_id, .. }
            | Self::Conflict { request_id, .. } => *request_id,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.request_id().0 == 0 {
            return Err("request id must be nonzero".into());
        }
        match self {
            Self::Document { document, .. } | Self::Saved { document, .. } => document.validate(),
            Self::Conflict { latest, .. } => latest.validate(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{
        ClientFrame, DaemonFrame, decode_client, decode_daemon, encode_client, encode_daemon,
    };

    #[test]
    fn room_settings_frames_round_trip() {
        let command = ClientFrame::RoomSettings {
            request_id: RequestId(3),
            command: RoomSettingsCommand::Open { room_id: RoomId(9) },
        };
        assert_eq!(
            decode_client(&encode_client(&command).unwrap()).unwrap(),
            command
        );

        let event = DaemonFrame::RoomSettings(RoomSettingsEvent::Document {
            request_id: RequestId(3),
            document: RoomSettingsDocument {
                server_id: ServerId(*b"room-settings-id"),
                server_name: "work".into(),
                room_id: RoomId(9),
                room_name: "general".into(),
                revision: 7,
                values: RoomSettingsValues {
                    download: DownloadOverride::Inherit,
                    download_path: None,
                    max_download_mb: None,
                    history: ToggleOverride::Inherit,
                    history_location: None,
                },
                inherited: EffectiveRoomSettings {
                    download: DownloadMode::Memory,
                    download_path: None,
                    max_download_mb: 50,
                    history_enabled: true,
                    history_location: None,
                },
                effective: EffectiveRoomSettings {
                    download: DownloadMode::Memory,
                    download_path: None,
                    max_download_mb: 50,
                    history_enabled: true,
                    history_location: None,
                },
            },
        });
        assert_eq!(
            decode_daemon(&encode_daemon(&event).unwrap()).unwrap(),
            event
        );
    }

    #[test]
    fn first_revision_and_unnamed_room_settings_round_trip() {
        let values = RoomSettingsValues {
            download: DownloadOverride::Persistent,
            download_path: None,
            max_download_mb: None,
            history: ToggleOverride::Inherit,
            history_location: None,
        };
        let command = ClientFrame::RoomSettings {
            request_id: RequestId(4),
            command: RoomSettingsCommand::Save {
                server_id: ServerId(*b"room-settings-id"),
                room_id: RoomId(9),
                expected_revision: 0,
                values: values.clone(),
            },
        };
        assert_eq!(
            decode_client(&encode_client(&command).unwrap()).unwrap(),
            command
        );

        let event = DaemonFrame::RoomSettings(RoomSettingsEvent::Document {
            request_id: RequestId(4),
            document: RoomSettingsDocument {
                server_id: ServerId(*b"room-settings-id"),
                server_name: String::new(),
                room_id: RoomId(9),
                room_name: String::new(),
                revision: 0,
                values,
                inherited: EffectiveRoomSettings {
                    download: DownloadMode::Persistent,
                    download_path: Some("files".into()),
                    max_download_mb: 50,
                    history_enabled: false,
                    history_location: None,
                },
                effective: EffectiveRoomSettings {
                    download: DownloadMode::Persistent,
                    download_path: Some("files".into()),
                    max_download_mb: 50,
                    history_enabled: false,
                    history_location: None,
                },
            },
        });
        assert_eq!(
            decode_daemon(&encode_daemon(&event).unwrap()).unwrap(),
            event
        );
    }

    #[test]
    fn rejects_zero_download_limits() {
        let values = RoomSettingsValues {
            download: DownloadOverride::Inherit,
            download_path: None,
            max_download_mb: Some(0),
            history: ToggleOverride::Inherit,
            history_location: None,
        };
        assert_eq!(
            values.validate(),
            Err("room download limit must be a positive MiB count".into())
        );

        let effective = EffectiveRoomSettings {
            download: DownloadMode::Memory,
            download_path: None,
            max_download_mb: 0,
            history_enabled: true,
            history_location: None,
        };
        assert_eq!(
            effective.validate(),
            Err("effective room download limit must be positive".into())
        );
    }
}
