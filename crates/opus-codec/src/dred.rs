//! Safe wrappers for libopus Deep Redundancy (DRED) decoder support.
//! This module is available when the `dred` Cargo feature is enabled.

use crate::bindings::{
    OpusDRED, OpusDREDDecoder, opus_decoder_dred_decode, opus_decoder_dred_decode_float,
    opus_dred_alloc, opus_dred_decoder_create, opus_dred_decoder_ctl, opus_dred_decoder_destroy,
    opus_dred_decoder_get_size, opus_dred_decoder_init, opus_dred_free, opus_dred_get_size,
    opus_dred_parse, opus_dred_process,
};
use crate::constants::{is_frame_size_2_5ms_aligned, max_frame_samples_for};
use crate::decoder::Decoder;
use crate::error::{Error, Result};
use crate::types::SampleRate;
use crate::{AlignedBuffer, Ownership, RawHandle};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

// libopus computes `100 * max_dred_samples / sampling_rate` in signed 32-bit math.
const MAX_SAFE_DRED_SAMPLES: usize = (i32::MAX as usize) / 100;

/// Managed handle for libopus `OpusDREDDecoder`.
pub struct DredDecoder {
    raw: RawHandle<OpusDREDDecoder>,
}

unsafe impl Send for DredDecoder {}
unsafe impl Sync for DredDecoder {}

/// Borrowed wrapper around an externally allocated DRED decoder.
pub struct DredDecoderRef<'a> {
    inner: DredDecoder,
    _marker: PhantomData<&'a mut OpusDREDDecoder>,
}

unsafe impl Send for DredDecoderRef<'_> {}
unsafe impl Sync for DredDecoderRef<'_> {}

impl DredDecoder {
    fn from_raw(ptr: NonNull<OpusDREDDecoder>, ownership: Ownership) -> Self {
        Self {
            raw: RawHandle::new(ptr, ownership, opus_dred_decoder_destroy),
        }
    }

    /// Allocate a new DRED decoder.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AllocFail`] if allocation fails or a mapped libopus error
    /// when decoder creation does not succeed.
    pub fn new() -> Result<Self> {
        let mut err = 0;
        let ptr = unsafe { opus_dred_decoder_create(std::ptr::addr_of_mut!(err)) };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let ptr = NonNull::new(ptr).ok_or(Error::AllocFail)?;
        Ok(Self::from_raw(ptr, Ownership::Owned))
    }

