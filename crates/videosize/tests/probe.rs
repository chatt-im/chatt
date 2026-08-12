use std::fs;
use std::io::{Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};

use videosize::{
    AspectRatio, Codec, VideoError, VideoInfo, VideoSize, VideoType, blob_probe, blob_size,
    file_type, probe, video_type,
};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

fn file_for(data: &[u8]) -> (std::path::PathBuf, std::fs::File) {
    let path = std::env::temp_dir().join(format!(
        "videosize-integration-{}-{}",
        std::process::id(),
        NEXT_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&path, data).unwrap();
    let mut file = std::fs::File::open(&path).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    (path, file)
}

fn probe_both(data: &[u8]) -> VideoInfo {
    let memory = blob_probe(data).unwrap();
    let (path, file) = file_for(data);
    let from_file = probe(file).unwrap();
    fs::remove_file(path).unwrap();
    assert_eq!(from_file, memory);
    memory
}

fn assert_limit_both(data: &[u8]) {
    assert!(matches!(blob_probe(data), Err(VideoError::LimitExceeded)));
    let (path, file) = file_for(data);
    assert!(matches!(probe(file), Err(VideoError::LimitExceeded)));
    fs::remove_file(path).unwrap();
}

fn be_box(kind: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut output = Vec::with_capacity(payload.len() + 8);
    output.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend(payload);
    output
}

fn mp4(tag: &[u8; 4], mov: bool, rotation: u16, pasp: Option<(u32, u32)>) -> Vec<u8> {
    let mut ftyp = if mov {
        b"qt  ".to_vec()
    } else {
        b"isom".to_vec()
    };
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(if mov { b"qt  " } else { b"isom" });

    let mut tkhd = vec![0; 84];
    let (a, b, c, d): (i32, i32, i32, i32) = match rotation {
        90 => (0, 1 << 16, -(1 << 16), 0),
        180 => (-(1 << 16), 0, 0, -(1 << 16)),
        270 => (0, -(1 << 16), 1 << 16, 0),
        _ => (1 << 16, 0, 0, 1 << 16),
    };
    tkhd[40..44].copy_from_slice(&a.to_be_bytes());
    tkhd[44..48].copy_from_slice(&b.to_be_bytes());
    tkhd[52..56].copy_from_slice(&c.to_be_bytes());
    tkhd[56..60].copy_from_slice(&d.to_be_bytes());
    tkhd[76..80].copy_from_slice(&(640u32 << 16).to_be_bytes());
    tkhd[80..84].copy_from_slice(&(360u32 << 16).to_be_bytes());

    let mut sample = vec![0; 78];
    sample[24..26].copy_from_slice(&640u16.to_be_bytes());
    sample[26..28].copy_from_slice(&360u16.to_be_bytes());
    if let Some((horizontal, vertical)) = pasp {
        let mut value = horizontal.to_be_bytes().to_vec();
        value.extend_from_slice(&vertical.to_be_bytes());
        sample.extend(be_box(b"pasp", value));
    }
    let entry = be_box(tag, sample);
    let mut stsd = vec![0; 4];
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend(entry);
    let stbl = be_box(b"stbl", be_box(b"stsd", stsd));
    let mut minf = Vec::new();
    if mov {
        let mut data_handler = vec![0; 12];
        data_handler[4..8].copy_from_slice(b"dhlr");
        data_handler[8..12].copy_from_slice(b"alis");
        minf.extend(be_box(b"hdlr", data_handler));
    }
    minf.extend(stbl);
    let minf = be_box(b"minf", minf);
    let mut hdlr = vec![0; 12];
    if mov {
        hdlr[4..8].copy_from_slice(b"mhlr");
    }
    hdlr[8..12].copy_from_slice(b"vide");
    let mut mdia = be_box(b"hdlr", hdlr);
    mdia.extend(minf);
    let mut trak = be_box(b"tkhd", tkhd);
    trak.extend(be_box(b"mdia", mdia));
    let mut output = be_box(b"ftyp", ftyp);
    output.extend(be_box(b"moov", be_box(b"trak", trak)));
    output
}

fn matrix(rotation: u16) -> (i32, i32, i32, i32) {
    match rotation {
        90 => (0, 1 << 16, -(1 << 16), 0),
        180 => (-(1 << 16), 0, 0, -(1 << 16)),
        270 => (0, -(1 << 16), 1 << 16, 0),
        _ => (1 << 16, 0, 0, 1 << 16),
    }
}

fn put_matrix(data: &mut [u8], offset: usize, rotation: u16) {
    let (a, b, c, d) = matrix(rotation);
    data[offset..offset + 4].copy_from_slice(&a.to_be_bytes());
    data[offset + 4..offset + 8].copy_from_slice(&b.to_be_bytes());
    data[offset + 12..offset + 16].copy_from_slice(&c.to_be_bytes());
    data[offset + 16..offset + 20].copy_from_slice(&d.to_be_bytes());
}

#[allow(clippy::too_many_arguments)]
fn mp4_track_box(
    track_id: u32,
    tag: &[u8; 4],
    width: u16,
    height: u16,
    flags: u32,
    rotation: u16,
    visual_extensions: Vec<u8>,
    table_extensions: Vec<u8>,
    tapt: Option<(u32, u32)>,
) -> Vec<u8> {
    let mut tkhd = vec![0; 84];
    tkhd[1] = ((flags >> 16) & 0xff) as u8;
    tkhd[2] = ((flags >> 8) & 0xff) as u8;
    tkhd[3] = (flags & 0xff) as u8;
    tkhd[12..16].copy_from_slice(&track_id.to_be_bytes());
    put_matrix(&mut tkhd, 40, rotation);
    tkhd[76..80].copy_from_slice(&((width as u32) << 16).to_be_bytes());
    tkhd[80..84].copy_from_slice(&((height as u32) << 16).to_be_bytes());

    let mut sample = vec![0; 78];
    sample[24..26].copy_from_slice(&width.to_be_bytes());
    sample[26..28].copy_from_slice(&height.to_be_bytes());
    sample.extend(visual_extensions);
    let entry = be_box(tag, sample);
    let mut stsd = vec![0; 4];
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend(entry);
    let mut stbl_data = be_box(b"stsd", stsd);
    stbl_data.extend(table_extensions);
    let stbl = be_box(b"stbl", stbl_data);
    let minf = be_box(b"minf", stbl);
    let mut hdlr = vec![0; 12];
    hdlr[8..12].copy_from_slice(b"vide");
    let mut mdia = be_box(b"hdlr", hdlr);
    mdia.extend(minf);
    let mut trak = be_box(b"tkhd", tkhd);
    if let Some((display_width, display_height)) = tapt {
        let mut clef = vec![0; 4];
        clef.extend_from_slice(&display_width.to_be_bytes());
        clef.extend_from_slice(&display_height.to_be_bytes());
        trak.extend(be_box(b"tapt", be_box(b"clef", clef)));
    }
    trak.extend(be_box(b"mdia", mdia));
    be_box(b"trak", trak)
}

fn mp4_with_tracks(tracks: Vec<Vec<u8>>, movie_rotation: u16) -> Vec<u8> {
    let mut ftyp = b"isom".to_vec();
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"isom");
    let mut mvhd = vec![0; 72];
    put_matrix(&mut mvhd, 36, movie_rotation);
    let mut moov = be_box(b"mvhd", mvhd);
    for track in tracks {
        moov.extend(track);
    }
    let mut output = be_box(b"ftyp", ftyp);
    output.extend(be_box(b"moov", moov));
    output
}

