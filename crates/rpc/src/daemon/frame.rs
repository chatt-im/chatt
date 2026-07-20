use jsony::Jsony;

use crate::ids::{FileTransferId, MessageId, RoomId, StreamId};

use super::{
    bulk::{BeginAttachmentRead, BeginUpload, BulkChunk, BulkFinished, BulkStarted},
    model::{
        BulkTransferId, ConnectionState, DaemonInstanceId, LiveShare, Message, Participant,
        RequestId, RoomSnapshot, RoomSummary, StateSnapshot, TransferSummary, TrustState,
        VoiceState,
    },
};

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
        if self.min_version == 0 || self.min_version > self.max_version {
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
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct NegotiatedLimits {
    pub frame_bytes: u32,
    pub chunk_bytes: u32,
    pub message_bytes: u32,
    pub upload_bytes: u64,
    pub concurrent_transfers: u16,
    pub outstanding_requests: u16,
}

impl Default for NegotiatedLimits {
    fn default() -> Self {
        Self {
            frame_bytes: super::MAX_FRAME_BYTES as u32,
            chunk_bytes: super::MAX_CHUNK_BYTES as u32,
            message_bytes: super::MAX_MESSAGE_BODY_BYTES as u32,
            upload_bytes: crate::control::DEFAULT_FILE_SIZE_LIMIT_BYTES,
            concurrent_transfers: super::MAX_CONCURRENT_TRANSFERS as u16,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum Operation {
    SelectRoom,
    LoadOlder,
    SendMessage,
    EditMessage,
    DeleteMessage,
    BeginUpload,
    FinishUpload,
    CancelUpload,
    BeginAttachmentRead,
    CancelBulkTransfer,
    CancelFileTransfer,
    SetMuted,
    SetDeafened,
    JoinVoice,
    LeaveVoice,
    SetOutputVolume,
    StartLiveShare,
    StopLiveShare,
    Ping,
    RequestSnapshot,
    Disconnect,
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
        messages: Vec<Message>,
        older_cursor: Option<MessageId>,
        at_start: bool,
    },
    HistoryStateChanged {
        room_id: RoomId,
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
    ParticipantsChanged {
        room_id: RoomId,
        participants: Vec<Participant>,
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
    VoiceStateChanged {
        voice: VoiceState,
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
    SelectRoom {
        request_id: RequestId,
        room_id: RoomId,
    },
    LoadOlder {
        request_id: RequestId,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: u16,
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
    CancelBulkTransfer {
        request_id: RequestId,
        transfer_id: BulkTransferId,
    },
    CancelFileTransfer {
        request_id: RequestId,
        transfer_id: FileTransferId,
    },
    SetMuted {
        request_id: RequestId,
        muted: bool,
    },
    SetDeafened {
        request_id: RequestId,
        deafened: bool,
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
    },
    StopLiveShare {
        request_id: RequestId,
        stream_id: StreamId,
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
}

impl ClientFrame {
    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::SelectRoom { request_id, .. }
            | Self::LoadOlder { request_id, .. }
            | Self::SendMessage { request_id, .. }
            | Self::EditMessage { request_id, .. }
            | Self::DeleteMessage { request_id, .. }
            | Self::BeginUpload { request_id, .. }
            | Self::FinishUpload { request_id, .. }
            | Self::CancelUpload { request_id, .. }
            | Self::BeginAttachmentRead { request_id, .. }
            | Self::CancelBulkTransfer { request_id, .. }
            | Self::CancelFileTransfer { request_id, .. }
            | Self::SetMuted { request_id, .. }
            | Self::SetDeafened { request_id, .. }
            | Self::JoinVoice { request_id, .. }
            | Self::LeaveVoice { request_id }
            | Self::SetOutputVolume { request_id, .. }
            | Self::StartLiveShare { request_id, .. }
            | Self::StopLiveShare { request_id, .. }
            | Self::Ping { request_id, .. }
            | Self::RequestSnapshot { request_id }
            | Self::Disconnect { request_id } => Some(*request_id),
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
    LiveShareOpened {
        request_id: RequestId,
        stream_id: StreamId,
    },
    Pong {
        request_id: RequestId,
        nonce: u64,
    },
    BulkStarted(BulkStarted),
    BulkChunk(BulkChunk),
    BulkFinished(BulkFinished),
    BulkCanceled {
        transfer_id: BulkTransferId,
        reason: String,
    },
}

pub fn encode_client(frame: &ClientFrame) -> Result<Vec<u8>, String> {
    validate_client(frame)?;
    bounded_encode(frame)
}

/// Serializes a complete length-prefixed client frame into reusable storage.
///
/// The prefix is reserved before `jsony` writes the payload, so framing does
/// not require a second allocation or a payload copy.
pub fn encode_client_framed_into(frame: &ClientFrame, output: &mut Vec<u8>) -> Result<(), String> {
    validate_client(frame)?;
    bounded_encode_framed_into(frame, output)
}

pub fn decode_client(bytes: &[u8]) -> Result<ClientFrame, String> {
    let frame = bounded_decode(bytes)?;
    validate_client(&frame)?;
    Ok(frame)
}

pub fn encode_daemon(frame: &DaemonFrame) -> Result<Vec<u8>, String> {
    validate_daemon(frame)?;
    bounded_encode(frame)
}

/// Serializes a complete length-prefixed daemon frame into reusable storage.
pub fn encode_daemon_framed_into(frame: &DaemonFrame, output: &mut Vec<u8>) -> Result<(), String> {
    validate_daemon(frame)?;
    bounded_encode_framed_into(frame, output)
}

pub fn decode_daemon(bytes: &[u8]) -> Result<DaemonFrame, String> {
    let frame = bounded_decode(bytes)?;
    validate_daemon(&frame)?;
    Ok(frame)
}

fn bounded_encode<T: jsony::ToBinary>(value: &T) -> Result<Vec<u8>, String> {
    let bytes = jsony::to_binary(value);
    if bytes.len() > super::MAX_FRAME_BYTES {
        return Err("daemon frame exceeds maximum length".into());
    }
    Ok(bytes)
}

fn bounded_encode_framed_into<T: jsony::ToBinary>(
    value: &T,
    output: &mut Vec<u8>,
) -> Result<(), String> {
    output.clear();
    output.extend_from_slice(&[0; crate::frame::LENGTH_PREFIX_LEN]);
    let payload_len = jsony::to_binary_into(value, &mut *output).len();
    if payload_len > super::MAX_FRAME_BYTES {
        output.clear();
        return Err("daemon frame exceeds maximum length".into());
    }
    let payload_len =
        u32::try_from(payload_len).map_err(|_| "daemon frame length does not fit in u32")?;
    output[..crate::frame::LENGTH_PREFIX_LEN].copy_from_slice(&payload_len.to_le_bytes());
    Ok(())
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
        ClientFrame::SendMessage { body, .. } | ClientFrame::EditMessage { body, .. } => {
            if body.len() > super::MAX_MESSAGE_BODY_BYTES {
                return Err("message body exceeds limit".into());
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
            if *limit == 0 || *limit > crate::control::MAX_HISTORY_FETCH_MESSAGES =>
        {
            return Err("history request limit is invalid".into());
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
            Ok(())
        }
        DaemonFrame::Event(event) => {
            validate_instance_and_sequence(event.instance_id, event.event_seq)?;
            validate_delta(&event.delta)
        }
        DaemonFrame::RequestResult(result) => validate_result(result),
        DaemonFrame::LiveShareOpened { request_id, .. } if request_id.0 == 0 => {
            Err("request id must be nonzero".into())
        }
        DaemonFrame::Pong { request_id, .. } if request_id.0 == 0 => {
            Err("request id must be nonzero".into())
        }
        DaemonFrame::BulkStarted(started) => started.validate(),
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
        DaemonFrame::Pong { .. } | DaemonFrame::LiveShareOpened { .. } => Ok(()),
    }
}

fn validate_delta(delta: &StateDelta) -> Result<(), String> {
    match delta {
        StateDelta::ConnectionChanged { active_server, .. } => {
            super::model::check_opt_string(active_server)
        }
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
        StateDelta::ParticipantsChanged { participants, .. } => {
            if participants.len() > super::MAX_PARTICIPANTS {
                return Err("participant collection exceeds limit".into());
            }
            for participant in participants {
                participant.validate()?;
            }
            Ok(())
        }
        StateDelta::TransferChanged { transfer } => transfer.validate(),
        StateDelta::TransferRemoved { transfer_id } if transfer_id.0 == 0 => {
            Err("transfer id must be nonzero".into())
        }
        StateDelta::VoiceStateChanged { voice } => voice.validate(),
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
        let frame = ClientFrame::UploadChunk(BulkChunk {
            transfer_id: BulkTransferId(1),
            offset: 0,
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
            crate::frame::parse_frame_with_limit(&buffer, super::super::MAX_FRAME_BYTES)
                .unwrap()
                .unwrap();
        assert_eq!(consumed, buffer.len());
        assert_eq!(decode_client(payload).unwrap(), first);

        let second = ClientFrame::Ping {
            request_id: RequestId(2),
            nonce: 3,
        };
        encode_client_framed_into(&second, &mut buffer).unwrap();
        assert_eq!(buffer.capacity(), capacity);
        let (payload, _) =
            crate::frame::parse_frame_with_limit(&buffer, super::super::MAX_FRAME_BYTES)
                .unwrap()
                .unwrap();
        assert_eq!(decode_client(payload).unwrap(), second);
    }

    #[test]
    fn rejects_empty_chunks_and_out_of_range_volume() {
        assert!(
            encode_client(&ClientFrame::UploadChunk(BulkChunk {
                transfer_id: BulkTransferId(1),
                offset: 0,
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
            ClientFrame::SelectRoom {
                request_id,
                room_id,
            },
            ClientFrame::LoadOlder {
                request_id,
                room_id,
                before: Some(MessageId(4)),
                limit: 20,
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
                offset: 0,
                bytes: vec![1, 2],
            }),
            ClientFrame::FinishUpload {
                request_id,
                finished: BulkFinished {
                    transfer_id,
                    byte_len: 2,
                    digest: [1; 32],
                },
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
                    attachment_id: super::super::model::AttachmentId([2; 16]),
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
            ClientFrame::SetMuted {
                request_id,
                muted: true,
            },
            ClientFrame::SetDeafened {
                request_id,
                deafened: true,
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
            },
            ClientFrame::StopLiveShare {
                request_id,
                stream_id: StreamId(5),
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
    fn every_phase_one_daemon_frame_round_trips() {
        let request_id = RequestId(1);
        let transfer_id = BulkTransferId(3);
        let instance_id = DaemonInstanceId([4; 16]);
        let descriptor = super::super::model::AttachmentDescriptor {
            id: super::super::model::AttachmentId([2; 16]),
            file_name: "a.png".into(),
            media_kind: super::super::model::MediaKind::Image,
            content_type: "image/png".into(),
            byte_len: 2,
            digest: [1; 32],
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
            }),
            DaemonFrame::Snapshot {
                instance_id,
                event_seq: 1,
                snapshot: StateSnapshot {
                    connection: ConnectionState::Online,
                    active_server: Some("local".into()),
                    local_identity: Some("alice".into()),
                    rooms: Vec::new(),
                    selected_room: None,
                    room: None,
                    voice: VoiceState {
                        muted: false,
                        deafened: false,
                        output_volume: 100.0,
                        joined_room: None,
                    },
                    transfers: Vec::new(),
                    live_shares: Vec::new(),
                },
            },
            DaemonFrame::Event(StateEvent {
                instance_id,
                event_seq: 2,
                delta: StateDelta::DaemonStopping,
            }),
            DaemonFrame::RequestResult(RequestResult {
                request_id,
                operation: Operation::Ping,
                outcome: RequestOutcome::Accepted,
            }),
            DaemonFrame::LiveShareOpened {
                request_id,
                stream_id: StreamId(5),
            },
            DaemonFrame::Pong {
                request_id,
                nonce: 9,
            },
            DaemonFrame::BulkStarted(BulkStarted {
                transfer_id,
                attachment: descriptor,
            }),
            DaemonFrame::BulkChunk(BulkChunk {
                transfer_id,
                offset: 0,
                bytes: vec![1, 2],
            }),
            DaemonFrame::BulkFinished(BulkFinished {
                transfer_id,
                byte_len: 2,
                digest: [1; 32],
            }),
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
}
