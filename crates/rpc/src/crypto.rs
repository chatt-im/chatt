use ring::{
    aead::{self, Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey},
    agreement, digest, hkdf, hmac, rand,
    signature::{self, KeyPair},
};

use crate::PROTOCOL_VERSION;
use crate::control::{ClientHello, ServerHello};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 32;
pub const X25519_PUBLIC_KEY_LEN: usize = 32;
pub const ED25519_PUBLIC_KEY_LEN: usize = 32;
pub const TAG_LEN: usize = 16;
pub const TRANSPORT_HEADER_LEN: usize = 12;
pub const CHANNEL_CONTROL: u8 = 1;
pub const CHANNEL_MEDIA: u8 = 2;
pub const REKEY_AFTER_TIME_SECS: u64 = 120;
pub const REJECT_AFTER_TIME_SECS: u64 = 180;
pub const REJECT_AFTER_MESSAGES: u64 = u64::MAX - (1 << 4);

const DEV_SERVER_SEED: [u8; KEY_LEN] = [
    0x54, 0x6f, 0x6d, 0x63, 0x68, 0x61, 0x74, 0x20, 0x64, 0x65, 0x76, 0x20, 0x73, 0x65, 0x72, 0x76,
    0x65, 0x72, 0x20, 0x6b, 0x65, 0x79, 0x20, 0x76, 0x31, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

#[derive(Debug)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandshakeMode {
    Encrypted(SessionSecrets),
    Plaintext,
}

pub struct ClientHandshake {
    private: agreement::EphemeralPrivateKey,
    pub hello: ClientHello,
}

pub struct ServerHandshake {
    pub hello: ServerHello,
    pub secrets: SessionSecrets,
}

pub struct PlaintextServerHandshake {
    pub hello: ServerHello,
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
            client_nonce: nonce.to_vec(),
            client_ephemeral: public.as_ref().to_vec(),
        },
    })
}

pub fn respond_to_client_hello(
    rng: &dyn rand::SecureRandom,
    server_key_pair: &signature::Ed25519KeyPair,
    client_hello: &ClientHello,
) -> Result<ServerHandshake, CryptoError> {
    validate_client_hello(client_hello)?;
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
        |shared| shared.to_vec(),
    )
    .map_err(|_| CryptoError::InvalidHandshake)?;

    let unsigned = server_transcript(
        client_hello,
        PROTOCOL_VERSION,
        true,
        &nonce,
        public.as_ref(),
        server_key_pair.public_key().as_ref(),
    );
    let signature = server_key_pair.sign(&unsigned);
    let hello = ServerHello {
        version: PROTOCOL_VERSION,
        encrypted: true,
        server_nonce: nonce.to_vec(),
        server_ephemeral: public.as_ref().to_vec(),
        signature: signature.as_ref().to_vec(),
    };
    let transcript = full_transcript(client_hello, &hello, server_key_pair.public_key().as_ref())?;
    let secrets = derive_session_secrets(Role::Server, &shared, &transcript);
    Ok(ServerHandshake { hello, secrets })
}

pub fn respond_to_client_hello_plaintext(
    rng: &dyn rand::SecureRandom,
    server_key_pair: &signature::Ed25519KeyPair,
    client_hello: &ClientHello,
) -> Result<PlaintextServerHandshake, CryptoError> {
    validate_client_hello(client_hello)?;
    let mut nonce = [0u8; NONCE_LEN];
    rng.fill(&mut nonce).map_err(|_| CryptoError::Random)?;
    let unsigned = server_transcript(
        client_hello,
        PROTOCOL_VERSION,
        false,
        &nonce,
        &[],
        server_key_pair.public_key().as_ref(),
    );
    let signature = server_key_pair.sign(&unsigned);
    Ok(PlaintextServerHandshake {
        hello: ServerHello {
            version: PROTOCOL_VERSION,
            encrypted: false,
            server_nonce: nonce.to_vec(),
            server_ephemeral: Vec::new(),
            signature: signature.as_ref().to_vec(),
        },
    })
}

pub fn complete_client_transport_handshake(
    handshake: ClientHandshake,
    server_hello: &ServerHello,
    pinned_server_public_key: &[u8; ED25519_PUBLIC_KEY_LEN],
) -> Result<HandshakeMode, CryptoError> {
    if server_hello.encrypted {
        complete_client_handshake(handshake, server_hello, pinned_server_public_key)
            .map(HandshakeMode::Encrypted)
    } else {
        verify_plaintext_server_hello(&handshake.hello, server_hello, pinned_server_public_key)?;
        Ok(HandshakeMode::Plaintext)
    }
}

