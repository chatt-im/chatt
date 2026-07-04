//! Minimal incremental NUT container demuxer for the capture pipe.
//!
//! Raw Annex-B has no frame-size framing, so a splitter can only emit an access
//! unit once the next frame's start code arrives — with damage-driven capture
//! the latest frame sits buffered until the screen changes again. NUT frame
//! headers carry an explicit payload size, so every frame emits the moment its
//! bytes arrive. Payloads for H.264/HEVC are the encoder's Annex-B access units
//! verbatim, so downstream consumers of [`CapturedFrame`] are unchanged.
//!
//! The demuxer handles what `ffmpeg -f nut` (format version 3, default
//! syncpoints) produces for a video stream: the main header's frame-code table,
//! stream headers, syncpoints (which reset the delta-coded timestamp state),
//! and frames. Info and index packets are skipped via their forward pointer.
//! Checksums are skipped, not verified: the pipe is a local subprocess, and a
//! malformed stream fails the capture with a visible reason instead.

use rpc::bitstream::Codec;

use super::capture::{CapturedFrame, start_code_offsets};

/// The ID string every NUT stream begins with, used by the capture reader to
/// detect NUT output before the first packet.
pub const NUT_MAGIC: &[u8; 25] = b"nut/multimedia container\0";

const MAIN_STARTCODE: u64 = 0x4E4D7A561F5F04AD;
const STREAM_STARTCODE: u64 = 0x4E5311405BF2F9DB;
const SYNCPOINT_STARTCODE: u64 = 0x4E4BE4ADEECA4569;

const FLAG_KEY: u64 = 1;
const FLAG_EOR: u64 = 2;
const FLAG_CODED_PTS: u64 = 8;
const FLAG_STREAM_ID: u64 = 16;
const FLAG_SIZE_MSB: u64 = 32;
const FLAG_CHECKSUM: u64 = 64;
const FLAG_RESERVED: u64 = 128;
const FLAG_SM_DATA: u64 = 256;
const FLAG_HEADER_IDX: u64 = 1024;
const FLAG_MATCH_TIME: u64 = 2048;
const FLAG_CODED: u64 = 4096;
const FLAG_INVALID: u64 = 8192;

/// Caps on sizes decoded from the stream, so corrupt input fails instead of
/// buffering without bound waiting for bytes that never come.
const MAX_PACKET_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FRAME_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXTRADATA_BYTES: u64 = 1024 * 1024;

/// Why the NUT stream could not be demuxed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NutError {
    /// The stream violates the NUT syntax the demuxer understands.
    Malformed(&'static str),
    /// The video stream's fourcc does not match the selected codec.
    CodecMismatch,
}

/// A parse attempt either needs more buffered bytes or failed for good.
enum Fault {
    NeedMore,
    Fail(NutError),
}

type Parse<T> = Result<T, Fault>;

fn malformed<T>(what: &'static str) -> Parse<T> {
    Err(Fault::Fail(NutError::Malformed(what)))
}

/// A read cursor over buffered bytes. Reads past the end report
/// [`Fault::NeedMore`] so the caller retries once more bytes arrive; nothing is
/// consumed from the buffer until a whole item parses.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn peek(&self) -> Parse<u8> {
        match self.data.get(self.pos) {
            Some(byte) => Ok(*byte),
            None => Err(Fault::NeedMore),
        }
    }

    fn u8(&mut self) -> Parse<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Parse<&'a [u8]> {
        let Some(bytes) = self.data.get(self.pos..self.pos + len) else {
            return Err(Fault::NeedMore);
        };
        self.pos += len;
        Ok(bytes)
    }

    fn skip(&mut self, len: usize) -> Parse<()> {
        self.take(len)?;
        Ok(())
    }

    fn u64_be(&mut self) -> Parse<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    /// Reads a NUT variable-length unsigned value: big-endian base-128 groups,
    /// high bit set on every byte but the last.
    fn varlen(&mut self) -> Parse<u64> {
        let mut value: u64 = 0;
        loop {
            let byte = self.u8()?;
            if value >> 57 != 0 {
                return malformed("variable-length value overflows 64 bits");
            }
            value = (value << 7) | u64::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
    }

    /// Reads a NUT signed value: `v = varlen + 1`, odd maps to `-(v >> 1)`,
    /// even to `v >> 1` (so 0, 1, 2, 3 decode to 0, 1, -1, 2).
    fn signed(&mut self) -> Parse<i64> {
        let value = self.varlen()?;
        if value >= i64::MAX as u64 {
            return malformed("signed value overflows 64 bits");
        }
        let value = value as i64 + 1;
        if value & 1 == 1 {
            Ok(-(value >> 1))
        } else {
            Ok(value >> 1)
        }
    }
}

