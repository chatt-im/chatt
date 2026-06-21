//! Opus decoder implementation with safe wrappers

#[cfg(feature = "dred")]
use crate::bindings::{
    OPUS_GET_DRED_DURATION_REQUEST, OPUS_SET_DNN_BLOB_REQUEST, OPUS_SET_DRED_DURATION_REQUEST,
};
use crate::bindings::{
    OPUS_GET_FINAL_RANGE_REQUEST, OPUS_GET_GAIN_REQUEST, OPUS_GET_LAST_PACKET_DURATION_REQUEST,
    OPUS_GET_PHASE_INVERSION_DISABLED_REQUEST, OPUS_GET_PITCH_REQUEST,
    OPUS_GET_SAMPLE_RATE_REQUEST, OPUS_RESET_STATE, OPUS_SET_GAIN_REQUEST,
    OPUS_SET_PHASE_INVERSION_DISABLED_REQUEST, OpusDecoder, opus_decode, opus_decode_float,
    opus_decoder_create, opus_decoder_ctl, opus_decoder_destroy, opus_decoder_get_nb_samples,
    opus_decoder_get_size, opus_decoder_init,
};
use crate::constants::{is_frame_size_2_5ms_aligned, max_frame_samples_for};
use crate::error::{Error, Result};
use crate::packet;
use crate::types::{Bandwidth, Channels, SampleRate};
use crate::{AlignedBuffer, Ownership, RawHandle};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::ptr::{self, NonNull};

/// Safe wrapper around a libopus `OpusDecoder`.
pub struct Decoder {
    raw: RawHandle<OpusDecoder>,
    sample_rate: SampleRate,
    channels: Channels,
}

unsafe impl Send for Decoder {}
unsafe impl Sync for Decoder {}

/// Borrowed wrapper around a decoder state.
pub struct DecoderRef<'a> {
    inner: Decoder,
    _marker: PhantomData<&'a mut OpusDecoder>,
}

unsafe impl Send for DecoderRef<'_> {}
unsafe impl Sync for DecoderRef<'_> {}

impl Decoder {
    fn from_raw(
        ptr: NonNull<OpusDecoder>,
        sample_rate: SampleRate,
        channels: Channels,
        ownership: Ownership,
    ) -> Self {
        Self {
            raw: RawHandle::new(ptr, ownership, opus_decoder_destroy),
            sample_rate,
            channels,
        }
    }

