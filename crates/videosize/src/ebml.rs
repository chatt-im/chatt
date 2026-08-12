use crate::codecs::{self, CodecGeometry};
use crate::source::{Source, Span};
use crate::util::{be, invalid, ratio4, scale_round};
use crate::{AspectRatio, Codec, VideoError, VideoInfo, VideoResult, VideoType, make_info};

const EBML: u64 = 0x1a45dfa3;
const DOC_TYPE: u64 = 0x4282;
const SEGMENT: u64 = 0x18538067;
const TRACKS: u64 = 0x1654ae6b;
const TRACK_ENTRY: u64 = 0xae;
const TRACK_NUMBER: u64 = 0xd7;
const TRACK_TYPE: u64 = 0x83;
const FLAG_ENABLED: u64 = 0xb9;
const FLAG_DEFAULT: u64 = 0x88;
const CODEC_ID: u64 = 0x86;
const CODEC_PRIVATE: u64 = 0x63a2;
const VIDEO: u64 = 0xe0;
const PIXEL_WIDTH: u64 = 0xb0;
const PIXEL_HEIGHT: u64 = 0xba;
const CROP_BOTTOM: u64 = 0x54aa;
const CROP_TOP: u64 = 0x54bb;
const CROP_LEFT: u64 = 0x54cc;
const CROP_RIGHT: u64 = 0x54dd;
const DISPLAY_WIDTH: u64 = 0x54b0;
const DISPLAY_HEIGHT: u64 = 0x54ba;
const DISPLAY_UNIT: u64 = 0x54b2;
const PROJECTION: u64 = 0x7670;
const PROJECTION_POSE_ROLL: u64 = 0x7675;
const CLUSTER: u64 = 0x1f43b675;
/// Bound on the block header window; a longer lace table means the frame is
/// found in a later block instead.
const BLOCK_HEADER: usize = 1024;
const SIMPLE_BLOCK: u64 = 0xa3;
const BLOCK_GROUP: u64 = 0xa0;
const BLOCK: u64 = 0xa1;

#[derive(Clone, Copy)]
struct Element {
    id: u64,
    data: u64,
    end: u64,
}

#[derive(Default)]
struct Track {
    number: u64,
    kind: u64,
    /// Stored negated: both EBML flags default to set.
    disabled: bool,
    undefaulted: bool,
    codec: Option<Codec>,
    private: Option<Span>,
    width: u64,
    height: u64,
    crop: [u64; 4],
    display_width: Option<u64>,
    display_height: Option<u64>,
    display_unit: u64,
    rotation: u16,
}

impl Track {
    fn rank(&self) -> u8 {
        if self.disabled {
            2
        } else {
            u8::from(self.undefaulted)
        }
    }
}

pub(crate) fn probe(source: &mut Source<'_>, detected: VideoType) -> VideoResult<VideoInfo> {
    let file_end = source.len();
    source.seek(0);
    let header = next_element(source, file_end)?.ok_or(VideoError::CorruptedVideo)?;
    if header.id != EBML {
        return invalid();
    }
    let kind = doc_type(source, header)?.unwrap_or(detected);
    source.seek(header.end);
    let mut segment = None;
    while let Some(element) = next_element(source, file_end)? {
        if element.id == SEGMENT {
            segment = Some(element);
            break;
        }
        source.seek(element.end);
    }
    let segment = segment.ok_or(VideoError::CorruptedVideo)?;
    let tracks = parse_tracks(source, segment)?;

    for wanted in 0..3 {
        for track in &tracks {
            if track.kind != 1 || track.number == 0 || track.rank() != wanted {
                continue;
            }
            let geometry = codec_geometry(source, segment, track)?;
            if (track.width != 0 && track.height != 0) || geometry.is_some() {
                return finish(kind, track, geometry);
            }
        }
    }
    invalid()
}

fn doc_type(source: &mut Source<'_>, header: Element) -> VideoResult<Option<VideoType>> {
    source.seek(header.data);
    while let Some(element) = next_element(source, header.end)? {
        if element.id == DOC_TYPE {
            let size = usize::try_from(element.end - element.data)
                .map_err(|_| VideoError::CorruptedVideo)?;
            if size > 128 {
                return invalid();
            }
            let value = source.view(element.data, size)?;
            return Ok(if value.starts_with(b"webm") {
                Some(VideoType::WebM)
            } else if value.starts_with(b"matroska") {
                Some(VideoType::Matroska)
            } else {
                return Err(VideoError::NotSupported);
            });
        }
        source.seek(element.end);
    }
    Ok(None)
}

