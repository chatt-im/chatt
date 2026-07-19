use jsony::Jsony;

use crate::ids::{FileTransferId, MessageId, RoomId, UserId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct RequestId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct AttachmentId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct BulkTransferId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct DaemonInstanceId(pub [u8; 16]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ConnectionState {
    Offline,
    Connecting,
    Online,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum RoomKind {
    Public,
    Private,
    Direct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum TrustState {
    NotApplicable,
    Unverified,
    Verified,
    Changed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum MediaKind {
    File,
    Image,
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct AttachmentDescriptor {
    pub id: AttachmentId,
    pub file_name: String,
    pub media_kind: MediaKind,
    pub content_type: String,
    pub byte_len: u64,
    pub digest: [u8; 32],
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct MessageReference {
    pub message_id: MessageId,
    pub sender_name: String,
    pub excerpt: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct Message {
    pub room_id: RoomId,
    pub message_id: MessageId,
    pub sender_id: UserId,
    pub sender_name: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub local: bool,
    pub edited: bool,
    pub unverified: bool,
    pub notice: bool,
    pub reference: Option<MessageReference>,
    pub attachment: Option<AttachmentDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RoomSummary {
    pub id: RoomId,
    pub name: String,
    pub kind: RoomKind,
    pub unread: u32,
    pub behind_head: bool,
    pub voice_active: bool,
    pub trust: TrustState,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct Participant {
    pub user_id: UserId,
    pub name: String,
    pub online: bool,
    pub speaking: bool,
    pub muted: bool,
    pub deafened: bool,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct VoiceState {
    pub muted: bool,
    pub deafened: bool,
    pub output_volume: f32,
    pub joined_room: Option<RoomId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum TransferDirection {
    Upload,
    Download,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum TransferStatus {
    Starting,
    Active,
    Complete,
    Canceled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct TransferSummary {
    pub transfer_id: FileTransferId,
    pub room_id: RoomId,
    pub direction: TransferDirection,
    pub file_name: String,
    pub byte_len: u64,
    pub transferred: u64,
    pub status: TransferStatus,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RoomSnapshot {
    pub room_id: RoomId,
    pub messages: Vec<Message>,
    pub older_cursor: Option<MessageId>,
    pub at_start: bool,
    pub participants: Vec<Participant>,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct StateSnapshot {
    pub connection: ConnectionState,
    pub active_server: Option<String>,
    pub local_identity: Option<String>,
    pub rooms: Vec<RoomSummary>,
    pub selected_room: Option<RoomId>,
    pub room: Option<RoomSnapshot>,
    pub voice: VoiceState,
    pub transfers: Vec<TransferSummary>,
}

impl StateSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        if self.rooms.len() > super::MAX_ROOMS {
            return Err("room collection exceeds limit".into());
        }
        if self.transfers.len() > super::MAX_TRANSFERS {
            return Err("transfer collection exceeds limit".into());
        }
        check_opt_string(&self.active_server)?;
        check_opt_string(&self.local_identity)?;
        for room in &self.rooms {
            room.validate()?;
        }
        if self
            .rooms
            .windows(2)
            .any(|rooms| rooms[0].id >= rooms[1].id)
        {
            return Err("room catalog must be strictly ordered by id".into());
        }
        if let Some(room) = &self.room {
            room.validate()?;
        }
        if self.room.as_ref().map(|room| room.room_id) != self.selected_room {
            return Err("selected room and room snapshot do not match".into());
        }
        if self
            .selected_room
            .is_some_and(|selected| !self.rooms.iter().any(|room| room.id == selected))
        {
            return Err("selected room is absent from room catalog".into());
        }
        self.voice.validate()?;
        for transfer in &self.transfers {
            transfer.validate()?;
            if Some(transfer.room_id) != self.selected_room {
                return Err("transfer belongs to a room other than the selected room".into());
            }
        }
        for (index, transfer) in self.transfers.iter().enumerate() {
            if self.transfers[..index]
                .iter()
                .any(|other| other.transfer_id == transfer.transfer_id)
            {
                return Err("duplicate transfer id".into());
            }
        }
        Ok(())
    }
}

impl Message {
    pub fn validate(&self) -> Result<(), String> {
        check_string(&self.sender_name)?;
        if self.body.len() > super::MAX_MESSAGE_BODY_BYTES {
            return Err("message body exceeds limit".into());
        }
        if let Some(attachment) = &self.attachment {
            attachment.validate()?;
        }
        if let Some(reference) = &self.reference {
            check_string(&reference.sender_name)?;
            check_string(&reference.excerpt)?;
        }
        Ok(())
    }
}

impl AttachmentDescriptor {
    pub fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.file_name)?;
        check_nonempty_string(&self.content_type)?;
        match (self.width, self.height) {
            (Some(width), Some(height)) if width != 0 && height != 0 => Ok(()),
            (None, None) => Ok(()),
            _ => Err("attachment dimensions must be a nonzero pair".into()),
        }
    }
}

impl RoomSummary {
    pub fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.name)
    }
}

impl Participant {
    pub fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.name)
    }
}

impl VoiceState {
    pub fn validate(&self) -> Result<(), String> {
        if !self.output_volume.is_finite()
            || !(0.0..=super::MAX_OUTPUT_VOLUME_PERCENT).contains(&self.output_volume)
        {
            return Err("output volume is outside the supported range".into());
        }
        Ok(())
    }
}

impl TransferSummary {
    pub fn validate(&self) -> Result<(), String> {
        if self.transfer_id.0 == 0 {
            return Err("transfer id must be nonzero".into());
        }
        check_nonempty_string(&self.file_name)?;
        if self.transferred > self.byte_len {
            return Err("transfer progress exceeds declared length".into());
        }
        if let Some(error) = &self.error {
            check_string(error)?;
        }
        Ok(())
    }
}

impl RoomSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        if self.messages.len() > super::MAX_MESSAGES {
            return Err("message collection exceeds limit".into());
        }
        if self.participants.len() > super::MAX_PARTICIPANTS {
            return Err("participant collection exceeds limit".into());
        }
        for message in &self.messages {
            if message.room_id != self.room_id {
                return Err("message belongs to a different room".into());
            }
            message.validate()?;
        }
        if self
            .messages
            .windows(2)
            .any(|messages| messages[0].message_id >= messages[1].message_id)
        {
            return Err("messages must be strictly ordered by id".into());
        }
        for participant in &self.participants {
            participant.validate()?;
        }
        Ok(())
    }
}

pub(super) fn check_string(value: &str) -> Result<(), String> {
    (value.len() <= super::MAX_STRING_BYTES)
        .then_some(())
        .ok_or_else(|| "string exceeds limit".into())
}

pub(super) fn check_nonempty_string(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("string must not be empty".into());
    }
    check_string(value)
}

pub(super) fn check_opt_string(value: &Option<String>) -> Result<(), String> {
    value.as_deref().map_or(Ok(()), check_string)
}
