use std::io;
use std::sync::Arc;

use chatt_platform::RawSock;

/// Wakes the event loop from another thread.
///
/// The event loop polls the read end. When an I/O-pool worker finishes a task
/// it calls [`WakeHandle::wake`], which makes a blocked poll return immediately
/// so the result is picked up without waiting for a timeout. Without this, a
/// file response would stall until the poll timeout elapsed (a ~10ms-per-request
/// latency at low concurrency).
///
/// On Unix this is the classic self-pipe. On Windows `WSAPoll` accepts only
/// sockets, so it is instead a UDP socket bound to the loopback interface and
/// connected to itself: a datagram sent to it makes it readable, which is the
/// same signal with a pollable descriptor.
pub(crate) struct Waker {
    inner: Inner,
}

#[cfg(unix)]
struct Inner {
    read: std::os::fd::OwnedFd,
    write: Arc<std::os::fd::OwnedFd>,
}

#[cfg(windows)]
struct Inner {
    /// Read and write are the same self-connected socket; the `Arc` is what the
    /// cloneable handle shares.
    sock: Arc<std::net::UdpSocket>,
}

impl Waker {
    #[cfg(unix)]
    pub(crate) fn new() -> io::Result<Self> {
        use std::os::fd::FromRawFd;

        let mut fds = [0 as libc::c_int; 2];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let rc = unsafe {
            let rc = libc::pipe(fds.as_mut_ptr());
            if rc == 0 {
                libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK);
                libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
            }
            rc
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe/pipe2 succeeded, so both fds are valid and owned by us.
        let read = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { std::os::fd::OwnedFd::from_raw_fd(fds[1]) };
        Ok(Self {
            inner: Inner {
                read,
                write: Arc::new(write),
            },
        })
    }

    #[cfg(windows)]
    pub(crate) fn new() -> io::Result<Self> {
        // Port 0 lets the OS pick; binding to loopback keeps the socket
        // unreachable from off-machine. Connecting it to its own address means
        // `send` needs no address and `recv` accepts only our own datagrams.
        let sock = std::net::UdpSocket::bind(("127.0.0.1", 0))?;
        let addr = sock.local_addr()?;
        sock.connect(addr)?;
        sock.set_nonblocking(true)?;
        Ok(Self {
            inner: Inner {
                sock: Arc::new(sock),
            },
        })
    }

    /// The descriptor the event loop polls for readability.
    pub(crate) fn read_sock(&self) -> RawSock {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            self.inner.read.as_raw_fd()
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            self.inner.sock.as_raw_socket()
        }
    }

    /// A cheap, cloneable handle that worker threads use to wake the loop.
    pub(crate) fn notifier(&self) -> WakeHandle {
        #[cfg(unix)]
        {
            WakeHandle {
                write: Arc::clone(&self.inner.write),
            }
        }
        #[cfg(windows)]
        {
            WakeHandle {
                sock: Arc::clone(&self.inner.sock),
            }
        }
    }

    /// Drain all pending wakeups (the read end is non-blocking).
    pub(crate) fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let n = unsafe {
                    libc::read(
                        self.inner.read.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                    )
                };
                if n <= 0 {
                    break;
                }
            }
            #[cfg(windows)]
            {
                // Each wakeup is one datagram, so drain until the socket is
                // empty. A datagram larger than the buffer cannot occur: only
                // `wake` writes here, and it sends a single byte.
                if self.inner.sock.recv(&mut buf).is_err() {
                    break;
                }
            }
        }
    }
}

/// A cheap, cloneable handle that wakes a blocked [`Server::poll_once`] from
/// another thread.
///
/// The server's event loop blocks in its poll call. Calling [`WakeHandle::wake`]
/// signals the loop's waker, which makes the blocked poll return so the
/// producer's work (a queued WebSocket frame, a finished file response) is
/// handled without waiting for a timeout.
///
/// [`Server::poll_once`]: crate::Server::poll_once
#[derive(Clone)]
pub struct WakeHandle {
    #[cfg(unix)]
    write: Arc<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    sock: Arc<std::net::UdpSocket>,
}

impl WakeHandle {
    /// Wakes the server's poll loop.
    pub fn wake(&self) {
        // Non-blocking write. A full pipe or socket buffer means a wakeup is
        // already pending, which is exactly what we want, so the error is
        // ignored.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let byte = [1u8];
            let _ = unsafe {
                libc::write(
                    self.write.as_raw_fd(),
                    byte.as_ptr() as *const libc::c_void,
                    1,
                )
            };
        }
        #[cfg(windows)]
        {
            let _ = self.sock.send(&[1u8]);
        }
    }
}
