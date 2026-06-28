//! Outbound screen share: orchestrates capture, the `StartShare` handshake, and
//! the dedicated publisher connection.
//!
//! The manager thread buffers captured frames until it has the first keyframe,
//! sends `StartShare` with the parsed codec, then waits for the per-stream
//! `publish_secret` the app relays from `ShareStarted`. Once it has the secret it
//! opens the dedicated connection, flushes the buffered keyframe-led burst, and
//! streams live frames.

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rpc::{
    crypto::{CHANNEL_VIDEO, TransportCipher},
    ids::StreamId,
    video::{self, VideoRole},
};

use crate::client_net::{CommandSender, NetworkCommand};

use super::capture::{self, Capture, CapturedFrame};

/// Cap on frames buffered while waiting for the publish secret, so a secret that
/// never arrives cannot grow memory without bound.
const MAX_BUFFERED_FRAMES: usize = 300;

/// Handle to an active outbound screen share. Dropping it stops capture and the
/// publisher connection.
pub struct ScreencastHandle {
    stop: Arc<AtomicBool>,
    secret_tx: Sender<(StreamId, Vec<u8>)>,
    join: Option<JoinHandle<()>>,
}

impl ScreencastHandle {
    /// Relays the per-stream publish secret (from `ShareStarted`) to the manager
    /// so it can bring up the dedicated connection.
    pub fn deliver_secret(&self, stream_id: StreamId, secret: Vec<u8>) {
        let _ = self.secret_tx.send((stream_id, secret));
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
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
    commands: CommandSender,
    tcp_addr: String,
) -> Result<ScreencastHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (frame_tx, frame_rx) = mpsc::channel();
    let (secret_tx, secret_rx) = mpsc::channel();
    let capture = capture::spawn(&argv, frame_tx, stop.clone())?;
    let manager_stop = stop.clone();
    let join = thread::Builder::new()
        .name("chatt-publish".to_string())
        .spawn(move || {
            run_manager(
                capture,
                frame_rx,
                secret_rx,
                commands,
                tcp_addr,
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
}

impl PublisherConn {
    fn send_frame(&mut self, frame: &CapturedFrame) -> Result<(), String> {
        let inner = video::encode_video_frame(frame.ts_ms, frame.is_key, &frame.data);
        let sealed = self
            .cipher
            .seal_next(CHANNEL_VIDEO, &inner)
            .map_err(|error| error.to_string())?;
        let mut record = Vec::with_capacity(sealed.len() + 4);
        video::write_record(&mut record, &sealed).map_err(|error| error.to_string())?;
        self.stream
            .write_all(&record)
            .map_err(|error| format!("video frame write failed: {error}"))
    }
}

fn run_manager(
    mut capture: Capture,
    frame_rx: Receiver<CapturedFrame>,
    secret_rx: Receiver<(StreamId, Vec<u8>)>,
    commands: CommandSender,
    tcp_addr: String,
    stop: Arc<AtomicBool>,
) {
    let mut buffered: Vec<CapturedFrame> = Vec::new();
    let mut start_share_sent = false;
    let mut conn: Option<PublisherConn> = None;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if conn.is_none()
            && let Ok((stream_id, secret)) = secret_rx.try_recv()
        {
            match connect(&tcp_addr, stream_id, &secret) {
                Ok(mut publisher) => {
                    let mut failed = false;
                    for frame in buffered.drain(..) {
                        if let Err(error) = publisher.send_frame(&frame) {
                            kvlog::warn!("video publish burst failed", error = error.as_str());
                            failed = true;
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                    kvlog::info!("video publisher connected", stream_id = stream_id.0);
                    conn = Some(publisher);
                }
                Err(error) => {
                    kvlog::warn!("video publisher connect failed", error = error.as_str());
                    break;
                }
            }
        }

        match frame_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => {
                if !start_share_sent {
                    if !frame.is_key {
                        continue;
                    }
                    let codec = capture::parse_codec(&frame.data)
                        .unwrap_or_else(|| "avc1.42e01f".to_string());
                    if commands
                        .send(NetworkCommand::StartShare {
                            codec,
                            coded_width: 0,
                            coded_height: 0,
                            annexb: true,
                            extradata: Vec::new(),
                        })
                        .is_err()
                    {
                        break;
                    }
                    start_share_sent = true;
                }
                match &mut conn {
                    Some(publisher) => {
                        if let Err(error) = publisher.send_frame(&frame) {
                            kvlog::warn!("video publish failed", error = error.as_str());
                            break;
                        }
                    }
                    None => buffer_frame(&mut buffered, frame),
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    capture.shutdown();
    kvlog::info!("video publisher stopped");
}

fn connect(tcp_addr: &str, stream_id: StreamId, secret: &[u8]) -> Result<PublisherConn, String> {
    let (stream, cipher, _residual) =
        super::open_video_connection(tcp_addr, stream_id, VideoRole::Publisher, secret)?;
    Ok(PublisherConn { stream, cipher })
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
