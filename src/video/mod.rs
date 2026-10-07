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

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use crate::dial::{Dial, DialError};
use rpc::{
    crypto::{
        CHANNEL_VIDEO, KEY_LEN, RecordProtection, TransportMode, VideoKeyRole, derive_video_keys,
    },
    ids::{SessionId, StreamId},
    recv::RecvBuffer,
    video::{self, VideoAck, VideoHello, VideoRole},
};

/// The in-process consumer end of a native viewer's frame queue.
#[cfg(renderer_clients)]
pub use fanout::LiveFrames;
/// Handle to a renderer client viewing a share; see the note in `runtime.rs`.
#[cfg(renderer_clients)]
pub use fanout::NativeViewerHandle;
pub use fanout::VideoFrameFanout;
/// The NUT muxer exists for `chatt video-stream`, which is Unix-only.
#[cfg(unix)]
pub(crate) use nut::NutPipeMuxer;
pub use publisher::{ScreencastHandle, start as start_screencast};
pub use subscriber::{SubscriberHandle, start as start_subscriber};

/// Fixed plaintext for the auth record. Its contents are irrelevant, opening it
/// is what proves the peer derived the per-stream key.
const AUTH_PAYLOAD: &[u8] = b"chatt-video-auth-v1";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on a single connect attempt, so an unreachable address fails in
/// seconds rather than waiting out the kernel's SYN retries. A teardown never
/// waits on it: every video thread is joined from the render loop, so its
/// stop request interrupts the connect and handshake waits.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Deliberately not `Debug`: it carries the session's video auth key, and the
/// derived form would print it into any log line that formatted a transport.
#[derive(Clone, Copy)]
pub struct VideoTransport {
    peer_addr: SocketAddr,
    mode: TransportMode,
    auth_key: [u8; KEY_LEN],
}

impl VideoTransport {
    pub fn new(peer_addr: SocketAddr, mode: TransportMode, auth_key: [u8; KEY_LEN]) -> Self {
        Self {
            peer_addr,
            mode,
            auth_key,
        }
    }

    pub(crate) fn peer_addr(self) -> SocketAddr {
        self.peer_addr
    }

    fn record_protection(self, secret: &[u8]) -> Result<RecordProtection, String> {
        match self.mode {
            TransportMode::Encrypted => {
                let secret: &[u8; KEY_LEN] = secret
                    .try_into()
                    .map_err(|_| "video secret must be 32 bytes".to_string())?;
                let (send, recv) = derive_video_keys(secret, VideoKeyRole::Client);
                Ok(RecordProtection::aead(send, recv))
            }
            TransportMode::Plaintext => Ok(RecordProtection::clear()),
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
            TransportMode::Encrypted => record
                .seal_next(CHANNEL_VIDEO, AUTH_PAYLOAD)
                .map_err(|error| error.to_string()),
            TransportMode::Plaintext => {
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
///
/// Every wait goes through `dial`, so the owner's stop request abandons a
/// connection to an unreachable server at once instead of after the connect
/// and handshake timeouts.
fn open_video_connection(
    dial: &mut Dial,
    session_id: SessionId,
    stream_id: StreamId,
    role: VideoRole,
    secret: &[u8],
    transport: VideoTransport,
) -> Result<(TcpStream, RecordProtection, RecvBuffer), String> {
    let mut record = transport.record_protection(secret)?;
    let mut stream = dial
        .connect(transport.peer_addr(), Instant::now() + CONNECT_TIMEOUT)
        .map_err(|error| describe_dial_error("video connect failed", error))?;
    let handshake = video_handshake(
        dial,
        &mut stream,
        &mut record,
        session_id,
        stream_id,
        role,
        transport,
    );
    let _ = dial.poll.registry().deregister(&mut stream);
    let recv = handshake?;
    let stream = TcpStream::from(stream);
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| error.to_string())?;
    Ok((stream, record, recv))
}

fn video_handshake(
    dial: &mut Dial,
    stream: &mut mio::net::TcpStream,
    record: &mut RecordProtection,
    session_id: SessionId,
    stream_id: StreamId,
    role: VideoRole,
    transport: VideoTransport,
) -> Result<RecvBuffer, String> {
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
    let auth = transport.auth_record(record, session_id, stream_id, role)?;
    video::write_record(&mut out, &auth).map_err(|error| error.to_string())?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    dial.write_all(stream, &out, deadline)
        .map_err(|error| describe_dial_error("video handshake write failed", error))?;

    let mut recv = RecvBuffer::new();
    let ack_plain = read_one_plain_record(dial, stream, &mut recv, record, deadline)?;
    match video::decode_video_ack(&ack_plain)? {
        VideoAck::Ok => {}
        VideoAck::Rejected => return Err("video stream rejected by server".to_string()),
    }
    Ok(recv)
}

fn describe_dial_error(context: &str, error: DialError) -> String {
    match error {
        DialError::Io(error) => format!("{context}: {error}"),
        DialError::Stopped => "video connection stopped".to_string(),
    }
}

/// Minimum spare receive capacity ensured before each video socket read.
const VIDEO_READ_CHUNK_BYTES: usize = 64 * 1024;

/// Reads from `stream` into `recv` until one whole record is buffered, opening
/// it in place and returning the plaintext. Bytes past the record stay
/// buffered for the caller.
fn read_one_plain_record(
    dial: &mut Dial,
    stream: &mio::net::TcpStream,
    recv: &mut RecvBuffer,
    record_protection: &mut RecordProtection,
    deadline: Instant,
) -> Result<Vec<u8>, String> {
    loop {
        let total = match video::parse_record(recv.pending()) {
            Ok(Some((_, total))) => total,
            Ok(None) => {
                match recv.fill(stream, VIDEO_READ_CHUNK_BYTES) {
                    Ok(0) => return Err("video connection closed".to_string()),
                    Ok(_) => {}
                    Err(error) => dial
                        .retry_after(error, deadline)
                        .map_err(|error| describe_dial_error("video read failed", error))?,
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
