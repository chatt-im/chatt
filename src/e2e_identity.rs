//! Canonical, public presentations of a Chatt X25519 identity.
//!
//! These values contain no private key material. The word form encodes the
//! exact 256-bit public key plus an eight-bit error-detection checksum; the
//! verification text binds that same key to a server and account context.

use ring::digest::{SHA256, digest};
use rpc::base32;
use rpc::crypto::{decode_hex, encode_hex};

const PUBLIC_KEY_LEN: usize = 32;
const WORD_COUNT: usize = 24;
const WORD_BITS: usize = 11;
const VERIFICATION_TEXT_CHECKSUM_BYTES: usize = 8;
const VERIFICATION_TEXT_PREFIX: &str = "chatt-e2e:v1";
const WORDLIST_TEXT: &str = include_str!("../assets/english.txt");

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct E2ePublicIdentity {
    public_key: [u8; PUBLIC_KEY_LEN],
}

impl E2ePublicIdentity {
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, IdentityError> {
        let public_key =
            <[u8; PUBLIC_KEY_LEN]>::try_from(bytes).map_err(|_| IdentityError::InvalidPublicKey)?;
        Ok(Self { public_key })
    }

    pub(crate) fn from_hex(value: &str) -> Result<Self, IdentityError> {
        let decoded = decode_hex(value).map_err(|_| IdentityError::InvalidPublicKey)?;
        Self::from_bytes(&decoded)
    }

    pub(crate) fn public_key(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.public_key
    }

    pub(crate) fn hex(&self) -> String {
        encode_hex(&self.public_key)
    }

