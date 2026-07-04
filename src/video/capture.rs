//! Screen capture: spawns a capture subprocess, reads its H.264 Annex-B stdout,
//! and splits it into access units the publisher streams. The subprocess is
//! ffmpeg by default, but `screencast start <COMMAND...>` runs any program that
//! writes Annex-B to stdout.
//!
//! The default command captures the X11 desktop. `repeat-headers=1` makes x264
//! emit SPS/PPS in band at every keyframe so a mid-stream viewer bootstraps, and
//! `sliced-threads=0` keeps one slice per frame so an access unit is one NAL.
//! The splitter promotes the Phase-0 spike's `split_access_units` into the
//! client. Codec metadata is derived from the parameter sets by
//! [`rpc::bitstream`] in the publisher.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use rpc::bitstream::Codec;

/// How many trailing stderr lines to retain so a capture failure can report why.
const STDERR_TAIL_LINES: usize = 8;
const FORMAT_PROBE_BYTES: usize = 512 * 1024;

/// One captured access unit: a monotonic millisecond timestamp, whether it is a
/// keyframe, and its Annex-B H.264 bytes.
pub struct CapturedFrame {
    pub ts_ms: i64,
    pub is_key: bool,
    pub data: Vec<u8>,
}

/// A running capture: the child process, its stdout reader, its stderr logger,
/// and the retained tail of stderr for failure diagnostics.
pub struct Capture {
    child: Child,
    reader: Option<JoinHandle<()>>,
    log_reader: Option<JoinHandle<()>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    failure_reason: Arc<Mutex<Option<String>>>,
}

/// Why a capture ended: the process exit status (absent if it was never reaped)
/// and the retained tail of its stderr, which usually explains a failure.
pub struct CaptureExit {
    pub status: Option<ExitStatus>,
    pub stderr_tail: String,
    pub failure_reason: Option<String>,
}

impl CaptureExit {
    /// A one-line reason built from the exit status and stderr tail, for the UI.
    /// `started` marks whether the share had already begun publishing, which
    /// distinguishes a capture that never produced video from one that stopped.
    pub fn reason(&self, started: bool) -> String {
        let mut message = if let Some(reason) = &self.failure_reason {
            reason.clone()
        } else if started {
            "screen capture stopped".to_string()
        } else {
            "screen capture exited before producing video".to_string()
        };
        if let Some(status) = self.status
            && !status.success()
            && let Some(code) = status.code()
        {
            message.push_str(&format!(" (exit {code})"));
        }
        if !self.stderr_tail.is_empty() {
            message.push_str(": ");
            message.push_str(&self.stderr_tail);
        }
        message
    }
}

impl Capture {
    /// Kills the child, joins the reader threads, and returns its exit
    /// diagnostics. Idempotent: a second call reaps nothing and returns the
    /// retained stderr tail.
    pub fn shutdown(&mut self) -> CaptureExit {
        let _ = self.child.kill();
        let status = self.child.wait().ok();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(log_reader) = self.log_reader.take() {
            let _ = log_reader.join();
        }
        CaptureExit {
            status,
            stderr_tail: self.recent_stderr(),
            failure_reason: self
                .failure_reason
                .lock()
                .ok()
                .and_then(|reason| reason.clone()),
        }
    }

    /// Joins the retained trailing stderr lines into one line for a status message.
    fn recent_stderr(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|lines| lines.iter().cloned().collect::<Vec<_>>().join(" / "))
            .unwrap_or_default()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The built-in capture command: x11grab into low-latency H.264 on stdout. argv
/// is split so the `screencast` subcommand can override it wholesale.
pub fn default_ffmpeg_argv() -> Vec<String> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "x11grab",
        "-framerate",
        "30",
        "-i",
        &display,
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-bf",
        "0",
        "-g",
        "60",
        "-x264-params",
        "repeat-headers=1:sliced-threads=0:keyint=60:min-keyint=60",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "h264",
        "pipe:1",
    ]
    .iter()
    .map(|part| part.to_string())
    .collect()
}

/// The HEVC capture command: x11grab into low-latency H.265 on stdout. `repeat-
/// headers=1` emits VPS/SPS/PPS in band at every keyframe so a mid-stream viewer
/// bootstraps. HEVC decode in the browser is platform-gated, so this is opt-in.
pub fn hevc_ffmpeg_argv() -> Vec<String> {
    let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "x11grab",
        "-framerate",
        "30",
        "-i",
        &display,
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-pix_fmt",
        "yuv420p",
        "-x265-params",
        "repeat-headers=1:keyint=60:min-keyint=60:log-level=none",
        "-f",
        "hevc",
        "pipe:1",
    ]
    .iter()
    .map(|part| part.to_string())
    .collect()
}

