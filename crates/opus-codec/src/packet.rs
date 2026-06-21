//! Safe helpers around opus packet inspection and parsing

#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![cfg_attr(not(opus_codec_rust_packet_ops), allow(dead_code))]

use crate::bindings::{
    OPUS_BANDWIDTH_FULLBAND, OPUS_BANDWIDTH_MEDIUMBAND, OPUS_BANDWIDTH_NARROWBAND,
    OPUS_BANDWIDTH_SUPERWIDEBAND, OPUS_BANDWIDTH_WIDEBAND, opus_multistream_packet_unpad,
    opus_packet_get_bandwidth, opus_packet_get_nb_channels, opus_packet_get_nb_frames,
    opus_packet_get_nb_samples, opus_packet_get_samples_per_frame, opus_packet_has_lbrr,
    opus_packet_parse, opus_packet_unpad, opus_pcm_soft_clip,
};
#[cfg(not(opus_codec_rust_packet_ops))]
use crate::bindings::{opus_multistream_packet_pad, opus_packet_pad};
use crate::error::{Error, Result};
use crate::types::{Bandwidth, Channels, SampleRate};

mod layout;

use layout::MAX_FRAMES_PER_PACKET;
#[cfg(opus_codec_rust_packet_ops)]
pub(crate) use layout::{
    RepacketizerInputs, packet_repacketizer_inputs, repacketize_frames, repacketize_frames_range,
};
#[cfg(opus_codec_rust_packet_ops)]
use layout::{multistream_last_stream_offset, pad_single_packet};

/// Get bandwidth from a packet.
///
/// # Errors
/// Returns `InvalidPacket` if the packet is malformed.
pub fn packet_bandwidth(packet: &[u8]) -> Result<Bandwidth> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let bw = unsafe { opus_packet_get_bandwidth(packet.as_ptr()) };
    match bw {
        x if x == OPUS_BANDWIDTH_NARROWBAND as i32 => Ok(Bandwidth::Narrowband),
        x if x == OPUS_BANDWIDTH_MEDIUMBAND as i32 => Ok(Bandwidth::Mediumband),
        x if x == OPUS_BANDWIDTH_WIDEBAND as i32 => Ok(Bandwidth::Wideband),
        x if x == OPUS_BANDWIDTH_SUPERWIDEBAND as i32 => Ok(Bandwidth::SuperWideband),
        x if x == OPUS_BANDWIDTH_FULLBAND as i32 => Ok(Bandwidth::Fullband),
        _ => Err(Error::InvalidPacket),
    }
}

/// Get channel count encoded by the packet.
///
/// # Errors
/// Returns `InvalidPacket` if the packet is malformed.
pub fn packet_channels(packet: &[u8]) -> Result<Channels> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let ch = unsafe { opus_packet_get_nb_channels(packet.as_ptr()) };
    match ch {
        1 => Ok(Channels::Mono),
        2 => Ok(Channels::Stereo),
        _ => Err(Error::InvalidPacket),
    }
}

/// Get number of frames in a packet.
///
/// # Errors
/// Returns an error if the packet cannot be parsed.
pub fn packet_frame_count(packet: &[u8]) -> Result<usize> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let len_i32 = i32::try_from(packet.len()).map_err(|_| Error::BadArg)?;
    let n = unsafe { opus_packet_get_nb_frames(packet.as_ptr(), len_i32) };
    if n < 0 {
        return Err(Error::from_code(n));
    }
    usize::try_from(n).map_err(|_| Error::InternalError)
}

/// Get total samples (per channel) in a packet at the given sample rate.
///
/// # Errors
/// Returns an error if the packet cannot be parsed.
pub fn packet_sample_count(packet: &[u8], sample_rate: SampleRate) -> Result<usize> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let len_i32 = i32::try_from(packet.len()).map_err(|_| Error::BadArg)?;
    let n = unsafe { opus_packet_get_nb_samples(packet.as_ptr(), len_i32, sample_rate.as_i32()) };
    if n < 0 {
        return Err(Error::from_code(n));
    }
    usize::try_from(n).map_err(|_| Error::InternalError)
}

/// Get the number of samples per frame for a packet at a given sample rate.
///
/// # Errors
/// Returns [`Error::BadArg`] if `packet` is empty.
pub fn packet_samples_per_frame(packet: &[u8], sample_rate: SampleRate) -> Result<usize> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let n = unsafe { opus_packet_get_samples_per_frame(packet.as_ptr(), sample_rate.as_i32()) };
    if n <= 0 {
        return Err(Error::InvalidPacket);
    }
    usize::try_from(n).map_err(|_| Error::InternalError)
}