fn regular_sample_tables(offset: u32, size: u32) -> Vec<u8> {
    let mut stco = vec![0; 4];
    stco.extend_from_slice(&1u32.to_be_bytes());
    stco.extend_from_slice(&offset.to_be_bytes());
    let mut stsz = vec![0; 4];
    stsz.extend_from_slice(&0u32.to_be_bytes());
    stsz.extend_from_slice(&1u32.to_be_bytes());
    stsz.extend_from_slice(&size.to_be_bytes());
    let mut output = be_box(b"stco", stco);
    output.extend(be_box(b"stsz", stsz));
    output
}

fn vint_size(size: usize) -> Vec<u8> {
    if size < 0x7f {
        vec![0x80 | size as u8]
    } else if size < 0x3fff {
        vec![0x40 | ((size >> 8) as u8), size as u8]
    } else {
        panic!("test element too large")
    }
}

fn ebml_element(id: &[u8], data: Vec<u8>) -> Vec<u8> {
    let mut output = id.to_vec();
    output.extend(vint_size(data.len()));
    output.extend(data);
    output
}

fn ebml_uint(id: &[u8], value: u16) -> Vec<u8> {
    if value <= u8::MAX as u16 {
        ebml_element(id, vec![value as u8])
    } else {
        ebml_element(id, value.to_be_bytes().to_vec())
    }
}

fn matroska(codec: &str, webm: bool) -> Vec<u8> {
    let doc_type = if webm {
        b"webm".to_vec()
    } else {
        b"matroska".to_vec()
    };
    let header = ebml_element(
        &[0x1a, 0x45, 0xdf, 0xa3],
        ebml_element(&[0x42, 0x82], doc_type),
    );
    let mut video = ebml_uint(&[0xb0], 720);
    video.extend(ebml_uint(&[0xba], 576));
    video.extend(ebml_uint(&[0x54, 0xb0], 1024));
    video.extend(ebml_uint(&[0x54, 0xba], 576));
    let mut track = ebml_uint(&[0x83], 1);
    track.extend(ebml_uint(&[0xd7], 1));
    track.extend(ebml_element(&[0x86], codec.as_bytes().to_vec()));
    track.extend(ebml_element(&[0xe0], video));
    let tracks = ebml_element(&[0x16, 0x54, 0xae, 0x6b], ebml_element(&[0xae], track));
    let mut output = header;
    output.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xff]);
    output.extend(tracks);
    output
}

