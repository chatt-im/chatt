//! The server's local control channel.
//!
//! `chatt-server invite` and the MLS maintenance subcommands reach a running
//! server over a same-machine, same-user channel: a Unix-domain socket in a
//! private directory, or a named pipe on Windows.
//!
//! Everything above that channel is here and shared. The wire format is a fixed
//! magic, a one-byte opcode or status, a big-endian `u32` length, then the
//! body. It has to stay identical between the two transports, and the surest way to
//! keep it identical is for there to be one copy of it. What differs is only
//! "read or write exactly N bytes by a deadline", which is what
//! [`ControlStream`] names.

use std::{
    sync::mpsc::{self, Sender},
    time::{Duration, Instant},
};

const MAGIC: &[u8] = b"chatt-server-control-v1\0";
const OP_INVITE: u8 = 1;
const OP_MLS_STORAGE_STATUS: u8 = 2;
const OP_MLS_COMPACT: u8 = 3;
const STATUS_OK: u8 = 0;
const STATUS_ERROR: u8 = 1;
const MAX_REQUEST_BYTES: u32 = 1024;
const MAX_RESPONSE_BYTES: u32 = 8 * 1024;
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);
const STREAM_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a client waits for the response once its request is written.
///
/// Longer than the server's [`STREAM_TIMEOUT`] wait for the event loop plus its
/// own response write, so a slow command such as `MlsCompact` reaches the
/// client as the server's answer rather than as a client-side timeout.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A request from the control channel, consumed by the server event loop.
pub enum AdminCommand {
    Invite {
        user: String,
        reply: Sender<Result<String, String>>,
    },
    MlsStorageStatus {
        reply: Sender<Result<String, String>>,
    },
    MlsCompact {
        reply: Sender<Result<String, String>>,
    },
    /// Stops the server event loop. Used by embedded lifecycle owners such as
    /// the in-process end-to-end test harness.
    Shutdown,
}

/// One end of a control connection, as the framing needs it.
///
/// Deadlines rather than lengths-and-blocking, because a peer that connects and
/// then trickles or goes silent must not hold the single-threaded control
/// worker. Unix enforces them with `poll`; Windows with overlapped I/O and a
/// timed wait.
trait ControlStream {
    fn read_exact_by(&self, buf: &mut [u8], deadline: Instant, what: &str) -> Result<(), String>;
    fn write_all_by(&self, buf: &[u8], deadline: Instant, what: &str) -> Result<(), String>;
}

struct Response {
    status: u8,
    message: String,
}

fn read_frame(
    stream: &impl ControlStream,
    max_len: u32,
    what: &str,
    deadline: Instant,
) -> Result<(u8, Vec<u8>), String> {
    let mut header = [0u8; MAGIC.len() + 5];
    stream.read_exact_by(&mut header, deadline, what)?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(format!("invalid server control {what} magic"));
    }
    let tag = header[MAGIC.len()];
    let len = u32::from_be_bytes(header[MAGIC.len() + 1..].try_into().expect("4 bytes"));
    if len > max_len {
        return Err(format!("server control {what} exceeds {max_len} bytes"));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact_by(&mut body, deadline, what)?;
    Ok((tag, body))
}

fn write_frame(
    stream: &impl ControlStream,
    tag: u8,
    body: &[u8],
    max_len: u32,
    what: &str,
    deadline: Instant,
) -> Result<(), String> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|len| *len <= max_len)
        .ok_or_else(|| format!("server control {what} exceeds {max_len} bytes"))?;
    let mut frame = Vec::with_capacity(MAGIC.len() + 5 + body.len());
    frame.extend_from_slice(MAGIC);
    frame.push(tag);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(body);
    stream.write_all_by(&frame, deadline, what)
}

