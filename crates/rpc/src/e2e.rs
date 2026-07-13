//! End-to-end encryption for DM rooms.
//!
//! Both DM members hold a long-term X25519 identity key. A static-static
//! Diffie-Hellman between the two identities yields one directional key pair
//! per DM, cached for the life of the process. Each message is sealed with a
//! one-shot ChaCha20-Poly1305 key derived from the directional key and a fresh
//! random salt, so no counter state survives restarts and nonce reuse is
//! impossible by construction. The server relays [`DmEnvelope`] bytes opaquely;
//! it can see who talks to whom and padded sizes, never content.
//!
//! There is deliberately no ratchet: keys are static, so server-fetched
//! history remains decryptable and the whole scheme stays a page of code.
//! Compromise of an identity seed therefore exposes all past and future DM
//! traffic for its owner.

use jsony::Jsony;
use ring::{digest, hkdf, rand::SecureRandom};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::control::FileContentEncoding;
use crate::crypto::{
    CryptoError, KEY_LEN, KeyMaterial, TAG_LEN, expand_key, open_in_place_with_aad,
    seal_in_place_append_tag,
};
use crate::ids::{RoomId, UserId};

pub const E2E_PUBLIC_KEY_LEN: usize = 32;
pub const E2E_SEED_LEN: usize = 32;
pub const DM_SALT_LEN: usize = 32;
/// Chat envelope plaintexts are zero-padded to the next multiple of this, so
/// the server only learns a coarse length class (Signal pads the same way).
pub const DM_PAD_MULTIPLE: usize = 160;
/// Upper bound for an encoded [`DmEnvelope`]: an 8 KiB body plus encoding,
/// padding, salt, and tag, with generous margin.
pub const MAX_DM_ENVELOPE_BYTES: usize = 16 * 1024;
/// Bytes a sealed file chunk adds over its payload: length prefix plus tag.
pub const DM_CHUNK_OVERHEAD: usize = 4 + TAG_LEN;
const DM_ENVELOPE_VERSION: u8 = 1;
const DM_CHUNK_AAD_LABEL: &[u8; 21] = b"chatt e2e dm chunk v1";

/// Returns the X25519 public key for an identity seed.
pub fn e2e_public_key(seed: &[u8; E2E_SEED_LEN]) -> [u8; E2E_PUBLIC_KEY_LEN] {
    let secret = StaticSecret::from(*seed);
    PublicKey::from(&secret).to_bytes()
}

/// Directional sealing keys for one DM, mirrored between the two members: one
/// end's `send` equals the other end's `recv`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmPairKeys {
    pub send: KeyMaterial,
    pub recv: KeyMaterial,
}

/// Derives the [`DmPairKeys`] for a DM from our identity seed and the peer's
/// public key.
///
/// The HKDF transcript binds both user ids and both public keys in id order,
/// so the derivation is symmetric and a key can never be confused across user
/// pairs. Low-order peer keys are rejected before any material is derived.
///
/// # Errors
///
/// [`CryptoError::InvalidKey`] when the peer key is low-order (the shared
/// secret would not depend on our seed) or the two user ids are equal.
pub fn dm_pair_keys(
    my_seed: &[u8; E2E_SEED_LEN],
    my_id: UserId,
    peer_public: &[u8; E2E_PUBLIC_KEY_LEN],
    peer_id: UserId,
) -> Result<DmPairKeys, CryptoError> {
    if my_id == peer_id {
        return Err(CryptoError::InvalidKey);
    }
    let secret = StaticSecret::from(*my_seed);
    let my_public = PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&PublicKey::from(*peer_public));
    if !shared.was_contributory() {
        return Err(CryptoError::InvalidKey);
    }
    let ((low_id, low_public), (high_id, high_public)) = if my_id < peer_id {
        ((my_id, my_public), (peer_id, *peer_public))
    } else {
        ((peer_id, *peer_public), (my_id, my_public))
    };
    let mut transcript = Vec::with_capacity(15 + 2 * (8 + E2E_PUBLIC_KEY_LEN));
    transcript.extend_from_slice(b"chatt e2e dm v1");
    transcript.extend_from_slice(&low_id.0.to_le_bytes());
    transcript.extend_from_slice(&low_public);
    transcript.extend_from_slice(&high_id.0.to_le_bytes());
    transcript.extend_from_slice(&high_public);
    let transcript = digest::digest(&digest::SHA256, &transcript);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, transcript.as_ref()).extract(shared.as_bytes());
    let low_high = KeyMaterial {
        id: 1,
        bytes: expand_key(&prk, b"chatt e2e dm low-high key v1"),
    };
    let high_low = KeyMaterial {
        id: 1,
        bytes: expand_key(&prk, b"chatt e2e dm high-low key v1"),
    };
    if my_id < peer_id {
        Ok(DmPairKeys {
            send: low_high,
            recv: high_low,
        })
    } else {
        Ok(DmPairKeys {
            send: high_low,
            recv: low_high,
        })
    }
}