fn parse_tracks(source: &mut Source<'_>, segment: Element) -> VideoResult<Vec<Track>> {
    source.seek(segment.data);
    while let Some(element) = next_element(source, segment.end)? {
        if element.id == TRACKS {
            let mut tracks = Vec::new();
            source.seek(element.data);
            while let Some(entry) = next_element(source, element.end)? {
                if entry.id == TRACK_ENTRY {
                    source.track()?;
                    tracks.push(parse_track(source, entry)?);
                }
                source.seek(entry.end);
            }
            return Ok(tracks);
        }
        source.seek(element.end);
    }
    Err(VideoError::CorruptedVideo)
}

fn parse_track(source: &mut Source<'_>, entry: Element) -> VideoResult<Track> {
    let mut track = Track::default();
    source.seek(entry.data);
    while let Some(element) = next_element(source, entry.end)? {
        match element.id {
            TRACK_NUMBER => track.number = uint(source, element)?,
            TRACK_TYPE => track.kind = uint(source, element)?,
            FLAG_ENABLED => track.disabled = uint(source, element)? == 0,
            FLAG_DEFAULT => track.undefaulted = uint(source, element)? == 0,
            CODEC_ID => track.codec = codec_id(source, element)?,
            CODEC_PRIVATE => {
                track.private = Some(Span {
                    position: element.data,
                    size: element.end - element.data,
                });
            }
            VIDEO => parse_video(source, element, &mut track)?,
            _ => {}
        }
        source.seek(element.end);
    }
    Ok(track)
}

fn parse_video(source: &mut Source<'_>, video: Element, track: &mut Track) -> VideoResult<()> {
    source.seek(video.data);
    while let Some(element) = next_element(source, video.end)? {
        match element.id {
            PIXEL_WIDTH => track.width = uint(source, element)?,
            PIXEL_HEIGHT => track.height = uint(source, element)?,
            CROP_BOTTOM => track.crop[0] = uint(source, element)?,
            CROP_TOP => track.crop[1] = uint(source, element)?,
            CROP_LEFT => track.crop[2] = uint(source, element)?,
            CROP_RIGHT => track.crop[3] = uint(source, element)?,
            DISPLAY_WIDTH => track.display_width = Some(uint(source, element)?),
            DISPLAY_HEIGHT => track.display_height = Some(uint(source, element)?),
            DISPLAY_UNIT => track.display_unit = uint(source, element)?,
            PROJECTION => parse_projection(source, element, track)?,
            _ => {}
        }
        source.seek(element.end);
    }
    Ok(())
}

fn parse_projection(
    source: &mut Source<'_>,
    projection: Element,
    track: &mut Track,
) -> VideoResult<()> {
    source.seek(projection.data);
    while let Some(element) = next_element(source, projection.end)? {
        if element.id == PROJECTION_POSE_ROLL {
            track.rotation = float(source, element)?.and_then(quarter_turn).unwrap_or(0);
        }
        source.seek(element.end);
    }
    Ok(())
}

fn codec_geometry(
    source: &mut Source<'_>,
    segment: Element,
    track: &Track,
) -> VideoResult<Option<CodecGeometry>> {
    let Some(codec) = track.codec else {
        return Ok(None);
    };
    let sample = find_frame(source, segment, track.number)?;
    source.geometry(codec, track.private, sample)
}

fn find_frame(source: &mut Source<'_>, segment: Element, track: u64) -> VideoResult<Option<Span>> {
    source.seek(segment.data);
    while let Some(element) = next_element(source, segment.end)? {
        if element.id == CLUSTER {
            source.seek(element.data);
            while let Some(block) = next_element(source, element.end)? {
                if block.id == SIMPLE_BLOCK {
                    if let Some(span) = parse_block(source, block, track)? {
                        return Ok(Some(span));
                    }
                } else if block.id == BLOCK_GROUP {
                    source.seek(block.data);
                    while let Some(child) = next_element(source, block.end)? {
                        if child.id == BLOCK
                            && let Some(span) = parse_block(source, child, track)?
                        {
                            return Ok(Some(span));
                        }
                        source.seek(child.end);
                    }
                }
                source.seek(block.end);
            }
        }
        source.seek(element.end);
    }
    Ok(None)
}

