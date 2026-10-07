//! Outbound TCP dials that a stop request can interrupt.
//!
//! A blocking `connect`, `getaddrinfo`, or handshake read pins its thread for
//! the full timeout, so an owner that joins the thread on shutdown freezes for
//! that long whenever the server is down. [`Dial`] does every wait in a mio
//! [`Poll`] instead, which the owner's [`Waker`] interrupts.

use std::{
    io::{self, Read, Write},
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use mio::{Events, Interest, Poll, Token, Waker, net::TcpStream};

/// Token the waker of a [`DialPoll`] is registered with.
const WAKE: Token = Token(1);
/// Token the socket under dial is registered with. It is deregistered before
/// [`Dial::connect`] fails, and the caller deregisters it once dialing is done.
pub const DIAL: Token = Token(0);
/// Upper bound on name resolution.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

/// A poll and the waker that interrupts it, for a thread that has no event
/// loop of its own to dial from.
pub struct DialPoll {
    pub poll: Poll,
    pub waker: Arc<Waker>,
}

impl DialPoll {
    pub fn new() -> io::Result<Self> {
        let poll = Poll::new()?;
        let waker = Arc::new(Waker::new(poll.registry(), WAKE)?);
        Ok(Self { poll, waker })
    }

    pub fn dial<'a>(&'a mut self, stopping: Option<&'a AtomicBool>) -> Dial<'a> {
        Dial::new(&mut self.poll, &self.waker, stopping)
    }
}

/// Waits on socket readiness and name resolution for one dial while staying
/// responsive to a stop request.
///
/// `stopping` is raised by the owner before it wakes `waker`; every wait
/// checks it on return from `poll`, so a stop takes effect immediately
/// instead of after the connect, DNS, or handshake timeout.
pub struct Dial<'a> {
    pub poll: &'a mut Poll,
    waker: &'a Arc<Waker>,
    stopping: Option<&'a AtomicBool>,
    events: Events,
}

pub enum DialWait {
    Woken,
    TimedOut,
}

/// A socket error, or a stop request that ended a wait.
pub enum DialError {
    Io(io::Error),
    Stopped,
}

impl From<io::Error> for DialError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl<'a> Dial<'a> {
    pub fn new(
        poll: &'a mut Poll,
        waker: &'a Arc<Waker>,
        stopping: Option<&'a AtomicBool>,
    ) -> Self {
        Self {
            poll,
            waker,
            stopping,
            events: Events::with_capacity(8),
        }
    }

    fn check_stopping(&self) -> Result<(), DialError> {
        match self.stopping {
            Some(stopping) if stopping.load(Ordering::Acquire) => Err(DialError::Stopped),
            _ => Ok(()),
        }
    }

