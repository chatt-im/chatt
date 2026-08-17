//! Socket receive buffer built for the framed transports: bytes land directly
//! in a reusable `Vec` and frames are consumed in place.
//!
//! [`RecvBuffer::fill`] reads with `libc::read` straight into the `Vec`'s
//! spare capacity, so there is no zero-initialized scratch buffer and no copy
//! from scratch into the buffer. The caller supplies the maximum bytes for one
//! syscall; retained capacity is reuse only, not an implicit larger read.
//! Frames are parsed from [`RecvBuffer::pending`]
//! and consumed by advancing a cursor, so popping a frame never memmoves the
//! bytes behind it, and sealed frames decrypt in place through
//! [`RecvBuffer::pending_mut`]. A read usually delivers exactly one whole
//! frame; [`RecvBuffer::ship`] hands that buffer onward without copying it.
//! Only bytes that straddle a read boundary are ever copied, to the front of
//! the buffer before the next read.

use std::io;

use chatt_platform::{SocketSource, is_interrupted};

pub struct RecvBuffer {
    buf: Vec<u8>,
    start: usize,
}

impl RecvBuffer {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
        }
    }

    /// Bytes received but not yet consumed.
    pub fn pending(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    /// Mutable view of the pending bytes, for opening a sealed frame in place.
    pub fn pending_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.start..]
    }

    pub fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.buf.len()
    }

    /// Marks the next `n` pending bytes consumed. The bytes stay in place
    /// behind the cursor; the buffer resets for reuse once everything pending
    /// has been consumed.
    pub fn consume(&mut self, n: usize) {
        self.start += n;
        debug_assert!(self.start <= self.buf.len());
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        }
    }

    /// Reads once from `socket` directly into the buffer's spare capacity,
    /// growing it to hold up to `max_read` more bytes first. Returns the
    /// number of bytes read; `Ok(0)` is end-of-stream, and a would-block on a
    /// nonblocking socket (or a receive timeout) surfaces as the usual
    /// [`io::ErrorKind::WouldBlock`].
    ///
    /// Any partial frame left from the previous read is moved to the front of
    /// the buffer first — the only copy this buffer ever makes.
    pub fn fill(
        &mut self,
        socket: &(impl SocketSource + ?Sized),
        max_read: usize,
    ) -> io::Result<usize> {
        self.fill_with(max_read, |destination, len| {
            loop {
                // SAFETY: `fill_with` hands out spare capacity valid for `len`
                // writes.
                match unsafe { socket.recv_into(destination, len) } {
                    Ok(read) => return Ok(read),
                    Err(error) if is_interrupted(&error) => continue,
                    Err(error) => return Err(error),
                }
            }
        })
    }

    /// Fills the buffer through a transport-specific receive operation.
    ///
    /// The callback receives writable spare capacity and must return the number
    /// of bytes it initialized. Keeping allocation and cursor management here
    /// lets Unix transports use `recvmsg` (for ancillary-data validation) while
    /// still landing bytes directly in this reusable buffer.
    pub(crate) fn fill_with(
        &mut self,
        max_read: usize,
        mut receive: impl FnMut(*mut u8, usize) -> io::Result<usize>,
    ) -> io::Result<usize> {
        debug_assert!(max_read > 0);
        if self.start > 0 {
            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(self.buf.len() - self.start);
            self.start = 0;
        }
        let len = self.buf.len();
        if self.buf.capacity() - len < max_read {
            self.buf.reserve(max_read);
        }
        let read = receive(unsafe { self.buf.as_mut_ptr().add(len) }, max_read)?;
        if read > max_read {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "receive operation exceeded its destination buffer",
            ));
        }
        unsafe { self.buf.set_len(len + read) };
        Ok(read)
    }

    /// Detaches and returns the backing buffer holding exactly
    /// `pending()[discard_front..discard_front + keep]` at index zero: the
    /// zero-copy handoff for a frame that ends exactly where the buffer ends.
    /// The receiver restarts empty and reallocates on the next fill.
    pub fn ship(&mut self, discard_front: usize, keep: usize) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.buf);
        buf.truncate(self.start + discard_front + keep);
        buf.drain(..self.start + discard_front);
        self.start = 0;
        buf
    }
}

impl Default for RecvBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    /// A connected pair of byte streams.
    ///
    /// These tests only need two ends of a stream; the receive path under test
    /// takes any [`SocketSource`]. Keeping them running on Windows matters more
    /// than most: the raw-read-versus-`mio` interaction this buffer sits on is
    /// what broke the port.
    #[cfg(unix)]
    fn stream_pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("socket pair")
    }

    #[cfg(not(unix))]
    fn stream_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("loopback addr");
        let client = std::net::TcpStream::connect(addr).expect("connect loopback");
        let (server, _) = listener.accept().expect("accept loopback");
        (client, server)
    }

    #[test]
    fn fill_reads_into_spare_capacity_and_cursor_consumes_in_place() {
        let (mut writer, reader) = stream_pair();
        writer.write_all(b"abcdef").unwrap();
        let mut recv = RecvBuffer::new();

        assert_eq!(recv.fill(&reader, 16).unwrap(), 6);
        assert_eq!(recv.pending(), b"abcdef");

        recv.consume(2);
        assert_eq!(recv.pending(), b"cdef");
        recv.consume(4);
        assert!(recv.is_empty());

        writer.write_all(b"gh").unwrap();
        assert_eq!(recv.fill(&reader, 16).unwrap(), 2);
        assert_eq!(recv.pending(), b"gh");
    }

    #[test]
    fn fill_moves_partial_tail_to_front_before_reading() {
        let (mut writer, reader) = stream_pair();
        writer.write_all(b"onetwo").unwrap();
        let mut recv = RecvBuffer::new();
        recv.fill(&reader, 16).unwrap();
        recv.consume(3);

        writer.write_all(b"three").unwrap();
        recv.fill(&reader, 16).unwrap();
        assert_eq!(recv.pending(), b"twothree");
    }

    #[test]
    fn fill_reads_at_most_requested_bytes() {
        let (mut writer, reader) = stream_pair();
        writer.write_all(b"abcdef").unwrap();
        let mut recv = RecvBuffer::new();

        assert_eq!(recv.fill(&reader, 3).unwrap(), 3);
        assert_eq!(recv.pending(), b"abc");
        assert_eq!(recv.fill(&reader, 3).unwrap(), 3);
        assert_eq!(recv.pending(), b"abcdef");
    }

    #[test]
    fn fill_reports_eof_and_would_block() {
        let (writer, reader) = stream_pair();
        reader.set_nonblocking(true).unwrap();
        let mut recv = RecvBuffer::new();
        assert_eq!(
            recv.fill(&reader, 16).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(writer);
        assert_eq!(recv.fill(&reader, 16).unwrap(), 0);
    }

    #[test]
    fn ship_hands_off_backing_buffer_trimmed_to_range() {
        let (mut writer, reader) = stream_pair();
        writer.write_all(b"hdrpayloadtag").unwrap();
        let mut recv = RecvBuffer::new();
        recv.fill(&reader, 16).unwrap();

        let shipped = recv.ship(3, 7);
        assert_eq!(shipped, b"payload");
        assert!(recv.is_empty());

        writer.write_all(b"next").unwrap();
        recv.fill(&reader, 16).unwrap();
        assert_eq!(recv.pending(), b"next");
    }
}
