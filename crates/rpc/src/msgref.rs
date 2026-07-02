//! Compact textual references to chat messages.
//!
//! A reference addresses a message by the durable key the rest of the system
//! already uses for dedup and merge: `(timestamp_ms, message_id)` scoped by
//! `room_id` (message ids restart with the server process, so the timestamp is
//! required for uniqueness). References travel inside message bodies as
//! `@@<code>` where the code is Crockford base32 of the LEB128-packed fields
//! plus a one-character checksum. Encoding is canonical: a code decodes only
//! if re-encoding the result reproduces it.

use crate::ids::{MessageId, RoomId};

/// Prefix that introduces a message reference in message bodies.
pub const REF_PREFIX: &str = "@@";

/// Shortest code [`MessageRef::decode`] can accept.
pub const MIN_CODE_LEN: usize = 6;

/// Longest code [`MessageRef::decode`] can accept.
pub const MAX_CODE_LEN: usize = 41;

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// The durable identity of a chat message, as carried by an `@@` reference.
///
/// # Examples
///
/// ```
/// use rpc::ids::{MessageId, RoomId};
/// use rpc::msgref::MessageRef;
///
/// let msg_ref = MessageRef {
///     room_id: RoomId(1),
///     timestamp_ms: 1_751_400_000_000,
///     message_id: MessageId(42),
/// };
/// let code = msg_ref.encode();
/// assert_eq!(MessageRef::decode(&code), Some(msg_ref));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageRef {
    pub room_id: RoomId,
    pub timestamp_ms: u64,
    pub message_id: MessageId,
}

impl MessageRef {
    /// Encodes this reference as a lowercase base32 code without the `@@` prefix.
    pub fn encode(&self) -> String {
        let mut payload = Vec::with_capacity(16);
        push_leb128(&mut payload, u64::from(self.room_id.0));
        push_leb128(&mut payload, self.message_id.0);
        push_leb128(&mut payload, self.timestamp_ms);
        let mut out = String::with_capacity(payload.len() * 2);
        let mut acc = 0u32;
        let mut bits = 0u32;
        for &byte in &payload {
            acc = acc << 8 | u32::from(byte);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(ALPHABET[(acc >> bits) as usize & 31] as char);
            }
        }
        if bits > 0 {
            out.push(ALPHABET[(acc << (5 - bits)) as usize & 31] as char);
        }
        out.push(ALPHABET[usize::from(checksum(&payload))] as char);
        out
    }

    /// Decodes a code produced by [`MessageRef::encode`].
    ///
    /// Accepts uppercase input and the Crockford aliases `i`/`l` for `1` and
    /// `o` for `0`. Returns [`None`] for anything that is not the canonical
    /// encoding of a reference: bad length or characters, checksum mismatch,
    /// or a non-minimal packing.
    pub fn decode(code: &str) -> Option<MessageRef> {
        if !(MIN_CODE_LEN..=MAX_CODE_LEN).contains(&code.len()) {
            return None;
        }
        let mut values = Vec::with_capacity(code.len());
        for byte in code.bytes() {
            values.push(char_value(byte)?);
        }
        let Some((&check, data)) = values.split_last() else {
            return None;
        };
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
        let room_id = read_leb128(&payload, &mut pos)?;
        let room_id = RoomId(u32::try_from(room_id).ok()?);
        let message_id = MessageId(read_leb128(&payload, &mut pos)?);
        let timestamp_ms = read_leb128(&payload, &mut pos)?;
        if pos != payload.len() {
            return None;
        }
        let decoded = MessageRef {
            room_id,
            timestamp_ms,
            message_id,
        };
        let mut normalized = String::with_capacity(values.len());
        for &value in &values {
            normalized.push(ALPHABET[usize::from(value)] as char);
        }
        if decoded.encode() != normalized {
            return None;
        }
        Some(decoded)
    }
}

/// Returns whether `byte` can appear in a reference code.
///
/// This is the accept set for tokenizers scanning `@@` codes: the Crockford
/// base32 alphabet in either case plus the decode aliases `i`, `l`, and `o`.
pub fn is_ref_char(byte: u8) -> bool {
    char_value(byte).is_some()
}

fn char_value(byte: u8) -> Option<u8> {
    let byte = byte.to_ascii_lowercase();
    let byte = match byte {
        b'i' | b'l' => b'1',
        b'o' => b'0',
        _ => byte,
    };
    let index = ALPHABET.iter().position(|&c| c == byte)?;
    Some(index as u8)
}

fn checksum(payload: &[u8]) -> u8 {
    let mut sum = 0u8;
    for &byte in payload {
        sum = sum.wrapping_add(byte);
    }
    sum & 31
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

    fn reference(room: u32, timestamp_ms: u64, message: u64) -> MessageRef {
        MessageRef {
            room_id: RoomId(room),
            timestamp_ms,
            message_id: MessageId(message),
        }
    }

    #[test]
    fn roundtrips_edge_values() {
        let cases = [
            reference(0, 0, 0),
            reference(0, 0, 1),
            reference(u32::MAX, u64::MAX, u64::MAX),
            reference(1, 1 << 42, 1),
            reference(7, 1_751_400_000_000, 12345),
        ];
        for case in cases {
            let code = case.encode();
            assert_eq!(MessageRef::decode(&code), Some(case), "code {code}");
            assert!(
                (MIN_CODE_LEN..=MAX_CODE_LEN).contains(&code.len()),
                "length {} outside gate for {code}",
                code.len()
            );
        }
    }

    #[test]
    fn typical_code_stays_short() {
        let code = reference(1, 1_751_400_000_000, 4821).encode();
        assert!(code.len() <= 17, "unexpectedly long code {code}");
    }

    #[test]
    fn decodes_uppercase_and_aliases() {
        let case = reference(3, 1_751_400_000_000, 99);
        let code = case.encode();
        assert_eq!(MessageRef::decode(&code.to_ascii_uppercase()), Some(case));
        let aliased = code.replace('1', "l").replace('0', "O");
        assert_eq!(MessageRef::decode(&aliased), Some(case));
    }

    #[test]
    fn rejects_checksum_flip() {
        let code = reference(1, 1_751_400_000_000, 7).encode();
        let mut flipped = code.into_bytes();
        let last = flipped.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let flipped = String::from_utf8(flipped).unwrap();
        assert_eq!(MessageRef::decode(&flipped), None);
    }

    #[test]
    fn rejects_truncated_and_extended_codes() {
        let code = reference(1, 1_751_400_000_000, 7).encode();
        assert_eq!(MessageRef::decode(&code[..code.len() - 1]), None);
        assert_eq!(MessageRef::decode(&format!("{code}0")), None);
        assert_eq!(MessageRef::decode(""), None);
        assert_eq!(MessageRef::decode("00000"), None);
    }

    #[test]
    fn rejects_invalid_characters() {
        assert_eq!(MessageRef::decode("abcdefu0"), None);
        assert_eq!(MessageRef::decode("abc def0"), None);
        assert!(!is_ref_char(b'u'));
        assert!(!is_ref_char(b'U'));
        assert!(is_ref_char(b'I'));
        assert!(is_ref_char(b'z'));
        assert!(is_ref_char(b'0'));
    }

    #[test]
    fn rejects_non_canonical_packing() {
        let case = reference(0, 0, 0);
        let code = case.encode();
        assert_eq!(code.len(), MIN_CODE_LEN);
        let padded = format!("0{code}");
        assert_eq!(MessageRef::decode(&padded), None);
    }
}