    /// Blocks until a registered source or the waker fires, or `deadline`
    /// passes. Callers retry their nonblocking operation after `Woken`.
    pub fn wait(&mut self, deadline: Instant) -> Result<DialWait, DialError> {
        self.check_stopping()?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(DialWait::TimedOut);
        };
        if let Err(error) = self.poll.poll(&mut self.events, Some(remaining))
            && error.kind() != io::ErrorKind::Interrupted
        {
            return Err(error.into());
        }
        self.check_stopping()?;
        Ok(DialWait::Woken)
    }

    /// Waits out `deadline` unless a stop request arrives first.
    pub fn sleep_until(&mut self, deadline: Instant) -> Result<(), DialError> {
        loop {
            if let DialWait::TimedOut = self.wait(deadline)? {
                return Ok(());
            }
        }
    }

    /// Resolves `endpoint`, never blocking past a stop request.
    ///
    /// A literal socket address resolves inline. A host name goes to a
    /// detached thread because `getaddrinfo` cannot be interrupted; an
    /// abandoned lookup finishes on its own and its result is discarded.
    pub fn resolve(&mut self, endpoint: &str) -> Result<Vec<SocketAddr>, DialError> {
        if let Ok(address) = endpoint.parse::<SocketAddr>() {
            return Ok(vec![address]);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let waker = Arc::clone(self.waker);
        let lookup = endpoint.to_string();
        thread::Builder::new()
            .name("chatt-resolve".to_string())
            .stack_size(256 * 1024)
            .spawn(move || {
                let result = lookup
                    .to_socket_addrs()
                    .map(|addresses| addresses.collect::<Vec<_>>());
                let _ = tx.send(result);
                let _ = waker.wake();
            })?;
        let deadline = Instant::now() + RESOLVE_TIMEOUT;
        loop {
            match rx.try_recv() {
                Ok(Ok(addresses)) if addresses.is_empty() => {
                    return Err(io::Error::other(format!(
                        "{endpoint}: no socket addresses resolved"
                    ))
                    .into());
                }
                Ok(Ok(addresses)) => return Ok(addresses),
                Ok(Err(error)) => {
                    return Err(io::Error::new(error.kind(), format!("{endpoint}: {error}")).into());
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(io::Error::other(format!("{endpoint}: resolver exited")).into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if let DialWait::TimedOut = self.wait(deadline)? {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{endpoint}: name resolution timed out"),
                )
                .into());
            }
        }
    }

    /// Connects to `address`, leaving the stream registered as [`DIAL`] for
    /// the handshake that follows.
    pub fn connect(
        &mut self,
        address: SocketAddr,
        deadline: Instant,
    ) -> Result<TcpStream, DialError> {
        let mut stream = TcpStream::connect(address)?;
        self.poll
            .registry()
            .register(&mut stream, DIAL, Interest::READABLE | Interest::WRITABLE)?;
        let result = self.await_connected(&stream, deadline);
        if result.is_err() {
            let _ = self.poll.registry().deregister(&mut stream);
        }
        result.map(|()| stream)
    }

    fn await_connected(
        &mut self,
        stream: &TcpStream,
        deadline: Instant,
    ) -> Result<(), DialError> {
        loop {
            if let Some(error) = stream.take_error()? {
                return Err(error.into());
            }
            match stream.peer_addr() {
                Ok(_) => return Ok(()),
                Err(error) if is_connect_pending(&error) => {}
                Err(error) => return Err(error.into()),
            }
            if let DialWait::TimedOut = self.wait(deadline)? {
                return Err(io::Error::from(io::ErrorKind::TimedOut).into());
            }
        }
    }

    pub fn write_all(
        &mut self,
        stream: &mut TcpStream,
        mut bytes: &[u8],
        deadline: Instant,
    ) -> Result<(), DialError> {
        while !bytes.is_empty() {
            match stream.write(bytes) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(written) => bytes = &bytes[written..],
                Err(error) => self.retry_after(error, deadline)?,
            }
        }
        Ok(())
    }

    pub fn read_exact(
        &mut self,
        stream: &mut TcpStream,
        mut buf: &mut [u8],
        deadline: Instant,
    ) -> Result<(), DialError> {
        while !buf.is_empty() {
            match stream.read(buf) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
                Ok(read) => buf = &mut buf[read..],
                Err(error) => self.retry_after(error, deadline)?,
            }
        }
        Ok(())
    }

    /// Absorbs a nonblocking socket error that only means "not yet", waiting
    /// for readiness when needed, and fails on anything else or on timeout.
    pub fn retry_after(
        &mut self,
        error: io::Error,
        deadline: Instant,
    ) -> Result<(), DialError> {
        match error.kind() {
            io::ErrorKind::Interrupted => Ok(()),
            io::ErrorKind::WouldBlock => match self.wait(deadline)? {
                DialWait::Woken => Ok(()),
                DialWait::TimedOut => Err(io::Error::from(io::ErrorKind::TimedOut).into()),
            },
            _ => Err(error.into()),
        }
    }
}

/// Whether `peer_addr` failed only because a nonblocking connect has not
/// completed yet.
fn is_connect_pending(error: &io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::EINPROGRESS) {
        return true;
    }
    error.kind() == io::ErrorKind::NotConnected
}
