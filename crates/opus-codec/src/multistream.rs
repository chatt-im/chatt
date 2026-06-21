//! Safe wrappers for the Opus Multistream API (surround and channel-mapped streams)

use crate::bindings::{
    OPUS_AUTO, OPUS_BANDWIDTH_FULLBAND, OPUS_BANDWIDTH_MEDIUMBAND, OPUS_BANDWIDTH_NARROWBAND,
    OPUS_BANDWIDTH_SUPERWIDEBAND, OPUS_BANDWIDTH_WIDEBAND, OPUS_BITRATE_MAX,
    OPUS_GET_BANDWIDTH_REQUEST, OPUS_GET_BITRATE_REQUEST, OPUS_GET_COMPLEXITY_REQUEST,
    OPUS_GET_DTX_REQUEST, OPUS_GET_FINAL_RANGE_REQUEST, OPUS_GET_FORCE_CHANNELS_REQUEST,
    OPUS_GET_GAIN_REQUEST, OPUS_GET_IN_DTX_REQUEST, OPUS_GET_INBAND_FEC_REQUEST,
    OPUS_GET_LAST_PACKET_DURATION_REQUEST, OPUS_GET_LOOKAHEAD_REQUEST,
    OPUS_GET_MAX_BANDWIDTH_REQUEST, OPUS_GET_PACKET_LOSS_PERC_REQUEST,
    OPUS_GET_PHASE_INVERSION_DISABLED_REQUEST, OPUS_GET_PITCH_REQUEST,
    OPUS_GET_SAMPLE_RATE_REQUEST, OPUS_GET_SIGNAL_REQUEST, OPUS_GET_VBR_CONSTRAINT_REQUEST,
    OPUS_GET_VBR_REQUEST, OPUS_MULTISTREAM_GET_DECODER_STATE_REQUEST,
    OPUS_MULTISTREAM_GET_ENCODER_STATE_REQUEST, OPUS_RESET_STATE, OPUS_SET_BANDWIDTH_REQUEST,
    OPUS_SET_BITRATE_REQUEST, OPUS_SET_COMPLEXITY_REQUEST, OPUS_SET_DTX_REQUEST,
    OPUS_SET_FORCE_CHANNELS_REQUEST, OPUS_SET_GAIN_REQUEST, OPUS_SET_INBAND_FEC_REQUEST,
    OPUS_SET_MAX_BANDWIDTH_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST,
    OPUS_SET_PHASE_INVERSION_DISABLED_REQUEST, OPUS_SET_SIGNAL_REQUEST,
    OPUS_SET_VBR_CONSTRAINT_REQUEST, OPUS_SET_VBR_REQUEST, OPUS_SIGNAL_MUSIC, OPUS_SIGNAL_VOICE,
    OpusDecoder, OpusEncoder, OpusMSDecoder, OpusMSEncoder, opus_multistream_decode,
    opus_multistream_decode_float, opus_multistream_decoder_create, opus_multistream_decoder_ctl,
    opus_multistream_decoder_destroy, opus_multistream_decoder_get_size,
    opus_multistream_decoder_init, opus_multistream_encode, opus_multistream_encode_float,
    opus_multistream_encoder_create, opus_multistream_encoder_ctl,
    opus_multistream_encoder_destroy, opus_multistream_encoder_get_size,
    opus_multistream_encoder_init, opus_multistream_surround_encoder_create,
    opus_multistream_surround_encoder_get_size, opus_multistream_surround_encoder_init,
};
use crate::constants::{is_frame_size_2_5ms_aligned, max_frame_samples_for};
use crate::error::{Error, Result};
use crate::types::{Application, Bandwidth, Bitrate, Channels, Complexity, SampleRate, Signal};
use crate::{AlignedBuffer, Ownership, RawHandle};
use std::marker::PhantomData;
use std::num::{NonZeroU8, NonZeroUsize};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Describes the multistream mapping configuration.
#[derive(Debug, Clone, Copy)]
pub struct Mapping<'a> {
    /// Total input/output channels.
    pub channels: u8,
    /// Total number of streams (coupled + uncoupled).
    pub streams: u8,
    /// Number of coupled stereo streams (each counts as 2 channels).
    pub coupled_streams: u8,
    /// Channel-to-stream mapping table (length == channels).
    pub mapping: &'a [u8],
}

