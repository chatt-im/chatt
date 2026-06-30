use jsony::Jsony;

use crate::ids::{BugReportId, FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId};

pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_CHAT_BODY_BYTES: usize = 8 * 1024;
pub const DEFAULT_FILE_SIZE_LIMIT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_FILE_CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_FILE_NAME_BYTES: usize = 255;
pub const MAX_BUG_REPORT_DESC_BYTES: usize = 4 * 1024;
pub const MAX_BUG_REPORT_METADATA_BYTES: usize = 32 * 1024;
pub const MAX_BUG_REPORT_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_AUTH_FIELD_BYTES: usize = 512;
pub const MAX_VIDEO_CODEC_BYTES: usize = 64;
pub const MAX_VIDEO_EXTRADATA_BYTES: usize = 4 * 1024;
pub const MIN_PAIRING_SECRET_BYTES: usize = 16;
pub const MIN_PAIRED_TOKEN_BYTES: usize = 32;
pub const ERROR_AUTH_REJECTED: u16 = 401;
pub const ERROR_PAIRING_NOT_ACTIVE: u16 = 410;
pub const ERROR_PAIRING_CODE_MISMATCH: u16 = 409;
pub const ERROR_PAIRING_INVALID_REQUEST: u16 = 422;
pub const ERROR_BUG_REPORT_REJECTED: u16 = 400;
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
        display_name: String,
        token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    Pair {
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
    SetVoiceStatus {
        status: ParticipantVoiceStatus,
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
    StartShare {
        room_id: RoomId,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        annexb: bool,
        extradata: Vec<u8>,
    },
    StopShare {
        stream_id: StreamId,
    },
    Ping {
        nonce: u64,
    },
    BugReportStart {
        report_id: BugReportId,
        description: String,
        /// JSON metadata: app version, device/buffer config, `/audio` snapshot.
        metadata: String,
        /// Total length of the zstd-compressed log payload that follows.
        logs_size: u64,
    },
    BugReportChunk {
        report_id: BugReportId,
        offset: u64,
        data: Vec<u8>,
    },
    BugReportComplete {
        report_id: BugReportId,
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
    VoiceStatus {
        room_id: RoomId,
        user_id: UserId,
        status: ParticipantVoiceStatus,
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
    /// Maps an uploader's connection-local transfer id to the durable identity
    /// assigned by the server and used by the room's chat announcement.
    UploadFileAccepted {
        client_transfer_id: FileTransferId,
        file: FileMetadata,
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
    ShareStarted {
        stream_id: StreamId,
        publish_secret: Vec<u8>,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        extradata: Vec<u8>,
    },
    ShareAvailable {
        room_id: RoomId,
        stream_id: StreamId,
        user_id: UserId,
        sender_name: String,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        annexb: bool,
        extradata: Vec<u8>,
        view_secret: Vec<u8>,
    },
    ShareEnded {
        room_id: RoomId,
        stream_id: StreamId,
    },
    Pong {
        nonce: u64,
    },
    Error {
        code: u16,
        message: String,
    },
    BugReportSaved {
        report_id: BugReportId,
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
    pub display_name: String,
    pub identifier: String,
    pub in_call: bool,
    pub voice_status: ParticipantVoiceStatus,
    /// Server wall-clock (UNIX ms) the participant joined the room. Lets a late
    /// joiner render how long each participant has already been present.
    pub joined_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ParticipantVoiceStatus {
    pub muted: bool,
    pub deafened: bool,
}

impl ParticipantVoiceStatus {
    pub fn normalized(mut self) -> Self {
        if self.deafened {
            self.muted = true;
        }
        self
    }
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
    /// `Some` when this chat announces a file transfer, carrying that transfer's
    /// id. The web view correlates the announcement with the inline file by it.
    pub file_transfer_id: Option<FileTransferId>,
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
    pub stun_key: P2pKey,
    pub connection_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct InviteTicket {
    pub version: u16,
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

/// Whether a published P2P candidate address is acceptable: either a literal
/// `ip:port` socket address or an `{token}.local:port` mDNS host name.
fn is_valid_candidate_addr(addr: &str) -> bool {
    if addr.parse::<std::net::SocketAddr>().is_ok() {
        return true;
    }
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    port.parse::<u16>().is_ok() && is_valid_mdns_candidate_name(host)
}

/// Whether a host name is a single-label `.local` mDNS candidate name, matching
/// the WebRTC mDNS-ICE filter (one dot, `.local` suffix, alphanumeric label).
pub fn is_valid_mdns_candidate_name(name: &str) -> bool {
    let trimmed = name.trim_end_matches('.');
    if trimmed.len() > 80 || trimmed.matches('.').count() != 1 {
        return false;
    }
    let Some(label) = trimmed.strip_suffix(".local") else {
        return false;
    };
    !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn validate_client_control(value: &ClientControl) -> Result<(), String> {
    match value {
        ClientControl::Authenticate {
            token,
            file_receive_limit_bytes,
            ..
        } => {
            validate_auth_field("token", token)?;
            if *file_receive_limit_bytes > DEFAULT_FILE_SIZE_LIMIT_BYTES {
                return Err("file receive limit exceeds maximum".to_string());
            }
        }
        ClientControl::Pair {
            display_name,
            pairing_code,
            token,
            file_receive_limit_bytes,
            ..
        } => {
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
                if !is_valid_candidate_addr(&candidate.addr) {
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
        ClientControl::BugReportStart {
            description,
            metadata,
            logs_size,
            ..
        } => {
            if description.len() > MAX_BUG_REPORT_DESC_BYTES {
                return Err("bug report description exceeds maximum length".to_string());
            }
            if metadata.len() > MAX_BUG_REPORT_METADATA_BYTES {
                return Err("bug report metadata exceeds maximum length".to_string());
            }
            if *logs_size > MAX_BUG_REPORT_BYTES {
                return Err("bug report logs exceed maximum length".to_string());
            }
        }
        ClientControl::BugReportChunk { data, .. } => {
            if data.is_empty() {
                return Err("bug report chunk is empty".to_string());
            }
            if data.len() > MAX_FILE_CHUNK_BYTES {
                return Err("bug report chunk exceeds maximum length".to_string());
            }
        }
        ClientControl::StartShare {
            codec, extradata, ..
        } => {
            if codec.trim().is_empty() {
                return Err("share codec is empty".to_string());
            }
            if codec.len() > MAX_VIDEO_CODEC_BYTES {
                return Err("share codec exceeds maximum length".to_string());
            }
            if extradata.len() > MAX_VIDEO_EXTRADATA_BYTES {
                return Err("share extradata exceeds maximum length".to_string());
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
    fn accepts_socket_addr_and_mdns_candidate_addrs() {
        assert!(is_valid_candidate_addr("192.168.1.2:5000"));
        assert!(is_valid_candidate_addr("[fe80::1]:5000"));
        assert!(is_valid_candidate_addr("abc123.local:5000"));
        // Rejects multi-label names, missing port, and non-.local hosts.
        assert!(!is_valid_candidate_addr("sub.token.local:5000"));
        assert!(!is_valid_candidate_addr("token.local"));
        assert!(!is_valid_candidate_addr("router.lan:5000"));
        assert!(!is_valid_candidate_addr("token.local:notaport"));
        assert!(!is_valid_mdns_candidate_name(".local"));
        assert!(!is_valid_mdns_candidate_name("under_score.local"));
    }

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
    fn authenticate_control_round_trips_without_user() {
        let message = ClientControl::Authenticate {
            display_name: "Alice".to_string(),
            token: "client-generated-token-with-at-least-32-bytes".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        let encoded = encode_client_control(&message).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), message);
    }

    #[test]
    fn participant_info_round_trips_identifier() {
        let message = ServerControl::Presence {
            room_id: RoomId(1),
            participant: ParticipantInfo {
                user_id: UserId(5),
                display_name: "Alice".to_string(),
                identifier: "alice-internal".to_string(),
                in_call: true,
                voice_status: ParticipantVoiceStatus::default(),
                joined_at_ms: 0,
            },
            online: true,
        };
        let encoded = encode_server_control(&message);
        assert_eq!(decode_server_control(&encoded).unwrap(), message);
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
    fn p2p_peer_info_round_trips() {
        let control = ServerControl::P2pPeer {
            peer: P2pPeerInfo {
                room_id: RoomId(1),
                session_id: SessionId(2),
                user_id: UserId(3),
                generation: 4,
                role: P2pRole::Controlling,
                nat: P2pNatKind::Cone,
                tie_breaker: 99,
                candidates: vec![],
                send_key: P2pKey {
                    id: 1,
                    bytes: vec![1, 2, 3],
                },
                recv_key: P2pKey {
                    id: 2,
                    bytes: vec![4, 5, 6],
                },
                stun_key: P2pKey {
                    id: 3,
                    bytes: vec![7, 8, 9],
                },
                connection_id: 77,
            },
        };
        let encoded = encode_server_control(&control);
        assert_eq!(decode_server_control(&encoded).unwrap(), control);
    }

    #[test]
    fn voice_status_controls_round_trip() {
        let status = ParticipantVoiceStatus {
            muted: true,
            deafened: false,
        };
        let client = ClientControl::SetVoiceStatus { status };
        let encoded = encode_client_control(&client).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), client);

        let server = ServerControl::VoiceStatus {
            room_id: RoomId(1),
            user_id: UserId(2),
            status,
        };
        let encoded = encode_server_control(&server);
        assert_eq!(decode_server_control(&encoded).unwrap(), server);
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
            display_name: "Alice".to_string(),
            pairing_code: "short".to_string(),
            token: "also-short".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&weak).is_err());

        let strong = ClientControl::Pair {
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

        let accepted = ServerControl::UploadFileAccepted {
            client_transfer_id: FileTransferId(9),
            file: FileMetadata {
                transfer_id: FileTransferId(17),
                room_id: RoomId(2),
                sender: UserId(3),
                sender_name: "alice".to_string(),
                file_name: "report.pdf".to_string(),
                original_name: "report.pdf".to_string(),
                size: 1234,
                timestamp_ms: 55,
            },
        };
        let encoded = encode_server_control(&accepted);
        assert_eq!(decode_server_control(&encoded).unwrap(), accepted);
    }

    #[test]
    fn bug_report_control_round_trips() {
        let start = ClientControl::BugReportStart {
            report_id: BugReportId(4),
            description: "audio cut out after rejoin".to_string(),
            metadata: "{\"version\":\"0.1.0\"}".to_string(),
            logs_size: 2048,
        };
        let encoded = encode_client_control(&start).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), start);

        let chunk = ClientControl::BugReportChunk {
            report_id: BugReportId(4),
            offset: 32,
            data: vec![9, 8, 7],
        };
        let encoded = encode_client_control(&chunk).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), chunk);
    }

    #[test]
    fn bug_report_rejects_oversize_logs() {
        let control = ClientControl::BugReportStart {
            report_id: BugReportId(1),
            description: String::new(),
            metadata: String::new(),
            logs_size: MAX_BUG_REPORT_BYTES + 1,
        };
        assert!(encode_client_control(&control).is_err());
    }

    #[test]
    fn share_control_round_trips() {
        let start = ClientControl::StartShare {
            room_id: RoomId(3),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            annexb: true,
            extradata: vec![],
        };
        let encoded = encode_client_control(&start).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), start);

        let available = ServerControl::ShareAvailable {
            room_id: RoomId(3),
            stream_id: StreamId(8),
            user_id: UserId(4),
            sender_name: "Alice".to_string(),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            annexb: true,
            extradata: vec![],
            view_secret: vec![9; 32],
        };
        let encoded = encode_server_control(&available);
        assert_eq!(decode_server_control(&encoded).unwrap(), available);
    }

    #[test]
    fn rejects_empty_share_codec() {
        let start = ClientControl::StartShare {
            room_id: RoomId(1),
            codec: "  ".to_string(),
            coded_width: 0,
            coded_height: 0,
            annexb: true,
            extradata: vec![],
        };
        assert!(encode_client_control(&start).is_err());
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