fn matroska_with_tracks(tracks: Vec<Vec<u8>>, cluster: Option<Vec<u8>>) -> Vec<u8> {
    let header = ebml_element(
        &[0x1a, 0x45, 0xdf, 0xa3],
        ebml_element(&[0x42, 0x82], b"matroska".to_vec()),
    );
    let mut entries = Vec::new();
    for track in tracks {
        entries.extend(ebml_element(&[0xae], track));
    }
    let mut output = header;
    output.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xff]);
    output.extend(ebml_element(&[0x16, 0x54, 0xae, 0x6b], entries));
    if let Some(cluster) = cluster {
        output.extend(ebml_element(&[0x1f, 0x43, 0xb6, 0x75], cluster));
    }
    output
}

fn matroska_track(
    number: u16,
    width: u16,
    height: u16,
    enabled: bool,
    default: bool,
    video_extra: Vec<u8>,
) -> Vec<u8> {
    let mut video = ebml_uint(&[0xb0], width);
    video.extend(ebml_uint(&[0xba], height));
    video.extend(video_extra);
    let mut track = ebml_uint(&[0xd7], number);
    track.extend(ebml_uint(&[0x83], 1));
    track.extend(ebml_uint(&[0xb9], u16::from(enabled)));
    track.extend(ebml_uint(&[0x88], u16::from(default)));
    track.extend(ebml_element(&[0x86], b"V_VP8".to_vec()));
    track.extend(ebml_element(&[0xe0], video));
    track
}

fn le_chunk(id: &[u8; 4], mut payload: Vec<u8>) -> Vec<u8> {
    let size = payload.len();
    let mut output = id.to_vec();
    output.extend_from_slice(&(size as u32).to_le_bytes());
    output.append(&mut payload);
    if size & 1 != 0 {
        output.push(0);
    }
    output
}

fn list(kind: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut data = kind.to_vec();
    data.extend(payload);
    le_chunk(b"LIST", data)
}

fn avi(tag: &[u8; 4], with_stream_size: bool) -> Vec<u8> {
    let mut avih = vec![0; 56];
    avih[32..36].copy_from_slice(&320u32.to_le_bytes());
    avih[36..40].copy_from_slice(&180u32.to_le_bytes());
    let mut strh = vec![0; 56];
    strh[..4].copy_from_slice(b"vids");
    strh[4..8].copy_from_slice(tag);
    strh[52..54].copy_from_slice(&320i16.to_le_bytes());
    strh[54..56].copy_from_slice(&180i16.to_le_bytes());
    let mut strf = vec![0; 40];
    strf[..4].copy_from_slice(&40u32.to_le_bytes());
    if with_stream_size {
        strf[4..8].copy_from_slice(&320i32.to_le_bytes());
        strf[8..12].copy_from_slice(&(-180i32).to_le_bytes());
    }
    strf[16..20].copy_from_slice(tag);
    let mut vprp = vec![0; 36];
    vprp[20..22].copy_from_slice(&9u16.to_le_bytes());
    vprp[22..24].copy_from_slice(&16u16.to_le_bytes());
    vprp[24..28].copy_from_slice(&320u32.to_le_bytes());
    vprp[28..32].copy_from_slice(&180u32.to_le_bytes());
    let mut strl = le_chunk(b"strh", strh);
    strl.extend(le_chunk(b"strf", strf));
    strl.extend(le_chunk(b"vprp", vprp));
    let mut hdrl = le_chunk(b"avih", avih);
    hdrl.extend(list(b"strl", strl));
    let payload = list(b"hdrl", hdrl);
    let mut output = b"RIFF".to_vec();
    output.extend_from_slice(&((payload.len() + 4) as u32).to_le_bytes());
    output.extend_from_slice(b"AVI ");
    output.extend(payload);
    output
}

fn avi_video_stream(
    tag: &[u8; 4],
    width: i32,
    height: i32,
    disabled: bool,
    with_vprp: bool,
) -> Vec<u8> {
    let mut strh = vec![0; 56];
    strh[..4].copy_from_slice(b"vids");
    strh[4..8].copy_from_slice(tag);
    strh[8..12].copy_from_slice(&u32::from(disabled).to_le_bytes());
    strh[52..54].copy_from_slice(&(width.max(0) as i16).to_le_bytes());
    strh[54..56].copy_from_slice(&(height.max(0) as i16).to_le_bytes());
    let mut strf = vec![0; 40];
    strf[..4].copy_from_slice(&40u32.to_le_bytes());
    strf[4..8].copy_from_slice(&width.to_le_bytes());
    strf[8..12].copy_from_slice(&height.to_le_bytes());
    strf[16..20].copy_from_slice(tag);
    let mut stream = le_chunk(b"strh", strh);
    stream.extend(le_chunk(b"strf", strf));
    if with_vprp {
        let mut vprp = vec![0; 36];
        vprp[20..22].copy_from_slice(&9u16.to_le_bytes());
        vprp[22..24].copy_from_slice(&16u16.to_le_bytes());
        vprp[24..28].copy_from_slice(&width.unsigned_abs().to_le_bytes());
        vprp[28..32].copy_from_slice(&height.unsigned_abs().to_le_bytes());
        stream.extend(le_chunk(b"vprp", vprp));
    }
    list(b"strl", stream)
}

