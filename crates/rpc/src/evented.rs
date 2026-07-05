//! Small evented-I/O building blocks shared by the client and server loops.
//!
//! These helpers deliberately cover mechanics only: readiness retention,
//! bounded read/write pumps, retrying interrupted syscalls, and cursor-based
//! queued writes. Loop scheduling policy stays in the caller.

use std::{
    io::{self, Write},
    os::fd::AsRawFd,
};

use crate::recv::RecvBuffer;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Readiness(bool);

impl Readiness {
    #[inline]
    pub fn new() -> Self {
        Self(false)
    }

    #[inline]
    pub fn primed() -> Self {
        Self(true)
    }

    #[inline]
    pub fn mark_ready(&mut self) {
        self.0 = true;
    }

    #[inline]
    pub fn mark_drained(&mut self) {
        self.0 = false;
    }

    #[inline]
    pub fn is_ready(self) -> bool {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadLimit {
    ByteBudget(usize),
    MaxBuffered(usize),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadPumpOutcome {
    pub bytes_read: usize,
    pub hit_limit: bool,
    pub disconnected: bool,
}

#[inline]
pub fn read_into_buffer(
    socket: &impl AsRawFd,
    read_buf: &mut RecvBuffer,
    readiness: &mut Readiness,
    max_read_bytes_per_syscall: usize,
    limit: ReadLimit,
) -> io::Result<ReadPumpOutcome> {
    debug_assert!(max_read_bytes_per_syscall > 0);
    debug_assert!(match limit {
        ReadLimit::ByteBudget(bytes) | ReadLimit::MaxBuffered(bytes) => bytes > 0,
    });

    let mut outcome = ReadPumpOutcome::default();
    while readiness.is_ready() && read_limit_allows_read(limit, read_buf, outcome.bytes_read) {
        let read_bytes = max_read_bytes_per_syscall.min(read_limit_remaining(
            limit,
            read_buf,
            outcome.bytes_read,
        ));
        debug_assert!(read_bytes > 0);
        match read_buf.fill(socket, read_bytes) {
            Ok(0) => {
                readiness.mark_drained();
                outcome.disconnected = true;
                break;
            }
            Ok(read) => {
                outcome.bytes_read += read;
                if read_limit_hit(limit, read_buf, outcome.bytes_read) {
                    outcome.hit_limit = true;
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                readiness.mark_drained();
                break;
            }
            Err(error) => return Err(error),
        }
    }
    if readiness.is_ready() && !read_limit_allows_read(limit, read_buf, outcome.bytes_read) {
        outcome.hit_limit = true;
    }
    Ok(outcome)
}

#[inline]
fn read_limit_allows_read(limit: ReadLimit, read_buf: &RecvBuffer, bytes_read: usize) -> bool {
    match limit {
        ReadLimit::ByteBudget(bytes) => bytes_read < bytes,
        ReadLimit::MaxBuffered(bytes) => read_buf.len() < bytes,
    }
}

#[inline]
fn read_limit_remaining(limit: ReadLimit, read_buf: &RecvBuffer, bytes_read: usize) -> usize {
    match limit {
        ReadLimit::ByteBudget(bytes) => bytes.saturating_sub(bytes_read),
        ReadLimit::MaxBuffered(bytes) => bytes.saturating_sub(read_buf.len()),
    }
}

#[inline]
fn read_limit_hit(limit: ReadLimit, read_buf: &RecvBuffer, bytes_read: usize) -> bool {
    !read_limit_allows_read(limit, read_buf, bytes_read)
}

/// Threshold above which [`WriteQueue::consume`] compacts the consumed prefix
/// away. Compaction additionally requires the prefix to be at least half the
/// buffer, keeping the memmove amortized against bytes already written.
const WRITE_QUEUE_COMPACT_BYTES: usize = 64 * 1024;

/// Outbound byte queue drained from the front through a cursor rather than
/// `Vec::drain`, so partial socket writes against a deep backlog do not memmove
/// remaining bytes on every write.
#[derive(Debug, Default)]
pub struct WriteQueue {
    buf: Vec<u8>,
    start: usize,
}

impl WriteQueue {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
        }
    }

    #[inline]
    pub fn pending(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.start == self.buf.len()
    }

    /// The backing vector, for encoders that append to a `Vec`. Only appends
    /// are valid; bytes before the cursor are dead.
    #[inline]
    pub fn tail_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    #[inline]
    pub fn clear(&mut self) {
        self.buf.clear();
        self.start = 0;
    }

    /// Marks the next `n` pending bytes written, compacting the dead prefix
    /// once it is both large and the majority of the buffer.
    pub fn consume(&mut self, n: usize) {
        self.start += n;
        debug_assert!(self.start <= self.buf.len());
        if self.start == self.buf.len() {
            self.clear();
        } else if self.start >= WRITE_QUEUE_COMPACT_BYTES && self.start >= self.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WritePumpOutcome {
    pub bytes_written: usize,
    pub attempts: usize,
    pub hit_limit: bool,
    pub blocked: bool,
    pub wrote_zero: bool,
}

pub fn write_queue_to(
    writer: &mut impl Write,
    queue: &mut WriteQueue,
    max_attempts: usize,
) -> io::Result<WritePumpOutcome> {
    debug_assert!(max_attempts > 0);

    let mut outcome = WritePumpOutcome::default();
    while !queue.is_empty() && outcome.attempts < max_attempts {
        outcome.attempts += 1;
        match writer.write(queue.pending()) {
            Ok(0) => {
                outcome.wrote_zero = true;
                break;
            }
            Ok(n) => {
                outcome.bytes_written += n;
                queue.consume(n);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                outcome.blocked = true;
                break;
            }
            Err(error) if is_interrupted_io_error(&error) => continue,
            Err(error) => return Err(error),
        }
    }
    outcome.hit_limit = !queue.is_empty()
        && outcome.attempts == max_attempts
        && !outcome.blocked
        && !outcome.wrote_zero;
    Ok(outcome)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MioReady {
    pub readable: bool,
    pub writable: bool,
    pub error: bool,
    pub read_closed: bool,
    pub write_closed: bool,
}

impl MioReady {
    #[inline]
    pub fn from_event(event: &mio::event::Event) -> Self {
        Self {
            readable: event.is_readable(),
            writable: event.is_writable(),
            error: event.is_error(),
            read_closed: event.is_read_closed(),
            write_closed: event.is_write_closed(),
        }
    }

    #[inline]
    pub fn readable_like(self) -> bool {
        self.readable || self.error || self.read_closed
    }

    #[inline]
    pub fn writable_like(self) -> bool {
        self.writable || self.error || self.write_closed
    }
}

#[inline]
pub fn is_interrupted_io_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted || error.raw_os_error() == Some(libc::EINTR)
}

#[inline]
pub fn recv_datagram_with<A>(
    buf: &mut [u8],
    mut recv_from: impl FnMut(&mut [u8]) -> io::Result<(usize, A)>,
) -> io::Result<Option<(usize, A)>> {
    loop {
        match recv_from(buf) {
            Ok(value) => return Ok(Some(value)),
            Err(error) if is_interrupted_io_error(&error) => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    struct PartialWriter {
        per_write: usize,
    }

    impl Write for PartialWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(self.per_write.min(buf.len()))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct InterruptedOnceWriter {
        interrupted: bool,
    }

    impl Write for InterruptedOnceWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct ZeroWriter;

    impl Write for ZeroWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Ok(0)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn read_byte_budget_stop_keeps_readiness_for_retry() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"x").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::ByteBudget(1),
        )
        .unwrap();

        assert!(outcome.hit_limit);
        assert!(!outcome.disconnected);
        assert!(outcome.bytes_read >= 1);
        assert!(readiness.is_ready());
        assert!(!read_buf.is_empty());
    }

    #[test]
    fn read_byte_budget_caps_single_large_syscall() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"abcdef").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            64,
            ReadLimit::ByteBudget(3),
        )
        .unwrap();

        assert_eq!(outcome.bytes_read, 3);
        assert!(outcome.hit_limit);
        assert!(readiness.is_ready());
        assert_eq!(read_buf.pending(), b"abc");
    }

