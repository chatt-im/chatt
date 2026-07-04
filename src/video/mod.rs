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

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use rpc::{
    crypto::{CHANNEL_VIDEO, KEY_LEN, TransportCipher, VideoKeyRole, derive_video_keys},
    ids::{SessionId, StreamId},
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
) -> Result<(TcpStream, TransportCipher, Vec<u8>), String> {
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

    let mut buf = Vec::new();
    let ack_record = read_one_record(&mut stream, &mut buf)?;
    let ack_plain = cipher
        .open_next(CHANNEL_VIDEO, &ack_record)
        .map_err(|error| format!("video ack open failed: {error}"))?;
    match video::decode_video_ack(&ack_plain)? {
        VideoAck::Ok => {}
        VideoAck::Rejected => return Err("video stream rejected by server".to_string()),
    }
    Ok((stream, cipher, buf))
}

/// Reads from `stream` into `buf` until one whole record is buffered, returning
/// it. Leftover bytes stay in `buf` for the caller.
fn read_one_record(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Vec<u8>, String> {
    loop {
        if let Some(record) = video::pop_record(buf).map_err(|error| error.to_string())? {
            return Ok(record);
        }
        let mut tmp = [0u8; 64 * 1024];
        let read = stream
            .read(&mut tmp)
            .map_err(|error| format!("video read failed: {error}"))?;
        if read == 0 {
            return Err("video connection closed".to_string());
        }
        buf.extend_from_slice(&tmp[..read]);
    }
}