/// The sealed unit relayed in place of DM message text: a fresh salt and the
/// AEAD output over the padded [`DmPlaintext`].
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DmEnvelope {
    pub salt: Vec<u8>,
    pub sealed: Vec<u8>,
}

/// What a DM envelope protects: the content plus the sender's wall clock, kept
/// inside the ciphertext so a relaying server rewriting `timestamp_ms` is
/// detectable.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DmPlaintext {
    pub sent_at_ms: u64,
    pub content: DmContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DmContent {
    Text { body: String },
    Edit { body: String },
    FileAnnounce { file: DmFileMeta },
}

/// The real metadata of a sealed file transfer, hidden from the server. The
/// wire-visible transfer carries a placeholder name and a padded size; this
/// struct restores the truth on the receiving client and hands over the
/// symmetric key its chunks are sealed with.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DmFileMeta {
    pub original_name: String,
    /// True decompressed byte length.
    pub size: u64,
    /// Encoding of the sealed chunk payloads.
    pub encoding: FileContentEncoding,
    /// Per-transfer chunk sealing key, [`KEY_LEN`] bytes.
    pub content_key: Vec<u8>,
}

/// The envelope class bound into the AAD, so the server cannot re-deliver an
/// edit as a fresh message or a file announcement as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmContentKind {
    Text,
    Edit,
    FileAnnounce,
}

impl DmContentKind {
    const fn wire_id(self) -> u8 {
        match self {
            DmContentKind::Text => 0,
            DmContentKind::Edit => 1,
            DmContentKind::FileAnnounce => 2,
        }
    }
}

impl DmContent {
    pub fn kind(&self) -> DmContentKind {
        match self {
            DmContent::Text { .. } => DmContentKind::Text,
            DmContent::Edit { .. } => DmContentKind::Edit,
            DmContent::FileAnnounce { .. } => DmContentKind::FileAnnounce,
        }
    }
}

fn dm_aad(kind: DmContentKind, room_id: RoomId, sender: UserId) -> [u8; 14] {
    let mut aad = [0u8; 14];
    aad[0] = DM_ENVELOPE_VERSION;
    aad[1] = kind.wire_id();
    aad[2..6].copy_from_slice(&room_id.0.to_le_bytes());
    aad[6..14].copy_from_slice(&sender.0.to_le_bytes());
    aad
}

fn dm_message_key(direction_key: &KeyMaterial, salt: &[u8]) -> KeyMaterial {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&direction_key.bytes);
    KeyMaterial {
        id: 1,
        bytes: expand_key(&prk, b"chatt e2e dm message key v1"),
    }
}

fn padded_envelope_len(inner_len: usize) -> usize {
    (4 + inner_len).next_multiple_of(DM_PAD_MULTIPLE)
}

