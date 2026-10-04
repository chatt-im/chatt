//! The resources a frontend frame can carry, and the connected byte-stream
//! pair the core uses to serve renderers across the Unix socket.
//!
//! A renderer across the socket gets live-share video and memory-held
//! attachments over a `socketpair` the core keeps one end of, whose other end
//! descriptor passing moves to it. An in-process frontend shares the core's
//! process, so it takes a live share's frame queue and a memory-held
//! attachment's buffer themselves; no stream is ever made for it. The socket
//! transport is Unix-only, so on Windows nothing here creates a socket at all.

#[cfg(unix)]
use std::{
    io,
    os::{fd::OwnedFd, unix::net::UnixStream},
};

/// One end of a [`local_stream_pair`].
#[cfg(unix)]
pub type LocalStream = UnixStream;

/// A resource handed to a frontend alongside a frame.
///
/// On Unix this is a descriptor, which is what the renderer socket passes with
/// `SCM_RIGHTS`.
#[cfg(unix)]
pub type FrameResource = OwnedFd;

/// A resource handed to a frontend alongside a frame: an attachment already on
/// disk, opened read-only.
#[cfg(not(unix))]
pub type FrameResource = std::fs::File;

/// Converts a frame resource into the file it must be.
///
/// On Unix a descriptor carries no type, so the frontend still checks what the
/// file actually is before reading it.
#[cfg(feature = "embedded-client")]
pub fn into_file(resource: FrameResource) -> std::fs::File {
    std::fs::File::from(resource)
}

/// Creates a connected pair of local streams.
#[cfg(unix)]
pub fn local_stream_pair() -> io::Result<(LocalStream, LocalStream)> {
    #[cfg(test)]
    PAIRS_CREATED.with(|created| created.set(created.get() + 1));
    UnixStream::pair()
}

#[cfg(all(test, unix))]
thread_local! {
    static PAIRS_CREATED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many [`local_stream_pair`] calls the current thread has made, so a test
/// can pin that a path creates no socket.
#[cfg(all(test, unix))]
pub fn pairs_created_on_this_thread() -> usize {
    PAIRS_CREATED.with(std::cell::Cell::get)
}

/// Stops a worker thread blocked reading or writing its [`InterruptibleStream`].
///
/// Teardown interrupts a worker and then joins it, so the interrupt has to
/// unblock whatever call the worker is parked in; `shutdown` on any descriptor
/// of the socket does that, and the worker then reads end of stream.
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

#[cfg(all(test, unix))]
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
    // then joins it. The join must not wait on the frontend, and the worker
    // sees an ordinary end of stream rather than an error.
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
        assert_eq!(read.unwrap(), 0);
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
    fn pairs_are_counted_per_thread() {
        let before = pairs_created_on_this_thread();
        let _pair = local_stream_pair().unwrap();
        assert_eq!(pairs_created_on_this_thread(), before + 1);
        let elsewhere = thread::spawn(pairs_created_on_this_thread).join().unwrap();
        assert_eq!(elsewhere, 0);
    }
}
