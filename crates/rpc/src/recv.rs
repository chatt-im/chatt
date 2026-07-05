//! Socket receive buffer built for the framed transports: bytes land directly
//! in a reusable `Vec` and frames are consumed in place.
//!
//! [`RecvBuffer::fill`] reads with `libc::read` straight into the `Vec`'s
//! spare capacity, so there is no zero-initialized scratch buffer and no copy
//! from scratch into the buffer. Frames are parsed from [`RecvBuffer::pending`]
//! and consumed by advancing a cursor, so popping a frame never memmoves the
//! bytes behind it, and sealed frames decrypt in place through
//! [`RecvBuffer::pending_mut`]. A read usually delivers exactly one whole
//! frame; [`RecvBuffer::ship`] hands that buffer onward without copying it.
//! Only bytes that straddle a read boundary are ever copied, to the front of
//! the buffer before the next read.

use std::io;
use std::os::fd::AsRawFd;

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
    /// growing it to hold at least `min_spare` more bytes first. Returns the
    /// number of bytes read; `Ok(0)` is end-of-stream, and a would-block on a
    /// nonblocking socket (or a receive timeout) surfaces as the usual
    /// [`io::ErrorKind::WouldBlock`].
    ///
    /// Any partial frame left from the previous read is moved to the front of
    /// the buffer first — the only copy this buffer ever makes.
    pub fn fill(&mut self, socket: &impl AsRawFd, min_spare: usize) -> io::Result<usize> {
        if self.start > 0 {
            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(self.buf.len() - self.start);
            self.start = 0;
        }
        let len = self.buf.len();
        if self.buf.capacity() - len < min_spare {
            self.buf.reserve(min_spare);
        }
        let spare = self.buf.capacity() - len;
        loop {
            let read = unsafe {
                libc::read(
                    socket.as_raw_fd(),
                    self.buf.as_mut_ptr().add(len).cast(),
                    spare,
                )
            };
            if read >= 0 {
                unsafe { self.buf.set_len(len + read as usize) };
                return Ok(read as usize);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
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
    use std::os::unix::net::UnixStream;

    #[test]
    fn fill_reads_into_spare_capacity_and_cursor_consumes_in_place() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
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
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(b"onetwo").unwrap();
        let mut recv = RecvBuffer::new();
        recv.fill(&reader, 16).unwrap();
        recv.consume(3);

        writer.write_all(b"three").unwrap();
        recv.fill(&reader, 16).unwrap();
        assert_eq!(recv.pending(), b"twothree");
    }

    #[test]
    fn fill_reports_eof_and_would_block() {
        let (writer, reader) = UnixStream::pair().unwrap();
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
        let (mut writer, reader) = UnixStream::pair().unwrap();
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
