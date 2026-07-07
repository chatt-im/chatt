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

use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use jsony::Jsony;

use crate::crypto::{self, AUTH_PROOF_LEN, TAG_LEN, TRANSPORT_HEADER_LEN, TransportMode};
use crate::evented::{ReadPumpOutcome, Readiness};
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

impl VideoRole {
    fn proof_byte(self) -> u8 {
        match self {
            VideoRole::Publisher => 0,
            VideoRole::Subscriber => 1,
        }
    }
}

/// Domain-separated message the external-link video auth proof covers, binding
/// the connection's identity, role, and transport mode so a proof issued for one
/// stream/role/session cannot be reused for another.
fn video_auth_message(
    session_id: SessionId,
    stream_id: StreamId,
    role: VideoRole,
    mode: TransportMode,
) -> [u8; 39] {
    let mut msg = [0u8; 39];
    msg[0..25].copy_from_slice(b"chatt video auth proof v1");
    msg[25..33].copy_from_slice(&session_id.0.to_le_bytes());
    msg[33..37].copy_from_slice(&stream_id.0.to_le_bytes());
    msg[37] = role.proof_byte();
    msg[38] = mode.wire_id();
    msg
}

/// Computes the external-link video auth proof: a truncated HMAC under the
/// session's video auth key proving the connecting peer completed the session
/// handshake. In native-encrypted mode possession is proven by opening the AEAD
/// auth record instead, so this is unused there.
pub fn video_auth_proof(
    video_auth_key: &[u8],
    session_id: SessionId,
    stream_id: StreamId,
    role: VideoRole,
    mode: TransportMode,
) -> [u8; AUTH_PROOF_LEN] {
    crypto::auth_proof(
        video_auth_key,
        &video_auth_message(session_id, stream_id, role, mode),
    )
}

/// Verifies a [`video_auth_proof`] in constant time.
pub fn video_auth_proof_verify(
    video_auth_key: &[u8],
    session_id: SessionId,
    stream_id: StreamId,
    role: VideoRole,
    mode: TransportMode,
    tag: &[u8],
) -> bool {
    crypto::auth_proof_verify(
        video_auth_key,
        &video_auth_message(session_id, stream_id, role, mode),
        tag,
    )
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

/// Times a [`SharedVideoFrame`] constructor copied its input to a fresh exact
/// allocation because the offered backing would retain more than crypto
/// overhead. A nonzero rate under load means a zero-copy path regressed.
static COPY_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// One shared immutable plaintext video frame, retained in fast-start caches
/// and fanned out to subscribers without copying.
///
/// The frame is a window into a shared backing allocation, normally the exact
/// sealed-record buffer the frame arrived in, so retaining the plaintext also
/// retains only the crypto header and tag around it. [`Self::retained_bytes`]
/// reads the backing allocation's live capacity, so ring accounting counts
/// what is actually held rather than what is visible. Constructors refuse to
/// retain oversized backings: anything holding more than crypto overhead
/// beyond the visible frame is copied to an exact allocation instead, which
/// makes pinning a large reusable receive buffer through a small frame slice
/// impossible by construction.
#[derive(Clone, Debug)]
pub struct SharedVideoFrame {
    buf: Arc<Vec<u8>>,
    off: usize,
    len: usize,
}

impl SharedVideoFrame {
    /// The visible frame bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[self.off..self.off + self.len]
    }

    /// Length of the visible frame.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the visible frame is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes of allocation kept alive by this frame, read live from the
    /// backing buffer's capacity. Memory budgets must charge this, not
    /// [`Self::len`].
    pub fn retained_bytes(&self) -> usize {
        self.buf.capacity()
    }

    /// Copies `slice` into a fresh exact allocation.
    pub fn copy_from_slice(slice: &[u8]) -> Self {
        let mut buf = Vec::with_capacity(slice.len());
        buf.extend_from_slice(slice);
        Self {
            buf: Arc::new(buf),
            off: 0,
            len: slice.len(),
        }
    }

    /// Wraps one whole received record allocation, exposing the plaintext
    /// window at `off..off + len` without copying.
    ///
    /// Zero-copy retention requires an exact backing: `buf` must have no spare
    /// capacity and hold at most the transport header and tag beyond the
    /// visible frame. A backing that would retain more is copied to an exact
    /// allocation and counted in [`copy_fallbacks`].
    ///
    /// # Panics
    ///
    /// Panics when `off + len` exceeds `buf.len()`.
    pub fn from_record(buf: Vec<u8>, off: usize, len: usize) -> Self {
        assert!(off + len <= buf.len(), "frame window exceeds record");
        let exact = buf.capacity() == buf.len();
        let overhead_bounded = buf.len() <= len + TRANSPORT_HEADER_LEN + TAG_LEN;
        if exact && overhead_bounded {
            return Self {
                buf: Arc::new(buf),
                off,
                len,
            };
        }
        COPY_FALLBACKS.fetch_add(1, Ordering::Relaxed);
        Self::copy_from_slice(&buf[off..off + len])
    }
}

