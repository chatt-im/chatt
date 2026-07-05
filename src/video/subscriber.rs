//! Inbound screen share: a viewer's dedicated connection.
//!
//! The subscriber thread authenticates, reads sealed frames, decrypts each, and
//! forwards the plaintext frame body to the browser over the web feed. The body
//! is byte-identical to what the WebCodecs decoder expects, so it is forwarded
//! without re-framing. The thread auto-reconnects on disconnect or overflow,
//! getting a fresh keyframe from the server's fast-start cache each time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rpc::{
    crypto::{CHANNEL_VIDEO, TRANSPORT_HEADER_LEN, TransportCipher},
    ids::{SessionId, StreamId},
    recv::RecvBuffer,
    video::{self, VideoRole},
};

use crate::web_server::WebFeedSender;

const READ_TIMEOUT: Duration = Duration::from_millis(200);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Handle to an active viewer connection. Dropping it tears the connection down.
pub struct SubscriberHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SubscriberHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for SubscriberHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawns a viewer thread that streams `stream_id` to the browser via `feed`.
pub fn start(
    session_id: SessionId,
    stream_id: StreamId,
    view_secret: Vec<u8>,
    tcp_addr: String,
    feed: WebFeedSender,
) -> SubscriberHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let join = thread::Builder::new()
        .name("chatt-subscribe".to_string())
        .spawn(move || {
            run(
                session_id,
                stream_id,
                &view_secret,
                &tcp_addr,
                &feed,
                &thread_stop,
            )
        })
        .ok();
    SubscriberHandle { stop, join }
}

fn run(
    session_id: SessionId,
    stream_id: StreamId,
    secret: &[u8],
    tcp_addr: &str,
    feed: &WebFeedSender,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        match run_once(session_id, stream_id, secret, tcp_addr, feed, stop) {
            Ok(()) => break,
            Err(error) => {
                kvlog::warn!(
                    "video subscriber reconnecting",
                    stream_id = stream_id.0,
                    error = error.as_str()
                );
                thread::sleep(RECONNECT_BACKOFF);
            }
        }
    }
    kvlog::info!("video subscriber stopped", stream_id = stream_id.0);
}

fn run_once(
    session_id: SessionId,
    stream_id: StreamId,
    secret: &[u8],
    tcp_addr: &str,
    feed: &WebFeedSender,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (stream, mut cipher, mut recv) = super::open_video_connection(
        tcp_addr,
        session_id,
        stream_id,
        VideoRole::Subscriber,
        secret,
    )?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|error| error.to_string())?;
    kvlog::info!("video subscriber connected", stream_id = stream_id.0);

    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        forward_buffered_frames(&mut recv, &mut cipher, feed)?;
        match recv.fill(&stream, super::VIDEO_READ_CHUNK_BYTES) {
            Ok(0) => return Err("video connection closed".to_string()),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("video read failed: {error}")),
        }
    }
}

/// Decrypts every whole record buffered in `recv` in place and forwards the
/// plaintext to the browser. A read usually delivers exactly one whole record,
/// and that record's buffer is shipped to the feed as-is rather than copied;
/// only records sharing the buffer with following bytes are copied out.
fn forward_buffered_frames(
    recv: &mut RecvBuffer,
    cipher: &mut TransportCipher,
    feed: &WebFeedSender,
) -> Result<(), String> {
    const PLAINTEXT_START: usize = video::VIDEO_LENGTH_PREFIX_LEN + TRANSPORT_HEADER_LEN;
    loop {
        let total = match video::parse_record(recv.pending()) {
            Ok(Some((_, total))) => total,
            Ok(None) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let plaintext_len = {
            let record = &mut recv.pending_mut()[video::VIDEO_LENGTH_PREFIX_LEN..total];
            cipher
                .open_next_in_place(CHANNEL_VIDEO, record)
                .map_err(|error| error.to_string())?
                .len()
        };
        if recv.len() == total {
            feed.send_video_frame(recv.ship(PLAINTEXT_START, plaintext_len));
        } else {
            let plaintext = &recv.pending()[PLAINTEXT_START..PLAINTEXT_START + plaintext_len];
            feed.send_video_frame(plaintext.to_vec());
            recv.consume(total);
        }
    }
}
