//! Wire codec for the dedicated screen-share video connection.
//!
//! A video stream uses its own TCP connection, separate from the encrypted
//! control channel, because a keyframe exceeds the control [`MAX_FRAME_LEN`].
//! The connection opens with [`VIDEO_MAGIC`] so the server can distinguish it
//! from a control connection on the first read, then carries length-prefixed
//! records: one clear [`VideoHello`], one sealed auth record, a sealed
//! [`VideoAck`], and a stream of sealed frames.
//!
//! Two framing layers ride the wire:
//! - Outer: a u32 length prefix per record ([`write_record`]/[`parse_record`]),
//!   capped at [`MAX_VIDEO_FRAME_LEN`].
//! - Inner (a frame record's sealed plaintext): the [`write_video_frame`] bytes,
//!   a 17-byte header (`[u32 size_incl_header][i64 ts_ms][u8 is_key][u32
//!   stream_id]`) followed by the length-prefixed video bitstream. This is
//!   byte-identical to what the browser decoder expects, so the client forwards
//!   a decrypted frame body to the browser without re-framing. The `stream_id`
//!   lets a browser route each frame to its per-stream decoder so it can watch
//!   several shares at once.
//!
//! [`MAX_FRAME_LEN`]: crate::frame::MAX_FRAME_LEN

use jsony::Jsony;

use crate::ids::{SessionId, StreamId};

/// Preamble that distinguishes a video connection from a control connection. Its
/// first four bytes (`0x56544843`) read as a little-endian length far above
/// [`crate::frame::MAX_FRAME_LEN`], so a control hello can never collide with it.
pub const VIDEO_MAGIC: [u8; 8] = *b"CHTVID01";

/// Length of the per-frame header:
/// `[u32 size_incl_header][i64 ts_ms][u8 is_key][u32 stream_id]`.
pub const VIDEO_FRAME_HEADER_LEN: usize = 17;

/// Length prefix on each outer wire record.
pub const VIDEO_LENGTH_PREFIX_LEN: usize = 4;

/// Cap on one outer record. A keyframe must fit a single record, and this stays
/// below the browser's default `max_websocket_payload` (16 MiB).
pub const MAX_VIDEO_FRAME_LEN: usize = 8 * 1024 * 1024;

/// Bound on a clear [`VideoHello`] payload, which is small and fixed-shape.
pub const MAX_VIDEO_HELLO_BYTES: usize = 4 * 1024;

/// The role a video connection plays, carried in the clear [`VideoHello`] so the
/// server can pick the matching per-stream secret and fan-out behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum VideoRole {
    Publisher,
    Subscriber,
}

/// The first record on a video connection, sent in the clear. The server needs
/// `stream_id` to look up the stream's secret before any sealed record arrives.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct VideoHello {
    pub version: u16,
    pub session_id: SessionId,
    pub stream_id: StreamId,
    pub role: VideoRole,
}

/// The server's sealed reply once it has opened the connection's auth record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum VideoAck {
    Ok,
    Rejected,
}

/// One decoded inner frame: its presentation timestamp, keyframe flag, source
/// stream id, and the length-prefixed video bitstream body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub ts_ms: i64,
    pub is_key: bool,
    pub stream_id: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFrameError {
    TooLarge,
    LengthOverflow,
    BadHeader,
}

impl std::fmt::Display for VideoFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoFrameError::TooLarge => f.write_str("video record exceeds maximum length"),
            VideoFrameError::LengthOverflow => {
                f.write_str("video record length does not fit in u32")
            }
            VideoFrameError::BadHeader => f.write_str("video frame header is malformed"),
        }
    }
}

impl std::error::Error for VideoFrameError {}

/// Writes one outer record: a u32 little-endian length prefix then `payload`.
pub fn write_record(out: &mut Vec<u8>, payload: &[u8]) -> Result<(), VideoFrameError> {
    if payload.len() > MAX_VIDEO_FRAME_LEN {
        return Err(VideoFrameError::TooLarge);
    }
    let len = u32::try_from(payload.len()).map_err(|_| VideoFrameError::LengthOverflow)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Parses the outer record at the front of `buffer` without consuming or
/// copying it, returning the payload as a borrowed slice and the total number
/// of bytes the record occupies. Video records can be large, so consumers
/// decrypt or relay straight from the borrowed slice and advance a cursor by
/// the returned length instead of draining per record.
pub fn parse_record(buffer: &[u8]) -> Result<Option<(&[u8], usize)>, VideoFrameError> {
    let Some(prefix) = buffer.get(..VIDEO_LENGTH_PREFIX_LEN) else {
        return Ok(None);
    };
    let len = u32::from_le_bytes(prefix.try_into().unwrap()) as usize;
    if len > MAX_VIDEO_FRAME_LEN {
        return Err(VideoFrameError::TooLarge);
    }
    let total = VIDEO_LENGTH_PREFIX_LEN + len;
    let Some(payload) = buffer.get(VIDEO_LENGTH_PREFIX_LEN..total) else {
        return Ok(None);
    };
    Ok(Some((payload, total)))
}

/// Writes one inner frame record: the 17-byte header then the bitstream body.
/// The size field counts the header, matching the browser-side reader.
pub fn write_video_frame(out: &mut Vec<u8>, ts_ms: i64, is_key: bool, stream_id: u32, body: &[u8]) {
    let size = (body.len() + VIDEO_FRAME_HEADER_LEN) as u32;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&ts_ms.to_le_bytes());
    out.push(u8::from(is_key));
    out.extend_from_slice(&stream_id.to_le_bytes());
    out.extend_from_slice(body);
}