/// Total [`SharedVideoFrame`] constructor copy fallbacks since process start.
pub fn copy_fallbacks() -> u64 {
    COPY_FALLBACKS.load(Ordering::Relaxed)
}

/// Reads outer video records into per-record exact allocations, the safe
/// source for zero-copy [`SharedVideoFrame`] retention.
///
/// The length prefix lands in a stack array and the sealed body in a `Vec`
/// reserved to exactly the record length, and a socket read never asks for
/// more than the current record still needs, so no read ever buffers bytes of
/// a following record. A completed body pops out through
/// [`Self::take_record`]; retaining its plaintext window retains only the
/// transport header and tag around it.
pub struct VideoRecordReader {
    prefix: [u8; VIDEO_LENGTH_PREFIX_LEN],
    prefix_len: usize,
    body: Vec<u8>,
    /// Sealed body length decoded from the prefix, `None` until the prefix
    /// completes.
    target: Option<usize>,
}

impl VideoRecordReader {
    pub fn new() -> Self {
        Self {
            prefix: [0; VIDEO_LENGTH_PREFIX_LEN],
            prefix_len: 0,
            body: Vec::new(),
            target: None,
        }
    }

    /// Whether a whole sealed record body is buffered and
    /// [`Self::take_record`] would return it.
    pub fn record_ready(&self) -> bool {
        self.target.is_some_and(|target| self.body.len() == target)
    }

    /// Detaches the completed record's exact allocation and resets for the
    /// next record, or `None` while the current record is incomplete.
    pub fn take_record(&mut self) -> Option<Vec<u8>> {
        if !self.record_ready() {
            return None;
        }
        self.target = None;
        self.prefix_len = 0;
        Some(std::mem::take(&mut self.body))
    }

    /// Consumes already-buffered bytes, taking exactly what the current
    /// record still needs and returning the count taken. Used to hand a
    /// receive buffer's residual bytes (read before this reader attached)
    /// over to exact per-record storage; call in a loop with
    /// [`Self::take_record`] until the residual drains. Returns zero once a
    /// record is ready.
    pub fn accept(&mut self, input: &[u8]) -> Result<usize, VideoFrameError> {
        if self.record_ready() || input.is_empty() {
            return Ok(0);
        }
        if self.prefix_len < VIDEO_LENGTH_PREFIX_LEN {
            let take = input.len().min(VIDEO_LENGTH_PREFIX_LEN - self.prefix_len);
            self.prefix[self.prefix_len..self.prefix_len + take].copy_from_slice(&input[..take]);
            self.prefix_len += take;
            if self.prefix_len == VIDEO_LENGTH_PREFIX_LEN {
                self.begin_body()?;
            }
            return Ok(take);
        }
        let target = self.target.expect("complete prefix implies target");
        let take = input.len().min(target - self.body.len());
        self.body.extend_from_slice(&input[..take]);
        Ok(take)
    }