fn parse_block(
    source: &mut Source<'_>,
    block: Element,
    selected_track: u64,
) -> VideoResult<Option<Span>> {
    let window = usize::try_from(block.end - block.data).unwrap_or(BLOCK_HEADER);
    let head = source.peek(block.data, window.min(BLOCK_HEADER))?;
    let Some((track, width)) = vint_value(head) else {
        return invalid();
    };
    if track != selected_track {
        return Ok(None);
    }
    let Some(&flags) = head.get(width + 2) else {
        return invalid();
    };
    let mut offset = width + 3;
    let lacing = (flags >> 1) & 3;
    let mut first_size = 0u64;
    let count = match (lacing, head.get(offset)) {
        (0, _) => 1,
        (_, Some(&count)) => {
            offset += 1;
            count as usize + 1
        }
        // A lace header longer than the window is left for the next block.
        (_, None) => return Ok(None),
    };
    match lacing {
        1 => {
            for lace in 0..count - 1 {
                let mut size = 0u64;
                loop {
                    let Some(&byte) = head.get(offset) else {
                        return Ok(None);
                    };
                    offset += 1;
                    size += byte as u64;
                    if byte != 255 {
                        break;
                    }
                }
                if lace == 0 {
                    first_size = size;
                }
            }
        }
        3 => {
            for lace in 0..count - 1 {
                let Some((size, width)) = vint_value(&head[offset..]) else {
                    return Ok(None);
                };
                offset += width;
                if lace == 0 {
                    first_size = size;
                }
            }
        }
        _ => {}
    }
    let payload = block.data + offset as u64;
    let remaining = block
        .end
        .checked_sub(payload)
        .ok_or(VideoError::CorruptedVideo)?;
    if lacing == 0 {
        first_size = remaining;
    } else if lacing == 2 {
        if count < 2 || remaining % count as u64 != 0 {
            return invalid();
        }
        first_size = remaining / count as u64;
    }
    if first_size == 0 || first_size > remaining {
        return invalid();
    }
    Ok(Some(Span {
        position: payload,
        size: first_size,
    }))
}

fn finish(
    kind: VideoType,
    track: &Track,
    geometry: Option<CodecGeometry>,
) -> VideoResult<VideoInfo> {
    let width = if track.width != 0 {
        track.width
    } else {
        geometry.map_or(0, |g| g.coded_width)
    };
    let height = if track.height != 0 {
        track.height
    } else {
        geometry.map_or(0, |g| g.coded_height)
    };
    if width == 0 || height == 0 || track.display_unit > 3 {
        return Err(VideoError::CorruptedVideo);
    }
    let horizontal_crop = track.crop[2]
        .checked_add(track.crop[3])
        .ok_or(VideoError::CorruptedVideo)?;
    let vertical_crop = track.crop[0]
        .checked_add(track.crop[1])
        .ok_or(VideoError::CorruptedVideo)?;
    let cropped = (
        width.checked_sub(horizontal_crop).filter(|v| *v != 0),
        height.checked_sub(vertical_crop).filter(|v| *v != 0),
    );
    let cropped = match cropped {
        (Some(width), Some(height)) => (width, height),
        _ => return Err(VideoError::CorruptedVideo),
    };
    let codec_display = geometry
        .and_then(CodecGeometry::display_dimensions)
        .unwrap_or(cropped);
    let visible = if horizontal_crop != 0 || vertical_crop != 0 {
        cropped
    } else {
        codec_display
    };
    let explicit = track.display_width.is_some() || track.display_height.is_some();
    let display = (
        track.display_width.unwrap_or(visible.0),
        track.display_height.unwrap_or(visible.1),
    );
    if display.0 == 0 || display.1 == 0 {
        return Err(VideoError::CorruptedVideo);
    }
    let codec_pixel = geometry
        .and_then(|value| value.pixel_aspect_ratio)
        .unwrap_or_else(AspectRatio::square);
    let pixel = if explicit {
        ratio4(display.0, display.1, visible.0, visible.1)
    } else {
        codec_pixel
    };
    let mut ratio = if explicit {
        AspectRatio::new(display.0, display.1)
    } else {
        AspectRatio::new(visible.0, visible.1).multiply_ratio(pixel)
    };
    let mut display_size = if explicit && track.display_unit == 0 {
        display
    } else {
        (
            scale_round(visible.1, ratio.numerator, ratio.denominator)
                .ok_or(VideoError::CorruptedVideo)?,
            visible.1,
        )
    };
    if matches!(track.rotation, 90 | 270) {
        ratio = ratio.inverse();
        display_size = (display_size.1, display_size.0);
    }
    make_info(
        kind,
        track.codec,
        width,
        height,
        display_size.0,
        display_size.1,
        pixel,
        ratio,
        track.rotation,
    )
}