/// Spawns the capture command and a reader thread that streams access units to
/// `frame_tx`. `codec` selects how NAL types are classified into access units.
/// The reader exits when ffmpeg's stdout closes or `stop` is set.
pub fn spawn(
    argv: &[String],
    codec: Codec,
    frame_tx: Sender<CapturedFrame>,
    stop: Arc<AtomicBool>,
) -> Result<Capture, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "capture command is empty".to_string())?;
    let mut child = Command::new(program)
        .args(args)
        // Pipe stderr instead of inheriting it: ffmpeg writes its diagnostics
        // there, and the parent's stderr is the TUI, so an inherited handle would
        // paint ffmpeg's output over the interface. A logger thread drains it.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to spawn capture command `{program}`: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture command stdout was not piped".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "capture command stderr was not piped".to_string())?;
    let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
    let failure_reason = Arc::new(Mutex::new(None));
    let reader = thread::Builder::new()
        .name("chatt-capture".to_string())
        .spawn({
            let failure_reason = failure_reason.clone();
            move || read_loop(stdout, codec, &frame_tx, &stop, &failure_reason)
        })
        .map_err(|error| format!("failed to spawn capture reader: {error}"))?;
    let log_tail = stderr_tail.clone();
    let log_reader = thread::Builder::new()
        .name("chatt-capture-log".to_string())
        .spawn(move || log_loop(stderr, &log_tail))
        .map_err(|error| format!("failed to spawn capture logger: {error}"))?;
    Ok(Capture {
        child,
        reader: Some(reader),
        log_reader: Some(log_reader),
        stderr_tail,
        failure_reason,
    })
}

/// Forwards the capture's stderr to the diagnostics log a line at a time, keeping
/// it off the terminal, and retains the trailing lines in `tail` so a failure can
/// report why. The thread exits when stderr closes. Lines are decoded lossily
/// since the capture can emit non-UTF-8 bytes.
fn log_loop(stderr: impl Read, tail: &Mutex<VecDeque<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&line);
                let trimmed = text.trim_end();
                if !trimmed.is_empty() {
                    kvlog::warn!("capture stderr", message = trimmed);
                    if let Ok(mut lines) = tail.lock() {
                        lines.push_back(trimmed.to_string());
                        while lines.len() > STDERR_TAIL_LINES {
                            lines.pop_front();
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
}

fn read_loop(
    mut stdout: impl Read,
    codec: Codec,
    frame_tx: &Sender<CapturedFrame>,
    stop: &AtomicBool,
    failure_reason: &Mutex<Option<String>>,
) {
    let mut splitter = Splitter::new(codec);
    let mut buf = [0u8; 64 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => match splitter.push(&buf[..n], frame_tx) {
                Ok(()) => {}
                Err(SplitterError::SenderClosed) => return,
                Err(error) => {
                    if let Ok(mut reason) = failure_reason.lock() {
                        *reason = Some(error.to_string());
                    }
                    return;
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                kvlog::warn!("capture read failed", error = %error);
                break;
            }
        }
    }
    splitter.flush(frame_tx);
}

/// Incremental Annex-B access-unit splitter. A NAL is complete once the next
/// start code arrives, so units emit with one frame of latency, which is
/// negligible for live view.
struct Splitter {
    codec: Codec,
    buf: Vec<u8>,
    unit: Vec<u8>,
    unit_is_key: bool,
    unit_has_vcl: bool,
    started: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SplitterError {
    SenderClosed,
    MissingAnnexBStartCode,
    CodecMismatch,
}

impl std::fmt::Display for SplitterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SplitterError::SenderClosed => f.write_str("screen capture reader stopped"),
            SplitterError::MissingAnnexBStartCode => {
                f.write_str("screen capture output is not Annex-B video")
            }
            SplitterError::CodecMismatch => {
                f.write_str("screen capture output does not match the selected video codec")
            }
        }
    }
}

/// How one NAL classifies for access-unit splitting: whether it is a coded
/// slice (VCL), starts a keyframe (IRAP/IDR), and whether it is a parameter set
/// or other metadata that begins a new unit when a slice was already buffered.
struct NalClass {
    is_vcl: bool,
    is_key: bool,
    is_param_or_meta: bool,
}