    /// Reads once from `socket` toward the current record: the rest of the
    /// length prefix or the rest of the sealed body, capped at `max_read`.
    /// Returns the bytes read; `Ok(0)` is end-of-stream, a would-block
    /// surfaces as [`io::ErrorKind::WouldBlock`], and an over-length prefix
    /// as [`io::ErrorKind::InvalidData`]. Must not be called while a
    /// completed record is waiting in the reader.
    pub fn fill(&mut self, socket: &impl AsRawFd, max_read: usize) -> io::Result<usize> {
        debug_assert!(max_read > 0);
        debug_assert!(!self.record_ready());
        if self.prefix_len < VIDEO_LENGTH_PREFIX_LEN {
            let want = (VIDEO_LENGTH_PREFIX_LEN - self.prefix_len).min(max_read);
            let read = read_fd(socket, self.prefix[self.prefix_len..].as_mut_ptr(), want)?;
            self.prefix_len += read;
            if self.prefix_len == VIDEO_LENGTH_PREFIX_LEN {
                self.begin_body()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            }
            return Ok(read);
        }
        let target = self.target.expect("complete prefix implies target");
        let len = self.body.len();
        let want = (target - len).min(max_read);
        let read = read_fd(socket, unsafe { self.body.as_mut_ptr().add(len) }, want)?;
        unsafe { self.body.set_len(len + read) };
        Ok(read)
    }

    fn begin_body(&mut self) -> Result<(), VideoFrameError> {
        let len = u32::from_le_bytes(self.prefix) as usize;
        if len > MAX_VIDEO_FRAME_LEN {
            return Err(VideoFrameError::TooLarge);
        }
        debug_assert!(self.body.is_empty());
        self.body.reserve_exact(len);
        self.target = Some(len);
        Ok(())
    }
}

impl Default for VideoRecordReader {
    fn default() -> Self {
        Self::new()
    }
}

