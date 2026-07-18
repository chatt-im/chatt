use aws_lc_rs::{
    aead::{self, Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey},
    agreement, digest, hkdf, hmac, rand,
    rand::SecureRandom,
    signature::{self, KeyPair, VerificationAlgorithm},
};

use jsony::Jsony;

use crate::PROTOCOL_VERSION;
use crate::control::{ClientHello, ServerHello};
use crate::ids::UserId;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 32;
pub const X25519_PUBLIC_KEY_LEN: usize = 32;
pub const ED25519_PUBLIC_KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const TRANSPORT_HEADER_LEN: usize = 12;
pub const CHANNEL_CONTROL: u8 = 1;
pub const CHANNEL_MEDIA: u8 = 2;
pub const CHANNEL_VIDEO: u8 = 3;
/// Prefix marking an opaque dynamic-user bearer token, distinguishing it from an
/// explicit user's plaintext token during authentication.
pub const DYNAMIC_TOKEN_PREFIX: &str = "tct1_";
/// Prefix marking a client-generated open-pair recovery secret. The server
/// deterministically derives a dynamic user id from this secret so retrying a
/// request whose response was lost recovers the same identity.
pub const OPEN_PAIR_RECOVERY_PREFIX: &str = "tcr1_";
/// AEAD channel byte domain-separating dynamic token sealing from transport frames.
const TOKEN_CHANNEL: u8 = 0;
pub const REKEY_AFTER_TIME_SECS: u64 = 120;
pub const REJECT_AFTER_TIME_SECS: u64 = 180;
pub const REJECT_AFTER_MESSAGES: u64 = u64::MAX - (1 << 4);

const DEV_SERVER_SEED: [u8; KEY_LEN] = [
    0x54, 0x6f, 0x6d, 0x63, 0x68, 0x61, 0x74, 0x20, 0x64, 0x65, 0x76, 0x20, 0x73, 0x65, 0x72, 0x76,
    0x65, 0x72, 0x20, 0x6b, 0x65, 0x79, 0x20, 0x76, 0x31, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptoError {
    Random,
    InvalidHandshake,
    InvalidSignature,
    InvalidKey,
    InvalidEncoding,
    UnsupportedVersion(u16),
    WrongKeyId,
    CounterMismatch,
    Replay,
    CounterExhausted,
    Cipher,
    BindProofMismatch,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::Random => f.write_str("secure random generation failed"),
            CryptoError::InvalidHandshake => f.write_str("invalid handshake"),
            CryptoError::InvalidSignature => f.write_str("server signature verification failed"),
            CryptoError::InvalidKey => f.write_str("invalid key material"),
            CryptoError::InvalidEncoding => f.write_str("invalid encoded key material"),
            CryptoError::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version {version}")
            }
            CryptoError::WrongKeyId => f.write_str("encrypted frame key id mismatch"),
            CryptoError::CounterMismatch => f.write_str("encrypted frame counter mismatch"),
            CryptoError::Replay => f.write_str("encrypted frame replay detected"),
            CryptoError::CounterExhausted => f.write_str("encrypted frame counter exhausted"),
            CryptoError::Cipher => f.write_str("authenticated encryption failure"),
            CryptoError::BindProofMismatch => f.write_str("bind proof mismatch"),
        }
    }
}

impl std::error::Error for CryptoError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyMaterial {
    pub id: u32,
    pub bytes: [u8; KEY_LEN],
}

impl Drop for KeyMaterial {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSecrets {
    pub control_send: KeyMaterial,
    pub control_recv: KeyMaterial,
    pub media_send: KeyMaterial,
    pub media_recv: KeyMaterial,
}

/// The trust boundary a session runs under, negotiated by the signed handshake.
///
/// Both modes derive full session material; the mode only decides whether chatt
/// itself protects record and datagram payloads or defers to an outer secure
/// link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    /// Chatt secures the wire: control, media, video, and file payloads are AEAD
    /// protected by session keys.
    NativeEncrypted,
    /// An outer tunnel secures the wire: payloads travel clear after the signed
    /// handshake, but UDP address claims still carry proof of possession. P2P is
    /// unavailable because it would bypass the outer link.
    ExternalSecureLink,
}

impl TransportMode {
    pub const fn wire_id(self) -> u8 {
        match self {
            TransportMode::NativeEncrypted => 1,
            TransportMode::ExternalSecureLink => 2,
        }
    }

    pub fn from_wire_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(TransportMode::NativeEncrypted),
            2 => Some(TransportMode::ExternalSecureLink),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            TransportMode::NativeEncrypted => "native-encrypted",
            TransportMode::ExternalSecureLink => "external-secure-link",
        }
    }

    /// The transport-mode ids this build supports, advertised in `ClientHello`.
    pub fn supported_wire_ids() -> Vec<u8> {
        vec![
            TransportMode::NativeEncrypted.wire_id(),
            TransportMode::ExternalSecureLink.wire_id(),
        ]
    }
}

/// Everything a session derives from the handshake: the negotiated mode, the
/// directional AEAD material, the UDP demux route id, and the auth keys for
/// external-link UDP address claims and video connection setup. This is the
/// object callers ask to build the concrete lane codecs; they do not choose
/// per-lane security states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTransport {
    pub mode: TransportMode,
    pub secrets: SessionSecrets,
    pub route_id: u32,
    pub bind_key: [u8; KEY_LEN],
    pub video_auth_key: [u8; KEY_LEN],
}

impl SessionTransport {
    /// Builds the control-lane record codec selected by the session mode.
    pub fn control_record(&self) -> RecordProtection {
        match self.mode {
            TransportMode::NativeEncrypted => RecordProtection::aead(
                self.secrets.control_send.clone(),
                self.secrets.control_recv.clone(),
            ),
            TransportMode::ExternalSecureLink => RecordProtection::clear(),
        }
    }
}

pub struct ClientHandshake {
    private: agreement::EphemeralPrivateKey,
    pub hello: ClientHello,
}

pub struct ServerHandshake {
    pub hello: ServerHello,
    pub transport: SessionTransport,
}

pub fn dev_server_public_key() -> [u8; ED25519_PUBLIC_KEY_LEN] {
    dev_server_key_pair()
        .public_key()
        .as_ref()
        .try_into()
        .unwrap()
}

pub fn dev_server_seed_hex() -> String {
    encode_hex(&DEV_SERVER_SEED)
}

pub fn dev_server_key_pair() -> signature::Ed25519KeyPair {
    signature::Ed25519KeyPair::from_seed_unchecked(&DEV_SERVER_SEED)
        .expect("hard-coded dev Ed25519 seed is valid")
}

