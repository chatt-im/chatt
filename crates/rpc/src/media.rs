use crate::crypto::{self, AntiReplay, CryptoError, KeyMaterial};
use crate::ids::{SessionId, StreamId};

pub const UDP_VERSION: u8 = 1;
pub const UDP_HEADER_LEN: usize = 14;
pub const SAFE_UDP_PAYLOAD_BYTES: usize = 1_200;
pub const MAX_VOICE_PAYLOAD_BYTES: usize = 1_024;

pub const KIND_BIND: u8 = 1;
pub const KIND_VOICE: u8 = 2;
pub const KIND_PING: u8 = 3;
pub const KIND_PONG: u8 = 4;
pub const KIND_PEER_VOICE: u8 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpHeader {
    pub version: u8,
    pub kind: u8,
    pub key_id: u32,
    pub counter: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaPayload {
    Bind {
        session_id: SessionId,
    },
    Voice {
        stream_id: StreamId,
        sequence: u32,
        flags: u8,
        opus: Vec<u8>,
    },
    PeerVoice {
        connection_id: u64,
        stream_id: StreamId,
        sequence: u32,
        flags: u8,
        opus: Vec<u8>,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaError {
    TooShort,
    UnsupportedVersion(u8),
    UnknownKind(u8),
    PayloadTooLarge,
    InvalidPayload,
    Crypto(String),
    Replay,
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::TooShort => f.write_str("media datagram is too short"),
            MediaError::UnsupportedVersion(version) => {
                write!(f, "unsupported UDP protocol version {version}")
            }
            MediaError::UnknownKind(kind) => write!(f, "unknown UDP media kind {kind}"),
            MediaError::PayloadTooLarge => f.write_str("media payload exceeds maximum length"),
            MediaError::InvalidPayload => f.write_str("invalid media payload"),
            MediaError::Crypto(error) => write!(f, "media crypto error: {error}"),
            MediaError::Replay => f.write_str("media datagram replay detected"),
        }
    }
}

impl std::error::Error for MediaError {}

impl From<CryptoError> for MediaError {
    fn from(value: CryptoError) -> Self {
        MediaError::Crypto(value.to_string())
    }
}

pub fn seal_media(
    key: &KeyMaterial,
    counter: u64,
    payload: &MediaPayload,
) -> Result<Vec<u8>, MediaError> {
    let kind = payload.kind();
    let plaintext = encode_payload(payload)?;
    if plaintext.len() > SAFE_UDP_PAYLOAD_BYTES {
        return Err(MediaError::PayloadTooLarge);
    }

    let mut header = [0u8; UDP_HEADER_LEN];
    header[0] = UDP_VERSION;
    header[1] = kind;
    header[2..6].copy_from_slice(&key.id.to_le_bytes());
    header[6..14].copy_from_slice(&counter.to_le_bytes());

    let mut sealed = crypto::seal_with_key(key, crypto::CHANNEL_MEDIA, counter, &plaintext)?;
    debug_assert_eq!(
        sealed.len(),
        crypto::TRANSPORT_HEADER_LEN + plaintext.len() + crypto::TAG_LEN
    );
    let mut out = Vec::with_capacity(UDP_HEADER_LEN + sealed.len() - crypto::TRANSPORT_HEADER_LEN);
    out.extend_from_slice(&header);
    out.extend_from_slice(&sealed.split_off(crypto::TRANSPORT_HEADER_LEN));
    Ok(out)
}

pub fn open_media(
    key: &KeyMaterial,
    replay: &mut AntiReplay,
    bytes: &[u8],
) -> Result<(UdpHeader, MediaPayload), MediaError> {
    let (header, body) = parse_header(bytes)?;
    if header.key_id != key.id {
        return Err(CryptoError::WrongKeyId.into());
    }

    let mut transport = Vec::with_capacity(crypto::TRANSPORT_HEADER_LEN + body.len());
    transport.extend_from_slice(&header.key_id.to_le_bytes());
    transport.extend_from_slice(&header.counter.to_le_bytes());
    transport.extend_from_slice(body);
    let (_, plaintext) = crypto::open_with_key(key, crypto::CHANNEL_MEDIA, &transport)?;
    if !replay.update(header.counter) {
        return Err(MediaError::Replay);
    }
    Ok((header, decode_payload(header.kind, &plaintext)?))
}

pub fn parse_header(bytes: &[u8]) -> Result<(UdpHeader, &[u8]), MediaError> {
    if bytes.len() < UDP_HEADER_LEN {
        return Err(MediaError::TooShort);
    }
    let version = bytes[0];
    if version != UDP_VERSION {
        return Err(MediaError::UnsupportedVersion(version));
    }
    let kind = bytes[1];
    match kind {
        KIND_BIND | KIND_VOICE | KIND_PING | KIND_PONG | KIND_PEER_VOICE => {}
        _ => return Err(MediaError::UnknownKind(kind)),
    }
    Ok((
        UdpHeader {
            version,
            kind,
            key_id: u32::from_le_bytes(bytes[2..6].try_into().unwrap()),
            counter: u64::from_le_bytes(bytes[6..14].try_into().unwrap()),
        },
        &bytes[UDP_HEADER_LEN..],
    ))
}

impl MediaPayload {
    pub fn kind(&self) -> u8 {
        match self {
            MediaPayload::Bind { .. } => KIND_BIND,
            MediaPayload::Voice { .. } => KIND_VOICE,
            MediaPayload::PeerVoice { .. } => KIND_PEER_VOICE,
            MediaPayload::Ping { .. } => KIND_PING,
            MediaPayload::Pong { .. } => KIND_PONG,
        }
    }
}

pub fn encode_payload(payload: &MediaPayload) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::new();
    out.push(payload.kind());
    match payload {
        MediaPayload::Bind { session_id } => {
            out.extend_from_slice(&session_id.0.to_le_bytes());
        }
        MediaPayload::Voice {
            stream_id,
            sequence,
            flags,
            opus,
        } => {
            if opus.is_empty() || opus.len() > MAX_VOICE_PAYLOAD_BYTES {
                return Err(MediaError::PayloadTooLarge);
            }
            out.extend_from_slice(&stream_id.0.to_le_bytes());
            out.extend_from_slice(&sequence.to_le_bytes());
            out.push(*flags);
            let len = u16::try_from(opus.len()).map_err(|_| MediaError::PayloadTooLarge)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(opus);
        }
        MediaPayload::PeerVoice {
            connection_id,
            stream_id,
            sequence,
            flags,
            opus,
        } => {
            if opus.is_empty() || opus.len() > MAX_VOICE_PAYLOAD_BYTES {
                return Err(MediaError::PayloadTooLarge);
            }
            out.extend_from_slice(&connection_id.to_le_bytes());
            out.extend_from_slice(&stream_id.0.to_le_bytes());
            out.extend_from_slice(&sequence.to_le_bytes());
            out.push(*flags);
            let len = u16::try_from(opus.len()).map_err(|_| MediaError::PayloadTooLarge)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(opus);
        }
        MediaPayload::Ping { nonce } | MediaPayload::Pong { nonce } => {
            out.extend_from_slice(&nonce.to_le_bytes());
        }
    }
    Ok(out)
}

