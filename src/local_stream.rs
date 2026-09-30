//! A connected byte-stream pair between the core and an in-process or local
//! frontend, and the resources a frontend frame can carry.
//!
//! The core hands frontends live-share video and memory-held attachments over a
//! stream it keeps one end of. On Unix that is a `socketpair`, which is also
//! what descriptor passing moves across the renderer socket. Windows has no
//! `socketpair`, so the pair is a loopback TCP connection: the same surface the
//! call sites use (`try_clone`, `shutdown`, blocking `Read`/`Write`,
//! `set_nonblocking`), with the peer checked so no other local process can
//! take the place of the connecting end.

#[cfg(not(unix))]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::{fd::OwnedFd, unix::net::UnixStream};

/// One end of a [`local_stream_pair`].
#[cfg(unix)]
pub type LocalStream = UnixStream;
/// One end of a [`local_stream_pair`].
#[cfg(not(unix))]
pub type LocalStream = std::net::TcpStream;

/// A resource handed to a frontend alongside a frame.
///
/// On Unix this is a descriptor, which is what the renderer socket passes with
/// `SCM_RIGHTS`.
#[cfg(unix)]
pub type FrameResource = OwnedFd;

/// A resource handed to a frontend alongside a frame.
#[cfg(not(unix))]
#[derive(Debug)]
pub enum FrameResource {
    /// An attachment already on disk, opened read-only.
    File(File),
    /// The frontend's end of a [`local_stream_pair`]: a live share's video, or
    /// a memory-held attachment served by range requests.
    Stream(LocalStream),
}

#[cfg(not(unix))]
impl From<File> for FrameResource {
    fn from(file: File) -> Self {
        Self::File(file)
    }
}

#[cfg(not(unix))]
impl From<LocalStream> for FrameResource {
    fn from(stream: LocalStream) -> Self {
        Self::Stream(stream)
    }
}

/// Converts a frame resource back into the stream it must be, or hands it back.
#[cfg(any(feature = "embedded-client", test))]
#[cfg(unix)]
pub fn into_local_stream(resource: FrameResource) -> Result<LocalStream, FrameResource> {
    Ok(UnixStream::from(resource))
}

/// Converts a frame resource back into the stream it must be, or hands it back.
#[cfg(any(feature = "embedded-client", test))]
#[cfg(not(unix))]
pub fn into_local_stream(resource: FrameResource) -> Result<LocalStream, FrameResource> {
    match resource {
        FrameResource::Stream(stream) => Ok(stream),
        other => Err(other),
    }
}

/// Creates a connected pair of local streams.
#[cfg(unix)]
pub fn local_stream_pair() -> io::Result<(LocalStream, LocalStream)> {
    UnixStream::pair()
}

/// How many loopback connections to try before giving up on a pair whose
/// accepted peer keeps turning out not to be our own connecting end.
#[cfg(not(unix))]
const PAIR_ATTEMPTS: usize = 4;

/// Creates a connected pair of local streams.
///
/// Binds an ephemeral loopback port, connects to it, accepts, and closes the
/// listener. The listener is reachable by every local process for that
/// instant, so the accepted connection is only kept when its peer address is
/// exactly the connecting socket's local address; anything else is a stranger
/// that raced us, and the whole attempt is discarded. Loopback binds raise no
/// firewall prompt.
#[cfg(not(unix))]
pub fn local_stream_pair() -> io::Result<(LocalStream, LocalStream)> {
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};

    for _ in 0..PAIR_ATTEMPTS {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let connector = TcpStream::connect(listener.local_addr()?)?;
        let (accepted, accepted_peer) = listener.accept()?;
        drop(listener);
        if !accepted_peer_is_connector(accepted_peer, connector.local_addr()?) {
            kvlog::warn!(
                "local stream pair accepted a foreign peer",
                peer = %accepted_peer
            );
            continue;
        }
        connector.set_nodelay(true)?;
        accepted.set_nodelay(true)?;
        return Ok((connector, accepted));
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "local stream pair was repeatedly intercepted by another local connection",
    ))
}

/// Whether the connection a pair's listener accepted is the pair's own
/// connecting end, rather than another process that reached the port first.
#[cfg(not(unix))]
fn accepted_peer_is_connector(
    accepted_peer: std::net::SocketAddr,
    connector_local: std::net::SocketAddr,
) -> bool {
    accepted_peer == connector_local
}