/// Classifies one NAL by codec. H.264 types are the low 5 bits with VCL 1..=5 and
/// IDR type 5; HEVC types are bits 1..7 with VCL below 32 and IRAP 16..=23.
fn classify_nal(codec: Codec, nal: &[u8]) -> NalClass {
    match codec {
        Codec::H264 => {
            let nal_type = nal.first().map(|byte| byte & 0x1f).unwrap_or(0);
            NalClass {
                is_vcl: (1..=5).contains(&nal_type),
                is_key: nal_type == 5,
                is_param_or_meta: matches!(nal_type, 6 | 7 | 8 | 9),
            }
        }
        Codec::Hevc => {
            let nal_type = nal.first().map(|byte| (byte >> 1) & 0x3f).unwrap_or(0);
            NalClass {
                is_vcl: nal_type < 32,
                is_key: (16..=23).contains(&nal_type),
                is_param_or_meta: nal_type >= 32,
            }
        }
    }
}

impl Splitter {
    fn new(codec: Codec) -> Self {
        Self {
            codec,
            buf: Vec::new(),
            unit: Vec::new(),
            unit_is_key: false,
            unit_has_vcl: false,
            started: Instant::now(),
        }
    }

    fn push(
        &mut self,
        bytes: &[u8],
        frame_tx: &Sender<CapturedFrame>,
    ) -> Result<(), SplitterError> {
        self.buf.extend_from_slice(bytes);
        let codes = start_code_offsets(&self.buf);
        if codes.len() < 2 {
            if codes.is_empty() && self.buf.len() >= FORMAT_PROBE_BYTES {
                return Err(SplitterError::MissingAnnexBStartCode);
            }
            return Ok(());
        }
        // Every NAL but the last is complete, the last begins an in-progress NAL.
        for index in 0..codes.len() - 1 {
            let body_start = codes[index] + 3;
            let mut body_end = codes[index + 1];
            if body_end > 0 && self.buf[body_end - 1] == 0 {
                body_end -= 1;
            }
            if body_start < body_end {
                let nal = self.buf[body_start..body_end].to_vec();
                if let Some(frame) = self.push_nal(&nal)? {
                    frame_tx
                        .send(frame)
                        .map_err(|_| SplitterError::SenderClosed)?;
                }
            }
        }
        self.buf.drain(..codes[codes.len() - 1]);
        Ok(())
    }

    fn flush(&mut self, frame_tx: &Sender<CapturedFrame>) {
        let codes = start_code_offsets(&self.buf);
        for index in 0..codes.len() {
            let body_start = codes[index] + 3;
            let body_end = if index + 1 < codes.len() {
                let mut end = codes[index + 1];
                if end > 0 && self.buf[end - 1] == 0 {
                    end -= 1;
                }
                end
            } else {
                self.buf.len()
            };
            if body_start < body_end {
                let nal = self.buf[body_start..body_end].to_vec();
                if let Ok(Some(frame)) = self.push_nal(&nal) {
                    let _ = frame_tx.send(frame);
                }
            }
        }
        self.buf.clear();
        if let Some(frame) = self.take_unit() {
            let _ = frame_tx.send(frame);
        }
    }

    /// Appends one NAL to the current unit, emitting the previous unit when this
    /// NAL begins a new one.
    fn push_nal(&mut self, nal: &[u8]) -> Result<Option<CapturedFrame>, SplitterError> {
        if !nal_compatible_with_codec(self.codec, nal) {
            return Err(SplitterError::CodecMismatch);
        }
        let class = classify_nal(self.codec, nal);
        let starts_new_unit = self.unit_has_vcl && (class.is_vcl || class.is_param_or_meta);
        let emitted = if starts_new_unit {
            self.take_unit()
        } else {
            None
        };
        self.unit.extend_from_slice(&[0, 0, 0, 1]);
        self.unit.extend_from_slice(nal);
        if class.is_key {
            self.unit_is_key = true;
        }
        if class.is_vcl {
            self.unit_has_vcl = true;
        }
        Ok(emitted)
    }

    fn take_unit(&mut self) -> Option<CapturedFrame> {
        if self.unit.is_empty() {
            return None;
        }
        let frame = CapturedFrame {
            ts_ms: self.started.elapsed().as_millis() as i64,
            is_key: self.unit_is_key,
            data: std::mem::take(&mut self.unit),
        };
        self.unit_is_key = false;
        self.unit_has_vcl = false;
        Some(frame)
    }
}

/// Offsets of each `00 00 01` start code in an Annex-B stream.
fn start_code_offsets(stream: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut index = 0;
    while index + 3 <= stream.len() {
        if stream[index] == 0 && stream[index + 1] == 0 && stream[index + 2] == 1 {
            offsets.push(index);
            index += 3;
        } else {
            index += 1;
        }
    }
    offsets
}

