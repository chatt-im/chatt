//! Safe wrapper for `OpusRepacketizer` utilities

use crate::bindings::{
    OpusRepacketizer, opus_repacketizer_cat, opus_repacketizer_create, opus_repacketizer_destroy,
    opus_repacketizer_get_nb_frames, opus_repacketizer_get_size, opus_repacketizer_init,
    opus_repacketizer_out, opus_repacketizer_out_range,
};
use crate::error::{Error, Result};
#[cfg(opus_codec_rust_packet_ops)]
use crate::packet;
use crate::{AlignedBuffer, Ownership, RawHandle};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Repackages Opus frames into packets.
pub struct Repacketizer {
    rp: RawHandle<OpusRepacketizer>,
    packets: Vec<Vec<u8>>,
}

unsafe impl Send for Repacketizer {}
unsafe impl Sync for Repacketizer {}

/// Borrowed wrapper around a repacketizer state.
pub struct RepacketizerRef<'a> {
    inner: Repacketizer,
    _marker: PhantomData<&'a mut OpusRepacketizer>,
}

unsafe impl Send for RepacketizerRef<'_> {}
unsafe impl Sync for RepacketizerRef<'_> {}

impl Repacketizer {
    fn from_raw(ptr: NonNull<OpusRepacketizer>, ownership: Ownership) -> Self {
        Self {
            rp: RawHandle::new(ptr, ownership, opus_repacketizer_destroy),
            packets: Vec::new(),
        }
    }

    /// Create a new repacketizer.
    ///
    /// # Errors
    /// Returns `AllocFail` if allocation fails.
    pub fn new() -> Result<Self> {
        let rp = unsafe { opus_repacketizer_create() };
        let rp = NonNull::new(rp).ok_or(Error::AllocFail)?;
        Ok(Self::from_raw(rp, Ownership::Owned))
    }

    /// Reset internal state.
    pub fn reset(&mut self) {
        unsafe { opus_repacketizer_init(self.rp.as_ptr()) };
        self.packets.clear();
    }