    pub(crate) fn words(&self) -> Vec<&'static str> {
        let words = wordlist();
        identity_word_indices(&self.public_key)
            .into_iter()
            .map(|index| words[index])
            .collect()
    }

    pub(crate) fn words_string(&self) -> String {
        self.words().join(" ")
    }

    #[allow(dead_code)]
    pub(crate) fn from_words(value: &str) -> Result<Self, IdentityError> {
        let wordlist = wordlist();
        let supplied: Vec<_> = value.split_whitespace().collect();
        if supplied.len() != WORD_COUNT {
            return Err(IdentityError::InvalidWordCount);
        }
        let mut bits = [0_u8; 33];
        for (word_number, word) in supplied.into_iter().enumerate() {
            let index = wordlist
                .binary_search(&word)
                .map_err(|_| IdentityError::UnknownWord(word.to_string()))?;
            for bit in 0..WORD_BITS {
                let source = (index >> (WORD_BITS - 1 - bit)) & 1;
                let offset = word_number * WORD_BITS + bit;
                bits[offset / 8] |= (source as u8) << (7 - offset % 8);
            }
        }
        let mut public_key = [0_u8; PUBLIC_KEY_LEN];
        public_key.copy_from_slice(&bits[..PUBLIC_KEY_LEN]);
        if bits[PUBLIC_KEY_LEN] != checksum_byte(&public_key) {
            return Err(IdentityError::WordChecksumMismatch);
        }
        Ok(Self { public_key })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerificationText {
    server_public_key: [u8; PUBLIC_KEY_LEN],
    user_id: u64,
    identity: E2ePublicIdentity,
}

impl VerificationText {
    pub(crate) fn new(
        server_public_key: &[u8],
        user_id: u64,
        public_key: &[u8],
    ) -> Result<Self, IdentityError> {
        let server_public_key = <[u8; PUBLIC_KEY_LEN]>::try_from(server_public_key)
            .map_err(|_| IdentityError::InvalidServerKey)?;
        Ok(Self {
            server_public_key,
            user_id,
            identity: E2ePublicIdentity::from_bytes(public_key)?,
        })
    }

    pub(crate) fn parse(value: &str) -> Result<Self, VerificationTextError> {
        let value = value.trim();
        let mut fields = value.split(':');
        if fields.next() != Some("chatt-e2e") || fields.next() != Some("v1") {
            return Err(VerificationTextError::UnsupportedVersion);
        }
        let server_base32 = fields.next().ok_or(VerificationTextError::Malformed)?;
        let user = fields.next().ok_or(VerificationTextError::Malformed)?;
        let public_base32 = fields.next().ok_or(VerificationTextError::Malformed)?;
        let checksum = fields.next().ok_or(VerificationTextError::Malformed)?;
        if fields.next().is_some() {
            return Err(VerificationTextError::Malformed);
        }
        let canonical =
            format!("{VERIFICATION_TEXT_PREFIX}:{server_base32}:{user}:{public_base32}");
        if checksum != verification_text_checksum(&canonical) {
            return Err(VerificationTextError::ChecksumMismatch);
        }
        let server =
            base32::decode(server_base32).ok_or(VerificationTextError::InvalidServerKey)?;
        let server_public_key = <[u8; PUBLIC_KEY_LEN]>::try_from(server.as_slice())
            .map_err(|_| VerificationTextError::InvalidServerKey)?;
        let user_id = user
            .parse::<u64>()
            .map_err(|_| VerificationTextError::InvalidUserId)?;
        if user != user_id.to_string() {
            return Err(VerificationTextError::NonCanonical);
        }
        let public_key =
            base32::decode(public_base32).ok_or(VerificationTextError::InvalidPublicKey)?;
        let identity = E2ePublicIdentity::from_bytes(&public_key)
            .map_err(|_| VerificationTextError::InvalidPublicKey)?;
        if server_base32 != base32::encode(&server_public_key)
            || public_base32 != base32::encode(identity.public_key())
        {
            return Err(VerificationTextError::NonCanonical);
        }
        Ok(Self {
            server_public_key,
            user_id,
            identity,
        })
    }

    pub(crate) fn encode(&self) -> String {
        let canonical = format!(
            "{VERIFICATION_TEXT_PREFIX}:{}:{}:{}",
            base32::encode(&self.server_public_key),
            self.user_id,
            base32::encode(self.identity.public_key())
        );
        format!("{canonical}:{}", verification_text_checksum(&canonical))
    }

    pub(crate) fn identity(&self) -> &E2ePublicIdentity {
        &self.identity
    }

    pub(crate) fn server_public_key(&self) -> &[u8; PUBLIC_KEY_LEN] {
        &self.server_public_key
    }

    pub(crate) fn user_id(&self) -> u64 {
        self.user_id
    }

    pub(crate) fn match_context(
        &self,
        server_public_key: &[u8],
        local_user_id: u64,
        expected_user_id: u64,
        expected_public_key: &[u8],
    ) -> Result<(), VerificationTextMatchError> {
        if self.server_public_key.as_slice() != server_public_key {
            return Err(VerificationTextMatchError::WrongServer);
        }
        if self.user_id == local_user_id {
            return Err(VerificationTextMatchError::SelfText);
        }
        if self.user_id != expected_user_id {
            return Err(VerificationTextMatchError::WrongUser {
                presented: self.user_id,
                expected: expected_user_id,
            });
        }
        if self.identity.public_key().as_slice() != expected_public_key {
            return Err(VerificationTextMatchError::KeyMismatch);
        }
        Ok(())
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IdentityError {
    InvalidPublicKey,
    InvalidServerKey,
    InvalidWordCount,
    UnknownWord(String),
    WordChecksumMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationTextError {
    UnsupportedVersion,
    Malformed,
    InvalidServerKey,
    InvalidUserId,
    InvalidPublicKey,
    ChecksumMismatch,
    NonCanonical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationTextMatchError {
    WrongServer,
    WrongUser { presented: u64, expected: u64 },
    SelfText,
    KeyMismatch,
}

fn wordlist() -> Vec<&'static str> {
    WORDLIST_TEXT.lines().collect()
}

fn identity_word_indices(public_key: &[u8; PUBLIC_KEY_LEN]) -> [usize; WORD_COUNT] {
    let mut input = [0_u8; 33];
    input[..PUBLIC_KEY_LEN].copy_from_slice(public_key);
    input[PUBLIC_KEY_LEN] = checksum_byte(public_key);
    let mut indices = [0_usize; WORD_COUNT];
    for (word, index) in indices.iter_mut().enumerate() {
        for bit in 0..WORD_BITS {
            let offset = word * WORD_BITS + bit;
            *index = (*index << 1) | usize::from((input[offset / 8] >> (7 - offset % 8)) & 1);
        }
    }
    indices
}

fn checksum_byte(public_key: &[u8; PUBLIC_KEY_LEN]) -> u8 {
    digest(&SHA256, public_key).as_ref()[0]
}

fn verification_text_checksum(canonical: &str) -> String {
    base32::encode(
        &digest(&SHA256, canonical.as_bytes()).as_ref()[..VERIFICATION_TEXT_CHECKSUM_BYTES],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn wordlist_is_canonical() {
        let words = wordlist();
        assert_eq!(words.len(), 2048);
        assert_eq!(words.iter().copied().collect::<HashSet<_>>().len(), 2048);
        assert!(words.iter().all(|word| {
            !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_lowercase())
        }));
        assert!(words.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn zero_public_key_has_stable_known_words() {
        let identity = E2ePublicIdentity::from_bytes(&[0; 32]).unwrap();
        assert_eq!(identity.hex(), "00".repeat(32));
        assert_eq!(
            identity.words_string(),
            format!(
                "{} art",
                std::iter::repeat_n("abandon", 23)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        );
        assert_eq!(
            E2ePublicIdentity::from_words(&identity.words_string()).unwrap(),
            identity
        );
    }

    #[test]
    fn word_checksum_detects_corruption() {
        let identity = E2ePublicIdentity::from_bytes(&[0; 32]).unwrap();
        let mut words = identity.words();
        words[23] = "zoo";
        assert_eq!(
            E2ePublicIdentity::from_words(&words.join(" ")),
            Err(IdentityError::WordChecksumMismatch)
        );
    }

    #[test]
    fn verification_text_round_trips_canonically() {
        let text = VerificationText::new(&[0x11; 32], 42, &[0x22; 32]).unwrap();
        let encoded = text.encode();
        assert!(encoded.len() < 150);
        assert_eq!(VerificationText::parse(&encoded).unwrap(), text);
        assert_eq!(VerificationText::parse(&encoded).unwrap().encode(), encoded);
    }

    #[test]
    fn verification_text_rejects_checksum_and_noncanonical_fields() {
        let text = VerificationText::new(&[0x11; 32], 42, &[0x22; 32]).unwrap();
        let mut encoded = text.encode();
        let last = encoded.pop().unwrap();
        encoded.push(if last == '0' { '1' } else { '0' });
        assert_eq!(
            VerificationText::parse(&encoded),
            Err(VerificationTextError::ChecksumMismatch)
        );

        let canonical = format!(
            "{VERIFICATION_TEXT_PREFIX}:{}:042:{}",
            base32::encode(&[0x11; 32]),
            base32::encode(&[0x22; 32])
        );
        let encoded = format!("{canonical}:{}", verification_text_checksum(&canonical));
        assert_eq!(
            VerificationText::parse(&encoded),
            Err(VerificationTextError::NonCanonical)
        );
    }

    #[test]
    fn verification_text_context_rejects_wrong_server_user_self_and_stale_key() {
        let text = VerificationText::new(&[0x11; 32], 42, &[0x22; 32]).unwrap();
        assert_eq!(
            text.match_context(&[0x33; 32], 7, 42, &[0x22; 32]),
            Err(VerificationTextMatchError::WrongServer)
        );
        assert_eq!(
            text.match_context(&[0x11; 32], 7, 43, &[0x22; 32]),
            Err(VerificationTextMatchError::WrongUser {
                presented: 42,
                expected: 43
            })
        );
        assert_eq!(
            text.match_context(&[0x11; 32], 42, 42, &[0x22; 32]),
            Err(VerificationTextMatchError::SelfText)
        );
        assert_eq!(
            text.match_context(&[0x11; 32], 7, 42, &[0x44; 32]),
            Err(VerificationTextMatchError::KeyMismatch)
        );
        assert_eq!(text.match_context(&[0x11; 32], 7, 42, &[0x22; 32]), Ok(()));
    }
}
