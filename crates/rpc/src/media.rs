use crate::crypto::{self, AntiReplay, CryptoError, KeyMaterial, SessionTransport, TransportMode};
use crate::ids::StreamId;

pub const UDP_VERSION: u8 = 4;
pub const UDP_HEADER_LEN: usize = 14;
pub const SAFE_UDP_PAYLOAD_BYTES: usize = 1_200;
pub const MAX_VOICE_PAYLOAD_BYTES: usize = 1_024;

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
    /// Per-session UDP demux tag. For server-session datagrams this is the
    /// session's derived media route id; for direct P2P peer datagrams it is the
    /// peer key id. Authenticated as AAD (native) or covered by the bind proof
    /// (external-link `Bind`).
    pub route_id: u32,
    pub counter: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaPayload {
    /// Claims (or refreshes) the session's UDP address. The session is
    /// identified by the datagram's route id; the media codec adds the address
    /// proof on the wire in external-link mode.
    Bind,
    NatProbe {
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

/// How a session's UDP media datagrams are protected, selected once from the
/// session [`TransportMode`].
///
/// `Aead` seals every datagram with the directional session keys and demuxes by
/// `route_id`. `Clear` sends payloads in the clear (the outer secure link
/// protects them) but authenticates `Bind` address claims with a truncated HMAC
/// under `bind_key`, so a spoofed datagram cannot rebind the session.
#[derive(Clone)]
pub enum MediaProtection {
    Aead {
        route_id: u32,
        send: KeyMaterial,
        recv: KeyMaterial,
    },
    Clear {
        route_id: u32,
        bind_key: [u8; crypto::KEY_LEN],
        mode: TransportMode,
    },
}

impl MediaProtection {
    /// Builds the media codec selected by the session's negotiated mode.
    pub fn from_transport(transport: &SessionTransport) -> Self {
        match transport.mode {
            TransportMode::NativeEncrypted => MediaProtection::Aead {
                route_id: transport.route_id,
                send: transport.secrets.media_send.clone(),
                recv: transport.secrets.media_recv.clone(),
            },
            TransportMode::ExternalSecureLink => MediaProtection::Clear {
                route_id: transport.route_id,
                bind_key: transport.bind_key,
                mode: transport.mode,
            },
        }
    }

    pub fn route_id(&self) -> u32 {
        match self {
            MediaProtection::Aead { route_id, .. } | MediaProtection::Clear { route_id, .. } => {
                *route_id
            }
        }
    }
}

/// Proof that an opened datagram may act on the session's UDP address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressProof {
    /// Native-encrypted: the whole datagram is AEAD-authenticated.
    AuthenticatedDatagram,
    /// External-link `Bind`: the address claim carries a valid bind proof.
    AuthenticatedAddressClaim,
    /// External-link data: unauthenticated by chatt, accepted only from the
    /// already-bound address.
    None,
}

/// A successfully opened media datagram together with its address-proof status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedMedia {
    pub header: UdpHeader,
    pub payload: MediaPayload,
    pub address_proof: AddressProof,
}

/// Domain-separated message covered by the external-link `Bind` proof. Binds the
/// route id, UDP kind, counter, and selected mode so a proof cannot be replayed
/// under a different session, kind, counter, or transport mode.
fn bind_proof_message(route_id: u32, kind: u8, counter: u64, mode: TransportMode) -> [u8; 39] {
    let mut msg = [0u8; 39];
    msg[0..25].copy_from_slice(b"chatt media bind proof v1");
    msg[25..29].copy_from_slice(&route_id.to_le_bytes());
    msg[29] = kind;
    msg[30..38].copy_from_slice(&counter.to_le_bytes());
    msg[38] = mode.wire_id();
    msg
}

pub fn seal_media(
    protection: &MediaProtection,
    counter: u64,
    payload: &MediaPayload,
) -> Result<Vec<u8>, MediaError> {
    let mut packet = Vec::new();
    let mut scratch = Vec::new();
    seal_media_into(protection, counter, payload, &mut packet, &mut scratch)?;
    Ok(packet)
}

