use jsony::Jsony;
use kvlog::{Encode, ValueEncoder};

use crate::ids::{FileTransferId, MessageId, RoomId, StreamId, UserId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct RequestId(pub u64);

impl Encode for RequestId {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        self.0.encode_log_value_into(output);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct AttachmentId {
    pub timestamp_ms: u64,
    pub transfer_id: FileTransferId,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct BulkTransferId(pub u64);

impl Encode for BulkTransferId {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        self.0.encode_log_value_into(output);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary)]
pub struct DaemonInstanceId(pub [u8; 16]);

impl Encode for DaemonInstanceId {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        uuid::Uuid::from_bytes(self.0).encode_log_value_into(output);
    }
}

#[cfg(test)]
mod log_encoding_tests {
    use kvlog::{
        Encode,
        encoding::{Encoder, Value},
    };

    use super::{AttachmentId, BulkTransferId, DaemonInstanceId, RequestId};
    use crate::ids::FileTransferId;

    fn with_encoded_value(value: &impl Encode, check: impl FnOnce(Value<'_>)) {
        let mut encoder = Encoder::new();
        {
            let mut fields = encoder.append(kvlog::LogLevel::Info, 0);
            value.encode_log_value_into(fields.dynamic_key("value"));
        }
        let (_, _, _, mut fields) = kvlog::encoding::decode(encoder.bytes())
            .next()
            .unwrap()
            .unwrap();
        check(fields.next().unwrap().unwrap().1);
    }

    #[test]
    fn integer_rpc_ids_delegate_to_integer_encoding() {
        with_encoded_value(&RequestId(17), |value| {
            assert!(matches!(value, Value::U64(17)));
        });
        with_encoded_value(&BulkTransferId(23), |value| {
            assert!(matches!(value, Value::U64(23)));
        });
    }

    #[test]
    fn daemon_instance_ids_use_uuid_encoding_without_changing_bytes() {
        let bytes = *b"0123456789abcdef";
        with_encoded_value(&DaemonInstanceId(bytes), |value| match value {
            Value::UUID(uuid) => assert_eq!(uuid.as_bytes(), &bytes),
            _ => panic!("expected UUID encoding"),
        });
    }

    #[test]
    fn composite_attachment_ids_expose_stable_components() {
        let id = AttachmentId {
            timestamp_ms: 1_234,
            transfer_id: FileTransferId(56),
        };
        let mut encoder = Encoder::new();
        {
            let mut fields = encoder.append(kvlog::LogLevel::Info, 0);
            id.timestamp_ms
                .encode_log_value_into(fields.dynamic_key("attachment_timestamp_ms"));
            id.transfer_id
                .encode_log_value_into(fields.dynamic_key("attachment_transfer_id"));
        }
        let (_, _, _, fields) = kvlog::encoding::decode(encoder.bytes())
            .next()
            .unwrap()
            .unwrap();
        let values = fields
            .map(Result::unwrap)
            .map(|(key, value)| (key.as_str().unwrap().to_owned(), value))
            .collect::<Vec<_>>();
        assert!(
            values
                .iter()
                .any(|(key, value)| key == "attachment_timestamp_ms"
                    && matches!(value, Value::U64(1_234)))
        );
        assert!(
            values
                .iter()
                .any(|(key, value)| key == "attachment_transfer_id"
                    && matches!(value, Value::U64(56)))
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ConnectionState {
    Offline,
    Connecting,
    Online,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ServerAvailability {
    Ready,
    PairingIncomplete,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ServerSummary {
    pub label: String,
    pub username: String,
    pub tcp_addr: String,
    pub require_transport_encryption: bool,
    pub availability: ServerAvailability,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ServerSelectionError {
    pub label: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ServerSelectionPrompt {
    AllowUnencryptedTransport { label: String, attempt_id: u64 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ServerSelectionState {
    pub servers: Vec<ServerSummary>,
    pub error: Option<ServerSelectionError>,
    pub prompt: Option<ServerSelectionPrompt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Jsony)]
#[jsony(Binary, version)]
pub enum CommandArgKind {
    None,
    User,
    Room,
    Sound,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Jsony)]
#[jsony(Binary, version)]
pub enum CommandCandidateKind {
    User,
    Room,
    Sound,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct CommandInfo {
    pub name: String,
    pub usage: String,
    pub description: String,
    pub arg: CommandArgKind,
    pub placeholder: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct CommandCandidate {
    pub value: String,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct CommandOutputLine {
    pub error: bool,
    pub text: String,
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

impl Encode for MediaKind {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        let value = match self {
            Self::File => "file",
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
        };
        value.encode_log_value_into(output);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct AttachmentDescriptor {
    pub id: AttachmentId,
    pub file_name: String,
    pub media_kind: MediaKind,
    pub content_type: String,
    pub byte_len: u64,
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
    pub reference: Option<MessageReference>,
    pub attachment: Option<AttachmentDescriptor>,
}

/// A daemon-session room timeline entry produced by the application rather
/// than by server chat. This generic shape is intentionally source-agnostic so
/// future system messages do not require another protocol change.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SystemMessage {
    pub room_id: RoomId,
    pub system_id: u64,
    /// Chat message after which this row was created. Renderers merge missing
    /// anchors by numeric position, then order equal anchors by `system_id`.
    pub after_message_id: Option<MessageId>,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub level: SystemMessageLevel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum SystemMessageLevel {
    #[default]
    Info,
    Warning,
    Error,
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
    pub voice_state: VoiceState,
}

/// The call this client is in: the room it belongs to and everyone in it.
///
/// The two travel together so a renderer can never draw one call's members
/// under another's header — the room is part of the value, not a field
/// alongside it that a consumer may ignore.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct VoiceRoster {
    pub room_id: RoomId,
    pub members: Vec<VoiceMember>,
}

/// One member of a voice call, with everything a renderer needs to draw a
/// roster row.
///
/// Kept separate from [`Participant`] because the two change on wildly
/// different cadences: a room's participant list is the whole server directory
/// for a public room and changes rarely, while [`VoiceMemberStatus::speaking`]
/// flips on every talk spurt. Folding these fields into `Participant` would
/// re-send thousands of names several times a second.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct VoiceMember {
    pub user_id: UserId,
    pub name: String,
    pub is_local: bool,
    /// UNIX milliseconds this member's current call membership began.
    ///
    /// Only joins the daemon observed are exact: a member already in the call
    /// when the daemon connected is stamped at connect time, because the server
    /// relays call occupancy as a bare set with no per-member join time.
    pub joined_ms: u64,
    pub status: VoiceMemberStatus,
}

/// The half of a roster row that moves while the call runs.
///
/// Split out so a talk spurt costs one of these rather than the whole roster:
/// [`super::frame::StateDelta::VoiceMembersUpdated`] carries only the rows whose
/// status changed, while the identifying half above is re-sent only when
/// membership itself does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct VoiceMemberStatus {
    /// This member's own mute/deafen. A renderer showing the local row should
    /// prefer [`VoiceSessionState::state`], which leads this optimistically.
    pub voice_state: VoiceState,
    pub speaking: bool,
    /// Whether this member's audio takes a direct peer-to-peer path rather than
    /// the server relay.
    pub p2p_direct: bool,
    /// Mouth-to-ear estimate for audio arriving from this member: their jitter
    /// buffer, the output ring, and the one-way network leg. `None` while no
    /// fresh reception report backs it.
    pub inbound_latency_ms: Option<u16>,
    /// The same estimate for the audio this member receives from us, derived
    /// from their reception reports about our stream.
    pub outbound_latency_ms: Option<u16>,
}

/// One member's [`VoiceMemberStatus`], addressed by user id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct VoiceMemberUpdate {
    pub user_id: UserId,
    pub status: VoiceMemberStatus,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum VoiceState {
    #[default]
    Live,
    Muted,
    Deafened,
}

impl VoiceState {
    pub fn is_muted(self) -> bool {
        !matches!(self, Self::Live)
    }

    pub fn is_deafened(self) -> bool {
        matches!(self, Self::Deafened)
    }

    /// Target state for a mute/unmute toggle.
    ///
    /// Deafened is also muted, so "unmute" always returns fully to Live.
    pub fn toggle_mute(self) -> Self {
        match self {
            Self::Live => Self::Muted,
            Self::Muted | Self::Deafened => Self::Live,
        }
    }

    /// Target state for a deafen/undeafen toggle.
    ///
    /// Undeafening returns fully to Live instead of retaining hidden mute state.
    pub fn toggle_deafen(self) -> Self {
        match self {
            Self::Deafened => Self::Live,
            Self::Live | Self::Muted => Self::Deafened,
        }
    }
}

#[cfg(test)]
mod voice_state_tests {
    use super::VoiceState;

    #[test]
    fn toggle_transitions_are_exhaustive() {
        let cases = [
            (VoiceState::Live, VoiceState::Muted, VoiceState::Deafened),
            (VoiceState::Muted, VoiceState::Live, VoiceState::Deafened),
            (VoiceState::Deafened, VoiceState::Live, VoiceState::Live),
        ];
        for (state, mute_target, deafen_target) in cases {
            assert_eq!(state.toggle_mute(), mute_target, "mute from {state:?}");
            assert_eq!(
                state.toggle_deafen(),
                deafen_target,
                "deafen from {state:?}"
            );
        }
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct VoiceSessionState {
    pub state: VoiceState,
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
pub struct LiveShare {
    pub room_id: RoomId,
    pub stream_id: StreamId,
    /// Daemon-local identity for this use of `stream_id`.
    pub generation: u64,
    pub sender_id: UserId,
    pub sender_name: String,
    pub codec: String,
    pub coded_width: u32,
    pub coded_height: u32,
    pub extradata: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum LiveShareViewStatus {
    WaitingForKeyframe,
    Reconnecting,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RoomSnapshot {
    pub room_id: RoomId,
    /// Server-continuity generation for rejecting stale room projections.
    pub room_generation: u64,
    /// Canonical resident-history revision represented by this snapshot.
    pub history_revision: u64,
    pub messages: Vec<Message>,
    pub system_messages: Vec<SystemMessage>,
    pub older_cursor: Option<MessageId>,
    pub at_start: bool,
    pub participants: Vec<Participant>,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct StateSnapshot {
    pub connection: ConnectionState,
    pub active_server: Option<String>,
    pub server_selection: ServerSelectionState,
    pub local_identity: Option<String>,
    pub rooms: Vec<RoomSummary>,
    pub selected_room: Option<RoomId>,
    pub room: Option<RoomSnapshot>,
    pub voice: VoiceSessionState,
    /// The call this client is in, if any. Global rather than per-room because
    /// [`VoiceSessionState::joined_room`] is independent of the selected room —
    /// a renderer draws the call it is *in*, not the room it is looking at.
    pub voice_roster: Option<VoiceRoster>,
    pub transfers: Vec<TransferSummary>,
    pub live_shares: Vec<LiveShare>,
}

impl StateSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        if self.rooms.len() > super::MAX_ROOMS {
            return Err("room collection exceeds limit".into());
        }
        if self.transfers.len() > super::MAX_TRANSFERS {
            return Err("transfer collection exceeds limit".into());
        }
        if self.live_shares.len() > super::MAX_LIVE_SHARES {
            return Err("live share collection exceeds limit".into());
        }
        check_opt_string(&self.active_server)?;
        self.server_selection.validate()?;
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
        if let Some(roster) = &self.voice_roster {
            roster.validate()?;
        }
        // The roster and the session state must name the same call. A snapshot
        // that disagrees would let a renderer draw one call's members under
        // another's header for as long as it took the next frame to arrive.
        if self.voice_roster.as_ref().map(|roster| roster.room_id) != self.voice.joined_room {
            return Err("voice roster and joined call do not match".into());
        }
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
        for share in &self.live_shares {
            share.validate()?;
        }
        if self
            .live_shares
            .windows(2)
            .any(|shares| shares[0].stream_id >= shares[1].stream_id)
        {
            return Err("live shares must be strictly ordered by stream id".into());
        }
        Ok(())
    }
}

impl ServerSelectionState {
    pub fn validate(&self) -> Result<(), String> {
        if self.servers.len() > super::MAX_SERVERS {
            return Err("server collection exceeds limit".into());
        }
        for server in &self.servers {
            server.validate()?;
        }
        for (index, server) in self.servers.iter().enumerate() {
            if self.servers[..index]
                .iter()
                .any(|other| other.label == server.label)
            {
                return Err("server labels must be unique".into());
            }
        }
        if let Some(error) = &self.error {
            error.validate()?;
        }
        if let Some(prompt) = &self.prompt {
            prompt.validate()?;
        }
        if self.error.is_some() && self.prompt.is_some() {
            return Err("server selection cannot contain an error and prompt".into());
        }
        Ok(())
    }
}

impl ServerSummary {
    pub fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.label)?;
        check_string(&self.username)?;
        check_nonempty_string(&self.tcp_addr)
    }
}

impl ServerSelectionError {
    pub fn validate(&self) -> Result<(), String> {
        check_opt_string(&self.label)?;
        check_nonempty_string(&self.message)
    }
}

impl ServerSelectionPrompt {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::AllowUnencryptedTransport { label, attempt_id } => {
                check_nonempty_string(label)?;
                if *attempt_id == 0 {
                    return Err("server selection prompt attempt id must be nonzero".into());
                }
                Ok(())
            }
        }
    }
}

impl LiveShare {
    pub fn validate(&self) -> Result<(), String> {
        if self.stream_id.0 == 0 || self.generation == 0 || self.sender_id.0 == 0 {
            return Err("live share identity must be nonzero".into());
        }
        check_nonempty_string(&self.sender_name)?;
        check_nonempty_string(&self.codec)?;
        if self.coded_width == 0 || self.coded_height == 0 {
            return Err("live share dimensions must be nonzero".into());
        }
        if self.extradata.len() > super::MAX_FRAME_BYTES {
            return Err("live share codec data exceeds limit".into());
        }
        Ok(())
    }
}

impl CommandInfo {
    pub fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.name)?;
        check_nonempty_string(&self.usage)?;
        check_nonempty_string(&self.description)?;
        check_opt_string(&self.placeholder)?;
        if !self.name.starts_with('/')
            || self.name.len() == 1
            || self.name.chars().any(char::is_whitespace)
        {
            return Err("command name is invalid".into());
        }
        if self.arg == CommandArgKind::Free && self.placeholder.is_none() {
            return Err("free-text command is missing its placeholder".into());
        }
        if self.arg != CommandArgKind::Free && self.placeholder.is_some() {
            return Err("non-free command must not carry a placeholder".into());
        }
        Ok(())
    }
}

impl CommandCandidate {
    pub fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.value)?;
        check_opt_string(&self.detail)
    }
}

impl CommandOutputLine {
    pub fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.text)
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

impl SystemMessage {
    pub fn validate(&self) -> Result<(), String> {
        if self.system_id == 0 {
            return Err("system message id must be nonzero".into());
        }
        check_nonempty_string(&self.sender)?;
        if self.body.is_empty() {
            return Err("system message body must not be empty".into());
        }
        if self.body.len() > super::MAX_MESSAGE_BODY_BYTES {
            return Err("system message body exceeds limit".into());
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

impl VoiceRoster {
    pub fn validate(&self) -> Result<(), String> {
        check_voice_members(&self.members)
    }
}

impl VoiceMember {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("string must not be empty".into());
        }
        // Bounded by what the server accepts as a username rather than by the
        // generic string cap: a roster carries one name per caller, and the
        // 16 KiB default would let a full call claim a far larger frame than
        // any real one can need.
        (self.name.len() <= super::MAX_USERNAME_BYTES)
            .then_some(())
            .ok_or_else(|| "voice member name exceeds the username limit".into())
    }
}

impl VoiceSessionState {
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
        if self.system_messages.len() > super::MAX_MESSAGES {
            return Err("system message collection exceeds limit".into());
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
        for message in &self.system_messages {
            if message.room_id != self.room_id {
                return Err("system message belongs to a different room".into());
            }
            message.validate()?;
        }
        if self
            .system_messages
            .windows(2)
            .any(|messages| messages[0].system_id >= messages[1].system_id)
        {
            return Err("system messages must be strictly ordered by id".into());
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

/// Validates a voice roster wherever it appears: the snapshot's own field and
/// the delta that resets it must agree on cap, contents, and ordering, so they
/// share one check rather than two that can drift.
pub fn check_voice_members(members: &[VoiceMember]) -> Result<(), String> {
    if members.len() > super::MAX_VOICE_MEMBERS {
        return Err("voice member collection exceeds limit".into());
    }
    for member in members {
        member.validate()?;
    }
    if members
        .windows(2)
        .any(|members| members[0].user_id >= members[1].user_id)
    {
        return Err("voice members must be strictly ordered by user id".into());
    }
    if members.iter().filter(|member| member.is_local).count() > 1 {
        return Err("voice roster names more than one local member".into());
    }
    Ok(())
}

/// The same cap and ordering for the volatile half. A renderer applies these by
/// user id, so a duplicate would make the frame's outcome depend on which of
/// two rows it happened to see last.
pub fn check_voice_member_updates(updates: &[VoiceMemberUpdate]) -> Result<(), String> {
    if updates.is_empty() {
        return Err("voice member updates must not be empty".into());
    }
    if updates.len() > super::MAX_VOICE_MEMBERS {
        return Err("voice member collection exceeds limit".into());
    }
    if updates
        .windows(2)
        .any(|updates| updates[0].user_id >= updates[1].user_id)
    {
        return Err("voice members must be strictly ordered by user id".into());
    }
    Ok(())
}