/// One reconstructed entry of the main header's 256-entry frame-code table.
#[derive(Clone, Copy)]
struct FrameCode {
    flags: u64,
    pts_delta: i64,
    stream_id: u64,
    size_mul: u64,
    size_lsb: u64,
    reserved_count: u64,
    header_idx: u64,
}

impl FrameCode {
    const INVALID: Self = Self {
        flags: FLAG_INVALID,
        pts_delta: 0,
        stream_id: 0,
        size_mul: 1,
        size_lsb: 0,
        reserved_count: 0,
        header_idx: 0,
    };
}

struct MainHeader {
    stream_count: usize,
    time_bases: Vec<(u64, u64)>,
    frame_codes: Vec<FrameCode>,
    elision_headers: Vec<Vec<u8>>,
}

struct StreamInfo {
    time_base: (u64, u64),
    msb_pts_shift: u32,
    extradata: Vec<u8>,
    last_pts: i64,
}

/// Incremental NUT demuxer: fed byte chunks as they arrive, it emits each video
/// frame as a [`CapturedFrame`] as soon as the frame's payload is complete.
pub struct NutDemuxer {
    codec: Codec,
    buf: Vec<u8>,
    seen_magic: bool,
    main: Option<MainHeader>,
    streams: Vec<Option<StreamInfo>>,
    emit_stream: Option<usize>,
}

impl NutDemuxer {
    pub fn new(codec: Codec) -> Self {
        Self {
            codec,
            buf: Vec::new(),
            seen_magic: false,
            main: None,
            streams: Vec::new(),
            emit_stream: None,
        }
    }

    /// Buffers `bytes` and parses as many complete items (headers, syncpoints,
    /// frames) as are available, pushing decoded frames onto `frames`.
    ///
    /// # Errors
    ///
    /// Returns [`NutError`] when the stream is malformed or its video stream
    /// does not match the selected codec. The demuxer is unusable afterwards.
    pub fn push(
        &mut self,
        bytes: &[u8],
        frames: &mut Vec<CapturedFrame>,
    ) -> Result<(), NutError> {
        self.buf.extend_from_slice(bytes);
        let mut consumed = 0;
        loop {
            let buf = std::mem::take(&mut self.buf);
            let mut cursor = Cursor::new(&buf[consumed..]);
            let result = self.parse_next(&mut cursor, frames);
            let advanced = cursor.pos;
            self.buf = buf;
            match result {
                Ok(()) => consumed += advanced,
                Err(Fault::NeedMore) => break,
                Err(Fault::Fail(error)) => return Err(error),
            }
        }
        self.buf.drain(..consumed);
        Ok(())
    }

    /// Parses one item from the head of the buffer: the ID string, one
    /// startcode-framed packet, or one frame. State only changes when the whole
    /// item is available, so a retry after more bytes arrive is safe.
    fn parse_next(
        &mut self,
        cursor: &mut Cursor<'_>,
        frames: &mut Vec<CapturedFrame>,
    ) -> Parse<()> {
        if !self.seen_magic {
            if cursor.take(NUT_MAGIC.len())? != NUT_MAGIC {
                return malformed("missing NUT ID string");
            }
            self.seen_magic = true;
            return Ok(());
        }
        if cursor.peek()? != b'N' {
            return self.parse_frame(cursor, frames);
        }
        let startcode = cursor.u64_be()?;
        let forward_ptr = cursor.varlen()?;
        if forward_ptr < 4 || forward_ptr > MAX_PACKET_BYTES {
            return malformed("packet forward pointer out of range");
        }
        if forward_ptr > 4096 {
            cursor.skip(4)?;
        }
        let packet = cursor.take(forward_ptr as usize)?;
        let payload = &packet[..packet.len() - 4];
        match startcode {
            MAIN_STARTCODE if self.main.is_none() => self.parse_main_header(payload),
            STREAM_STARTCODE => self.parse_stream_header(payload),
            SYNCPOINT_STARTCODE => self.apply_syncpoint(payload),
            _ => Ok(()),
        }
    }

