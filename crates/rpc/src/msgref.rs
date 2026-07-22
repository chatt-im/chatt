//! Compact textual references to chat messages.
//!
//! A reference addresses a message by the durable key the rest of the system
//! uses for dedup and merge: `message_id` scoped by `room_id`. Message ids are
//! per-room, monotonic, and persistently watermarked, so the id alone is the
//! identity. (A server that loses its room-state database on a room without a
//! durable log can restart ids, in which case an old code may resolve to a different
//! message; accepted for a reference that is ultimately display text.)
//! References travel inside message bodies as `@@<code>` where the code is
//! Crockford base32 of the LEB128-packed fields plus a one-character checksum.
//! Encoding is canonical: a code decodes only if re-encoding the result
//! reproduces it.

use crate::{
    base32,
    ids::{MessageId, RoomId},
};

pub use chatt_message_format::reference::{
    MAX_CODE_LEN, MIN_CODE_LEN, REF_PREFIX, is_ref_char,
};

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
///     message_id: MessageId(42),
/// };
/// let code = msg_ref.encode();
/// assert_eq!(MessageRef::decode(&code), Some(msg_ref));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageRef {
    pub room_id: RoomId,
    pub message_id: MessageId,
}

impl MessageRef {
    /// Encodes this reference as a lowercase base32 code without the `@@` prefix.
    pub fn encode(&self) -> String {
        let mut payload = Vec::with_capacity(16);
        push_leb128(&mut payload, u64::from(self.room_id.0));
        push_leb128(&mut payload, self.message_id.0);
        let mut out = base32::encode(&payload);
        out.push(base32::encode_value(checksum(&payload)));
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
            values.push(base32::decode_value(byte)?);
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
        if pos != payload.len() {
            return None;
        }
        let decoded = MessageRef {
            room_id,
            message_id,
        };
        let mut normalized = String::with_capacity(values.len());
        for &value in &values {
            normalized.push(base32::encode_value(value));
        }
        if decoded.encode() != normalized {
            return None;
        }
        Some(decoded)
    }
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

    fn reference(room: u32, message: u64) -> MessageRef {
        MessageRef {
            room_id: RoomId(room),
            message_id: MessageId(message),
        }
    }

    #[test]
    fn roundtrips_edge_values() {
        let cases = [
            reference(0, 0),
            reference(0, 1),
            reference(u32::MAX, u64::MAX),
            reference(1, 1 << 42),
            reference(7, 12345),
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
        let code = reference(1, 4821).encode();
        assert!(code.len() <= 8, "unexpectedly long code {code}");
    }

    #[test]
    fn decodes_uppercase_and_aliases() {
        let case = reference(3, 99);
        let code = case.encode();
        assert_eq!(MessageRef::decode(&code.to_ascii_uppercase()), Some(case));
        let aliased = code.replace('1', "l").replace('0', "O");
        assert_eq!(MessageRef::decode(&aliased), Some(case));
    }

    #[test]
    fn rejects_checksum_flip() {
        let code = reference(1, 7).encode();
        let mut flipped = code.into_bytes();
        let last = flipped.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let flipped = String::from_utf8(flipped).unwrap();
        assert_eq!(MessageRef::decode(&flipped), None);
    }

    #[test]
    fn rejects_truncated_and_extended_codes() {
        let code = reference(1, 700_000).encode();
        assert_eq!(MessageRef::decode(&code[..code.len() - 1]), None);
        assert_eq!(MessageRef::decode(&format!("{code}0")), None);
        assert_eq!(MessageRef::decode(""), None);
        assert_eq!(MessageRef::decode("0000"), None);
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
        let case = reference(0, 0);
        let code = case.encode();
        assert_eq!(code.len(), MIN_CODE_LEN);
        let padded = format!("0{code}");
        assert_eq!(MessageRef::decode(&padded), None);
    }
}