/// Stops a worker thread blocked reading or writing its [`InterruptibleStream`].
///
/// Teardown interrupts a worker and then joins it, so the interrupt has to
/// unblock whatever call the worker is parked in. On Unix `shutdown` on any
/// descriptor of the socket does that. On Windows it does not: a blocked `recv`
/// stays parked until the peer closes or the FIN_WAIT_2 timer runs out, and a
/// blocked `send` until the peer reads, so the Windows stream is nonblocking
/// and waits in a bounded poll that checks for the interrupt between waits.
#[cfg(unix)]
pub struct StreamInterrupt(LocalStream);

#[cfg(unix)]
impl StreamInterrupt {
    pub fn interrupt(&self) {
        let _ = self.0.shutdown(std::net::Shutdown::Both);
    }
}

/// The end of a local stream a worker thread owns, stoppable from another
/// thread through its [`StreamInterrupt`].
#[cfg(unix)]
pub type InterruptibleStream = LocalStream;

/// Splits a stream into the end a worker owns and the handle that stops it.
#[cfg(unix)]
pub fn interruptible(stream: LocalStream) -> io::Result<(InterruptibleStream, StreamInterrupt)> {
    let control = stream.try_clone()?;
    Ok((stream, StreamInterrupt(control)))
}

/// How long a Windows worker waits for readiness before rechecking for an
/// interrupt. It bounds how long a teardown join can take.
#[cfg(not(unix))]
const INTERRUPT_POLL_MS: i32 = 50;

/// Stops a worker thread blocked reading or writing its [`InterruptibleStream`].
///
/// See the Unix definition for why Windows needs more than `shutdown`.
#[cfg(not(unix))]
pub struct StreamInterrupt {
    stream: LocalStream,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(not(unix))]
impl StreamInterrupt {
    pub fn interrupt(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        // Still worth sending: the peer sees end of stream at once.
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// The end of a local stream a worker thread owns, stoppable from another
/// thread through its [`StreamInterrupt`].
///
/// Reads and writes block like a blocking socket's, but by polling a
/// nonblocking one: a nonblocking call that would block transfers nothing, so
/// retrying it after a poll loses and duplicates no bytes. An interrupted call
/// fails with `ConnectionAborted`.
#[cfg(not(unix))]
pub struct InterruptibleStream {
    stream: LocalStream,
    stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(not(unix))]
impl InterruptibleStream {
    pub fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        self.stream.shutdown(how)
    }

    fn retry<T>(
        &mut self,
        events: i16,
        mut operation: impl FnMut(&mut LocalStream) -> io::Result<T>,
    ) -> io::Result<T> {
        use chatt_platform::AsRawSock as _;

        loop {
            if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "local stream interrupted",
                ));
            }
            match operation(&mut self.stream) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
            let mut fds = [chatt_platform::pollfd(self.stream.as_raw_sock(), events)];
            match chatt_platform::poll(&mut fds, INTERRUPT_POLL_MS) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(not(unix))]
impl io::Read for InterruptibleStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.retry(chatt_platform::POLLIN, |stream| io::Read::read(stream, buf))
    }
}