impl Mapping<'_> {
    fn validate_common(&self) -> Result<(usize, usize, usize)> {
        let channel_count = NonZeroU8::new(self.channels).ok_or(Error::BadArg)?;
        let channel_count = usize::from(channel_count.get());
        if self.mapping.len() != channel_count {
            return Err(Error::BadArg);
        }

        let streams = NonZeroU8::new(self.streams).ok_or(Error::BadArg)?;
        let streams = usize::from(streams.get());
        let coupled = usize::from(self.coupled_streams);
        if coupled > streams {
            return Err(Error::BadArg);
        }
        if streams + coupled > u8::MAX as usize {
            return Err(Error::BadArg);
        }
        let total_streams = streams + coupled;
        for &entry in self.mapping {
            if entry == u8::MAX {
                continue;
            }
            if usize::from(entry) >= total_streams {
                return Err(Error::BadArg);
            }
        }
        Ok((channel_count, streams, coupled))
    }

    /// Validate mapping for use with libopus multistream decoder.
    fn validate_for_decoder(&self) -> Result<()> {
        self.validate_common()?;
        Ok(())
    }

    /// Validate mapping for use with libopus multistream encoder.
    fn validate_for_encoder(&self) -> Result<()> {
        let (channel_count, streams, coupled) = self.validate_common()?;
        if streams + coupled > channel_count {
            return Err(Error::BadArg);
        }

        let mut has_left = vec![false; coupled];
        let mut has_right = vec![false; coupled];
        let mut has_mono = vec![false; streams.saturating_sub(coupled)];
        for &entry in self.mapping {
            if entry == u8::MAX {
                continue;
            }
            let idx = usize::from(entry);
            if idx < 2 * coupled {
                let stream = idx / 2;
                if idx % 2 == 0 {
                    has_left[stream] = true;
                } else {
                    has_right[stream] = true;
                }
            } else {
                let stream = idx - coupled;
                if stream >= coupled && stream < streams {
                    has_mono[stream - coupled] = true;
                }
            }
        }

        if has_left.iter().any(|has| !has) || has_right.iter().any(|has| !has) {
            return Err(Error::BadArg);
        }
        if has_mono.iter().any(|has| !has) {
            return Err(Error::BadArg);
        }
        Ok(())
    }
}

/// Safe wrapper around `OpusMSEncoder`.
pub struct MultistreamEncoder {
    raw: RawHandle<OpusMSEncoder>,
    sample_rate: SampleRate,
    channels: u8,
    streams: u8,
    coupled_streams: u8,
}

unsafe impl Send for MultistreamEncoder {}
unsafe impl Sync for MultistreamEncoder {}

/// Borrowed wrapper around a multistream encoder.
pub struct MultistreamEncoderRef<'a> {
    inner: MultistreamEncoder,
    _marker: PhantomData<&'a mut OpusMSEncoder>,
}

unsafe impl Send for MultistreamEncoderRef<'_> {}
unsafe impl Sync for MultistreamEncoderRef<'_> {}

impl MultistreamEncoder {
    fn from_raw(
        ptr: NonNull<OpusMSEncoder>,
        sample_rate: SampleRate,
        channels: u8,
        streams: u8,
        coupled_streams: u8,
        ownership: Ownership,
    ) -> Self {
        Self {
            raw: RawHandle::new(ptr, ownership, opus_multistream_encoder_destroy),
            sample_rate,
            channels,
            streams,
            coupled_streams,
        }
    }

