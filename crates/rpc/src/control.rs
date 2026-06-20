use jsony::Jsony;

use crate::ids::{MessageId, RoomId, SessionId, StreamId, UserId};

pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_CHAT_BODY_BYTES: usize = 8 * 1024;

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
    pub server_nonce: Vec<u8>,
    pub server_ephemeral: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ClientControl {
    Authenticate { user: String, token: String },
    JoinRoom { room_id: RoomId },
    SendChat { room_id: RoomId, body: String },
    StartVoice { room_id: RoomId },
    StopVoice { stream_id: StreamId },
    Ping { nonce: u64 },
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
    if let ClientControl::SendChat { body, .. } = value {
        if body.trim().is_empty() {
            return Err("chat message is empty".to_string());
        }
        if body.len() > MAX_CHAT_BODY_BYTES {
            return Err("chat message exceeds maximum length".to_string());
        }
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
    fn rejects_empty_chat() {
        let message = ClientControl::SendChat {
            room_id: RoomId(7),
            body: "  ".to_string(),
        };
        assert!(encode_client_control(&message).is_err());
    }
}