    /// Size in bytes of a decoder state for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the channel count is invalid or libopus reports
    /// an impossible size.
    pub fn size(channels: Channels) -> Result<usize> {
        let raw = unsafe { opus_decoder_get_size(channels.as_i32()) };
        if raw <= 0 {
            return Err(Error::BadArg);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Initialize a previously allocated decoder state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `Decoder::size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if `ptr` is null, or a mapped libopus error.
    pub unsafe fn init_in_place(
        ptr: *mut OpusDecoder,
        sample_rate: SampleRate,
        channels: Channels,
    ) -> Result<()> {
        if ptr.is_null() {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        let r = unsafe { opus_decoder_init(ptr, sample_rate.as_i32(), channels.as_i32()) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Create a new decoder for a given sample rate and channel layout.
    ///
    /// # Errors
    /// Returns an error if allocation fails or arguments are invalid.
    pub fn new(sample_rate: SampleRate, channels: Channels) -> Result<Self> {
        // Validate sample rate
        if !sample_rate.is_valid() {
            return Err(Error::BadArg);
        }

        let mut error = 0i32;
        let decoder = unsafe {
            opus_decoder_create(
                sample_rate.as_i32(),
                channels.as_i32(),
                std::ptr::addr_of_mut!(error),
            )
        };

        if error != 0 {
            return Err(Error::from_code(error));
        }

        let decoder = NonNull::new(decoder).ok_or(Error::AllocFail)?;

        Ok(Self::from_raw(
            decoder,
            sample_rate,
            channels,
            Ownership::Owned,
        ))
    }

    /// Decode a packet into 16-bit PCM.
    ///
    /// - `input`: Opus packet bytes. Pass empty slice to invoke PLC.
    /// - `output`: Interleaved output buffer sized to `frame_size * channels`.
    /// - `fec`: Enable in-band FEC if available.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is invalid, [`Error::BadArg`]
    /// for invalid buffer sizes or frame sizes, or a mapped libopus error via
    /// [`Error::from_code`].
    pub fn decode(&mut self, input: &[u8], output: &mut [i16], fec: bool) -> Result<usize> {
        // Errors: InvalidState, BadArg, or libopus error mapped.
        // Validate buffer sizes up-front
        if !input.is_empty() && input.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        if output.is_empty() {
            return Err(Error::BadArg);
        }
        if !output.len().is_multiple_of(self.channels.as_usize()) {
            return Err(Error::BadArg);
        }
        let frame_size = output.len() / self.channels.as_usize();
        let frame_size = NonZeroUsize::new(frame_size).ok_or(Error::BadArg)?;
        let max_frame = max_frame_samples_for(self.sample_rate);
        if frame_size.get() > max_frame {
            return Err(Error::BadArg);
        }
        // libopus requires PLC/FEC frame sizes to be multiples of 2.5 ms.
        if (input.is_empty() || fec)
            && !is_frame_size_2_5ms_aligned(frame_size.get(), self.sample_rate)
        {
            return Err(Error::BadArg);
        }

        let input_len_i32 = if input.is_empty() {
            0
        } else {
            i32::try_from(input.len()).map_err(|_| Error::BadArg)?
        };
        let frame_size_i32 = i32::try_from(frame_size.get()).map_err(|_| Error::BadArg)?;

        let result = unsafe {
            opus_decode(
                self.raw.as_ptr(),
                if input.is_empty() {
                    ptr::null()
                } else {
                    input.as_ptr()
                },
                input_len_i32,
                output.as_mut_ptr(),
                frame_size_i32,
                i32::from(fec),
            )
        };

        if result < 0 {
            return Err(Error::from_code(result));
        }

        usize::try_from(result).map_err(|_| Error::InternalError)
    }

    /// Decode a packet into `f32` PCM.
    ///
    /// See [`Self::decode`] for parameter semantics.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is invalid, [`Error::BadArg`]
    /// for invalid buffer sizes or frame sizes, or a mapped libopus error via
    /// [`Error::from_code`].
    pub fn decode_float(&mut self, input: &[u8], output: &mut [f32], fec: bool) -> Result<usize> {
        // Validate buffer sizes up-front
        if !input.is_empty() && input.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        if output.is_empty() {
            return Err(Error::BadArg);
        }
        if !output.len().is_multiple_of(self.channels.as_usize()) {
            return Err(Error::BadArg);
        }
        let frame_size = output.len() / self.channels.as_usize();
        let frame_size = NonZeroUsize::new(frame_size).ok_or(Error::BadArg)?;
        let max_frame = max_frame_samples_for(self.sample_rate);
        if frame_size.get() > max_frame {
            return Err(Error::BadArg);
        }
        // libopus requires PLC/FEC frame sizes to be multiples of 2.5 ms.
        if (input.is_empty() || fec)
            && !is_frame_size_2_5ms_aligned(frame_size.get(), self.sample_rate)
        {
            return Err(Error::BadArg);
        }

        let input_len_i32 = if input.is_empty() {
            0
        } else {
            i32::try_from(input.len()).map_err(|_| Error::BadArg)?
        };
        let frame_size_i32 = i32::try_from(frame_size.get()).map_err(|_| Error::BadArg)?;

        let result = unsafe {
            opus_decode_float(
                self.raw.as_ptr(),
                if input.is_empty() {
                    ptr::null()
                } else {
                    input.as_ptr()
                },
                input_len_i32,
                output.as_mut_ptr(),
                frame_size_i32,
                i32::from(fec),
            )
        };

        if result < 0 {
            return Err(Error::from_code(result));
        }

        usize::try_from(result).map_err(|_| Error::InternalError)
    }

    /// Return the number of samples (per channel) in an Opus `packet` at this decoder's rate.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, [`Error::BadArg`] for
    /// overlong input, or a mapped libopus error.
    pub fn packet_samples(&self, packet: &[u8]) -> Result<usize> {
        // Errors: InvalidState or libopus error mapped.
        if packet.is_empty() {
            return Err(Error::BadArg);
        }
        if packet.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        let len_i32 = i32::try_from(packet.len()).map_err(|_| Error::BadArg)?;
        let result =
            unsafe { opus_decoder_get_nb_samples(self.raw.as_ptr(), packet.as_ptr(), len_i32) };

        if result < 0 {
            return Err(Error::from_code(result));
        }

        usize::try_from(result).map_err(|_| Error::InternalError)
    }

    /// Return the bandwidth encoded in an Opus `packet`.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or [`Error::InvalidPacket`]
    /// if the packet cannot be parsed.
    pub fn packet_bandwidth(&self, packet: &[u8]) -> Result<Bandwidth> {
        // Errors: InvalidState or InvalidPacket.
        packet::packet_bandwidth(packet)
    }

    /// Return the number of channels described by an Opus `packet`.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or [`Error::InvalidPacket`]
    /// if the packet cannot be parsed.
    pub fn packet_channels(&self, packet: &[u8]) -> Result<Channels> {
        // Errors: InvalidState or InvalidPacket.
        packet::packet_channels(packet)
    }

    /// Reset the decoder to its initial state.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error
    /// if resetting fails.
    pub fn reset(&mut self) -> Result<()> {
        // Errors: InvalidState or request failure.
        // OPUS_RESET_STATE takes no additional argument. Passing extras is undefined behavior.
        let result = unsafe { opus_decoder_ctl(self.raw.as_ptr(), OPUS_RESET_STATE as i32) };

        if result != 0 {
            return Err(Error::from_code(result));
        }

        Ok(())
    }

    /// The decoder's configured sample rate.
    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// The decoder's channel configuration.
    #[must_use]
    pub const fn channels(&self) -> Channels {
        self.channels
    }

    #[cfg_attr(not(feature = "dred"), allow(dead_code))]
    pub(crate) fn as_mut_ptr(&mut self) -> *mut OpusDecoder {
        self.raw.as_ptr()
    }

    /// Query decoder output sample rate.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn get_sample_rate(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_SAMPLE_RATE_REQUEST as i32)
    }

    /// Query pitch (fundamental period) of the last decoded frame (in samples at 48 kHz domain).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn get_pitch(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_PITCH_REQUEST as i32)
    }

    /// Duration (per channel) of the last decoded packet.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn get_last_packet_duration(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_LAST_PACKET_DURATION_REQUEST as i32)
    }

    /// Final RNG state after the last decode.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn final_range(&mut self) -> Result<u32> {
        let mut v: u32 = 0;
        let r = unsafe {
            opus_decoder_ctl(
                self.raw.as_ptr(),
                OPUS_GET_FINAL_RANGE_REQUEST as i32,
                &mut v,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(v)
    }

    /// Set post-decode gain in Q8 dB units.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn set_gain(&mut self, q8_db: i32) -> Result<()> {
        self.simple_ctl(OPUS_SET_GAIN_REQUEST as i32, q8_db)
    }
    /// Query post-decode gain in Q8 dB units.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn gain(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_GAIN_REQUEST as i32)
    }

    /// Returns true if phase inversion is disabled (CELT stereo decorrelation).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn phase_inversion_disabled(&mut self) -> Result<bool> {
        Ok(self.get_int_ctl(OPUS_GET_PHASE_INVERSION_DISABLED_REQUEST as i32)? != 0)
    }

    /// Disable/enable phase inversion (CELT stereo decorrelation).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn set_phase_inversion_disabled(&mut self, disabled: bool) -> Result<()> {
        self.simple_ctl(
            OPUS_SET_PHASE_INVERSION_DISABLED_REQUEST as i32,
            i32::from(disabled),
        )
    }

    #[cfg(feature = "dred")]
    /// Set DRED duration in ms (if libopus built with DRED).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn set_dred_duration(&mut self, ms: i32) -> Result<()> {
        self.simple_ctl(OPUS_SET_DRED_DURATION_REQUEST as i32, ms)
    }
    #[cfg(feature = "dred")]
    /// Query DRED duration in ms.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub fn dred_duration(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_DRED_DURATION_REQUEST as i32)
    }
    #[cfg(feature = "dred")]
    /// Set DNN blob for DRED (feature-gated; will error if unsupported).
    ///
    /// # Safety
    /// Caller must ensure `ptr` is valid for reads as expected by libopus for the duration of the call
    /// and points to a properly formatted DNN blob. Passing an invalid or dangling pointer is UB.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder is invalid, or a mapped libopus error.
    pub unsafe fn set_dnn_blob(&mut self, ptr: *const u8, len: i32) -> Result<()> {
        if ptr.is_null() || len <= 0 {
            return Err(Error::BadArg);
        }
        let r = unsafe {
            opus_decoder_ctl(
                self.raw.as_ptr(),
                OPUS_SET_DNN_BLOB_REQUEST as i32,
                ptr,
                len,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    // --- internal helpers for CTLs ---
    fn simple_ctl(&mut self, req: i32, val: i32) -> Result<()> {
        let r = unsafe { opus_decoder_ctl(self.raw.as_ptr(), req, val) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }
    fn get_int_ctl(&mut self, req: i32) -> Result<i32> {
        let mut v: i32 = 0;
        let r = unsafe { opus_decoder_ctl(self.raw.as_ptr(), req, &mut v) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(v)
    }
}

impl<'a> DecoderRef<'a> {
    /// Wrap an externally-initialized decoder without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`Decoder::size()`] bytes
    /// - `ptr` must be aligned to at least `align_of::<usize>()` (malloc-style alignment)
    /// - `sample_rate` and `channels` must exactly match the decoder state already stored at `ptr`
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    ///
    /// Passing mismatched metadata is undefined behavior: later safe methods may validate buffer
    /// sizes against the wrong channel/rate and then call libopus with out-of-bounds buffers.
    ///
    /// Use [`Decoder::init_in_place`] to initialize the memory before calling this.
    #[must_use]
    pub unsafe fn from_raw(
        ptr: *mut OpusDecoder,
        sample_rate: SampleRate,
        channels: Channels,
    ) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        let decoder = Decoder::from_raw(
            unsafe { NonNull::new_unchecked(ptr) },
            sample_rate,
            channels,
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
        channels: Channels,
    ) -> Result<Self> {
        let required = Decoder::size(channels)?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusDecoder>();
        unsafe { Decoder::init_in_place(ptr, sample_rate, channels)? };
        Ok(unsafe { Self::from_raw(ptr, sample_rate, channels) })
    }
}

impl Deref for DecoderRef<'_> {
    type Target = Decoder;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for DecoderRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
