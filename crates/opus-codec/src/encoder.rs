//! Opus encoder implementation with safe wrappers

use crate::bindings::{
    OPUS_AUTO, OPUS_BANDWIDTH_FULLBAND, OPUS_BITRATE_MAX, OPUS_GET_BANDWIDTH_REQUEST,
    OPUS_GET_BITRATE_REQUEST, OPUS_GET_COMPLEXITY_REQUEST, OPUS_GET_DTX_REQUEST,
    OPUS_GET_EXPERT_FRAME_DURATION_REQUEST, OPUS_GET_FINAL_RANGE_REQUEST,
    OPUS_GET_FORCE_CHANNELS_REQUEST, OPUS_GET_IN_DTX_REQUEST, OPUS_GET_INBAND_FEC_REQUEST,
    OPUS_GET_LOOKAHEAD_REQUEST, OPUS_GET_LSB_DEPTH_REQUEST, OPUS_GET_MAX_BANDWIDTH_REQUEST,
    OPUS_GET_PACKET_LOSS_PERC_REQUEST, OPUS_GET_PHASE_INVERSION_DISABLED_REQUEST,
    OPUS_GET_PREDICTION_DISABLED_REQUEST, OPUS_GET_SIGNAL_REQUEST, OPUS_GET_VBR_CONSTRAINT_REQUEST,
    OPUS_GET_VBR_REQUEST, OPUS_SET_BANDWIDTH_REQUEST, OPUS_SET_BITRATE_REQUEST,
    OPUS_SET_COMPLEXITY_REQUEST, OPUS_SET_DTX_REQUEST, OPUS_SET_EXPERT_FRAME_DURATION_REQUEST,
    OPUS_SET_FORCE_CHANNELS_REQUEST, OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_LSB_DEPTH_REQUEST,
    OPUS_SET_MAX_BANDWIDTH_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST,
    OPUS_SET_PHASE_INVERSION_DISABLED_REQUEST, OPUS_SET_PREDICTION_DISABLED_REQUEST,
    OPUS_SET_SIGNAL_REQUEST, OPUS_SET_VBR_CONSTRAINT_REQUEST, OPUS_SET_VBR_REQUEST, OpusEncoder,
    opus_encode, opus_encode_float, opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy,
    opus_encoder_get_size, opus_encoder_init,
};
use crate::constants::max_frame_samples_for;
use crate::error::{Error, Result};
use crate::types::{
    Application, Bandwidth, Bitrate, Channels, Complexity, ExpertFrameDuration, SampleRate, Signal,
};
use crate::{AlignedBuffer, Ownership, RawHandle};
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Safe wrapper around a libopus `OpusEncoder`.
pub struct Encoder {
    raw: RawHandle<OpusEncoder>,
    sample_rate: SampleRate,
    channels: Channels,
}

unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

/// Borrowed wrapper around an encoder state.
pub struct EncoderRef<'a> {
    inner: Encoder,
    _marker: PhantomData<&'a mut OpusEncoder>,
}

unsafe impl Send for EncoderRef<'_> {}
unsafe impl Sync for EncoderRef<'_> {}

impl Encoder {
    fn from_raw(
        ptr: NonNull<OpusEncoder>,
        sample_rate: SampleRate,
        channels: Channels,
        ownership: Ownership,
    ) -> Self {
        Self {
            raw: RawHandle::new(ptr, ownership, opus_encoder_destroy),
            sample_rate,
            channels,
        }
    }