/// Check if packet has LBRR.
///
/// # Errors
/// Returns an error if the packet cannot be parsed.
pub fn packet_has_lbrr(packet: &[u8]) -> Result<bool> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let len_i32 = i32::try_from(packet.len()).map_err(|_| Error::BadArg)?;
    let v = unsafe { opus_packet_has_lbrr(packet.as_ptr(), len_i32) };
    if v < 0 {
        return Err(Error::from_code(v));
    }
    Ok(v != 0)
}

/// Apply libopus soft clipping to keep float PCM within [-1, 1].
///
/// The clipping state memory must be provided per-channel and preserved across calls
/// for continuous processing. Initialize with zeros for a new stream.
///
/// # Errors
/// Returns [`Error::BadArg`] when the PCM slice, frame size, or soft-clip memory
/// do not match the provided channel configuration.
pub fn soft_clip(
    pcm: &mut [f32],
    frame_size_per_ch: usize,
    channels: i32,
    softclip_mem: &mut [f32],
) -> Result<()> {
    if frame_size_per_ch == 0 {
        return Err(Error::BadArg);
    }
    let channels_usize = usize::try_from(channels).map_err(|_| Error::BadArg)?;
    if channels_usize == 0 {
        return Err(Error::BadArg);
    }
    if softclip_mem.len() < channels_usize {
        return Err(Error::BadArg);
    }
    let needed_samples = frame_size_per_ch
        .checked_mul(channels_usize)
        .ok_or(Error::BadArg)?;
    if pcm.len() < needed_samples {
        return Err(Error::BadArg);
    }
    let frame_i32 = i32::try_from(frame_size_per_ch).map_err(|_| Error::BadArg)?;
    unsafe {
        opus_pcm_soft_clip(
            pcm.as_mut_ptr(),
            frame_i32,
            channels,
            softclip_mem.as_mut_ptr(),
        );
    }
    Ok(())
}

/// Parse packet into frame pointers and sizes. Returns (toc, `payload_offset`, `frame_sizes`).
/// Note: Returned frame slices borrow from `packet` and are valid as long as `packet` lives.
///
/// # Errors
/// Returns an error if the packet cannot be parsed.
pub fn packet_parse(packet: &[u8]) -> Result<(u8, usize, Vec<&[u8]>)> {
    if packet.is_empty() {
        return Err(Error::BadArg);
    }
    let mut out_toc: u8 = 0;
    let mut payload_offset: i32 = 0;
    // libopus caps frames at MAX_FRAMES_PER_PACKET according to docs.
    let mut frames_ptrs: [*const u8; MAX_FRAMES_PER_PACKET] =
        [std::ptr::null(); MAX_FRAMES_PER_PACKET];
    let mut sizes: [i16; MAX_FRAMES_PER_PACKET] = [0; MAX_FRAMES_PER_PACKET];
    let len_i32 = i32::try_from(packet.len()).map_err(|_| Error::BadArg)?;
    let n = unsafe {
        opus_packet_parse(
            packet.as_ptr(),
            len_i32,
            &raw mut out_toc,
            frames_ptrs.as_mut_ptr().cast::<*const u8>(),
            sizes.as_mut_ptr(),
            &raw mut payload_offset,
        )
    };
    if n < 0 {
        return Err(Error::from_code(n));
    }
    let count = usize::try_from(n).map_err(|_| Error::InternalError)?;
    let mut frames = Vec::with_capacity(count);
    for i in 0..count {
        let size = usize::try_from(sizes[i]).map_err(|_| Error::InternalError)?;
        let ptr = frames_ptrs[i];
        if ptr.is_null() {
            return Err(Error::InvalidPacket);
        }
        let ptr_addr = ptr as usize;
        let base_addr = packet.as_ptr() as usize;
        if ptr_addr < base_addr {
            return Err(Error::InvalidPacket);
        }
        // SAFETY: pointers are into `packet`; derive offset via pointer arithmetic
        let start = ptr_addr - base_addr;
        let end = start.checked_add(size).ok_or(Error::InternalError)?;
        if end > packet.len() {
            return Err(Error::InvalidPacket);
        }
        frames.push(&packet[start..end]);
    }
    Ok((
        out_toc,
        usize::try_from(payload_offset).map_err(|_| Error::InternalError)?,
        frames,
    ))
}

/// Increase a packet's size by adding padding to reach `new_len`.
///
/// # Errors
/// Returns [`Error::BadArg`] for invalid lengths or another error if padding fails.
#[cfg(opus_codec_rust_packet_ops)]
pub fn packet_pad(packet: &mut [u8], len: usize, new_len: usize) -> Result<()> {
    if new_len < len || new_len > packet.len() {
        return Err(Error::BadArg);
    }
    if len == 0 {
        return Err(Error::BadArg);
    }
    if len == new_len {
        return Ok(());
    }
    pad_single_packet(packet, len, new_len)
}

