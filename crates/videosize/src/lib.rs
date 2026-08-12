//! Fast, dependency-free probing of complete video files and byte slices.
//!
//! `videosize` reads container metadata and bounded codec headers without
//! decoding frames. It supports ISO base media files (MP4 and QuickTime/MOV),
//! Matroska/WebM, and AVI.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::fs::File;

mod codecs;
mod ebml;
mod isobmff;
mod riff;
mod source;
mod util;

use source::Source;

/// An error returned while identifying or probing a video.
#[derive(Debug)]
pub enum VideoError {
    /// The input is not one of the supported containers.
    NotSupported,
    /// The container is malformed, truncated, or has no usable video geometry.
    CorruptedVideo,
    /// A fixed parser security budget was exceeded.
    LimitExceeded,
    /// A genuine filesystem operation failed.
    IoError(std::io::Error),
}

impl fmt::Display for VideoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported => f.write_str("unsupported video format"),
            Self::CorruptedVideo => f.write_str("invalid video or dimensions not found"),
            Self::LimitExceeded => f.write_str("video probing limit exceeded"),
            Self::IoError(error) => error.fmt(f),
        }
    }
}

impl Error for VideoError {}

impl From<std::io::Error> for VideoError {
    fn from(error: std::io::Error) -> Self {
        Self::IoError(error)
    }
}

/// Result type used by this crate.
pub type VideoResult<T> = Result<T, VideoError>;

/// A supported video container.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoType {
    /// ISO base media / MPEG-4 Part 14.
    Mp4,
    /// QuickTime file format.
    Mov,
    /// WebM, the restricted Matroska profile.
    WebM,
    /// Matroska.
    Matroska,
    /// Resource Interchange File Format AVI.
    Avi,
}

/// A video codec recognized in a supported container.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Vp9,
    Vp8,
}

/// Encoded/container pixel dimensions of a video track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VideoSize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl VideoSize {
    /// The reduced width-to-height ratio, ignoring non-square pixels.
    pub fn aspect_ratio(self) -> AspectRatio {
        AspectRatio::new(self.width as u64, self.height as u64)
    }
}

/// A reduced, positive ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AspectRatio {
    /// Ratio numerator.
    pub numerator: u64,
    /// Ratio denominator.
    pub denominator: u64,
}

impl AspectRatio {
    /// Creates a reduced ratio. A zero component produces the neutral ratio 1:1.
    pub fn new(numerator: u64, denominator: u64) -> Self {
        if numerator == 0 || denominator == 0 {
            return Self::square();
        }
        let (numerator, denominator) = util::reduce(numerator, denominator);
        Self {
            numerator,
            denominator,
        }
    }

    /// The ratio for square pixels.
    pub const fn square() -> Self {
        Self {
            numerator: 1,
            denominator: 1,
        }
    }

    /// Returns the ratio as a floating-point value.
    pub fn as_f64(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }

    pub(crate) fn multiply(self, numerator: u64, denominator: u64) -> Self {
        util::ratio_from_u128(
            self.numerator as u128 * numerator as u128,
            self.denominator as u128 * denominator as u128,
        )
    }

    pub(crate) fn multiply_ratio(self, other: Self) -> Self {
        self.multiply(other.numerator, other.denominator)
    }

    pub(crate) fn inverse(self) -> Self {
        Self::new(self.denominator, self.numerator)
    }
}

impl fmt::Display for AspectRatio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.numerator, self.denominator)
    }
}

/// Container and track information discovered without decoding video frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VideoInfo {
    /// Encoded/container pixel dimensions.
    pub size: VideoSize,
    /// Best-known integral square-pixel presentation canvas after visible
    /// cropping, render geometry, pixel aspect, and supported transforms.
    /// Fractional axes are rounded to the nearest pixel.
    pub display_size: VideoSize,
    /// Authoritative display aspect after aperture, cropping, pixel aspect,
    /// render size, and supported transforms are applied.
    pub display_aspect_ratio: AspectRatio,
    /// Container type.
    pub video_type: VideoType,
    /// Codec, when it is one recognized by this crate.
    pub codec: Option<Codec>,
    /// Width-to-height ratio of one pixel.
    pub pixel_aspect_ratio: AspectRatio,
    /// Clockwise display rotation in degrees (0, 90, 180, or 270).
    pub rotation: u16,
}

impl VideoInfo {
    /// Returns [`VideoInfo::display_aspect_ratio`].
    pub const fn aspect_ratio(self) -> AspectRatio {
        self.display_aspect_ratio
    }
}

/// Probe a complete owned file and return its encoded/container dimensions.
pub fn size(file: File) -> VideoResult<VideoSize> {
    Ok(probe(file)?.size)
}

/// Probe encoded/container dimensions from a complete byte slice.
pub fn blob_size(data: &[u8]) -> VideoResult<VideoSize> {
    Ok(blob_probe(data)?.size)
}

