//! Inbound screen share: a viewer's dedicated connection.
//!
//! The subscriber thread authenticates, then reads each sealed frame record
//! into its own exact allocation through [`VideoRecordReader`], decrypts it in
//! place, and forwards the plaintext window to the browser as a
//! [`SharedVideoFrame`]. The body is byte-identical to what the WebCodecs
//! decoder expects, so it is forwarded without re-framing or copying. The
//! thread auto-reconnects on disconnect or overflow, getting a fresh keyframe
//! from the server's fast-start cache each time.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rpc::{
    crypto::{CHANNEL_VIDEO, RecordProtection},
    ids::{SessionId, StreamId},
    video::{SharedVideoFrame, VideoRecordReader, VideoRole},
};

use crate::web_server::WebFeedSender;

use super::VideoTransport;

const READ_TIMEOUT: Duration = Duration::from_millis(200);
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);
const COPY_STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Rolling per-second counters for receive-path copy behaviour: frame bytes
/// forwarded zero-copy from exact record allocations versus handshake-residual
/// bytes copied into the reader once at connect. Logged at most once per
/// [`COPY_STATS_INTERVAL`].
struct CopyStats {
    shipped_bytes: u64,
    copied_bytes: u64,
    frames: u64,
    last_log: std::time::Instant,
}

impl CopyStats {
    fn new() -> Self {
        Self {
            shipped_bytes: 0,
            copied_bytes: 0,
            frames: 0,
            last_log: std::time::Instant::now(),
        }
    }

    fn maybe_log(&mut self) {
        if self.last_log.elapsed() < COPY_STATS_INTERVAL {
            return;
        }
        kvlog::debug!(
            "video subscribe copy stats",
            shipped_bytes = self.shipped_bytes,
            copied_bytes = self.copied_bytes,
            frames = self.frames
        );
        *self = Self::new();
    }
}

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
    video_transport: VideoTransport,
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
                video_transport,
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
    video_transport: VideoTransport,
    feed: &WebFeedSender,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        match run_once(
            session_id,
            stream_id,
            secret,
            tcp_addr,
            video_transport,
            feed,
            stop,
        ) {
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
    video_transport: VideoTransport,
    feed: &WebFeedSender,
    stop: &AtomicBool,
) -> Result<(), String> {
    let (stream, mut record_protection, mut recv) = super::open_video_connection(
        tcp_addr,
        session_id,
        stream_id,
        VideoRole::Subscriber,
        secret,
        video_transport,
    )?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|error| error.to_string())?;
    kvlog::info!("video subscriber connected", stream_id = stream_id.0);

    let mut copy_stats = CopyStats::new();
    let mut reader = VideoRecordReader::new();
    // Bytes the handshake read past the ack drain into exact per-record
    // storage, the one copy this connection ever makes.
    while !recv.is_empty() {
        let taken = reader
            .accept(recv.pending())
            .map_err(|error| error.to_string())?;
        copy_stats.copied_bytes += taken as u64;
        recv.consume(taken);
        forward_ready_record(&mut reader, &mut record_protection, feed, &mut copy_stats)?;
    }

    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        forward_ready_record(&mut reader, &mut record_protection, feed, &mut copy_stats)?;
        match reader.fill(&stream, super::VIDEO_READ_CHUNK_BYTES) {
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

/// Forwards the reader's completed record, when one is buffered: decrypts it
/// in place in its own exact allocation and hands the plaintext window to the
/// browser as a [`SharedVideoFrame`] without copying.
fn forward_ready_record(
    reader: &mut VideoRecordReader,
    record_protection: &mut RecordProtection,
    feed: &WebFeedSender,
    copy_stats: &mut CopyStats,
) -> Result<(), String> {
    let Some(record) = reader.take_record() else {
        return Ok(());
    };
    let frame = open_record_frame(record, record_protection)?;
    copy_stats.shipped_bytes += frame.len() as u64;
    copy_stats.frames += 1;
    copy_stats.maybe_log();
    feed.send_video_frame(frame);
    Ok(())
}

/// Decrypts one whole sealed record body in place and wraps its plaintext
/// window, keeping the record's exact allocation as the frame's backing.
fn open_record_frame(
    mut record: Vec<u8>,
    record_protection: &mut RecordProtection,
) -> Result<SharedVideoFrame, String> {
    let base = record.as_ptr() as usize;
    let (plaintext_offset, plaintext_len) = {
        let plaintext = record_protection
            .open_next_in_place(CHANNEL_VIDEO, &mut record)
            .map_err(|error| error.to_string())?;
        (plaintext.as_ptr() as usize - base, plaintext.len())
    };
    Ok(SharedVideoFrame::from_record(
        record,
        plaintext_offset,
        plaintext_len,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::crypto::{
        KEY_LEN, TAG_LEN, TRANSPORT_HEADER_LEN, TransportCipher, VideoKeyRole, derive_video_keys,
    };
    use rpc::video;

    #[test]
    fn multi_record_residual_forwards_exact_backed_frames() {
        let secret = [3u8; KEY_LEN];
        let (server_send, server_recv) = derive_video_keys(&secret, VideoKeyRole::Server);
        let mut server_cipher = TransportCipher::new(server_send, server_recv);
        let (client_send, client_recv) = derive_video_keys(&secret, VideoKeyRole::Client);
        let mut client_record = RecordProtection::aead(client_send, client_recv);

        let mut inners = Vec::new();
        let mut residual = Vec::new();
        for (ts_ms, body) in [(1i64, vec![1u8; 8]), (2, vec![2u8; 24]), (3, vec![3u8; 4])] {
            let mut inner = Vec::new();
            video::write_video_frame(&mut inner, ts_ms, ts_ms == 1, 7, &body);
            let sealed = server_cipher.seal_next(CHANNEL_VIDEO, &inner).unwrap();
            video::write_record(&mut residual, &sealed).unwrap();
            inners.push(inner);
        }

        let mut reader = VideoRecordReader::new();
        let mut offset = 0;
        let mut frames = Vec::new();
        while offset < residual.len() || reader.record_ready() {
            let taken = reader.accept(&residual[offset..]).unwrap();
            offset += taken;
            if let Some(record) = reader.take_record() {
                frames.push(open_record_frame(record, &mut client_record).unwrap());
            }
        }

        assert_eq!(frames.len(), inners.len());
        for (frame, inner) in frames.iter().zip(&inners) {
            assert_eq!(frame.as_slice(), inner);
            // Each frame keeps exactly its own sealed record allocation.
            assert_eq!(
                frame.retained_bytes(),
                inner.len() + TRANSPORT_HEADER_LEN + TAG_LEN
            );
        }
    }

    #[test]
    fn clear_record_frame_uses_zero_plaintext_offset() {
        let mut inner = Vec::new();
        video::write_video_frame(&mut inner, 1, true, 7, &[1, 2, 3]);
        let mut record = RecordProtection::clear();
        let frame = open_record_frame(inner.clone(), &mut record).unwrap();
        assert_eq!(frame.as_slice(), inner);
        assert_eq!(frame.retained_bytes(), inner.len());
    }
}