    /// Size in bytes of a multistream encoder state for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the stream counts are invalid or libopus reports
    /// an impossible size.
    pub fn size(streams: u8, coupled_streams: u8) -> Result<usize> {
        let raw = unsafe {
            opus_multistream_encoder_get_size(i32::from(streams), i32::from(coupled_streams))
        };
        if raw <= 0 {
            return Err(Error::BadArg);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Size in bytes of a surround multistream encoder state for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the channel/mapping configuration is invalid.
    pub fn surround_size(channels: u8, mapping_family: i32) -> Result<usize> {
        let raw = unsafe {
            opus_multistream_surround_encoder_get_size(i32::from(channels), mapping_family)
        };
        if raw <= 0 {
            return Err(Error::BadArg);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Initialize a previously allocated multistream encoder state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `MultistreamEncoder::size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the mapping is invalid or `ptr` is null, or a
    /// mapped libopus error on failure.
    pub unsafe fn init_in_place(
        ptr: *mut OpusMSEncoder,
        sr: SampleRate,
        app: Application,
        mapping: Mapping<'_>,
    ) -> Result<()> {
        if ptr.is_null() {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        mapping.validate_for_encoder()?;
        let r = unsafe {
            opus_multistream_encoder_init(
                ptr,
                sr as i32,
                i32::from(mapping.channels),
                i32::from(mapping.streams),
                i32::from(mapping.coupled_streams),
                mapping.mapping.as_ptr(),
                app as i32,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Initialize a previously allocated surround multistream encoder state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `MultistreamEncoder::surround_size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] for invalid channel counts or a mapped libopus error.
    pub unsafe fn init_surround_in_place(
        ptr: *mut OpusMSEncoder,
        sr: SampleRate,
        channels: u8,
        mapping_family: i32,
        app: Application,
    ) -> Result<(u8, u8, Vec<u8>)> {
        if ptr.is_null() || channels == 0 {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        let mut streams = 0i32;
        let mut coupled = 0i32;
        let mut mapping = vec![0u8; channels as usize];
        let r = unsafe {
            opus_multistream_surround_encoder_init(
                ptr,
                sr as i32,
                i32::from(channels),
                mapping_family,
                std::ptr::addr_of_mut!(streams),
                std::ptr::addr_of_mut!(coupled),
                mapping.as_mut_ptr(),
                app as i32,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok((
            u8::try_from(streams).map_err(|_| Error::BadArg)?,
            u8::try_from(coupled).map_err(|_| Error::BadArg)?,
            mapping,
        ))
    }

    /// Create a new multistream encoder.
    ///
    /// The `mapping.mapping` array describes how input channels are assigned to streams.
    /// See libopus docs for standard surround layouts.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] when the mapping dimensions are inconsistent, or
    /// propagates allocation/configuration failures from libopus.
    pub fn new(sr: SampleRate, app: Application, mapping: Mapping<'_>) -> Result<Self> {
        mapping.validate_for_encoder()?;
        let mut err = 0i32;
        let enc = unsafe {
            opus_multistream_encoder_create(
                sr as i32,
                i32::from(mapping.channels),
                i32::from(mapping.streams),
                i32::from(mapping.coupled_streams),
                mapping.mapping.as_ptr(),
                app as i32,
                std::ptr::addr_of_mut!(err),
            )
        };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let enc = NonNull::new(enc).ok_or(Error::AllocFail)?;
        Ok(Self::from_raw(
            enc,
            sr,
            mapping.channels,
            mapping.streams,
            mapping.coupled_streams,
            Ownership::Owned,
        ))
    }

    /// Encode interleaved i16 PCM into a multistream Opus packet.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid, [`Error::BadArg`]
    /// for buffer mismatches, or the mapped libopus error code.
    #[allow(clippy::missing_panics_doc)]
    pub fn encode(
        &mut self,
        pcm: &[i16],
        frame_size_per_ch: usize,
        out: &mut [u8],
    ) -> Result<usize> {
        let frame_size_per_ch = NonZeroUsize::new(frame_size_per_ch).ok_or(Error::BadArg)?;
        if frame_size_per_ch.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }
        if pcm.len() != frame_size_per_ch.get() * self.channels as usize {
            return Err(Error::BadArg);
        }
        if out.is_empty() || out.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        let n = unsafe {
            opus_multistream_encode(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                i32::try_from(frame_size_per_ch.get()).map_err(|_| Error::BadArg)?,
                out.as_mut_ptr(),
                i32::try_from(out.len()).map_err(|_| Error::BadArg)?,
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Encode interleaved f32 PCM into a multistream Opus packet.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid, [`Error::BadArg`]
    /// for buffer mismatches, or the mapped libopus error code.
    pub fn encode_float(
        &mut self,
        pcm: &[f32],
        frame_size_per_ch: usize,
        out: &mut [u8],
    ) -> Result<usize> {
        let frame_size_per_ch = NonZeroUsize::new(frame_size_per_ch).ok_or(Error::BadArg)?;
        if frame_size_per_ch.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }
        if pcm.len() != frame_size_per_ch.get() * self.channels as usize {
            return Err(Error::BadArg);
        }
        if out.is_empty() || out.len() > i32::MAX as usize {
            return Err(Error::BadArg);
        }
        let n = unsafe {
            opus_multistream_encode_float(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                i32::try_from(frame_size_per_ch.get()).map_err(|_| Error::BadArg)?,
                out.as_mut_ptr(),
                i32::try_from(out.len()).map_err(|_| Error::BadArg)?,
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Final RNG state from the last encode.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] when the encoder handle is null or
    /// propagates the libopus error.
    pub fn final_range(&mut self) -> Result<u32> {
        let mut v: u32 = 0;
        let r = unsafe {
            opus_multistream_encoder_ctl(
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

    /// Set target bitrate for the encoder.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_bitrate(&mut self, bitrate: Bitrate) -> Result<()> {
        self.simple_ctl(OPUS_SET_BITRATE_REQUEST as i32, bitrate.value())
    }

    /// Query the current bitrate target.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null, [`Error::InternalError`]
    /// if the returned value cannot be represented, or propagates any error reported by
    /// libopus.
    pub fn bitrate(&mut self) -> Result<Bitrate> {
        let v = self.get_int_ctl(OPUS_GET_BITRATE_REQUEST as i32)?;
        Ok(match v {
            x if x == OPUS_AUTO => Bitrate::Auto,
            x if x == OPUS_BITRATE_MAX => Bitrate::Max,
            other => Bitrate::Custom(other),
        })
    }

    /// Set encoder complexity in the range 0..=10.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_complexity(&mut self, complexity: Complexity) -> Result<()> {
        self.simple_ctl(
            OPUS_SET_COMPLEXITY_REQUEST as i32,
            complexity.value() as i32,
        )
    }

    /// Query encoder complexity.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null, [`Error::InternalError`]
    /// if the response is outside the valid range, or propagates any error reported by libopus.
    pub fn complexity(&mut self) -> Result<Complexity> {
        let v = self.get_int_ctl(OPUS_GET_COMPLEXITY_REQUEST as i32)?;
        Ok(Complexity::new(
            u32::try_from(v).map_err(|_| Error::InternalError)?,
        ))
    }

    /// Enable/disable discontinuous transmission (DTX).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_dtx(&mut self, enabled: bool) -> Result<()> {
        self.simple_ctl(OPUS_SET_DTX_REQUEST as i32, i32::from(enabled))
    }

    /// Query whether DTX is enabled.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn dtx(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_DTX_REQUEST as i32)
    }

    /// Query whether the encoder is currently in DTX.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn in_dtx(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_IN_DTX_REQUEST as i32)
    }

    /// Enable/disable in-band FEC generation.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_inband_fec(&mut self, enabled: bool) -> Result<()> {
        self.simple_ctl(OPUS_SET_INBAND_FEC_REQUEST as i32, i32::from(enabled))
    }

    /// Query whether in-band FEC is enabled.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn inband_fec(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_INBAND_FEC_REQUEST as i32)
    }

    /// Set expected packet loss percentage (0..=100).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] when `perc` is outside `0..=100`, [`Error::InvalidState`] if
    /// the encoder handle is null, or propagates any error reported by libopus.
    pub fn set_packet_loss_perc(&mut self, perc: i32) -> Result<()> {
        if !(0..=100).contains(&perc) {
            return Err(Error::BadArg);
        }
        self.simple_ctl(OPUS_SET_PACKET_LOSS_PERC_REQUEST as i32, perc)
    }

    /// Query expected packet loss percentage.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn packet_loss_perc(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_PACKET_LOSS_PERC_REQUEST as i32)
    }

    /// Enable/disable variable bitrate.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_vbr(&mut self, enabled: bool) -> Result<()> {
        self.simple_ctl(OPUS_SET_VBR_REQUEST as i32, i32::from(enabled))
    }

    /// Query VBR status.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn vbr(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_VBR_REQUEST as i32)
    }

    /// Constrain VBR to reduce instantaneous bitrate swings.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_vbr_constraint(&mut self, constrained: bool) -> Result<()> {
        self.simple_ctl(
            OPUS_SET_VBR_CONSTRAINT_REQUEST as i32,
            i32::from(constrained),
        )
    }

    /// Query VBR constraint flag.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn vbr_constraint(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_VBR_CONSTRAINT_REQUEST as i32)
    }