/// Seals `plaintext` for the DM `(room_id, sender)` context, returning encoded
/// [`DmEnvelope`] bytes ready for the wire.
///
/// Every call draws a fresh salt and derives a one-shot message key from it,
/// so sealing is stateless: no counters, no reuse hazard across restarts or
/// concurrent sessions holding the same identity. The plaintext is
/// length-prefixed and zero-padded to [`DM_PAD_MULTIPLE`].
///
/// # Errors
///
/// [`CryptoError::Random`] when salt generation fails, [`CryptoError::Cipher`]
/// when sealing fails.
pub fn seal_dm_envelope(
    keys: &DmPairKeys,
    room_id: RoomId,
    sender: UserId,
    plaintext: &DmPlaintext,
    rng: &dyn SecureRandom,
) -> Result<Vec<u8>, CryptoError> {
    let mut salt = [0u8; DM_SALT_LEN];
    rng.fill(&mut salt).map_err(|_| CryptoError::Random)?;
    let inner = jsony::to_binary(plaintext);
    let padded_len = padded_envelope_len(inner.len());
    let mut sealed = Vec::with_capacity(padded_len + TAG_LEN);
    sealed.extend_from_slice(&(inner.len() as u32).to_le_bytes());
    sealed.extend_from_slice(&inner);
    sealed.resize(padded_len, 0);
    let key = dm_message_key(&keys.send, &salt);
    let aad = dm_aad(plaintext.content.kind(), room_id, sender);
    seal_in_place_append_tag(&key, 0, &aad, 0, &mut sealed)?;
    let envelope = DmEnvelope {
        salt: salt.to_vec(),
        sealed,
    };
    Ok(jsony::to_binary(&envelope))
}

/// Opens encoded [`DmEnvelope`] bytes sealed by the peer for the
/// `(room_id, sender, kind)` context.
///
/// `kind` is what the outer message metadata claims the envelope to be (an
/// edit record, a file announcement, plain text); authentication fails if the
/// sealed class disagrees, and the decoded content is checked to match too.
///
/// # Errors
///
/// [`CryptoError::Cipher`] when authentication fails (wrong context, tampering,
/// or class mismatch), [`CryptoError::InvalidEncoding`] when the envelope or
/// its padding framing does not decode.
pub fn open_dm_envelope(
    keys: &DmPairKeys,
    room_id: RoomId,
    sender: UserId,
    kind: DmContentKind,
    envelope: &[u8],
) -> Result<DmPlaintext, CryptoError> {
    if envelope.len() > MAX_DM_ENVELOPE_BYTES {
        return Err(CryptoError::InvalidEncoding);
    }
    let Ok(envelope) = jsony::from_binary::<DmEnvelope>(envelope) else {
        return Err(CryptoError::InvalidEncoding);
    };
    let DmEnvelope { salt, mut sealed } = envelope;
    if salt.len() != DM_SALT_LEN {
        return Err(CryptoError::InvalidEncoding);
    }
    let key = dm_message_key(&keys.recv, &salt);
    let aad = dm_aad(kind, room_id, sender);
    let padded_len = open_in_place_with_aad(&key, 0, &aad, &mut sealed)?;
    let padded = &sealed[..padded_len];
    let Some(prefix) = padded.first_chunk::<4>() else {
        return Err(CryptoError::InvalidEncoding);
    };
    let inner_len = u32::from_le_bytes(*prefix) as usize;
    let Some(inner) = padded[4..].get(..inner_len) else {
        return Err(CryptoError::InvalidEncoding);
    };
    let Ok(plaintext) = jsony::from_binary::<DmPlaintext>(inner) else {
        return Err(CryptoError::InvalidEncoding);
    };
    if plaintext.content.kind().wire_id() != kind.wire_id() {
        return Err(CryptoError::Cipher);
    }
    Ok(plaintext)
}