/// Increase a packet's size by adding padding to reach `new_len`.
///
/// # Errors
/// Returns [`Error::BadArg`] for invalid lengths or a mapped libopus error if padding fails.
#[cfg(not(opus_codec_rust_packet_ops))]
pub fn packet_pad(packet: &mut [u8], len: usize, new_len: usize) -> Result<()> {
    if new_len < len || new_len > packet.len() {
        return Err(Error::BadArg);
    }
    if len == 0 {
        return Err(Error::BadArg);
    }
    let len_i32 = i32::try_from(len).map_err(|_| Error::BadArg)?;
    let new_len_i32 = i32::try_from(new_len).map_err(|_| Error::BadArg)?;
    let r = unsafe { opus_packet_pad(packet.as_mut_ptr(), len_i32, new_len_i32) };
    if r != 0 {
        return Err(Error::from_code(r));
    }
    Ok(())
}

/// Remove padding from a packet; returns new length or error.
///
/// # Errors
/// Returns [`Error::BadArg`] for invalid lengths or a mapped libopus error if unpadding fails.
pub fn packet_unpad(packet: &mut [u8], len: usize) -> Result<usize> {
    if len > packet.len() {
        return Err(Error::BadArg);
    }
    if len == 0 {
        return Err(Error::BadArg);
    }
    let len_i32 = i32::try_from(len).map_err(|_| Error::BadArg)?;
    let n = unsafe { opus_packet_unpad(packet.as_mut_ptr(), len_i32) };
    if n < 0 {
        return Err(Error::from_code(n));
    }
    usize::try_from(n).map_err(|_| Error::InternalError)
}

/// Pad a multistream packet to `new_len` given `nb_streams`.
///
/// # Errors
/// Returns [`Error::BadArg`] for invalid lengths or another error if padding fails.
#[cfg(opus_codec_rust_packet_ops)]
pub fn multistream_packet_pad(
    packet: &mut [u8],
    len: usize,
    new_len: usize,
    nb_streams: i32,
) -> Result<()> {
    if new_len < len || new_len > packet.len() {
        return Err(Error::BadArg);
    }
    if len == 0 {
        return Err(Error::BadArg);
    }
    // The public API requires at least one stream. Reject invalid counts
    // before delegating to libopus' multistream packet walker.
    if nb_streams < 1 {
        return Err(Error::BadArg);
    }
    if len == new_len {
        return Ok(());
    }
    let nb_streams = usize::try_from(nb_streams).map_err(|_| Error::BadArg)?;
    let last_stream_offset = multistream_last_stream_offset(&packet[..len], nb_streams)?;
    let amount = new_len - len;
    let last_len = len - last_stream_offset;
    let last_new_len = last_len.checked_add(amount).ok_or(Error::BadArg)?;
    pad_single_packet(&mut packet[last_stream_offset..], last_len, last_new_len)
}

/// Pad a multistream packet to `new_len` given `nb_streams`.
///
/// # Errors
/// Returns [`Error::BadArg`] for invalid lengths or a mapped libopus error if padding fails.
#[cfg(not(opus_codec_rust_packet_ops))]
pub fn multistream_packet_pad(
    packet: &mut [u8],
    len: usize,
    new_len: usize,
    nb_streams: i32,
) -> Result<()> {
    if new_len < len || new_len > packet.len() {
        return Err(Error::BadArg);
    }
    if len == 0 {
        return Err(Error::BadArg);
    }
    if nb_streams < 1 {
        return Err(Error::BadArg);
    }
    let len_i32 = i32::try_from(len).map_err(|_| Error::BadArg)?;
    let new_len_i32 = i32::try_from(new_len).map_err(|_| Error::BadArg)?;
    let r = unsafe {
        opus_multistream_packet_pad(packet.as_mut_ptr(), len_i32, new_len_i32, nb_streams)
    };
    if r != 0 {
        return Err(Error::from_code(r));
    }
    Ok(())
}

/// Remove padding from a multistream packet; returns new length.
///
/// # Errors
/// Returns [`Error::BadArg`] for invalid lengths or a mapped libopus error if unpadding fails.
pub fn multistream_packet_unpad(packet: &mut [u8], len: usize, nb_streams: i32) -> Result<usize> {
    if len > packet.len() {
        return Err(Error::BadArg);
    }
    if len == 0 {
        return Err(Error::BadArg);
    }
    // The public API requires at least one stream. Reject invalid counts
    // before delegating to libopus' multistream packet walker.
    if nb_streams < 1 {
        return Err(Error::BadArg);
    }
    let len_i32 = i32::try_from(len).map_err(|_| Error::BadArg)?;
    let n = unsafe { opus_multistream_packet_unpad(packet.as_mut_ptr(), len_i32, nb_streams) };
    if n < 0 {
        return Err(Error::from_code(n));
    }
    usize::try_from(n).map_err(|_| Error::InternalError)
}