fn uint(source: &mut Source<'_>, element: Element) -> VideoResult<u64> {
    let size = element.end - element.data;
    if size == 0 || size > 8 {
        return invalid();
    }
    let bytes = source.view(element.data, size as usize)?;
    Ok(be(bytes, 0, bytes.len()).unwrap_or(0))
}

fn codec_id(source: &mut Source<'_>, element: Element) -> VideoResult<Option<Codec>> {
    let size =
        usize::try_from(element.end - element.data).map_err(|_| VideoError::CorruptedVideo)?;
    if size > 128 {
        return invalid();
    }
    let bytes = source.view(element.data, size)?;
    Ok(codecs::from_id(bytes))
}

fn float(source: &mut Source<'_>, element: Element) -> VideoResult<Option<f64>> {
    let size = element.end - element.data;
    if size != 4 && size != 8 {
        return Ok(None);
    }
    let bytes = source.view(element.data, size as usize)?;
    let Some(bits) = be(bytes, 0, size as usize) else {
        return Ok(None);
    };
    let value = if size == 4 {
        f32::from_bits(bits as u32) as f64
    } else {
        f64::from_bits(bits)
    };
    Ok(value.is_finite().then_some(value))
}

/// Decodes a variable-length integer, keeping the width marker.
///
/// Element ids are compared as written, so they keep the marker; sizes strip it
/// with [`vint_value`].
fn vint(bytes: &[u8], max_width: usize) -> Option<(u64, usize)> {
    let &first = bytes.first()?;
    if first == 0 {
        return None;
    }
    let width = first.leading_zeros() as usize + 1;
    if width > max_width {
        return None;
    }
    Some((be(bytes, 0, width)?, width))
}

fn vint_value(bytes: &[u8]) -> Option<(u64, usize)> {
    let (value, width) = vint(bytes, 8)?;
    Some((value & ((1u64 << (7 * width)) - 1), width))
}

fn next_element(source: &mut Source<'_>, parent_end: u64) -> VideoResult<Option<Element>> {
    let start = source.position();
    if start == parent_end {
        return Ok(None);
    }
    if start > parent_end {
        return invalid();
    }
    source.element()?;
    let head = source.peek(start, 12)?;
    let Some((id, id_width)) = vint(head, 4) else {
        return invalid();
    };
    let Some((size, size_width)) = vint_value(&head[id_width..]) else {
        return invalid();
    };
    let data = start + (id_width + size_width) as u64;
    // The header is read from the file rather than from the parent's remaining
    // bytes, so one that overruns the parent has to be rejected here; an unknown
    // size would otherwise place the element's end before its start.
    if data > parent_end {
        return invalid();
    }
    let unknown = size == (1u64 << (7 * size_width)) - 1;
    let end = if unknown {
        parent_end
    } else {
        data.checked_add(size).ok_or(VideoError::CorruptedVideo)?
    };
    if end > parent_end {
        return invalid();
    }
    Ok(Some(Element { id, data, end }))
}

fn quarter_turn(value: f64) -> Option<u16> {
    let rounded = value.round();
    if (value - rounded).abs() > 0.001 {
        return None;
    }
    match (rounded as i64).rem_euclid(360) {
        0 => Some(0),
        90 => Some(270),
        180 => Some(180),
        270 => Some(90),
        _ => None,
    }
}