/// Seals `payload` into `packet` as a UDP media datagram under `protection`,
/// reusing `scratch` for the encoded plaintext. Both buffers are cleared first,
/// so callers reuse them across frames to avoid per-frame allocation.
///
/// In `Aead` mode the body is AEAD-sealed. In `Clear` mode the body is written
/// in the clear; a `Bind` additionally carries a 16-byte proof, so voice and the
/// other data kinds add no per-packet overhead beyond the shared UDP header.
pub fn seal_media_into(
    protection: &MediaProtection,
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
    packet.extend_from_slice(&protection.route_id().to_le_bytes());
    packet.extend_from_slice(&counter.to_le_bytes());
    let cipher_start = packet.len();
    debug_assert_eq!(cipher_start, UDP_HEADER_LEN);
    packet.extend_from_slice(scratch);

    match protection {
        MediaProtection::Aead { send, .. } => {
            // `packet[2..UDP_HEADER_LEN]` (route id + counter) is authenticated as
            // AAD instead of carrying a second copy in the body.
            let mut aad = [0u8; 1 + crypto::TRANSPORT_HEADER_LEN];
            aad[0] = crypto::CHANNEL_MEDIA;
            aad[1..].copy_from_slice(&packet[2..UDP_HEADER_LEN]);
            crypto::seal_in_place_append_tag(send, counter, &aad, cipher_start, packet)?;
        }
        MediaProtection::Clear { bind_key, mode, .. } => {
            if kind == KIND_BIND {
                let message = bind_proof_message(protection.route_id(), kind, counter, *mode);
                packet.extend_from_slice(&crypto::auth_proof(bind_key, &message));
            }
        }
    }
    Ok(())
}

/// Opens a session UDP media datagram under `protection`, returning the header,
/// decoded payload, and how far the datagram is authenticated. Anti-replay is
/// enforced for authenticated datagrams only: every `Aead` datagram, and every
/// `Clear` `Bind` after its proof verifies. Clear data kinds are unauthenticated
/// by chatt (the outer link protects them) and carry no replay state.
pub fn open_media(
    protection: &MediaProtection,
    replay: &mut AntiReplay,
    bytes: &[u8],
) -> Result<OpenedMedia, MediaError> {
    let (header, body) = parse_header(bytes)?;
    if header.route_id != protection.route_id() {
        return Err(CryptoError::WrongKeyId.into());
    }

    match protection {
        MediaProtection::Aead { recv, .. } => {
            let mut buf = body.to_vec();
            let mut aad = [0u8; 1 + crypto::TRANSPORT_HEADER_LEN];
            aad[0] = crypto::CHANNEL_MEDIA;
            aad[1..].copy_from_slice(&bytes[2..UDP_HEADER_LEN]);
            let plaintext_len =
                crypto::open_in_place_with_aad(recv, header.counter, &aad, &mut buf)?;
            buf.truncate(plaintext_len);
            if !replay.update(header.counter) {
                return Err(MediaError::Replay);
            }
            Ok(OpenedMedia {
                header,
                payload: decode_payload(header.kind, &buf)?,
                address_proof: AddressProof::AuthenticatedDatagram,
            })
        }
        MediaProtection::Clear { bind_key, mode, .. } => {
            if header.kind == KIND_BIND {
                if body.len() < crypto::AUTH_PROOF_LEN {
                    return Err(MediaError::InvalidPayload);
                }
                let split = body.len() - crypto::AUTH_PROOF_LEN;
                let (payload_bytes, proof) = body.split_at(split);
                let message =
                    bind_proof_message(header.route_id, header.kind, header.counter, *mode);
                if !crypto::auth_proof_verify(bind_key, &message, proof) {
                    return Err(MediaError::Crypto("bind proof mismatch".to_string()));
                }
                if !replay.update(header.counter) {
                    return Err(MediaError::Replay);
                }
                Ok(OpenedMedia {
                    header,
                    payload: decode_payload(header.kind, payload_bytes)?,
                    address_proof: AddressProof::AuthenticatedAddressClaim,
                })
            } else {
                Ok(OpenedMedia {
                    header,
                    payload: decode_payload(header.kind, body)?,
                    address_proof: AddressProof::None,
                })
            }
        }
    }
}

/// Seals a direct P2P peer datagram under a raw AEAD `key`, using the key id as
/// the header route tag. P2P runs only in native-encrypted mode, so peer media
/// is always AEAD; this is the raw-key counterpart to [`seal_media_into`].
pub fn seal_peer_media_into(
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
    let cipher_start = packet.len();
    debug_assert_eq!(cipher_start, UDP_HEADER_LEN);
    packet.extend_from_slice(scratch);

    let mut aad = [0u8; 1 + crypto::TRANSPORT_HEADER_LEN];
    aad[0] = crypto::CHANNEL_MEDIA;
    aad[1..].copy_from_slice(&packet[2..UDP_HEADER_LEN]);
    crypto::seal_in_place_append_tag(key, counter, &aad, cipher_start, packet)?;
    Ok(())
}