pub fn decode_payload(kind: u8, bytes: &[u8]) -> Result<MediaPayload, MediaError> {
    let Some((&payload_kind, bytes)) = bytes.split_first() else {
        return Err(MediaError::InvalidPayload);
    };
    if payload_kind != kind {
        return Err(MediaError::InvalidPayload);
    }
    match kind {
        KIND_BIND => {
            if bytes.len() != 8 {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::Bind {
                session_id: SessionId(u64::from_le_bytes(bytes.try_into().unwrap())),
            })
        }
        KIND_VOICE => {
            if bytes.len() < 11 {
                return Err(MediaError::InvalidPayload);
            }
            let stream_id = StreamId(u32::from_le_bytes(bytes[0..4].try_into().unwrap()));
            let sequence = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            let flags = bytes[8];
            let len = u16::from_le_bytes(bytes[9..11].try_into().unwrap()) as usize;
            if len == 0 || len > MAX_VOICE_PAYLOAD_BYTES || bytes.len() != 11 + len {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::Voice {
                stream_id,
                sequence,
                flags,
                opus: bytes[11..].to_vec(),
            })
        }
        KIND_PEER_VOICE => {
            if bytes.len() < 19 {
                return Err(MediaError::InvalidPayload);
            }
            let connection_id = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
            let stream_id = StreamId(u32::from_le_bytes(bytes[8..12].try_into().unwrap()));
            let sequence = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
            let flags = bytes[16];
            let len = u16::from_le_bytes(bytes[17..19].try_into().unwrap()) as usize;
            if len == 0 || len > MAX_VOICE_PAYLOAD_BYTES || bytes.len() != 19 + len {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::PeerVoice {
                connection_id,
                stream_id,
                sequence,
                flags,
                opus: bytes[19..].to_vec(),
            })
        }
        KIND_PING | KIND_PONG => {
            if bytes.len() != 8 {
                return Err(MediaError::InvalidPayload);
            }
            let nonce = u64::from_le_bytes(bytes.try_into().unwrap());
            if kind == KIND_PING {
                Ok(MediaPayload::Ping { nonce })
            } else {
                Ok(MediaPayload::Pong { nonce })
            }
        }
        _ => Err(MediaError::UnknownKind(kind)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_payload_round_trips() {
        let payload = MediaPayload::Voice {
            stream_id: StreamId(9),
            sequence: 42,
            flags: 3,
            opus: vec![1, 2, 3],
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_VOICE, &encoded).unwrap(), payload);
    }

    #[test]
    fn peer_voice_payload_round_trips() {
        let payload = MediaPayload::PeerVoice {
            connection_id: 99,
            stream_id: StreamId(9),
            sequence: 42,
            flags: 3,
            opus: vec![1, 2, 3],
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_PEER_VOICE, &encoded).unwrap(), payload);
    }

    #[test]
    fn encrypted_media_round_trips_and_rejects_replay() {
        let key = KeyMaterial {
            id: 77,
            bytes: [8; crypto::KEY_LEN],
        };
        let payload = MediaPayload::Ping { nonce: 123 };
        let packet = seal_media(&key, 0, &payload).unwrap();
        let mut replay = AntiReplay::new();
        assert_eq!(open_media(&key, &mut replay, &packet).unwrap().1, payload);
        assert_eq!(
            open_media(&key, &mut replay, &packet).unwrap_err(),
            MediaError::Replay
        );
    }
}
