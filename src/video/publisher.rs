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
    bitstream::{self, Codec},
    crypto::{CHANNEL_VIDEO, TransportCipher},
    ids::StreamId,
    video::{self, VideoRole},
};

use crate::client_net::{CommandSender, NetworkCommand};
use crate::web_server::WebFeedSender;

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
    codec: Codec,
    commands: CommandSender,
    tcp_addr: String,
    web_feed: Option<WebFeedSender>,
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
    ) -> Result<(), String> {
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
        self.stream
            .write_all(&record)
            .map_err(|error| format!("video frame write failed: {error}"))
    }
}

fn run_manager(
    mut capture: Capture,
    codec: Codec,
    frame_rx: Receiver<CapturedFrame>,
    secret_rx: Receiver<(StreamId, Vec<u8>)>,
    commands: CommandSender,
    tcp_addr: String,
    web_feed: Option<WebFeedSender>,
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
            match connect(&tcp_addr, stream_id, &secret, codec) {
                Ok(mut publisher) => {
                    let mut failed = false;
                    for frame in buffered.drain(..) {
                        if let Err(error) = publisher.send_frame(&frame, web_feed.as_ref()) {
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
                    // The encoder emits parameter sets in band at every keyframe,
                    // so a complete keyframe carries the codec metadata the
                    // descriptor needs. A keyframe without them is incomplete, so
                    // wait for the next.
                    let Some(params) = bitstream::parse_keyframe(codec, &frame.data) else {
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
                        break;
                    }
                    start_share_sent = true;
                }
                match &mut conn {
                    Some(publisher) => {
                        if let Err(error) = publisher.send_frame(&frame, web_feed.as_ref()) {
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
    stream_id: StreamId,
    secret: &[u8],
    codec: Codec,
) -> Result<PublisherConn, String> {
    let (stream, cipher, _residual) =
        super::open_video_connection(tcp_addr, stream_id, VideoRole::Publisher, secret)?;
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
}