pub fn server_key_pair_from_seed_hex(
    seed_hex: &str,
) -> Result<signature::Ed25519KeyPair, CryptoError> {
    let seed = decode_fixed_hex::<KEY_LEN>(seed_hex)?;
    signature::Ed25519KeyPair::from_seed_unchecked(&seed).map_err(|_| CryptoError::InvalidKey)
}

pub fn ed25519_public_key_from_hex(
    public_key_hex: &str,
) -> Result<[u8; ED25519_PUBLIC_KEY_LEN], CryptoError> {
    decode_fixed_hex::<ED25519_PUBLIC_KEY_LEN>(public_key_hex)
}

pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub fn decode_hex(value: &str) -> Result<Vec<u8>, CryptoError> {
    let value = value.trim();
    if value.len() % 2 != 0 {
        return Err(CryptoError::InvalidEncoding);
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for chunk in value.as_bytes().chunks_exact(2) {
        let high = decode_hex_nibble(chunk[0])?;
        let low = decode_hex_nibble(chunk[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn decode_fixed_hex<const N: usize>(value: &str) -> Result<[u8; N], CryptoError> {
    let decoded = decode_hex(value)?;
    decoded.try_into().map_err(|_| CryptoError::InvalidEncoding)
}

fn decode_hex_nibble(byte: u8) -> Result<u8, CryptoError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(CryptoError::InvalidEncoding),
    }
}

pub fn generate_client_hello(rng: &dyn rand::SecureRandom) -> Result<ClientHandshake, CryptoError> {
    let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, rng)
        .map_err(|_| CryptoError::Random)?;
    let public = private
        .compute_public_key()
        .map_err(|_| CryptoError::Random)?;
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).map_err(|_| CryptoError::Random)?;
    Ok(ClientHandshake {
        private,
        hello: ClientHello {
            version: PROTOCOL_VERSION,
            modes: TransportMode::supported_wire_ids(),
            client_nonce: nonce.to_vec(),
            client_ephemeral: public.as_ref().to_vec(),
        },
    })
}

/// Runs the server side of the signed handshake for the server-selected
/// transport `mode`, deriving full session material regardless of mode.
///
/// The client must have advertised `mode` in its `ClientHello`; otherwise the
/// connection is rejected. The signature covers the selected mode and both
/// ephemeral keys, so a downgrade cannot be forged.
pub fn respond_to_client_hello(
    rng: &dyn rand::SecureRandom,
    server_key_pair: &signature::Ed25519KeyPair,
    client_hello: &ClientHello,
    mode: TransportMode,
) -> Result<ServerHandshake, CryptoError> {
    validate_client_hello(client_hello)?;
    if !client_hello.modes.contains(&mode.wire_id()) {
        return Err(CryptoError::InvalidHandshake);
    }
    let private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, rng)
        .map_err(|_| CryptoError::Random)?;
    let public = private
        .compute_public_key()
        .map_err(|_| CryptoError::Random)?;
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).map_err(|_| CryptoError::Random)?;

    let shared = agreement::agree_ephemeral(
        private,
        &agreement::UnparsedPublicKey::new(&agreement::X25519, &client_hello.client_ephemeral),
        (),
        |shared| Ok(shared.to_vec()),
    )
    .map_err(|_| CryptoError::InvalidHandshake)?;

    let unsigned = server_transcript(
        client_hello,
        PROTOCOL_VERSION,
        mode.wire_id(),
        &nonce,
        public.as_ref(),
        server_key_pair.public_key().as_ref(),
    );
    let signature = server_key_pair.sign(&unsigned);
    let hello = ServerHello {
        version: PROTOCOL_VERSION,
        mode: mode.wire_id(),
        server_nonce: nonce.to_vec(),
        server_ephemeral: public.as_ref().to_vec(),
        server_public_key: server_key_pair.public_key().as_ref().to_vec(),
        signature: signature.as_ref().to_vec(),
    };
    let transcript = full_transcript(client_hello, &hello, server_key_pair.public_key().as_ref())?;
    let transport = derive_session_transport(Role::Server, mode, &shared, &transcript);
    Ok(ServerHandshake { hello, transport })
}

/// Completes the client handshake, returning the derived [`SessionTransport`]
/// and the Ed25519 key that was trusted. When `pinned_server_public_key` is
/// `None` the server's presented key is trusted on first use and returned so the
/// caller can pin it for later connections.
///
/// The server-selected mode must be one the client advertised, and the
/// signature over the transcript (which binds that mode and both ephemerals) is
/// always verified.
pub fn complete_client_transport_handshake(
    handshake: ClientHandshake,
    server_hello: &ServerHello,
    pinned_server_public_key: Option<&[u8; ED25519_PUBLIC_KEY_LEN]>,
) -> Result<(SessionTransport, [u8; ED25519_PUBLIC_KEY_LEN]), CryptoError> {
    validate_server_hello(server_hello)?;
    let mode =
        TransportMode::from_wire_id(server_hello.mode).ok_or(CryptoError::InvalidHandshake)?;
    if !handshake.hello.modes.contains(&server_hello.mode) {
        return Err(CryptoError::InvalidHandshake);
    }
    let trusted = resolve_server_public_key(server_hello, pinned_server_public_key)?;
    let transcript = full_transcript(&handshake.hello, server_hello, &trusted)?;
    signature::ED25519
        .verify_sig(
            &trusted,
            &server_transcript(
                &handshake.hello,
                server_hello.version,
                server_hello.mode,
                &server_hello.server_nonce,
                &server_hello.server_ephemeral,
                &trusted,
            ),
            &server_hello.signature,
        )
        .map_err(|_| CryptoError::InvalidSignature)?;

    let shared = agreement::agree_ephemeral(
        handshake.private,
        &agreement::UnparsedPublicKey::new(&agreement::X25519, &server_hello.server_ephemeral),
        (),
        |shared| Ok(shared.to_vec()),
    )
    .map_err(|_| CryptoError::InvalidHandshake)?;

    let transport = derive_session_transport(Role::Client, mode, &shared, &transcript);
    Ok((transport, trusted))
}

fn validate_client_hello(hello: &ClientHello) -> Result<(), CryptoError> {
    if hello.version != PROTOCOL_VERSION {
        return Err(CryptoError::UnsupportedVersion(hello.version));
    }
    if hello.client_nonce.len() != NONCE_LEN
        || hello.client_ephemeral.len() != X25519_PUBLIC_KEY_LEN
        || hello.modes.is_empty()
    {
        return Err(CryptoError::InvalidHandshake);
    }
    Ok(())
}