    /// Parses the main header payload: version, counts, time bases, the
    /// run-length-coded frame-code table, and the elision header list.
    fn parse_main_header(&mut self, payload: &[u8]) -> Parse<()> {
        let mut cursor = Cursor::new(payload);
        let version = cursor.varlen()?;
        if !(2..=4).contains(&version) {
            return malformed("unsupported NUT version");
        }
        if version > 3 {
            cursor.varlen()?;
        }
        let stream_count = cursor.varlen()?;
        if !(1..=256).contains(&stream_count) {
            return malformed("stream count out of range");
        }
        let _max_distance = cursor.varlen()?;
        let time_base_count = cursor.varlen()?;
        if !(1..=1024).contains(&time_base_count) {
            return malformed("time base count out of range");
        }
        let mut time_bases = Vec::with_capacity(time_base_count as usize);
        for _ in 0..time_base_count {
            let num = cursor.varlen()?;
            let den = cursor.varlen()?;
            if num == 0 || den == 0 {
                return malformed("zero time base");
            }
            time_bases.push((num, den));
        }
        let frame_codes = parse_frame_code_table(&mut cursor, stream_count)?;
        let elision_count = cursor.varlen()? + 1;
        if elision_count > 128 {
            return malformed("elision header count out of range");
        }
        let mut elision_headers = vec![Vec::new()];
        for _ in 1..elision_count {
            let len = cursor.varlen()?;
            if len == 0 || len > 256 {
                return malformed("elision header length out of range");
            }
            elision_headers.push(cursor.take(len as usize)?.to_vec());
        }
        self.streams = std::iter::repeat_with(|| None)
            .take(stream_count as usize)
            .collect();
        self.main = Some(MainHeader {
            stream_count: stream_count as usize,
            time_bases,
            frame_codes,
            elision_headers,
        });
        Ok(())
    }

    /// Parses one stream header. The first video stream is validated against
    /// the selected codec's fourcc and becomes the stream whose frames emit;
    /// other streams are tracked only so their frames decode and skip cleanly.
    fn parse_stream_header(&mut self, payload: &[u8]) -> Parse<()> {
        let Some(main) = &self.main else {
            return malformed("stream header before main header");
        };
        let mut cursor = Cursor::new(payload);
        let stream_id = cursor.varlen()? as usize;
        if stream_id >= main.stream_count {
            return malformed("stream id out of range");
        }
        if self.streams[stream_id].is_some() {
            return Ok(());
        }
        let stream_class = cursor.varlen()?;
        let fourcc_len = cursor.varlen()?;
        if !(1..=8).contains(&fourcc_len) {
            return malformed("fourcc length out of range");
        }
        let fourcc = cursor.take(fourcc_len as usize)?.to_vec();
        let time_base_id = cursor.varlen()? as usize;
        let Some(&time_base) = main.time_bases.get(time_base_id) else {
            return malformed("stream time base out of range");
        };
        let msb_pts_shift = cursor.varlen()?;
        if msb_pts_shift >= 16 {
            return malformed("msb pts shift out of range");
        }
        let _max_pts_distance = cursor.varlen()?;
        let _decode_delay = cursor.varlen()?;
        let _stream_flags = cursor.varlen()?;
        let extradata_len = cursor.varlen()?;
        if extradata_len > MAX_EXTRADATA_BYTES {
            return malformed("extradata too large");
        }
        let extradata = cursor.take(extradata_len as usize)?.to_vec();
        if stream_class == 0 && self.emit_stream.is_none() {
            let expected: &[u8] = match self.codec {
                Codec::H264 => b"H264",
                Codec::Hevc => b"HEVC",
            };
            if fourcc != expected {
                return Err(Fault::Fail(NutError::CodecMismatch));
            }
            self.emit_stream = Some(stream_id);
        }
        self.streams[stream_id] = Some(StreamInfo {
            time_base,
            msb_pts_shift: msb_pts_shift as u32,
            extradata,
            last_pts: 0,
        });
        Ok(())
    }