/// Padmé padded length for a blob of `len` bytes (the PURB padding function):
/// the next length whose low `⌊log₂ len⌋ − ⌊log₂⌊log₂ len⌋⌋ − 1` bits are
/// zero. Overhead is at most ~12% and shrinks as `len` grows, while leaking
/// only O(log log len) bits of the true size.
pub fn padme_len(len: u64) -> u64 {
    if len < 2 {
        return len;
    }
    let e = 63 - len.leading_zeros() as u64;
    let s = 64 - e.leading_zeros() as u64;
    let mask = (1u64 << (e - s)) - 1;
    (len + mask) & !mask
}

fn dm_chunk_aad(room_id: RoomId, sender: UserId) -> [u8; 33] {
    let mut aad = [0u8; 33];
    aad[0..21].copy_from_slice(DM_CHUNK_AAD_LABEL);
    aad[21..25].copy_from_slice(&room_id.0.to_le_bytes());
    aad[25..33].copy_from_slice(&sender.0.to_le_bytes());
    aad
}

fn dm_content_key(content_key: &[u8]) -> Result<KeyMaterial, CryptoError> {
    let Ok(bytes) = <[u8; KEY_LEN]>::try_from(content_key) else {
        return Err(CryptoError::InvalidKey);
    };
    Ok(KeyMaterial { id: 1, bytes })
}

/// Seals one file-transfer chunk under the per-transfer content key.
///
/// The frame is `payload_len || payload || pad_len zero bytes`, encrypted with
/// the chunk `index` as the nonce counter so reordered or dropped chunks fail
/// to open. `pad_len` is zero except on trailing padding that conceals the
/// stream's true length.
///
/// # Errors
///
/// [`CryptoError::InvalidKey`] when `content_key` is not [`KEY_LEN`] bytes,
/// [`CryptoError::Cipher`] when sealing fails.
pub fn seal_dm_chunk(
    content_key: &[u8],
    room_id: RoomId,
    sender: UserId,
    index: u64,
    payload: &[u8],
    pad_len: usize,
) -> Result<Vec<u8>, CryptoError> {
    let key = dm_content_key(content_key)?;
    let mut frame = Vec::with_capacity(DM_CHUNK_OVERHEAD + payload.len() + pad_len);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame.resize(4 + payload.len() + pad_len, 0);
    let aad = dm_chunk_aad(room_id, sender);
    seal_in_place_append_tag(&key, index, &aad, 0, &mut frame)?;
    Ok(frame)
}

