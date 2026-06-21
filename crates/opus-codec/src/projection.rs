//! Safe wrappers for the libopus projection (ambisonics) API

use crate::bindings::{
    OPUS_BITRATE_MAX, OPUS_GET_BITRATE_REQUEST, OPUS_PROJECTION_GET_DEMIXING_MATRIX_GAIN_REQUEST,
    OPUS_PROJECTION_GET_DEMIXING_MATRIX_REQUEST, OPUS_PROJECTION_GET_DEMIXING_MATRIX_SIZE_REQUEST,
    OPUS_SET_BITRATE_REQUEST, OpusProjectionDecoder, OpusProjectionEncoder,
    opus_projection_ambisonics_encoder_create, opus_projection_ambisonics_encoder_get_size,
    opus_projection_ambisonics_encoder_init, opus_projection_decode, opus_projection_decode_float,
    opus_projection_decoder_create, opus_projection_decoder_destroy,
    opus_projection_decoder_get_size, opus_projection_decoder_init, opus_projection_encode,
    opus_projection_encode_float, opus_projection_encoder_ctl, opus_projection_encoder_destroy,
};
use crate::constants::{is_frame_size_2_5ms_aligned, max_frame_samples_for};
use crate::error::{Error, Result};
use crate::types::{Application, Bitrate, SampleRate};
use crate::{AlignedBuffer, Ownership, RawHandle};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Safe wrapper around `OpusProjectionEncoder`.
pub struct ProjectionEncoder {
    raw: RawHandle<OpusProjectionEncoder>,
    sample_rate: SampleRate,
    channels: u8,
    streams: u8,
    coupled_streams: u8,
}

unsafe impl Send for ProjectionEncoder {}
unsafe impl Sync for ProjectionEncoder {}

/// Borrowed wrapper around a projection encoder state.
pub struct ProjectionEncoderRef<'a> {
    inner: ProjectionEncoder,
    _marker: PhantomData<&'a mut OpusProjectionEncoder>,
}

unsafe impl Send for ProjectionEncoderRef<'_> {}
unsafe impl Sync for ProjectionEncoderRef<'_> {}

impl ProjectionEncoder {
    fn from_raw(
        ptr: NonNull<OpusProjectionEncoder>,
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
        ownership: Ownership,
    ) -> Self {
        Self {
            raw: RawHandle::new(ptr, ownership, opus_projection_encoder_destroy),
            sample_rate,
            channels,
            streams,
            coupled_streams,
        }
    }

