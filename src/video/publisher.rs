//! Outbound screen share: orchestrates capture, the `StartShare` handshake, and
//! the dedicated publisher connection.
//!
//! The manager thread buffers captured frames until it has the first keyframe,
//! sends `StartShare` with the parsed codec, then waits for the per-stream
//! `publish_secret` the app relays from `ShareStarted`. Once it has the secret it
//! opens the dedicated connection, flushes the buffered keyframe-led burst, and
//! streams live frames.

use std::collections::VecDeque;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rpc::{
    bitstream::{self, Codec},
    crypto::{CHANNEL_VIDEO, TransportCipher},
    ids::{SessionId, StreamId},
    video::{self, VideoRole},
};

use crate::app::{AppEvent, EventSender, ScreencastProgress};
use crate::client_net::{CommandSender, NetworkCommand};
use crate::web_server::WebFeedSender;

use super::capture::{self, Capture, CapturedFrame};

/// Cap on frames buffered while waiting for the publish secret, so a secret that
/// never arrives cannot grow memory without bound.
const MAX_BUFFERED_FRAMES: usize = 300;
const STARTUP_KEYFRAME_TIMEOUT: Duration = Duration::from_secs(10);
const PUBLISH_SECRET_TIMEOUT: Duration = Duration::from_secs(10);
const VIDEO_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const PROGRESS_WINDOW: Duration = Duration::from_secs(5);

/// Handle to an active outbound screen share. Dropping it stops capture and the
/// publisher connection.
pub struct ScreencastHandle {
    stop: Arc<AtomicBool>,
    secret_tx: Sender<(SessionId, StreamId, Vec<u8>)>,
    join: Option<JoinHandle<()>>,
}

impl ScreencastHandle {
    /// Relays the per-stream publish secret (from `ShareStarted`) to the manager
    /// so it can bring up the dedicated connection.
    pub fn deliver_secret(&self, session_id: SessionId, stream_id: StreamId, secret: Vec<u8>) {
        let _ = self.secret_tx.send((session_id, stream_id, secret));
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let (secret_tx, _secret_rx) = mpsc::channel();
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            secret_tx,
            join: None,
        }
    }
}

impl Drop for ScreencastHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawns capture and the publisher manager. `StartShare` is sent over
/// `commands` once the first keyframe is captured.
pub fn start(
    argv: Vec<String>,
    codec: Codec,
    commands: CommandSender,
    tcp_addr: String,
    web_feed: Option<WebFeedSender>,
    events: EventSender,
) -> Result<ScreencastHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (frame_tx, frame_rx) = mpsc::channel();
    let (secret_tx, secret_rx) = mpsc::channel();
    let capture = capture::spawn(&argv, codec, frame_tx, stop.clone())?;
    let manager_stop = stop.clone();
    let join = thread::Builder::new()
        .name("chatt-publish".to_string())
        .spawn(move || {
            run_manager(
                capture,
                codec,
                frame_rx,
                secret_rx,
                commands,
                tcp_addr,
                web_feed,
                events,
                manager_stop,
            )
        })
        .map_err(|error| format!("failed to spawn publisher: {error}"))?;
    Ok(ScreencastHandle {
        stop,
        secret_tx,
        join: Some(join),
    })
}

struct PublisherConn {
    stream: TcpStream,
    cipher: TransportCipher,
    stream_id: u32,
    codec: Codec,
}

impl PublisherConn {
    /// Converts one captured frame to the length-prefixed wire body, seals it,
    /// and writes it to the server. When `web_feed` is present the same plaintext
    /// body is forwarded to the local browser so a sharer sees their own stream
    /// without a server round-trip. The body carries `stream_id` so the browser
    /// routes it to the matching decoder.
    fn send_frame(
        &mut self,
        frame: &CapturedFrame,
        web_feed: Option<&WebFeedSender>,
    ) -> Result<u64, String> {
        let inner = encode_publish_frame(frame, self.stream_id, self.codec);
        if let Some(feed) = web_feed {
            feed.send_video_frame(inner.clone());
        }
        let sealed = self
            .cipher
            .seal_next(CHANNEL_VIDEO, &inner)
            .map_err(|error| error.to_string())?;
        let mut record = Vec::with_capacity(sealed.len() + 4);
        video::write_record(&mut record, &sealed).map_err(|error| error.to_string())?;
        let bytes = record.len() as u64;
        self.stream
            .write_all(&record)
            .map_err(|error| format!("video frame write failed: {error}"))?;
        Ok(bytes)
    }
}

struct TransferStats {
    total_bytes: u64,
    total_frames: u64,
    samples: VecDeque<(Instant, u64)>,
    last_emit: Option<Instant>,
}

