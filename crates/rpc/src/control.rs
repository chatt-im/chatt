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
pub const JOIN_STRING_PREFIX: &str = "tcj1_";
const MAX_JOIN_STRING_BYTES: usize = 4096;
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

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
        display_name: String,
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

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct InviteTicket {
    pub version: u16,
    pub user: String,
    pub pairing_code: String,
    pub tcp_addr: String,
    pub udp_addr: String,
    pub udp_probe_addr: Option<String>,
    pub server_public_key: String,
    pub room_id: u32,
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

pub fn encode_invite_ticket(value: &InviteTicket) -> Result<String, String> {
    validate_invite_ticket(value)?;
    let payload = jsony::to_binary(value);
    let mut out =
        String::with_capacity(JOIN_STRING_PREFIX.len() + encoded_base64_len(payload.len()));
    out.push_str(JOIN_STRING_PREFIX);
    encode_base64url_no_pad(&payload, &mut out);
    Ok(out)
}

pub fn decode_invite_ticket(value: &str) -> Result<InviteTicket, String> {
    let value = value.trim();
    if value.len() > MAX_JOIN_STRING_BYTES {
        return Err("join string exceeds maximum length".to_string());
    }
    let payload = value
        .strip_prefix(JOIN_STRING_PREFIX)
        .ok_or_else(|| "join string has an unknown prefix".to_string())?;
    let bytes = decode_base64url_no_pad(payload)?;
    let ticket: InviteTicket = bounded_decode(&bytes)?;
    validate_invite_ticket(&ticket)?;
    Ok(ticket)
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
            display_name,
            pairing_code,
            token,
            file_receive_limit_bytes,
            ..
        } => {
            validate_auth_field("user", user)?;
            validate_auth_field("display name", display_name)?;
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

fn validate_invite_ticket(value: &InviteTicket) -> Result<(), String> {
    if value.version != crate::PROTOCOL_VERSION {
        return Err(format!("unsupported invite version {}", value.version));
    }
    validate_auth_field("user", &value.user)?;
    validate_auth_field("pairing code", &value.pairing_code)?;
    if value.pairing_code.len() < MIN_PAIRING_SECRET_BYTES {
        return Err("pairing code is too short".to_string());
    }
    validate_auth_field("server public key", &value.server_public_key)?;
    validate_endpoint("invite TCP address", &value.tcp_addr)?;
    validate_endpoint("invite UDP address", &value.udp_addr)?;
    if let Some(addr) = &value.udp_probe_addr {
        validate_endpoint("invite UDP probe address", addr)?;
    }
    if value.room_id == 0 {
        return Err("invite room id must be non-zero".to_string());
    }
    Ok(())
}

fn validate_endpoint(name: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if value.len() > MAX_AUTH_FIELD_BYTES {
        return Err(format!("{name} exceeds maximum length"));
    }
    if value.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(());
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(format!("{name} must include a port"));
    };
    if host.trim().is_empty() || port.trim().is_empty() {
        return Err(format!("{name} must include a host and port"));
    }
    port.parse::<u16>()
        .map(|_| ())
        .map_err(|_| format!("{name} port is invalid"))
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

fn encoded_base64_len(len: usize) -> usize {
    (len / 3) * 4
        + match len % 3 {
            0 => 0,
            1 => 2,
            _ => 3,
        }
}

fn encode_base64url_no_pad(bytes: &[u8], out: &mut String) {
    let mut chunks = bytes.chunks_exact(3);
    for chunk in &mut chunks {
        let value = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
        out.push(BASE64URL[((value >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 12) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 6) & 0x3f) as usize] as char);
        out.push(BASE64URL[(value & 0x3f) as usize] as char);
    }
    let remainder = chunks.remainder();
    if remainder.len() == 1 {
        let value = (remainder[0] as u32) << 16;
        out.push(BASE64URL[((value >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 12) & 0x3f) as usize] as char);
    } else if remainder.len() == 2 {
        let value = ((remainder[0] as u32) << 16) | ((remainder[1] as u32) << 8);
        out.push(BASE64URL[((value >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 12) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 6) & 0x3f) as usize] as char);
    }
}

fn decode_base64url_no_pad(value: &str) -> Result<Vec<u8>, String> {
    if value.is_empty() {
        return Err("join string payload is empty".to_string());
    }
    if value.as_bytes().contains(&b'=') {
        return Err("join string payload must not contain padding".to_string());
    }
    if value.len() % 4 == 1 {
        return Err("join string payload length is invalid".to_string());
    }

    let mut out = Vec::with_capacity((value.len() / 4) * 3 + 2);
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        let a = decode_base64url_byte(bytes[index])?;
        let b = decode_base64url_byte(bytes[index + 1])?;
        let c = decode_base64url_byte(bytes[index + 2])?;
        let d = decode_base64url_byte(bytes[index + 3])?;
        let packed = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | d as u32;
        out.push(((packed >> 16) & 0xff) as u8);
        out.push(((packed >> 8) & 0xff) as u8);
        out.push((packed & 0xff) as u8);
        index += 4;
    }

    match bytes.len() - index {
        0 => {}
        2 => {
            let a = decode_base64url_byte(bytes[index])?;
            let b = decode_base64url_byte(bytes[index + 1])?;
            let packed = ((a as u32) << 18) | ((b as u32) << 12);
            out.push(((packed >> 16) & 0xff) as u8);
        }
        3 => {
            let a = decode_base64url_byte(bytes[index])?;
            let b = decode_base64url_byte(bytes[index + 1])?;
            let c = decode_base64url_byte(bytes[index + 2])?;
            let packed = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6);
            out.push(((packed >> 16) & 0xff) as u8);
            out.push(((packed >> 8) & 0xff) as u8);
        }
        _ => return Err("join string payload length is invalid".to_string()),
    }

    Ok(out)
}

fn decode_base64url_byte(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err("join string payload contains invalid base64url data".to_string()),
    }
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
            display_name: "Alice".to_string(),
            pairing_code: "short".to_string(),
            token: "also-short".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&weak).is_err());

        let strong = ClientControl::Pair {
            user: "alice".to_string(),
            display_name: "Alice".to_string(),
            pairing_code: "pair-alice-please-change".to_string(),
            token: "client-generated-token-with-at-least-32-bytes".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&strong).is_ok());
    }

    #[test]
    fn invite_ticket_round_trips_join_string() {
        let ticket = InviteTicket {
            version: crate::PROTOCOL_VERSION,
            user: "alice".to_string(),
            pairing_code: "pair-alice-please-change".to_string(),
            tcp_addr: "127.0.0.1:41000".to_string(),
            udp_addr: "127.0.0.1:41000".to_string(),
            udp_probe_addr: Some("127.0.0.1:41002".to_string()),
            server_public_key: "de1235b52a8b96f16f91124a8b462d463f2af83756946effa70e842142a6d7cf"
                .to_string(),
            room_id: 1,
        };

        let encoded = encode_invite_ticket(&ticket).unwrap();

        assert!(encoded.starts_with(JOIN_STRING_PREFIX));
        assert_eq!(decode_invite_ticket(&encoded).unwrap(), ticket);
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