    /// Set the maximum bandwidth the encoder may use.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_max_bandwidth(&mut self, bw: Bandwidth) -> Result<()> {
        self.simple_ctl(OPUS_SET_MAX_BANDWIDTH_REQUEST as i32, bw as i32)
    }

    /// Query the configured maximum bandwidth.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null, [`Error::InternalError`]
    /// if the value cannot be represented, or propagates any error reported by libopus.
    pub fn max_bandwidth(&mut self) -> Result<Bandwidth> {
        self.get_bandwidth_ctl(OPUS_GET_MAX_BANDWIDTH_REQUEST as i32)
    }

    /// Force a specific output bandwidth (overrides automatic selection).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_bandwidth(&mut self, bw: Bandwidth) -> Result<()> {
        self.simple_ctl(OPUS_SET_BANDWIDTH_REQUEST as i32, bw as i32)
    }

    /// Query the current forced bandwidth, if any.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or [`Error::InternalError`]
    /// if the value is outside the known set, and propagates any error reported by libopus.
    pub fn bandwidth(&mut self) -> Result<Bandwidth> {
        self.get_bandwidth_ctl(OPUS_GET_BANDWIDTH_REQUEST as i32)
    }

    /// Force mono/stereo output for coupled streams, or `None` for automatic.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_force_channels(&mut self, channels: Option<Channels>) -> Result<()> {
        let value = match channels {
            Some(Channels::Mono) => 1,
            Some(Channels::Stereo) => 2,
            None => OPUS_AUTO,
        };
        self.simple_ctl(OPUS_SET_FORCE_CHANNELS_REQUEST as i32, value)
    }

    /// Query forced channel configuration (if any).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn force_channels(&mut self) -> Result<Option<Channels>> {
        let v = self.get_int_ctl(OPUS_GET_FORCE_CHANNELS_REQUEST as i32)?;
        Ok(match v {
            1 => Some(Channels::Mono),
            2 => Some(Channels::Stereo),
            x if x == OPUS_AUTO => None,
            _ => None,
        })
    }

    /// Hint the type of content being encoded (voice/music).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_signal(&mut self, signal: Signal) -> Result<()> {
        self.simple_ctl(OPUS_SET_SIGNAL_REQUEST as i32, signal as i32)
    }

    /// Query the current signal hint.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null, [`Error::InternalError`]
    /// if the response is not recognized, or propagates any error reported by libopus.
    pub fn signal(&mut self) -> Result<Signal> {
        let v = self.get_int_ctl(OPUS_GET_SIGNAL_REQUEST as i32)?;
        match v {
            x if x == OPUS_AUTO => Ok(Signal::Auto),
            x if x == OPUS_SIGNAL_VOICE as i32 => Ok(Signal::Voice),
            x if x == OPUS_SIGNAL_MUSIC as i32 => Ok(Signal::Music),
            _ => Err(Error::InternalError),
        }
    }

    /// Query the algorithmic lookahead in samples at 48 kHz.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn lookahead(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_LOOKAHEAD_REQUEST as i32)
    }

    /// Reset the encoder state (retaining configuration).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is null or propagates any error
    /// reported by libopus.
    pub fn reset(&mut self) -> Result<()> {
        let r = unsafe { opus_multistream_encoder_ctl(self.raw.as_ptr(), OPUS_RESET_STATE as i32) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Channels of this encoder (interleaved input).
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }
    /// Input sampling rate.
    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }
    /// Number of mono streams.
    #[must_use]
    pub const fn streams(&self) -> u8 {
        self.streams
    }
    /// Number of coupled streams.
    #[must_use]
    pub const fn coupled_streams(&self) -> u8 {
        self.coupled_streams
    }

    /// Create a multistream encoder using libopus surround mapping helpers.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] for invalid channel counts or the mapped libopus
    /// error when surround initialisation fails.
    pub fn new_surround(
        sr: SampleRate,
        channels: u8,
        mapping_family: i32,
        app: Application,
    ) -> Result<(Self, Vec<u8>)> {
        if channels == 0 {
            return Err(Error::BadArg);
        }
        let mut err = 0i32;
        let mut streams = 0i32;
        let mut coupled = 0i32;
        let mut mapping = vec![0u8; channels as usize];
        let enc = unsafe {
            opus_multistream_surround_encoder_create(
                sr as i32,
                i32::from(channels),
                mapping_family,
                std::ptr::addr_of_mut!(streams),
                std::ptr::addr_of_mut!(coupled),
                mapping.as_mut_ptr(),
                app as i32,
                std::ptr::addr_of_mut!(err),
            )
        };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let enc = NonNull::new(enc).ok_or(Error::AllocFail)?;
        let streams_u8 = u8::try_from(streams).map_err(|_| Error::BadArg)?;
        let coupled_u8 = u8::try_from(coupled).map_err(|_| Error::BadArg)?;
        Ok((
            Self::from_raw(enc, sr, channels, streams_u8, coupled_u8, Ownership::Owned),
            mapping,
        ))
    }

    /// Borrow a pointer to an individual underlying encoder state for CTLs.
    ///
    /// # Safety
    /// Caller must not outlive the multistream encoder and must ensure the
    /// returned pointer is only used for immediate FFI calls.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the encoder handle is invalid or propagates the
    /// libopus error if retrieving the state fails.
    pub unsafe fn encoder_state_ptr(&mut self, stream_index: i32) -> Result<*mut OpusEncoder> {
        let mut state: *mut OpusEncoder = std::ptr::null_mut();
        let r = unsafe {
            opus_multistream_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_MULTISTREAM_GET_ENCODER_STATE_REQUEST as i32,
                stream_index,
                &mut state,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        if state.is_null() {
            return Err(Error::InternalError);
        }
        Ok(state)
    }

    fn simple_ctl(&mut self, req: i32, val: i32) -> Result<()> {
        let r = unsafe { opus_multistream_encoder_ctl(self.raw.as_ptr(), req, val) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    fn get_int_ctl(&mut self, req: i32) -> Result<i32> {
        let mut v: i32 = 0;
        let r = unsafe { opus_multistream_encoder_ctl(self.raw.as_ptr(), req, &mut v) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(v)
    }

    fn get_bool_ctl(&mut self, req: i32) -> Result<bool> {
        Ok(self.get_int_ctl(req)? != 0)
    }

    fn get_bandwidth_ctl(&mut self, req: i32) -> Result<Bandwidth> {
        let v = u32::try_from(self.get_int_ctl(req)?).map_err(|_| Error::InternalError)?;
        match v {
            x if x == OPUS_BANDWIDTH_NARROWBAND => Ok(Bandwidth::Narrowband),
            x if x == OPUS_BANDWIDTH_MEDIUMBAND => Ok(Bandwidth::Mediumband),
            x if x == OPUS_BANDWIDTH_WIDEBAND => Ok(Bandwidth::Wideband),
            x if x == OPUS_BANDWIDTH_SUPERWIDEBAND => Ok(Bandwidth::SuperWideband),
            x if x == OPUS_BANDWIDTH_FULLBAND => Ok(Bandwidth::Fullband),
            _ => Err(Error::InternalError),
        }
    }
}

impl<'a> MultistreamEncoderRef<'a> {
    /// Wrap an externally-initialized multistream encoder without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`MultistreamEncoder::size()`] bytes
    /// - `ptr` must be aligned to at least `align_of::<usize>()` (malloc-style alignment)
    /// - `sr` and `mapping` must exactly match the encoder state already stored at `ptr`
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    ///
    /// Passing mismatched metadata is undefined behavior: later safe methods may validate buffer
    /// sizes with the wrong layout and then call libopus with out-of-bounds buffers.
    ///
    /// Use [`MultistreamEncoder::init_in_place`] to initialize the memory before calling this.
    #[must_use]
    pub unsafe fn from_raw(ptr: *mut OpusMSEncoder, sr: SampleRate, mapping: Mapping<'_>) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        debug_assert!(mapping.validate_for_encoder().is_ok());
        let encoder = MultistreamEncoder::from_raw(
            unsafe { NonNull::new_unchecked(ptr) },
            sr,
            mapping.channels,
            mapping.streams,
            mapping.coupled_streams,
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
        sr: SampleRate,
        app: Application,
        mapping: Mapping<'_>,
    ) -> Result<Self> {
        let required = MultistreamEncoder::size(mapping.streams, mapping.coupled_streams)?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusMSEncoder>();
        unsafe { MultistreamEncoder::init_in_place(ptr, sr, app, mapping)? };
        Ok(unsafe { Self::from_raw(ptr, sr, mapping) })
    }

    /// Initialize a surround encoder in an externally allocated buffer.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the buffer is too small, or a mapped libopus error.
    pub fn init_in_surround(
        buf: &'a mut AlignedBuffer,
        sr: SampleRate,
        channels: u8,
        mapping_family: i32,
        app: Application,
    ) -> Result<(Self, Vec<u8>)> {
        let required = MultistreamEncoder::surround_size(channels, mapping_family)?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusMSEncoder>();
        let (streams, coupled, mapping) = unsafe {
            MultistreamEncoder::init_surround_in_place(ptr, sr, channels, mapping_family, app)?
        };
        let mapping_ref = Mapping {
            channels,
            streams,
            coupled_streams: coupled,
            mapping: &mapping,
        };
        let encoder = unsafe { Self::from_raw(ptr, sr, mapping_ref) };
        Ok((encoder, mapping))
    }
}

impl Deref for MultistreamEncoderRef<'_> {
    type Target = MultistreamEncoder;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for MultistreamEncoderRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Safe wrapper around `OpusMSDecoder`.
pub struct MultistreamDecoder {
    raw: RawHandle<OpusMSDecoder>,
    sample_rate: SampleRate,
    channels: u8,
}

unsafe impl Send for MultistreamDecoder {}
unsafe impl Sync for MultistreamDecoder {}

/// Borrowed wrapper around a multistream decoder.
pub struct MultistreamDecoderRef<'a> {
    inner: MultistreamDecoder,
    _marker: PhantomData<&'a mut OpusMSDecoder>,
}

unsafe impl Send for MultistreamDecoderRef<'_> {}
unsafe impl Sync for MultistreamDecoderRef<'_> {}

impl MultistreamDecoder {
    fn from_raw(
        ptr: NonNull<OpusMSDecoder>,
        sample_rate: SampleRate,
        channels: u8,
        ownership: Ownership,
    ) -> Self {
        Self {
            raw: RawHandle::new(ptr, ownership, opus_multistream_decoder_destroy),
            sample_rate,
            channels,
        }
    }

    /// Size in bytes of a multistream decoder state for external allocation.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the stream counts are invalid or libopus reports
    /// an impossible size.
    pub fn size(streams: u8, coupled_streams: u8) -> Result<usize> {
        let raw = unsafe {
            opus_multistream_decoder_get_size(i32::from(streams), i32::from(coupled_streams))
        };
        if raw <= 0 {
            return Err(Error::BadArg);
        }
        usize::try_from(raw).map_err(|_| Error::InternalError)
    }

    /// Initialize a previously allocated multistream decoder state.
    ///
    /// # Safety
    /// The caller must provide a valid pointer to `MultistreamDecoder::size()` bytes,
    /// aligned to at least `align_of::<usize>()` (malloc-style alignment).
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] if the mapping is invalid or `ptr` is null, or a
    /// mapped libopus error on failure.
    pub unsafe fn init_in_place(
        ptr: *mut OpusMSDecoder,
        sr: SampleRate,
        mapping: Mapping<'_>,
    ) -> Result<()> {
        if ptr.is_null() {
            return Err(Error::BadArg);
        }
        if !crate::opus_ptr_is_aligned(ptr.cast()) {
            return Err(Error::BadArg);
        }
        mapping.validate_for_decoder()?;
        let r = unsafe {
            opus_multistream_decoder_init(
                ptr,
                sr as i32,
                i32::from(mapping.channels),
                i32::from(mapping.streams),
                i32::from(mapping.coupled_streams),
                mapping.mapping.as_ptr(),
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Create a new multistream decoder.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] when the mapping dimensions are inconsistent, or
    /// propagates allocation/configuration failures from libopus.
    pub fn new(sr: SampleRate, mapping: Mapping<'_>) -> Result<Self> {
        mapping.validate_for_decoder()?;
        let mut err = 0i32;
        let dec = unsafe {
            opus_multistream_decoder_create(
                sr as i32,
                i32::from(mapping.channels),
                i32::from(mapping.streams),
                i32::from(mapping.coupled_streams),
                mapping.mapping.as_ptr(),
                std::ptr::addr_of_mut!(err),
            )
        };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let dec = NonNull::new(dec).ok_or(Error::AllocFail)?;
        Ok(Self::from_raw(dec, sr, mapping.channels, Ownership::Owned))
    }

    /// Decode into interleaved i16 PCM (`frame_size` is per-channel).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is invalid, [`Error::BadArg`]
    /// for buffer mismatches, or the mapped libopus error code.
    pub fn decode(
        &mut self,
        packet: &[u8],
        out: &mut [i16],
        frame_size_per_ch: usize,
        fec: bool,
    ) -> Result<usize> {
        let frame_size_per_ch = NonZeroUsize::new(frame_size_per_ch).ok_or(Error::BadArg)?;
        if frame_size_per_ch.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }
        if out.len() != frame_size_per_ch.get() * self.channels as usize {
            return Err(Error::BadArg);
        }
        // libopus requires PLC/FEC frame sizes to be multiples of 2.5 ms.
        if (packet.is_empty() || fec)
            && !is_frame_size_2_5ms_aligned(frame_size_per_ch.get(), self.sample_rate)
        {
            return Err(Error::BadArg);
        }
        let n = unsafe {
            opus_multistream_decode(
                self.raw.as_ptr(),
                if packet.is_empty() {
                    std::ptr::null()
                } else {
                    packet.as_ptr()
                },
                if packet.is_empty() {
                    0
                } else {
                    i32::try_from(packet.len()).map_err(|_| Error::BadArg)?
                },
                out.as_mut_ptr(),
                i32::try_from(frame_size_per_ch.get()).map_err(|_| Error::BadArg)?,
                i32::from(fec),
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Decode into interleaved f32 PCM (`frame_size` is per-channel).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is invalid, [`Error::BadArg`]
    /// for buffer mismatches, or the mapped libopus error code.
    pub fn decode_float(
        &mut self,
        packet: &[u8],
        out: &mut [f32],
        frame_size_per_ch: usize,
        fec: bool,
    ) -> Result<usize> {
        let frame_size_per_ch = NonZeroUsize::new(frame_size_per_ch).ok_or(Error::BadArg)?;
        if frame_size_per_ch.get() > max_frame_samples_for(self.sample_rate) {
            return Err(Error::BadArg);
        }
        if out.len() != frame_size_per_ch.get() * self.channels as usize {
            return Err(Error::BadArg);
        }
        // libopus requires PLC/FEC frame sizes to be multiples of 2.5 ms.
        if (packet.is_empty() || fec)
            && !is_frame_size_2_5ms_aligned(frame_size_per_ch.get(), self.sample_rate)
        {
            return Err(Error::BadArg);
        }
        let n = unsafe {
            opus_multistream_decode_float(
                self.raw.as_ptr(),
                if packet.is_empty() {
                    std::ptr::null()
                } else {
                    packet.as_ptr()
                },
                if packet.is_empty() {
                    0
                } else {
                    i32::try_from(packet.len()).map_err(|_| Error::BadArg)?
                },
                out.as_mut_ptr(),
                i32::try_from(frame_size_per_ch.get()).map_err(|_| Error::BadArg)?,
                i32::from(fec),
            )
        };
        if n < 0 {
            return Err(Error::from_code(n));
        }
        usize::try_from(n).map_err(|_| Error::InternalError)
    }

    /// Final RNG state from the last decode.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] when the decoder handle is null or
    /// propagates the libopus error.
    pub fn final_range(&mut self) -> Result<u32> {
        let mut v: u32 = 0;
        let r = unsafe {
            opus_multistream_decoder_ctl(
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

    /// Reset the decoder to its initial state.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn reset(&mut self) -> Result<()> {
        let r = unsafe { opus_multistream_decoder_ctl(self.raw.as_ptr(), OPUS_RESET_STATE as i32) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    /// Set post-decode gain in Q8 dB units.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_gain(&mut self, q8_db: i32) -> Result<()> {
        self.simple_ctl(OPUS_SET_GAIN_REQUEST as i32, q8_db)
    }

    /// Query post-decode gain in Q8 dB units.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn gain(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_GAIN_REQUEST as i32)
    }

    /// Disable or enable phase inversion (CELT stereo decorrelation).
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn set_phase_inversion_disabled(&mut self, disabled: bool) -> Result<()> {
        self.simple_ctl(
            OPUS_SET_PHASE_INVERSION_DISABLED_REQUEST as i32,
            i32::from(disabled),
        )
    }

    /// Query the phase inversion disabled flag.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn phase_inversion_disabled(&mut self) -> Result<bool> {
        self.get_bool_ctl(OPUS_GET_PHASE_INVERSION_DISABLED_REQUEST as i32)
    }

    /// Query decoder output sample rate.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn get_sample_rate(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_SAMPLE_RATE_REQUEST as i32)
    }

    /// Query the pitch (fundamental period) of the last decoded frame.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn get_pitch(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_PITCH_REQUEST as i32)
    }

    /// Query the duration (per channel) of the last decoded packet.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is null or propagates any error
    /// reported by libopus.
    pub fn get_last_packet_duration(&mut self) -> Result<i32> {
        self.get_int_ctl(OPUS_GET_LAST_PACKET_DURATION_REQUEST as i32)
    }

    /// Output channels (interleaved).
    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }
    /// Output sample rate.
    #[must_use]
    pub const fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Create a multistream decoder using libopus surround mapping helpers.
    ///
    /// # Errors
    /// Returns [`Error::BadArg`] for invalid channel counts or the mapped libopus
    /// error when decoder initialisation fails.
    pub fn new_surround(
        sr: SampleRate,
        channels: u8,
        mapping_family: i32,
    ) -> Result<(Self, Vec<u8>, u8, u8)> {
        if channels == 0 {
            return Err(Error::BadArg);
        }
        let mut err = 0i32;
        let mut streams = 0i32;
        let mut coupled = 0i32;
        let mut mapping = vec![0u8; channels as usize];
        // libopus exposes surround helper creation only for encoders; callers
        // should use the returned mapping/stream counts to configure this decoder.
        let enc = unsafe {
            opus_multistream_surround_encoder_create(
                sr as i32,
                i32::from(channels),
                mapping_family,
                std::ptr::addr_of_mut!(streams),
                std::ptr::addr_of_mut!(coupled),
                mapping.as_mut_ptr(),
                Application::Audio as i32,
                std::ptr::addr_of_mut!(err),
            )
        };
        if !enc.is_null() {
            unsafe { opus_multistream_encoder_destroy(enc) };
        }
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let dec = unsafe {
            opus_multistream_decoder_create(
                sr as i32,
                i32::from(channels),
                streams,
                coupled,
                mapping.as_ptr(),
                std::ptr::addr_of_mut!(err),
            )
        };
        if err != 0 {
            return Err(Error::from_code(err));
        }
        let dec = NonNull::new(dec).ok_or(Error::AllocFail)?;
        Ok((
            Self::from_raw(dec, sr, channels, Ownership::Owned),
            mapping,
            u8::try_from(streams).map_err(|_| Error::BadArg)?,
            u8::try_from(coupled).map_err(|_| Error::BadArg)?,
        ))
    }

    /// Borrow a pointer to an individual underlying decoder state for CTLs.
    ///
    /// # Safety
    /// Caller must not outlive the multistream decoder and must ensure the
    /// returned pointer is only used for immediate FFI calls.
    ///
    /// # Errors
    /// Returns [`Error::InvalidState`] if the decoder handle is invalid or propagates the
    /// libopus error when retrieving the per-stream state fails.
    pub unsafe fn decoder_state_ptr(&mut self, stream_index: i32) -> Result<*mut OpusDecoder> {
        let mut state: *mut OpusDecoder = std::ptr::null_mut();
        let r = unsafe {
            opus_multistream_decoder_ctl(
                self.raw.as_ptr(),
                OPUS_MULTISTREAM_GET_DECODER_STATE_REQUEST as i32,
                stream_index,
                &mut state,
            )
        };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        if state.is_null() {
            return Err(Error::InternalError);
        }
        Ok(state)
    }

    fn simple_ctl(&mut self, req: i32, val: i32) -> Result<()> {
        let r = unsafe { opus_multistream_decoder_ctl(self.raw.as_ptr(), req, val) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(())
    }

    fn get_int_ctl(&mut self, req: i32) -> Result<i32> {
        let mut v: i32 = 0;
        let r = unsafe { opus_multistream_decoder_ctl(self.raw.as_ptr(), req, &mut v) };
        if r != 0 {
            return Err(Error::from_code(r));
        }
        Ok(v)
    }

    fn get_bool_ctl(&mut self, req: i32) -> Result<bool> {
        Ok(self.get_int_ctl(req)? != 0)
    }
}

impl<'a> MultistreamDecoderRef<'a> {
    /// Wrap an externally-initialized multistream decoder without taking ownership.
    ///
    /// # Safety
    /// - `ptr` must point to valid, initialized memory of at least [`MultistreamDecoder::size()`] bytes
    /// - `ptr` must be aligned to at least `align_of::<usize>()` (malloc-style alignment)
    /// - `sr` and `mapping` must exactly match the decoder state already stored at `ptr`
    /// - The memory must remain valid for the lifetime `'a`
    /// - Caller is responsible for freeing the memory after this wrapper is dropped
    ///
    /// Passing mismatched metadata is undefined behavior: later safe methods may validate buffer
    /// sizes with the wrong layout and then call libopus with out-of-bounds buffers.
    ///
    /// Use [`MultistreamDecoder::init_in_place`] to initialize the memory before calling this.
    #[must_use]
    pub unsafe fn from_raw(ptr: *mut OpusMSDecoder, sr: SampleRate, mapping: Mapping<'_>) -> Self {
        debug_assert!(!ptr.is_null(), "from_raw called with null ptr");
        debug_assert!(crate::opus_ptr_is_aligned(ptr.cast()));
        debug_assert!(mapping.validate_for_decoder().is_ok());
        let decoder = MultistreamDecoder::from_raw(
            unsafe { NonNull::new_unchecked(ptr) },
            sr,
            mapping.channels,
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
        sr: SampleRate,
        mapping: Mapping<'_>,
    ) -> Result<Self> {
        let required = MultistreamDecoder::size(mapping.streams, mapping.coupled_streams)?;
        if buf.capacity_bytes() < required {
            return Err(Error::BadArg);
        }
        let ptr = buf.as_mut_ptr::<OpusMSDecoder>();
        unsafe { MultistreamDecoder::init_in_place(ptr, sr, mapping)? };
        Ok(unsafe { Self::from_raw(ptr, sr, mapping) })
    }
}

impl Deref for MultistreamDecoderRef<'_> {
    type Target = MultistreamDecoder;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for MultistreamDecoderRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_allows_dropped_channels() {
        let mapping = Mapping {
            channels: 6,
            streams: 2,
            coupled_streams: 1,
            mapping: &[0, 1, 2, u8::MAX, u8::MAX, u8::MAX],
        };
        assert!(mapping.validate_for_encoder().is_ok());
    }

    #[test]
    fn mapping_requires_encoder_stream_coverage() {
        let mapping = Mapping {
            channels: 2,
            streams: 1,
            coupled_streams: 1,
            mapping: &[0, 0],
        };
        assert!(mapping.validate_for_decoder().is_ok());
        assert!(mapping.validate_for_encoder().is_err());
    }
}
