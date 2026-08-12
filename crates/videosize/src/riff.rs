use crate::codecs::{self, CodecGeometry};
use crate::source::{Source, Span};
use crate::util::{MAX_NESTING, invalid, le, ratio4, scale_round};
use crate::{AspectRatio, Codec, VideoError, VideoInfo, VideoResult, VideoType, make_info};

const MAX_MOVI: usize = 4;

#[derive(Clone, Copy)]
struct Chunk {
    id: [u8; 4],
    data: u64,
    end: u64,
    next: u64,
}

#[derive(Default)]
struct Stream {
    index: usize,
    video: bool,
    disabled: bool,
    handler: [u8; 4],
    compression: [u8; 4],
    width: u64,
    height: u64,
    frame_width: u64,
    frame_height: u64,
    display_aspect: Option<AspectRatio>,
    active_height: u64,
    private: Option<Span>,
}

pub(crate) fn probe(source: &mut Source<'_>) -> VideoResult<VideoInfo> {
    if source.len() < 12 {
        return invalid();
    }
    let header = source.view(0, 12)?;
    if &header[..4] != b"RIFF" || &header[8..] != b"AVI " {
        return Err(VideoError::NotSupported);
    }
    let riff_end = 8u64
        .checked_add(le(header, 4, 4).unwrap_or(0))
        .ok_or(VideoError::CorruptedVideo)?;
    if riff_end < 12 || riff_end > source.len() {
        return invalid();
    }

    let mut streams = Vec::new();
    let mut global = (0, 0);
    let mut movi = [(0u64, 0u64); MAX_MOVI];
    let mut movi_count = 0;
    source.seek(12);
    while let Some(chunk) = next_chunk(source, riff_end)? {
        if chunk.id == *b"LIST" && chunk.end - chunk.data >= 4 {
            let kind = source.view(chunk.data, 4)?;
            match kind {
                b"hdrl" => (streams, global) = parse_hdrl(source, chunk.data + 4, chunk.end)?,
                b"movi" if movi_count < MAX_MOVI => {
                    movi[movi_count] = (chunk.data + 4, chunk.end);
                    movi_count += 1;
                }
                _ => {}
            }
        }
        source.seek(chunk.next);
    }
    let movi = &movi[..movi_count];

    for wanted in [false, true] {
        for stream in &streams {
            if !stream.video || stream.disabled != wanted {
                continue;
            }
            let geometry = codec_geometry(source, stream, movi)?;
            if dimensions(stream, None, global) != (0, 0) || geometry.is_some() {
                return finish(stream, geometry, global);
            }
        }
    }
    invalid()
}

fn parse_hdrl(
    source: &mut Source<'_>,
    start: u64,
    end: u64,
) -> VideoResult<(Vec<Stream>, (u64, u64))> {
    let mut streams = Vec::new();
    let mut global = (0, 0);
    source.seek(start);
    while let Some(chunk) = next_chunk(source, end)? {
        if chunk.id == *b"avih" && chunk.end - chunk.data >= 40 {
            let bytes = source.view(chunk.data + 32, 8)?;
            global = (le(bytes, 0, 4).unwrap_or(0), le(bytes, 4, 4).unwrap_or(0));
        } else if chunk.id == *b"LIST" && chunk.end - chunk.data >= 4 {
            let kind = source.view(chunk.data, 4)?;
            if kind == b"strl" {
                source.track()?;
                streams.push(parse_stream(
                    source,
                    chunk.data + 4,
                    chunk.end,
                    streams.len(),
                )?);
            }
        }
        source.seek(chunk.next);
    }
    Ok((streams, global))
}

fn parse_stream(
    source: &mut Source<'_>,
    start: u64,
    end: u64,
    index: usize,
) -> VideoResult<Stream> {
    let mut stream = Stream {
        index,
        ..Stream::default()
    };
    source.seek(start);
    while let Some(chunk) = next_chunk(source, end)? {
        match &chunk.id {
            b"strh" => parse_strh(source, chunk, &mut stream)?,
            b"strf" => parse_strf(source, chunk, &mut stream)?,
            b"vprp" => parse_vprp(source, chunk, &mut stream)?,
            _ => {}
        }
        source.seek(chunk.next);
    }
    Ok(stream)
}