fn validate_server_hello(hello: &ServerHello) -> Result<(), CryptoError> {
    if hello.version != PROTOCOL_VERSION {
        return Err(CryptoError::UnsupportedVersion(hello.version));
    }
    if TransportMode::from_wire_id(hello.mode).is_none()
        || hello.server_nonce.len() != NONCE_LEN
        || hello.server_ephemeral.len() != X25519_PUBLIC_KEY_LEN
        || hello.server_public_key.len() != ED25519_PUBLIC_KEY_LEN
        || hello.signature.is_empty()
    {
        return Err(CryptoError::InvalidHandshake);
    }
    Ok(())
}

/// Resolves the Ed25519 key the client will trust for this handshake.
///
/// With a pinned key, the presented key must match it exactly (defense against a
/// substituted server). Without one, the presented key is trusted on first use
/// and returned so the caller can store it.
fn resolve_server_public_key(
    server_hello: &ServerHello,
    pinned_server_public_key: Option<&[u8; ED25519_PUBLIC_KEY_LEN]>,
) -> Result<[u8; ED25519_PUBLIC_KEY_LEN], CryptoError> {
    let presented: [u8; ED25519_PUBLIC_KEY_LEN] = server_hello
        .server_public_key
        .as_slice()
        .try_into()
        .map_err(|_| CryptoError::InvalidHandshake)?;
    match pinned_server_public_key {
        Some(pinned) if pinned != &presented => Err(CryptoError::InvalidSignature),
        _ => Ok(presented),
    }
}

fn server_transcript(
    client_hello: &ClientHello,
    server_version: u16,
    mode: u8,
    server_nonce: &[u8],
    server_ephemeral: &[u8],
    server_public_key: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    out.extend_from_slice(b"chatt server hello v3");
    out.extend_from_slice(&client_hello.version.to_le_bytes());
    out.extend_from_slice(&server_version.to_le_bytes());
    out.push(mode);
    out.extend_from_slice(&client_hello.client_nonce);
    out.extend_from_slice(&client_hello.client_ephemeral);
    out.extend_from_slice(server_nonce);
    out.extend_from_slice(server_ephemeral);
    out.extend_from_slice(server_public_key);
    out
}

fn full_transcript(
    client_hello: &ClientHello,
    server_hello: &ServerHello,
    server_public_key: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    validate_client_hello(client_hello)?;
    validate_server_hello(server_hello)?;
    let mut out = server_transcript(
        client_hello,
        server_hello.version,
        server_hello.mode,
        &server_hello.server_nonce,
        &server_hello.server_ephemeral,
        server_public_key,
    );
    out.extend_from_slice(&server_hello.signature);
    Ok(digest::digest(&digest::SHA256, &out).as_ref().to_vec())
}

#[derive(Clone, Copy)]
enum Role {
    Client,
    Server,
}

/// Derives every session output from the handshake: the directional AEAD keys,
/// the shared UDP demux route id, and the auth keys used for external-link UDP
/// address claims and video connection setup.
///
/// The route id and auth keys are direction-agnostic — both ends derive the same
/// values — while the AEAD keys are mirrored per role like the control/media
/// send/recv pairs.
fn derive_session_transport(
    role: Role,
    mode: TransportMode,
    shared: &[u8],
    transcript_hash: &[u8],
) -> SessionTransport {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, transcript_hash);
    let prk = salt.extract(shared);

    let client_control = expand_key(&prk, b"chatt client control key v1");
    let server_control = expand_key(&prk, b"chatt server control key v1");
    let client_media = expand_key(&prk, b"chatt client media key v1");
    let server_media = expand_key(&prk, b"chatt server media key v1");
    let ids = expand_key(&prk, b"chatt transport key ids v1");

    let client_control_id = u32::from_le_bytes(ids[0..4].try_into().unwrap()).max(1);
    let server_control_id = u32::from_le_bytes(ids[4..8].try_into().unwrap()).max(1);
    let client_media_id = u32::from_le_bytes(ids[8..12].try_into().unwrap()).max(1);
    let server_media_id = u32::from_le_bytes(ids[12..16].try_into().unwrap()).max(1);

    let client_control = KeyMaterial {
        id: client_control_id,
        bytes: client_control,
    };
    let server_control = KeyMaterial {
        id: server_control_id,
        bytes: server_control,
    };
    let client_media = KeyMaterial {
        id: client_media_id,
        bytes: client_media,
    };
    let server_media = KeyMaterial {
        id: server_media_id,
        bytes: server_media,
    };

    let secrets = match role {
        Role::Client => SessionSecrets {
            control_send: client_control,
            control_recv: server_control,
            media_send: client_media,
            media_recv: server_media,
        },
        Role::Server => SessionSecrets {
            control_send: server_control,
            control_recv: client_control,
            media_send: server_media,
            media_recv: client_media,
        },
    };

    let route_material = expand_key(&prk, b"chatt media route id v1");
    let route_id = u32::from_le_bytes(route_material[0..4].try_into().unwrap()).max(1);
    let bind_key = expand_key(&prk, b"chatt media bind key v1");
    let video_auth_key = expand_key(&prk, b"chatt video auth key v1");

    SessionTransport {
        mode,
        secrets,
        route_id,
        bind_key,
        video_auth_key,
    }
}

struct HkdfLen(usize);

impl hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

pub(crate) fn expand_key(prk: &hkdf::Prk, label: &'static [u8]) -> [u8; KEY_LEN] {
    let info = [label];
    let okm = prk.expand(&info, HkdfLen(KEY_LEN)).unwrap();
    let mut out = [0u8; KEY_LEN];
    okm.fill(&mut out).unwrap();
    out
}

/// Keys derived from the random root capability embedded in a device-link
/// ticket. Only the derived redemption secret is disclosed to the server; the
/// root capability and enrollment key remain client-side.
pub struct DeviceLinkKeys {
    pub redemption_secret: [u8; KEY_LEN],
    pub enrollment_key: [u8; KEY_LEN],
}

/// Domain-separates the server redemption capability from the key protecting
/// the server-retained enrollment bundle. This deliberately reuses the same
/// HKDF-SHA256 extract/expand path as the live transport handshake above.
pub fn derive_device_link_keys(
    pairing_secret: &[u8],
    server_public_key: &[u8],
) -> Result<DeviceLinkKeys, CryptoError> {
    if pairing_secret.len() != KEY_LEN || server_public_key.len() != ED25519_PUBLIC_KEY_LEN {
        return Err(CryptoError::InvalidKey);
    }
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, server_public_key);
    let prk = salt.extract(pairing_secret);
    Ok(DeviceLinkKeys {
        redemption_secret: expand_key(&prk, b"chatt device-link redemption v3"),
        enrollment_key: expand_key(&prk, b"chatt device-link enrollment key v3"),
    })
}