/// Opens one sealed file-transfer chunk, returning the payload with any
/// padding stripped.
///
/// # Errors
///
/// [`CryptoError::InvalidKey`] when `content_key` is not [`KEY_LEN`] bytes,
/// [`CryptoError::Cipher`] when authentication fails (tampering or a chunk
/// index mismatch), [`CryptoError::InvalidEncoding`] when the length framing
/// is inconsistent.
pub fn open_dm_chunk(
    content_key: &[u8],
    room_id: RoomId,
    sender: UserId,
    index: u64,
    frame: &mut Vec<u8>,
) -> Result<Vec<u8>, CryptoError> {
    let key = dm_content_key(content_key)?;
    let aad = dm_chunk_aad(room_id, sender);
    let plain_len = open_in_place_with_aad(&key, index, &aad, frame)?;
    let plain = &frame[..plain_len];
    let Some(prefix) = plain.first_chunk::<4>() else {
        return Err(CryptoError::InvalidEncoding);
    };
    let payload_len = u32::from_le_bytes(*prefix) as usize;
    let Some(payload) = plain[4..].get(..payload_len) else {
        return Err(CryptoError::InvalidEncoding);
    };
    Ok(payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::decode_hex;
    use ring::rand::SystemRandom;

    fn seed(hex: &str) -> [u8; E2E_SEED_LEN] {
        decode_hex(hex).unwrap().try_into().unwrap()
    }

    fn test_pair() -> (DmPairKeys, DmPairKeys) {
        let alice_seed = [1u8; E2E_SEED_LEN];
        let bob_seed = [2u8; E2E_SEED_LEN];
        let alice = dm_pair_keys(
            &alice_seed,
            UserId(7),
            &e2e_public_key(&bob_seed),
            UserId(9),
        )
        .unwrap();
        let bob = dm_pair_keys(
            &bob_seed,
            UserId(9),
            &e2e_public_key(&alice_seed),
            UserId(7),
        )
        .unwrap();
        (alice, bob)
    }

    #[test]
    fn x25519_matches_rfc7748_test_vector() {
        let alice = seed("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob = seed("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        assert_eq!(
            e2e_public_key(&alice).to_vec(),
            decode_hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a").unwrap()
        );
        assert_eq!(
            e2e_public_key(&bob).to_vec(),
            decode_hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f").unwrap()
        );
        let secret = StaticSecret::from(alice);
        let shared = secret.diffie_hellman(&PublicKey::from(e2e_public_key(&bob)));
        assert_eq!(
            shared.as_bytes().to_vec(),
            decode_hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742").unwrap()
        );
    }

    #[test]
    fn both_ends_derive_mirrored_dm_pair_keys() {
        let (alice, bob) = test_pair();
        assert_eq!(alice.send, bob.recv);
        assert_eq!(alice.recv, bob.send);
        assert_ne!(alice.send, alice.recv);
    }

    #[test]
    fn different_user_pairs_derive_distinct_keys() {
        let alice_seed = [1u8; E2E_SEED_LEN];
        let bob_public = e2e_public_key(&[2u8; E2E_SEED_LEN]);
        let a = dm_pair_keys(&alice_seed, UserId(7), &bob_public, UserId(9)).unwrap();
        let b = dm_pair_keys(&alice_seed, UserId(7), &bob_public, UserId(10)).unwrap();
        assert_ne!(a.send, b.send);
        assert_ne!(a.recv, b.recv);
    }

    #[test]
    fn rejects_low_order_peer_public_key() {
        let result = dm_pair_keys(&[1u8; E2E_SEED_LEN], UserId(7), &[0u8; 32], UserId(9));
        assert!(matches!(result, Err(CryptoError::InvalidKey)));
    }

    #[test]
    fn rejects_matching_user_ids() {
        let public = e2e_public_key(&[2u8; E2E_SEED_LEN]);
        let result = dm_pair_keys(&[1u8; E2E_SEED_LEN], UserId(7), &public, UserId(7));
        assert!(matches!(result, Err(CryptoError::InvalidKey)));
    }

    #[test]
    fn dm_envelope_round_trips_text_edit_and_file_announce() {
        let (alice, bob) = test_pair();
        let rng = SystemRandom::new();
        let contents = [
            DmContent::Text {
                body: "hello there".to_string(),
            },
            DmContent::Edit {
                body: "hello, there".to_string(),
            },
            DmContent::FileAnnounce {
                file: DmFileMeta {
                    original_name: "cat.png".to_string(),
                    size: 12345,
                    encoding: FileContentEncoding::Zstd,
                    content_key: vec![3u8; KEY_LEN],
                },
            },
        ];
        for content in contents {
            let kind = content.kind();
            let plaintext = DmPlaintext {
                sent_at_ms: 1_720_000_000_000,
                content,
            };
            let envelope =
                seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
            let opened = open_dm_envelope(&bob, RoomId(4), UserId(7), kind, &envelope).unwrap();
            assert_eq!(opened, plaintext);
        }
    }

    #[test]
    fn dm_envelope_rejects_wrong_room_sender_kind_and_tampering() {
        let (alice, bob) = test_pair();
        let rng = SystemRandom::new();
        let plaintext = DmPlaintext {
            sent_at_ms: 1,
            content: DmContent::Text {
                body: "secret".to_string(),
            },
        };
        let envelope = seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
        let open =
            |room, sender, kind, bytes: &[u8]| open_dm_envelope(&bob, room, sender, kind, bytes);
        assert!(open(RoomId(5), UserId(7), DmContentKind::Text, &envelope).is_err());
        assert!(open(RoomId(4), UserId(8), DmContentKind::Text, &envelope).is_err());
        assert!(open(RoomId(4), UserId(7), DmContentKind::Edit, &envelope).is_err());
        let mut tampered = envelope.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(RoomId(4), UserId(7), DmContentKind::Text, &tampered).is_err());
        assert!(
            open_dm_envelope(&alice, RoomId(4), UserId(7), DmContentKind::Text, &envelope).is_err(),
            "sender's own recv key must not open its send-direction envelope"
        );
        assert!(open(RoomId(4), UserId(7), DmContentKind::Text, &envelope).is_ok());
    }

    #[test]
    fn dm_envelope_pads_to_160_byte_multiples() {
        let (alice, _) = test_pair();
        let rng = SystemRandom::new();
        let sealed_len = |body: &str| {
            let plaintext = DmPlaintext {
                sent_at_ms: 1,
                content: DmContent::Text {
                    body: body.to_string(),
                },
            };
            let envelope =
                seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
            jsony::from_binary::<DmEnvelope>(&envelope)
                .unwrap()
                .sealed
                .len()
        };
        assert_eq!(sealed_len("a"), DM_PAD_MULTIPLE + TAG_LEN);
        assert_eq!(sealed_len(&"a".repeat(100)), DM_PAD_MULTIPLE + TAG_LEN);
        assert_eq!(sealed_len(&"a".repeat(200)), 2 * DM_PAD_MULTIPLE + TAG_LEN);
        assert_eq!(sealed_len("b"), sealed_len(&"b".repeat(100)));
    }

    #[test]
    fn padme_len_matches_reference_values() {
        assert_eq!(padme_len(0), 0);
        assert_eq!(padme_len(1), 1);
        assert_eq!(padme_len(2), 2);
        assert_eq!(padme_len(9), 10);
        assert_eq!(padme_len(100), 104);
        assert_eq!(padme_len(1000), 1024);
        assert_eq!(padme_len(1024), 1024);
        assert_eq!(padme_len(1025), 1088);
        assert_eq!(padme_len(1_000_000), 1_015_808);
        for len in 2..4096u64 {
            let padded = padme_len(len);
            assert!(padded >= len);
            assert!((padded - len) as f64 / len as f64 <= 0.12);
            assert_eq!(padme_len(padded), padded, "padmé must be idempotent");
        }
    }

    #[test]
    fn dm_chunk_round_trips_and_rejects_reordered_index() {
        let content_key = vec![5u8; KEY_LEN];
        let payload = vec![9u8; 1000];
        let frame = seal_dm_chunk(&content_key, RoomId(4), UserId(7), 3, &payload, 24).unwrap();
        assert_eq!(frame.len(), DM_CHUNK_OVERHEAD + payload.len() + 24);
        let mut opened = frame.clone();
        assert_eq!(
            open_dm_chunk(&content_key, RoomId(4), UserId(7), 3, &mut opened).unwrap(),
            payload
        );
        let mut reordered = frame.clone();
        assert!(open_dm_chunk(&content_key, RoomId(4), UserId(7), 4, &mut reordered).is_err());
        let mut wrong_room = frame;
        assert!(open_dm_chunk(&content_key, RoomId(5), UserId(7), 3, &mut wrong_room).is_err());
    }

    #[test]
    fn open_rejects_truncated_and_oversized_envelopes() {
        let (alice, bob) = test_pair();
        let rng = SystemRandom::new();
        let plaintext = DmPlaintext {
            sent_at_ms: 1,
            content: DmContent::Text {
                body: "x".to_string(),
            },
        };
        let envelope = seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
        for len in [0, 1, envelope.len() / 2] {
            assert!(
                open_dm_envelope(
                    &bob,
                    RoomId(4),
                    UserId(7),
                    DmContentKind::Text,
                    &envelope[..len]
                )
                .is_err()
            );
        }
        let oversized = vec![0u8; MAX_DM_ENVELOPE_BYTES + 1];
        assert!(
            open_dm_envelope(&bob, RoomId(4), UserId(7), DmContentKind::Text, &oversized).is_err()
        );
    }
}