pub fn seal_peer_media(
    key: &KeyMaterial,
    counter: u64,
    payload: &MediaPayload,
) -> Result<Vec<u8>, MediaError> {
    let mut packet = Vec::new();
    let mut scratch = Vec::new();
    seal_peer_media_into(key, counter, payload, &mut packet, &mut scratch)?;
    Ok(packet)
}

/// Opens a direct P2P peer datagram sealed by [`seal_peer_media_into`], checking
/// the header route tag against `key.id` and enforcing anti-replay.
pub fn open_peer_media(
    key: &KeyMaterial,
    replay: &mut AntiReplay,
    bytes: &[u8],
) -> Result<(UdpHeader, MediaPayload), MediaError> {
    let (header, body) = parse_header(bytes)?;
    if header.route_id != key.id {
        return Err(CryptoError::WrongKeyId.into());
    }

    let mut transport = Vec::with_capacity(crypto::TRANSPORT_HEADER_LEN + body.len());
    transport.extend_from_slice(&header.route_id.to_le_bytes());
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
            route_id: u32::from_le_bytes(bytes[2..6].try_into().unwrap()),
            counter: u64::from_le_bytes(bytes[6..14].try_into().unwrap()),
        },
        &bytes[UDP_HEADER_LEN..],
    ))
}

