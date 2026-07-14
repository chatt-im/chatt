//! Unpadded Crockford base32 used by Chatt's compact textual identifiers.

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// Encodes arbitrary bytes as lowercase, unpadded Crockford base32.
pub fn encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    for &byte in bytes {
        accumulator = accumulator << 8 | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            output.push(encode_value(((accumulator >> bits) & 31) as u8));
        }
    }
    if bits > 0 {
        output.push(encode_value(((accumulator << (5 - bits)) & 31) as u8));
    }
    output
}

/// Decodes unpadded Crockford base32.
///
/// Uppercase input and the conventional `i`/`l`/`o` aliases are accepted.
/// Non-zero padding bits are rejected.
pub fn decode(value: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len() * 5 / 8);
    let mut accumulator = 0_u32;
    let mut bits = 0_u32;
    for byte in value.bytes() {
        accumulator = accumulator << 5 | u32::from(decode_value(byte)?);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits).wrapping_sub(1);
        }
    }
    if bits > 0 && accumulator != 0 {
        return None;
    }
    Some(output)
}

pub(crate) fn encode_value(value: u8) -> char {
    ALPHABET[usize::from(value & 31)] as char
}

pub(crate) fn decode_value(byte: u8) -> Option<u8> {
    let byte = match byte.to_ascii_lowercase() {
        b'i' | b'l' => b'1',
        b'o' => b'0',
        byte => byte,
    };
    ALPHABET
        .iter()
        .position(|&candidate| candidate == byte)
        .map(|index| index as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_bytes_round_trip_without_padding() {
        for length in 0..=64 {
            let bytes: Vec<_> = (0..length).map(|index| index as u8).collect();
            let encoded = encode(&bytes);
            assert!(!encoded.contains('='));
            assert_eq!(decode(&encoded), Some(bytes));
        }
    }

    #[test]
    fn decoder_accepts_crockford_aliases_but_rejects_nonzero_padding() {
        let encoded = encode(b"thing");
        assert_eq!(decode(&encoded), Some(b"thing".to_vec()));
        assert_eq!(
            decode(&encoded.to_ascii_uppercase()),
            Some(b"thing".to_vec())
        );
        assert_eq!(decode("00"), Some(vec![0]));
        assert_eq!(decode("O0"), Some(vec![0]));
        assert_eq!(decode("10"), Some(vec![8]));
        assert_eq!(decode("I0"), Some(vec![8]));
        assert_eq!(decode("03"), None);
    }
}