impl TransferStats {
    fn new() -> Self {
        Self {
            total_bytes: 0,
            total_frames: 0,
            samples: VecDeque::new(),
            last_emit: None,
        }
    }

    fn record(&mut self, stream_id: StreamId, bytes: u64, events: &EventSender) {
        let now = Instant::now();
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.total_frames = self.total_frames.saturating_add(1);
        self.samples.push_back((now, bytes));
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > PROGRESS_WINDOW)
        {
            self.samples.pop_front();
        }
        if self
            .last_emit
            .is_some_and(|last| now.saturating_duration_since(last) < PROGRESS_INTERVAL)
        {
            return;
        }
        self.last_emit = Some(now);
        let rolling_bytes = self.samples.iter().map(|(_, bytes)| *bytes).sum::<u64>();
        let span = self
            .samples
            .front()
            .map(|(first, _)| now.saturating_duration_since(*first))
            .unwrap_or(PROGRESS_INTERVAL)
            .max(PROGRESS_INTERVAL);
        let rolling_bytes_per_sec = ((rolling_bytes as f64) / span.as_secs_f64())
            .round()
            .max(0.0) as u64;
        let _ = events.send(AppEvent::ScreencastProgress(ScreencastProgress {
            stream_id,
            total_bytes: self.total_bytes,
            total_frames: self.total_frames,
            rolling_bytes_per_sec,
        }));
    }
}

fn run_manager(
    mut capture: Capture,
    codec: Codec,
    frame_rx: Receiver<CapturedFrame>,
    secret_rx: Receiver<(SessionId, StreamId, Vec<u8>)>,
    commands: CommandSender,
    tcp_addr: String,
    web_feed: Option<WebFeedSender>,
    events: EventSender,
    stop: Arc<AtomicBool>,
) {
    let mut buffered: Vec<CapturedFrame> = Vec::new();
    let mut start_share_sent = false;
    let started_at = Instant::now();
    let mut startup_frames = 0usize;
    let mut start_share_sent_at: Option<Instant> = None;
    let mut conn: Option<PublisherConn> = None;
    let mut stats = TransferStats::new();
    // Set when the loop breaks for a reason the user should see. `None` after the
    // loop means either a clean stop or that the capture ended on its own, which
    // the exit diagnostics then explain.
    let mut error: Option<String> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if conn.is_none()
            && let Ok((session_id, stream_id, secret)) = secret_rx.try_recv()
        {
            match connect(&tcp_addr, session_id, stream_id, &secret, codec) {
                Ok(mut publisher) => {
                    let mut failed = false;
                    for frame in buffered.drain(..) {
                        match publisher.send_frame(&frame, web_feed.as_ref()) {
                            Ok(bytes) => stats.record(stream_id, bytes, &events),
                            Err(reason) => {
                                error = Some(format!("screen publish failed: {reason}"));
                                failed = true;
                                break;
                            }
                        }
                    }
                    if failed {
                        break;
                    }
                    kvlog::info!("video publisher connected", stream_id = stream_id.0);
                    conn = Some(publisher);
                }
                Err(reason) => {
                    error = Some(format!("screen share connect failed: {reason}"));
                    break;
                }
            }
        }

        match frame_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                if !start_share_sent {
                    startup_frames = startup_frames.saturating_add(1);
                    if !frame.is_key {
                        if startup_frames >= MAX_BUFFERED_FRAMES {
                            error = Some(format!(
                                "screen capture did not produce a keyframe after {startup_frames} frames"
                            ));
                            break;
                        }
                        continue;
                    }
                    // The encoder emits parameter sets in band at every keyframe,
                    // so a complete keyframe carries the codec metadata the
                    // descriptor needs. A keyframe without them is incomplete, so
                    // wait for the next.
                    let Some(params) = bitstream::parse_keyframe(codec, &frame.data) else {
                        if startup_frames >= MAX_BUFFERED_FRAMES {
                            error = Some(format!(
                                "screen capture keyframes did not contain usable {} parameters",
                                codec_label(codec)
                            ));
                            break;
                        }
                        continue;
                    };
                    if commands
                        .send(NetworkCommand::StartShare {
                            codec: params.codec,
                            coded_width: params.width,
                            coded_height: params.height,
                            annexb: false,
                            extradata: params.extra_data,
                        })
                        .is_err()
                    {
                        error = Some("screen share control channel closed".to_string());
                        break;
                    }
                    start_share_sent = true;
                    start_share_sent_at = Some(Instant::now());
                }
                match &mut conn {
                    Some(publisher) => match publisher.send_frame(&frame, web_feed.as_ref()) {
                        Ok(bytes) => {
                            stats.record(StreamId(publisher.stream_id), bytes, &events);
                        }
                        Err(reason) => {
                            error = Some(format!("screen publish failed: {reason}"));
                            break;
                        }
                    },
                    None => buffer_frame(&mut buffered, frame),
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                if !start_share_sent
                    && now.saturating_duration_since(started_at) >= STARTUP_KEYFRAME_TIMEOUT
                {
                    error = Some(format!(
                        "screen capture did not produce a decodable {} keyframe within {}s",
                        codec_label(codec),
                        STARTUP_KEYFRAME_TIMEOUT.as_secs()
                    ));
                    break;
                }
                if conn.is_none()
                    && let Some(sent_at) = start_share_sent_at
                    && now.saturating_duration_since(sent_at) >= PUBLISH_SECRET_TIMEOUT
                {
                    error = Some(format!(
                        "screen share publish secret timed out after {}s",
                        PUBLISH_SECRET_TIMEOUT.as_secs()
                    ));
                    break;
                }
            }
            // The capture reader thread dropped its sender, so the capture process
            // closed its stdout. The exit diagnostics classify why.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let exit = capture.shutdown();
    if stop.load(Ordering::SeqCst) {
        kvlog::info!("video publisher stopped");
        return;
    }
    let message = error.unwrap_or_else(|| exit.reason(start_share_sent));
    kvlog::warn!("screen share failed", message = message.as_str());
    let _ = events.send(AppEvent::ScreencastFailed(message));
}