/// Which side of a dedicated video connection is deriving keys.
///
/// The chatt client (whether it publishes a capture or views one) sends on the
/// "up" key and receives on the "down" key. The server is the mirror: it sends
/// on "down" and receives on "up". Both ends derive from the same per-stream
/// secret distributed over the encrypted control channel, so the secret never
/// appears on the video connection itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoKeyRole {
    Client,
    Server,
}

/// Derives the directional `(send, recv)` key pair for one end of a video
/// connection from a per-stream secret, mirroring [`derive_session_secrets`].
///
/// The publisher's secret and a viewer's secret are independent HKDF inputs, so
/// the same direction labels are reused for both connection kinds without
/// collision.
pub fn derive_video_keys(secret: &[u8; KEY_LEN], role: VideoKeyRole) -> (KeyMaterial, KeyMaterial) {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"chatt video keys v1");
    let prk = salt.extract(secret);
    let up = expand_key(&prk, b"chatt video up key v1");
    let down = expand_key(&prk, b"chatt video down key v1");
    let ids = expand_key(&prk, b"chatt video key ids v1");
    let up_id = u32::from_le_bytes(ids[0..4].try_into().unwrap()).max(1);
    let down_id = u32::from_le_bytes(ids[4..8].try_into().unwrap()).max(1);
    let up = KeyMaterial {
        id: up_id,
        bytes: up,
    };
    let down = KeyMaterial {
        id: down_id,
        bytes: down,
    };
    match role {
        VideoKeyRole::Client => (up, down),
        VideoKeyRole::Server => (down, up),
    }
}

#[derive(Debug)]
pub struct TransportCipher {
    send: KeyMaterial,
    recv: KeyMaterial,
    next_send_counter: u64,
    next_recv_counter: u64,
}

impl TransportCipher {
    pub fn new(send: KeyMaterial, recv: KeyMaterial) -> Self {
        Self {
            send,
            recv,
            next_send_counter: 0,
            next_recv_counter: 0,
        }
    }

    pub fn send_key_id(&self) -> u32 {
        self.send.id
    }

    pub fn recv_key_id(&self) -> u32 {
        self.recv.id
    }

    pub fn seal_next(&mut self, channel: u8, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut out = Vec::with_capacity(TRANSPORT_HEADER_LEN + plaintext.len() + TAG_LEN);
        self.seal_next_into(channel, plaintext, &mut out)?;
        Ok(out)
    }

    /// Seals `plaintext` directly onto the end of `out` — transport header,
    /// ciphertext, tag — skipping the frame allocation [`seal_next`] makes.
    /// The sealed frame occupies exactly [`TRANSPORT_HEADER_LEN`]` +
    /// plaintext.len() + `[`TAG_LEN`] bytes, so callers can write a length
    /// prefix before sealing. On error nothing is appended.
    ///
    /// [`seal_next`]: Self::seal_next
    pub fn seal_next_into(
        &mut self,
        channel: u8,
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        let counter = self.next_send_counter;
        if counter >= REJECT_AFTER_MESSAGES {
            return Err(CryptoError::CounterExhausted);
        }
        self.next_send_counter = self
            .next_send_counter
            .checked_add(1)
            .ok_or(CryptoError::CounterExhausted)?;
        seal_with_key_into(&self.send, channel, counter, plaintext, out)
    }

    pub fn open_next(&mut self, channel: u8, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (counter, plaintext) = open_with_key(&self.recv, channel, frame)?;
        self.check_recv_counter(counter)?;
        Ok(plaintext)
    }

    /// Opens the sealed frame in place, skipping the body copy [`open_next`]
    /// makes: afterwards the plaintext occupies the returned slice, which
    /// starts at [`TRANSPORT_HEADER_LEN`] within `frame`. Callers that hold
    /// the frame in a mutable receive buffer decrypt without allocating.
    ///
    /// [`open_next`]: Self::open_next
    pub fn open_next_in_place<'f>(
        &mut self,
        channel: u8,
        frame: &'f mut [u8],
    ) -> Result<&'f [u8], CryptoError> {
        let (counter, plaintext_len) = open_with_key_in_place(&self.recv, channel, frame)?;
        self.check_recv_counter(counter)?;
        Ok(&frame[TRANSPORT_HEADER_LEN..TRANSPORT_HEADER_LEN + plaintext_len])
    }

    fn check_recv_counter(&mut self, counter: u64) -> Result<(), CryptoError> {
        if counter != self.next_recv_counter {
            return Err(CryptoError::CounterMismatch);
        }
        self.next_recv_counter = self
            .next_recv_counter
            .checked_add(1)
            .ok_or(CryptoError::CounterExhausted)?;
        Ok(())
    }
}

/// Record-framing codec for the control and video lanes. In
/// [`Aead`](Self::Aead) mode records are ChaCha20-Poly1305 sealed by a
/// [`TransportCipher`]; in [`Clear`](Self::Clear) mode the outer secure link
/// protects the wire and record bytes pass through unchanged. The session's
/// [`TransportMode`] selects the variant; this is not a per-lane policy.
#[derive(Debug)]
pub enum RecordProtection {
    Aead(TransportCipher),
    Clear,
}

impl RecordProtection {
    pub fn aead(send: KeyMaterial, recv: KeyMaterial) -> Self {
        Self::Aead(TransportCipher::new(send, recv))
    }