fn avi_audio_stream() -> Vec<u8> {
    let mut strh = vec![0; 56];
    strh[..4].copy_from_slice(b"auds");
    list(b"strl", le_chunk(b"strh", strh))
}

fn avi_custom(streams: Vec<Vec<u8>>, global: (u32, u32), movi: Option<Vec<u8>>) -> Vec<u8> {
    let mut avih = vec![0; 56];
    avih[32..36].copy_from_slice(&global.0.to_le_bytes());
    avih[36..40].copy_from_slice(&global.1.to_le_bytes());
    let mut hdrl = le_chunk(b"avih", avih);
    for stream in streams {
        hdrl.extend(stream);
    }
    let mut payload = list(b"hdrl", hdrl);
    if let Some(movi) = movi {
        payload.extend(list(b"movi", movi));
    }
    let mut output = b"RIFF".to_vec();
    output.extend_from_slice(&((payload.len() + 4) as u32).to_le_bytes());
    output.extend_from_slice(b"AVI ");
    output.extend(payload);
    output
}

#[test]
fn probes_all_mp4_codec_tags() {
    let cases = [
        (b"avc1", Codec::H264),
        (b"hvc1", Codec::H265),
        (b"av01", Codec::Av1),
        (b"vp09", Codec::Vp9),
        (b"vp08", Codec::Vp8),
    ];
    for (tag, codec) in cases {
        let info = probe_both(&mp4(tag, false, 0, None));
        assert_eq!(
            info.size,
            VideoSize {
                width: 640,
                height: 360
            }
        );
        assert_eq!(info.video_type, VideoType::Mp4);
        assert_eq!(info.codec, Some(codec));
        assert_eq!(info.display_size, VideoSize { width: 640, height: 360 });
        assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
    }
}

#[test]
fn mp4_clap_tapt_and_composed_transforms_are_applied() {
    let mut clap = Vec::new();
    for value in [1279u32, 2, 719, 2, 0, 1, 0, 1] {
        clap.extend_from_slice(&value.to_be_bytes());
    }
    let track = mp4_track_box(
        1,
        b"avc1",
        720,
        480,
        3,
        0,
        be_box(b"clap", clap),
        Vec::new(),
        None,
    );
    let info = probe_both(&mp4_with_tracks(vec![track], 0));
    assert_eq!(
        info.size,
        VideoSize {
            width: 720,
            height: 480
        }
    );
    assert_eq!(info.aspect_ratio(), AspectRatio::new(1279, 719));
    assert_eq!(info.display_size, VideoSize { width: 640, height: 360 });

    let track = mp4_track_box(
        1,
        b"avc1",
        720,
        480,
        3,
        0,
        Vec::new(),
        Vec::new(),
        Some((1024 << 16, 576 << 16)),
    );
    let info = probe_both(&mp4_with_tracks(vec![track], 0));
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
    assert_eq!(info.display_size, VideoSize { width: 1024, height: 576 });

    let track = mp4_track_box(1, b"avc1", 640, 360, 3, 90, Vec::new(), Vec::new(), None);
    let info = probe_both(&mp4_with_tracks(vec![track], 90));
    assert_eq!(info.rotation, 180);
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
}