    /// Size in bytes of an encoder state for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the channel count is invalid or libopus reports
    /// an impossible size.
    pub fn size(channels: Channels) -> Result<usize> {
        let raw = unsafe { opus_encoder_get_size(channels.as_i32()) };
        if raw <= 0 {
            return Err(Error::BadArg);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Initialize a previously allocated encoder state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `Encoder::size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if `ptr` is null, or a mapped libopus error.
    pub unsafe fn init_in_place(
        ptr: *mut OpusEncoder,
        sample_rate: SampleRate,
        channels: Channels,
        application: Application,
    ) -> Result<()> {
        if ptr.is_null() {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        let r = unsafe {
            opus_encoder_init(
                ptr,
                sample_rate.as_i32(),
                channels.as_i32(),
                application as i32,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Create a new encoder.
    ///
    /// # Errors
    /// Returns an error if allocation fails or arguments are invalid.
    pub fn new(
        sample_rate: SampleRate,
        channels: Channels,
        application: Application,
    ) -> Result<Self> {
        // Validate sample rate
        if !sample_rate.is_valid() {
            return Err(Error::BadArg);
        }

        let mut error = 0i32;
        let encoder = unsafe {
            opus_encoder_create(
                sample_rate.as_i32(),
                channels.as_i32(),
                application as i32,
                std::ptr::addr_of_mut!(error),
            )
        };

        if error != 0 {
            return Err(Error::from_code(error));
        }

        let encoder = NonNull::new(encoder).ok_or(Error::AllocFail)?;

        Ok(Self::from_raw(
            encoder,
            sample_rate,
            channels,
            Ownership::Owned,
        ))
    }

    /// Encode 16-bit PCM into an Opus packet.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, [`Error::BadArg`] for
    /// invalid buffer sizes or frame size, or a mapped libopus error.
    pub fn encode(&mut self, input: &[i16], output: &mut [u8]) -> Result<usize> {
        // Validate input buffer size
        if input.is_empty() {
            return Err(Error::BadArg);
        }

        // Ensure input buffer is properly sized for the number of channels
        if !input.len().is_multiple_of(self.channels.as_usize()) {
            return Err(Error::BadArg);
        }

        let frame_size = input.len() / self.channels.as_usize();
        let frame_size = NonZeroUsize::new(frame_size).ok_or(Error::BadArg)?;
        // Validate frame size is within Opus limits for the configured sample rate
        if frame_size.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }

        // Validate output buffer size
        if output.is_empty() {
            return Err(Error::BadArg);
        }
        if output.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }

        let frame_size_i32 = i32::try_from(frame_size.get()).map_err(|_| Error::BadArg)?;
        let out_len_i32 = i32::try_from(output.len()).map_err(|_| Error::BadArg)?;
        let result = unsafe {
            opus_encode(
                self.raw.as_ptr(),
                input.as_ptr(),
                frame_size_i32,
                output.as_mut_ptr(),
                out_len_i32,
            )
        };

        if result < 0 {
            return Err(Error::from_code(result));
        }

        usize::try_from(result).map_err(|_| Error::InternalError)
    }

    /// Encode 16-bit PCM, capping output to `max_data_bytes`.
    ///
    /// Note: This does not itself enable FEC; use `set_inband_fec(true)` and
    /// `set_packet_loss_perc(…)` to actually make the encoder produce FEC.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, [`Error::BadArg`] for
    /// invalid buffer sizes or frame size, or a mapped libopus error.
    pub fn encode_limited(
        &mut self,
        input: &[i16],
        output: &mut [u8],
        max_data_bytes: usize,
    ) -> Result<usize> {
        // Validate input buffer size
        if input.is_empty() {
            return Err(Error::BadArg);
        }

        // Ensure input buffer is properly sized for the number of channels
        if !input.len().is_multiple_of(self.channels.as_usize()) {
            return Err(Error::BadArg);
        }

        let frame_size = input.len() / self.channels.as_usize();
        let frame_size = NonZeroUsize::new(frame_size).ok_or(Error::BadArg)?;
        // Validate frame size is within Opus limits for the configured sample rate
        if frame_size.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }

        // Validate output buffer size
        if output.is_empty() {
            return Err(Error::BadArg);
        }
        if output.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        // Validate max_data_bytes parameter
        if max_data_bytes == 0 || max_data_bytes > output.len() {
            return Err(Error::BadArg);
        }

        let frame_size_i32 = i32::try_from(frame_size.get()).map_err(|_| Error::BadArg)?;
        let max_bytes_i32 = i32::try_from(max_data_bytes).map_err(|_| Error::BadArg)?;
        let result = unsafe {
            opus_encode(
                self.raw.as_ptr(),
                input.as_ptr(),
                frame_size_i32,
                output.as_mut_ptr(),
                max_bytes_i32,
            )
        };

        if result < 0 {
            return Err(Error::from_code(result));
        }

        usize::try_from(result).map_err(|_| Error::InternalError)
    }

    /// Encode f32 PCM into an Opus packet.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, [`Error::BadArg`] for
    /// invalid buffer sizes or frame size, or a mapped libopus error.
    pub fn encode_float(&mut self, input: &[f32], output: &mut [u8]) -> Result<usize> {
        if input.is_empty() {
            return Err(Error::BadArg);
        }
        if !input.len().is_multiple_of(self.channels.as_usize()) {
            return Err(Error::BadArg);
        }
        let frame_size = input.len() / self.channels.as_usize();
        let frame_size = NonZeroUsize::new(frame_size).ok_or(Error::BadArg)?;
        if frame_size.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }
        if output.is_empty() || output.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        let frame_i32 = i32::try_from(frame_size.get()).map_err(|_| Error::BadArg)?;
        let out_len_i32 = i32::try_from(output.len()).map_err(|_| Error::BadArg)?;
        let n = unsafe {
            opus_encode_float(
                self.raw.as_ptr(),
                input.as_ptr(),
                frame_i32,
                output.as_mut_ptr(),
                out_len_i32,
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    // ===== Common encoder CTLs =====

    /// Enable/disable in-band FEC generation (decoder can recover from losses).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_inband_fec(&mut self, enabled: bool) -> Result<()> {
        self.simple_ctl(OPUS_SET_INBAND_FEC_REQUEST as i32, i32::from(enabled))
    }
    /// Query in-band FEC setting.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn inband_fec(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_INBAND_FEC_REQUEST as i32)
    }

    /// Hint expected packet loss percentage [0..=100].
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, [`Error::BadArg`] for out-of-range values,
    /// or a mapped libopus error.
    pub fn set_packet_loss_perc(&mut self, perc: i32) -> Result<()> {
        if !(0..=100).contains(&perc) {
            return Err(Error::BadArg);
        }
        self.simple_ctl(OPUS_SET_PACKET_LOSS_PERC_REQUEST as i32, perc)
    }
    /// Query packet loss percentage hint.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn packet_loss_perc(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_PACKET_LOSS_PERC_REQUEST as i32)
    }

    /// Enable/disable DTX (discontinuous transmission).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_dtx(&mut self, enabled: bool) -> Result<()> {
        self.simple_ctl(OPUS_SET_DTX_REQUEST as i32, i32::from(enabled))
    }
    /// Query DTX setting.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn dtx(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_DTX_REQUEST as i32)
    }
    /// Returns true if encoder is currently in DTX.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn in_dtx(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_IN_DTX_REQUEST as i32)
    }

    /// Constrain VBR to reduce instant bitrate swings.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_vbr_constraint(&mut self, constrained: bool) -> Result<()> {
        self.simple_ctl(
            OPUS_SET_VBR_CONSTRAINT_REQUEST as i32,
            i32::from(constrained),
        )
    }
    /// Query VBR constraint setting.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn vbr_constraint(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_VBR_CONSTRAINT_REQUEST as i32)
    }

    /// Set maximum audio bandwidth the encoder may use.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_max_bandwidth(&mut self, bw: Bandwidth) -> Result<()> {
        self.simple_ctl(OPUS_SET_MAX_BANDWIDTH_REQUEST as i32, bw as i32)
    }
    /// Query max bandwidth.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn max_bandwidth(&mut self) -> Result<Bandwidth> {
        self.get_bandwidth_ctl(OPUS_GET_MAX_BANDWIDTH_REQUEST as i32)
    }

    /// Force a specific bandwidth (overrides automatic).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_bandwidth(&mut self, bw: Bandwidth) -> Result<()> {
        self.simple_ctl(OPUS_SET_BANDWIDTH_REQUEST as i32, bw as i32)
    }
    /// Query current forced bandwidth.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn bandwidth(&mut self) -> Result<Bandwidth> {
        self.get_bandwidth_ctl(OPUS_GET_BANDWIDTH_REQUEST as i32)
    }

    /// Force mono/stereo output, or None for automatic.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_force_channels(&mut self, channels: Option<Channels>) -> Result<()> {
        let val = match channels {
            Some(Channels::Mono) => 1,
            Some(Channels::Stereo) => 2,
            None => crate::bindings::OPUS_AUTO,
        };
        self.simple_ctl(OPUS_SET_FORCE_CHANNELS_REQUEST as i32, val)
    }
    /// Query forced channels.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn force_channels(&mut self) -> Result<Option<Channels>> {
        let v = self.get_int_ctl(OPUS_GET_FORCE_CHANNELS_REQUEST as i32)?;
        Ok(match v {
            1 => Some(Channels::Mono),
            2 => Some(Channels::Stereo),
            _ => None,
        })
    }

    /// Hint content type (voice or music).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_signal(&mut self, signal: Signal) -> Result<()> {
        self.simple_ctl(OPUS_SET_SIGNAL_REQUEST as i32, signal as i32)
    }
    /// Query current signal hint.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, [`Error::InternalError`] if the
    /// response is not recognized, or a mapped libopus error.
    pub fn signal(&mut self) -> Result<Signal> {
        let v = self.get_int_ctl(OPUS_GET_SIGNAL_REQUEST as i32)?;
        match v {
            x if x == OPUS_AUTO => Ok(Signal::Auto),
            x if x == crate::bindings::OPUS_SIGNAL_VOICE as i32 => Ok(Signal::Voice),
            x if x == crate::bindings::OPUS_SIGNAL_MUSIC as i32 => Ok(Signal::Music),
            _ => Err(Error::InternalError),
        }
    }

    /// Encoder algorithmic lookahead (in samples at 48 kHz domain).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn lookahead(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_LOOKAHEAD_REQUEST as i32)
    }
    /// Final RNG state from the last encode (debugging/bitstream id).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn final_range(&mut self) -> Result<u32> {
        let mut val: u32 = 0;
        let r = unsafe {
            opus_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_GET_FINAL_RANGE_REQUEST as i32,
                &mut val,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(val)
    }

    /// Set input LSB depth (typically 16-24 bits).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, [`Error::BadArg`] for an
    /// out-of-range bit depth, or a mapped libopus error.
    pub fn set_lsb_depth(&mut self, bits: i32) -> Result<()> {
        if !(8..=24).contains(&bits) {
            return Err(Error::BadArg);
        }
        self.simple_ctl(OPUS_SET_LSB_DEPTH_REQUEST as i32, bits)
    }
    /// Query input LSB depth.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn lsb_depth(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_LSB_DEPTH_REQUEST as i32)
    }

    /// Set expert frame duration choice.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_expert_frame_duration(&mut self, dur: ExpertFrameDuration) -> Result<()> {
        self.simple_ctl(OPUS_SET_EXPERT_FRAME_DURATION_REQUEST as i32, dur as i32)
    }
    /// Query expert frame duration.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, [`Error::InternalError`] if the
    /// response is not recognized, or a mapped libopus error.
    pub fn expert_frame_duration(&mut self) -> Result<ExpertFrameDuration> {
        let v = self.get_int_ctl(OPUS_GET_EXPERT_FRAME_DURATION_REQUEST as i32)?;
        let vu = u32::try_from(v).map_err(|_| Error::InternalError)?;
        match vu {
            x if x == crate::bindings::OPUS_FRAMESIZE_ARG => Ok(ExpertFrameDuration::Auto),
            x if x == crate::bindings::OPUS_FRAMESIZE_2_5_MS => Ok(ExpertFrameDuration::Ms2_5),
            x if x == crate::bindings::OPUS_FRAMESIZE_5_MS => Ok(ExpertFrameDuration::Ms5),
            x if x == crate::bindings::OPUS_FRAMESIZE_10_MS => Ok(ExpertFrameDuration::Ms10),
            x if x == crate::bindings::OPUS_FRAMESIZE_20_MS => Ok(ExpertFrameDuration::Ms20),
            x if x == crate::bindings::OPUS_FRAMESIZE_40_MS => Ok(ExpertFrameDuration::Ms40),
            x if x == crate::bindings::OPUS_FRAMESIZE_60_MS => Ok(ExpertFrameDuration::Ms60),
            x if x == crate::bindings::OPUS_FRAMESIZE_80_MS => Ok(ExpertFrameDuration::Ms80),
            x if x == crate::bindings::OPUS_FRAMESIZE_100_MS => Ok(ExpertFrameDuration::Ms100),
            x if x == crate::bindings::OPUS_FRAMESIZE_120_MS => Ok(ExpertFrameDuration::Ms120),
            _ => Err(Error::InternalError),
        }
    }

    /// Disable/enable inter-frame prediction (expert option).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_prediction_disabled(&mut self, disabled: bool) -> Result<()> {
        self.simple_ctl(
            OPUS_SET_PREDICTION_DISABLED_REQUEST as i32,
            i32::from(disabled),
        )
    }
    /// Query prediction disabled flag.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn prediction_disabled(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_PREDICTION_DISABLED_REQUEST as i32)
    }

    /// Disable/enable phase inversion (stereo decorrelation) in CELT (expert option).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_phase_inversion_disabled(&mut self, disabled: bool) -> Result<()> {
        self.simple_ctl(
            OPUS_SET_PHASE_INVERSION_DISABLED_REQUEST as i32,
            i32::from(disabled),
        )
    }
    /// Query phase inversion disabled flag.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn phase_inversion_disabled(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_PHASE_INVERSION_DISABLED_REQUEST as i32)
    }

    // --- internal helpers ---
    fn simple_ctl(&mut self, req: i32, val: i32) -> Result<()> {
        let r = unsafe { opus_encoder_ctl(self.raw.as_ptr(), req, val) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }
    fn get_bool_ctl(&mut self, req: i32) -> Result<bool> {
        Ok(self.get_int_ctl(req)? != 0)
    }
    fn get_int_ctl(&mut self, req: i32) -> Result<i32> {
        let mut v: i32 = 0;
        let r = unsafe { opus_encoder_ctl(self.raw.as_ptr(), req, &mut v) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(v)
    }
    fn get_bandwidth_ctl(&mut self, req: i32) -> Result<Bandwidth> {
        let v = self.get_int_ctl(req)?;
        let vu = u32::try_from(v).map_err(|_| Error::InternalError)?;
        match vu {
            x if x == crate::bindings::OPUS_BANDWIDTH_NARROWBAND => Ok(Bandwidth::Narrowband),
            x if x == crate::bindings::OPUS_BANDWIDTH_MEDIUMBAND => Ok(Bandwidth::Mediumband),
            x if x == crate::bindings::OPUS_BANDWIDTH_WIDEBAND => Ok(Bandwidth::Wideband),
            x if x == crate::bindings::OPUS_BANDWIDTH_SUPERWIDEBAND => Ok(Bandwidth::SuperWideband),
            x if x == OPUS_BANDWIDTH_FULLBAND => Ok(Bandwidth::Fullband),
            _ => Err(Error::InternalError),
        }
    }

    /// Set target bitrate.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_bitrate(&mut self, bitrate: Bitrate) -> Result<()> {
        let result = unsafe {
            opus_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_SET_BITRATE_REQUEST as i32,
                bitrate.value(),
            )
        };

        if result != 0 {
            return Err(Error::from_code(result));
        }

        Ok(())
    }

    /// Query current bitrate.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn bitrate(&mut self) -> Result<Bitrate> {
        let mut bitrate = 0i32;
        let result = unsafe {
            opus_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_GET_BITRATE_REQUEST as i32,
                &mut bitrate,
            )
        };

        if result != 0 {
            return Err(Error::from_code(result));
        }

        match bitrate {
            OPUS_AUTO => Ok(Bitrate::Auto),
            OPUS_BITRATE_MAX => Ok(Bitrate::Max),
            bps => Ok(Bitrate::Custom(bps)),
        }
    }

    /// Set encoder complexity [0..=10].
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_complexity(&mut self, complexity: Complexity) -> Result<()> {
        let result = unsafe {
            opus_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_SET_COMPLEXITY_REQUEST as i32,
                complexity.value() as i32,
            )
        };

        if result != 0 {
            return Err(Error::from_code(result));
        }

        Ok(())
    }

    /// Query encoder complexity.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn complexity(&mut self) -> Result<Complexity> {
        let mut complexity = 0i32;
        let result = unsafe {
            opus_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_GET_COMPLEXITY_REQUEST as i32,
                &mut complexity,
            )
        };

        if result != 0 {
            return Err(Error::from_code(result));
        }

        Ok(Complexity::new(
            u32::try_from(complexity).map_err(|_| Error::InternalError)?,
        ))
    }

    /// Enable or disable VBR.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn set_vbr(&mut self, enabled: bool) -> Result<()> {
        let vbr = i32::from(enabled);
        let result =
            unsafe { opus_encoder_ctl(self.raw.as_ptr(), OPUS_SET_VBR_REQUEST as i32, vbr) };

        if result != 0 {
            return Err(Error::from_code(result));
        }

        Ok(())
    }

    /// Query VBR status.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn vbr(&mut self) -> Result<bool> {
        let mut vbr = 0i32;
        let result =
            unsafe { opus_encoder_ctl(self.raw.as_ptr(), OPUS_GET_VBR_REQUEST as i32, &mut vbr) };

        if result != 0 {
            return Err(Error::from_code(result));
        }

        Ok(vbr != 0)
    }

    /// The encoder's configured sample rate.
    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// The encoder's channel configuration.
    #[must_use]
    pub const fn channels(&self) -> Channels {
        self.channels
    }

    /// Reset the encoder to its initial state (same config, cleared history).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder is invalid, or a mapped libopus error.
    pub fn reset(&mut self) -> Result<()> {
        let r = unsafe {
            opus_encoder_ctl(self.raw.as_ptr(), crate::bindings::OPUS_RESET_STATE as i32)
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }
}

impl<'a> EncoderRef<'a> {
    /// Wrap an externally-initialized encoder without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`Encoder::size()`] bytes
    /// - `ptr` must be aligned to at least `align_of::<usize>()` (malloc-style alignment)
    /// - `sample_rate` and `channels` must exactly match the encoder state already stored at `ptr`
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    ///
    /// Passing mismatched metadata is undefined behavior: later safe methods may validate buffer
    /// sizes against the wrong channel/rate and then call libopus with out-of-bounds buffers.
    ///
    /// Use [`Encoder::init_in_place`] to initialize the memory before calling this.
    #[must_use]
    pub unsafe fn from_raw(
        ptr: *mut OpusEncoder,
        sample_rate: SampleRate,
        channels: Channels,
    ) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        let encoder = Encoder::from_raw(
            unsafe { NonNull::new_unchecked(ptr) },
            sample_rate,
            channels,
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
        channels: Channels,
        application: Application,
    ) -> Result<Self> {
        let required = Encoder::size(channels)?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusEncoder>();
        unsafe { Encoder::init_in_place(ptr, sample_rate, channels, application)? };
        Ok(unsafe { Self::from_raw(ptr, sample_rate, channels) })
    }
}

impl Deref for EncoderRef<'_> {
    type Target = Encoder;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for EncoderRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