    pub fn clear() -> Self {
        Self::Clear
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Aead(_))
    }

    pub fn send_key_id(&self) -> Option<u32> {
        match self {
            Self::Aead(cipher) => Some(cipher.send_key_id()),
            Self::Clear => None,
        }
    }

    pub fn recv_key_id(&self) -> Option<u32> {
        match self {
            Self::Aead(cipher) => Some(cipher.recv_key_id()),
            Self::Clear => None,
        }
    }

    pub fn seal_next(&mut self, channel: u8, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Aead(cipher) => cipher.seal_next(channel, plaintext),
            Self::Clear => Ok(plaintext.to_vec()),
        }
    }

    /// Number of bytes [`seal_next_into`](Self::seal_next_into) appends for a
    /// payload of `plaintext_len` bytes, for writing a length prefix first.
    pub fn sealed_len(&self, plaintext_len: usize) -> usize {
        match self {
            Self::Aead(_) => TRANSPORT_HEADER_LEN + plaintext_len + TAG_LEN,
            Self::Clear => plaintext_len,
        }
    }

    /// Seals `plaintext` directly onto the end of `out`, appending exactly
    /// [`sealed_len`](Self::sealed_len) bytes; on error nothing is appended.
    pub fn seal_next_into(
        &mut self,
        channel: u8,
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        match self {
            Self::Aead(cipher) => cipher.seal_next_into(channel, plaintext, out),
            Self::Clear => {
                out.extend_from_slice(plaintext);
                Ok(())
            }
        }
    }

    pub fn open_next(&mut self, channel: u8, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Aead(cipher) => cipher.open_next(channel, frame),
            Self::Clear => Ok(frame.to_vec()),
        }
    }

    /// Opens the sealed frame in place, returning the plaintext slice within
    /// `frame` without the copy [`open_next`](Self::open_next) makes.
    pub fn open_next_in_place<'f>(
        &mut self,
        channel: u8,
        frame: &'f mut [u8],
    ) -> Result<&'f [u8], CryptoError> {
        match self {
            Self::Aead(cipher) => cipher.open_next_in_place(channel, frame),
            Self::Clear => Ok(frame),
        }
    }
}

pub fn seal_with_key(
    key: &KeyMaterial,
    channel: u8,
    counter: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let mut out = Vec::with_capacity(TRANSPORT_HEADER_LEN + plaintext.len() + TAG_LEN);
    seal_with_key_into(key, channel, counter, plaintext, &mut out)?;
    Ok(out)
}

/// Seals a transport frame directly onto the end of `out`: header, ciphertext,
/// tag. On error `out` is left exactly as it was.
pub fn seal_with_key_into(
    key: &KeyMaterial,
    channel: u8,
    counter: u64,
    plaintext: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), CryptoError> {
    if counter >= REJECT_AFTER_MESSAGES {
        return Err(CryptoError::CounterExhausted);
    }
    let seal_key = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| CryptoError::InvalidKey)?,
    );
    let base = out.len();
    out.reserve(TRANSPORT_HEADER_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(&key.id.to_le_bytes());
    out.extend_from_slice(&counter.to_le_bytes());
    out.extend_from_slice(plaintext);
    let nonce = nonce_from_counter(counter);
    let aad = transport_aad(channel, &out[base..base + TRANSPORT_HEADER_LEN]);
    let tag = seal_key.seal_in_place_separate_tag(
        nonce,
        Aad::from(aad),
        &mut out[base + TRANSPORT_HEADER_LEN..],
    );
    let Ok(tag) = tag else {
        out.truncate(base);
        return Err(CryptoError::Cipher);
    };
    out.extend_from_slice(tag.as_ref());
    Ok(())
}

/// Encrypts `out[cipher_start..]` in place under `key`, authenticating `aad`,
/// and appends the 16-byte tag to `out`. Callers that have already written their
/// own framing (for example the media UDP header, which embeds the same key id
/// and counter the transport header would carry) seal directly into that buffer
/// instead of allocating a separate transport frame.
///
/// `aad` must match what the receiver reconstructs, and `out[cipher_start..]`
/// must already hold the plaintext.
pub fn seal_in_place_append_tag(
    key: &KeyMaterial,
    counter: u64,
    aad: &[u8],
    cipher_start: usize,
    out: &mut Vec<u8>,
) -> Result<(), CryptoError> {
    if counter >= REJECT_AFTER_MESSAGES {
        return Err(CryptoError::CounterExhausted);
    }
    let nonce = nonce_from_counter(counter);
    let seal_key = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| CryptoError::InvalidKey)?,
    );
    let tag = seal_key
        .seal_in_place_separate_tag(nonce, Aad::from(aad), &mut out[cipher_start..])
        .map_err(|_| CryptoError::Cipher)?;
    out.extend_from_slice(tag.as_ref());
    Ok(())
}

/// Opens `body` (ciphertext followed by the 16-byte tag) in place under
/// `key.bytes`, with the nonce derived from `counter` and `aad` authenticated,
/// returning the plaintext length. Unlike [`open_with_key`] this does not embed
/// or check a key id — the caller owns the framing and AAD, so it suits the UDP
/// media path where demux is by route id rather than key id.
pub fn open_in_place_with_aad(
    key: &KeyMaterial,
    counter: u64,
    aad: &[u8],
    body: &mut [u8],
) -> Result<usize, CryptoError> {
    if counter >= REJECT_AFTER_MESSAGES {
        return Err(CryptoError::CounterExhausted);
    }
    let nonce = nonce_from_counter(counter);
    let open_key = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| CryptoError::InvalidKey)?,
    );
    let plaintext = open_key
        .open_in_place(nonce, Aad::from(aad), body)
        .map_err(|_| CryptoError::Cipher)?;
    Ok(plaintext.len())
}

pub fn open_with_key(
    key: &KeyMaterial,
    channel: u8,
    frame: &[u8],
) -> Result<(u64, Vec<u8>), CryptoError> {
    if frame.len() < TRANSPORT_HEADER_LEN + TAG_LEN {
        return Err(CryptoError::Cipher);
    }
    let (header, body) = frame.split_at(TRANSPORT_HEADER_LEN);
    let mut body = body.to_vec();
    let (counter, plaintext_len) = open_body_in_place(key, channel, header, &mut body)?;
    body.truncate(plaintext_len);
    Ok((counter, body))
}

/// Opens a sealed transport frame in place: afterwards the plaintext occupies
/// `frame[TRANSPORT_HEADER_LEN..][..plaintext_len]`. Returns the frame counter
/// and the plaintext length.
pub fn open_with_key_in_place(
    key: &KeyMaterial,
    channel: u8,
    frame: &mut [u8],
) -> Result<(u64, usize), CryptoError> {
    if frame.len() < TRANSPORT_HEADER_LEN + TAG_LEN {
        return Err(CryptoError::Cipher);
    }
    let (header, body) = frame.split_at_mut(TRANSPORT_HEADER_LEN);
    open_body_in_place(key, channel, header, body)
}

/// Decrypts `body` (ciphertext plus tag) in place against the 12-byte
/// transport `header`, returning the counter and plaintext length.
fn open_body_in_place(
    key: &KeyMaterial,
    channel: u8,
    header: &[u8],
    body: &mut [u8],
) -> Result<(u64, usize), CryptoError> {
    let key_id = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if key_id != key.id {
        return Err(CryptoError::WrongKeyId);
    }
    let counter = u64::from_le_bytes(header[4..12].try_into().unwrap());
    if counter >= REJECT_AFTER_MESSAGES {
        return Err(CryptoError::CounterExhausted);
    }
    let nonce = nonce_from_counter(counter);
    let aad = transport_aad(channel, header);
    let open_key = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| CryptoError::InvalidKey)?,
    );
    let plaintext = open_key
        .open_in_place(nonce, Aad::from(aad), body)
        .map_err(|_| CryptoError::Cipher)?;
    Ok((counter, plaintext.len()))
}