pub fn complete_client_handshake(
    handshake: ClientHandshake,
    server_hello: &ServerHello,
    pinned_server_public_key: &[u8; ED25519_PUBLIC_KEY_LEN],
) -> Result<SessionSecrets, CryptoError> {
    validate_server_hello(server_hello)?;
    let transcript = full_transcript(&handshake.hello, server_hello, pinned_server_public_key)?;
    signature::UnparsedPublicKey::new(&signature::ED25519, pinned_server_public_key)
        .verify(
            &server_transcript(
                &handshake.hello,
                server_hello.version,
                true,
                &server_hello.server_nonce,
                &server_hello.server_ephemeral,
                pinned_server_public_key,
            ),
            &server_hello.signature,
        )
        .map_err(|_| CryptoError::InvalidSignature)?;

    let shared = agreement::agree_ephemeral(
        handshake.private,
        &agreement::UnparsedPublicKey::new(&agreement::X25519, &server_hello.server_ephemeral),
        |shared| shared.to_vec(),
    )
    .map_err(|_| CryptoError::InvalidHandshake)?;

    Ok(derive_session_secrets(Role::Client, &shared, &transcript))
}

fn verify_plaintext_server_hello(
    client_hello: &ClientHello,
    server_hello: &ServerHello,
    pinned_server_public_key: &[u8; ED25519_PUBLIC_KEY_LEN],
) -> Result<(), CryptoError> {
    validate_client_hello(client_hello)?;
    validate_plaintext_server_hello(server_hello)?;
    signature::UnparsedPublicKey::new(&signature::ED25519, pinned_server_public_key)
        .verify(
            &server_transcript(
                client_hello,
                server_hello.version,
                false,
                &server_hello.server_nonce,
                &server_hello.server_ephemeral,
                pinned_server_public_key,
            ),
            &server_hello.signature,
        )
        .map_err(|_| CryptoError::InvalidSignature)
}

fn validate_client_hello(hello: &ClientHello) -> Result<(), CryptoError> {
    if hello.version != PROTOCOL_VERSION {
        return Err(CryptoError::UnsupportedVersion(hello.version));
    }
    if hello.client_nonce.len() != NONCE_LEN
        || hello.client_ephemeral.len() != X25519_PUBLIC_KEY_LEN
    {
        return Err(CryptoError::InvalidHandshake);
    }
    Ok(())
}

fn validate_server_hello(hello: &ServerHello) -> Result<(), CryptoError> {
    if hello.version != PROTOCOL_VERSION {
        return Err(CryptoError::UnsupportedVersion(hello.version));
    }
    if !hello.encrypted
        || hello.server_nonce.len() != NONCE_LEN
        || hello.server_ephemeral.len() != X25519_PUBLIC_KEY_LEN
        || hello.signature.is_empty()
    {
        return Err(CryptoError::InvalidHandshake);
    }
    Ok(())
}

fn validate_plaintext_server_hello(hello: &ServerHello) -> Result<(), CryptoError> {
    if hello.version != PROTOCOL_VERSION {
        return Err(CryptoError::UnsupportedVersion(hello.version));
    }
    if hello.encrypted
        || hello.server_nonce.len() != NONCE_LEN
        || !hello.server_ephemeral.is_empty()
        || hello.signature.is_empty()
    {
        return Err(CryptoError::InvalidHandshake);
    }
    Ok(())
}

fn server_transcript(
    client_hello: &ClientHello,
    server_version: u16,
    encrypted: bool,
    server_nonce: &[u8],
    server_ephemeral: &[u8],
    server_public_key: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    out.extend_from_slice(b"chatt server hello v2");
    out.extend_from_slice(&client_hello.version.to_le_bytes());
    out.extend_from_slice(&server_version.to_le_bytes());
    out.push(u8::from(encrypted));
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
        server_hello.encrypted,
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

fn derive_session_secrets(role: Role, shared: &[u8], transcript_hash: &[u8]) -> SessionSecrets {
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

    match role {
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
    }
}

struct HkdfLen(usize);

impl hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand_key(prk: &hkdf::Prk, label: &'static [u8]) -> [u8; KEY_LEN] {
    let info = [label];
    let okm = prk.expand(&info, HkdfLen(KEY_LEN)).unwrap();
    let mut out = [0u8; KEY_LEN];
    okm.fill(&mut out).unwrap();
    out
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
        let counter = self.next_send_counter;
        if counter >= REJECT_AFTER_MESSAGES {
            return Err(CryptoError::CounterExhausted);
        }
        self.next_send_counter = self
            .next_send_counter
            .checked_add(1)
            .ok_or(CryptoError::CounterExhausted)?;
        seal_with_key(&self.send, channel, counter, plaintext)
    }

    pub fn open_next(&mut self, channel: u8, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (counter, plaintext) = open_with_key(&self.recv, channel, frame)?;
        if counter != self.next_recv_counter {
            return Err(CryptoError::CounterMismatch);
        }
        self.next_recv_counter = self
            .next_recv_counter
            .checked_add(1)
            .ok_or(CryptoError::CounterExhausted)?;
        Ok(plaintext)
    }
}

#[derive(Debug)]
pub enum ControlTransport {
    Encrypted(TransportCipher),
    Plaintext,
}