/// Serves one connection: reads a request, runs it through the event loop, and
/// writes the reply.
fn handle_connection(stream: &impl ControlStream, commands: &AdminSender) {
    let deadline = Instant::now() + STREAM_TIMEOUT;
    let response = match read_frame(stream, MAX_REQUEST_BYTES, "request", deadline) {
        Ok((OP_INVITE, body)) => match String::from_utf8(body) {
            Ok(user) => dispatch(commands, |reply| AdminCommand::Invite { user, reply }),
            Err(error) => Response {
                status: STATUS_ERROR,
                message: format!("invite user is not UTF-8: {error}"),
            },
        },
        Ok((opcode @ (OP_MLS_STORAGE_STATUS | OP_MLS_COMPACT), body)) if body.is_empty() => {
            if opcode == OP_MLS_STORAGE_STATUS {
                dispatch(commands, |reply| AdminCommand::MlsStorageStatus { reply })
            } else {
                dispatch(commands, |reply| AdminCommand::MlsCompact { reply })
            }
        }
        Ok((opcode, _)) => Response {
            status: STATUS_ERROR,
            message: format!("unknown server control opcode {opcode}"),
        },
        Err(error) => {
            kvlog::warn!("server control request failed", error = error.as_str());
            Response {
                status: STATUS_ERROR,
                message: error,
            }
        }
    };
    if let Err(error) = write_frame(
        stream,
        response.status,
        response.message.as_bytes(),
        MAX_RESPONSE_BYTES,
        "response",
        Instant::now() + STREAM_TIMEOUT,
    ) {
        kvlog::warn!("server control response failed", error = error.as_str());
    }
}

/// Sends one command to the server loop and waits for its reply.
fn dispatch(
    commands: &AdminSender,
    build: impl FnOnce(Sender<Result<String, String>>) -> AdminCommand,
) -> Response {
    let (reply_tx, reply_rx) = mpsc::channel();
    match commands.send(build(reply_tx)) {
        Ok(()) => match reply_rx.recv_timeout(STREAM_TIMEOUT) {
            Ok(Ok(message)) => Response {
                status: STATUS_OK,
                message,
            },
            Ok(Err(message)) => Response {
                status: STATUS_ERROR,
                message,
            },
            Err(error) => {
                kvlog::warn!("server control request was not answered", error = %error);
                Response {
                    status: STATUS_ERROR,
                    message: format!("server did not answer control request: {error}"),
                }
            }
        },
        Err(_) => Response {
            status: STATUS_ERROR,
            message: "chatt server is not running".to_string(),
        },
    }
}

/// Runs one request/response exchange from the client side.
fn request(stream: &impl ControlStream, opcode: u8, body: &[u8]) -> Result<String, String> {
    let deadline = Instant::now() + STREAM_TIMEOUT;
    write_frame(stream, opcode, body, MAX_REQUEST_BYTES, "request", deadline)?;
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    let (status, body) = read_frame(stream, MAX_RESPONSE_BYTES, "response", deadline)?;
    let message = String::from_utf8(body)
        .map_err(|error| format!("server control response is not UTF-8: {error}"))?;
    match status {
        STATUS_OK => Ok(message),
        STATUS_ERROR => Err(message),
        status => Err(format!(
            "server control socket returned unknown status {status}"
        )),
    }
}

pub fn send_invite(user: &str) -> Result<String, String> {
    request(&imp::connect()?, OP_INVITE, user.as_bytes())
}

pub fn mls_storage_status() -> Result<String, String> {
    request(&imp::connect()?, OP_MLS_STORAGE_STATUS, &[])
}

pub fn mls_compact() -> Result<String, String> {
    request(&imp::connect()?, OP_MLS_COMPACT, &[])
}

#[cfg(unix)]
#[path = "local_admin_unix.rs"]
mod imp;

/// Windows serves the same control protocol over a named pipe; see the module
/// for how its access control compares to the Unix socket's.
#[cfg(windows)]
#[path = "local_admin_windows.rs"]
mod imp;

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::path::Path;

    pub struct AdminSocket;

    impl AdminSocket {
        pub(crate) fn spawn(_commands: super::AdminSender) -> Result<Self, String> {
            Err("chatt server control sockets are only supported on Unix and Windows".to_string())
        }

        pub(crate) fn path(&self) -> &Path {
            Path::new("")
        }
    }

    /// Uninhabited: [`connect`] never returns one.
    pub(super) enum Stream {}

    impl super::ControlStream for Stream {
        fn read_exact_by(
            &self,
            _buf: &mut [u8],
            _deadline: std::time::Instant,
            _what: &str,
        ) -> Result<(), String> {
            match *self {}
        }

        fn write_all_by(
            &self,
            _buf: &[u8],
            _deadline: std::time::Instant,
            _what: &str,
        ) -> Result<(), String> {
            match *self {}
        }
    }

    pub(super) fn connect() -> Result<Stream, String> {
        Err("chatt-server control commands are only supported on Unix and Windows".to_string())
    }
}

pub use imp::AdminSocket;

#[derive(Clone)]
pub struct AdminSender {
    events: std::sync::Arc<crate::event_queue::EventQueue<AdminCommand>>,
}

