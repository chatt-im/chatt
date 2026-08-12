use crate::codecs::{self, CodecGeometry};
use crate::source::{Source, Span};
use crate::util::{MAX_NESTING, be, invalid, ratio_from_u128};
use crate::{AspectRatio, Codec, VideoError, VideoInfo, VideoResult, VideoType, make_info};

#[derive(Clone, Copy)]
struct Atom {
    start: u64,
    data: u64,
    end: u64,
    kind: [u8; 4],
}

#[derive(Clone, Copy)]
struct Matrix {
    a: i128,
    b: i128,
    c: i128,
    d: i128,
}

impl Default for Matrix {
    fn default() -> Self {
        Self {
            a: 1 << 16,
            b: 0,
            c: 0,
            d: 1 << 16,
        }
    }
}

impl Matrix {
    fn compose(self, inner: Self) -> Self {
        Self {
            a: self.a * inner.a + self.c * inner.b,
            b: self.b * inner.a + self.d * inner.b,
            c: self.a * inner.c + self.c * inner.d,
            d: self.b * inner.c + self.d * inner.d,
        }
    }

    fn presentation(self, ratio: AspectRatio) -> (AspectRatio, u16) {
        if self.b.abs() + self.c.abs() > self.a.abs() + self.d.abs() {
            let ratio = if self.b == 0 || self.c == 0 {
                ratio.inverse()
            } else {
                ratio_from_u128(
                    ratio.denominator as u128 * self.c.unsigned_abs(),
                    ratio.numerator as u128 * self.b.unsigned_abs(),
                )
            };
            return (ratio, if self.b > 0 && self.c < 0 { 90 } else { 270 });
        }
        let ratio = if self.a == 0 || self.d == 0 {
            ratio
        } else {
            ratio_from_u128(
                ratio.numerator as u128 * self.a.unsigned_abs(),
                ratio.denominator as u128 * self.d.unsigned_abs(),
            )
        };
        (ratio, if self.a < 0 && self.d < 0 { 180 } else { 0 })
    }
}

#[derive(Default)]
struct Track {
    video: bool,
    flags: u32,
    id: u32,
    codec: Option<Codec>,
    description: bool,
    width: u64,
    height: u64,
    track_width: u32,
    track_height: u32,
    pasp: Option<AspectRatio>,
    clap: Option<AspectRatio>,
    clef: Option<AspectRatio>,
    matrix: Matrix,
    private: Option<Span>,
    chunk: Option<u64>,
    sample_size: Option<u64>,
}

impl Track {
    fn rank(&self) -> u8 {
        if self.flags & 3 == 3 {
            0
        } else if self.flags & 1 != 0 {
            1
        } else {
            2
        }
    }

    fn has_geometry(&self) -> bool {
        (self.width != 0 && self.height != 0) || (self.track_width != 0 && self.track_height != 0)
    }
}

pub(crate) fn probe(source: &mut Source<'_>, detected: VideoType) -> VideoResult<VideoInfo> {
    let file_end = source.len();
    let mut kind = detected;
    let mut tracks = Vec::new();
    let mut movie_matrix = Matrix::default();
    source.seek(0);
    while let Some(atom) = next_atom(source, file_end)? {
        match &atom.kind {
            b"ftyp" => {
                let size = usize::try_from((atom.end - atom.data).min(4096)).unwrap_or(4096);
                let bytes = source.view(atom.data, size)?;
                kind = if bytes.chunks_exact(4).any(|brand| brand == b"qt  ") {
                    VideoType::Mov
                } else {
                    VideoType::Mp4
                };
            }
            b"moov" => {
                (tracks, movie_matrix) = parse_moov(source, atom)?;
            }
            _ => {}
        }
        source.seek(atom.end);
    }

    for wanted in 0..3 {
        for track in &tracks {
            if !track.video || track.rank() != wanted {
                continue;
            }
            let geometry = codec_geometry(source, file_end, track)?;
            if track.has_geometry() || geometry.is_some() {
                return finish(kind, track, movie_matrix, geometry);
            }
        }
    }
    invalid()
}