/// Claims carried inside a dynamic-user bearer token. The server issues one at
/// pairing time and reads it back on every authentication instead of storing a
/// row per user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub struct DynamicTokenClaims {
    pub user_id: UserId,
    pub password_epoch: u32,
}

/// Derives the static ChaCha20-Poly1305 key that seals dynamic tokens from the
/// server identity seed. The seed already gates the Ed25519 identity, so the
/// same secret authenticates tokens without new key material to configure.
fn dynamic_token_key(seed_hex: &str) -> Result<KeyMaterial, CryptoError> {
    let seed = decode_fixed_hex::<KEY_LEN>(seed_hex)?;
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"chatt dynamic token salt v1");
    let prk = salt.extract(&seed);
    let bytes = expand_key(&prk, b"chatt dynamic token key v1");
    Ok(KeyMaterial { id: 0, bytes })
}

/// Seals `claims` into an opaque bearer token string. The AEAD tag authenticates
/// the token, so a client cannot forge or alter its `user_id` or `password_epoch`.
pub fn issue_dynamic_token(
    seed_hex: &str,
    claims: &DynamicTokenClaims,
) -> Result<String, CryptoError> {
    let key = dynamic_token_key(seed_hex)?;
    let plaintext = jsony::to_binary(claims);
    let mut counter_bytes = [0u8; 8];
    rand::SystemRandom::new()
        .fill(&mut counter_bytes)
        .map_err(|_| CryptoError::Random)?;
    // Keep the random nonce counter below REJECT_AFTER_MESSAGES by clearing the
    // top four bits.
    let counter = u64::from_le_bytes(counter_bytes) & 0x0fff_ffff_ffff_ffff;
    let frame = seal_with_key(&key, TOKEN_CHANNEL, counter, &plaintext)?;
    Ok(format!("{DYNAMIC_TOKEN_PREFIX}{}", encode_hex(&frame)))
}

/// Opens a dynamic token and returns its claims. Fails when the token was not
/// issued by this server seed or was tampered with.
pub fn verify_dynamic_token(
    seed_hex: &str,
    token: &str,
) -> Result<DynamicTokenClaims, CryptoError> {
    let hex = token
        .strip_prefix(DYNAMIC_TOKEN_PREFIX)
        .ok_or(CryptoError::InvalidEncoding)?;
    let frame = decode_hex(hex)?;
    let key = dynamic_token_key(seed_hex)?;
    let (_counter, plaintext) = open_with_key(&key, TOKEN_CHANNEL, &frame)?;
    jsony::from_binary(&plaintext).map_err(|_| CryptoError::InvalidEncoding)
}

/// Derives a stable dynamic user id from a client-generated recovery secret.
/// The server seed keys the derivation, so clients cannot target another known
/// user id. New derived ids occupy the upper half of the `u64` range.
pub fn dynamic_user_id_from_recovery_token(
    seed_hex: &str,
    token: &str,
) -> Result<UserId, CryptoError> {
    let secret = token
        .strip_prefix(OPEN_PAIR_RECOVERY_PREFIX)
        .ok_or(CryptoError::InvalidEncoding)?;
    let secret = decode_hex(secret)?;
    if secret.len() < KEY_LEN {
        return Err(CryptoError::InvalidEncoding);
    }
    dynamic_user_id_from_pairing_secret(seed_hex, &secret)
}

/// Derives a stable dynamic user id from client-held pairing material. This is
/// also used when an authentic but stale dynamic token must become a fresh
/// identity on a passwordless server.
pub fn dynamic_user_id_from_pairing_secret(
    seed_hex: &str,
    secret: &[u8],
) -> Result<UserId, CryptoError> {
    let seed = decode_fixed_hex::<KEY_LEN>(seed_hex)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, &seed);
    let mut input = b"chatt open-pair recovery id v1".to_vec();
    input.extend_from_slice(secret);
    let tag = hmac::sign(&key, &input);
    let mut id = u64::from_le_bytes(tag.as_ref()[..8].try_into().expect("8-byte id"));
    id |= 1 << 63;
    Ok(UserId(id))
}

/// Computes the STUN `MESSAGE-INTEGRITY-SHA256` value for a message prefix.
///
/// `key` is the shared per-pair STUN key and `message_prefix` is the STUN
/// message up to but excluding the integrity attribute, with the header length
/// already adjusted per RFC 8489 §14.6. The full 32-byte HMAC-SHA256 output is
/// returned.
pub fn stun_integrity(key: &[u8], message_prefix: &[u8]) -> [u8; 32] {
    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, key);
    let tag = hmac::sign(&signing_key, message_prefix);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// Verifies a STUN `MESSAGE-INTEGRITY-SHA256` value in constant time.
///
/// Returns `true` when `tag` is the HMAC-SHA256 of `message_prefix` under `key`.
pub fn stun_verify(key: &[u8], message_prefix: &[u8], tag: &[u8]) -> bool {
    let verify_key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::verify(&verify_key, message_prefix, tag).is_ok()
}

/// Length of a truncated HMAC-SHA256 authentication proof.
pub const AUTH_PROOF_LEN: usize = 16;

/// Computes a 16-byte HMAC-SHA256 proof over `message` under `key`, used to
/// authenticate external-link UDP address claims and video connection setup
/// without putting AEAD on payload bytes.
pub fn auth_proof(key: &[u8], message: &[u8]) -> [u8; AUTH_PROOF_LEN] {
    let full = stun_integrity(key, message);
    let mut out = [0u8; AUTH_PROOF_LEN];
    out.copy_from_slice(&full[..AUTH_PROOF_LEN]);
    out
}