    /// Add a packet to the current state.
    ///
    /// The packet data is copied and retained until the next call to [`Self::reset`].
    ///
    /// # Errors
    /// Returns an error if the packet is invalid for the current state.
    pub fn push(&mut self, packet: &[u8]) -> Result<()> {
        if packet.is_empty() {
            return Err(Error::BadArg);
        }
        let len_i32 = i32::try_from(packet.len()).map_err(|_| Error::BadArg)?;
        let packet = packet.to_vec();
        let r = unsafe { opus_repacketizer_cat(self.rp.as_ptr(), packet.as_ptr(), len_i32) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        // libopus stores pointers into packet data; keep owned buffers alive.
        self.packets.push(packet);
        Ok(())
    }

    /// Number of frames currently queued.
    #[must_use]
    pub fn frame_count(&self) -> i32 {
        unsafe { opus_repacketizer_get_nb_frames(self.rp.as_ptr()) }
    }

    /// Number of frames currently queued as a `usize`.
    #[must_use]
    pub fn len(&self) -> usize {
        let frames = self.frame_count();
        debug_assert!(
            frames >= 0,
            "repacketizer frame count should be non-negative"
        );
        usize::try_from(frames).unwrap_or(0)
    }

    /// Returns true when there are no queued frames.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Emit a packet containing frames in range [begin, end).
    ///
    /// # Errors
    /// Returns an error if range is invalid or output buffer is too small.
    pub fn emit_range(&mut self, begin: i32, end: i32, out: &mut [u8]) -> Result<usize> {
        if begin < 0 || end <= begin {
            return Err(Error::BadArg);
        }
        #[cfg(opus_codec_rust_packet_ops)]
        let begin_index = usize::try_from(begin).map_err(|_| Error::BadArg)?;
        #[cfg(opus_codec_rust_packet_ops)]
        let end_index = usize::try_from(end).map_err(|_| Error::BadArg)?;
        #[cfg(not(opus_codec_rust_packet_ops))]
        {
            self.emit_range_via_c(begin, end, out)
        }
        #[cfg(opus_codec_rust_packet_ops)]
        if let Some((toc, frames, paddings)) = self.repacketizer_inputs()? {
            if end_index > frames.len() {
                return Err(Error::BadArg);
            }
            packet::repacketize_frames_range(toc, &frames, &paddings, begin_index, end_index, out)
        } else {
            self.emit_range_via_c(begin, end, out)
        }
    }

    /// Emit a packet with all queued frames.
    ///
    /// # Errors
    /// Returns an error if the output buffer is too small.
    pub fn emit(&mut self, out: &mut [u8]) -> Result<usize> {
        #[cfg(not(opus_codec_rust_packet_ops))]
        {
            self.emit_via_c(out)
        }
        #[cfg(opus_codec_rust_packet_ops)]
        if let Some((toc, frames, paddings)) = self.repacketizer_inputs()? {
            packet::repacketize_frames(toc, &frames, &paddings, out)
        } else {
            self.emit_via_c(out)
        }
    }

    /// Size of a repacketizer state in bytes for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::InternalError`] if libopus reports an invalid size.
    pub fn size() -> Result<usize> {
        let raw = unsafe { opus_repacketizer_get_size() };
        if raw <= 0 {
            return Err(Error::InternalError);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Initialize a previously allocated repacketizer state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `Repacketizer::size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if `ptr` is null.
    pub unsafe fn init_in_place(ptr: *mut OpusRepacketizer) -> Result<()> {
        if ptr.is_null() {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        unsafe { opus_repacketizer_init(ptr) };
        Ok(())
    }

    #[cfg(opus_codec_rust_packet_ops)]
    fn repacketizer_inputs(&self) -> Result<Option<packet::RepacketizerInputs<'_>>> {
        let c_frame_count = self.len();
        if self.packets.is_empty() {
            return if c_frame_count == 0 {
                Err(Error::BadArg)
            } else {
                Ok(None)
            };
        }

        let mut toc = None;
        let mut frames = Vec::new();
        let mut paddings = Vec::new();

        for packet in &self.packets {
            let (packet_toc, mut packet_frames, mut packet_paddings) =
                packet::packet_repacketizer_inputs(packet)?;
            toc.get_or_insert(packet_toc);
            frames.append(&mut packet_frames);
            paddings.append(&mut packet_paddings);
        }

        if frames.len() != c_frame_count {
            return Ok(None);
        }

        let toc = toc.ok_or(Error::BadArg)?;
        Ok(Some((toc, frames, paddings)))
    }

    fn emit_range_via_c(&self, begin: i32, end: i32, out: &mut [u8]) -> Result<usize> {
        let out_len_i32 = i32::try_from(out.len()).map_err(|_| Error::BadArg)?;
        let n = unsafe {
            opus_repacketizer_out_range(self.rp.as_ptr(), begin, end, out.as_mut_ptr(), out_len_i32)
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    fn emit_via_c(&self, out: &mut [u8]) -> Result<usize> {
        let out_len_i32 = i32::try_from(out.len()).map_err(|_| Error::BadArg)?;
        let n = unsafe { opus_repacketizer_out(self.rp.as_ptr(), out.as_mut_ptr(), out_len_i32) };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }
}

impl<'a> RepacketizerRef<'a> {
    /// Wrap an externally-initialized repacketizer without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`Repacketizer::size()`] bytes
    /// - `ptr` must be aligned to at least `align_of::<usize>()` (malloc-style alignment)
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    /// - If `ptr` already contains packet pointers, their backing storage must
    ///   remain valid while this wrapper is used.
    ///
    /// Use [`Repacketizer::init_in_place`] to initialize the memory before calling this.
    /// If packets are pushed through this wrapper, dropping it resets the raw
    /// state to avoid leaving dangling pointers to the wrapper's owned buffers.
    #[must_use]
    pub unsafe fn from_raw(ptr: *mut OpusRepacketizer) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        let repacketizer =
            Repacketizer::from_raw(unsafe { NonNull::new_unchecked(ptr) }, Ownership::Borrowed);
        Self {
            inner: repacketizer,
            _marker: PhantomData,
        }
    }

    /// Initialize and wrap an externally allocated buffer.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the buffer is too small.
    pub fn init_in(buf: &'a mut AlignedBuffer) -> Result<Self> {
        let required = Repacketizer::size()?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusRepacketizer>();
        unsafe { Repacketizer::init_in_place(ptr)? };
        Ok(unsafe { Self::from_raw(ptr) })
    }
}

impl Drop for RepacketizerRef<'_> {
    fn drop(&mut self) {
        if !self.inner.packets.is_empty() {
            // Reinitialize the external C state to clear packet pointers that
            // reference our `self.inner.packets` buffers, which are about to be
            // freed.  Without this the caller could reuse the external
            // OpusRepacketizer and dereference dangling pointers.
            unsafe { opus_repacketizer_init(self.inner.rp.as_ptr()) };
        }
    }
}

impl Deref for RepacketizerRef<'_> {
    type Target = Repacketizer;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for RepacketizerRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