impl ControlTransport {
    pub fn encrypted(send: KeyMaterial, recv: KeyMaterial) -> Self {
        Self::Encrypted(TransportCipher::new(send, recv))
    }

    pub fn plaintext() -> Self {
        Self::Plaintext
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted(_))
    }

    pub fn send_key_id(&self) -> Option<u32> {
        match self {
            Self::Encrypted(cipher) => Some(cipher.send_key_id()),
            Self::Plaintext => None,
        }
    }

    pub fn recv_key_id(&self) -> Option<u32> {
        match self {
            Self::Encrypted(cipher) => Some(cipher.recv_key_id()),
            Self::Plaintext => None,
        }
    }

    pub fn seal_next(&mut self, channel: u8, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Encrypted(cipher) => cipher.seal_next(channel, plaintext),
            Self::Plaintext => Ok(plaintext.to_vec()),
        }
    }

    pub fn open_next(&mut self, channel: u8, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match self {
            Self::Encrypted(cipher) => cipher.open_next(channel, frame),
            Self::Plaintext => Ok(frame.to_vec()),
        }
    }
}

pub fn seal_with_key(
    key: &KeyMaterial,
    channel: u8,
    counter: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if counter >= REJECT_AFTER_MESSAGES {
        return Err(CryptoError::CounterExhausted);
    }
    let mut out = Vec::with_capacity(TRANSPORT_HEADER_LEN + plaintext.len() + TAG_LEN);
    out.extend_from_slice(&key.id.to_le_bytes());
    out.extend_from_slice(&counter.to_le_bytes());
    out.extend_from_slice(plaintext);
    let nonce = nonce_from_counter(counter);
    let aad = transport_aad(channel, &out[..TRANSPORT_HEADER_LEN]);
    let seal_key = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| CryptoError::InvalidKey)?,
    );
    let tag = seal_key
        .seal_in_place_separate_tag(nonce, Aad::from(aad), &mut out[TRANSPORT_HEADER_LEN..])
        .map_err(|_| CryptoError::Cipher)?;
    out.extend_from_slice(tag.as_ref());
    Ok(out)
}

pub fn open_with_key(
    key: &KeyMaterial,
    channel: u8,
    frame: &[u8],
) -> Result<(u64, Vec<u8>), CryptoError> {
    if frame.len() < TRANSPORT_HEADER_LEN + TAG_LEN {
        return Err(CryptoError::Cipher);
    }
    let key_id = u32::from_le_bytes(frame[0..4].try_into().unwrap());
    if key_id != key.id {
        return Err(CryptoError::WrongKeyId);
    }
    let counter = u64::from_le_bytes(frame[4..12].try_into().unwrap());
    if counter >= REJECT_AFTER_MESSAGES {
        return Err(CryptoError::CounterExhausted);
    }
    let nonce = nonce_from_counter(counter);
    let aad = transport_aad(channel, &frame[..TRANSPORT_HEADER_LEN]);
    let open_key = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| CryptoError::InvalidKey)?,
    );
    let mut body = frame[TRANSPORT_HEADER_LEN..].to_vec();
    let plaintext = open_key
        .open_in_place(nonce, Aad::from(aad), &mut body)
        .map_err(|_| CryptoError::Cipher)?;
    Ok((counter, plaintext.to_vec()))
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

fn transport_aad(channel: u8, header: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(1 + header.len());
    aad.push(channel);
    aad.extend_from_slice(header);
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
    fn client_and_server_derive_opposite_keys() {
        let rng = rand::SystemRandom::new();
        let client = generate_client_hello(&rng).unwrap();
        let client_hello = client.hello.clone();
        let server = respond_to_client_hello(&rng, &dev_server_key_pair(), &client_hello).unwrap();
        let client_keys =
            complete_client_handshake(client, &server.hello, &dev_server_public_key()).unwrap();

        assert_eq!(client_keys.control_send, server.secrets.control_recv);
        assert_eq!(client_keys.control_recv, server.secrets.control_send);
        assert_eq!(client_keys.media_send, server.secrets.media_recv);
        assert_eq!(client_keys.media_recv, server.secrets.media_send);
    }

    #[test]
    fn server_can_select_signed_plaintext_transport() {
        let rng = rand::SystemRandom::new();
        let client = generate_client_hello(&rng).unwrap();
        let client_hello = client.hello.clone();
        let server =
            respond_to_client_hello_plaintext(&rng, &dev_server_key_pair(), &client_hello).unwrap();

        assert!(!server.hello.encrypted);
        assert_eq!(
            complete_client_transport_handshake(client, &server.hello, &dev_server_public_key())
                .unwrap(),
            HandshakeMode::Plaintext
        );
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
    fn plaintext_control_transport_passes_payloads_without_framing() {
        let mut transport = ControlTransport::plaintext();
        let frame = transport.seal_next(CHANNEL_CONTROL, b"hello").unwrap();
        assert_eq!(frame, b"hello");
        assert_eq!(
            transport.open_next(CHANNEL_CONTROL, &frame).unwrap(),
            b"hello"
        );
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