fn parse_moov(source: &mut Source<'_>, moov: Atom) -> VideoResult<(Vec<Track>, Matrix)> {
    let mut tracks = Vec::new();
    let mut matrix = Matrix::default();
    source.seek(moov.data);
    while let Some(atom) = next_atom(source, moov.end)? {
        match &atom.kind {
            b"mvhd" => matrix = matrix_at(source, atom, true)?,
            b"trak" => {
                source.track()?;
                tracks.push(parse_track(source, atom)?);
            }
            _ => {}
        }
        source.seek(atom.end);
    }
    Ok((tracks, matrix))
}

fn parse_track(source: &mut Source<'_>, trak: Atom) -> VideoResult<Track> {
    let mut track = Track::default();
    source.seek(trak.data);
    while let Some(atom) = next_atom(source, trak.end)? {
        match &atom.kind {
            b"tkhd" => parse_tkhd(source, atom, &mut track)?,
            b"tapt" => parse_tapt(source, atom, &mut track)?,
            b"mdia" => scan_track(source, atom, &mut track, 0)?,
            _ => {}
        }
        source.seek(atom.end);
    }
    Ok(track)
}

/// Walks a media box and the sample table below it.
///
/// `depth` counts levels below the `mdia` the walk started at, which both bounds
/// recursion and identifies the media handler: a `hdlr` nested any deeper
/// describes data references rather than the track's media type.
fn scan_track(
    source: &mut Source<'_>,
    parent: Atom,
    track: &mut Track,
    depth: u32,
) -> VideoResult<()> {
    if depth > MAX_NESTING {
        return Err(VideoError::LimitExceeded);
    }
    source.seek(parent.data);
    while let Some(atom) = next_atom(source, parent.end)? {
        match &atom.kind {
            b"minf" | b"stbl" => scan_track(source, atom, track, depth + 1)?,
            b"hdlr" if depth == 0 && atom.end - atom.data >= 12 => {
                let bytes = source.view(atom.data + 8, 4)?;
                track.video = bytes == b"vide";
            }
            b"stsd" => parse_stsd(source, atom, track)?,
            b"stsz" => parse_stsz(source, atom, track)?,
            b"stz2" => parse_stz2(source, atom, track)?,
            b"stco" => parse_stco(source, atom, track, false)?,
            b"co64" => parse_stco(source, atom, track, true)?,
            _ => {}
        }
        source.seek(atom.end);
    }
    Ok(())
}

fn parse_stsd(source: &mut Source<'_>, atom: Atom, track: &mut Track) -> VideoResult<()> {
    if atom.end - atom.data < 8 {
        return invalid();
    }
    let count = be(source.view(atom.data + 4, 4)?, 0, 4).unwrap_or(0) as usize;
    source.sample_descriptions(count)?;
    source.seek(atom.data + 8);
    for _ in 0..count {
        let entry = next_atom(source, atom.end)?.ok_or(VideoError::CorruptedVideo)?;
        if !track.description && entry.end - entry.data >= 28 {
            let bytes = source.view(entry.data + 24, 4)?;
            let width = be(bytes, 0, 2).unwrap_or(0);
            let height = be(bytes, 2, 2).unwrap_or(0);
            let codec = codecs::from_id(&entry.kind);
            if (width != 0 && height != 0) || codec.is_some() {
                track.description = true;
                track.width = width;
                track.height = height;
                track.codec = codec;
                parse_visual_extensions(source, entry, track)?;
            }
        }
        source.seek(entry.end);
    }
    Ok(())
}