fn read_fd(socket: &impl AsRawFd, ptr: *mut u8, want: usize) -> io::Result<usize> {
    loop {
        let read = unsafe { libc::read(socket.as_raw_fd(), ptr.cast(), want) };
        if read >= 0 {
            return Ok(read as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Budgeted socket pump for one [`VideoRecordReader`]: reads until the
/// current record completes, the socket drains, end-of-stream, or
/// `byte_budget` bytes land. Follows the same retained-readiness contract as
/// [`crate::evented::read_into_buffer`]: readiness is cleared only on a
/// would-block or end-of-stream, and `hit_limit` reports a budget stop that
/// must be requeued rather than waiting for a new edge.
pub fn read_video_record(
    socket: &impl AsRawFd,
    reader: &mut VideoRecordReader,
    readiness: &mut Readiness,
    byte_budget: usize,
) -> io::Result<ReadPumpOutcome> {
    let mut outcome = ReadPumpOutcome::default();
    while readiness.is_ready() && !reader.record_ready() && outcome.bytes_read < byte_budget {
        match reader.fill(socket, byte_budget - outcome.bytes_read) {
            Ok(0) => {
                readiness.mark_drained();
                outcome.disconnected = true;
                break;
            }
            Ok(read) => outcome.bytes_read += read,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                readiness.mark_drained();
                break;
            }
            Err(error) => return Err(error),
        }
    }
    if readiness.is_ready() && !reader.record_ready() && outcome.bytes_read >= byte_budget {
        outcome.hit_limit = true;
    }
    Ok(outcome)
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
    fn video_auth_proof_binds_identity_and_role() {
        let key = [4u8; crate::crypto::KEY_LEN];
        let mode = TransportMode::ExternalSecureLink;
        let proof = video_auth_proof(&key, SessionId(7), StreamId(3), VideoRole::Publisher, mode);
        assert!(video_auth_proof_verify(
            &key,
            SessionId(7),
            StreamId(3),
            VideoRole::Publisher,
            mode,
            &proof
        ));
        // A proof for one connection must not verify for a different role,
        // stream, session, key, or transport mode.
        assert!(!video_auth_proof_verify(
            &key,
            SessionId(7),
            StreamId(3),
            VideoRole::Subscriber,
            mode,
            &proof
        ));
        assert!(!video_auth_proof_verify(
            &key,
            SessionId(8),
            StreamId(3),
            VideoRole::Publisher,
            mode,
            &proof
        ));
        assert!(!video_auth_proof_verify(
            &key,
            SessionId(7),
            StreamId(4),
            VideoRole::Publisher,
            mode,
            &proof
        ));
        assert!(!video_auth_proof_verify(
            &[5u8; crate::crypto::KEY_LEN],
            SessionId(7),
            StreamId(3),
            VideoRole::Publisher,
            mode,
            &proof
        ));
        assert!(!video_auth_proof_verify(
            &key,
            SessionId(7),
            StreamId(3),
            VideoRole::Publisher,
            TransportMode::NativeEncrypted,
            &proof
        ));
    }

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
    fn shared_frame_retains_exact_record_without_copy() {
        let mut record = Vec::with_capacity(TRANSPORT_HEADER_LEN + 5 + TAG_LEN);
        record.extend_from_slice(&[0u8; TRANSPORT_HEADER_LEN]);
        record.extend_from_slice(b"frame");
        record.extend_from_slice(&[0u8; TAG_LEN]);
        let backing = record.as_ptr();
        let frame = SharedVideoFrame::from_record(record, TRANSPORT_HEADER_LEN, 5);
        assert_eq!(frame.as_slice(), b"frame");
        assert_eq!(frame.len(), 5);
        assert_eq!(frame.retained_bytes(), TRANSPORT_HEADER_LEN + 5 + TAG_LEN);
        // The window points into the original allocation: no copy happened.
        assert_eq!(frame.as_slice().as_ptr(), unsafe {
            backing.add(TRANSPORT_HEADER_LEN)
        });
    }

    #[test]
    fn shared_frame_copies_out_of_oversized_backing() {
        let mut record = Vec::with_capacity(64 * 1024);
        record.extend_from_slice(&[0u8; TRANSPORT_HEADER_LEN]);
        record.extend_from_slice(b"frame");
        record.extend_from_slice(&[0u8; TAG_LEN]);
        let fallbacks = copy_fallbacks();
        let frame = SharedVideoFrame::from_record(record, TRANSPORT_HEADER_LEN, 5);
        assert_eq!(frame.as_slice(), b"frame");
        assert_eq!(frame.retained_bytes(), 5);
        // Other tests may bump the shared counter concurrently.
        assert!(copy_fallbacks() > fallbacks);
    }

    #[test]
    fn shared_frame_copies_when_record_holds_extra_payload() {
        let mut record = Vec::with_capacity(TRANSPORT_HEADER_LEN + 500 + TAG_LEN);
        record.resize(record.capacity(), 7);
        let fallbacks = copy_fallbacks();
        let frame = SharedVideoFrame::from_record(record, TRANSPORT_HEADER_LEN, 5);
        assert_eq!(frame.retained_bytes(), 5);
        assert!(copy_fallbacks() > fallbacks);
    }

    #[test]
    fn shared_frame_clone_shares_the_backing_allocation() {
        let frame = SharedVideoFrame::copy_from_slice(b"frame");
        let clone = frame.clone();
        assert_eq!(clone.as_slice().as_ptr(), frame.as_slice().as_ptr());
        assert_eq!(clone.retained_bytes(), frame.retained_bytes());
    }

    #[test]
    fn record_reader_assembles_record_across_partial_reads() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let mut record = Vec::new();
        write_record(&mut record, b"sealed-record-body").unwrap();

        let (mut writer, reader_sock) = UnixStream::pair().unwrap();
        writer.write_all(&record).unwrap();
        let mut reader = VideoRecordReader::new();

        // A 3-byte read cap forces partial prefix and partial body reads.
        let mut fills = 0;
        while !reader.record_ready() {
            assert!(reader.fill(&reader_sock, 3).unwrap() > 0);
            fills += 1;
        }
        assert!(fills > 2);
        let body = reader.take_record().unwrap();
        assert_eq!(body, b"sealed-record-body");
        assert_eq!(body.capacity(), body.len());
    }

    #[test]
    fn record_reader_never_reads_past_the_current_record() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let mut records = Vec::new();
        write_record(&mut records, b"first-record").unwrap();
        write_record(&mut records, b"second").unwrap();

        let (mut writer, reader_sock) = UnixStream::pair().unwrap();
        writer.write_all(&records).unwrap();
        let mut reader = VideoRecordReader::new();

        while !reader.record_ready() {
            reader.fill(&reader_sock, 64 * 1024).unwrap();
        }
        let first = reader.take_record().unwrap();
        assert_eq!(first, b"first-record");
        assert_eq!(first.capacity(), first.len());

        while !reader.record_ready() {
            reader.fill(&reader_sock, 64 * 1024).unwrap();
        }
        let second = reader.take_record().unwrap();
        assert_eq!(second, b"second");
        assert_eq!(second.capacity(), second.len());
        assert_ne!(first.as_ptr(), second.as_ptr());
    }

    #[test]
    fn record_reader_rejects_oversized_record_length() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let (mut writer, reader_sock) = UnixStream::pair().unwrap();
        let len = (MAX_VIDEO_FRAME_LEN + 1) as u32;
        writer.write_all(&len.to_le_bytes()).unwrap();
        let mut reader = VideoRecordReader::new();

        let mut result = Ok(0);
        for _ in 0..VIDEO_LENGTH_PREFIX_LEN {
            result = reader.fill(&reader_sock, 64 * 1024);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn record_reader_accepts_buffered_residual_across_records() {
        let mut residual = Vec::new();
        write_record(&mut residual, b"one").unwrap();
        write_record(&mut residual, b"two-longer").unwrap();

        let mut reader = VideoRecordReader::new();
        let mut offset = 0;
        let mut bodies = Vec::new();
        while offset < residual.len() {
            let taken = reader.accept(&residual[offset..]).unwrap();
            offset += taken;
            if let Some(body) = reader.take_record() {
                assert_eq!(body.capacity(), body.len());
                bodies.push(body);
            } else {
                assert!(taken > 0, "no progress on remaining residual");
            }
        }
        assert_eq!(reader.take_record(), None);
        assert_eq!(bodies, vec![b"one".to_vec(), b"two-longer".to_vec()]);
    }

    #[test]
    fn record_reader_pump_stops_at_budget_and_keeps_readiness() {
        use crate::evented::Readiness;
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let mut record = Vec::new();
        write_record(&mut record, &[7u8; 32]).unwrap();

        let (mut writer, reader_sock) = UnixStream::pair().unwrap();
        reader_sock.set_nonblocking(true).unwrap();
        writer.write_all(&record).unwrap();
        let mut reader = VideoRecordReader::new();
        let mut readiness = Readiness::primed();

        let outcome = read_video_record(&reader_sock, &mut reader, &mut readiness, 10).unwrap();
        assert_eq!(outcome.bytes_read, 10);
        assert!(outcome.hit_limit);
        assert!(readiness.is_ready());
        assert!(!reader.record_ready());

        let outcome =
            read_video_record(&reader_sock, &mut reader, &mut readiness, 64 * 1024).unwrap();
        assert!(!outcome.hit_limit);
        assert!(reader.record_ready());
        assert_eq!(reader.take_record().unwrap(), vec![7u8; 32]);
        assert!(readiness.is_ready());

        let outcome =
            read_video_record(&reader_sock, &mut reader, &mut readiness, 64 * 1024).unwrap();
        assert_eq!(outcome.bytes_read, 0);
        assert!(!readiness.is_ready());
    }

    #[test]
    fn video_magic_first_word_exceeds_control_frame_cap() {
        let word = u32::from_le_bytes(VIDEO_MAGIC[0..4].try_into().unwrap()) as usize;
        assert!(word > crate::frame::MAX_FRAME_LEN);
    }
}