fn codec_label(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "H.264",
        Codec::Hevc => "HEVC",
    }
}

/// Converts one captured Annex-B access unit to the wire frame body and frames
/// it: the access unit becomes a length-prefixed bitstream with parameter sets
/// stripped, tagged with `stream_id`. This is the exact body sealed to the server
/// and teed to the local web feed, so a sharer's self-view decodes identically.
fn encode_publish_frame(frame: &CapturedFrame, stream_id: u32, codec: Codec) -> Vec<u8> {
    let body = bitstream::annex_b_to_length_prefixed(codec, &frame.data);
    video::encode_video_frame(frame.ts_ms, frame.is_key, stream_id, &body)
}

fn connect(
    tcp_addr: &str,
    session_id: SessionId,
    stream_id: StreamId,
    secret: &[u8],
    codec: Codec,
) -> Result<PublisherConn, String> {
    let (stream, cipher, _residual) = super::open_video_connection(
        tcp_addr,
        session_id,
        stream_id,
        VideoRole::Publisher,
        secret,
    )?;
    stream
        .set_write_timeout(Some(VIDEO_WRITE_TIMEOUT))
        .map_err(|error| format!("video write timeout setup failed: {error}"))?;
    Ok(PublisherConn {
        stream,
        cipher,
        stream_id: stream_id.0,
        codec,
    })
}

/// Appends a frame to the pre-connection buffer, trimming to the most recent
/// keyframe when it grows too large so a late secret still starts decodable.
fn buffer_frame(buffered: &mut Vec<CapturedFrame>, frame: CapturedFrame) {
    buffered.push(frame);
    if buffered.len() > MAX_BUFFERED_FRAMES
        && let Some(last_key) = buffered.iter().rposition(|frame| frame.is_key)
    {
        buffered.drain(..last_key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn publish_frame_strips_parameter_sets_and_tags_stream() {
        // An Annex-B keyframe: SPS(7), PPS(8), then the IDR slice(5).
        let data = vec![
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0xaa, 0xbb,
        ];
        let frame = CapturedFrame {
            ts_ms: 5,
            is_key: true,
            data,
        };
        let mut inner = encode_publish_frame(&frame, 9, Codec::H264);
        let parsed = video::pop_video_frame(&mut inner).unwrap().unwrap();
        assert_eq!(parsed.stream_id, 9);
        assert!(parsed.is_key);
        assert_eq!(parsed.ts_ms, 5);
        // Only the IDR slice survives, length-prefixed; SPS/PPS are stripped.
        assert_eq!(parsed.data, vec![0, 0, 0, 3, 0x65, 0xaa, 0xbb]);
    }

    #[test]
    fn transfer_stats_reports_initial_rate_without_zero_span_spike() {
        let (tx, rx) = mpsc::channel();
        let events = EventSender(tx);
        let mut stats = TransferStats::new();

        stats.record(StreamId(3), 2_048, &events);

        let AppEvent::ScreencastProgress(progress) = rx.try_recv().unwrap() else {
            panic!("expected screencast progress");
        };
        assert_eq!(progress.stream_id, StreamId(3));
        assert_eq!(progress.total_bytes, 2_048);
        assert_eq!(progress.total_frames, 1);
        assert_eq!(progress.rolling_bytes_per_sec, 2_048);
    }
}