#[cfg(not(unix))]
impl io::Write for InterruptibleStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.retry(chatt_platform::POLLOUT, |stream| io::Write::write(stream, buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Splits a stream into the end a worker owns and the handle that stops it.
///
/// The stream becomes nonblocking; that state belongs to the socket, so the
/// handle's clone shares it, which is harmless since the handle only shuts down.
#[cfg(not(unix))]
pub fn interruptible(stream: LocalStream) -> io::Result<(InterruptibleStream, StreamInterrupt)> {
    stream.set_nonblocking(true)?;
    let control = stream.try_clone()?;
    let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    Ok((
        InterruptibleStream {
            stream,
            stopped: stopped.clone(),
        },
        StreamInterrupt {
            stream: control,
            stopped,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::Shutdown,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn bytes_flow_both_ways() {
        let (mut left, mut right) = local_stream_pair().unwrap();
        left.write_all(b"ping").unwrap();
        let mut buffer = [0; 4];
        right.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"ping");
        right.write_all(b"pong").unwrap();
        left.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"pong");
    }

    #[test]
    fn shutdown_makes_the_peer_read_end_of_stream() {
        let (left, mut right) = local_stream_pair().unwrap();
        left.shutdown(Shutdown::Both).unwrap();
        let mut buffer = [0; 1];
        assert_eq!(right.read(&mut buffer).unwrap(), 0);
    }

    #[test]
    fn a_clone_writes_to_the_same_peer() {
        let (left, mut right) = local_stream_pair().unwrap();
        let mut clone = left.try_clone().unwrap();
        clone.write_all(b"via clone").unwrap();
        let mut buffer = [0; 9];
        right.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"via clone");
    }

    // Teardown interrupts an attachment range worker blocked reading its end,
    // then joins it. The join must not wait on the frontend.
    #[test]
    fn an_interrupt_wakes_a_blocked_reader() {
        let (left, _right) = local_stream_pair().unwrap();
        let (mut worker_end, interrupt) = interruptible(left).unwrap();
        let reader = thread::spawn(move || {
            let mut buffer = [0; 1];
            worker_end.read(&mut buffer)
        });
        thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        interrupt.interrupt();
        let read = reader.join().unwrap();
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    // A native video writer blocks writing when its viewer stops reading, and
    // dropping the viewer handle interrupts and joins it.
    #[test]
    fn an_interrupt_wakes_a_blocked_writer() {
        let (left, _right) = local_stream_pair().unwrap();
        let (mut worker_end, interrupt) = interruptible(left).unwrap();
        let writer = thread::spawn(move || {
            let chunk = vec![0; 64 * 1024];
            // Bounded, so a regression fails the test instead of spinning.
            for _ in 0..1024 {
                worker_end.write_all(&chunk)?;
            }
            Ok::<(), std::io::Error>(())
        });
        thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        interrupt.interrupt();
        assert!(writer.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_interruptible_stream_carries_bytes_that_arrive_across_waits() {
        let (left, mut right) = local_stream_pair().unwrap();
        let (mut worker_end, _interrupt) = interruptible(left).unwrap();
        let reader = thread::spawn(move || {
            let mut buffer = [0; 4];
            worker_end.read_exact(&mut buffer).map(|()| buffer)
        });
        right.write_all(b"ab").unwrap();
        thread::sleep(Duration::from_millis(150));
        right.write_all(b"cd").unwrap();
        assert_eq!(&reader.join().unwrap().unwrap(), b"abcd");
    }

    #[test]
    fn a_stream_resource_converts_back_to_a_stream() {
        let (left, mut right) = local_stream_pair().unwrap();
        let resource: FrameResource = left.into();
        let mut stream = into_local_stream(resource).unwrap();
        stream.write_all(b"x").unwrap();
        let mut buffer = [0; 1];
        right.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"x");
    }

    #[cfg(not(unix))]
    #[test]
    fn a_file_resource_is_not_a_stream() {
        let file = tempfile::tempfile().unwrap();
        assert!(matches!(
            into_local_stream(FrameResource::from(file)),
            Err(FrameResource::File(_))
        ));
    }

    #[cfg(not(unix))]
    #[test]
    fn the_pair_ends_are_each_others_peers() {
        let (left, right) = local_stream_pair().unwrap();
        assert_eq!(left.local_addr().unwrap(), right.peer_addr().unwrap());
        assert_eq!(right.local_addr().unwrap(), left.peer_addr().unwrap());
        assert!(left.local_addr().unwrap().ip().is_loopback());
        assert!(left.nodelay().unwrap() && right.nodelay().unwrap());
    }

    #[cfg(not(unix))]
    #[test]
    fn a_foreign_accepted_peer_is_rejected() {
        let connector: std::net::SocketAddr = "127.0.0.1:50000".parse().unwrap();
        assert!(accepted_peer_is_connector(connector, connector));
        let other_port: std::net::SocketAddr = "127.0.0.1:50001".parse().unwrap();
        assert!(!accepted_peer_is_connector(other_port, connector));
        let other_host: std::net::SocketAddr = "127.0.0.2:50000".parse().unwrap();
        assert!(!accepted_peer_is_connector(other_host, connector));
    }
}