/// Verifies a 16-byte [`auth_proof`] in constant time.
pub fn auth_proof_verify(key: &[u8], message: &[u8], tag: &[u8]) -> bool {
    if tag.len() != AUTH_PROOF_LEN {
        return false;
    }
    let expected = auth_proof(key, message);
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(tag.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Builds the additional-authenticated-data block for a transport frame: the
/// channel byte followed by the 12-byte transport header. Returned by value on
/// the stack so the per-frame seal/open paths do not allocate.
fn transport_aad(channel: u8, header: &[u8]) -> [u8; 1 + TRANSPORT_HEADER_LEN] {
    let mut aad = [0u8; 1 + TRANSPORT_HEADER_LEN];
    aad[0] = channel;
    aad[1..].copy_from_slice(header);
    aad
}

fn nonce_from_counter(counter: u64) -> Nonce {
    let mut nonce = [0u8; aead::NONCE_LEN];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::assume_unique_for_key(nonce)
}

#[cfg(target_pointer_width = "64")]
type ReplayWord = u64;

#[cfg(target_pointer_width = "32")]
type ReplayWord = u32;

const REPLAY_WORD_BITS: usize = std::mem::size_of::<ReplayWord>() * 8;
const REPLAY_BITLEN: usize = 2048;
const REPLAY_LEN: usize = REPLAY_BITLEN / REPLAY_WORD_BITS;
const REPLAY_INDEX_MASK: u64 = REPLAY_LEN as u64 - 1;
const REPLAY_LOC_MASK: u64 = (REPLAY_WORD_BITS - 1) as u64;
const REPLAY_WINDOW_SIZE: u64 = (REPLAY_BITLEN - REPLAY_WORD_BITS) as u64;

#[derive(Clone, Debug)]
pub struct AntiReplay {
    bitmap: [ReplayWord; REPLAY_LEN],
    last: u64,
}

impl Default for AntiReplay {
    fn default() -> Self {
        Self::new()
    }
}

impl AntiReplay {
    pub fn new() -> Self {
        Self {
            bitmap: [0; REPLAY_LEN],
            last: 0,
        }
    }

    pub fn check(&self, sequence: u64) -> bool {
        if sequence > self.last {
            return true;
        }
        if self.last - sequence > REPLAY_WINDOW_SIZE {
            return false;
        }
        let bit_location = sequence & REPLAY_LOC_MASK;
        let index = (sequence / REPLAY_WORD_BITS as u64) & REPLAY_INDEX_MASK;
        self.bitmap[index as usize] & ((1 as ReplayWord) << bit_location) == 0
    }

    pub fn update(&mut self, sequence: u64) -> bool {
        if !self.check(sequence) {
            return false;
        }
        let index = sequence / REPLAY_WORD_BITS as u64;
        if sequence > self.last {
            let current_index = self.last / REPLAY_WORD_BITS as u64;
            let diff = index - current_index;
            if diff >= REPLAY_LEN as u64 {
                self.bitmap = [0; REPLAY_LEN];
            } else {
                for i in 0..diff {
                    let real_index = (current_index + i + 1) & REPLAY_INDEX_MASK;
                    self.bitmap[real_index as usize] = 0;
                }
            }
            self.last = sequence;
        }
        let wrapped_index = index & REPLAY_INDEX_MASK;
        let bit_location = sequence & REPLAY_LOC_MASK;
        self.bitmap[wrapped_index as usize] |= (1 as ReplayWord) << bit_location;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_token_round_trips() {
        let seed = dev_server_seed_hex();
        let claims = DynamicTokenClaims {
            user_id: UserId(FIRST_DYNAMIC_USER_ID_TEST),
            password_epoch: 7,
        };
        let token = issue_dynamic_token(&seed, &claims).unwrap();
        assert!(token.starts_with(DYNAMIC_TOKEN_PREFIX));
        assert_eq!(verify_dynamic_token(&seed, &token).unwrap(), claims);
    }

    #[test]
    fn open_pair_recovery_token_derives_a_stable_server_specific_user_id() {
        let seed = dev_server_seed_hex();
        let other_seed = encode_hex(&[0x11u8; KEY_LEN]);
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}{}", "ab".repeat(KEY_LEN));

        let first = dynamic_user_id_from_recovery_token(&seed, &token).unwrap();
        let second = dynamic_user_id_from_recovery_token(&seed, &token).unwrap();
        let other = dynamic_user_id_from_recovery_token(&other_seed, &token).unwrap();

        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_ne!(first.0 & (1 << 63), 0);
    }

    #[test]
    fn device_link_keys_are_domain_separated_and_server_bound() {
        let root = [0x42; KEY_LEN];
        let first = derive_device_link_keys(&root, &[0x11; ED25519_PUBLIC_KEY_LEN]).unwrap();
        let repeated = derive_device_link_keys(&root, &[0x11; ED25519_PUBLIC_KEY_LEN]).unwrap();
        let other_server = derive_device_link_keys(&root, &[0x12; ED25519_PUBLIC_KEY_LEN]).unwrap();

        assert_ne!(first.redemption_secret, first.enrollment_key);
        assert_eq!(first.redemption_secret, repeated.redemption_secret);
        assert_eq!(first.enrollment_key, repeated.enrollment_key);
        assert_ne!(first.redemption_secret, other_server.redemption_secret);
        assert_ne!(first.enrollment_key, other_server.enrollment_key);
    }

    #[test]
    fn dynamic_token_rejects_wrong_seed() {
        let seed = dev_server_seed_hex();
        let other_seed = encode_hex(&[0x11u8; KEY_LEN]);
        let claims = DynamicTokenClaims {
            user_id: UserId(42),
            password_epoch: 0,
        };
        let token = issue_dynamic_token(&seed, &claims).unwrap();
        assert!(verify_dynamic_token(&other_seed, &token).is_err());
    }

    #[test]
    fn dynamic_token_rejects_tampering() {
        let seed = dev_server_seed_hex();
        let claims = DynamicTokenClaims {
            user_id: UserId(9),
            password_epoch: 1,
        };
        let token = issue_dynamic_token(&seed, &claims).unwrap();
        // Flip the last hex nibble of the sealed body.
        let mut bytes = token.into_bytes();
        let last = bytes.last_mut().unwrap();
        *last = if *last == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(verify_dynamic_token(&seed, &tampered).is_err());
    }

    const FIRST_DYNAMIC_USER_ID_TEST: u64 = u32::MAX as u64 + 1;

    #[test]
    fn client_and_server_derive_opposite_keys() {
        for mode in [
            TransportMode::NativeEncrypted,
            TransportMode::ExternalSecureLink,
        ] {
            let rng = rand::SystemRandom::new();
            let client = generate_client_hello(&rng).unwrap();
            let client_hello = client.hello.clone();
            let server =
                respond_to_client_hello(&rng, &dev_server_key_pair(), &client_hello, mode).unwrap();
            let (client_transport, trusted) = complete_client_transport_handshake(
                client,
                &server.hello,
                Some(&dev_server_public_key()),
            )
            .unwrap();

            assert_eq!(trusted, dev_server_public_key());
            assert_eq!(client_transport.mode, mode);
            assert_eq!(server.transport.mode, mode);
            let client_keys = &client_transport.secrets;
            assert_eq!(
                client_keys.control_send,
                server.transport.secrets.control_recv
            );
            assert_eq!(
                client_keys.control_recv,
                server.transport.secrets.control_send
            );
            assert_eq!(client_keys.media_send, server.transport.secrets.media_recv);
            assert_eq!(client_keys.media_recv, server.transport.secrets.media_send);
            // Route id and auth keys are direction-agnostic: identical on both ends.
            assert_eq!(client_transport.route_id, server.transport.route_id);
            assert_ne!(client_transport.route_id, 0);
            assert_eq!(client_transport.bind_key, server.transport.bind_key);
            assert_eq!(
                client_transport.video_auth_key,
                server.transport.video_auth_key
            );
        }
    }

    #[test]
    fn server_signature_covers_selected_mode() {
        // A ServerHello signed for one mode must not verify when its mode byte is
        // swapped to the other.
        let rng = rand::SystemRandom::new();
        let client = generate_client_hello(&rng).unwrap();
        let client_hello = client.hello.clone();
        let server = respond_to_client_hello(
            &rng,
            &dev_server_key_pair(),
            &client_hello,
            TransportMode::NativeEncrypted,
        )
        .unwrap();
        let mut tampered = server.hello.clone();
        tampered.mode = TransportMode::ExternalSecureLink.wire_id();
        assert!(
            complete_client_transport_handshake(client, &tampered, Some(&dev_server_public_key()))
                .is_err()
        );
    }

    #[test]
    fn server_rejects_unadvertised_mode() {
        let rng = rand::SystemRandom::new();
        let mut client = generate_client_hello(&rng).unwrap();
        client.hello.modes = vec![TransportMode::NativeEncrypted.wire_id()];
        assert!(
            respond_to_client_hello(
                &rng,
                &dev_server_key_pair(),
                &client.hello,
                TransportMode::ExternalSecureLink,
            )
            .is_err()
        );
    }

    #[test]
    fn video_keys_derive_opposite_on_each_end() {
        let secret = [7u8; KEY_LEN];
        let (client_send, client_recv) = derive_video_keys(&secret, VideoKeyRole::Client);
        let (server_send, server_recv) = derive_video_keys(&secret, VideoKeyRole::Server);
        assert_eq!(client_send, server_recv);
        assert_eq!(client_recv, server_send);

        // A frame sealed by the client opens with the server's recv key.
        let frame = seal_with_key(&client_send, CHANNEL_VIDEO, 0, b"frame").unwrap();
        let (counter, plaintext) = open_with_key(&server_recv, CHANNEL_VIDEO, &frame).unwrap();
        assert_eq!(counter, 0);
        assert_eq!(plaintext, b"frame");
    }

    #[test]
    fn external_link_handshake_is_signed_and_derives_material() {
        let rng = rand::SystemRandom::new();
        let client = generate_client_hello(&rng).unwrap();
        let client_hello = client.hello.clone();
        let server = respond_to_client_hello(
            &rng,
            &dev_server_key_pair(),
            &client_hello,
            TransportMode::ExternalSecureLink,
        )
        .unwrap();

        assert_eq!(
            server.hello.mode,
            TransportMode::ExternalSecureLink.wire_id()
        );
        // The external-link server still runs the signed ephemeral handshake.
        assert_eq!(server.hello.server_ephemeral.len(), X25519_PUBLIC_KEY_LEN);
        let (transport, trusted) = complete_client_transport_handshake(
            client,
            &server.hello,
            Some(&dev_server_public_key()),
        )
        .unwrap();
        assert_eq!(transport.mode, TransportMode::ExternalSecureLink);
        assert_eq!(trusted, dev_server_public_key());
    }

    #[test]
    fn transport_cipher_round_trips_and_rejects_tamper() {
        let key_a = KeyMaterial {
            id: 11,
            bytes: [1; KEY_LEN],
        };
        let key_b = KeyMaterial {
            id: 22,
            bytes: [2; KEY_LEN],
        };
        let mut a = TransportCipher::new(key_a.clone(), key_b.clone());
        let mut b = TransportCipher::new(key_b, key_a);
        let frame = a.seal_next(CHANNEL_CONTROL, b"hello").unwrap();
        assert_eq!(b.open_next(CHANNEL_CONTROL, &frame).unwrap(), b"hello");

        let mut tampered = frame;
        let last = tampered.len() - 1;
        tampered[last] ^= 0x55;
        assert!(b.open_next(CHANNEL_CONTROL, &tampered).is_err());
    }

    #[test]
    fn clear_record_protection_passes_payloads_without_framing() {
        let mut record = RecordProtection::clear();
        let frame = record.seal_next(CHANNEL_CONTROL, b"hello").unwrap();
        assert_eq!(frame, b"hello");
        assert_eq!(record.open_next(CHANNEL_CONTROL, &frame).unwrap(), b"hello");
    }

    #[test]
    fn auth_proof_round_trips_and_rejects_tamper() {
        let key = [9u8; KEY_LEN];
        let proof = auth_proof(&key, b"claim");
        assert!(auth_proof_verify(&key, b"claim", &proof));
        assert!(!auth_proof_verify(&key, b"other", &proof));
        let mut tampered = proof;
        tampered[0] ^= 0x01;
        assert!(!auth_proof_verify(&key, b"claim", &tampered));
    }

    #[test]
    fn anti_replay_tracks_window() {
        let mut replay = AntiReplay::new();
        for i in 0..20_000 {
            assert!(replay.update(i));
        }
        for i in (0..20_000).rev() {
            assert!(!replay.check(i));
        }
        assert!(replay.update(65_536));
        for i in (65_536 - REPLAY_WINDOW_SIZE)..65_535 {
            assert!(replay.update(i));
        }
        for i in (65_536 - 10 * REPLAY_WINDOW_SIZE)..65_535 {
            assert!(!replay.check(i));
        }
    }

    #[test]
    fn stun_integrity_matches_known_vector() {
        // RFC 4231 test case 1: key = 20 bytes of 0x0b, data = "Hi There".
        let key = [0x0bu8; 20];
        let expected = "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7";
        assert_eq!(encode_hex(&stun_integrity(&key, b"Hi There")), expected);
    }

    #[test]
    fn hex_key_helpers_round_trip() {
        let encoded = dev_server_seed_hex();
        let pair = server_key_pair_from_seed_hex(&encoded).unwrap();
        assert_eq!(pair.public_key().as_ref(), dev_server_public_key());
        assert_eq!(
            ed25519_public_key_from_hex(&encode_hex(&dev_server_public_key())).unwrap(),
            dev_server_public_key()
        );
    }
}