fn nal_compatible_with_codec(codec: Codec, nal: &[u8]) -> bool {
    match codec {
        Codec::H264 => {
            let Some(header) = nal.first() else {
                return false;
            };
            matches!(header & 0x1f, 1..=12)
        }
        Codec::Hevc => {
            if nal.len() < 2 {
                return false;
            }
            let nal_type = (nal[0] >> 1) & 0x3f;
            let temporal_id_plus_one = nal[1] & 0x07;
            nal[0] & 0x80 == 0 && nal_type <= 40 && temporal_id_plus_one != 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn annexb(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for part in parts {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(part);
        }
        out
    }

    #[test]
    fn splits_access_units_with_keyframe_classification() {
        // SPS(7) PPS(8) IDR(5) form one keyframe unit, then a non-IDR slice(1).
        let stream = annexb(&[
            &[0x67, 0x42, 0xc0, 0x1f],
            &[0x68, 0x00],
            &[0x65, 0x88],
            &[0x41, 0x9a],
        ]);
        let (tx, rx) = mpsc::channel();
        let mut splitter = Splitter::new(Codec::H264);
        splitter.push(&stream, &tx).unwrap();
        splitter.flush(&tx);
        let frames: Vec<_> = rx.try_iter().collect();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
        assert!(!frames[1].is_key);
    }

    #[test]
    fn splits_hevc_access_units_with_keyframe_classification() {
        // VPS(32) SPS(33) PPS(34) IDR(19) form one keyframe unit, then a
        // non-IRAP TRAIL slice(1). HEVC NAL type is bits 1..7 of the first byte.
        let stream = annexb(&[
            &[0x40, 0x01, 0x0c], // VPS (type 32)
            &[0x42, 0x01, 0x01], // SPS (type 33)
            &[0x44, 0x01, 0xc1], // PPS (type 34)
            &[0x26, 0x01, 0x88], // IDR_W_RADL (type 19)
            &[0x02, 0x01, 0x9a], // TRAIL_R (type 1)
        ]);
        let (tx, rx) = mpsc::channel();
        let mut splitter = Splitter::new(Codec::Hevc);
        splitter.push(&stream, &tx).unwrap();
        splitter.flush(&tx);
        let frames: Vec<_> = rx.try_iter().collect();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
        assert!(!frames[1].is_key);
    }

    #[test]
    fn exit_reason_reports_no_video_with_stderr_tail() {
        let exit = CaptureExit {
            status: None,
            stderr_tail: "wl-screenrec: unknown option --low-power".to_string(),
            failure_reason: None,
        };
        let reason = exit.reason(false);
        assert!(reason.starts_with("screen capture exited before producing video"));
        assert!(reason.ends_with("wl-screenrec: unknown option --low-power"));
    }

    #[test]
    fn exit_reason_distinguishes_a_started_share() {
        let exit = CaptureExit {
            status: None,
            stderr_tail: String::new(),
            failure_reason: None,
        };
        assert_eq!(exit.reason(true), "screen capture stopped");
    }

    #[test]
    fn exit_reason_prefers_format_failure() {
        let exit = CaptureExit {
            status: None,
            stderr_tail: "ffmpeg stderr".to_string(),
            failure_reason: Some("screen capture output is not Annex-B video".to_string()),
        };
        assert_eq!(
            exit.reason(false),
            "screen capture output is not Annex-B video: ffmpeg stderr"
        );
    }

    #[test]
    fn split_survives_chunk_boundaries() {
        let stream = annexb(&[&[0x67, 0x42, 0xc0, 0x1f], &[0x65, 0x88], &[0x41, 0x9a]]);
        let (tx, rx) = mpsc::channel();
        let mut splitter = Splitter::new(Codec::H264);
        for chunk in stream.chunks(3) {
            splitter.push(chunk, &tx).unwrap();
        }
        splitter.flush(&tx);
        let frames: Vec<_> = rx.try_iter().collect();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
    }

    #[test]
    fn rejects_output_without_annex_b_start_code() {
        let (tx, _rx) = mpsc::channel();
        let mut splitter = Splitter::new(Codec::H264);
        let bytes = vec![b'x'; FORMAT_PROBE_BYTES];
        assert_eq!(
            splitter.push(&bytes, &tx),
            Err(SplitterError::MissingAnnexBStartCode)
        );
    }

    #[test]
    fn rejects_codec_mismatched_nal_headers() {
        let (tx, _rx) = mpsc::channel();
        let mut splitter = Splitter::new(Codec::Hevc);
        let h264 = annexb(&[&[0x67, 0x42, 0xc0, 0x1f], &[0x68, 0x00]]);
        assert_eq!(splitter.push(&h264, &tx), Err(SplitterError::CodecMismatch));
    }
}