/// Encodes one inner frame into a fresh buffer.
pub fn encode_video_frame(ts_ms: i64, is_key: bool, stream_id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(VIDEO_FRAME_HEADER_LEN + body.len());
    write_video_frame(&mut out, ts_ms, is_key, stream_id, body);
    out
}

/// Pops one inner frame from `buffer`, draining it. Returns `None` when fewer
/// than one whole frame is buffered.
pub fn pop_video_frame(buffer: &mut Vec<u8>) -> Result<Option<Frame>, VideoFrameError> {
    if buffer.len() < VIDEO_FRAME_HEADER_LEN {
        return Ok(None);
    }
    let size = u32::from_le_bytes(buffer[0..4].try_into().unwrap()) as usize;
    if size < VIDEO_FRAME_HEADER_LEN {
        return Err(VideoFrameError::BadHeader);
    }
    if size > MAX_VIDEO_FRAME_LEN {
        return Err(VideoFrameError::TooLarge);
    }
    if buffer.len() < size {
        return Ok(None);
    }
    let ts_ms = i64::from_le_bytes(buffer[4..12].try_into().unwrap());
    let is_key = buffer[12] == 1;
    let stream_id = u32::from_le_bytes(buffer[13..17].try_into().unwrap());
    let data = buffer[VIDEO_FRAME_HEADER_LEN..size].to_vec();
    buffer.drain(..size);
    Ok(Some(Frame {
        ts_ms,
        is_key,
        stream_id,
        data,
    }))
}

pub fn encode_video_hello(value: &VideoHello) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_video_hello(bytes: &[u8]) -> Result<VideoHello, String> {
    if bytes.len() > MAX_VIDEO_HELLO_BYTES {
        return Err("video hello exceeds maximum length".to_string());
    }
    jsony::from_binary(bytes).map_err(|error| error.to_string())
}

pub fn encode_video_ack(value: &VideoAck) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_video_ack(bytes: &[u8]) -> Result<VideoAck, String> {
    if bytes.len() > MAX_VIDEO_HELLO_BYTES {
        return Err("video ack exceeds maximum length".to_string());
    }
    jsony::from_binary(bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_frame_round_trips() {
        let mut buffer = Vec::new();
        write_video_frame(&mut buffer, 33, true, 7, &[1, 2, 3, 4]);
        let frame = pop_video_frame(&mut buffer).unwrap().unwrap();
        assert_eq!(frame.ts_ms, 33);
        assert!(frame.is_key);
        assert_eq!(frame.stream_id, 7);
        assert_eq!(frame.data, vec![1, 2, 3, 4]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn pop_video_frame_waits_for_whole_frame() {
        let mut full = Vec::new();
        write_video_frame(&mut full, 0, false, 1, &[9; 16]);
        let mut partial = full[..full.len() - 1].to_vec();
        assert_eq!(pop_video_frame(&mut partial).unwrap(), None);
    }

    #[test]
    fn video_record_round_trips() {
        let mut buffer = Vec::new();
        write_record(&mut buffer, b"sealed-record").unwrap();
        let (payload, consumed) = parse_record(&buffer).unwrap().expect("whole record");
        assert_eq!(payload, b"sealed-record".as_slice());
        assert_eq!(consumed, buffer.len());
    }

    #[test]
    fn video_hello_round_trips() {
        let hello = VideoHello {
            version: crate::PROTOCOL_VERSION,
            session_id: SessionId(7),
            stream_id: StreamId(42),
            role: VideoRole::Subscriber,
        };
        let encoded = encode_video_hello(&hello);
        assert_eq!(decode_video_hello(&encoded).unwrap(), hello);
    }

    #[test]
    fn video_ack_round_trips() {
        let encoded = encode_video_ack(&VideoAck::Ok);
        assert_eq!(decode_video_ack(&encoded).unwrap(), VideoAck::Ok);
    }

    #[test]
    fn rejects_oversized_video_frame() {
        let mut buffer = Vec::new();
        let size = (MAX_VIDEO_FRAME_LEN + 1) as u32;
        buffer.extend_from_slice(&size.to_le_bytes());
        buffer.extend_from_slice(&[0u8; VIDEO_FRAME_HEADER_LEN]);
        assert_eq!(pop_video_frame(&mut buffer), Err(VideoFrameError::TooLarge));
    }

    #[test]
    fn video_magic_first_word_exceeds_control_frame_cap() {
        let word = u32::from_le_bytes(VIDEO_MAGIC[0..4].try_into().unwrap()) as usize;
        assert!(word > crate::frame::MAX_FRAME_LEN);
    }
}