    /// Applies a syncpoint: decodes its global timestamp and resets every
    /// stream's `last_pts` to it, which delta-coded frame timestamps require.
    fn apply_syncpoint(&mut self, payload: &[u8]) -> Parse<()> {
        let Some(main) = &self.main else {
            return malformed("syncpoint before main header");
        };
        let mut cursor = Cursor::new(payload);
        let coded = cursor.varlen()?;
        let _back_ptr = cursor.varlen()?;
        let count = main.time_bases.len() as u64;
        let time_base = main.time_bases[(coded % count) as usize];
        let ts = (coded / count) as i64;
        for stream in self.streams.iter_mut().flatten() {
            stream.last_pts = rescale(ts, time_base, stream.time_base);
        }
        Ok(())
    }

    /// Decodes one frame: header fields per its frame-code table entry, then
    /// exactly the coded payload size, emitting immediately with no lookahead.
    fn parse_frame(
        &mut self,
        cursor: &mut Cursor<'_>,
        frames: &mut Vec<CapturedFrame>,
    ) -> Parse<()> {
        let Some(main) = &self.main else {
            return malformed("frame before main header");
        };
        let code = cursor.u8()?;
        let entry = main.frame_codes[code as usize];
        let mut flags = entry.flags;
        if flags & FLAG_INVALID != 0 {
            return malformed("invalid frame code");
        }
        if flags & FLAG_CODED != 0 {
            flags ^= cursor.varlen()?;
        }
        let mut stream_id = entry.stream_id;
        if flags & FLAG_STREAM_ID != 0 {
            stream_id = cursor.varlen()?;
        }
        let stream_id = stream_id as usize;
        let Some(Some(stream)) = self.streams.get(stream_id) else {
            return malformed("frame for undeclared stream");
        };
        let pts = if flags & FLAG_CODED_PTS != 0 {
            let coded = cursor.varlen()?;
            let range = 1u64 << stream.msb_pts_shift;
            if coded < range {
                lsb_to_full_pts(stream.last_pts, coded as i64, stream.msb_pts_shift)
            } else {
                (coded - range) as i64
            }
        } else {
            stream.last_pts + entry.pts_delta
        };
        let mut size = entry.size_lsb;
        if flags & FLAG_SIZE_MSB != 0 {
            let msb = cursor.varlen()?;
            size = msb
                .checked_mul(entry.size_mul)
                .and_then(|scaled| scaled.checked_add(size))
                .ok_or(Fault::Fail(NutError::Malformed("frame size overflows")))?;
        }
        if flags & FLAG_MATCH_TIME != 0 {
            cursor.signed()?;
        }
        let mut header_idx = entry.header_idx;
        if flags & FLAG_HEADER_IDX != 0 {
            header_idx = cursor.varlen()?;
        }
        let mut reserved_count = entry.reserved_count;
        if flags & FLAG_RESERVED != 0 {
            reserved_count = cursor.varlen()?;
        }
        for _ in 0..reserved_count {
            cursor.varlen()?;
        }
        if flags & FLAG_SM_DATA != 0 {
            return malformed("unsupported NUT side data");
        }
        let header_idx = if size > 4096 { 0 } else { header_idx as usize };
        let Some(prefix) = main.elision_headers.get(header_idx) else {
            return malformed("elision header index out of range");
        };
        let Some(size) = size.checked_sub(prefix.len() as u64) else {
            return malformed("frame smaller than its elision header");
        };
        if size > MAX_FRAME_BYTES {
            return malformed("frame size out of range");
        }
        if flags & FLAG_CHECKSUM != 0 {
            cursor.skip(4)?;
        }
        let payload = cursor.take(size as usize)?;
        let emit =
            self.emit_stream == Some(stream_id) && flags & FLAG_EOR == 0 && !payload.is_empty();
        if emit {
            let mut data = Vec::with_capacity(prefix.len() + payload.len());
            data.extend_from_slice(prefix);
            data.extend_from_slice(payload);
            let is_key = flags & FLAG_KEY != 0;
            if is_key && !contains_parameter_set(self.codec, &data) {
                data = with_extradata_prefix(&stream.extradata, data);
            }
            frames.push(CapturedFrame {
                ts_ms: pts_to_ms(pts, stream.time_base),
                is_key,
                data,
            });
        }
        let Some(Some(stream)) = self.streams.get_mut(stream_id) else {
            unreachable!("stream presence checked above");
        };
        stream.last_pts = pts;
        Ok(())
    }
}

