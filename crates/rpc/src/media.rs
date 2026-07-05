use crate::crypto::{self, AntiReplay, CryptoError, KeyMaterial};
use crate::ids::{SessionId, StreamId};

pub const UDP_VERSION: u8 = 4;
pub const UDP_HEADER_LEN: usize = 14;
pub const SAFE_UDP_PAYLOAD_BYTES: usize = 1_200;
pub const MAX_VOICE_PAYLOAD_BYTES: usize = 1_024;
pub const PLAINTEXT_KEY_ID: u32 = 0;

pub const KIND_BIND: u8 = 1;
pub const KIND_VOICE: u8 = 2;
pub const KIND_PING: u8 = 3;
pub const KIND_PONG: u8 = 4;
pub const KIND_PEER_VOICE: u8 = 5;
pub const KIND_NAT_PROBE: u8 = 6;
pub const KIND_VOICE_FEEDBACK: u8 = 7;
pub const KIND_PEER_VOICE_FEEDBACK: u8 = 8;
const VOICE_PAYLOAD_OPUS: u8 = 0;
const VOICE_PAYLOAD_SILENCE: u8 = 1;

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
    NatProbe {
        session_id: SessionId,
        probe_id: u8,
    },
    Voice {
        stream_id: StreamId,
        sequence: u32,
        /// Media sample clock for the first sample in this packet, in 48 kHz
        /// samples. Unlike `sequence` it advances across sender silence, so the
        /// receiver's NetEQ packet buffer can reconstruct true inter-packet gaps.
        timestamp: u32,
        flags: u8,
        payload: VoicePayload,
    },
    PeerVoice {
        connection_id: u64,
        stream_id: StreamId,
        sequence: u32,
        /// See [`MediaPayload::Voice::timestamp`].
        timestamp: u32,
        flags: u8,
        payload: VoicePayload,
    },
    VoiceFeedback {
        stream_id: StreamId,
        feedback: VoiceFeedback,
    },
    PeerVoiceFeedback {
        connection_id: u64,
        stream_id: StreamId,
        feedback: VoiceFeedback,
    },
    Ping {
        nonce: u64,
        /// The sender's previous RTT estimate for its server link. Populated
        /// only on client-to-server probes; direct peer probes leave it `None`.
        observed_rtt_ms: Option<u16>,
    },
    Pong {
        nonce: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VoicePayload {
    Opus(Vec<u8>),
    Silence,
}

impl VoicePayload {
    pub fn len(&self) -> usize {
        match self {
            VoicePayload::Opus(opus) => opus.len(),
            VoicePayload::Silence => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceFeedback {
    pub highest_contiguous_sequence: u32,
    pub expected_packets: u16,
    pub lost_packets: u16,
    pub late_packets: u16,
    pub duplicate_packets: u16,
    pub reordered_packets: u16,
    pub window_ms: u16,
    /// Receiver-side staged playout depth in milliseconds. This reports the
    /// local mix-adapter carry, not a protocol buffer or sender-controlled
    /// queue.
    pub max_output_ring_ms: u16,
    pub max_neteq_target_ms: u16,
    pub max_neteq_playout_delay_ms: u16,
    pub max_neteq_packet_buffer_ms: u16,
    pub max_interarrival_jitter_ms: u16,
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
    let mut packet = Vec::new();
    let mut scratch = Vec::new();
    seal_media_into(key, counter, payload, &mut packet, &mut scratch)?;
    Ok(packet)
}

/// Seals `payload` into `packet` as a UDP media datagram, reusing `scratch` for
/// the encoded plaintext. Both buffers are cleared first, so callers reuse them
/// across frames to avoid per-frame allocation. Produces the same bytes as
/// [`seal_media`].
pub fn seal_media_into(
    key: &KeyMaterial,
    counter: u64,
    payload: &MediaPayload,
    packet: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Result<(), MediaError> {
    let kind = payload.kind();
    scratch.clear();
    encode_payload_into(payload, scratch)?;
    if scratch.len() > SAFE_UDP_PAYLOAD_BYTES {
        return Err(MediaError::PayloadTooLarge);
    }

    packet.clear();
    packet.push(UDP_VERSION);
    packet.push(kind);
    packet.extend_from_slice(&key.id.to_le_bytes());
    packet.extend_from_slice(&counter.to_le_bytes());
    // `packet[2..UDP_HEADER_LEN]` is the transport header (key id + counter); it
    // is authenticated as AAD instead of carrying a second copy in the body.
    let cipher_start = packet.len();
    debug_assert_eq!(cipher_start, UDP_HEADER_LEN);
    packet.extend_from_slice(scratch);

    let mut aad = [0u8; 1 + crypto::TRANSPORT_HEADER_LEN];
    aad[0] = crypto::CHANNEL_MEDIA;
    aad[1..].copy_from_slice(&packet[2..UDP_HEADER_LEN]);
    crypto::seal_in_place_append_tag(key, counter, &aad, cipher_start, packet)?;
    Ok(())
}

pub fn seal_plaintext_media(counter: u64, payload: &MediaPayload) -> Result<Vec<u8>, MediaError> {
    let mut packet = Vec::new();
    let mut scratch = Vec::new();
    seal_plaintext_media_into(counter, payload, &mut packet, &mut scratch)?;
    Ok(packet)
}

/// Buffer-reusing counterpart to [`seal_plaintext_media`]. Clears `packet` and
/// `scratch` before use.
pub fn seal_plaintext_media_into(
    counter: u64,
    payload: &MediaPayload,
    packet: &mut Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Result<(), MediaError> {
    let kind = payload.kind();
    scratch.clear();
    encode_payload_into(payload, scratch)?;
    if scratch.len() > SAFE_UDP_PAYLOAD_BYTES {
        return Err(MediaError::PayloadTooLarge);
    }

    packet.clear();
    packet.push(UDP_VERSION);
    packet.push(kind);
    packet.extend_from_slice(&PLAINTEXT_KEY_ID.to_le_bytes());
    packet.extend_from_slice(&counter.to_le_bytes());
    packet.extend_from_slice(scratch);
    Ok(())
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

pub fn open_plaintext_media(bytes: &[u8]) -> Result<(UdpHeader, MediaPayload), MediaError> {
    let (header, body) = parse_header(bytes)?;
    if header.key_id != PLAINTEXT_KEY_ID {
        return Err(CryptoError::WrongKeyId.into());
    }
    Ok((header, decode_payload(header.kind, body)?))
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
        KIND_BIND
        | KIND_VOICE
        | KIND_PING
        | KIND_PONG
        | KIND_PEER_VOICE
        | KIND_NAT_PROBE
        | KIND_VOICE_FEEDBACK
        | KIND_PEER_VOICE_FEEDBACK => {}
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
            MediaPayload::NatProbe { .. } => KIND_NAT_PROBE,
            MediaPayload::Voice { .. } => KIND_VOICE,
            MediaPayload::PeerVoice { .. } => KIND_PEER_VOICE,
            MediaPayload::VoiceFeedback { .. } => KIND_VOICE_FEEDBACK,
            MediaPayload::PeerVoiceFeedback { .. } => KIND_PEER_VOICE_FEEDBACK,
            MediaPayload::Ping { .. } => KIND_PING,
            MediaPayload::Pong { .. } => KIND_PONG,
        }
    }
}

pub fn encode_payload(payload: &MediaPayload) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::new();
    encode_payload_into(payload, &mut out)?;
    Ok(out)
}

/// Appends the wire encoding of `payload` to `out` without clearing it first.
/// [`encode_payload`] is the allocating convenience wrapper.
pub fn encode_payload_into(payload: &MediaPayload, out: &mut Vec<u8>) -> Result<(), MediaError> {
    out.push(payload.kind());
    match payload {
        MediaPayload::Bind { session_id } => {
            out.extend_from_slice(&session_id.0.to_le_bytes());
        }
        MediaPayload::NatProbe {
            session_id,
            probe_id,
        } => {
            out.extend_from_slice(&session_id.0.to_le_bytes());
            out.push(*probe_id);
        }
        MediaPayload::Voice {
            stream_id,
            sequence,
            timestamp,
            flags,
            payload,
        } => {
            out.extend_from_slice(&stream_id.0.to_le_bytes());
            out.extend_from_slice(&sequence.to_le_bytes());
            out.extend_from_slice(&timestamp.to_le_bytes());
            out.push(*flags);
            encode_voice_payload(payload, out)?;
        }
        MediaPayload::PeerVoice {
            connection_id,
            stream_id,
            sequence,
            timestamp,
            flags,
            payload,
        } => {
            out.extend_from_slice(&connection_id.to_le_bytes());
            out.extend_from_slice(&stream_id.0.to_le_bytes());
            out.extend_from_slice(&sequence.to_le_bytes());
            out.extend_from_slice(&timestamp.to_le_bytes());
            out.push(*flags);
            encode_voice_payload(payload, out)?;
        }
        MediaPayload::VoiceFeedback {
            stream_id,
            feedback,
        } => {
            out.extend_from_slice(&stream_id.0.to_le_bytes());
            encode_voice_feedback(*feedback, out);
        }
        MediaPayload::PeerVoiceFeedback {
            connection_id,
            stream_id,
            feedback,
        } => {
            out.extend_from_slice(&connection_id.to_le_bytes());
            out.extend_from_slice(&stream_id.0.to_le_bytes());
            encode_voice_feedback(*feedback, out);
        }
        MediaPayload::Ping {
            nonce,
            observed_rtt_ms,
        } => {
            out.extend_from_slice(&nonce.to_le_bytes());
            match observed_rtt_ms {
                Some(rtt_ms) => {
                    out.push(1);
                    out.extend_from_slice(&rtt_ms.to_le_bytes());
                }
                None => out.push(0),
            }
        }
        MediaPayload::Pong { nonce } => {
            out.extend_from_slice(&nonce.to_le_bytes());
        }
    }
    Ok(())
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
        KIND_NAT_PROBE => {
            if bytes.len() != 9 {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::NatProbe {
                session_id: SessionId(u64::from_le_bytes(bytes[0..8].try_into().unwrap())),
                probe_id: bytes[8],
            })
        }
        KIND_VOICE => {
            if bytes.len() < 16 {
                return Err(MediaError::InvalidPayload);
            }
            let stream_id = StreamId(u32::from_le_bytes(bytes[0..4].try_into().unwrap()));
            let sequence = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
            let timestamp = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
            let flags = bytes[12];
            let payload = decode_voice_payload(&bytes[13..])?;
            Ok(MediaPayload::Voice {
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
            })
        }
        KIND_PEER_VOICE => {
            if bytes.len() < 24 {
                return Err(MediaError::InvalidPayload);
            }
            let connection_id = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
            let stream_id = StreamId(u32::from_le_bytes(bytes[8..12].try_into().unwrap()));
            let sequence = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
            let timestamp = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
            let flags = bytes[20];
            let payload = decode_voice_payload(&bytes[21..])?;
            Ok(MediaPayload::PeerVoice {
                connection_id,
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
            })
        }
        KIND_VOICE_FEEDBACK => {
            if bytes.len() != 30 {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::VoiceFeedback {
                stream_id: StreamId(u32::from_le_bytes(bytes[0..4].try_into().unwrap())),
                feedback: decode_voice_feedback(&bytes[4..])?,
            })
        }
        KIND_PEER_VOICE_FEEDBACK => {
            if bytes.len() != 38 {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::PeerVoiceFeedback {
                connection_id: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
                stream_id: StreamId(u32::from_le_bytes(bytes[8..12].try_into().unwrap())),
                feedback: decode_voice_feedback(&bytes[12..])?,
            })
        }
        KIND_PING => {
            if bytes.len() != 9 && bytes.len() != 11 {
                return Err(MediaError::InvalidPayload);
            }
            let nonce = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
            let observed_rtt_ms = match (bytes[8], bytes.len()) {
                (0, 9) => None,
                (1, 11) => Some(u16::from_le_bytes(bytes[9..11].try_into().unwrap())),
                _ => return Err(MediaError::InvalidPayload),
            };
            Ok(MediaPayload::Ping {
                nonce,
                observed_rtt_ms,
            })
        }
        KIND_PONG => {
            if bytes.len() != 8 {
                return Err(MediaError::InvalidPayload);
            }
            let nonce = u64::from_le_bytes(bytes.try_into().unwrap());
            Ok(MediaPayload::Pong { nonce })
        }
        _ => Err(MediaError::UnknownKind(kind)),
    }
}

fn encode_voice_feedback(feedback: VoiceFeedback, out: &mut Vec<u8>) {
    out.extend_from_slice(&feedback.highest_contiguous_sequence.to_le_bytes());
    out.extend_from_slice(&feedback.expected_packets.to_le_bytes());
    out.extend_from_slice(&feedback.lost_packets.to_le_bytes());
    out.extend_from_slice(&feedback.late_packets.to_le_bytes());
    out.extend_from_slice(&feedback.duplicate_packets.to_le_bytes());
    out.extend_from_slice(&feedback.reordered_packets.to_le_bytes());
    out.extend_from_slice(&feedback.window_ms.to_le_bytes());
    out.extend_from_slice(&feedback.max_output_ring_ms.to_le_bytes());
    out.extend_from_slice(&feedback.max_neteq_target_ms.to_le_bytes());
    out.extend_from_slice(&feedback.max_neteq_playout_delay_ms.to_le_bytes());
    out.extend_from_slice(&feedback.max_neteq_packet_buffer_ms.to_le_bytes());
    out.extend_from_slice(&feedback.max_interarrival_jitter_ms.to_le_bytes());
}

fn encode_voice_payload(payload: &VoicePayload, out: &mut Vec<u8>) -> Result<(), MediaError> {
    match payload {
        VoicePayload::Opus(opus) => {
            if opus.is_empty() || opus.len() > MAX_VOICE_PAYLOAD_BYTES {
                return Err(MediaError::PayloadTooLarge);
            }
            out.push(VOICE_PAYLOAD_OPUS);
            let len = u16::try_from(opus.len()).map_err(|_| MediaError::PayloadTooLarge)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(opus);
        }
        VoicePayload::Silence => {
            out.push(VOICE_PAYLOAD_SILENCE);
            out.extend_from_slice(&0u16.to_le_bytes());
        }
    }
    Ok(())
}

fn decode_voice_payload(bytes: &[u8]) -> Result<VoicePayload, MediaError> {
    if bytes.len() < 3 {
        return Err(MediaError::InvalidPayload);
    }
    let len = u16::from_le_bytes(bytes[1..3].try_into().unwrap()) as usize;
    match bytes[0] {
        VOICE_PAYLOAD_OPUS => {
            if len == 0 || len > MAX_VOICE_PAYLOAD_BYTES || bytes.len() != 3 + len {
                return Err(MediaError::InvalidPayload);
            }
            Ok(VoicePayload::Opus(bytes[3..].to_vec()))
        }
        VOICE_PAYLOAD_SILENCE => {
            if len != 0 || bytes.len() != 3 {
                return Err(MediaError::InvalidPayload);
            }
            Ok(VoicePayload::Silence)
        }
        _ => Err(MediaError::InvalidPayload),
    }
}

fn decode_voice_feedback(bytes: &[u8]) -> Result<VoiceFeedback, MediaError> {
    if bytes.len() != 26 {
        return Err(MediaError::InvalidPayload);
    }
    Ok(VoiceFeedback {
        highest_contiguous_sequence: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        expected_packets: u16::from_le_bytes(bytes[4..6].try_into().unwrap()),
        lost_packets: u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
        late_packets: u16::from_le_bytes(bytes[8..10].try_into().unwrap()),
        duplicate_packets: u16::from_le_bytes(bytes[10..12].try_into().unwrap()),
        reordered_packets: u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
        window_ms: u16::from_le_bytes(bytes[14..16].try_into().unwrap()),
        max_output_ring_ms: u16::from_le_bytes(bytes[16..18].try_into().unwrap()),
        max_neteq_target_ms: u16::from_le_bytes(bytes[18..20].try_into().unwrap()),
        max_neteq_playout_delay_ms: u16::from_le_bytes(bytes[20..22].try_into().unwrap()),
        max_neteq_packet_buffer_ms: u16::from_le_bytes(bytes[22..24].try_into().unwrap()),
        max_interarrival_jitter_ms: u16::from_le_bytes(bytes[24..26].try_into().unwrap()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_payload_round_trips() {
        let payload = MediaPayload::Voice {
            stream_id: StreamId(9),
            sequence: 42,
            timestamp: 40_320,
            flags: 3,
            payload: VoicePayload::Opus(vec![1, 2, 3]),
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
            timestamp: 40_320,
            flags: 3,
            payload: VoicePayload::Opus(vec![1, 2, 3]),
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_PEER_VOICE, &encoded).unwrap(), payload);
    }

    #[test]
    fn silence_voice_payload_round_trips() {
        let payload = MediaPayload::Voice {
            stream_id: StreamId(9),
            sequence: 42,
            timestamp: 40_320,
            flags: 3,
            payload: VoicePayload::Silence,
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_VOICE, &encoded).unwrap(), payload);
    }

    #[test]
    fn voice_feedback_payload_round_trips() {
        let feedback = VoiceFeedback {
            highest_contiguous_sequence: 80,
            expected_packets: 25,
            lost_packets: 4,
            late_packets: 2,
            duplicate_packets: 1,
            reordered_packets: 3,
            window_ms: 500,
            max_output_ring_ms: 240,
            max_neteq_target_ms: 120,
            max_neteq_playout_delay_ms: 160,
            max_neteq_packet_buffer_ms: 80,
            max_interarrival_jitter_ms: 87,
        };
        let payload = MediaPayload::VoiceFeedback {
            stream_id: StreamId(9),
            feedback,
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(
            decode_payload(KIND_VOICE_FEEDBACK, &encoded).unwrap(),
            payload
        );
    }

    #[test]
    fn peer_voice_feedback_payload_round_trips() {
        let feedback = VoiceFeedback {
            highest_contiguous_sequence: 81,
            expected_packets: 26,
            lost_packets: 5,
            late_packets: 3,
            duplicate_packets: 2,
            reordered_packets: 4,
            window_ms: 501,
            max_output_ring_ms: 241,
            max_neteq_target_ms: 121,
            max_neteq_playout_delay_ms: 161,
            max_neteq_packet_buffer_ms: 81,
            max_interarrival_jitter_ms: 88,
        };
        let payload = MediaPayload::PeerVoiceFeedback {
            connection_id: 99,
            stream_id: StreamId(9),
            feedback,
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(
            decode_payload(KIND_PEER_VOICE_FEEDBACK, &encoded).unwrap(),
            payload
        );
    }

    #[test]
    fn nat_probe_payload_round_trips() {
        let payload = MediaPayload::NatProbe {
            session_id: SessionId(42),
            probe_id: 2,
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_NAT_PROBE, &encoded).unwrap(), payload);
    }

    #[test]
    fn ping_payload_round_trips_with_observed_rtt() {
        let payload = MediaPayload::Ping {
            nonce: 456,
            observed_rtt_ms: Some(37),
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_PING, &encoded).unwrap(), payload);
    }

    #[test]
    fn ping_payload_round_trips_without_observed_rtt() {
        let payload = MediaPayload::Ping {
            nonce: 456,
            observed_rtt_ms: None,
        };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_PING, &encoded).unwrap(), payload);
    }

    #[test]
    fn encrypted_media_round_trips_and_rejects_replay() {
        let key = KeyMaterial {
            id: 77,
            bytes: [8; crypto::KEY_LEN],
        };
        let payload = MediaPayload::Ping {
            nonce: 123,
            observed_rtt_ms: Some(25),
        };
        let packet = seal_media(&key, 0, &payload).unwrap();
        let mut replay = AntiReplay::new();
        assert_eq!(open_media(&key, &mut replay, &packet).unwrap().1, payload);
        assert_eq!(
            open_media(&key, &mut replay, &packet).unwrap_err(),
            MediaError::Replay
        );
    }

    #[test]
    fn plaintext_media_round_trips_and_rejects_replay() {
        let payload = MediaPayload::Ping {
            nonce: 123,
            observed_rtt_ms: None,
        };
        let packet = seal_plaintext_media(0, &payload).unwrap();
        let (header, decoded) = open_plaintext_media(&packet).unwrap();
        assert_eq!(header.key_id, PLAINTEXT_KEY_ID);
        assert_eq!(decoded, payload);

        let mut replay = AntiReplay::new();
        assert!(replay.update(header.counter));
        assert_eq!(open_plaintext_media(&packet).unwrap().1, payload);
        assert!(!replay.update(header.counter));
    }

    #[test]
    fn seal_media_into_matches_seal_media_byte_for_byte() {
        let key = KeyMaterial {
            id: 77,
            bytes: [8; crypto::KEY_LEN],
        };
        let payload = MediaPayload::Voice {
            stream_id: StreamId(5),
            sequence: 9,
            timestamp: 8_640,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3, 4, 5, 6, 7, 8]),
        };

        let expected = seal_media(&key, 3, &payload).unwrap();

        // Pre-populate the reusable buffers with stale data to prove they are
        // cleared, mirroring the steady-state reuse in the client worker.
        let mut packet = vec![0xAA; 64];
        let mut scratch = vec![0xBB; 64];
        seal_media_into(&key, 3, &payload, &mut packet, &mut scratch).unwrap();
        assert_eq!(packet, expected);

        let mut replay = AntiReplay::new();
        assert_eq!(open_media(&key, &mut replay, &packet).unwrap().1, payload);
    }

    #[test]
    fn seal_media_into_matches_seal_media_across_fan_out() {
        // The server relay reuses one packet/scratch pair while sealing the
        // same payload for many recipients under different keys and counters;
        // every sealed datagram must match the allocating path byte for byte.
        let keys = [
            KeyMaterial {
                id: 7,
                bytes: [1; crypto::KEY_LEN],
            },
            KeyMaterial {
                id: 8,
                bytes: [2; crypto::KEY_LEN],
            },
        ];
        let payloads = [
            MediaPayload::Voice {
                stream_id: StreamId(5),
                sequence: 9,
                timestamp: 8_640,
                flags: 0,
                payload: VoicePayload::Opus(vec![1, 2, 3, 4, 5, 6, 7, 8]),
            },
            MediaPayload::Voice {
                stream_id: StreamId(5),
                sequence: 10,
                timestamp: 9_600,
                flags: 1,
                payload: VoicePayload::Silence,
            },
            MediaPayload::Pong { nonce: 44 },
        ];

        let mut packet = Vec::new();
        let mut scratch = Vec::new();
        let mut counter = 0u64;
        for payload in &payloads {
            for key in &keys {
                let expected = seal_media(key, counter, payload).unwrap();
                seal_media_into(key, counter, payload, &mut packet, &mut scratch).unwrap();
                assert_eq!(packet, expected);
                counter += 1;
            }
        }
    }

    #[test]
    fn seal_plaintext_media_into_matches_seal_plaintext_media() {
        let payload = MediaPayload::Voice {
            stream_id: StreamId(5),
            sequence: 9,
            timestamp: 8_640,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3]),
        };
        let expected = seal_plaintext_media(6, &payload).unwrap();
        let mut packet = vec![0xAA; 64];
        let mut scratch = vec![0xBB; 64];
        seal_plaintext_media_into(6, &payload, &mut packet, &mut scratch).unwrap();
        assert_eq!(packet, expected);
    }
}