fn parse_visual_extensions(
    source: &mut Source<'_>,
    entry: Atom,
    track: &mut Track,
) -> VideoResult<()> {
    let start = entry.data + 78;
    if start > entry.end {
        return Ok(());
    }
    source.seek(start);
    while let Some(atom) = next_atom(source, entry.end)? {
        match &atom.kind {
            b"pasp" if atom.end - atom.data >= 8 => {
                let bytes = source.view(atom.data, 8)?;
                let width = be(bytes, 0, 4).unwrap_or(0);
                let height = be(bytes, 4, 4).unwrap_or(0);
                if width != 0 && height != 0 {
                    track.pasp = Some(AspectRatio::new(width, height));
                }
            }
            b"clap" if atom.end - atom.data >= 16 => {
                let bytes = source.view(atom.data, 16)?;
                let wn = be(bytes, 0, 4).unwrap_or(0);
                let wd = be(bytes, 4, 4).unwrap_or(0);
                let hn = be(bytes, 8, 4).unwrap_or(0);
                let hd = be(bytes, 12, 4).unwrap_or(0);
                if wn != 0 && wd != 0 && hn != 0 && hd != 0 {
                    track.clap = Some(ratio_from_u128(
                        wn as u128 * hd as u128,
                        wd as u128 * hn as u128,
                    ));
                }
            }
            b"avcC" | b"hvcC" | b"av1C" | b"vpcC" => {
                track.private = Some(Span {
                    position: atom.data,
                    size: atom.end - atom.data,
                });
            }
            _ => {}
        }
        source.seek(atom.end);
    }
    Ok(())
}

fn parse_tkhd(source: &mut Source<'_>, atom: Atom, track: &mut Track) -> VideoResult<()> {
    if atom.end - atom.data < 4 {
        return Ok(());
    }
    let version = source.view(atom.data, 4)?;
    track.flags = be(version, 1, 3).unwrap_or(0) as u32;
    let (id, matrix, dimensions, required) = if version[0] == 1 {
        (20, 52, 88, 96)
    } else {
        (12, 40, 76, 84)
    };
    if atom.end - atom.data < required {
        return Ok(());
    }
    let bytes = source.view(atom.data, required as usize)?;
    track.id = be(bytes, id, 4).unwrap_or(0) as u32;
    track.matrix = matrix_bytes(bytes, matrix);
    track.track_width = be(bytes, dimensions, 4).unwrap_or(0) as u32;
    track.track_height = be(bytes, dimensions + 4, 4).unwrap_or(0) as u32;
    Ok(())
}

fn parse_tapt(source: &mut Source<'_>, tapt: Atom, track: &mut Track) -> VideoResult<()> {
    source.seek(tapt.data);
    while let Some(atom) = next_atom(source, tapt.end)? {
        if atom.kind == *b"clef" && atom.end - atom.data >= 12 {
            let bytes = source.view(atom.data + 4, 8)?;
            let width = be(bytes, 0, 4).unwrap_or(0);
            let height = be(bytes, 4, 4).unwrap_or(0);
            if width != 0 && height != 0 {
                track.clef = Some(AspectRatio::new(width, height));
            }
        }
        source.seek(atom.end);
    }
    Ok(())
}

fn parse_stsz(source: &mut Source<'_>, atom: Atom, track: &mut Track) -> VideoResult<()> {
    if atom.end - atom.data < 12 {
        return invalid();
    }
    let size = usize::try_from((atom.end - atom.data).min(16)).unwrap_or(16);
    let bytes = source.view(atom.data, size)?;
    let default = be(bytes, 4, 4).unwrap_or(0);
    let count = be(bytes, 8, 4).unwrap_or(0);
    if count != 0 {
        track.sample_size = if default != 0 {
            Some(default)
        } else {
            be(bytes, 12, 4)
        };
    }
    Ok(())
}

fn parse_stz2(source: &mut Source<'_>, atom: Atom, track: &mut Track) -> VideoResult<()> {
    if atom.end - atom.data < 13 {
        return invalid();
    }
    let bytes = source.view(
        atom.data,
        usize::try_from((atom.end - atom.data).min(14)).unwrap_or(14),
    )?;
    if be(bytes, 8, 4).unwrap_or(0) == 0 {
        return Ok(());
    }
    track.sample_size = match bytes[7] {
        4 => Some((bytes[12] >> 4) as u64),
        8 => Some(bytes[12] as u64),
        16 if bytes.len() >= 14 => be(bytes, 12, 2),
        _ => None,
    };
    Ok(())
}

