//! Inbound screen share: a viewer's dedicated connection.
//!
//! The subscriber thread authenticates, reads sealed frames, decrypts each, and
//! forwards the plaintext frame body to the browser over the web feed. The body
//! is byte-identical to what the WebCodecs decoder expects, so it is forwarded
//! without re-framing. The thread auto-reconnects on disconnect or overflow,
//! getting a fresh keyframe from the server's fast-start cache each time.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rpc::{
    crypto::CHANNEL_VIDEO,
    ids::StreamId,
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
    stream_id: StreamId,
    view_secret: Vec<u8>,
    tcp_addr: String,
    feed: WebFeedSender,
) -> SubscriberHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let join = thread::Builder::new()
        .name("chatt-subscribe".to_string())
        .spawn(move || run(stream_id, &view_secret, &tcp_addr, &feed, &thread_stop))
        .ok();
    SubscriberHandle { stop, join }
}

fn run(
    stream_id: StreamId,
    secret: &[u8],
    tcp_addr: &str,
    feed: &WebFeedSender,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        match run_once(stream_id, secret, tcp_addr, feed, stop) {
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
    stream_id: StreamId,
    secret: &[u8],
    tcp_addr: &str,
    feed: &WebFeedSender,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (mut stream, mut cipher, mut buf) =
        super::open_video_connection(tcp_addr, stream_id, VideoRole::Subscriber, secret)?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|error| error.to_string())?;
    kvlog::info!("video subscriber connected", stream_id = stream_id.0);

    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match video::pop_record(&mut buf).map_err(|error| error.to_string())? {
            Some(record) => {
                let plaintext = cipher
                    .open_next(CHANNEL_VIDEO, &record)
                    .map_err(|error| error.to_string())?;
                feed.send_video_frame(plaintext);
                continue;
            }
            None => {}
        }
        let mut tmp = [0u8; 64 * 1024];
        match stream.read(&mut tmp) {
            Ok(0) => return Err("video connection closed".to_string()),
            Ok(read) => buf.extend_from_slice(&tmp[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("video read failed: {error}")),
        }
    }
}