/// Reconstructs the 256-entry frame-code table from its run-length coding,
/// mirroring FFmpeg's `decode_main_header`: each run repeats one template with
/// `size_lsb` increasing across the run, and index `'N'` is a hole marked
/// invalid that does not consume a run slot.
fn parse_frame_code_table(cursor: &mut Cursor<'_>, stream_count: u64) -> Parse<Vec<FrameCode>> {
    let mut table = vec![FrameCode::INVALID; 256];
    let mut pts_delta = 0i64;
    let mut size_mul = 1u64;
    let mut stream_id = 0u64;
    let mut header_idx = 0u64;
    let mut index = 0usize;
    while index < 256 {
        let flags = cursor.varlen()?;
        let fields = cursor.varlen()?;
        let mut size_lsb = 0u64;
        let mut reserved_count = 0u64;
        if fields > 0 {
            pts_delta = cursor.signed()?;
        }
        if fields > 1 {
            size_mul = cursor.varlen()?;
        }
        if fields > 2 {
            stream_id = cursor.varlen()?;
        }
        if fields > 3 {
            size_lsb = cursor.varlen()?;
        }
        if fields > 4 {
            reserved_count = cursor.varlen()?;
        }
        let count = if fields > 5 {
            cursor.varlen()?
        } else {
            let Some(count) = size_mul.checked_sub(size_lsb) else {
                return malformed("frame code run count underflows");
            };
            count
        };
        if fields > 6 {
            cursor.signed()?;
        }
        if fields > 7 {
            header_idx = cursor.varlen()?;
        }
        for _ in 8..fields {
            cursor.varlen()?;
        }
        if stream_id >= stream_count || header_idx > 127 {
            return malformed("frame code template out of range");
        }
        let hole = usize::from(index <= usize::from(b'N'));
        if count == 0 || count as usize > 256 - index - hole {
            return malformed("frame code run count out of range");
        }
        let mut offset = 0u64;
        while offset < count {
            if index == usize::from(b'N') {
                index += 1;
                continue;
            }
            table[index] = FrameCode {
                flags,
                pts_delta,
                stream_id,
                size_mul,
                size_lsb: size_lsb + offset,
                reserved_count,
                header_idx,
            };
            index += 1;
            offset += 1;
        }
    }
    Ok(table)
}

/// Reconstructs a full timestamp from its low `shift` bits, choosing the value
/// nearest `last_pts` (FFmpeg's `ff_lsb2full`).
fn lsb_to_full_pts(last_pts: i64, lsb: i64, shift: u32) -> i64 {
    let mask = (1i64 << shift) - 1;
    let delta = last_pts - mask / 2;
    ((lsb - delta) & mask) + delta
}

/// Rescales a timestamp between rationals, rounding down.
fn rescale(ts: i64, from: (u64, u64), to: (u64, u64)) -> i64 {
    let scaled = ts as i128 * from.0 as i128 * to.1 as i128;
    scaled.div_euclid(from.1 as i128 * to.0 as i128) as i64
}

/// Converts a stream timestamp to milliseconds via its time base.
fn pts_to_ms(pts: i64, time_base: (u64, u64)) -> i64 {
    (pts as i128 * time_base.0 as i128 * 1000).div_euclid(time_base.1 as i128) as i64
}