fn parse_strh(source: &mut Source<'_>, chunk: Chunk, stream: &mut Stream) -> VideoResult<()> {
    let size = usize::try_from((chunk.end - chunk.data).min(56)).unwrap_or(56);
    if size < 8 {
        return Ok(());
    }
    let bytes = source.view(chunk.data, size)?;
    stream.video = &bytes[..4] == b"vids";
    stream.handler.copy_from_slice(&bytes[4..8]);
    if size >= 12 {
        stream.disabled = le(bytes, 8, 4).unwrap_or(0) & 1 != 0;
    }
    if size >= 56 {
        let left = le(bytes, 48, 2).unwrap_or(0) as u16 as i16 as i32;
        let top = le(bytes, 50, 2).unwrap_or(0) as u16 as i16 as i32;
        let right = le(bytes, 52, 2).unwrap_or(0) as u16 as i16 as i32;
        let bottom = le(bytes, 54, 2).unwrap_or(0) as u16 as i16 as i32;
        stream.frame_width = right.saturating_sub(left).max(0) as u64;
        stream.frame_height = bottom.saturating_sub(top).max(0) as u64;
    }
    Ok(())
}

fn parse_strf(source: &mut Source<'_>, chunk: Chunk, stream: &mut Stream) -> VideoResult<()> {
    if chunk.end - chunk.data < 20 {
        return Ok(());
    }
    let bytes = source.view(chunk.data, 20)?;
    let header_size = le(bytes, 0, 4).unwrap_or(0);
    stream.width = (le(bytes, 4, 4).unwrap_or(0) as u32 as i32).unsigned_abs() as u64;
    stream.height = (le(bytes, 8, 4).unwrap_or(0) as u32 as i32).unsigned_abs() as u64;
    stream.compression.copy_from_slice(&bytes[16..20]);
    if header_size >= 20 && header_size < chunk.end - chunk.data {
        stream.private = Some(Span {
            position: chunk.data + header_size,
            size: chunk.end - chunk.data - header_size,
        });
    }
    Ok(())
}

fn parse_vprp(source: &mut Source<'_>, chunk: Chunk, stream: &mut Stream) -> VideoResult<()> {
    if chunk.end - chunk.data < 32 {
        return Ok(());
    }
    let bytes = source.view(chunk.data + 20, 12)?;
    let packed = le(bytes, 0, 4).unwrap_or(0);
    let active_width = le(bytes, 4, 4).unwrap_or(0);
    let active_height = le(bytes, 8, 4).unwrap_or(0);
    let numerator = packed >> 16;
    let denominator = packed & 0xffff;
    if numerator != 0 && denominator != 0 && active_width != 0 && active_height != 0 {
        stream.display_aspect = Some(AspectRatio::new(numerator, denominator));
        stream.active_height = active_height;
    }
    Ok(())
}

fn codec_geometry(
    source: &mut Source<'_>,
    stream: &Stream,
    movi: &[(u64, u64)],
) -> VideoResult<Option<CodecGeometry>> {
    let Some(codec) = codec(stream) else {
        return Ok(None);
    };
    let sample = find_sample(source, movi, stream.index)?;
    source.geometry(codec, stream.private, sample)
}

fn find_sample(
    source: &mut Source<'_>,
    movi: &[(u64, u64)],
    stream: usize,
) -> VideoResult<Option<Span>> {
    if stream >= 100 {
        return Ok(None);
    }
    for &(start, end) in movi {
        if let Some(span) = scan_movi(source, start, end, stream, 0)? {
            return Ok(Some(span));
        }
    }
    Ok(None)
}

