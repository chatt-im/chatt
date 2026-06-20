use jsony::Jsony;

use crate::ids::{FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId};

pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_CHAT_BODY_BYTES: usize = 8 * 1024;
pub const DEFAULT_FILE_SIZE_LIMIT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_FILE_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_FILE_NAME_BYTES: usize = 255;
pub const MAX_AUTH_FIELD_BYTES: usize = 512;
pub const MIN_PAIRING_SECRET_BYTES: usize = 16;
pub const MIN_PAIRED_TOKEN_BYTES: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ClientHello {
    pub version: u16,
    pub client_nonce: Vec<u8>,
    pub client_ephemeral: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ServerHello {
    pub version: u16,
    pub encrypted: bool,
    pub server_nonce: Vec<u8>,
    pub server_ephemeral: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ClientControl {
    Authenticate {
        user: String,
        token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    Pair {
        user: String,
        pairing_code: String,
        token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    JoinRoom {
        room_id: RoomId,
    },
    SendChat {
        room_id: RoomId,
        body: String,
    },
    StartVoice {
        room_id: RoomId,
    },
    StopVoice {
        stream_id: StreamId,
    },
    PublishP2p {
        room_id: RoomId,
        generation: u64,
        nat: P2pNatKind,
        tie_breaker: u64,
        candidates: Vec<P2pCandidate>,
    },
    UploadFileStart {
        room_id: RoomId,
        transfer_id: FileTransferId,
        name: String,
        size: u64,
    },
    UploadFileChunk {
        transfer_id: FileTransferId,
        offset: u64,
        data: Vec<u8>,
    },
    UploadFileComplete {
        transfer_id: FileTransferId,
    },
    UploadFileCancel {
        transfer_id: FileTransferId,
        reason: String,
    },
    Ping {
        nonce: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ServerControl {
    Authenticated {
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
        current_room: Option<RoomId>,
    },
    RoomJoined {
        room_id: RoomId,
        history: Vec<ChatMessage>,
        participants: Vec<ParticipantInfo>,
    },
    Chat {
        message: ChatMessage,
    },
    Presence {
        room_id: RoomId,
        participant: ParticipantInfo,
        online: bool,
    },
    VoiceStarted {
        room_id: RoomId,
        user_id: UserId,
        stream_id: StreamId,
    },
    VoiceStopped {
        room_id: RoomId,
        user_id: UserId,
        stream_id: StreamId,
    },
    UdpBound,
    UdpReflexive {
        addr: String,
    },
    P2pNatProbe {
        probe_id: u8,
        addr: String,
    },
    P2pPeer {
        peer: P2pPeerInfo,
    },
    P2pPeerGone {
        session_id: SessionId,
        user_id: UserId,
    },
    FileOffered {
        file: FileMetadata,
        contents: bool,
    },
    FileChunk {
        transfer_id: FileTransferId,
        offset: u64,
        data: Vec<u8>,
    },
    FileComplete {
        transfer_id: FileTransferId,
    },
    FileCanceled {
        transfer_id: FileTransferId,
        reason: String,
    },
    Pong {
        nonce: u64,
    },
    Error {
        code: u16,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RoomInfo {
    pub room_id: RoomId,
    pub name: String,
    pub participants: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ParticipantInfo {
    pub user_id: UserId,
    pub name: String,
    pub in_call: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ChatMessage {
    pub message_id: MessageId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub sender_name: String,
    pub timestamp_ms: u64,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct FileMetadata {
    pub transfer_id: FileTransferId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub sender_name: String,
    pub file_name: String,
    pub original_name: String,
    pub size: u64,
    pub timestamp_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum P2pNatKind {
    Unknown,
    Cone,
    Symmetric,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum P2pRole {
    Controlling,
    Controlled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum P2pCandidateKind {
    Host,
    ServerReflexive,
    PeerReflexive,
    PortMapped,
    Relay,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct P2pCandidate {
    pub id: u32,
    pub socket_id: u32,
    pub generation: u64,
    pub kind: P2pCandidateKind,
    pub addr: String,
    pub priority: u32,
    pub foundation: String,
    pub verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct P2pKey {
    pub id: u32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct P2pPeerInfo {
    pub room_id: RoomId,
    pub session_id: SessionId,
    pub user_id: UserId,
    pub generation: u64,
    pub role: P2pRole,
    pub nat: P2pNatKind,
    pub tie_breaker: u64,
    pub candidates: Vec<P2pCandidate>,
    pub send_key: P2pKey,
    pub recv_key: P2pKey,
    pub connection_id: u64,
}

pub fn encode_client_hello(value: &ClientHello) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_client_hello(bytes: &[u8]) -> Result<ClientHello, String> {
    bounded_decode(bytes)
}

pub fn encode_server_hello(value: &ServerHello) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_server_hello(bytes: &[u8]) -> Result<ServerHello, String> {
    bounded_decode(bytes)
}

pub fn encode_client_control(value: &ClientControl) -> Result<Vec<u8>, String> {
    validate_client_control(value)?;
    Ok(jsony::to_binary(value))
}

pub fn decode_client_control(bytes: &[u8]) -> Result<ClientControl, String> {
    let value: ClientControl = bounded_decode(bytes)?;
    validate_client_control(&value)?;
    Ok(value)
}

pub fn encode_server_control(value: &ServerControl) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_server_control(bytes: &[u8]) -> Result<ServerControl, String> {
    bounded_decode(bytes)
}

fn bounded_decode<T>(bytes: &[u8]) -> Result<T, String>
where
    T: for<'a> jsony::FromBinary<'a>,
{
    if bytes.len() > MAX_CONTROL_PAYLOAD_BYTES {
        return Err("control payload exceeds maximum length".to_string());
    }
    jsony::from_binary(bytes).map_err(|error| error.to_string())
}

fn validate_client_control(value: &ClientControl) -> Result<(), String> {
    match value {
        ClientControl::Authenticate {
            user,
            token,
            file_receive_limit_bytes,
            ..
        } => {
            validate_auth_field("user", user)?;
            validate_auth_field("token", token)?;
            if *file_receive_limit_bytes > DEFAULT_FILE_SIZE_LIMIT_BYTES {
                return Err("file receive limit exceeds maximum".to_string());
            }
        }
        ClientControl::Pair {
            user,
            pairing_code,
            token,
            file_receive_limit_bytes,
            ..
        } => {
            validate_auth_field("user", user)?;
            validate_auth_field("pairing code", pairing_code)?;
            validate_auth_field("token", token)?;
            if pairing_code.len() < MIN_PAIRING_SECRET_BYTES {
                return Err("pairing code is too short".to_string());
            }
            if token.len() < MIN_PAIRED_TOKEN_BYTES {
                return Err("paired token is too short".to_string());
            }
            if *file_receive_limit_bytes > DEFAULT_FILE_SIZE_LIMIT_BYTES {
                return Err("file receive limit exceeds maximum".to_string());
            }
        }
        ClientControl::SendChat { body, .. } => {
            if body.trim().is_empty() {
                return Err("chat message is empty".to_string());
            }
            if body.len() > MAX_CHAT_BODY_BYTES {
                return Err("chat message exceeds maximum length".to_string());
            }
        }
        ClientControl::PublishP2p { candidates, .. } => {
            if candidates.len() > 64 {
                return Err("too many P2P candidates".to_string());
            }
            for candidate in candidates {
                if candidate.addr.parse::<std::net::SocketAddr>().is_err() {
                    return Err("P2P candidate address is invalid".to_string());
                }
            }
        }
        ClientControl::UploadFileStart { name, size, .. } => {
            validate_file_name(name)?;
            if *size > DEFAULT_FILE_SIZE_LIMIT_BYTES {
                return Err("file exceeds maximum length".to_string());
            }
        }
        ClientControl::UploadFileChunk { data, .. } => {
            if data.is_empty() {
                return Err("file chunk is empty".to_string());
            }
            if data.len() > MAX_FILE_CHUNK_BYTES {
                return Err("file chunk exceeds maximum length".to_string());
            }
        }
        ClientControl::UploadFileCancel { reason, .. } => {
            if reason.len() > 512 {
                return Err("file cancel reason exceeds maximum length".to_string());
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_auth_field(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} is empty"));
    }
    if value.len() > MAX_AUTH_FIELD_BYTES {
        return Err(format!("{name} exceeds maximum length"));
    }
    Ok(())
}

fn validate_file_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("file name is empty".to_string());
    }
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err("file name exceeds maximum length".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("file name must not contain path separators".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_control_round_trips() {
        let message = ClientControl::SendChat {
            room_id: RoomId(7),
            body: "hello".to_string(),
        };
        let encoded = encode_client_control(&message).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), message);
    }

    #[test]
    fn p2p_control_round_trips() {
        let control = ClientControl::PublishP2p {
            room_id: RoomId(1),
            generation: 2,
            nat: P2pNatKind::Cone,
            tie_breaker: 99,
            candidates: vec![P2pCandidate {
                id: 1,
                socket_id: 1,
                generation: 2,
                kind: P2pCandidateKind::Host,
                addr: "192.168.1.2:5000".to_string(),
                priority: 1,
                foundation: "host-udp4".to_string(),
                verified: true,
            }],
        };
        let encoded = encode_client_control(&control).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), control);
    }

    #[test]
    fn rejects_empty_chat() {
        let message = ClientControl::SendChat {
            room_id: RoomId(7),
            body: "  ".to_string(),
        };
        assert!(encode_client_control(&message).is_err());
    }

    #[test]
    fn pair_control_requires_strong_one_time_and_session_secrets() {
        let weak = ClientControl::Pair {
            user: "alice".to_string(),
            pairing_code: "short".to_string(),
            token: "also-short".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&weak).is_err());

        let strong = ClientControl::Pair {
            user: "alice".to_string(),
            pairing_code: "pair-alice-please-change".to_string(),
            token: "client-generated-token-with-at-least-32-bytes".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&strong).is_ok());
    }

    #[test]
    fn file_upload_control_round_trips() {
        let control = ClientControl::UploadFileChunk {
            transfer_id: FileTransferId(9),
            offset: 32,
            data: vec![1, 2, 3],
        };
        let encoded = encode_client_control(&control).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), control);
    }

    #[test]
    fn rejects_oversized_file_chunk() {
        let control = ClientControl::UploadFileChunk {
            transfer_id: FileTransferId(9),
            offset: 0,
            data: vec![0; MAX_FILE_CHUNK_BYTES + 1],
        };
        assert!(encode_client_control(&control).is_err());
    }
}
