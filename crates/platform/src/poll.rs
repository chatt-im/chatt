//! Portable readiness polling over sockets.
//!
//! POSIX `poll(2)` and Winsock `WSAPoll` take the same
//! descriptor/events/revents triple and use the same event bits, differing only
//! in the descriptor's width and the count argument's type. This exposes the
//! platform's own struct under one name rather than converting between two
//! layouts on every poll: the loops that use it reuse their `PollFd` buffer
//! and must not allocate.
//!
//! Windows note: `WSAPoll` accepts only sockets. Anything a loop wants to wait
//! on there has to be a socket too, which is why `darkhttp`'s waker is a socket
//! pair on Windows rather than a pipe.

use std::io;

use crate::sock::RawSock;

#[cfg(unix)]
mod sys {
    pub type PollFd = libc::pollfd;

    pub const POLLIN: i16 = libc::POLLIN;
    pub const POLLOUT: i16 = libc::POLLOUT;
    pub const POLLERR: i16 = libc::POLLERR;
    pub const POLLHUP: i16 = libc::POLLHUP;

    pub fn pollfd(fd: super::RawSock, events: i16) -> PollFd {
        PollFd {
            fd,
            events,
            revents: 0,
        }
    }

    pub unsafe fn poll(fds: &mut [PollFd], timeout_ms: i32) -> i32 {
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) }
    }
}

#[cfg(windows)]
mod sys {
    use windows_sys::Win32::Networking::WinSock;

    pub type PollFd = WinSock::WSAPOLLFD;

    // Winsock spells these the same way POSIX does: POLLIN is
    // POLLRDNORM|POLLRDBAND, POLLOUT is POLLWRNORM, and POLLERR/POLLHUP are
    // output-only.
    pub const POLLIN: i16 = WinSock::POLLIN;
    pub const POLLOUT: i16 = WinSock::POLLOUT;
    pub const POLLERR: i16 = WinSock::POLLERR;
    pub const POLLHUP: i16 = WinSock::POLLHUP;

    pub fn pollfd(fd: super::RawSock, events: i16) -> PollFd {
        PollFd {
            fd: fd as WinSock::SOCKET,
            events,
            revents: 0,
        }
    }

    pub unsafe fn poll(fds: &mut [PollFd], timeout_ms: i32) -> i32 {
        unsafe { WinSock::WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms) }
    }
}

pub use sys::{POLLERR, POLLHUP, POLLIN, POLLOUT, PollFd};

/// Builds a poll entry requesting `events` on `fd`.
pub fn pollfd(fd: RawSock, events: i16) -> PollFd {
    sys::pollfd(fd, events)
}

/// Waits for readiness on `fds`, up to `timeout_ms` (-1 blocks indefinitely).
///
/// Returns the number of ready descriptors. `Interrupted` is surfaced rather
/// than retried here, because the callers treat a spurious wakeup as an
/// ordinary empty poll.
pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    // `WSAPoll` rejects an empty set, and a zero-length `poll` is just a sleep
    // on POSIX. The callers always register at least one descriptor, so treat
    // an empty set as "nothing ready" instead of an error.
    if fds.is_empty() {
        return Ok(0);
    }
    // SAFETY: the slice's pointer and length are passed together, and the
    // kernel writes only into its `revents` fields.
    let ret = unsafe { sys::poll(fds, timeout_ms) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ret as usize)
}