/// Searches a `movi` list for the selected stream's first media chunk.
///
/// `depth` counts nested `rec ` lists, which the format allows but which are
/// otherwise free to nest deeply enough to exhaust the stack.
fn scan_movi(
    source: &mut Source<'_>,
    start: u64,
    end: u64,
    stream: usize,
    depth: u32,
) -> VideoResult<Option<Span>> {
    if depth > MAX_NESTING {
        return Err(VideoError::LimitExceeded);
    }
    source.seek(start);
    while let Some(chunk) = next_chunk(source, end)? {
        if media_chunk(chunk.id, stream) {
            return Ok(Some(Span {
                position: chunk.data,
                size: chunk.end - chunk.data,
            }));
        }
        if chunk.id == *b"LIST" && chunk.end - chunk.data >= 4 {
            let kind = source.view(chunk.data, 4)?;
            if kind == b"rec "
                && let Some(span) = scan_movi(source, chunk.data + 4, chunk.end, stream, depth + 1)?
            {
                return Ok(Some(span));
            }
        }
        source.seek(chunk.next);
    }
    Ok(None)
}

fn dimensions(stream: &Stream, geometry: Option<CodecGeometry>, global: (u64, u64)) -> (u64, u64) {
    let width = first(
        stream.width,
        stream.frame_width,
        geometry.map_or(0, |g| g.coded_width),
        global.0,
    );
    let height = first(
        stream.height,
        stream.frame_height,
        geometry.map_or(0, |g| g.coded_height),
        global.1,
    );
    (width, height)
}

fn finish(
    stream: &Stream,
    geometry: Option<CodecGeometry>,
    global: (u64, u64),
) -> VideoResult<VideoInfo> {
    let (width, height) = dimensions(stream, geometry, global);
    if width == 0 || height == 0 {
        return Err(VideoError::CorruptedVideo);
    }
    let visible = geometry
        .and_then(CodecGeometry::display_dimensions)
        .unwrap_or((width, height));
    let codec_pixel = geometry
        .and_then(|value| value.pixel_aspect_ratio)
        .unwrap_or_else(AspectRatio::square);
    let (pixel, display) = if let Some(display) = stream.display_aspect {
        (
            ratio4(display.numerator, display.denominator, visible.0, visible.1),
            display,
        )
    } else {
        (
            codec_pixel,
            AspectRatio::new(visible.0, visible.1).multiply_ratio(codec_pixel),
        )
    };
    let display_height = if stream.active_height != 0 {
        stream.active_height
    } else {
        visible.1
    };
    let display_width = scale_round(display_height, display.numerator, display.denominator)
        .ok_or(VideoError::CorruptedVideo)?;
    make_info(
        VideoType::Avi,
        codec(stream),
        width,
        height,
        display_width,
        display_height,
        pixel,
        display,
        0,
    )
}

fn next_chunk(source: &mut Source<'_>, parent_end: u64) -> VideoResult<Option<Chunk>> {
    let start = source.position();
    if start == parent_end {
        return Ok(None);
    }
    if start > parent_end || parent_end - start < 8 {
        return invalid();
    }
    source.element()?;
    let head = source.view(start, 8)?;
    let id = [head[0], head[1], head[2], head[3]];
    let size = le(head, 4, 4).unwrap_or(0);
    let data = start + 8;
    let end = data.checked_add(size).ok_or(VideoError::CorruptedVideo)?;
    let next = end
        .checked_add(size & 1)
        .ok_or(VideoError::CorruptedVideo)?;
    if end > parent_end || next > parent_end {
        return invalid();
    }
    Ok(Some(Chunk {
        id,
        data,
        end,
        next,
    }))
}

fn first(a: u64, b: u64, c: u64, d: u64) -> u64 {
    if a != 0 {
        a
    } else if b != 0 {
        b
    } else if c != 0 {
        c
    } else {
        d
    }
}

fn media_chunk(id: [u8; 4], stream: usize) -> bool {
    id[0] == b'0' + (stream / 10) as u8
        && id[1] == b'0' + (stream % 10) as u8
        && matches!(&id[2..], b"dc" | b"db")
}

fn codec(stream: &Stream) -> Option<Codec> {
    codecs::from_id(&stream.compression).or_else(|| codecs::from_id(&stream.handler))
}