    #[test]
    fn read_max_buffer_stop_keeps_readiness_for_retry() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"x").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::MaxBuffered(1),
        )
        .unwrap();

        assert!(outcome.hit_limit);
        assert!(!outcome.disconnected);
        assert!(outcome.bytes_read >= 1);
        assert!(readiness.is_ready());
    }

    #[test]
    fn read_would_block_clears_retained_readiness_after_buffer_consumed() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"x").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::ByteBudget(1),
        )
        .unwrap();
        assert!(outcome.hit_limit);
        assert!(readiness.is_ready());

        read_buf.consume(read_buf.len());
        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::ByteBudget(1),
        )
        .unwrap();

        assert_eq!(outcome, ReadPumpOutcome::default());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn write_queue_consumes_through_cursor_and_compacts() {
        let mut queue = WriteQueue::new();
        queue.tail_mut().extend_from_slice(b"abcdef");
        queue.consume(2);
        assert_eq!(queue.pending(), b"cdef");
        queue.tail_mut().extend_from_slice(b"gh");
        assert_eq!(queue.pending(), b"cdefgh");
        queue.consume(6);
        assert!(queue.is_empty());

        queue
            .tail_mut()
            .extend_from_slice(&vec![7u8; WRITE_QUEUE_COMPACT_BYTES + 16]);
        queue.consume(WRITE_QUEUE_COMPACT_BYTES);
        assert_eq!(queue.pending(), &[7u8; 16]);
    }

    #[test]
    fn write_pump_stops_at_attempt_limit_with_queued_bytes() {
        let mut writer = PartialWriter { per_write: 2 };
        let mut queue = WriteQueue::new();
        queue.tail_mut().extend_from_slice(b"abcdef");

        let outcome = write_queue_to(&mut writer, &mut queue, 1).unwrap();

        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.bytes_written, 2);
        assert!(outcome.hit_limit);
        assert_eq!(queue.pending(), b"cdef");
    }

    #[test]
    fn write_pump_retries_interrupted_write() {
        let mut writer = InterruptedOnceWriter { interrupted: false };
        let mut queue = WriteQueue::new();
        queue.tail_mut().extend_from_slice(b"abcdef");

        let outcome = write_queue_to(&mut writer, &mut queue, 2).unwrap();

        assert_eq!(outcome.attempts, 2);
        assert_eq!(outcome.bytes_written, 6);
        assert!(!outcome.hit_limit);
        assert!(queue.is_empty());
    }

    #[test]
    fn write_pump_reports_zero_write_without_consuming() {
        let mut writer = ZeroWriter;
        let mut queue = WriteQueue::new();
        queue.tail_mut().extend_from_slice(b"abcdef");

        let outcome = write_queue_to(&mut writer, &mut queue, 1).unwrap();

        assert_eq!(outcome.attempts, 1);
        assert!(outcome.wrote_zero);
        assert!(!outcome.hit_limit);
        assert_eq!(queue.pending(), b"abcdef");
    }

    #[test]
    fn interrupted_io_errors_are_retryable() {
        assert!(is_interrupted_io_error(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(is_interrupted_io_error(&io::Error::from_raw_os_error(
            libc::EINTR
        )));
        assert!(!is_interrupted_io_error(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
    }

    #[test]
    fn datagram_recv_retries_interrupted_before_datagram_or_drain() {
        let src = "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap();
        let mut calls = 0;
        let mut buf = [0u8; 8];

        let received = recv_datagram_with(&mut buf, |_| {
            calls += 1;
            match calls {
                1 => Err(io::Error::from_raw_os_error(libc::EINTR)),
                2 => Ok((3, src)),
                _ => unreachable!("receive loop should return after one datagram"),
            }
        })
        .unwrap();

        assert_eq!(received, Some((3, src)));
        assert_eq!(calls, 2);

        let mut calls = 0;
        let drained: Option<(usize, std::net::SocketAddr)> = recv_datagram_with(&mut buf, |_| {
            calls += 1;
            match calls {
                1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                2 => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                _ => unreachable!("receive loop should stop at WouldBlock"),
            }
        })
        .unwrap();

        assert_eq!(drained, None);
        assert_eq!(calls, 2);
    }
}
