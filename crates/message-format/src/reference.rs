//! Lexical contract for compact `@@` message references.

/// Prefix that introduces a message reference in a message body.
pub const REF_PREFIX: &str = "@@";

/// Shortest reference code accepted by the codec.
pub const MIN_CODE_LEN: usize = 5;

/// Longest reference code accepted by the codec.
pub const MAX_CODE_LEN: usize = 25;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