#[test]
fn mp4_extended_atoms_and_enabled_track_selection() {
    let disabled = mp4_track_box(1, b"vp08", 320, 240, 0, 0, Vec::new(), Vec::new(), None);
    let enabled = mp4_track_box(2, b"xxxx", 640, 360, 3, 0, Vec::new(), Vec::new(), None);
    let mut data = mp4_with_tracks(vec![disabled, enabled], 0);
    let short_size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
    let ftyp_payload = data[8..short_size].to_vec();
    let mut extended = 1u32.to_be_bytes().to_vec();
    extended.extend_from_slice(b"ftyp");
    extended.extend_from_slice(&((ftyp_payload.len() + 16) as u64).to_be_bytes());
    extended.extend(ftyp_payload);
    extended.extend_from_slice(&data[short_size..]);
    data = extended;
    let info = probe_both(&data);
    assert_eq!(
        info.size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
    assert_eq!(info.codec, None);
}

#[test]
fn mp4_regular_and_fragmented_first_samples_supply_geometry() {
    let frame = vp8_keyframe(640, 360);
    let preliminary_track = mp4_track_box(
        1,
        b"vp08",
        0,
        0,
        3,
        0,
        Vec::new(),
        regular_sample_tables(0, frame.len() as u32),
        None,
    );
    let preliminary = mp4_with_tracks(vec![preliminary_track], 0);
    let offset = preliminary.len() as u32 + 8;
    let track = mp4_track_box(
        1,
        b"vp08",
        0,
        0,
        3,
        0,
        Vec::new(),
        regular_sample_tables(offset, frame.len() as u32),
        None,
    );
    let mut regular = mp4_with_tracks(vec![track], 0);
    regular.extend(be_box(b"mdat", frame.clone()));
    let info = probe_both(&regular);
    assert_eq!(
        info.size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));

    let track = mp4_track_box(1, b"vp08", 0, 0, 3, 0, Vec::new(), Vec::new(), None);
    let mut fragmented = mp4_with_tracks(vec![track], 0);
    let mut tfhd = vec![0, 0, 0, 0x10];
    tfhd.extend_from_slice(&1u32.to_be_bytes());
    tfhd.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    let mut trun = vec![0, 0, 0x02, 0x01];
    trun.extend_from_slice(&1u32.to_be_bytes());
    trun.extend_from_slice(&0i32.to_be_bytes());
    trun.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    let preliminary_moof = be_box(
        b"moof",
        be_box(
            b"traf",
            [be_box(b"tfhd", tfhd.clone()), be_box(b"trun", trun.clone())].concat(),
        ),
    );
    let data_offset = preliminary_moof.len() as i32 + 8;
    trun[8..12].copy_from_slice(&data_offset.to_be_bytes());
    let moof = be_box(
        b"moof",
        be_box(
            b"traf",
            [be_box(b"tfhd", tfhd), be_box(b"trun", trun)].concat(),
        ),
    );
    fragmented.extend(moof);
    fragmented.extend(be_box(b"mdat", frame));
    let info = probe_both(&fragmented);
    assert_eq!(
        info.size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
}

#[test]
fn mov_rotation_and_pixel_aspect_are_applied_to_display_ratio() {
    let data = mp4(b"avc1", true, 90, Some((4, 3)));
    assert!(matches!(video_type(&data), Ok(VideoType::Mov)));
    let info = probe_both(&data);
    assert_eq!(info.rotation, 90);
    assert_eq!(info.pixel_aspect_ratio, AspectRatio::new(4, 3));
    assert_eq!(info.display_size, VideoSize { width: 360, height: 853 });
    assert_eq!(info.aspect_ratio(), AspectRatio::new(27, 64));
}

#[test]
fn mov_unknown_prores_entry_uses_container_geometry() {
    let info = probe_both(&mp4(b"apch", true, 0, None));
    assert_eq!(
        info.size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
    assert_eq!(info.video_type, VideoType::Mov);
    assert_eq!(info.codec, None);
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
}

#[test]
fn probes_matroska_and_webm_codecs_and_display_size() {
    let cases = [
        ("V_MPEG4/ISO/AVC", Codec::H264),
        ("V_MPEGH/ISO/HEVC", Codec::H265),
        ("V_AV1", Codec::Av1),
        ("V_VP9", Codec::Vp9),
        ("V_VP8", Codec::Vp8),
    ];
    for (id, codec) in cases {
        let info = probe_both(&matroska(id, false));
        assert_eq!(
            info.size,
            VideoSize {
                width: 720,
                height: 576
            }
        );
        assert_eq!(info.video_type, VideoType::Matroska);
        assert_eq!(info.codec, Some(codec));
        assert_eq!(info.display_size, VideoSize { width: 1024, height: 576 });
        assert_eq!(info.pixel_aspect_ratio, AspectRatio::new(64, 45));
        assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
    }
    assert_eq!(
        probe_both(&matroska("V_VP9", true)).video_type,
        VideoType::WebM
    );
}

#[test]
fn probes_avi_tags_aspect_and_global_dimension_fallback() {
    let cases = [
        (b"H264", Codec::H264),
        (b"HEVC", Codec::H265),
        (b"AV01", Codec::Av1),
        (b"VP90", Codec::Vp9),
        (b"VP80", Codec::Vp8),
    ];
    for (tag, codec) in cases {
        let info = probe_both(&avi(tag, true));
        assert_eq!(info.codec, Some(codec));
        assert_eq!(info.display_size, VideoSize { width: 320, height: 180 });
        assert_eq!(
            info.size,
            VideoSize {
                width: 320,
                height: 180
            }
        );
        assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
    }
    assert_eq!(
        blob_size(&avi(b"VP80", false)).unwrap(),
        VideoSize {
            width: 320,
            height: 180
        }
    );
}

#[test]
fn avi_disabled_streams_media_chunks_codec_crop_and_audio_rejection() {
    let data = avi_custom(
        vec![
            avi_video_stream(b"VP80", 320, 240, true, false),
            avi_video_stream(b"zzzz", 640, 360, false, false),
        ],
        (1920, 1080),
        None,
    );
    let info = probe_both(&data);
    assert_eq!(
        info.size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
    assert_eq!(info.codec, None);

    let frame = vp8_keyframe(640, 360);
    let mut rec = le_chunk(b"00dc", vec![1, 2, 3]);
    rec.extend(le_chunk(b"01dc", frame));
    let data = avi_custom(
        vec![
            avi_audio_stream(),
            avi_video_stream(b"VP80", 0, 0, false, false),
        ],
        (0, 0),
        Some(list(b"rec ", rec)),
    );
    let info = probe_both(&data);
    assert_eq!(
        info.size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
    assert_eq!(info.display_size, VideoSize { width: 640, height: 360 });

    let avc = vec![
        0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0,
        0, 3, 0, 4, 0, 0, 3, 0, 0xf1, 0x83, 0x19, 0x60,
    ];
    let data = avi_custom(
        vec![avi_video_stream(b"H264", 1920, 1088, false, false)],
        (1920, 1088),
        Some(le_chunk(b"00dc", avc)),
    );
    let info = probe_both(&data);
    assert_eq!(
        info.size,
        VideoSize {
            width: 1920,
            height: 1088
        }
    );
    assert_eq!(info.display_size, VideoSize { width: 1920, height: 1080 });
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));

    let audio_only = avi_custom(vec![avi_audio_stream()], (1920, 1080), None);
    assert!(matches!(
        blob_probe(&audio_only),
        Err(VideoError::CorruptedVideo)
    ));
}

#[test]
fn every_container_work_limit_reports_limit_exceeded() {
    let tracks = (1..=65)
        .map(|number| matroska_track(number, 16, 16, true, true, Vec::new()))
        .collect();
    assert_limit_both(&matroska_with_tracks(tracks, None));

    let mut entries = Vec::new();
    for _ in 0..65 {
        entries.extend(be_box(b"avc1", vec![0; 78]));
    }
    let mut stsd = vec![0; 4];
    stsd.extend_from_slice(&65u32.to_be_bytes());
    stsd.extend(entries);
    let stbl = be_box(b"stbl", be_box(b"stsd", stsd));
    let minf = be_box(b"minf", stbl);
    let mut hdlr = vec![0; 12];
    hdlr[8..12].copy_from_slice(b"vide");
    let mut mdia = be_box(b"hdlr", hdlr);
    mdia.extend(minf);
    let mut tkhd = vec![0; 84];
    tkhd[3] = 3;
    put_matrix(&mut tkhd, 40, 0);
    tkhd[76..80].copy_from_slice(&(16u32 << 16).to_be_bytes());
    tkhd[80..84].copy_from_slice(&(16u32 << 16).to_be_bytes());
    let mut trak = be_box(b"tkhd", tkhd);
    trak.extend(be_box(b"mdia", mdia));
    assert_limit_both(&mp4_with_tracks(vec![be_box(b"trak", trak)], 0));

    let mut structural = be_box(b"ftyp", b"isom\0\0\0\0isom".to_vec());
    for _ in 0..65_536 {
        structural.extend_from_slice(&8u32.to_be_bytes());
        structural.extend_from_slice(b"free");
    }
    assert_limit_both(&structural);
}

/// Nests `depth` `minf` boxes around a sample table, descending into each one.
fn nested_minf_mp4(depth: u32) -> Vec<u8> {
    let mut sample = vec![0; 78];
    sample[24..26].copy_from_slice(&640u16.to_be_bytes());
    sample[26..28].copy_from_slice(&360u16.to_be_bytes());
    let mut stsd = vec![0; 4];
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend(be_box(b"avc1", sample));
    let table = be_box(b"stbl", be_box(b"stsd", stsd));
    let mut nest = Vec::with_capacity(depth as usize * 8 + table.len());
    for level in 0..depth {
        nest.extend_from_slice(&(8 * (depth - level) + table.len() as u32).to_be_bytes());
        nest.extend_from_slice(b"minf");
    }
    nest.extend(table);
    let mut hdlr = vec![0; 12];
    hdlr[8..12].copy_from_slice(b"vide");
    let mut mdia = be_box(b"hdlr", hdlr);
    mdia.extend(nest);
    let trak = be_box(b"trak", be_box(b"mdia", mdia));
    mp4_with_tracks(vec![trak], 0)
}

/// Nests `depth` `rec ` lists inside `movi`, which the sample search descends.
fn nested_rec_avi(depth: u32) -> Vec<u8> {
    let mut nest = Vec::with_capacity(depth as usize * 12);
    for level in 0..depth {
        nest.extend_from_slice(b"LIST");
        nest.extend_from_slice(&(4 + 12 * (depth - 1 - level)).to_le_bytes());
        nest.extend_from_slice(b"rec ");
    }
    avi_custom(
        vec![avi_video_stream(b"VP80", 0, 0, false, false)],
        (0, 0),
        Some(nest),
    )
}

#[test]
fn deeply_nested_boxes_are_refused_instead_of_recursing() {
    // Self-nesting boxes cost eight bytes a level, so the element budget alone
    // admits a chain deep enough to exhaust the stack.
    assert_limit_both(&nested_minf_mp4(40_000));
    assert_limit_both(&nested_rec_avi(40_000));
    // Far below every other budget, so only the nesting cap can refuse these.
    assert_limit_both(&nested_minf_mp4(40));
    assert_limit_both(&nested_rec_avi(40));
    // Real nesting is a few levels deep and must keep working.
    assert_eq!(
        probe_both(&nested_minf_mp4(2)).size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
}

#[test]
fn ebml_element_headers_may_not_cross_their_parent_end() {
    // This TrackEntry holds a bare element id; its child's size vint is then read
    // from past the entry, and an all-ones "unknown" size would otherwise put the
    // child's end before its own start.
    let mut entries = ebml_element(&[0xae], vec![0x83]);
    entries.push(0xff);
    let mut data = ebml_element(
        &[0x1a, 0x45, 0xdf, 0xa3],
        ebml_element(&[0x42, 0x82], b"webm".to_vec()),
    );
    data.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0xff]);
    data.extend(ebml_element(&[0x16, 0x54, 0xae, 0x6b], entries));
    assert!(matches!(blob_probe(&data), Err(VideoError::CorruptedVideo)));
    let (path, file) = file_for(&data);
    assert!(matches!(probe(file), Err(VideoError::CorruptedVideo)));
    fs::remove_file(path).unwrap();
}

#[test]
fn a_codec_sample_running_past_the_end_keeps_container_geometry() {
    let frame = vp8_keyframe(640, 360);
    let track = mp4_track_box(
        1,
        b"vp08",
        320,
        240,
        3,
        0,
        Vec::new(),
        regular_sample_tables(64, frame.len() as u32 * 4_000),
        None,
    );
    let mut data = mp4_with_tracks(vec![track], 0);
    data.extend(be_box(b"mdat", frame));
    assert_eq!(
        probe_both(&data).size,
        VideoSize {
            width: 320,
            height: 240
        }
    );
}

#[test]
fn codec_buffers_larger_than_the_scan_prefix_fall_back_to_the_container() {
    // Only a bounded prefix of codec private data and of the first sample is
    // read, so an oversized one costs a bounded read instead of failing.
    let oversized_private = be_box(b"avcC", vec![0; 256 * 1024 + 1]);
    let track = mp4_track_box(
        1,
        b"avc1",
        16,
        16,
        3,
        0,
        oversized_private,
        Vec::new(),
        None,
    );
    let expected = VideoSize {
        width: 16,
        height: 16,
    };
    assert_eq!(probe_both(&mp4_with_tracks(vec![track], 0)).size, expected);

    let sample_size = 256 * 1024 + 1;
    let track = |offset: u32| {
        mp4_track_box(
            1,
            b"vp08",
            16,
            16,
            3,
            0,
            Vec::new(),
            regular_sample_tables(offset, sample_size as u32),
            None,
        )
    };
    let preliminary = mp4_with_tracks(vec![track(0)], 0);
    let mut oversized_sample = mp4_with_tracks(vec![track((preliminary.len() + 8) as u32)], 0);
    oversized_sample.extend(be_box(b"mdat", vec![0; sample_size]));
    assert_eq!(probe_both(&oversized_sample).size, expected);
}

#[test]
fn large_first_sample_is_parsed_from_its_leading_bytes() {
    // Only a bounded prefix of a sample is materialized; a keyframe far larger
    // than that prefix must still yield geometry from its header.
    let sample_size = 200 * 1024;
    let track = |offset: u32| {
        mp4_track_box(
            1,
            b"vp08",
            0,
            0,
            3,
            0,
            Vec::new(),
            regular_sample_tables(offset, sample_size as u32),
            None,
        )
    };
    let preliminary = mp4_with_tracks(vec![track(0)], 0);
    let mut data = mp4_with_tracks(vec![track((preliminary.len() + 8) as u32)], 0);
    let mut payload = vp8_keyframe(640, 360);
    payload.resize(sample_size, 0);
    data.extend(be_box(b"mdat", payload));

    let info = probe_both(&data);
    assert_eq!(
        info.size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
    assert_eq!(info.display_size, VideoSize { width: 640, height: 360 });
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
    assert_eq!(info.codec, Some(Codec::Vp8));
}

#[test]
fn mutated_inputs_terminate_without_panicking() {
    // Codec headers are parsed with reads that yield zeroes past the end rather
    // than failing, so every loop they feed must stay bounded on garbage.
    let seeds = [
        mp4(b"avc1", false, 0, None),
        mp4(b"hvc1", true, 90, Some((4, 3))),
        matroska("V_AV1", true),
        matroska("V_VP9", false),
        avi(b"VP80", true),
    ];
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut random = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for seed in &seeds {
        for _ in 0..2_000 {
            let mut data = seed.clone();
            for _ in 0..8 {
                let index = (random() % data.len() as u64) as usize;
                data[index] = random() as u8;
            }
            let truncated = &data[..(random() % data.len() as u64) as usize + 1];
            let _ = blob_probe(&data);
            let _ = blob_probe(truncated);
            let _ = video_type(truncated);
        }
    }
}

#[test]
fn rejects_unknown_and_truncated_inputs_without_panicking() {
    assert!(matches!(
        video_type(b"not a video"),
        Err(VideoError::NotSupported)
    ));
    assert!(matches!(
        blob_probe(&mp4(b"avc1", false, 0, None)[..20]),
        Err(VideoError::CorruptedVideo)
    ));
    assert!(matches!(
        blob_probe(&matroska("V_VP9", false)[..12]),
        Err(VideoError::CorruptedVideo)
    ));
    assert!(matches!(
        blob_probe(&avi(b"VP80", true)[..16]),
        Err(VideoError::CorruptedVideo)
    ));
}

#[test]
fn owned_file_type_ignores_initial_cursor() {
    let data = mp4(b"avc1", false, 0, None);
    let (path, file) = file_for(&data);
    assert_eq!(file_type(file).unwrap(), VideoType::Mp4);
    fs::remove_file(path).unwrap();
}

#[test]
fn matroska_crop_defaults_and_projection_geometry() {
    let crop = ebml_uint(&[0x54, 0xaa], 8);
    let data = matroska_with_tracks(vec![matroska_track(1, 1920, 1088, true, true, crop)], None);
    let info = probe_both(&data);
    assert_eq!(
        info.size,
        VideoSize {
            width: 1920,
            height: 1088
        }
    );
    assert_eq!(info.display_size, VideoSize { width: 1920, height: 1080 });
    assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));

    let only_width = ebml_uint(&[0x54, 0xb0], 1024);
    let data = matroska_with_tracks(
        vec![matroska_track(1, 720, 576, true, true, only_width)],
        None,
    );
    assert_eq!(probe_both(&data).aspect_ratio(), AspectRatio::new(16, 9));

    let only_height = ebml_uint(&[0x54, 0xba], 405);
    let data = matroska_with_tracks(
        vec![matroska_track(1, 720, 576, true, true, only_height)],
        None,
    );
    assert_eq!(probe_both(&data).aspect_ratio(), AspectRatio::new(16, 9));

    let projection = ebml_element(
        &[0x76, 0x70],
        ebml_element(&[0x76, 0x75], 90f32.to_be_bytes().to_vec()),
    );
    let data = matroska_with_tracks(
        vec![matroska_track(1, 640, 360, true, true, projection)],
        None,
    );
    let info = probe_both(&data);
    assert_eq!(info.rotation, 270);
    assert_eq!(info.display_size, VideoSize { width: 360, height: 640 });
    assert_eq!(info.aspect_ratio(), AspectRatio::new(9, 16));
}