impl AdminSender {
    pub(crate) fn new(
        events: std::sync::Arc<crate::event_queue::EventQueue<AdminCommand>>,
    ) -> Self {
        Self { events }
    }

    pub fn send(&self, command: AdminCommand) -> Result<(), AdminCommand> {
        self.events.push(command);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory `ControlStream` over a fixed script of bytes, so the
    /// framing can be exercised without either platform's transport.
    struct Scripted {
        input: std::cell::RefCell<std::collections::VecDeque<u8>>,
        output: std::cell::RefCell<Vec<u8>>,
    }

    impl Scripted {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input: std::cell::RefCell::new(input.into()),
                output: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl ControlStream for Scripted {
        fn read_exact_by(
            &self,
            buf: &mut [u8],
            _deadline: Instant,
            what: &str,
        ) -> Result<(), String> {
            let mut input = self.input.borrow_mut();
            if input.len() < buf.len() {
                return Err(format!("server control {what} ended early"));
            }
            for slot in buf.iter_mut() {
                *slot = input.pop_front().expect("length checked above");
            }
            Ok(())
        }

        fn write_all_by(&self, buf: &[u8], _deadline: Instant, _what: &str) -> Result<(), String> {
            self.output.borrow_mut().extend_from_slice(buf);
            Ok(())
        }
    }

    fn framed(tag: u8, body: &[u8]) -> Vec<u8> {
        let stream = Scripted::new(Vec::new());
        write_frame(
            &stream,
            tag,
            body,
            MAX_RESPONSE_BYTES,
            "request",
            Instant::now() + STREAM_TIMEOUT,
        )
        .expect("write frame");
        stream.output.take()
    }

    #[test]
    fn frame_round_trips_request_and_response_shapes() {
        let stream = Scripted::new(framed(OP_INVITE, b"alice"));

        let (opcode, body) = read_frame(
            &stream,
            MAX_REQUEST_BYTES,
            "request",
            Instant::now() + STREAM_TIMEOUT,
        )
        .expect("read frame");

        assert_eq!(opcode, OP_INVITE);
        assert_eq!(body, b"alice");
    }

    #[test]
    fn read_frame_rejects_oversized_length() {
        let stream = Scripted::new(framed(
            OP_INVITE,
            &vec![0u8; MAX_REQUEST_BYTES as usize + 1],
        ));

        let result = read_frame(
            &stream,
            MAX_REQUEST_BYTES,
            "request",
            Instant::now() + STREAM_TIMEOUT,
        );

        assert!(result.is_err());
    }

    #[test]
    fn read_frame_rejects_a_foreign_magic() {
        let mut bytes = framed(OP_INVITE, b"alice");
        bytes[0] = b'x';
        let stream = Scripted::new(bytes);

        let error = read_frame(
            &stream,
            MAX_REQUEST_BYTES,
            "request",
            Instant::now() + STREAM_TIMEOUT,
        )
        .expect_err("a foreign magic must be rejected");

        assert!(error.contains("magic"));
    }

    #[test]
    fn write_frame_rejects_an_oversized_body() {
        let stream = Scripted::new(Vec::new());

        let error = write_frame(
            &stream,
            STATUS_OK,
            &vec![0u8; MAX_RESPONSE_BYTES as usize + 1],
            MAX_RESPONSE_BYTES,
            "response",
            Instant::now() + STREAM_TIMEOUT,
        )
        .expect_err("an oversized body must not be framed");

        assert!(error.contains("exceeds"));
        assert!(stream.output.borrow().is_empty());
    }

    #[test]
    fn an_unknown_opcode_is_answered_with_an_error_status() {
        let events = std::sync::Arc::new(crate::event_queue::EventQueue::new(
            std::sync::Arc::new(crate::event_queue::EventNotifier::new(std::sync::Arc::new(
                mio::Waker::new(mio::Poll::new().unwrap().registry(), mio::Token(9)).unwrap(),
            ))),
            crate::event_queue::ADMIN_EVENTS,
            "admin-test",
        ));
        let stream = Scripted::new(framed(200, b""));

        handle_connection(&stream, &AdminSender::new(events));

        let reply = Scripted::new(stream.output.take());
        let (status, body) = read_frame(
            &reply,
            MAX_RESPONSE_BYTES,
            "response",
            Instant::now() + STREAM_TIMEOUT,
        )
        .expect("read reply");
        assert_eq!(status, STATUS_ERROR);
        assert!(String::from_utf8(body).unwrap().contains("unknown"));
    }
}
