use jsony::Jsony;
use kvlog::{Encode, ValueEncoder};

use crate::ids::{FileTransferId, MessageId, RoomId, StreamId};

use super::{
    appearance::{AppearanceCommand, AppearanceEvent},
    bulk::{BeginAttachmentRead, BeginUpload, BulkChunk, BulkFinished},
    identity::{IdentityCommand, IdentityEvent, IdentityResult},
    model::{
        AttachmentId, BulkTransferId, CommandCandidate, CommandCandidateKind, CommandInfo,
        CommandOutputLine, ConnectionState, DaemonInstanceId, LiveShare, LiveShareViewStatus,
        Message, Participant, RequestId, RoomSnapshot, RoomSummary, ServerSelectionState,
        StateSnapshot, SystemMessage, TransferSummary, TrustState, VoiceMemberUpdate, VoiceRoster,
        VoiceSessionState, VoiceState, check_voice_member_updates,
    },
    settings::{SettingsCommand, SettingsEvent, SettingsResult},
};

pub(crate) const WIRE_JSON: u8 = 0;
pub(crate) const WIRE_BULK_CHUNK: u8 = 1;
const WIRE_BULK_HEADER_LEN: usize = 1 + std::mem::size_of::<u64>();

pub(crate) enum DecodedWire<'a, T> {
    Frame(T),
    BulkChunk {
        transfer_id: BulkTransferId,
        bytes: &'a [u8],
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ClientHello {
    pub min_version: u16,
    pub max_version: u16,
    pub build: String,
}

impl ClientHello {
    pub fn current(build: impl Into<String>) -> Self {
        Self {
            min_version: super::PROTOCOL_MIN_VERSION,
            max_version: super::PROTOCOL_MAX_VERSION,
            build: build.into(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.min_version > self.max_version {
            return Err("invalid daemon protocol version range".into());
        }
        super::model::check_nonempty_string(&self.build)?;
        Ok(())
    }

    pub fn negotiated_version(&self) -> Option<u16> {
        let low = self.min_version.max(super::PROTOCOL_MIN_VERSION);
        let high = self.max_version.min(super::PROTOCOL_MAX_VERSION);
        (low <= high).then_some(high)
    }

    pub fn unsupported_version_message(&self) -> String {
        format!(
            "unsupported daemon RPC protocol version: client supports {}..={}, daemon supports {}..={}",
            self.min_version,
            self.max_version,
            super::PROTOCOL_MIN_VERSION,
            super::PROTOCOL_MAX_VERSION
        )
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct NegotiatedLimits {
    pub frame_bytes: u32,
    pub chunk_bytes: u32,
    pub message_bytes: u32,
    pub upload_bytes: u64,
    pub concurrent_transfers: u16,
    pub concurrent_attachment_streams: u16,
    pub attachment_read_bytes: u32,
    pub outstanding_requests: u16,
}

impl Default for NegotiatedLimits {
    fn default() -> Self {
        Self {
            frame_bytes: super::MAX_FRAME_BYTES as u32,
            chunk_bytes: super::MAX_CHUNK_BYTES as u32,
            message_bytes: super::MAX_MESSAGE_BODY_BYTES as u32,
            upload_bytes: super::DEFAULT_UPLOAD_LIMIT_BYTES,
            concurrent_transfers: super::MAX_CONCURRENT_TRANSFERS as u16,
            concurrent_attachment_streams: super::MAX_CONCURRENT_ATTACHMENT_STREAMS as u16,
            attachment_read_bytes: super::MAX_ATTACHMENT_READ_BYTES as u32,
            outstanding_requests: super::MAX_OUTSTANDING_REQUESTS as u16,
        }
    }
}

impl NegotiatedLimits {
    pub fn validate(&self) -> Result<(), String> {
        if self.frame_bytes == 0 || self.frame_bytes as usize > super::MAX_FRAME_BYTES {
            return Err("negotiated frame limit is invalid".into());
        }
        if self.chunk_bytes == 0 || self.chunk_bytes as usize > super::MAX_CHUNK_BYTES {
            return Err("negotiated chunk limit is invalid".into());
        }
        if self.message_bytes == 0 || self.message_bytes as usize > super::MAX_MESSAGE_BODY_BYTES {
            return Err("negotiated message limit is invalid".into());
        }
        if self.upload_bytes == 0 {
            return Err("negotiated upload limit is invalid".into());
        }
        if self.concurrent_transfers == 0
            || self.concurrent_transfers as usize > super::MAX_CONCURRENT_TRANSFERS
        {
            return Err("negotiated transfer limit is invalid".into());
        }
        if self.concurrent_attachment_streams == 0
            || self.concurrent_attachment_streams as usize
                > super::MAX_CONCURRENT_ATTACHMENT_STREAMS
        {
            return Err("negotiated attachment stream limit is invalid".into());
        }
        if self.attachment_read_bytes == 0
            || self.attachment_read_bytes as usize > super::MAX_ATTACHMENT_READ_BYTES
        {
            return Err("negotiated attachment read limit is invalid".into());
        }
        if self.outstanding_requests == 0
            || self.outstanding_requests as usize > super::MAX_OUTSTANDING_REQUESTS
        {
            return Err("negotiated request limit is invalid".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct Welcome {
    pub version: u16,
    pub instance_id: DaemonInstanceId,
    pub daemon_build: String,
    pub connection: ConnectionState,
    pub active_server: Option<String>,
    pub first_event_seq: u64,
    pub limits: NegotiatedLimits,
    pub commands: Vec<CommandInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum Operation {
    SelectServer,
    ResolveServerPrompt,
    SelectRoom,
    LoadOlder,
    SendMessage,
    EditMessage,
    DeleteMessage,
    BeginUpload,
    FinishUpload,
    CancelUpload,
    BeginAttachmentRead,
    OpenAttachmentSource,
    CancelBulkTransfer,
    CancelFileTransfer,
    SetVoiceState,
    JoinVoice,
    LeaveVoice,
    SetOutputVolume,
    StartLiveShare,
    StopLiveShare,
    RunCommand,
    Ping,
    RequestSnapshot,
    Disconnect,
    OpenSettings,
    SetAudioPreviewActive,
    PreviewAudioSettings,
    RefreshSettingsChoices,
    ReloadSettings,
    SaveSettings,
    CloseSettings,
    PreviewAppearance,
    CommitAppearance,
    EndAppearancePreview,
    OpenIdentity,
    CheckIdentityText,
    VerifyIdentity,
    ForgetIdentity,
    CloseIdentity,
}

impl Encode for Operation {
    fn encode_log_value_into(&self, output: ValueEncoder<'_>) {
        let value = match self {
            Self::SelectServer => "select-server",
            Self::ResolveServerPrompt => "resolve-server-prompt",
            Self::SelectRoom => "select-room",
            Self::LoadOlder => "load-older",
            Self::SendMessage => "send-message",
            Self::EditMessage => "edit-message",
            Self::DeleteMessage => "delete-message",
            Self::BeginUpload => "begin-upload",
            Self::FinishUpload => "finish-upload",
            Self::CancelUpload => "cancel-upload",
            Self::BeginAttachmentRead => "begin-attachment-read",
            Self::OpenAttachmentSource => "open-attachment-source",
            Self::CancelBulkTransfer => "cancel-bulk-transfer",
            Self::CancelFileTransfer => "cancel-file-transfer",
            Self::SetVoiceState => "set-voice-state",
            Self::JoinVoice => "join-voice",
            Self::LeaveVoice => "leave-voice",
            Self::SetOutputVolume => "set-output-volume",
            Self::StartLiveShare => "start-live-share",
            Self::StopLiveShare => "stop-live-share",
            Self::RunCommand => "run-command",
            Self::Ping => "ping",
            Self::RequestSnapshot => "request-snapshot",
            Self::Disconnect => "disconnect",
            Self::OpenSettings => "open-settings",
            Self::SetAudioPreviewActive => "set-audio-preview-active",
            Self::PreviewAudioSettings => "preview-audio-settings",
            Self::RefreshSettingsChoices => "refresh-settings-choices",
            Self::ReloadSettings => "reload-settings",
            Self::SaveSettings => "save-settings",
            Self::CloseSettings => "close-settings",
            Self::PreviewAppearance => "preview-appearance",
            Self::CommitAppearance => "commit-appearance",
            Self::EndAppearancePreview => "end-appearance-preview",
            Self::OpenIdentity => "open-identity",
            Self::CheckIdentityText => "check-identity-text",
            Self::VerifyIdentity => "verify-identity",
            Self::ForgetIdentity => "forget-identity",
            Self::CloseIdentity => "close-identity",
        };
        value.encode_log_value_into(output);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum RequestOutcome {
    Accepted,
    Rejected { code: u16, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RequestResult {
    pub request_id: RequestId,
    pub operation: Operation,
    pub outcome: RequestOutcome,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum StateDelta {
    ConnectionChanged {
        connection: ConnectionState,
        active_server: Option<String>,
    },
    LocalIdentityChanged {
        local_identity: Option<String>,
    },
    ServerSelectionChanged {
        selection: ServerSelectionState,
    },
    RoomCatalogReset {
        rooms: Vec<RoomSummary>,
    },
    RoomUpserted {
        room: RoomSummary,
    },
    RoomRemoved {
        room_id: RoomId,
    },
    RoomUnreadChanged {
        room_id: RoomId,
        unread: u32,
        behind_head: bool,
    },
    ActiveRoomChanged {
        room_id: Option<RoomId>,
    },
    RoomSnapshot(RoomSnapshot),
    MessagesPrepended {
        room_id: RoomId,
        room_generation: u64,
        messages: Vec<Message>,
        older_cursor: Option<MessageId>,
        at_start: bool,
    },
    HistoryStateChanged {
        room_id: RoomId,
        room_generation: u64,
        older_cursor: Option<MessageId>,
        at_start: bool,
    },
    MessageUpserted {
        message: Message,
    },
    MessageDeleted {
        room_id: RoomId,
        message_id: MessageId,
    },
    SystemMessageUpserted {
        message: SystemMessage,
    },
    SystemMessageDeleted {
        room_id: RoomId,
        system_id: u64,
    },
    ParticipantsChanged {
        room_id: RoomId,
        participants: Vec<Participant>,
    },
    /// The call's membership, set or cleared wholesale. Separate from
    /// [`Self::ParticipantsChanged`] so a talk spurt never re-sends the room's
    /// participant list; the vector is bounded by call size, not room size.
    ///
    /// Low frequency by construction: only joining, leaving, and renaming reach
    /// it. Everything that moves during a call arrives as
    /// [`Self::VoiceMembersUpdated`].
    VoiceRosterReset {
        roster: Option<VoiceRoster>,
    },
    /// The volatile half of the rows that moved, and nothing else.
    ///
    /// Carries no room id: [`Self::VoiceRosterReset`] establishes which call
    /// this is and [`VoiceSessionState::joined_room`] is the authority on it, so
    /// repeating it on the frame a talk spurt emits would be paying per spurt
    /// for a value the consumer already holds.
    VoiceMembersUpdated {
        updates: Vec<VoiceMemberUpdate>,
    },
    SecurityChanged {
        room_id: RoomId,
        trust: TrustState,
    },
    TransferChanged {
        transfer: TransferSummary,
    },
    TransferRemoved {
        transfer_id: FileTransferId,
    },
    VoiceSessionChanged {
        voice: VoiceSessionState,
    },
    LiveShareUpserted {
        share: LiveShare,
    },
    LiveShareRemoved {
        stream_id: StreamId,
    },
    ResyncRequired {
        reason: String,
    },
    DaemonStopping,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct StateEvent {
    pub instance_id: DaemonInstanceId,
    pub event_seq: u64,
    pub delta: StateDelta,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum ClientFrame {
    SelectServer {
        request_id: RequestId,
        label: String,
    },
    ResolveServerPrompt {
        request_id: RequestId,
        attempt_id: u64,
        accept: bool,
    },
    SelectRoom {
        request_id: RequestId,
        room_id: RoomId,
    },
    LoadOlder {
        request_id: RequestId,
        room_id: RoomId,
        room_generation: u64,
        before: Option<MessageId>,
        limit: u16,
    },
    ResolveMessageReference {
        request_id: RequestId,
        room_id: RoomId,
        room_generation: u64,
        message_id: MessageId,
    },
    SendMessage {
        request_id: RequestId,
        room_id: RoomId,
        body: String,
    },
    EditMessage {
        request_id: RequestId,
        room_id: RoomId,
        target: MessageId,
        body: String,
    },
    DeleteMessage {
        request_id: RequestId,
        room_id: RoomId,
        target: MessageId,
    },
    BeginUpload {
        request_id: RequestId,
        upload: BeginUpload,
    },
    UploadChunk(BulkChunk),
    FinishUpload {
        request_id: RequestId,
        finished: BulkFinished,
    },
    CancelUpload {
        request_id: RequestId,
        transfer_id: BulkTransferId,
    },
    BeginAttachmentRead {
        request_id: RequestId,
        read: BeginAttachmentRead,
    },
    OpenAttachmentSource {
        request_id: RequestId,
        room_id: RoomId,
        attachment_id: AttachmentId,
    },
    CancelBulkTransfer {
        request_id: RequestId,
        transfer_id: BulkTransferId,
    },
    CancelFileTransfer {
        request_id: RequestId,
        transfer_id: FileTransferId,
    },
    SetVoiceState {
        request_id: RequestId,
        state: VoiceState,
    },
    JoinVoice {
        request_id: RequestId,
        room_id: RoomId,
    },
    LeaveVoice {
        request_id: RequestId,
    },
    SetOutputVolume {
        request_id: RequestId,
        volume: f32,
    },
    StartLiveShare {
        request_id: RequestId,
        stream_id: StreamId,
        generation: u64,
    },
    StopLiveShare {
        request_id: RequestId,
        stream_id: StreamId,
        generation: u64,
    },
    RunCommand {
        request_id: RequestId,
        body: String,
    },
    RequestCommandCandidates {
        request_id: RequestId,
        kind: CommandCandidateKind,
    },
    Ping {
        request_id: RequestId,
        nonce: u64,
    },
    RequestSnapshot {
        request_id: RequestId,
    },
    Disconnect {
        request_id: RequestId,
    },
    Settings {
        request_id: RequestId,
        command: SettingsCommand,
    },
    Appearance {
        request_id: RequestId,
        command: AppearanceCommand,
    },
    Identity {
        request_id: RequestId,
        command: IdentityCommand,
    },
}

impl ClientFrame {
    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::SelectServer { request_id, .. }
            | Self::ResolveServerPrompt { request_id, .. }
            | Self::SelectRoom { request_id, .. }
            | Self::LoadOlder { request_id, .. }
            | Self::ResolveMessageReference { request_id, .. }
            | Self::SendMessage { request_id, .. }
            | Self::EditMessage { request_id, .. }
            | Self::DeleteMessage { request_id, .. }
            | Self::BeginUpload { request_id, .. }
            | Self::FinishUpload { request_id, .. }
            | Self::CancelUpload { request_id, .. }
            | Self::BeginAttachmentRead { request_id, .. }
            | Self::OpenAttachmentSource { request_id, .. }
            | Self::CancelBulkTransfer { request_id, .. }
            | Self::CancelFileTransfer { request_id, .. }
            | Self::SetVoiceState { request_id, .. }
            | Self::JoinVoice { request_id, .. }
            | Self::LeaveVoice { request_id }
            | Self::SetOutputVolume { request_id, .. }
            | Self::StartLiveShare { request_id, .. }
            | Self::StopLiveShare { request_id, .. }
            | Self::RunCommand { request_id, .. }
            | Self::RequestCommandCandidates { request_id, .. }
            | Self::Ping { request_id, .. }
            | Self::RequestSnapshot { request_id }
            | Self::Disconnect { request_id }
            | Self::Settings { request_id, .. }
            | Self::Appearance { request_id, .. }
            | Self::Identity { request_id, .. } => Some(*request_id),
            Self::UploadChunk(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum DaemonFrame {
    Welcome(Welcome),
    Snapshot {
        instance_id: DaemonInstanceId,
        event_seq: u64,
        snapshot: StateSnapshot,
    },
    Event(StateEvent),
    RequestResult(RequestResult),
    CommandResult {
        result: RequestResult,
        lines: Vec<CommandOutputLine>,
    },
    CommandCandidates {
        request_id: RequestId,
        kind: CommandCandidateKind,
        items: Vec<CommandCandidate>,
    },
    MessageReferenceResolved {
        request_id: RequestId,
        room_id: RoomId,
        room_generation: u64,
        message_id: MessageId,
        message: Option<Message>,
    },
    LiveShareOpened {
        request_id: RequestId,
        stream_id: StreamId,
        generation: u64,
        status: LiveShareViewStatus,
    },
    LiveShareStatus {
        stream_id: StreamId,
        generation: u64,
        status: LiveShareViewStatus,
    },
    AttachmentSourceOpened {
        request_id: RequestId,
        room_id: RoomId,
        attachment_id: AttachmentId,
        byte_len: u64,
        transport: AttachmentSourceTransport,
    },
    Pong {
        request_id: RequestId,
        nonce: u64,
    },
    BulkChunk(BulkChunk),
    BulkFinished(BulkFinished),
    BulkCanceled {
        transfer_id: BulkTransferId,
        reason: String,
    },
    SettingsResult(SettingsResult),
    SettingsEvent(SettingsEvent),
    Appearance(AppearanceEvent),
    IdentityResult(IdentityResult),
    IdentityEvent(IdentityEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum AttachmentSourceTransport {
    DirectFile,
    ReadAtSocket,
}

pub fn encode_client(frame: &ClientFrame) -> Result<Vec<u8>, String> {
    validate_client(frame)?;
    let mut output = Vec::new();
    if let ClientFrame::UploadChunk(chunk) = frame {
        encode_bulk_wire_into(chunk, &mut output)?;
    } else {
        bounded_encode_wire_into(frame, &mut output)?;
    }
    Ok(output)
}

/// Serializes a complete length-prefixed client frame into reusable storage.
///
/// The prefix is reserved before `jsony` writes the payload, so framing does
/// not require a second allocation or a payload copy.
pub fn encode_client_framed_into(frame: &ClientFrame, output: &mut Vec<u8>) -> Result<(), String> {
    output.clear();
    validate_client(frame)?;
    if let ClientFrame::UploadChunk(chunk) = frame {
        return encode_bulk_framed_into(chunk, output);
    }
    bounded_encode_framed_into(frame, output)
}

pub fn decode_client(bytes: &[u8]) -> Result<ClientFrame, String> {
    match decode_client_wire(bytes)? {
        DecodedWire::Frame(frame) => Ok(frame),
        DecodedWire::BulkChunk { transfer_id, bytes } => Ok(ClientFrame::UploadChunk(BulkChunk {
            transfer_id,
            bytes: bytes.to_vec(),
        })),
    }
}

pub fn encode_daemon(frame: &DaemonFrame) -> Result<Vec<u8>, String> {
    validate_daemon(frame)?;
    let mut output = Vec::new();
    if let DaemonFrame::BulkChunk(chunk) = frame {
        encode_bulk_wire_into(chunk, &mut output)?;
    } else {
        bounded_encode_wire_into(frame, &mut output)?;
    }
    Ok(output)
}

/// Serializes a complete length-prefixed daemon frame into reusable storage.
pub fn encode_daemon_framed_into(frame: &DaemonFrame, output: &mut Vec<u8>) -> Result<(), String> {
    output.clear();
    validate_daemon(frame)?;
    if let DaemonFrame::BulkChunk(chunk) = frame {
        return encode_bulk_framed_into(chunk, output);
    }
    bounded_encode_framed_into(frame, output)
}

pub fn decode_daemon(bytes: &[u8]) -> Result<DaemonFrame, String> {
    match decode_daemon_wire(bytes)? {
        DecodedWire::Frame(frame) => Ok(frame),
        DecodedWire::BulkChunk { transfer_id, bytes } => Ok(DaemonFrame::BulkChunk(BulkChunk {
            transfer_id,
            bytes: bytes.to_vec(),
        })),
    }
}

fn bounded_encode_wire_into<T: jsony::ToBinary>(
    value: &T,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    output.push(WIRE_JSON);
    jsony::to_binary_into(value, &mut *output);
    if output.len() > super::MAX_FRAME_BYTES {
        output.clear();
        return Err("daemon frame exceeds maximum length".into());
    }
    Ok(())
}

fn bounded_encode_framed_into<T: jsony::ToBinary>(
    value: &T,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    output.extend_from_slice(&[0; crate::framing::LENGTH_PREFIX_LEN]);
    output.push(WIRE_JSON);
    let encoded_len = jsony::to_binary_into(value, &mut *output).len();
    let payload_len = encoded_len + 1;
    if payload_len > super::MAX_FRAME_BYTES {
        output.clear();
        return Err("daemon frame exceeds maximum length".into());
    }
    let payload_len =
        u32::try_from(payload_len).map_err(|_| "daemon frame length does not fit in u32")?;
    output[..crate::framing::LENGTH_PREFIX_LEN].copy_from_slice(&payload_len.to_le_bytes());
    Ok(())
}

fn encode_bulk_framed_into(chunk: &BulkChunk, output: &mut Vec<u8>) -> Result<(), String> {
    let payload_len = WIRE_BULK_HEADER_LEN
        .checked_add(chunk.bytes.len())
        .ok_or_else(|| "daemon frame length overflow".to_string())?;
    if payload_len > super::MAX_FRAME_BYTES {
        return Err("daemon frame exceeds maximum length".into());
    }
    let payload_len =
        u32::try_from(payload_len).map_err(|_| "daemon frame length does not fit in u32")?;
    output.extend_from_slice(&payload_len.to_le_bytes());
    output.push(WIRE_BULK_CHUNK);
    output.extend_from_slice(&chunk.transfer_id.0.to_le_bytes());
    output.extend_from_slice(&chunk.bytes);
    Ok(())
}

fn encode_bulk_wire_into(chunk: &BulkChunk, output: &mut Vec<u8>) -> Result<(), String> {
    let payload_len = WIRE_BULK_HEADER_LEN
        .checked_add(chunk.bytes.len())
        .ok_or_else(|| "daemon frame length overflow".to_string())?;
    if payload_len > super::MAX_FRAME_BYTES {
        return Err("daemon frame exceeds maximum length".into());
    }
    output.push(WIRE_BULK_CHUNK);
    output.extend_from_slice(&chunk.transfer_id.0.to_le_bytes());
    output.extend_from_slice(&chunk.bytes);
    Ok(())
}

pub(crate) fn bulk_framed_header(
    transfer_id: BulkTransferId,
    bytes_len: usize,
) -> Result<[u8; crate::framing::LENGTH_PREFIX_LEN + WIRE_BULK_HEADER_LEN], String> {
    if transfer_id.0 == 0 {
        return Err("transfer id must be nonzero".into());
    }
    if bytes_len == 0 {
        return Err("bulk chunk must not be empty".into());
    }
    if bytes_len > super::MAX_CHUNK_BYTES {
        return Err("bulk chunk exceeds limit".into());
    }
    let payload_len = WIRE_BULK_HEADER_LEN
        .checked_add(bytes_len)
        .ok_or_else(|| "daemon frame length overflow".to_string())?;
    if payload_len > super::MAX_FRAME_BYTES {
        return Err("daemon frame exceeds maximum length".into());
    }
    let payload_len =
        u32::try_from(payload_len).map_err(|_| "daemon frame length does not fit in u32")?;
    let mut header = [0; crate::framing::LENGTH_PREFIX_LEN + WIRE_BULK_HEADER_LEN];
    header[..crate::framing::LENGTH_PREFIX_LEN].copy_from_slice(&payload_len.to_le_bytes());
    header[crate::framing::LENGTH_PREFIX_LEN] = WIRE_BULK_CHUNK;
    header[crate::framing::LENGTH_PREFIX_LEN + 1..].copy_from_slice(&transfer_id.0.to_le_bytes());
    Ok(header)
}

pub(crate) fn decode_client_wire(bytes: &[u8]) -> Result<DecodedWire<'_, ClientFrame>, String> {
    decode_wire(bytes, decode_client_body)
}

pub(crate) fn decode_daemon_wire(bytes: &[u8]) -> Result<DecodedWire<'_, DaemonFrame>, String> {
    decode_wire(bytes, decode_daemon_body)
}

fn decode_client_body(bytes: &[u8]) -> Result<ClientFrame, String> {
    let frame = bounded_decode(bytes)?;
    validate_client(&frame)?;
    Ok(frame)
}

fn decode_daemon_body(bytes: &[u8]) -> Result<DaemonFrame, String> {
    let frame = bounded_decode(bytes)?;
    validate_daemon(&frame)?;
    Ok(frame)
}

fn decode_wire<'a, T>(
    bytes: &'a [u8],
    decode: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Result<DecodedWire<'a, T>, String> {
    let Some((&kind, body)) = bytes.split_first() else {
        return Err("daemon wire frame is empty".into());
    };
    match kind {
        WIRE_JSON => decode(body).map(DecodedWire::Frame),
        WIRE_BULK_CHUNK => {
            let Some((transfer_id, bytes)) = body.split_at_checked(std::mem::size_of::<u64>())
            else {
                return Err("bulk chunk wire header is incomplete".into());
            };
            let transfer_id = BulkTransferId(u64::from_le_bytes(
                transfer_id
                    .try_into()
                    .expect("checked bulk transfer id length"),
            ));
            if transfer_id.0 == 0 {
                return Err("transfer id must be nonzero".into());
            }
            if bytes.is_empty() {
                return Err("bulk chunk must not be empty".into());
            }
            if bytes.len() > super::MAX_CHUNK_BYTES {
                return Err("bulk chunk exceeds limit".into());
            }
            Ok(DecodedWire::BulkChunk { transfer_id, bytes })
        }
        _ => Err("unknown daemon wire frame kind".into()),
    }
}

fn bounded_decode<T: for<'a> jsony::FromBinary<'a>>(bytes: &[u8]) -> Result<T, String> {
    if bytes.len() > super::MAX_FRAME_BYTES {
        return Err("daemon frame exceeds maximum length".into());
    }
    jsony::from_binary(bytes).map_err(|error| error.to_string())
}

fn validate_client(frame: &ClientFrame) -> Result<(), String> {
    let request_id = frame.request_id();
    if let ClientFrame::UploadChunk(chunk) = frame {
        chunk.validate()?;
    }
    if request_id.is_some_and(|id| id.0 == 0) {
        return Err("request id must be nonzero".into());
    }
    match frame {
        ClientFrame::SelectServer { label, .. } => {
            super::model::check_nonempty_string(label)?;
        }
        ClientFrame::ResolveServerPrompt { attempt_id, .. } if *attempt_id == 0 => {
            return Err("server selection prompt attempt id must be nonzero".into());
        }
        ClientFrame::SendMessage { body, .. } | ClientFrame::EditMessage { body, .. } => {
            if body.len() > super::MAX_MESSAGE_BODY_BYTES {
                return Err("message body exceeds limit".into());
            }
        }
        ClientFrame::RunCommand { body, .. } => {
            if body.len() > super::MAX_MESSAGE_BODY_BYTES {
                return Err("command body exceeds limit".into());
            }
            if !body.starts_with('/') {
                return Err("command body must start with a slash".into());
            }
            if body.contains(['\r', '\n']) {
                return Err("command body must be a single line".into());
            }
        }
        ClientFrame::SetOutputVolume { volume, .. } if !volume.is_finite() => {
            return Err("output volume must be finite".into());
        }
        ClientFrame::SetOutputVolume { volume, .. }
            if !(0.0..=super::MAX_OUTPUT_VOLUME_PERCENT).contains(volume) =>
        {
            return Err("output volume is outside the supported range".into());
        }
        ClientFrame::LoadOlder { limit, .. }
            if *limit == 0 || *limit > super::MAX_HISTORY_REQUEST_MESSAGES =>
        {
            return Err("history request limit is invalid".into());
        }
        ClientFrame::ResolveMessageReference { message_id, .. } if message_id.0 == 0 => {
            return Err("message reference target must be nonzero".into());
        }
        ClientFrame::BeginUpload { upload, .. } => {
            upload.validate()?;
        }
        ClientFrame::FinishUpload { finished, .. } => {
            finished.validate()?;
        }
        ClientFrame::CancelUpload { transfer_id, .. }
        | ClientFrame::CancelBulkTransfer { transfer_id, .. }
            if transfer_id.0 == 0 =>
        {
            return Err("transfer id must be nonzero".into());
        }
        ClientFrame::CancelFileTransfer { transfer_id, .. } if transfer_id.0 == 0 => {
            return Err("file transfer id must be nonzero".into());
        }
        ClientFrame::BeginAttachmentRead { read, .. } => {
            read.validate()?;
        }
        ClientFrame::OpenAttachmentSource { attachment_id, .. }
            if attachment_id.transfer_id.0 == 0 =>
        {
            return Err("attachment transfer id must be nonzero".into());
        }
        ClientFrame::StartLiveShare {
            stream_id,
            generation,
            ..
        }
        | ClientFrame::StopLiveShare {
            stream_id,
            generation,
            ..
        } if stream_id.0 == 0 || *generation == 0 => {
            return Err("live share identity must be nonzero".into());
        }
        ClientFrame::Settings { command, .. } => command.validate()?,
        ClientFrame::Appearance { command, .. } => command.validate()?,
        ClientFrame::Identity { command, .. } => command.validate()?,
        _ => {}
    }
    Ok(())
}

fn validate_daemon(frame: &DaemonFrame) -> Result<(), String> {
    match frame {
        DaemonFrame::Snapshot {
            instance_id,
            event_seq,
            snapshot,
        } => {
            validate_instance_and_sequence(*instance_id, *event_seq)?;
            snapshot.validate()
        }
        DaemonFrame::BulkChunk(chunk) => chunk.validate(),
        DaemonFrame::Welcome(welcome) => {
            if !(super::PROTOCOL_MIN_VERSION..=super::PROTOCOL_MAX_VERSION)
                .contains(&welcome.version)
            {
                return Err("daemon selected an unsupported protocol version".into());
            }
            if welcome.instance_id.0 == [0; 16] {
                return Err("daemon welcome contains an invalid identity".into());
            }
            if welcome.first_event_seq == 0 {
                return Err("daemon event sequence must be nonzero".into());
            }
            super::model::check_nonempty_string(&welcome.daemon_build)?;
            super::model::check_opt_string(&welcome.active_server)?;
            welcome.limits.validate()?;
            if welcome.commands.len() > super::MAX_COMMANDS {
                return Err("command catalog exceeds limit".into());
            }
            for command in &welcome.commands {
                command.validate()?;
            }
            if welcome
                .commands
                .windows(2)
                .any(|commands| commands[0].name >= commands[1].name)
            {
                return Err("command catalog must be strictly ordered by name".into());
            }
            Ok(())
        }
        DaemonFrame::Event(event) => {
            validate_instance_and_sequence(event.instance_id, event.event_seq)?;
            validate_delta(&event.delta)
        }
        DaemonFrame::RequestResult(result) => validate_result(result),
        DaemonFrame::CommandResult { result, lines } => {
            validate_result(result)?;
            if result.operation != Operation::RunCommand {
                return Err("command result carries the wrong operation".into());
            }
            if lines.len() > super::MAX_COMMAND_OUTPUT_LINES {
                return Err("command output exceeds limit".into());
            }
            for line in lines {
                line.validate()?;
            }
            Ok(())
        }
        DaemonFrame::CommandCandidates {
            request_id, items, ..
        } => {
            if request_id.0 == 0 {
                return Err("request id must be nonzero".into());
            }
            if items.len() > super::MAX_COMMAND_CANDIDATES {
                return Err("command candidate collection exceeds limit".into());
            }
            for item in items {
                item.validate()?;
            }
            Ok(())
        }
        DaemonFrame::MessageReferenceResolved {
            request_id,
            room_id,
            room_generation: _,
            message_id,
            message,
        } => {
            if request_id.0 == 0 {
                return Err("request id must be nonzero".into());
            }
            if message_id.0 == 0 {
                return Err("message reference target must be nonzero".into());
            }
            if let Some(message) = message {
                message.validate()?;
                if message.room_id != *room_id || message.message_id != *message_id {
                    return Err("resolved message reference identity does not match target".into());
                }
            }
            Ok(())
        }
        DaemonFrame::LiveShareOpened {
            request_id,
            stream_id,
            generation,
            ..
        } if request_id.0 == 0 || stream_id.0 == 0 || *generation == 0 => {
            Err("live share request or identity is invalid".into())
        }
        DaemonFrame::LiveShareStatus {
            stream_id,
            generation,
            ..
        } if stream_id.0 == 0 || *generation == 0 => {
            Err("live share identity must be nonzero".into())
        }
        DaemonFrame::AttachmentSourceOpened {
            request_id,
            attachment_id,
            byte_len,
            ..
        } if request_id.0 == 0 || attachment_id.transfer_id.0 == 0 || *byte_len == 0 => {
            Err("attachment source identity or length is invalid".into())
        }
        DaemonFrame::Pong { request_id, .. } if request_id.0 == 0 => {
            Err("request id must be nonzero".into())
        }
        DaemonFrame::BulkFinished(finished) => finished.validate(),
        DaemonFrame::BulkCanceled {
            transfer_id,
            reason,
        } => {
            if transfer_id.0 == 0 {
                return Err("transfer id must be nonzero".into());
            }
            super::model::check_nonempty_string(reason)
        }
        DaemonFrame::SettingsResult(result) => result.validate(),
        DaemonFrame::SettingsEvent(event) => event.validate(),
        DaemonFrame::Appearance(event) => event.validate(),
        DaemonFrame::IdentityResult(result) => result.validate(),
        DaemonFrame::IdentityEvent(event) => event.validate(),
        DaemonFrame::Pong { .. }
        | DaemonFrame::LiveShareOpened { .. }
        | DaemonFrame::LiveShareStatus { .. }
        | DaemonFrame::AttachmentSourceOpened { .. } => Ok(()),
    }
}

fn validate_delta(delta: &StateDelta) -> Result<(), String> {
    match delta {
        StateDelta::ConnectionChanged { active_server, .. } => {
            super::model::check_opt_string(active_server)
        }
        StateDelta::LocalIdentityChanged { local_identity } => {
            super::model::check_opt_string(local_identity)
        }
        StateDelta::ServerSelectionChanged { selection } => selection.validate(),
        StateDelta::RoomCatalogReset { rooms } => {
            if rooms.len() > super::MAX_ROOMS {
                return Err("room collection exceeds limit".into());
            }
            for room in rooms {
                room.validate()?;
            }
            if rooms.windows(2).any(|rooms| rooms[0].id >= rooms[1].id) {
                return Err("room catalog must be strictly ordered by id".into());
            }
            Ok(())
        }
        StateDelta::RoomUpserted { room } => room.validate(),
        StateDelta::RoomSnapshot(room) => room.validate(),
        StateDelta::MessagesPrepended {
            room_id, messages, ..
        } => {
            if messages.len() > super::MAX_MESSAGES {
                return Err("message collection exceeds limit".into());
            }
            for message in messages {
                if message.room_id != *room_id {
                    return Err("message belongs to a different room".into());
                }
                message.validate()?;
            }
            if messages
                .windows(2)
                .any(|messages| messages[0].message_id >= messages[1].message_id)
            {
                return Err("messages must be strictly ordered by id".into());
            }
            Ok(())
        }
        StateDelta::MessageUpserted { message } => message.validate(),
        StateDelta::SystemMessageUpserted { message } => message.validate(),
        StateDelta::SystemMessageDeleted { system_id: 0, .. } => {
            Err("system message id must be nonzero".into())
        }
        StateDelta::ParticipantsChanged { participants, .. } => {
            if participants.len() > super::MAX_PARTICIPANTS {
                return Err("participant collection exceeds limit".into());
            }
            for participant in participants {
                participant.validate()?;
            }
            Ok(())
        }
        StateDelta::VoiceRosterReset { roster } => {
            roster.as_ref().map_or(Ok(()), VoiceRoster::validate)
        }
        StateDelta::VoiceMembersUpdated { updates } => check_voice_member_updates(updates),
        StateDelta::TransferChanged { transfer } => transfer.validate(),
        StateDelta::TransferRemoved { transfer_id } if transfer_id.0 == 0 => {
            Err("transfer id must be nonzero".into())
        }
        StateDelta::VoiceSessionChanged { voice } => voice.validate(),
        StateDelta::LiveShareUpserted { share } => share.validate(),
        StateDelta::ResyncRequired { reason } => super::model::check_nonempty_string(reason),
        _ => Ok(()),
    }
}

fn validate_instance_and_sequence(
    instance_id: DaemonInstanceId,
    event_seq: u64,
) -> Result<(), String> {
    if instance_id.0 == [0; 16] || event_seq == 0 {
        return Err("daemon instance or event sequence is invalid".into());
    }
    Ok(())
}

fn validate_result(result: &RequestResult) -> Result<(), String> {
    if result.request_id.0 == 0 {
        return Err("request id must be nonzero".into());
    }
    if let RequestOutcome::Rejected { message, .. } = &result.outcome {
        super::model::check_nonempty_string(message)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{VoiceMember, VoiceMemberStatus};

    #[test]
    fn directional_frames_round_trip() {
        let client = ClientFrame::SendMessage {
            request_id: RequestId(7),
            room_id: RoomId(2),
            body: "hello".into(),
        };
        assert_eq!(
            decode_client(&encode_client(&client).unwrap()).unwrap(),
            client
        );

        let daemon = DaemonFrame::Pong {
            request_id: RequestId(7),
            nonce: 9,
        };
        assert_eq!(
            decode_daemon(&encode_daemon(&daemon).unwrap()).unwrap(),
            daemon
        );
    }

    #[test]
    fn rejects_zero_request_id_and_large_chunk() {
        let frame = ClientFrame::RequestSnapshot {
            request_id: RequestId(0),
        };
        assert!(encode_client(&frame).is_err());
        let mut reusable = vec![1, 2, 3];
        assert!(encode_client_framed_into(&frame, &mut reusable).is_err());
        assert!(reusable.is_empty());
        let frame = ClientFrame::UploadChunk(BulkChunk {
            transfer_id: BulkTransferId(1),
            bytes: vec![0; super::super::MAX_CHUNK_BYTES + 1],
        });
        assert!(encode_client(&frame).is_err());
    }

    #[test]
    fn framed_encoding_reuses_one_buffer_and_decodes_in_place() {
        let first = ClientFrame::SendMessage {
            request_id: RequestId(1),
            room_id: RoomId(2),
            body: "first payload".into(),
        };
        let mut buffer = Vec::new();
        encode_client_framed_into(&first, &mut buffer).unwrap();
        let capacity = buffer.capacity();
        let (payload, consumed) =
            crate::framing::parse_frame_with_limit(&buffer, super::super::MAX_FRAME_BYTES)
                .unwrap()
                .unwrap();
        assert_eq!(consumed, buffer.len());
        let DecodedWire::Frame(decoded) = decode_client_wire(payload).unwrap() else {
            panic!("ordinary client frame decoded as bulk data")
        };
        assert_eq!(decoded, first);

        let second = ClientFrame::Ping {
            request_id: RequestId(2),
            nonce: 3,
        };
        encode_client_framed_into(&second, &mut buffer).unwrap();
        assert_eq!(buffer.capacity(), capacity);
        let (payload, _) =
            crate::framing::parse_frame_with_limit(&buffer, super::super::MAX_FRAME_BYTES)
                .unwrap()
                .unwrap();
        let DecodedWire::Frame(decoded) = decode_client_wire(payload).unwrap() else {
            panic!("ordinary client frame decoded as bulk data")
        };
        assert_eq!(decoded, second);
    }

    #[test]
    fn bulk_wire_payload_is_raw_and_borrowed() {
        let bytes = vec![7; 4096];
        let chunk = ClientFrame::UploadChunk(BulkChunk {
            transfer_id: BulkTransferId(9),
            bytes: bytes.clone(),
        });
        let mut framed = Vec::new();
        encode_client_framed_into(&chunk, &mut framed).unwrap();
        let (payload, _) =
            crate::framing::parse_frame_with_limit(&framed, super::super::MAX_FRAME_BYTES)
                .unwrap()
                .unwrap();
        let DecodedWire::BulkChunk {
            transfer_id,
            bytes: decoded,
        } = decode_client_wire(payload).unwrap()
        else {
            panic!("bulk client frame decoded as an ordinary frame")
        };
        assert_eq!(transfer_id, BulkTransferId(9));
        assert_eq!(decoded, bytes);
        assert_eq!(decoded.as_ptr(), payload[WIRE_BULK_HEADER_LEN..].as_ptr());
    }

    #[test]
    fn rejects_empty_chunks_and_out_of_range_volume() {
        assert!(
            encode_client(&ClientFrame::UploadChunk(BulkChunk {
                transfer_id: BulkTransferId(1),
                bytes: Vec::new(),
            }))
            .is_err()
        );
        assert!(
            encode_client(&ClientFrame::SetOutputVolume {
                request_id: RequestId(1),
                volume: super::super::MAX_OUTPUT_VOLUME_PERCENT + 1.0,
            })
            .is_err()
        );
    }

    #[test]
    fn every_phase_one_client_frame_round_trips() {
        let request_id = RequestId(1);
        let room_id = RoomId(2);
        let transfer_id = BulkTransferId(3);
        let frames = vec![
            ClientFrame::SelectServer {
                request_id,
                label: "work".into(),
            },
            ClientFrame::ResolveServerPrompt {
                request_id,
                attempt_id: 7,
                accept: true,
            },
            ClientFrame::SelectRoom {
                request_id,
                room_id,
            },
            ClientFrame::LoadOlder {
                request_id,
                room_id,
                room_generation: 3,
                before: Some(MessageId(4)),
                limit: 20,
            },
            ClientFrame::ResolveMessageReference {
                request_id,
                room_id,
                room_generation: 3,
                message_id: MessageId(4),
            },
            ClientFrame::SendMessage {
                request_id,
                room_id,
                body: "hello".into(),
            },
            ClientFrame::EditMessage {
                request_id,
                room_id,
                target: MessageId(4),
                body: "edit".into(),
            },
            ClientFrame::DeleteMessage {
                request_id,
                room_id,
                target: MessageId(4),
            },
            ClientFrame::BeginUpload {
                request_id,
                upload: BeginUpload {
                    transfer_id,
                    room_id,
                    file_name: "a.png".into(),
                    byte_len: 2,
                },
            },
            ClientFrame::UploadChunk(BulkChunk {
                transfer_id,
                bytes: vec![1, 2],
            }),
            ClientFrame::FinishUpload {
                request_id,
                finished: BulkFinished { transfer_id },
            },
            ClientFrame::CancelUpload {
                request_id,
                transfer_id,
            },
            ClientFrame::BeginAttachmentRead {
                request_id,
                read: BeginAttachmentRead {
                    transfer_id,
                    room_id,
                    attachment_id: super::super::model::AttachmentId {
                        timestamp_ms: 2,
                        transfer_id: FileTransferId(2),
                    },
                },
            },
            ClientFrame::OpenAttachmentSource {
                request_id,
                room_id,
                attachment_id: super::super::model::AttachmentId {
                    timestamp_ms: 2,
                    transfer_id: FileTransferId(2),
                },
            },
            ClientFrame::CancelBulkTransfer {
                request_id,
                transfer_id,
            },
            ClientFrame::CancelFileTransfer {
                request_id,
                transfer_id: FileTransferId(4),
            },
            ClientFrame::SetVoiceState {
                request_id,
                state: VoiceState::Deafened,
            },
            ClientFrame::JoinVoice {
                request_id,
                room_id,
            },
            ClientFrame::LeaveVoice { request_id },
            ClientFrame::SetOutputVolume {
                request_id,
                volume: 75.0,
            },
            ClientFrame::StartLiveShare {
                request_id,
                stream_id: StreamId(5),
                generation: 6,
            },
            ClientFrame::StopLiveShare {
                request_id,
                stream_id: StreamId(5),
                generation: 6,
            },
            ClientFrame::RunCommand {
                request_id,
                body: "/whoami".into(),
            },
            ClientFrame::RequestCommandCandidates {
                request_id,
                kind: CommandCandidateKind::Room,
            },
            ClientFrame::Ping {
                request_id,
                nonce: 9,
            },
            ClientFrame::RequestSnapshot { request_id },
            ClientFrame::Disconnect { request_id },
        ];
        for frame in frames {
            assert_eq!(
                decode_client(&encode_client(&frame).unwrap()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn every_voice_state_client_frame_round_trips() {
        for state in [VoiceState::Live, VoiceState::Muted, VoiceState::Deafened] {
            let frame = ClientFrame::SetVoiceState {
                request_id: RequestId(1),
                state,
            };
            assert_eq!(
                decode_client(&encode_client(&frame).unwrap()).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn rejects_invalid_server_selection_requests() {
        assert!(
            encode_client(&ClientFrame::SelectServer {
                request_id: RequestId(1),
                label: String::new(),
            })
            .is_err()
        );
        assert!(
            encode_client(&ClientFrame::ResolveServerPrompt {
                request_id: RequestId(1),
                attempt_id: 0,
                accept: false,
            })
            .is_err()
        );
    }

    #[test]
    fn negotiated_attachment_limits_reject_zero_and_values_above_compiled_maxima() {
        let mut limits = NegotiatedLimits::default();
        limits.concurrent_attachment_streams = 0;
        assert!(limits.validate().is_err());
        limits.concurrent_attachment_streams =
            super::super::MAX_CONCURRENT_ATTACHMENT_STREAMS as u16 + 1;
        assert!(limits.validate().is_err());

        let mut limits = NegotiatedLimits::default();
        limits.attachment_read_bytes = 0;
        assert!(limits.validate().is_err());
        limits.attachment_read_bytes = super::super::MAX_ATTACHMENT_READ_BYTES as u32 + 1;
        assert!(limits.validate().is_err());
    }

    #[test]
    fn rejects_ambiguous_or_duplicate_server_selection_state() {
        let server = super::super::model::ServerSummary {
            label: "work".into(),
            username: "alice".into(),
            tcp_addr: "127.0.0.1:4000".into(),
            require_transport_encryption: true,
            availability: super::super::model::ServerAvailability::Ready,
        };
        let selection = ServerSelectionState {
            servers: vec![server.clone(), server],
            error: Some(super::super::model::ServerSelectionError {
                label: Some("work".into()),
                message: "failed".into(),
            }),
            prompt: Some(
                super::super::model::ServerSelectionPrompt::AllowUnencryptedTransport {
                    label: "work".into(),
                    attempt_id: 1,
                },
            ),
        };
        let frame = DaemonFrame::Event(StateEvent {
            instance_id: DaemonInstanceId([1; 16]),
            event_seq: 1,
            delta: StateDelta::ServerSelectionChanged { selection },
        });

        assert!(encode_daemon(&frame).is_err());
    }

    #[test]
    fn every_phase_one_daemon_frame_round_trips() {
        let request_id = RequestId(1);
        let transfer_id = BulkTransferId(3);
        let instance_id = DaemonInstanceId([4; 16]);
        let descriptor = super::super::model::AttachmentDescriptor {
            id: super::super::model::AttachmentId {
                timestamp_ms: 2,
                transfer_id: FileTransferId(2),
            },
            file_name: "a.png".into(),
            media_kind: super::super::model::MediaKind::Image,
            content_type: "image/png".into(),
            byte_len: 2,
            width: Some(2),
            height: Some(1),
        };
        let frames = vec![
            DaemonFrame::Welcome(Welcome {
                version: super::super::PROTOCOL_MAX_VERSION,
                instance_id,
                daemon_build: "test".into(),
                connection: ConnectionState::Online,
                active_server: Some("local".into()),
                first_event_seq: 1,
                limits: NegotiatedLimits::default(),
                commands: vec![CommandInfo {
                    name: "/whoami".into(),
                    usage: "/whoami".into(),
                    description: "show the current authenticated user".into(),
                    arg: super::super::model::CommandArgKind::None,
                    placeholder: None,
                }],
            }),
            DaemonFrame::Snapshot {
                instance_id,
                event_seq: 1,
                snapshot: StateSnapshot {
                    connection: ConnectionState::Online,
                    active_server: Some("local".into()),
                    server_selection: Default::default(),
                    local_identity: Some("alice".into()),
                    rooms: Vec::new(),
                    selected_room: None,
                    room: None,
                    voice: VoiceSessionState {
                        state: VoiceState::Live,
                        output_volume: 100.0,
                        joined_room: None,
                    },
                    voice_roster: None,
                    transfers: Vec::new(),
                    live_shares: Vec::new(),
                },
            },
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 2,
                delta: StateDelta::LocalIdentityChanged {
                    local_identity: Some("bob".into()),
                },
            }),
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 7,
                delta: StateDelta::VoiceRosterReset {
                    roster: Some(VoiceRoster {
                        room_id: RoomId(2),
                        members: vec![
                            VoiceMember {
                                user_id: crate::ids::UserId(3),
                                name: "alice".into(),
                                is_local: true,
                                joined_ms: 1,
                                status: VoiceMemberStatus {
                                    voice_state: VoiceState::Live,
                                    speaking: true,
                                    p2p_direct: false,
                                    inbound_latency_ms: None,
                                    outbound_latency_ms: None,
                                },
                            },
                            VoiceMember {
                                user_id: crate::ids::UserId(4),
                                name: "bob".into(),
                                is_local: false,
                                joined_ms: 2,
                                status: VoiceMemberStatus {
                                    voice_state: VoiceState::Deafened,
                                    speaking: false,
                                    p2p_direct: true,
                                    inbound_latency_ms: Some(90),
                                    outbound_latency_ms: Some(60),
                                },
                            },
                        ],
                    }),
                },
            }),
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 8,
                delta: StateDelta::VoiceMembersUpdated {
                    updates: vec![VoiceMemberUpdate {
                        user_id: crate::ids::UserId(4),
                        status: VoiceMemberStatus {
                            voice_state: VoiceState::Muted,
                            speaking: true,
                            p2p_direct: false,
                            inbound_latency_ms: Some(85),
                            outbound_latency_ms: None,
                        },
                    }],
                },
            }),
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 5,
                delta: StateDelta::SystemMessageUpserted {
                    message: SystemMessage {
                        room_id: RoomId(2),
                        system_id: 7,
                        after_message_id: Some(MessageId(2)),
                        sender: "call".into(),
                        body: "alice joined the call".into(),
                        timestamp_ms: 5,
                        level: super::super::model::SystemMessageLevel::Info,
                    },
                },
            }),
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 6,
                delta: StateDelta::SystemMessageDeleted {
                    room_id: RoomId(2),
                    system_id: 7,
                },
            }),
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 3,
                delta: StateDelta::DaemonStopping,
            }),
            DaemonFrame::RequestResult(RequestResult {
                request_id,
                operation: Operation::Ping,
                outcome: RequestOutcome::Accepted,
            }),
            DaemonFrame::CommandResult {
                result: RequestResult {
                    request_id,
                    operation: Operation::RunCommand,
                    outcome: RequestOutcome::Accepted,
                },
                lines: vec![CommandOutputLine {
                    error: false,
                    text: "alice".into(),
                }],
            },
            DaemonFrame::CommandCandidates {
                request_id,
                kind: CommandCandidateKind::Room,
                items: vec![CommandCandidate {
                    value: "general".into(),
                    detail: None,
                }],
            },
            DaemonFrame::MessageReferenceResolved {
                request_id,
                room_id: RoomId(2),
                room_generation: 3,
                message_id: MessageId(4),
                message: None,
            },
            DaemonFrame::LiveShareOpened {
                request_id,
                stream_id: StreamId(5),
                generation: 6,
                status: LiveShareViewStatus::WaitingForKeyframe,
            },
            DaemonFrame::LiveShareStatus {
                stream_id: StreamId(5),
                generation: 6,
                status: LiveShareViewStatus::Reconnecting,
            },
            DaemonFrame::AttachmentSourceOpened {
                request_id,
                room_id: RoomId(2),
                attachment_id: descriptor.id,
                byte_len: descriptor.byte_len,
                transport: AttachmentSourceTransport::ReadAtSocket,
            },
            DaemonFrame::Pong {
                request_id,
                nonce: 9,
            },
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 4,
                delta: StateDelta::MessageUpserted {
                    message: Message {
                        room_id: RoomId(2),
                        message_id: MessageId(2),
                        sender_id: crate::ids::UserId(3),
                        sender_name: "alice".into(),
                        body: String::new(),
                        timestamp_ms: 4,
                        local: false,
                        edited: false,
                        unverified: false,
                        reference: None,
                        attachment: Some(descriptor),
                    },
                },
            }),
            DaemonFrame::BulkChunk(BulkChunk {
                transfer_id,
                bytes: vec![1, 2],
            }),
            DaemonFrame::BulkFinished(BulkFinished { transfer_id }),
            DaemonFrame::BulkCanceled {
                transfer_id,
                reason: "canceled".into(),
            },
        ];
        for frame in frames {
            assert_eq!(
                decode_daemon(&encode_daemon(&frame).unwrap()).unwrap(),
                frame
            );
        }
    }

    fn voice_member(user_id: u64) -> VoiceMember {
        VoiceMember {
            user_id: crate::ids::UserId(user_id),
            name: "alice".into(),
            is_local: false,
            joined_ms: 0,
            status: VoiceMemberStatus::default(),
        }
    }

    fn roster_frame(members: Vec<VoiceMember>) -> DaemonFrame {
        DaemonFrame::Event(StateEvent {
            instance_id: DaemonInstanceId([4; 16]),
            event_seq: 1,
            delta: StateDelta::VoiceRosterReset {
                roster: Some(VoiceRoster {
                    room_id: RoomId(2),
                    members,
                }),
            },
        })
    }

    /// The roster is diffed by equality and rendered in wire order, so an
    /// unordered or duplicated one would make identical calls compare unequal
    /// and re-send on every projection.
    #[test]
    fn rejects_a_voice_roster_that_is_not_ordered_by_user_id() {
        assert!(encode_daemon(&roster_frame(vec![voice_member(1), voice_member(2)])).is_ok());
        assert!(encode_daemon(&roster_frame(vec![voice_member(2), voice_member(1)])).is_err());
        assert!(encode_daemon(&roster_frame(vec![voice_member(1), voice_member(1)])).is_err());
    }

    /// Updates are applied by user id, so a duplicate would make the frame's
    /// outcome depend on iteration order, and an empty one would be a frame
    /// that says nothing.
    #[test]
    fn rejects_voice_member_updates_that_are_empty_or_unordered() {
        let update = |user_id: u64| VoiceMemberUpdate {
            user_id: crate::ids::UserId(user_id),
            status: VoiceMemberStatus::default(),
        };
        let updated = |updates: Vec<VoiceMemberUpdate>| {
            DaemonFrame::Event(StateEvent {
                instance_id: DaemonInstanceId([4; 16]),
                event_seq: 1,
                delta: StateDelta::VoiceMembersUpdated { updates },
            })
        };
        assert!(encode_daemon(&updated(vec![update(1), update(2)])).is_ok());
        assert!(encode_daemon(&updated(vec![update(2), update(1)])).is_err());
        assert!(encode_daemon(&updated(Vec::new())).is_err());
    }

    /// The roster carries one name per caller, so it is bounded by the server's
    /// username limit rather than by the generic 16 KiB string cap.
    #[test]
    fn rejects_a_voice_member_name_beyond_the_username_limit() {
        let mut member = voice_member(1);
        member.name = "a".repeat(super::super::MAX_USERNAME_BYTES);
        assert!(encode_daemon(&roster_frame(vec![member.clone()])).is_ok());

        member.name.push('a');
        assert!(encode_daemon(&roster_frame(vec![member])).is_err());
    }

    /// Exactly one row is the viewer's own. Two would make "which row is me"
    /// depend on which the renderer happened to check first.
    #[test]
    fn rejects_a_voice_roster_with_two_local_members() {
        let local = |user_id: u64| VoiceMember {
            is_local: true,
            ..voice_member(user_id)
        };
        assert!(encode_daemon(&roster_frame(vec![local(1), voice_member(2)])).is_ok());
        assert!(encode_daemon(&roster_frame(vec![local(1), local(2)])).is_err());
    }

    #[test]
    fn live_share_frames_require_a_generation() {
        let request_id = RequestId(1);
        let stream_id = StreamId(5);
        assert!(
            encode_client(&ClientFrame::StartLiveShare {
                request_id,
                stream_id,
                generation: 0,
            })
            .is_err()
        );
        assert!(
            encode_client(&ClientFrame::StopLiveShare {
                request_id,
                stream_id,
                generation: 0,
            })
            .is_err()
        );
        assert!(
            encode_daemon(&DaemonFrame::LiveShareOpened {
                request_id,
                stream_id,
                generation: 0,
                status: LiveShareViewStatus::WaitingForKeyframe,
            })
            .is_err()
        );
        assert!(
            encode_daemon(&DaemonFrame::LiveShareStatus {
                stream_id,
                generation: 0,
                status: LiveShareViewStatus::Reconnecting,
            })
            .is_err()
        );
    }
}