    /// Size in bytes of a projection encoder state for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the channel/mapping configuration is invalid.
    pub fn size(channels: u8, mapping_family: i32) -> Result<usize> {
        let raw = unsafe {
            opus_projection_ambisonics_encoder_get_size(i32::from(channels), mapping_family)
        };
        if raw <= 0 {
            return Err(Error::BadArg);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Initialize a previously allocated projection encoder state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `ProjectionEncoder::size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] for invalid inputs or a mapped libopus error.
    pub unsafe fn init_in_place(
        ptr: *mut OpusProjectionEncoder,
        sample_rate: SampleRate,
        channels: u8,
        mapping_family: i32,
        application: Application,
    ) -> Result<(u8, u8)> {
        if ptr.is_null() || channels == 0 {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        let mut streams = 0i32;
        let mut coupled = 0i32;
        let r = unsafe {
            opus_projection_ambisonics_encoder_init(
                ptr,
                sample_rate as i32,
                i32::from(channels),
                mapping_family,
                std::ptr::addr_of_mut!(streams),
                std::ptr::addr_of_mut!(coupled),
                application as i32,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok((
            u8::try_from(streams).map_err(|_| Error::BadArg)?,
            u8::try_from(coupled).map_err(|_| Error::BadArg)?,
        ))
    }

    /// Create a new projection encoder using the ambisonics helper.
    ///
    /// Returns [`Error::BadArg`] for unsupported channel/mapping combinations
    /// or propagates libopus allocation failures.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] for invalid arguments or the libopus error produced by
    /// the underlying create call; [`Error::AllocFail`] if libopus returns a null handle.
    pub fn new(
        sample_rate: SampleRate,
        channels: u8,
        mapping_family: i32,
        application: Application,
    ) -> Result<Self> {
        let mut err = 0i32;
        let mut streams = 0i32;
        let mut coupled = 0i32;
        let enc = unsafe {
            opus_projection_ambisonics_encoder_create(
                sample_rate as i32,
                i32::from(channels),
                mapping_family,
                &raw mut streams,
                &raw mut coupled,
                application as i32,
                &raw mut err,
            )
        };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let enc = NonNull::new(enc).ok_or(Error::AllocFail)?;
        let streams_u8 = u8::try_from(streams).map_err(|_| Error::BadArg)?;
        let coupled_u8 = u8::try_from(coupled).map_err(|_| Error::BadArg)?;
        Ok(Self::from_raw(
            enc,
            sample_rate,
            channels,
            streams_u8,
            coupled_u8,
            Ownership::Owned,
        ))
    }

    fn validate_frame_size(&self, frame_size_per_ch: usize) -> Result<i32> {
        let frame_size = NonZeroUsize::new(frame_size_per_ch).ok_or(Error::BadArg)?;
        if frame_size.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }
        i32::try_from(frame_size.get()).map_err(|_| Error::BadArg)
    }

    fn ensure_pcm_layout(&self, len: usize, frame_size_per_ch: usize) -> Result<()> {
        let expected = frame_size_per_ch
            .checked_mul(self.channels as usize)
            .ok_or(Error::BadArg)?;
        if len != expected {
            return Err(Error::BadArg);
        }
        Ok(())
    }

    /// Encode interleaved `i16` PCM.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle was freed, [`Error::BadArg`] for
    /// buffer/layout issues, the libopus error mapped via [`Error::from_code`], or
    /// [`Error::InternalError`] if libopus reports an impossible packet length.
    pub fn encode(
        &mut self,
        pcm: &[i16],
        frame_size_per_ch: usize,
        out: &mut [u8],
    ) -> Result<usize> {
        if out.is_empty() || out.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        self.ensure_pcm_layout(pcm.len(), frame_size_per_ch)?;
        let frame_size = self.validate_frame_size(frame_size_per_ch)?;
        let out_len = i32::try_from(out.len()).map_err(|_| Error::BadArg)?;
        let n = unsafe {
            opus_projection_encode(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                frame_size,
                out.as_mut_ptr(),
                out_len,
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Encode interleaved `f32` PCM.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle was freed, [`Error::BadArg`] for
    /// buffer/layout issues, the libopus error mapped via [`Error::from_code`], or
    /// [`Error::InternalError`] if libopus reports an impossible packet length.
    pub fn encode_float(
        &mut self,
        pcm: &[f32],
        frame_size_per_ch: usize,
        out: &mut [u8],
    ) -> Result<usize> {
        if out.is_empty() || out.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        self.ensure_pcm_layout(pcm.len(), frame_size_per_ch)?;
        let frame_size = self.validate_frame_size(frame_size_per_ch)?;
        let out_len = i32::try_from(out.len()).map_err(|_| Error::BadArg)?;
        let n = unsafe {
            opus_projection_encode_float(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                frame_size,
                out.as_mut_ptr(),
                out_len,
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Set target bitrate for the encoder.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid or a mapped libopus error.
    pub fn set_bitrate(&mut self, bitrate: Bitrate) -> Result<()> {
        self.simple_ctl(OPUS_SET_BITRATE_REQUEST as i32, bitrate.value())
    }

    /// Query current bitrate configuration.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid or a mapped libopus error.
    pub fn bitrate(&mut self) -> Result<Bitrate> {
        let v = self.get_int_ctl(OPUS_GET_BITRATE_REQUEST as i32)?;
        Ok(match v {
            x if x == crate::bindings::OPUS_AUTO => Bitrate::Auto,
            x if x == OPUS_BITRATE_MAX => Bitrate::Max,
            other => Bitrate::Custom(other),
        })
    }

    /// Size in bytes of the current demixing matrix.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid or a mapped libopus error.
    pub fn demixing_matrix_size(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_PROJECTION_GET_DEMIXING_MATRIX_SIZE_REQUEST as i32)
    }

    /// Gain (in Q8 dB) of the demixing matrix.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid or a mapped libopus error.
    pub fn demixing_matrix_gain(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_PROJECTION_GET_DEMIXING_MATRIX_GAIN_REQUEST as i32)
    }

    /// Copy the demixing matrix into `out` and return the number of bytes written.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid, [`Error::BufferTooSmall`]
    /// when `out` cannot fit the matrix, a mapped libopus error, or [`Error::InternalError`]
    /// when libopus reports an invalid matrix size.
    pub fn write_demixing_matrix(&mut self, out: &mut [u8]) -> Result<usize> {
        let size = self.demixing_matrix_size()?;
        if size <= 0 {
            return Err(Error::InternalError);
        }
        let needed = usize::try_from(size).map_err(|_| Error::InternalError)?;
        if out.len() < needed {
            return Err(Error::BufferTooSmall);
        }
        let r = unsafe {
            opus_projection_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_PROJECTION_GET_DEMIXING_MATRIX_REQUEST as i32,
                out.as_mut_ptr(),
                size,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(needed)
    }

    /// Convenience helper returning the demixing matrix as a newly allocated buffer.
    ///
    /// # Errors
    /// Propagates errors from [`Self::demixing_matrix_size`] and [`Self::write_demixing_matrix`],
    /// including [`Error::InternalError`] if libopus reports impossible sizes.
    pub fn demixing_matrix_bytes(&mut self) -> Result<Vec<u8>> {
        let size = self.demixing_matrix_size()?;
        let len = usize::try_from(size).map_err(|_| Error::InternalError)?;
        let mut buf = vec![0u8; len];
        self.write_demixing_matrix(&mut buf)?;
        Ok(buf)
    }

    /// Number of coded streams.
    #[must_use]
    pub const fn streams(&self) -> u8 {
        self.streams
    }

    /// Number of coupled (stereo) coded streams.
    #[must_use]
    pub const fn coupled_streams(&self) -> u8 {
        self.coupled_streams
    }

    /// Input channels passed to the encoder.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }

    /// Encoder sample rate.
    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    fn simple_ctl(&mut self, req: i32, val: i32) -> Result<()> {
        let r = unsafe { opus_projection_encoder_ctl(self.raw.as_ptr(), req, val) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    fn get_int_ctl(&mut self, req: i32) -> Result<i32> {
        let mut v = 0i32;
        let r = unsafe { opus_projection_encoder_ctl(self.raw.as_ptr(), req, &mut v) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(v)
    }
}

impl<'a> ProjectionEncoderRef<'a> {
    /// Wrap an externally-initialized projection encoder without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`ProjectionEncoder::size()`] bytes
    /// - `ptr` must be aligned to at least `align_of::<usize>()` (malloc-style alignment)
    /// - `sample_rate`, `channels`, `streams`, and `coupled_streams` must exactly match
    ///   the encoder state already stored at `ptr`
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    ///
    /// Passing mismatched metadata is undefined behavior: later safe methods may validate buffer
    /// sizes with the wrong layout and then call libopus with out-of-bounds buffers.
    ///
    /// Use [`ProjectionEncoder::init_in_place`] to initialize the memory before calling this.
    #[must_use]
    pub unsafe fn from_raw(
        ptr: *mut OpusProjectionEncoder,
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
    ) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        let encoder = ProjectionEncoder::from_raw(
            unsafe { NonNull::new_unchecked(ptr) },
            sample_rate,
            channels,
            streams,
            coupled_streams,
            Ownership::Borrowed,
        );
        Self {
            inner: encoder,
            _marker: PhantomData,
        }
    }

    /// Initialize and wrap an externally allocated buffer.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the buffer is too small, or a mapped libopus error.
    pub fn init_in(
        buf: &'a mut AlignedBuffer,
        sample_rate: SampleRate,
        channels: u8,
        mapping_family: i32,
        application: Application,
    ) -> Result<Self> {
        let required = ProjectionEncoder::size(channels, mapping_family)?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusProjectionEncoder>();
        let (streams, coupled) = unsafe {
            ProjectionEncoder::init_in_place(
                ptr,
                sample_rate,
                channels,
                mapping_family,
                application,
            )?
        };
        Ok(unsafe { Self::from_raw(ptr, sample_rate, channels, streams, coupled) })
    }
}

impl Deref for ProjectionEncoderRef<'_> {
    type Target = ProjectionEncoder;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ProjectionEncoderRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Safe wrapper around `OpusProjectionDecoder`.
pub struct ProjectionDecoder {
    raw: RawHandle<OpusProjectionDecoder>,
    sample_rate: SampleRate,
    channels: u8,
    streams: u8,
    coupled_streams: u8,
}

unsafe impl Send for ProjectionDecoder {}
unsafe impl Sync for ProjectionDecoder {}

/// Borrowed wrapper around a projection decoder state.
pub struct ProjectionDecoderRef<'a> {
    inner: ProjectionDecoder,
    _marker: PhantomData<&'a mut OpusProjectionDecoder>,
}

unsafe impl Send for ProjectionDecoderRef<'_> {}
unsafe impl Sync for ProjectionDecoderRef<'_> {}

impl ProjectionDecoder {
    fn from_raw(
        ptr: NonNull<OpusProjectionDecoder>,
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
        ownership: Ownership,
    ) -> Self {
        Self {
            raw: RawHandle::new(ptr, ownership, opus_projection_decoder_destroy),
            sample_rate,
            channels,
            streams,
            coupled_streams,
        }
    }

    /// Size in bytes of a projection decoder state for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the channel/stream configuration is invalid.
    pub fn size(channels: u8, streams: u8, coupled_streams: u8) -> Result<usize> {
        let raw = unsafe {
            opus_projection_decoder_get_size(
                i32::from(channels),
                i32::from(streams),
                i32::from(coupled_streams),
            )
        };
        if raw <= 0 {
            return Err(Error::BadArg);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Initialize a previously allocated projection decoder state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `ProjectionDecoder::size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] for invalid inputs or a mapped libopus error.
    pub unsafe fn init_in_place(
        ptr: *mut OpusProjectionDecoder,
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
        demixing_matrix: &[u8],
    ) -> Result<()> {
        if ptr.is_null() || demixing_matrix.is_empty() {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        let matrix_len = i32::try_from(demixing_matrix.len()).map_err(|_| Error::BadArg)?;
        let mut demixing_matrix = demixing_matrix.to_vec();
        // SAFETY: libopus' C ABI takes a non-const pointer despite documenting this
        // parameter as input-only. Pass a mutable scratch copy so C never receives a
        // mutable pointer into the caller's immutable slice. libopus copies the
        // bytes before returning and does not retain this pointer.
        let r = unsafe {
            opus_projection_decoder_init(
                ptr,
                sample_rate as i32,
                i32::from(channels),
                i32::from(streams),
                i32::from(coupled_streams),
                demixing_matrix.as_mut_ptr(),
                matrix_len,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Create a projection decoder given the demixing matrix provided by the encoder.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] for invalid inputs, `Error::from_code` for libopus failures,
    /// or [`Error::AllocFail`] if libopus returns a null handle.
    pub fn new(
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
        demixing_matrix: &[u8],
    ) -> Result<Self> {
        if demixing_matrix.is_empty() {
            return Err(Error::BadArg);
        }
        let matrix_len = i32::try_from(demixing_matrix.len()).map_err(|_| Error::BadArg)?;
        let mut demixing_matrix = demixing_matrix.to_vec();
        let mut err = 0i32;
        // SAFETY: see comment in init_in_place; libopus copies the scratch input
        // before returning and does not retain this pointer.
        let dec = unsafe {
            opus_projection_decoder_create(
                sample_rate as i32,
                i32::from(channels),
                i32::from(streams),
                i32::from(coupled_streams),
                demixing_matrix.as_mut_ptr(),
                matrix_len,
                &raw mut err,
            )
        };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let dec = NonNull::new(dec).ok_or(Error::AllocFail)?;
        Ok(Self::from_raw(
            dec,
            sample_rate,
            channels,
            streams,
            coupled_streams,
            Ownership::Owned,
        ))
    }

    fn validate_frame_size(&self, frame_size_per_ch: usize) -> Result<i32> {
        let frame_size = NonZeroUsize::new(frame_size_per_ch).ok_or(Error::BadArg)?;
        if frame_size.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }
        i32::try_from(frame_size.get()).map_err(|_| Error::BadArg)
    }

    fn ensure_output_layout(&self, len: usize, frame_size_per_ch: usize) -> Result<()> {
        let expected = frame_size_per_ch
            .checked_mul(self.channels as usize)
            .ok_or(Error::BadArg)?;
        if len != expected {
            return Err(Error::BadArg);
        }
        Ok(())
    }

    /// Decode into interleaved `i16` PCM.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle was freed, [`Error::BadArg`] for
    /// buffer/layout issues, a mapped libopus error, or [`Error::InternalError`] if libopus
    /// reports an impossible decoded sample count.
    pub fn decode(
        &mut self,
        packet: &[u8],
        out: &mut [i16],
        frame_size_per_ch: usize,
        fec: bool,
    ) -> Result<usize> {
        self.ensure_output_layout(out.len(), frame_size_per_ch)?;
        let frame_size = self.validate_frame_size(frame_size_per_ch)?;
        // libopus requires PLC/FEC frame sizes to be multiples of 2.5 ms.
        if (packet.is_empty() || fec)
            && !is_frame_size_2_5ms_aligned(frame_size_per_ch, self.sample_rate)
        {
            return Err(Error::BadArg);
        }
        let packet_len = if packet.is_empty() {
            0
        } else {
            i32::try_from(packet.len()).map_err(|_| Error::BadArg)?
        };
        let n = unsafe {
            opus_projection_decode(
                self.raw.as_ptr(),
                if packet.is_empty() {
                    std::ptr::null()
                } else {
                    packet.as_ptr()
                },
                packet_len,
                out.as_mut_ptr(),
                frame_size,
                i32::from(fec),
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Decode into interleaved `f32` PCM.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle was freed, [`Error::BadArg`] for
    /// buffer/layout issues, a mapped libopus error, or [`Error::InternalError`] if libopus
    /// reports an impossible decoded sample count.
    pub fn decode_float(
        &mut self,
        packet: &[u8],
        out: &mut [f32],
        frame_size_per_ch: usize,
        fec: bool,
    ) -> Result<usize> {
        self.ensure_output_layout(out.len(), frame_size_per_ch)?;
        let frame_size = self.validate_frame_size(frame_size_per_ch)?;
        // libopus requires PLC/FEC frame sizes to be multiples of 2.5 ms.
        if (packet.is_empty() || fec)
            && !is_frame_size_2_5ms_aligned(frame_size_per_ch, self.sample_rate)
        {
            return Err(Error::BadArg);
        }
        let packet_len = if packet.is_empty() {
            0
        } else {
            i32::try_from(packet.len()).map_err(|_| Error::BadArg)?
        };
        let n = unsafe {
            opus_projection_decode_float(
                self.raw.as_ptr(),
                if packet.is_empty() {
                    std::ptr::null()
                } else {
                    packet.as_ptr()
                },
                packet_len,
                out.as_mut_ptr(),
                frame_size,
                i32::from(fec),
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Output channel count.
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }

    /// Number of coded streams expected in the input bitstream.
    #[must_use]
    pub const fn streams(&self) -> u8 {
        self.streams
    }

    /// Number of coupled coded streams expected in the input bitstream.
    #[must_use]
    pub const fn coupled_streams(&self) -> u8 {
        self.coupled_streams
    }

    /// Decoder sample rate.
    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }
}

impl<'a> ProjectionDecoderRef<'a> {
    /// Wrap an externally-initialized projection decoder without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`ProjectionDecoder::size()`] bytes
    /// - `ptr` must be aligned to at least `align_of::<usize>()` (malloc-style alignment)
    /// - `sample_rate`, `channels`, `streams`, and `coupled_streams` must exactly match
    ///   the decoder state already stored at `ptr`
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    ///
    /// Passing mismatched metadata is undefined behavior: later safe methods may validate buffer
    /// sizes with the wrong layout and then call libopus with out-of-bounds buffers.
    ///
    /// Use [`ProjectionDecoder::init_in_place`] to initialize the memory before calling this.
    #[must_use]
    pub unsafe fn from_raw(
        ptr: *mut OpusProjectionDecoder,
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
    ) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        let decoder = ProjectionDecoder::from_raw(
            unsafe { NonNull::new_unchecked(ptr) },
            sample_rate,
            channels,
            streams,
            coupled_streams,
            Ownership::Borrowed,
        );
        Self {
            inner: decoder,
            _marker: PhantomData,
        }
    }

    /// Initialize and wrap an externally allocated buffer.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the buffer is too small, or a mapped libopus error.
    pub fn init_in(
        buf: &'a mut AlignedBuffer,
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
        demixing_matrix: &[u8],
    ) -> Result<Self> {
        let required = ProjectionDecoder::size(channels, streams, coupled_streams)?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusProjectionDecoder>();
        unsafe {
            ProjectionDecoder::init_in_place(
                ptr,
                sample_rate,
                channels,
                streams,
                coupled_streams,
                demixing_matrix,
            )?;
        }
        Ok(unsafe { Self::from_raw(ptr, sample_rate, channels, streams, coupled_streams) })
    }
}

impl Deref for ProjectionDecoderRef<'_> {
    type Target = ProjectionDecoder;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for ProjectionDecoderRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
