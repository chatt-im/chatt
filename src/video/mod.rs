//! Screen-share video: capture, publish, and subscribe over dedicated encrypted
//! TCP connections, outside the latency-sensitive `client_net` mio loop.
//!
//! A dedicated connection opens with [`rpc::video::VIDEO_MAGIC`], a clear
//! [`VideoHello`], and a sealed auth record proving possession of the per-stream
//! secret, then carries sealed frames (publisher up, server down). Each
//! connection runs on its own blocking thread so a slow video link never stalls
//! voice and restarts are cheap.

pub mod capture;
mod fanout;
mod nut;
mod publisher;
mod subscriber;

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use rpc::{
    crypto::{
        CHANNEL_VIDEO, KEY_LEN, RecordProtection, TransportMode, VideoKeyRole, derive_video_keys,
    },
    ids::{SessionId, StreamId},
    recv::RecvBuffer,
    video::{self, VideoAck, VideoHello, VideoRole},
};

pub use fanout::{NativeViewerHandle, VideoFrameFanout};
pub use publisher::{ScreencastHandle, start as start_screencast};
pub use subscriber::{SubscriberHandle, start as start_subscriber};

/// Fixed plaintext for the auth record. Its contents are irrelevant, opening it
/// is what proves the peer derived the per-stream key.
const AUTH_PAYLOAD: &[u8] = b"chatt-video-auth-v1";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub struct VideoTransport {
    mode: TransportMode,
    auth_key: [u8; KEY_LEN],
}

impl VideoTransport {
    pub fn new(mode: TransportMode, auth_key: [u8; KEY_LEN]) -> Self {
        Self { mode, auth_key }
    }

    fn record_protection(self, secret: &[u8]) -> Result<RecordProtection, String> {
        match self.mode {
            TransportMode::NativeEncrypted => {
                let secret: &[u8; KEY_LEN] = secret
                    .try_into()
                    .map_err(|_| "video secret must be 32 bytes".to_string())?;
                let (send, recv) = derive_video_keys(secret, VideoKeyRole::Client);
                Ok(RecordProtection::aead(send, recv))
            }
            TransportMode::ExternalSecureLink => Ok(RecordProtection::clear()),
        }
    }

    fn auth_record(
        self,
        record: &mut RecordProtection,
        session_id: SessionId,
        stream_id: StreamId,
        role: VideoRole,
    ) -> Result<Vec<u8>, String> {
        match self.mode {
            TransportMode::NativeEncrypted => record
                .seal_next(CHANNEL_VIDEO, AUTH_PAYLOAD)
                .map_err(|error| error.to_string()),
            TransportMode::ExternalSecureLink => {
                Ok(
                    video::video_auth_proof(&self.auth_key, session_id, stream_id, role, self.mode)
                        .to_vec(),
                )
            }
        }
    }
}

/// Brings up a dedicated video connection from the client side: connects, writes
/// the magic, hello, and auth record, then reads the ack. Returns the stream,
/// record protection, and any bytes already buffered past the ack.
fn open_video_connection(
    addr: &str,
    session_id: SessionId,
    stream_id: StreamId,
    role: VideoRole,
    secret: &[u8],
    transport: VideoTransport,
) -> Result<(TcpStream, RecordProtection, RecvBuffer), String> {
    let mut record = transport.record_protection(secret)?;
    let mut stream =
        TcpStream::connect(addr).map_err(|error| format!("video connect failed: {error}"))?;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream.set_nodelay(true).ok();

    let mut out = Vec::new();
    out.extend_from_slice(&video::VIDEO_MAGIC);
    let hello = VideoHello {
        version: rpc::PROTOCOL_VERSION,
        session_id,
        stream_id,
        role,
    };
    video::write_record(&mut out, &video::encode_video_hello(&hello))
        .map_err(|error| error.to_string())?;
    let auth = transport.auth_record(&mut record, session_id, stream_id, role)?;
    video::write_record(&mut out, &auth).map_err(|error| error.to_string())?;
    stream
        .write_all(&out)
        .map_err(|error| format!("video handshake write failed: {error}"))?;
    stream.flush().map_err(|error| error.to_string())?;

    let mut recv = RecvBuffer::new();
    let ack_plain = read_one_plain_record(&mut stream, &mut recv, &mut record)?;
    match video::decode_video_ack(&ack_plain)? {
        VideoAck::Ok => {}
        VideoAck::Rejected => return Err("video stream rejected by server".to_string()),
    }
    Ok((stream, record, recv))
}

/// Minimum spare receive capacity ensured before each video socket read.
const VIDEO_READ_CHUNK_BYTES: usize = 64 * 1024;

/// Reads from `stream` into `recv` until one whole record is buffered, opening
/// it in place and returning the plaintext. Bytes past the record stay
/// buffered for the caller.
fn read_one_plain_record(
    stream: &mut TcpStream,
    recv: &mut RecvBuffer,
    record_protection: &mut RecordProtection,
) -> Result<Vec<u8>, String> {
    loop {
        let total = match video::parse_record(recv.pending()) {
            Ok(Some((_, total))) => total,
            Ok(None) => {
                let read = recv
                    .fill(stream, VIDEO_READ_CHUNK_BYTES)
                    .map_err(|error| format!("video read failed: {error}"))?;
                if read == 0 {
                    return Err("video connection closed".to_string());
                }
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        let record = &mut recv.pending_mut()[video::VIDEO_LENGTH_PREFIX_LEN..total];
        let plaintext = record_protection
            .open_next_in_place(CHANNEL_VIDEO, record)
            .map_err(|error| format!("video record open failed: {error}"))?
            .to_vec();
        recv.consume(total);
        return Ok(plaintext);
    }
}
