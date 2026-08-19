use jsony::Jsony;

use crate::{
    ids::{RoomId, UserId},
    model::RequestId,
};

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DirectoryPresence {
    Online {
        voice_room_id: Option<RoomId>,
        voice_room_name: Option<String>,
    },
    AwaySeen {
        away_for_ms: u64,
    },
    AwayUnseen,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DirectoryUser {
    pub user_id: UserId,
    pub name: String,
    pub local: bool,
    pub presence: DirectoryPresence,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum DirectoryCommand {
    Open,
    OpenDm { user_id: UserId },
    GetParticipantAudio { user_id: UserId },
    SetParticipantMuted { user_id: UserId, muted: bool },
    PreviewParticipantGain { user_id: UserId, gain_db: f32 },
    CommitParticipantGain { user_id: UserId, gain_db: f32 },
    CancelParticipantGain { user_id: UserId },
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct ParticipantAudioState {
    pub user_id: UserId,
    pub muted: bool,
    pub gain_db: f32,
    pub minimum_gain_db: f32,
    pub maximum_gain_db: f32,
    pub previewing: bool,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum DirectoryEvent {
    Users {
        request_id: Option<RequestId>,
        users: Vec<DirectoryUser>,
    },
    ParticipantAudio {
        request_id: Option<RequestId>,
        state: ParticipantAudioState,
    },
}

impl DirectoryCommand {
    pub fn validate(&self) -> Result<(), String> {
        let user_id = match self {
            Self::Open => return Ok(()),
            Self::OpenDm { user_id }
            | Self::GetParticipantAudio { user_id }
            | Self::SetParticipantMuted { user_id, .. }
            | Self::PreviewParticipantGain { user_id, .. }
            | Self::CommitParticipantGain { user_id, .. }
            | Self::CancelParticipantGain { user_id } => *user_id,
        };
        if user_id.0 == 0 {
            return Err("directory user id must be nonzero".into());
        }
        match self {
            Self::PreviewParticipantGain { gain_db, .. }
            | Self::CommitParticipantGain { gain_db, .. }
                if !gain_db.is_finite() =>
            {
                Err("participant gain must be finite".into())
            }
            _ => Ok(()),
        }
    }
}

impl DirectoryEvent {
    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::Users { request_id, .. } | Self::ParticipantAudio { request_id, .. } => {
                *request_id
            }
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self
            .request_id()
            .is_some_and(|request_id| request_id.0 == 0)
        {
            return Err("request id must be nonzero".into());
        }
        match self {
            Self::Users { users, .. } => {
                if users.len() > crate::MAX_DIRECTORY_USERS {
                    return Err("user directory exceeds limit".into());
                }
                for user in users {
                    if user.user_id.0 == 0 {
                        return Err("directory user id must be nonzero".into());
                    }
                    crate::model::check_string(&user.name)?;
                    if let DirectoryPresence::Online {
                        voice_room_id,
                        voice_room_name,
                    } = &user.presence
                    {
                        if voice_room_id.is_some() != voice_room_name.is_some() {
                            return Err("directory voice room identity is incomplete".into());
                        }
                        if voice_room_id.is_some_and(|room_id| room_id.0 == 0) {
                            return Err("directory voice room id must be nonzero".into());
                        }
                        if let Some(name) = voice_room_name {
                            crate::model::check_nonempty_string(name)?;
                        }
                    }
                }
                Ok(())
            }
            Self::ParticipantAudio { state, .. } => state.validate(),
        }
    }
}

impl ParticipantAudioState {
    fn validate(&self) -> Result<(), String> {
        if self.user_id.0 == 0 {
            return Err("participant audio user id must be nonzero".into());
        }
        if !self.gain_db.is_finite()
            || !self.minimum_gain_db.is_finite()
            || !self.maximum_gain_db.is_finite()
            || self.minimum_gain_db > self.maximum_gain_db
            || !(self.minimum_gain_db..=self.maximum_gain_db).contains(&self.gain_db)
        {
            return Err("participant gain is outside the supported range".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{
        ClientFrame, DaemonFrame, decode_client, decode_daemon, encode_client, encode_daemon,
    };

    #[test]
    fn directory_and_participant_audio_frames_round_trip() {
        let command = ClientFrame::Directory {
            request_id: RequestId(4),
            command: DirectoryCommand::CommitParticipantGain {
                user_id: UserId(7),
                gain_db: -6.0,
            },
        };
        assert_eq!(
            decode_client(&encode_client(&command).unwrap()).unwrap(),
            command
        );
        let event = DaemonFrame::Directory(DirectoryEvent::Users {
            request_id: Some(RequestId(4)),
            users: vec![DirectoryUser {
                user_id: UserId(7),
                name: "alice".into(),
                local: false,
                presence: DirectoryPresence::Online {
                    voice_room_id: Some(RoomId(2)),
                    voice_room_name: Some("Lobby".into()),
                },
            }],
        });
        assert_eq!(
            decode_daemon(&encode_daemon(&event).unwrap()).unwrap(),
            event
        );
    }

    #[test]
    fn unnamed_directory_user_round_trips() {
        let event = DaemonFrame::Directory(DirectoryEvent::Users {
            request_id: Some(RequestId(5)),
            users: vec![DirectoryUser {
                user_id: UserId(8),
                name: String::new(),
                local: false,
                presence: DirectoryPresence::AwayUnseen,
            }],
        });

        assert_eq!(
            decode_daemon(&encode_daemon(&event).unwrap()).unwrap(),
            event
        );
    }

    #[test]
    fn rejects_nonfinite_gain_and_incomplete_voice_room_identity() {
        let command = ClientFrame::Directory {
            request_id: RequestId(1),
            command: DirectoryCommand::PreviewParticipantGain {
                user_id: UserId(2),
                gain_db: f32::NAN,
            },
        };
        assert!(encode_client(&command).is_err());
        let event = DaemonFrame::Directory(DirectoryEvent::Users {
            request_id: Some(RequestId(1)),
            users: vec![DirectoryUser {
                user_id: UserId(2),
                name: "alice".into(),
                local: false,
                presence: DirectoryPresence::Online {
                    voice_room_id: Some(RoomId(2)),
                    voice_room_name: None,
                },
            }],
        });
        assert!(encode_daemon(&event).is_err());
    }
}