fn parse_stco(
    source: &mut Source<'_>,
    atom: Atom,
    track: &mut Track,
    wide: bool,
) -> VideoResult<()> {
    let needed = if wide { 16 } else { 12 };
    if atom.end - atom.data < needed {
        return invalid();
    }
    let bytes = source.view(atom.data, needed as usize)?;
    if be(bytes, 4, 4).unwrap_or(0) != 0 {
        track.chunk = be(bytes, 8, if wide { 8 } else { 4 });
    }
    Ok(())
}

fn codec_geometry(
    source: &mut Source<'_>,
    file_end: u64,
    track: &Track,
) -> VideoResult<Option<CodecGeometry>> {
    let Some(codec) = track.codec else {
        return Ok(None);
    };
    let sample = match (track.chunk, track.sample_size) {
        (Some(position), Some(size)) => Some(Span { position, size }),
        _ => find_fragment(source, file_end, track.id)?,
    };
    source.geometry(codec, track.private, sample)
}

fn find_fragment(source: &mut Source<'_>, file_end: u64, track: u32) -> VideoResult<Option<Span>> {
    if track == 0 {
        return Ok(None);
    }
    source.seek(0);
    while let Some(moof) = next_atom(source, file_end)? {
        if moof.kind == *b"moof" {
            source.seek(moof.data);
            while let Some(traf) = next_atom(source, moof.end)? {
                if traf.kind == *b"traf"
                    && let Some(sample) = parse_traf(source, traf, moof, track)?
                {
                    return Ok(Some(sample));
                }
                source.seek(traf.end);
            }
        }
        source.seek(moof.end);
    }
    Ok(None)
}

fn parse_traf(
    source: &mut Source<'_>,
    traf: Atom,
    moof: Atom,
    selected: u32,
) -> VideoResult<Option<Span>> {
    let mut track = 0;
    let mut base = moof.start;
    let mut explicit_base = false;
    let mut default_size = 0;
    let mut first_trun = None;
    source.seek(traf.data);
    while let Some(atom) = next_atom(source, traf.end)? {
        if atom.kind == *b"tfhd" {
            let size = usize::try_from((atom.end - atom.data).min(40)).unwrap_or(40);
            let bytes = source.view(atom.data, size)?;
            let flags = be(bytes, 1, 3).unwrap_or(0) as u32;
            track = be(bytes, 4, 4).unwrap_or(0) as u32;
            let mut offset = 8;
            if flags & 1 != 0 {
                base = be(bytes, offset, 8).unwrap_or(base);
                explicit_base = true;
                offset += 8;
            }
            if flags & 2 != 0 {
                offset += 4;
            }
            if flags & 8 != 0 {
                offset += 4;
            }
            if flags & 0x10 != 0 {
                default_size = be(bytes, offset, 4).unwrap_or(0);
            }
        } else if atom.kind == *b"trun" && first_trun.is_none() {
            first_trun = Some(atom);
        }
        source.seek(atom.end);
    }
    if track != selected {
        return Ok(None);
    }
    let Some(trun) = first_trun else {
        return Ok(None);
    };
    let size = usize::try_from((trun.end - trun.data).min(40)).unwrap_or(40);
    let bytes = source.view(trun.data, size)?;
    let flags = be(bytes, 1, 3).unwrap_or(0) as u32;
    if be(bytes, 4, 4).unwrap_or(0) == 0 {
        return Ok(None);
    }
    let mut offset = 8;
    let data_offset = if flags & 1 != 0 {
        let value = be(bytes, offset, 4).unwrap_or(0) as u32 as i32;
        offset += 4;
        Some(value)
    } else {
        None
    };
    if flags & 4 != 0 {
        offset += 4;
    }
    if flags & 0x100 != 0 {
        offset += 4;
    }
    let sample_size = if flags & 0x200 != 0 {
        be(bytes, offset, 4).unwrap_or(0)
    } else {
        default_size
    };
    if sample_size == 0 {
        return Ok(None);
    }
    let position = match data_offset {
        Some(value) if value < 0 => base.checked_sub(value.unsigned_abs() as u64),
        Some(value) => base.checked_add(value as u64),
        None if explicit_base => Some(base),
        None => moof.end.checked_add(8),
    }
    .ok_or(VideoError::CorruptedVideo)?;
    Ok(Some(Span {
        position,
        size: sample_size,
    }))
}

