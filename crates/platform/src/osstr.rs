//! Byte encoding for OS strings.
//!
//! Chatt persists a few filesystem paths as `Vec<u8>` inside durable local
//! records. A path is not necessarily valid UTF-8 on either platform, so it
//! cannot go through `String` without risking a path that no longer opens the
//! file it named.
//!
//! Unix paths are already byte strings, so they are stored verbatim. Windows
//! paths are sequences of UTF-16 code units that may include unpaired
//! surrogates, so they are stored as those code units in little-endian order.
//! Both encodings round-trip exactly, and the bytes are only ever decoded on
//! the platform that wrote them.

use std::ffi::{OsStr, OsString};

/// Encodes an OS string as bytes that [`os_string_from_bytes`] can decode.
pub fn os_str_to_bytes(value: &OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut bytes = Vec::new();
        for unit in value.encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }
}

/// Decodes bytes produced by [`os_str_to_bytes`].
///
/// A Windows encoding with an odd length cannot be a sequence of UTF-16 code
/// units; the trailing byte is dropped rather than failing, because the caller
/// is recovering a best-effort record rather than validating input.
pub fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        OsString::from_wide(&units)
    }
}

/// Encodes `value` as the NUL-terminated UTF-16 that Win32 `*W` entry points
/// take.
///
/// Going through `OsStr` rather than `str` keeps a path that is not valid
/// Unicode addressable, which is the same reason [`os_str_to_bytes`] exists.
#[cfg(windows)]
pub fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn wide_null_terminates_and_preserves_units() {
        let encoded = wide_null("ab");
        assert_eq!(encoded, vec![b'a' as u16, b'b' as u16, 0]);
    }

    #[test]
    fn round_trips_ascii() {
        let original = OsString::from("some/dir/file.txt");
        let bytes = os_str_to_bytes(&original);
        assert_eq!(os_string_from_bytes(&bytes), original);
    }

    #[test]
    fn round_trips_non_ascii() {
        let original = OsString::from("répertoire/日本語.txt");
        let bytes = os_str_to_bytes(&original);
        assert_eq!(os_string_from_bytes(&bytes), original);
    }

    #[test]
    fn round_trips_empty() {
        let original = OsString::new();
        let bytes = os_str_to_bytes(&original);
        assert!(bytes.is_empty());
        assert_eq!(os_string_from_bytes(&bytes), original);
    }

    /// The encodings are not interchangeable across platforms, but each must be
    /// self-consistent for any byte string it can produce.
    #[test]
    fn decoding_tolerates_a_truncated_encoding() {
        let original = OsString::from("abc");
        let mut bytes = os_str_to_bytes(&original);
        bytes.pop();
        // Must not panic; the exact result is platform-dependent.
        let _ = os_string_from_bytes(&bytes);
    }
}