    /// Initialize an externally allocated decoder buffer.
    ///
    /// # Safety
    ///
    /// Caller must provide a valid pointer to `opus_dred_decoder_get_size()` bytes.
    ///
    /// # Errors
    ///
    /// Returns a mapped libopus error if initialization fails.
    pub unsafe fn init_in_place(ptr: *mut OpusDREDDecoder) -> Result<()> {
        if ptr.is_null() {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        let r = unsafe { opus_dred_decoder_init(ptr) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Borrow the raw decoder pointer.
    pub fn as_mut_ptr(&mut self) -> *mut OpusDREDDecoder {
        self.raw.as_ptr()
    }

    /// Size of a decoder object in bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InternalError`] if libopus reports a non-positive size,
    /// indicating an unexpected ABI/runtime mismatch.
    pub fn size() -> Result<usize> {
        let raw = unsafe { opus_dred_decoder_get_size() };
        if raw <= 0 {
            return Err(Error::InternalError);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Run a control request directly.
    ///
    /// # Safety
    ///
    /// The caller must ensure the request and argument combination is valid for the
    /// underlying libopus build and that `arg` satisfies libopus expectations.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus
    /// error when the control call fails.
    pub unsafe fn control<T>(&mut self, request: i32, arg: T) -> Result<()>
    where
        T: Copy,
    {
        let r = unsafe { opus_dred_decoder_ctl(self.raw.as_ptr(), request, arg) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Parse DRED payload and update `state`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] if handles are invalid, [`Error::BadArg`] for
    /// size conversion failures, or a mapped libopus error from [`opus_dred_parse`].
    pub fn parse(
        &mut self,
        state: &mut DredState,
        data: &[u8],
        max_dred_samples: usize,
        sampling_rate: SampleRate,
        dred_end: &mut i32,
        defer_processing: bool,
    ) -> Result<usize> {
        let len = i32::try_from(data.len()).map_err(|_| Error::BadArg)?;
        let max_samples = checked_max_dred_samples(max_dred_samples)?;
        let result = unsafe {
            opus_dred_parse(
                self.raw.as_ptr(),
                state.raw.as_ptr(),
                data.as_ptr(),
                len,
                max_samples,
                sampling_rate.as_i32(),
                dred_end,
                i32::from(defer_processing),
            )
        };
        if result < 0 {
            return Err(Error::from_code(result));
        }
        usize::try_from(result).map_err(|_| Error::InternalError)
    }

    /// Complete deferred processing between `src` and `dst` states.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] if pointers are invalid, or a mapped libopus
    /// error when [`opus_dred_process`] fails.
    pub fn process(&mut self, src: &DredState, dst: &mut DredState) -> Result<()> {
        let r = unsafe { opus_dred_process(self.raw.as_ptr(), src.raw.as_ptr(), dst.raw.as_ptr()) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Complete deferred processing in-place.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] if pointers are invalid, or a mapped libopus
    /// error when [`opus_dred_process`] fails.
    pub fn process_in_place(&mut self, state: &mut DredState) -> Result<()> {
        let r =
            unsafe { opus_dred_process(self.raw.as_ptr(), state.raw.as_ptr(), state.raw.as_ptr()) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Decode redundancy into i16 PCM using a normal Opus decoder.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] if handles are invalid, [`Error::BadArg`] for
    /// invalid buffer sizing, or a mapped libopus error from
    /// [`opus_decoder_dred_decode`].
    pub fn decode_into_i16(
        &mut self,
        decoder: &mut Decoder,
        state: &DredState,
        dred_offset: i32,
        pcm: &mut [i16],
    ) -> Result<usize> {
        let channel_count = decoder.channels().as_usize();
        let frame_size = validate_pcm_frame_len(pcm, channel_count, decoder.sample_rate())?;
        let result = unsafe {
            opus_decoder_dred_decode(
                decoder.as_mut_ptr(),
                state.raw.as_ptr(),
                dred_offset,
                pcm.as_mut_ptr(),
                frame_size,
            )
        };
        if result < 0 {
            return Err(Error::from_code(result));
        }
        usize::try_from(result).map_err(|_| Error::InternalError)
    }

    /// Decode redundancy into f32 PCM using a normal Opus decoder.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidState`] if handles are invalid, [`Error::BadArg`] for
    /// invalid buffer sizing, or a mapped libopus error from
    /// [`opus_decoder_dred_decode_float`].
    pub fn decode_into_f32(
        &mut self,
        decoder: &mut Decoder,
        state: &DredState,
        dred_offset: i32,
        pcm: &mut [f32],
    ) -> Result<usize> {
        let channel_count = decoder.channels().as_usize();
        let frame_size = validate_pcm_frame_len(pcm, channel_count, decoder.sample_rate())?;
        let result = unsafe {
            opus_decoder_dred_decode_float(
                decoder.as_mut_ptr(),
                state.raw.as_ptr(),
                dred_offset,
                pcm.as_mut_ptr(),
                frame_size,
            )
        };
        if result < 0 {
            return Err(Error::from_code(result));
        }
        usize::try_from(result).map_err(|_| Error::InternalError)
    }
}

fn checked_max_dred_samples(max_dred_samples: usize) -> Result<i32> {
    if max_dred_samples > MAX_SAFE_DRED_SAMPLES {
        return Err(Error::BadArg);
    }
    i32::try_from(max_dred_samples).map_err(|_| Error::BadArg)
}

impl<'a> DredDecoderRef<'a> {
    /// Wrap an externally-initialized DRED decoder without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`DredDecoder::size()`] bytes
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    ///
    /// Use [`DredDecoder::init_in_place`] to initialize the memory before calling this.
    #[must_use]
    pub unsafe fn from_raw(ptr: *mut OpusDREDDecoder) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        let decoder =
            DredDecoder::from_raw(unsafe { NonNull::new_unchecked(ptr) }, Ownership::Borrowed);
        Self {
            inner: decoder,
            _marker: PhantomData,
        }
    }

    /// Initialize and wrap an externally allocated buffer.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the buffer is too small, or a mapped libopus error.
    pub fn init_in(buf: &'a mut AlignedBuffer) -> Result<Self> {
        let required = DredDecoder::size()?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusDREDDecoder>();
        unsafe { DredDecoder::init_in_place(ptr)? };
        Ok(unsafe { Self::from_raw(ptr) })
    }
}

impl Deref for DredDecoderRef<'_> {
    type Target = DredDecoder;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for DredDecoderRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

fn validate_pcm_frame_len<T>(
    pcm: &[T],
    channel_count: usize,
    sample_rate: SampleRate,
) -> Result<i32> {
    if channel_count == 0 {
        return Err(Error::InvalidState);
    }
    if pcm.is_empty() {
        return Err(Error::BadArg);
    }
    if !pcm.len().is_multiple_of(channel_count) {
        return Err(Error::BadArg);
    }
    let frame_size_per_ch = pcm.len() / channel_count;
    if frame_size_per_ch == 0 || frame_size_per_ch > max_frame_samples_for(sample_rate) {
        return Err(Error::BadArg);
    }
    // libopus requires DRED decode frame sizes to be multiples of 2.5 ms.
    if !is_frame_size_2_5ms_aligned(frame_size_per_ch, sample_rate) {
        return Err(Error::BadArg);
    }
    i32::try_from(frame_size_per_ch).map_err(|_| Error::BadArg)
}

/// Managed handle for libopus `OpusDRED` state.
pub struct DredState {
    raw: NonNull<OpusDRED>,
    size: usize,
}

unsafe impl Send for DredState {}
unsafe impl Sync for DredState {}

impl DredState {
    /// Allocate a new DRED state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::AllocFail`] if allocation fails or a mapped libopus error when
    /// creation does not succeed.
    pub fn new() -> Result<Self> {
        let size = Self::size()?;
        let mut err = 0;
        let ptr = unsafe { opus_dred_alloc(std::ptr::addr_of_mut!(err)) };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let ptr = NonNull::new(ptr).ok_or(Error::AllocFail)?;
        // opus_dred_alloc() is malloc-like and does not initialize OpusDRED.
        unsafe { std::ptr::write_bytes(ptr.as_ptr().cast::<u8>(), 0, size) };
        Ok(Self { raw: ptr, size })
    }

    /// Reset the state in place, equivalent to a fresh [`DredState::new`]
    /// without the allocation: `new` only zeroes the malloc'd block, so
    /// re-zeroing restores the exact initial state.
    pub fn reset(&mut self) {
        unsafe { std::ptr::write_bytes(self.raw.as_ptr().cast::<u8>(), 0, self.size) };
    }

    /// Size of a DRED state in bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unimplemented`] if DRED is disabled in the linked
    /// libopus, or [`Error::InternalError`] if libopus reports an invalid size.
    pub fn size() -> Result<usize> {
        let raw = unsafe { opus_dred_get_size() };
        if raw == 0 {
            return Err(Error::Unimplemented);
        }
        if raw < 0 {
            return Err(Error::InternalError);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Borrow the raw pointer.
    pub fn as_mut_ptr(&mut self) -> *mut OpusDRED {
        self.raw.as_ptr()
    }
}

impl Drop for DredState {
    fn drop(&mut self) {
        unsafe { opus_dred_free(self.raw.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_pcm_frame_len_checks_arguments() {
        // 2.5 ms at 48 kHz = 120 samples/ch, so 240 total for stereo.
        let pcm = vec![0i16; 240];
        assert!(validate_pcm_frame_len(&pcm, 2, SampleRate::Hz48000).is_ok());

        let err = validate_pcm_frame_len(&pcm, 0, SampleRate::Hz48000).unwrap_err();
        assert_eq!(err, Error::InvalidState);

        let err = validate_pcm_frame_len(&pcm[..3], 2, SampleRate::Hz48000).unwrap_err();
        assert_eq!(err, Error::BadArg);

        let err = validate_pcm_frame_len(&[] as &[i16], 2, SampleRate::Hz48000).unwrap_err();
        assert_eq!(err, Error::BadArg);

        // Non-2.5ms-aligned frame size must be rejected.
        let bad_pcm = vec![0i16; 4];
        let err = validate_pcm_frame_len(&bad_pcm, 2, SampleRate::Hz48000).unwrap_err();
        assert_eq!(err, Error::BadArg);
    }

    #[test]
    fn checked_max_dred_samples_blocks_overflow_inputs() {
        assert_eq!(
            checked_max_dred_samples(MAX_SAFE_DRED_SAMPLES),
            Ok(i32::MAX / 100)
        );
        assert_eq!(
            checked_max_dred_samples(MAX_SAFE_DRED_SAMPLES + 1),
            Err(Error::BadArg)
        );
    }

    #[test]
    fn fresh_dred_state_is_inactive() {
        let mut decoder = match DredDecoder::new() {
            Ok(decoder) => decoder,
            Err(Error::Unimplemented) => return,
            Err(err) => panic!("unexpected DRED decoder error: {err:?}"),
        };
        let state = match DredState::new() {
            Ok(state) => state,
            Err(Error::Unimplemented) => return,
            Err(err) => panic!("unexpected DRED state error: {err:?}"),
        };
        let mut dst = DredState::new().unwrap();

        assert_eq!(decoder.process(&state, &mut dst), Err(Error::BadArg));
    }
}