fn finish(
    kind: VideoType,
    track: &Track,
    movie: Matrix,
    geometry: Option<CodecGeometry>,
) -> VideoResult<VideoInfo> {
    let codec_width = geometry.map_or(0, |value| value.coded_width);
    let codec_height = geometry.map_or(0, |value| value.coded_height);
    let width = if track.width != 0 {
        track.width
    } else if codec_width != 0 {
        codec_width
    } else {
        fixed(track.track_width)
    };
    let height = if track.height != 0 {
        track.height
    } else if codec_height != 0 {
        codec_height
    } else {
        fixed(track.track_height)
    };
    if width == 0 || height == 0 {
        return Err(VideoError::CorruptedVideo);
    }
    let display = geometry
        .and_then(CodecGeometry::display_dimensions)
        .unwrap_or((width, height));
    let pixel = track
        .pasp
        .or_else(|| geometry.and_then(|value| value.pixel_aspect_ratio))
        .unwrap_or_else(AspectRatio::square);
    let ratio = if let Some(clef) = track.clef {
        clef
    } else if let Some(clap) = track.clap {
        clap.multiply_ratio(pixel)
    } else if track.pasp.is_some() {
        AspectRatio::new(display.0, display.1).multiply_ratio(pixel)
    } else if track.track_width != 0 && track.track_height != 0 {
        AspectRatio::new(track.track_width as u64, track.track_height as u64)
    } else {
        AspectRatio::new(display.0, display.1).multiply_ratio(pixel)
    };
    let (ratio, rotation) = movie.compose(track.matrix).presentation(ratio);
    make_info(kind, track.codec, width, height, pixel, ratio, rotation)
}

fn matrix_at(source: &mut Source<'_>, atom: Atom, movie: bool) -> VideoResult<Matrix> {
    if atom.end - atom.data < 4 {
        return Ok(Matrix::default());
    }
    let version = source.view(atom.data, 1)?;
    let offset = match (movie, version[0]) {
        (true, 1) => 48,
        (true, _) => 36,
        (false, 1) => 52,
        (false, _) => 40,
    };
    if atom.end - atom.data < offset + 20 {
        return Ok(Matrix::default());
    }
    let bytes = source.view(atom.data + offset, 20)?;
    Ok(matrix_bytes(bytes, 0))
}

fn matrix_bytes(bytes: &[u8], offset: usize) -> Matrix {
    let signed = |at| be(bytes, offset + at, 4).unwrap_or(0) as u32 as i32 as i128;
    Matrix {
        a: signed(0),
        b: signed(4),
        c: signed(12),
        d: signed(16),
    }
}

fn fixed(value: u32) -> u64 {
    (value as u64 + 0x8000) >> 16
}

fn next_atom(source: &mut Source<'_>, parent_end: u64) -> VideoResult<Option<Atom>> {
    let start = source.position();
    if start == parent_end {
        return Ok(None);
    }
    if start > parent_end || parent_end - start < 8 {
        return invalid();
    }
    source.element()?;
    let head = source.view(start, 8)?;
    let short_size = be(head, 0, 4).unwrap_or(0);
    let kind = [head[4], head[5], head[6], head[7]];
    let (size, header) = match short_size {
        0 => (parent_end - start, 8),
        1 if parent_end - start >= 16 => (be(source.view(start + 8, 8)?, 0, 8).unwrap_or(0), 16),
        1 => return invalid(),
        value => (value, 8),
    };
    let end = start.checked_add(size).ok_or(VideoError::CorruptedVideo)?;
    if size < header || end > parent_end {
        return invalid();
    }
    Ok(Some(Atom {
        start,
        data: start + header,
        end,
        kind,
    }))
}
