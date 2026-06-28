use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

/// Self-pipe used to wake the `poll(2)` loop from another thread.
///
/// The event loop polls the read end. When an I/O-pool worker finishes a task
/// it calls [`Notifier::notify`], which writes a byte to the pipe; that makes a
/// blocked `poll` return immediately so the result is picked up without waiting
/// for a timeout. Without this, a file response would stall until the poll
/// timeout elapsed (a ~10ms-per-request latency at low concurrency).
pub(crate) struct Waker {
    read: OwnedFd,
    write: Arc<OwnedFd>,
}

impl Waker {
    pub(crate) fn new() -> io::Result<Self> {
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
        // SAFETY: pipe2 succeeded, so both fds are valid and owned by us.
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        Ok(Self {
            read,
            write: Arc::new(write),
        })
    }

    pub(crate) fn read_fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    /// A cheap, cloneable handle that worker threads use to wake the loop.
    pub(crate) fn notifier(&self) -> WakeHandle {
        WakeHandle {
            write: Arc::clone(&self.write),
        }
    }

    /// Drain all pending wakeup bytes (the read end is non-blocking).
    pub(crate) fn drain(&self) {
        let mut buf = [0u8; 64];
        loop {
            let n = unsafe {
                libc::read(
                    self.read.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
        }
    }
}

/// A cheap, cloneable handle that wakes a blocked [`Server::poll_once`] from
/// another thread.
///
/// The server's event loop blocks in `poll(2)`. Calling [`WakeHandle::wake`]
/// writes a byte to the loop's self-pipe, which makes the blocked poll return so
/// the producer's work (a queued WebSocket frame, a finished file response) is
/// handled without waiting for a timeout.
///
/// [`Server::poll_once`]: crate::Server::poll_once
#[derive(Clone)]
pub struct WakeHandle {
    write: Arc<OwnedFd>,
}

impl WakeHandle {
    /// Wakes the server's poll loop.
    pub fn wake(&self) {
        let byte = [1u8];
        // Non-blocking write; a full pipe (EAGAIN) means a wakeup is already
        // pending, which is exactly what we want, so the error is ignored.
        let _ = unsafe {
            libc::write(
                self.write.as_raw_fd(),
                byte.as_ptr() as *const libc::c_void,
                1,
            )
        };
    }
}
