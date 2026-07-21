//! Plaintext encoded-video frames shared by remote RPC and local renderers.
//!
//! Each frame is a 17-byte header followed by codec bitstream bytes. Parsing
//! borrows the bitstream directly, allowing the daemon to relay decrypted
//! frames to a renderer without copying or reframing them.

/// Length of the per-frame header:
/// `[u32 size_incl_header][i64 ts_ms][u8 flags][u32 stream_id]`.
pub const VIDEO_FRAME_HEADER_LEN: usize = 17;

const VIDEO_FRAME_FLAG_KEY: u8 = 1;
const VIDEO_FRAME_FLAG_BOOTSTRAP_END: u8 = 2;

/// Maximum size of one encoded video frame.
pub const MAX_VIDEO_FRAME_LEN: usize = 8 * 1024 * 1024;

/// Metadata decoded from the fixed portion of one plaintext video frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFrameHeader {
    /// Total frame length, including [`VIDEO_FRAME_HEADER_LEN`].
    pub size: usize,
    pub ts_ms: i64,
    pub is_key: bool,
    /// Marks the ordered boundary between a native subscriber's cached GOP
    /// and subsequent live frames. It has no bitstream body.
    pub bootstrap_end: bool,
    pub stream_id: u32,
}

/// A borrowed plaintext video frame. Parsing never copies the bitstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFrame<'a> {
    pub ts_ms: i64,
    pub is_key: bool,
    pub bootstrap_end: bool,
    pub stream_id: u32,
    pub data: &'a [u8],
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

/// Writes one plaintext frame: the 17-byte header then the bitstream body.
/// The size field counts the header, matching the browser-side reader.
pub fn write_video_frame(out: &mut Vec<u8>, ts_ms: i64, is_key: bool, stream_id: u32, body: &[u8]) {
    let size = (body.len() + VIDEO_FRAME_HEADER_LEN) as u32;
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&ts_ms.to_le_bytes());
    out.push(if is_key { VIDEO_FRAME_FLAG_KEY } else { 0 });
    out.extend_from_slice(&stream_id.to_le_bytes());
    out.extend_from_slice(body);
}

/// Writes the ordered boundary between the cached GOP seeded to a native
/// subscriber and the live frames that follow it.
pub fn write_video_bootstrap_end(out: &mut Vec<u8>, stream_id: u32) {
    out.extend_from_slice(&(VIDEO_FRAME_HEADER_LEN as u32).to_le_bytes());
    out.extend_from_slice(&0i64.to_le_bytes());
    out.push(VIDEO_FRAME_FLAG_BOOTSTRAP_END);
    out.extend_from_slice(&stream_id.to_le_bytes());
}

/// Encodes a native subscriber bootstrap boundary into a fresh buffer.
pub fn encode_video_bootstrap_end(stream_id: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(VIDEO_FRAME_HEADER_LEN);
    write_video_bootstrap_end(&mut out, stream_id);
    out
}

/// Encodes one plaintext frame into a fresh buffer.
pub fn encode_video_frame(ts_ms: i64, is_key: bool, stream_id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(VIDEO_FRAME_HEADER_LEN + body.len());
    write_video_frame(&mut out, ts_ms, is_key, stream_id, body);
    out
}

/// Parses the fixed header at the front of `buffer`, returning `None` until all
/// [`VIDEO_FRAME_HEADER_LEN`] bytes are available.
pub fn parse_video_frame_header(
    buffer: &[u8],
) -> Result<Option<VideoFrameHeader>, VideoFrameError> {
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
    let (is_key, bootstrap_end) = match buffer[12] {
        0 => (false, false),
        VIDEO_FRAME_FLAG_KEY => (true, false),
        VIDEO_FRAME_FLAG_BOOTSTRAP_END if size == VIDEO_FRAME_HEADER_LEN => (false, true),
        _ => return Err(VideoFrameError::BadHeader),
    };
    Ok(Some(VideoFrameHeader {
        size,
        ts_ms: i64::from_le_bytes(buffer[4..12].try_into().unwrap()),
        is_key,
        bootstrap_end,
        stream_id: u32::from_le_bytes(buffer[13..17].try_into().unwrap()),
    }))
}

/// Parses one complete frame from the front of `buffer` without copying,
/// returning the borrowed frame and the number of bytes it occupies.
pub fn parse_video_frame(
    buffer: &[u8],
) -> Result<Option<(VideoFrame<'_>, usize)>, VideoFrameError> {
    let Some(header) = parse_video_frame_header(buffer)? else {
        return Ok(None);
    };
    if buffer.len() < header.size {
        return Ok(None);
    }
    Ok(Some((
        VideoFrame {
            ts_ms: header.ts_ms,
            is_key: header.is_key,
            bootstrap_end: header.bootstrap_end,
            stream_id: header.stream_id,
            data: &buffer[VIDEO_FRAME_HEADER_LEN..header.size],
        },
        header.size,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_frame_round_trips_without_copying_body() {
        let mut buffer = Vec::new();
        write_video_frame(&mut buffer, 33, true, 7, &[1, 2, 3, 4]);
        let body_ptr = buffer[VIDEO_FRAME_HEADER_LEN..].as_ptr();
        let (frame, consumed) = parse_video_frame(&buffer).unwrap().unwrap();
        assert_eq!(frame.ts_ms, 33);
        assert!(frame.is_key);
        assert!(!frame.bootstrap_end);
        assert_eq!(frame.stream_id, 7);
        assert_eq!(frame.data, [1, 2, 3, 4]);
        assert_eq!(frame.data.as_ptr(), body_ptr);
        assert_eq!(consumed, buffer.len());
    }

    #[test]
    fn video_bootstrap_boundary_round_trips() {
        let buffer = encode_video_bootstrap_end(7);
        let (frame, consumed) = parse_video_frame(&buffer).unwrap().unwrap();
        assert_eq!(frame.ts_ms, 0);
        assert!(!frame.is_key);
        assert!(frame.bootstrap_end);
        assert_eq!(frame.stream_id, 7);
        assert!(frame.data.is_empty());
        assert_eq!(consumed, buffer.len());
    }

    #[test]
    fn parse_video_frame_waits_for_whole_frame() {
        let mut full = Vec::new();
        write_video_frame(&mut full, 0, false, 1, &[9; 16]);
        assert_eq!(parse_video_frame(&full[..full.len() - 1]).unwrap(), None);
    }

    #[test]
    fn rejects_oversized_video_frame() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&((MAX_VIDEO_FRAME_LEN + 1) as u32).to_le_bytes());
        buffer.extend_from_slice(&[0u8; VIDEO_FRAME_HEADER_LEN]);
        assert_eq!(parse_video_frame(&buffer), Err(VideoFrameError::TooLarge));
    }

    #[test]
    fn rejects_invalid_video_key_flag() {
        let mut buffer = encode_video_frame(0, false, 1, b"body");
        buffer[12] = 3;
        assert_eq!(parse_video_frame(&buffer), Err(VideoFrameError::BadHeader));
    }
}
