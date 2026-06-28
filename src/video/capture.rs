//! Screen capture: spawns an ffmpeg subprocess, reads its H.264 Annex-B stdout,
//! and splits it into access units the publisher streams.
//!
//! The default command captures the X11 desktop. `repeat-headers=1` makes x264
//! emit SPS/PPS in band at every keyframe so a mid-stream viewer bootstraps, and
//! `sliced-threads=0` keeps one slice per frame so an access unit is one NAL.
//! The splitter promotes the Phase-0 spike's `split_access_units` and
//! `codecFromSps` into the client.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};
use std::time::Instant;

/// One captured access unit: a monotonic millisecond timestamp, whether it is a
/// keyframe, and its Annex-B H.264 bytes.
pub struct CapturedFrame {
    pub ts_ms: i64,
    pub is_key: bool,
    pub data: Vec<u8>,
}

/// A running capture: the ffmpeg child and its reader thread.
pub struct Capture {
    child: Child,
    reader: Option<JoinHandle<()>>,
}

impl Capture {
    /// Kills ffmpeg and joins the reader thread.
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
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

/// Spawns the capture command and a reader thread that streams access units to
/// `frame_tx`. The reader exits when ffmpeg's stdout closes or `stop` is set.
pub fn spawn(
    argv: &[String],
    frame_tx: Sender<CapturedFrame>,
    stop: Arc<AtomicBool>,
) -> Result<Capture, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "capture command is empty".to_string())?;
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to spawn capture command `{program}`: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "capture command stdout was not piped".to_string())?;
    let reader = thread::Builder::new()
        .name("chatt-capture".to_string())
        .spawn(move || read_loop(stdout, &frame_tx, &stop))
        .map_err(|error| format!("failed to spawn capture reader: {error}"))?;
    Ok(Capture {
        child,
        reader: Some(reader),
    })
}

fn read_loop(mut stdout: impl Read, frame_tx: &Sender<CapturedFrame>, stop: &AtomicBool) {
    let mut splitter = Splitter::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if splitter.push(&buf[..n], frame_tx).is_err() {
                    return;
                }
            }
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
    buf: Vec<u8>,
    unit: Vec<u8>,
    unit_is_key: bool,
    unit_has_vcl: bool,
    started: Instant,
}

impl Splitter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            unit: Vec::new(),
            unit_is_key: false,
            unit_has_vcl: false,
            started: Instant::now(),
        }
    }

    fn push(&mut self, bytes: &[u8], frame_tx: &Sender<CapturedFrame>) -> Result<(), ()> {
        self.buf.extend_from_slice(bytes);
        let codes = start_code_offsets(&self.buf);
        if codes.len() < 2 {
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
                if let Some(frame) = self.push_nal(&nal) {
                    frame_tx.send(frame).map_err(|_| ())?;
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
                if let Some(frame) = self.push_nal(&nal) {
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
    fn push_nal(&mut self, nal: &[u8]) -> Option<CapturedFrame> {
        let nal_type = nal.first().map(|byte| byte & 0x1f).unwrap_or(0);
        let is_vcl = (1..=5).contains(&nal_type);
        let starts_new_unit = self.unit_has_vcl && (is_vcl || matches!(nal_type, 6 | 7 | 8 | 9));
        let emitted = if starts_new_unit {
            self.take_unit()
        } else {
            None
        };
        self.unit.extend_from_slice(&[0, 0, 0, 1]);
        self.unit.extend_from_slice(nal);
        if nal_type == 5 {
            self.unit_is_key = true;
        }
        if is_vcl {
            self.unit_has_vcl = true;
        }
        emitted
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

/// Derives the WebCodecs codec string from a keyframe's SPS NAL: `avc1.PPCCLL`
/// where PP is profile_idc, CC the constraint flags, and LL the level_idc.
/// Returns `None` when the unit carries no SPS.
pub fn parse_codec(unit: &[u8]) -> Option<String> {
    for offset in start_code_offsets(unit) {
        let nal_start = offset + 3;
        if nal_start + 3 < unit.len() && unit[nal_start] & 0x1f == 7 {
            return Some(format!(
                "avc1.{:02x}{:02x}{:02x}",
                unit[nal_start + 1],
                unit[nal_start + 2],
                unit[nal_start + 3]
            ));
        }
    }
    None
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
        let mut splitter = Splitter::new();
        splitter.push(&stream, &tx).unwrap();
        splitter.flush(&tx);
        let frames: Vec<_> = rx.try_iter().collect();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
        assert!(!frames[1].is_key);
    }

    #[test]
    fn parse_codec_reads_sps_profile() {
        let unit = annexb(&[&[0x67, 0x42, 0xc0, 0x1f], &[0x65, 0x88]]);
        assert_eq!(parse_codec(&unit).as_deref(), Some("avc1.42c01f"));
    }

    #[test]
    fn split_survives_chunk_boundaries() {
        let stream = annexb(&[&[0x67, 0x42, 0xc0, 0x1f], &[0x65, 0x88], &[0x41, 0x9a]]);
        let (tx, rx) = mpsc::channel();
        let mut splitter = Splitter::new();
        for chunk in stream.chunks(3) {
            splitter.push(chunk, &tx).unwrap();
        }
        splitter.flush(&tx);
        let frames: Vec<_> = rx.try_iter().collect();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].is_key);
    }
}