/// Whether the Annex-B access unit carries an in-band SPS. NUT muxing moves
/// parameter sets into the stream header's extradata, and the publisher needs
/// them in band at keyframes, so keyframes lacking an SPS get the extradata
/// prepended.
fn contains_parameter_set(codec: Codec, unit: &[u8]) -> bool {
    for offset in start_code_offsets(unit) {
        let Some(&header) = unit.get(offset + 3) else {
            continue;
        };
        let is_sps = match codec {
            Codec::H264 => header & 0x1f == 7,
            Codec::Hevc => (header >> 1) & 0x3f == 33,
        };
        if is_sps {
            return true;
        }
    }
    false
}

/// Prepends the stream extradata to a keyframe when the extradata is itself
/// Annex-B; non-Annex-B extradata cannot be spliced in, so the frame passes
/// through unchanged.
fn with_extradata_prefix(extradata: &[u8], data: Vec<u8>) -> Vec<u8> {
    let annex_b = extradata.starts_with(&[0, 0, 1]) || extradata.starts_with(&[0, 0, 0, 1]);
    if !annex_b {
        return data;
    }
    let mut out = Vec::with_capacity(extradata.len() + data.len());
    out.extend_from_slice(extradata);
    out.extend_from_slice(&data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn put_v(out: &mut Vec<u8>, value: u64) {
        let mut groups = vec![(value & 0x7f) as u8];
        let mut value = value >> 7;
        while value != 0 {
            groups.push((value & 0x7f) as u8 | 0x80);
            value >>= 7;
        }
        groups.reverse();
        out.extend_from_slice(&groups);
    }

    fn put_s(out: &mut Vec<u8>, value: i64) {
        let coded = if value > 0 {
            2 * value as u64 - 1
        } else {
            (-2 * value) as u64
        };
        put_v(out, coded);
    }

    fn packet(startcode: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = startcode.to_be_bytes().to_vec();
        let forward = payload.len() as u64 + 4;
        put_v(&mut out, forward);
        if forward > 4096 {
            out.extend_from_slice(&[0; 4]);
        }
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0; 4]);
        out
    }

    /// A main header with one 1/1000 time base (so pts is in milliseconds) and
    /// a frame-code table of one 255-entry run of `FLAG_CODED` templates with
    /// `size_mul` 1, so each frame codes its own flags, pts, and size.
    fn test_main_header() -> Vec<u8> {
        let mut payload = Vec::new();
        put_v(&mut payload, 3);
        put_v(&mut payload, 1);
        put_v(&mut payload, 32767);
        put_v(&mut payload, 1);
        put_v(&mut payload, 1);
        put_v(&mut payload, 1000);
        put_v(&mut payload, FLAG_CODED);
        put_v(&mut payload, 6);
        put_s(&mut payload, 0);
        put_v(&mut payload, 1);
        put_v(&mut payload, 0);
        put_v(&mut payload, 0);
        put_v(&mut payload, 0);
        put_v(&mut payload, 255);
        put_v(&mut payload, 0);
        packet(MAIN_STARTCODE, &payload)
    }

    fn test_stream_header(fourcc: &[u8], extradata: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        put_v(&mut payload, 0);
        put_v(&mut payload, 0);
        put_v(&mut payload, fourcc.len() as u64);
        payload.extend_from_slice(fourcc);
        put_v(&mut payload, 0);
        put_v(&mut payload, 7);
        put_v(&mut payload, 32);
        put_v(&mut payload, 0);
        put_v(&mut payload, 0);
        put_v(&mut payload, extradata.len() as u64);
        payload.extend_from_slice(extradata);
        for dimension in [320u64, 240, 0, 0, 0] {
            put_v(&mut payload, dimension);
        }
        packet(STREAM_STARTCODE, &payload)
    }

    fn test_syncpoint(ts: u64) -> Vec<u8> {
        let mut payload = Vec::new();
        put_v(&mut payload, ts);
        put_v(&mut payload, 0);
        packet(SYNCPOINT_STARTCODE, &payload)
    }

    /// A frame using code 0 (`size_lsb` 0): flags are toggled via `FLAG_CODED`
    /// and the payload length rides in `data_size_msb`.
    fn test_frame(flags: u64, coded_pts: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8];
        put_v(&mut out, FLAG_CODED ^ (flags | FLAG_CODED_PTS | FLAG_SIZE_MSB));
        put_v(&mut out, coded_pts);
        put_v(&mut out, payload.len() as u64);
        out.extend_from_slice(payload);
        out
    }

    const SPS: &[u8] = &[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f];
    const PPS: &[u8] = &[0, 0, 0, 1, 0x68, 0xce];
    const IDR: &[u8] = &[0, 0, 0, 1, 0x65, 0x88, 0x84];
    const SLICE: &[u8] = &[0, 0, 0, 1, 0x41, 0x9a];

    fn extradata() -> Vec<u8> {
        [SPS, PPS].concat()
    }

    fn test_stream() -> Vec<u8> {
        [
            NUT_MAGIC.as_slice(),
            &test_main_header(),
            &test_stream_header(b"H264", &extradata()),
            &test_syncpoint(0),
            &test_frame(FLAG_KEY, 0 + (1 << 7), IDR),
            &test_frame(0, 33 + (1 << 7), SLICE),
        ]
        .concat()
    }

    #[test]
    fn varlen_and_signed_values_round_trip() {
        for value in [0u64, 1, 127, 128, 300, 32767, u64::from(u32::MAX)] {
            let mut bytes = Vec::new();
            put_v(&mut bytes, value);
            assert_eq!(Cursor::new(&bytes).varlen().ok(), Some(value));
        }
        for value in [0i64, 1, -1, 2, -2, 1000, -1000] {
            let mut bytes = Vec::new();
            put_s(&mut bytes, value);
            assert_eq!(Cursor::new(&bytes).signed().ok(), Some(value));
        }
    }

    #[test]
    fn demuxes_frames_and_prepends_extradata_on_bare_keyframes() {
        let mut demuxer = NutDemuxer::new(Codec::H264);
        let mut frames = Vec::new();
        demuxer.push(&test_stream(), &mut frames).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
        assert_eq!(frames[0].ts_ms, 0);
        assert_eq!(frames[0].data, [&extradata()[..], IDR].concat());
        assert!(!frames[1].is_key);
        assert_eq!(frames[1].ts_ms, 33);
        assert_eq!(frames[1].data, SLICE);
    }

    #[test]
    fn keyframe_with_inband_parameter_sets_passes_through() {
        let unit = [SPS, PPS, IDR].concat();
        let stream = [
            NUT_MAGIC.as_slice(),
            &test_main_header(),
            &test_stream_header(b"H264", &extradata()),
            &test_syncpoint(0),
            &test_frame(FLAG_KEY, 1 << 7, &unit),
        ]
        .concat();
        let mut demuxer = NutDemuxer::new(Codec::H264);
        let mut frames = Vec::new();
        demuxer.push(&stream, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, unit);
    }

    #[test]
    fn survives_chunk_boundaries() {
        let stream = test_stream();
        let mut demuxer = NutDemuxer::new(Codec::H264);
        let mut frames = Vec::new();
        for byte in stream {
            demuxer.push(&[byte], &mut frames).unwrap();
        }
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
        assert_eq!(frames[1].ts_ms, 33);
    }

    #[test]
    fn syncpoint_resets_delta_coded_pts() {
        // With msb_pts_shift 7, a coded pts below 128 is the low bits of a
        // timestamp near last_pts: 1002 & 127 == 106, so after a syncpoint at
        // 1000 the frame decodes to 1002.
        let stream = [
            NUT_MAGIC.as_slice(),
            &test_main_header(),
            &test_stream_header(b"H264", &extradata()),
            &test_syncpoint(1000),
            &test_frame(0, 106, SLICE),
        ]
        .concat();
        let mut demuxer = NutDemuxer::new(Codec::H264);
        let mut frames = Vec::new();
        demuxer.push(&stream, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].ts_ms, 1002);
    }

    #[test]
    fn frame_code_table_skips_the_startcode_hole() {
        // The run in the test table covers every code but 'N' (0x4E), so code
        // 0x4F lands one run slot later and inherits size_lsb 78. A frame
        // using it without FLAG_SIZE_MSB carries exactly that many bytes.
        let mut payload = SLICE.to_vec();
        payload.resize(78, 0xaa);
        let mut frame = vec![0x4fu8];
        put_v(&mut frame, FLAG_CODED ^ FLAG_CODED_PTS);
        put_v(&mut frame, 1 << 7);
        frame.extend_from_slice(&payload);
        let stream = [
            NUT_MAGIC.as_slice(),
            &test_main_header(),
            &test_stream_header(b"H264", &extradata()),
            &test_syncpoint(0),
            &frame,
        ]
        .concat();
        let mut demuxer = NutDemuxer::new(Codec::H264);
        let mut frames = Vec::new();
        demuxer.push(&stream, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, payload);
    }

    #[test]
    fn skips_info_packets_and_empty_frames() {
        let info = packet(0x4E49AB68B596BA78, &[0x01, 0x02, 0x03]);
        let stream = [
            NUT_MAGIC.as_slice(),
            &test_main_header(),
            &test_stream_header(b"H264", &extradata()),
            &info,
            &test_syncpoint(0),
            &test_frame(FLAG_EOR, 1 << 7, &[]),
            &test_frame(FLAG_KEY, 1 << 7, IDR),
        ]
        .concat();
        let mut demuxer = NutDemuxer::new(Codec::H264);
        let mut frames = Vec::new();
        demuxer.push(&stream, &mut frames).unwrap();
        assert_eq!(frames.len(), 1);
        assert!(frames[0].is_key);
    }

    #[test]
    fn rejects_fourcc_mismatching_the_selected_codec() {
        let stream = [
            NUT_MAGIC.as_slice(),
            &test_main_header(),
            &test_stream_header(b"HEVC", &[]),
        ]
        .concat();
        let mut demuxer = NutDemuxer::new(Codec::H264);
        let mut frames = Vec::new();
        assert_eq!(
            demuxer.push(&stream, &mut frames),
            Err(NutError::CodecMismatch)
        );
    }

    fn ffmpeg_nut_capture(codec_args: &[&str]) -> Vec<u8> {
        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=30",
                "-t",
                "1",
            ])
            .args(codec_args)
            .args(["-f", "nut", "-"])
            .output()
            .expect("ffmpeg runs");
        assert!(
            output.status.success(),
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn demux_in_chunks(codec: Codec, bytes: &[u8]) -> Vec<CapturedFrame> {
        let mut demuxer = NutDemuxer::new(codec);
        let mut frames = Vec::new();
        for chunk in bytes.chunks(1024) {
            demuxer.push(chunk, &mut frames).unwrap();
        }
        frames
    }

    fn assert_live_stream(codec: Codec, frames: &[CapturedFrame], width: u32, height: u32) {
        assert!(frames.len() >= 25, "expected ~30 frames, got {}", frames.len());
        assert!(frames[0].is_key);
        let params = rpc::bitstream::parse_keyframe(codec, &frames[0].data)
            .expect("first keyframe carries parameter sets");
        assert_eq!((params.width, params.height), (width, height));
        for window in frames.windows(2) {
            assert!(window[0].ts_ms <= window[1].ts_ms);
        }
        for frame in frames {
            let annex_b =
                frame.data.starts_with(&[0, 0, 1]) || frame.data.starts_with(&[0, 0, 0, 1]);
            assert!(annex_b);
        }
    }

    // No `repeat-headers`: the encoder keeps parameter sets in the container
    // extradata only, so these also exercise the keyframe prepend end to end.
    #[test]
    fn demuxes_ffmpeg_h264_nut_output() {
        let bytes = ffmpeg_nut_capture(&[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-bf",
            "0",
            "-g",
            "15",
        ]);
        let frames = demux_in_chunks(Codec::H264, &bytes);
        assert_live_stream(Codec::H264, &frames, 320, 240);
    }

    #[test]
    fn demuxes_ffmpeg_hevc_nut_output() {
        let bytes = ffmpeg_nut_capture(&[
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "bframes=0:keyint=15:min-keyint=15:log-level=none",
        ]);
        let frames = demux_in_chunks(Codec::Hevc, &bytes);
        assert_live_stream(Codec::Hevc, &frames, 320, 240);
    }
}