impl MediaPayload {
    pub fn kind(&self) -> u8 {
        match self {
            MediaPayload::Bind => KIND_BIND,
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
        MediaPayload::Bind => {}
        MediaPayload::NatProbe { probe_id } => {
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
            if !bytes.is_empty() {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::Bind)
        }
        KIND_NAT_PROBE => {
            if bytes.len() != 1 {
                return Err(MediaError::InvalidPayload);
            }
            Ok(MediaPayload::NatProbe { probe_id: bytes[0] })
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
        let payload = MediaPayload::NatProbe { probe_id: 2 };
        let encoded = encode_payload(&payload).unwrap();
        assert_eq!(decode_payload(KIND_NAT_PROBE, &encoded).unwrap(), payload);
    }

    fn aead_protection(route_id: u32) -> MediaProtection {
        // A self-test protection whose send and recv keys match, so one instance
        // both seals and opens a datagram (as a client's send key matches the
        // server's recv key for that session).
        let key = KeyMaterial {
            id: 42,
            bytes: [7; crypto::KEY_LEN],
        };
        MediaProtection::Aead {
            route_id,
            send: key.clone(),
            recv: key,
        }
    }

    fn clear_protection(route_id: u32) -> MediaProtection {
        MediaProtection::Clear {
            route_id,
            bind_key: [3; crypto::KEY_LEN],
            mode: TransportMode::ExternalSecureLink,
        }
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
    fn native_media_round_trips_and_rejects_replay() {
        let protection = aead_protection(9);
        let payload = MediaPayload::Ping {
            nonce: 123,
            observed_rtt_ms: Some(25),
        };
        let packet = seal_media(&protection, 0, &payload).unwrap();
        let mut replay = AntiReplay::new();
        let opened = open_media(&protection, &mut replay, &packet).unwrap();
        assert_eq!(opened.payload, payload);
        assert_eq!(opened.address_proof, AddressProof::AuthenticatedDatagram);
        assert_eq!(
            open_media(&protection, &mut replay, &packet).unwrap_err(),
            MediaError::Replay
        );
    }

    #[test]
    fn native_media_rejects_tamper() {
        let protection = aead_protection(9);
        let payload = MediaPayload::Pong { nonce: 7 };
        let mut packet = seal_media(&protection, 0, &payload).unwrap();
        let last = packet.len() - 1;
        packet[last] ^= 0x55;
        let mut replay = AntiReplay::new();
        assert!(open_media(&protection, &mut replay, &packet).is_err());
    }

    #[test]
    fn media_open_rejects_wrong_route_id() {
        let payload = MediaPayload::Pong { nonce: 7 };
        let packet = seal_media(&aead_protection(9), 0, &payload).unwrap();
        let mut replay = AntiReplay::new();
        assert_eq!(
            open_media(&aead_protection(10), &mut replay, &packet).unwrap_err(),
            MediaError::Crypto(CryptoError::WrongKeyId.to_string())
        );
    }

    #[test]
    fn external_voice_has_no_aead_overhead() {
        let protection = clear_protection(9);
        let payload = MediaPayload::Voice {
            stream_id: StreamId(5),
            sequence: 9,
            timestamp: 8_640,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3, 4, 5, 6, 7, 8]),
        };
        let packet = seal_media(&protection, 0, &payload).unwrap();
        // Clear voice carries exactly the UDP header plus the encoded payload —
        // no transport header, tag, or bind proof.
        assert_eq!(
            packet.len(),
            UDP_HEADER_LEN + encode_payload(&payload).unwrap().len()
        );
        let mut replay = AntiReplay::new();
        let opened = open_media(&protection, &mut replay, &packet).unwrap();
        assert_eq!(opened.payload, payload);
        assert_eq!(opened.address_proof, AddressProof::None);
    }

    #[test]
    fn external_bind_with_valid_proof_binds() {
        let protection = clear_protection(9);
        let packet = seal_media(&protection, 0, &MediaPayload::Bind).unwrap();
        let mut replay = AntiReplay::new();
        let opened = open_media(&protection, &mut replay, &packet).unwrap();
        assert_eq!(opened.payload, MediaPayload::Bind);
        assert_eq!(
            opened.address_proof,
            AddressProof::AuthenticatedAddressClaim
        );
    }

    #[test]
    fn external_bind_with_wrong_proof_is_rejected() {
        let protection = clear_protection(9);
        let mut packet = seal_media(&protection, 0, &MediaPayload::Bind).unwrap();
        let last = packet.len() - 1;
        packet[last] ^= 0x01;
        let mut replay = AntiReplay::new();
        assert!(open_media(&protection, &mut replay, &packet).is_err());
    }

    #[test]
    fn external_bind_from_wrong_key_is_rejected() {
        // A blind spoofer without the bind key cannot forge an accepted Bind.
        let sender = MediaProtection::Clear {
            route_id: 9,
            bind_key: [1; crypto::KEY_LEN],
            mode: TransportMode::ExternalSecureLink,
        };
        let receiver = MediaProtection::Clear {
            route_id: 9,
            bind_key: [2; crypto::KEY_LEN],
            mode: TransportMode::ExternalSecureLink,
        };
        let packet = seal_media(&sender, 0, &MediaPayload::Bind).unwrap();
        let mut replay = AntiReplay::new();
        assert!(open_media(&receiver, &mut replay, &packet).is_err());
    }

    #[test]
    fn external_bind_replay_is_rejected() {
        let protection = clear_protection(9);
        let packet = seal_media(&protection, 5, &MediaPayload::Bind).unwrap();
        let mut replay = AntiReplay::new();
        assert!(open_media(&protection, &mut replay, &packet).is_ok());
        // A previously accepted Bind (same counter) cannot roll the session back.
        assert_eq!(
            open_media(&protection, &mut replay, &packet).unwrap_err(),
            MediaError::Replay
        );
    }

    #[test]
    fn seal_media_into_matches_seal_media_byte_for_byte() {
        let protection = aead_protection(9);
        let payload = MediaPayload::Voice {
            stream_id: StreamId(5),
            sequence: 9,
            timestamp: 8_640,
            flags: 0,
            payload: VoicePayload::Opus(vec![1, 2, 3, 4, 5, 6, 7, 8]),
        };

        let expected = seal_media(&protection, 3, &payload).unwrap();

        // Pre-populate the reusable buffers with stale data to prove they are
        // cleared, mirroring the steady-state reuse in the client worker.
        let mut packet = vec![0xAA; 64];
        let mut scratch = vec![0xBB; 64];
        seal_media_into(&protection, 3, &payload, &mut packet, &mut scratch).unwrap();
        assert_eq!(packet, expected);

        let mut replay = AntiReplay::new();
        assert_eq!(
            open_media(&protection, &mut replay, &packet)
                .unwrap()
                .payload,
            payload
        );
    }

    #[test]
    fn peer_media_round_trips_and_rejects_replay() {
        let key = KeyMaterial {
            id: 77,
            bytes: [8; crypto::KEY_LEN],
        };
        let payload = MediaPayload::PeerVoice {
            connection_id: 99,
            stream_id: StreamId(9),
            sequence: 42,
            timestamp: 40_320,
            flags: 3,
            payload: VoicePayload::Opus(vec![1, 2, 3]),
        };
        let packet = seal_peer_media(&key, 0, &payload).unwrap();
        let mut replay = AntiReplay::new();
        assert_eq!(
            open_peer_media(&key, &mut replay, &packet).unwrap().1,
            payload
        );
        assert_eq!(
            open_peer_media(&key, &mut replay, &packet).unwrap_err(),
            MediaError::Replay
        );
    }
}
