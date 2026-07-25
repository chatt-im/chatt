//! Codec and lexical contract for compact `@@` message references.

use chatt_ids::{MessageId, RoomId};

/// Prefix that introduces a message reference in a message body.
pub const REF_PREFIX: &str = "@@";

/// Shortest reference code accepted by the codec.
pub const MIN_CODE_LEN: usize = 5;

/// Longest reference code accepted by the codec.
pub const MAX_CODE_LEN: usize = 25;

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The durable identity of a message referenced from plaintext.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MessageRef {
    pub room_id: RoomId,
    pub message_id: MessageId,
}

impl MessageRef {
    /// Encodes this reference as a lowercase Crockford-base32 code without
    /// [`REF_PREFIX`].
    pub fn encode(self) -> String {
        let mut payload = Vec::with_capacity(16);
        push_leb128(&mut payload, u64::from(self.room_id.0));
        push_leb128(&mut payload, self.message_id.0);
        let mut out = encode_base32(&payload);
        out.push(encode_value(checksum(&payload)));
        out
    }

    /// Decodes one canonical reference code without [`REF_PREFIX`].
    ///
    /// Uppercase and the Crockford `i`/`l`/`o` aliases are accepted. Bad
    /// lengths, characters, checksums, and non-minimal encodings are rejected.
    pub fn decode(code: &str) -> Option<Self> {
        if !(MIN_CODE_LEN..=MAX_CODE_LEN).contains(&code.len()) {
            return None;
        }
        let mut values = Vec::with_capacity(code.len());
        for byte in code.bytes() {
            values.push(decode_value(byte)?);
        }
        let (&check, data) = values.split_last()?;
        let mut payload = Vec::with_capacity(data.len() * 5 / 8);
        let mut acc = 0u32;
        let mut bits = 0u32;
        for &value in data {
            acc = acc << 5 | u32::from(value);
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                payload.push((acc >> bits) as u8);
            }
        }
        if checksum(&payload) != check {
            return None;
        }
        let mut pos = 0;
        let room_id = RoomId(u32::try_from(read_leb128(&payload, &mut pos)?).ok()?);
        let message_id = MessageId(read_leb128(&payload, &mut pos)?);
        if pos != payload.len() {
            return None;
        }
        let decoded = Self {
            room_id,
            message_id,
        };
        let normalized: String = values.iter().map(|value| encode_value(*value)).collect();
        (decoded.encode() == normalized).then_some(decoded)
    }
}

/// Returns whether `byte` belongs to the accepted Crockford base32 alphabet.
///
/// Uppercase and the conventional `i`/`l`/`o` aliases are accepted. `u` is
/// deliberately absent from Crockford base32.
pub const fn is_ref_char(byte: u8) -> bool {
    matches!(
        byte.to_ascii_lowercase(),
        b'0'..=b'9'
            | b'a'..=b'h'
            | b'i'
            | b'j'
            | b'k'
            | b'l'
            | b'm'
            | b'n'
            | b'o'
            | b'p'..=b't'
            | b'v'..=b'z'
    )
}

fn encode_base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8).div_ceil(5));
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &byte in bytes {
        acc = acc << 8 | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(encode_value(((acc >> bits) & 31) as u8));
        }
    }
    if bits > 0 {
        out.push(encode_value(((acc << (5 - bits)) & 31) as u8));
    }
    out
}

fn encode_value(value: u8) -> char {
    ALPHABET[usize::from(value & 31)] as char
}

fn decode_value(byte: u8) -> Option<u8> {
    let byte = match byte.to_ascii_lowercase() {
        b'i' | b'l' => b'1',
        b'o' => b'0',
        byte => byte,
    };
    ALPHABET
        .iter()
        .position(|candidate| *candidate == byte)
        .map(|value| value as u8)
}

fn checksum(payload: &[u8]) -> u8 {
    payload
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        & 31
}

fn push_leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn read_leb128(bytes: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let &byte = bytes.get(*pos)?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(room: u32, message: u64) -> MessageRef {
        MessageRef {
            room_id: RoomId(room),
            message_id: MessageId(message),
        }
    }

    #[test]
    fn reference_alphabet_matches_crockford_alias_contract() {
        assert!(is_ref_char(b'0'));
        assert!(is_ref_char(b'z'));
        assert!(is_ref_char(b'I'));
        assert!(is_ref_char(b'L'));
        assert!(is_ref_char(b'O'));
        assert!(!is_ref_char(b'u'));
        assert!(!is_ref_char(b'U'));
        assert!(!is_ref_char(b'-'));
    }

    #[test]
    fn reference_codec_roundtrips_edge_values() {
        for case in [
            reference(0, 0),
            reference(0, 1),
            reference(u32::MAX, u64::MAX),
            reference(1, 1 << 42),
            reference(7, 12_345),
        ] {
            let code = case.encode();
            assert_eq!(MessageRef::decode(&code), Some(case), "code {code}");
            assert!((MIN_CODE_LEN..=MAX_CODE_LEN).contains(&code.len()));
        }
    }

    #[test]
    fn reference_codec_accepts_aliases_and_rejects_damage() {
        let case = reference(3, 99);
        let code = case.encode();
        assert_eq!(MessageRef::decode(&code.to_ascii_uppercase()), Some(case));
        assert_eq!(
            MessageRef::decode(&code.replace('1', "l").replace('0', "O")),
            Some(case)
        );

        let mut damaged = code.into_bytes();
        let last = damaged.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        assert_eq!(
            MessageRef::decode(&String::from_utf8(damaged).unwrap()),
            None
        );
        assert_eq!(MessageRef::decode("abcdefu0"), None);
    }

    #[test]
    fn reference_codec_rejects_noncanonical_padding() {
        let code = reference(0, 0).encode();
        assert_eq!(code.len(), MIN_CODE_LEN);
        assert_eq!(MessageRef::decode(&format!("0{code}")), None);
    }
}