/// Probe a complete owned file.
///
/// The file's initial cursor position is ignored.
pub fn probe(file: File) -> VideoResult<VideoInfo> {
    let mut source = Source::file(file)?;
    let kind = sniff_complete_file(&mut source)?;
    source.seek(0);
    match kind {
        VideoType::Mp4 | VideoType::Mov => isobmff::probe(&mut source, kind),
        VideoType::WebM | VideoType::Matroska => ebml::probe(&mut source, kind),
        VideoType::Avi => riff::probe(&mut source),
    }
}

/// Probe a complete video held in a byte slice.
///
/// Prefix-only probing is unsupported; callers must provide the whole file.
pub fn blob_probe(data: &[u8]) -> VideoResult<VideoInfo> {
    let mut source = Source::memory(data);
    let header_size = data.len().min(4096);
    let header = source.view(0, header_size)?;
    let kind = sniff_complete(header)?;
    match kind {
        VideoType::Mp4 | VideoType::Mov => isobmff::probe(&mut source, kind),
        VideoType::WebM | VideoType::Matroska => ebml::probe(&mut source, kind),
        VideoType::Avi => riff::probe(&mut source),
    }
}

/// Identify the container of a complete owned file.
///
/// The file's initial cursor position is ignored.
pub fn file_type(file: File) -> VideoResult<VideoType> {
    let mut source = Source::file(file)?;
    sniff_complete_file(&mut source)
}

/// Identify a container from header bytes.
///
/// This is the only API intentionally designed for partial input. Passing at
/// least the first 4 KiB is recommended.
pub fn video_type(header: &[u8]) -> VideoResult<VideoType> {
    sniff(header).ok_or(VideoError::NotSupported)
}

fn sniff_complete_file(source: &mut Source<'_>) -> VideoResult<VideoType> {
    let size = usize::try_from(source.len().min(4096)).expect("at most 4096");
    let header = source.view(0, size)?;
    sniff_complete(header)
}

fn sniff_complete(data: &[u8]) -> VideoResult<VideoType> {
    if let Some(kind) = sniff(data) {
        return Ok(kind);
    }
    if data.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Ok(VideoType::Matroska);
    }
    if data.starts_with(b"RIFF") {
        return Ok(VideoType::Avi);
    }
    if data.len() >= 8
        && matches!(
            &data[4..8],
            b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip"
        )
    {
        return Ok(if &data[4..8] == b"ftyp" {
            VideoType::Mp4
        } else {
            VideoType::Mov
        });
    }
    Err(VideoError::NotSupported)
}

fn sniff(data: &[u8]) -> Option<VideoType> {
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"AVI " {
        return Some(VideoType::Avi);
    }
    if data.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        for (index, byte) in data.iter().enumerate() {
            match byte {
                b'w' if data[index..].starts_with(b"webm") => return Some(VideoType::WebM),
                b'm' if data[index..].starts_with(b"matroska") => {
                    return Some(VideoType::Matroska);
                }
                _ => {}
            }
        }
        return None;
    }
    sniff_isobmff(data)
}

fn sniff_isobmff(data: &[u8]) -> Option<VideoType> {
    let mut offset = 0usize;
    let mut fallback = None;
    while offset.checked_add(8)? <= data.len() {
        let short_size = util::be(data, offset, 4)? as usize;
        let name = &data[offset + 4..offset + 8];
        if matches!(name, b"moov" | b"mdat" | b"wide" | b"free" | b"skip") {
            fallback = Some(VideoType::Mov);
        }
        let (size, header) = if short_size == 1 {
            match util::be(data, offset + 8, 8) {
                Some(size) => (usize::try_from(size).ok()?, 16),
                None => break,
            }
        } else if short_size == 0 {
            (data.len() - offset, 8)
        } else {
            (short_size, 8)
        };
        if size < header {
            break;
        }
        let end = offset.checked_add(size)?;
        if name == b"ftyp" && size >= header + 4 && offset + header + 4 <= data.len() {
            let available_end = end.min(data.len());
            let brands = &data[offset + header..available_end];
            let quicktime = brands.chunks_exact(4).any(|brand| brand == b"qt  ");
            return Some(if quicktime {
                VideoType::Mov
            } else {
                VideoType::Mp4
            });
        }
        if short_size == 0 || end > data.len() {
            break;
        }
        offset = end;
    }
    fallback
}

pub(crate) fn make_info(
    video_type: VideoType,
    codec: Option<Codec>,
    width: u64,
    height: u64,
    display_width: u64,
    display_height: u64,
    pixel_aspect_ratio: AspectRatio,
    display_aspect_ratio: AspectRatio,
    rotation: u16,
) -> VideoResult<VideoInfo> {
    if width == 0
        || height == 0
        || display_width == 0
        || display_height == 0
        || width > util::MAX_DIMENSION
        || height > util::MAX_DIMENSION
        || display_width > util::MAX_DIMENSION
        || display_height > util::MAX_DIMENSION
    {
        return Err(VideoError::CorruptedVideo);
    }
    Ok(VideoInfo {
        size: VideoSize {
            width: width as u32,
            height: height as u32,
        },
        display_size: VideoSize {
            width: display_width as u32,
            height: display_height as u32,
        },
        display_aspect_ratio,
        video_type,
        codec,
        pixel_aspect_ratio,
        rotation,
    })
}
