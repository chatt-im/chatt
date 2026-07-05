//! Screen-share video: capture, publish, and subscribe over dedicated encrypted
//! TCP connections, outside the latency-sensitive `client_net` mio loop.
//!
//! A dedicated connection opens with [`rpc::video::VIDEO_MAGIC`], a clear
//! [`VideoHello`], and a sealed auth record proving possession of the per-stream
//! secret, then carries sealed frames (publisher up, server down). Each
//! connection runs on its own blocking thread so a slow video link never stalls
//! voice and restarts are cheap.

pub mod capture;
mod nut;
mod publisher;
mod subscriber;

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use rpc::{
    crypto::{CHANNEL_VIDEO, KEY_LEN, TransportCipher, VideoKeyRole, derive_video_keys},
    ids::{SessionId, StreamId},
    recv::RecvBuffer,
    video::{self, VideoAck, VideoHello, VideoRole},
};

pub use publisher::{ScreencastHandle, start as start_screencast};
pub use subscriber::{SubscriberHandle, start as start_subscriber};

/// Fixed plaintext for the auth record. Its contents are irrelevant, opening it
/// is what proves the peer derived the per-stream key.
const AUTH_PAYLOAD: &[u8] = b"chatt-video-auth-v1";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Brings up a dedicated video connection from the client side: connects, writes
/// the magic, hello, and sealed auth, then reads the sealed ack. Returns the
/// stream, the transport cipher, and any bytes already buffered past the ack.
fn open_video_connection(
    addr: &str,
    session_id: SessionId,
    stream_id: StreamId,
    role: VideoRole,
    secret: &[u8],
) -> Result<(TcpStream, TransportCipher, RecvBuffer), String> {
    let secret: &[u8; KEY_LEN] = secret
        .try_into()
        .map_err(|_| "video secret must be 32 bytes".to_string())?;
    let (send, recv) = derive_video_keys(secret, VideoKeyRole::Client);
    let mut cipher = TransportCipher::new(send, recv);

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
    let auth = cipher
        .seal_next(CHANNEL_VIDEO, AUTH_PAYLOAD)
        .map_err(|error| error.to_string())?;
    video::write_record(&mut out, &auth).map_err(|error| error.to_string())?;
    stream
        .write_all(&out)
        .map_err(|error| format!("video handshake write failed: {error}"))?;
    stream.flush().map_err(|error| error.to_string())?;

    let mut recv = RecvBuffer::new();
    let ack_plain = read_one_plain_record(&mut stream, &mut recv, &mut cipher)?;
    match video::decode_video_ack(&ack_plain)? {
        VideoAck::Ok => {}
        VideoAck::Rejected => return Err("video stream rejected by server".to_string()),
    }
    Ok((stream, cipher, recv))
}

/// Minimum spare receive capacity ensured before each video socket read.
const VIDEO_READ_CHUNK_BYTES: usize = 64 * 1024;

/// Reads from `stream` into `recv` until one whole record is buffered, opening
/// it in place and returning the plaintext. Bytes past the record stay
/// buffered for the caller.
fn read_one_plain_record(
    stream: &mut TcpStream,
    recv: &mut RecvBuffer,
    cipher: &mut TransportCipher,
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
        let plaintext = cipher
            .open_next_in_place(CHANNEL_VIDEO, record)
            .map_err(|error| format!("video record open failed: {error}"))?
            .to_vec();
        recv.consume(total);
        return Ok(plaintext);
    }
}
