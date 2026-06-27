use crate::http::request::Request;
use crate::http::response::{self, PreparedResponse};
use crate::util::sha1_digest;

pub(crate) fn is_upgrade(request: &Request) -> bool {
    request.is_websocket_upgrade()
}

pub(crate) fn prepare_response(request: &Request) -> Result<PreparedResponse, HandshakeError> {
    let key = request.websocket_key().ok_or(HandshakeError::MissingKey)?;
    if !request.websocket_version_supported() {
        return Err(HandshakeError::UnsupportedVersion);
    }
    Ok(response::switching_protocols(&websocket_accept_key(key)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandshakeError {
    MissingKey,
    UnsupportedVersion,
}

fn websocket_accept_key(key: &[u8]) -> [u8; 28] {
    let mut input = [0u8; 164];
    let len = key.len() + 36;
    input[..key.len()].copy_from_slice(key);
    input[key.len()..len].copy_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64_sha1(&sha1_digest(&input[..len]))
}

fn base64_sha1(input: &[u8; 20]) -> [u8; 28] {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = [b'='; 28];
    let mut i = 0;
    let mut j = 0;
    while i < input.len() {
        let a = input[i] as u32;
        i += 1;
        let b = if i < input.len() {
            let v = input[i] as u32;
            i += 1;
            v
        } else {
            0
        };
        let c = if i < input.len() {
            let v = input[i] as u32;
            i += 1;
            v
        } else {
            0
        };
        let triple = (a << 16) | (b << 8) | c;
        out[j] = TABLE[((triple >> 18) & 0x3f) as usize];
        out[j + 1] = TABLE[((triple >> 12) & 0x3f) as usize];
        out[j + 2] = TABLE[((triple >> 6) & 0x3f) as usize];
        out[j + 3] = TABLE[(triple & 0x3f) as usize];
        j += 4;
    }
    out[27] = b'=';
    out
}

#[cfg(test)]
mod tests {
    use super::websocket_accept_key;

    #[test]
    fn accept_key_matches_rfc_example() {
        assert_eq!(
            websocket_accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="),
            *b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