#[test]
fn matroska_default_track_selection_ignores_codec_preference() {
    let first = matroska_track(1, 320, 240, true, false, Vec::new());
    let second = matroska_track(2, 640, 360, true, true, Vec::new());
    let data = matroska_with_tracks(vec![first, second], None);
    assert_eq!(
        probe_both(&data).size,
        VideoSize {
            width: 640,
            height: 360
        }
    );
}

fn vp8_keyframe(width: u16, height: u16) -> Vec<u8> {
    let mut data = vec![0x10, 0, 0, 0x9d, 1, 0x2a];
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data
}

#[test]
fn matroska_selected_blocks_support_all_lacing_modes_and_block_groups() {
    let frame = vp8_keyframe(640, 360);
    let mut blocks = Vec::new();

    let mut plain = vec![0x81, 0, 0, 0x80];
    plain.extend(&frame);
    blocks.push(ebml_element(&[0xa3], plain));

    let mut xiph = vec![0x81, 0, 0, 0x82, 1, frame.len() as u8];
    xiph.extend(&frame);
    xiph.extend(vec![0; frame.len()]);
    blocks.push(ebml_element(&[0xa3], xiph));

    let mut fixed = vec![0x81, 0, 0, 0x84, 1];
    fixed.extend(&frame);
    fixed.extend(vec![0; frame.len()]);
    blocks.push(ebml_element(&[0xa3], fixed));

    let mut ebml_laced = vec![0x81, 0, 0, 0x86, 1, 0x80 | frame.len() as u8];
    ebml_laced.extend(&frame);
    ebml_laced.extend(vec![0; frame.len()]);
    blocks.push(ebml_element(&[0xa0], ebml_element(&[0xa1], ebml_laced)));

    for block in blocks {
        let data = matroska_with_tracks(
            vec![matroska_track(1, 1, 1, true, true, Vec::new())],
            Some(block),
        );
        let info = probe_both(&data);
        assert_eq!(
            info.size,
            VideoSize {
                width: 1,
                height: 1
            }
        );
        assert_eq!(info.aspect_ratio(), AspectRatio::new(16, 9));
    }
}
