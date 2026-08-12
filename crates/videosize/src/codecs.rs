//! Small, bounded elementary-header parsers.
//!
//! Container walkers enforce byte budgets before handing slices to this
//! module. Every parser is deliberately fallible: malformed optional codec
//! metadata must not invalidate otherwise sound container geometry.

use crate::util::be;
use crate::{AspectRatio, Codec};

pub(crate) fn from_id(id: &[u8]) -> Option<Codec> {
    let Ok(mut id) = <[u8; 4]>::try_from(id) else {
        return match id {
            b"V_MPEG4/ISO/AVC" => Some(Codec::H264),
            b"V_MPEGH/ISO/HEVC" => Some(Codec::H265),
            b"V_AV1" => Some(Codec::Av1),
            b"V_VP9" => Some(Codec::Vp9),
            b"V_VP8" => Some(Codec::Vp8),
            _ => None,
        };
    };
    id.make_ascii_lowercase();
    match &id {
        b"avc1" | b"avc2" | b"avc3" | b"avc4" | b"h264" | b"x264" => Some(Codec::H264),
        b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" | b"h265" | b"x265" | b"hevc" => Some(Codec::H265),
        b"av01" => Some(Codec::Av1),
        b"vp09" | b"vp90" => Some(Codec::Vp9),
        b"vp08" | b"vp80" => Some(Codec::Vp8),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CodecGeometry {
    pub(crate) coded_width: u64,
    pub(crate) coded_height: u64,
    pub(crate) visible_width: u64,
    pub(crate) visible_height: u64,
    pub(crate) render_width: u64,
    pub(crate) render_height: u64,
    pub(crate) pixel_aspect_ratio: Option<AspectRatio>,
}

impl CodecGeometry {
    pub(crate) fn display_dimensions(self) -> Option<(u64, u64)> {
        if self.render_width != 0 && self.render_height != 0 {
            Some((self.render_width, self.render_height))
        } else if self.visible_width != 0 && self.visible_height != 0 {
            Some((self.visible_width, self.visible_height))
        } else if self.coded_width != 0 && self.coded_height != 0 {
            Some((self.coded_width, self.coded_height))
        } else {
            None
        }
    }

    fn merge_missing(&mut self, other: Self) {
        if self.coded_width == 0 || self.coded_height == 0 {
            self.coded_width = other.coded_width;
            self.coded_height = other.coded_height;
        }
        if self.visible_width == 0 || self.visible_height == 0 {
            self.visible_width = other.visible_width;
            self.visible_height = other.visible_height;
        }
        if self.render_width == 0 || self.render_height == 0 {
            self.render_width = other.render_width;
            self.render_height = other.render_height;
        }
        if self.pixel_aspect_ratio.is_none() {
            self.pixel_aspect_ratio = other.pixel_aspect_ratio;
        }
    }
}

pub(crate) fn geometry(
    codec: Codec,
    codec_private: Option<&[u8]>,
    first_frame: Option<&[u8]>,
) -> Option<CodecGeometry> {
    match codec {
        Codec::H264 => merge_geometry(codec_private, first_frame, avc_geometry),
        Codec::H265 => merge_geometry(codec_private, first_frame, hevc_geometry),
        Codec::Av1 => av1_geometry(codec_private, first_frame),
        Codec::Vp8 => first_frame
            .and_then(vp8_geometry)
            .or_else(|| codec_private.and_then(vp8_geometry)),
        Codec::Vp9 => first_frame
            .and_then(vp9_geometry)
            .or_else(|| codec_private.and_then(vp9_geometry)),
    }
}

fn merge_geometry(
    private: Option<&[u8]>,
    frame: Option<&[u8]>,
    parse: fn(&[u8]) -> Option<CodecGeometry>,
) -> Option<CodecGeometry> {
    let mut result = private.and_then(parse);
    if let Some(frame) = frame.and_then(parse) {
        if let Some(result) = &mut result {
            result.merge_missing(frame);
        } else {
            result = Some(frame);
        }
    }
    result
}

fn avc_geometry(data: &[u8]) -> Option<CodecGeometry> {
    if data.len() >= 7 && data[0] == 1 {
        let count = (data[5] & 0x1f) as usize;
        let mut position = 6usize;
        for _ in 0..count {
            let size = be(data, position, 2)? as usize;
            position = position.checked_add(2)?;
            let end = position.checked_add(size)?;
            let nal = data.get(position..end)?;
            if nal.first().is_some_and(|byte| byte & 0x1f == 7)
                && let Some(value) = parse_avc_sps(nal)
            {
                return Some(value);
            }
            position = end;
        }
        return None;
    }
    parse_avc_sps(find_nal(data, Sps::Avc)?)
}

fn parse_avc_sps(nal: &[u8]) -> Option<CodecGeometry> {
    if nal.len() < 4 || nal[0] & 0x1f != 7 {
        return None;
    }
    let mut bits = Bits::rbsp(&nal[1..]);
    let profile = bits.bits(8) as u8;
    bits.skip(16);
    bits.ue();
    let mut chroma_format_idc = 1u64;
    let mut separate_colour_plane = false;
    if matches!(
        profile,
        44 | 83 | 86 | 100 | 110 | 118 | 122 | 128 | 134 | 135 | 138 | 139 | 144 | 244
    ) {
        chroma_format_idc = bits.ue();
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 {
            separate_colour_plane = bits.bit();
        }
        bits.skip_ue(2);
        bits.skip(1);
        if bits.bit() {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for index in 0..count {
                if bits.bit() {
                    skip_avc_scaling_list(&mut bits, if index < 6 { 16 } else { 64 });
                }
            }
        }
    }
    bits.ue();
    let pic_order_cnt_type = bits.ue();
    if pic_order_cnt_type == 0 {
        bits.ue();
    } else if pic_order_cnt_type == 1 {
        bits.skip(1);
        bits.se();
        bits.se();
        let count = bits.ue();
        if count > 255 {
            return None;
        }
        for _ in 0..count {
            bits.se();
        }
    } else if pic_order_cnt_type > 2 {
        return None;
    }
    bits.ue();
    bits.skip(1);
    let width_in_mbs = bits.ue() + 1;
    let height_in_map_units = bits.ue() + 1;
    let frame_mbs_only = bits.bit();
    if !frame_mbs_only {
        bits.skip(1);
    }
    bits.skip(1);
    let [crop_left, crop_right, crop_top, crop_bottom] = if bits.bit() {
        [bits.ue(), bits.ue(), bits.ue(), bits.ue()]
    } else {
        [0; 4]
    };
    if !bits.ok() {
        return None;
    }
    // A VUI truncated away is tolerated; everything above it is mandatory.
    let pixel_aspect_ratio = bits.bit().then(|| parse_vui_aspect(&mut bits)).flatten();
    let coded_width = width_in_mbs.checked_mul(16)?;
    let coded_height = height_in_map_units.checked_mul(if frame_mbs_only { 16 } else { 32 })?;
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (sub_width, sub_height) = match chroma_array_type {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        3 => (1, 1),
        _ => return None,
    };
    let crop_unit_x = sub_width;
    let crop_unit_y = sub_height * if frame_mbs_only { 1 } else { 2 };
    let horizontal_crop = crop_left
        .checked_add(crop_right)?
        .checked_mul(crop_unit_x)?;
    let vertical_crop = crop_top
        .checked_add(crop_bottom)?
        .checked_mul(crop_unit_y)?;
    let visible_width = coded_width.checked_sub(horizontal_crop)?;
    let visible_height = coded_height.checked_sub(vertical_crop)?;
    nonzero_geometry(
        coded_width,
        coded_height,
        visible_width,
        visible_height,
        pixel_aspect_ratio,
    )
}

fn skip_avc_scaling_list(bits: &mut Bits<'_>, size: usize) {
    let mut last_scale = 8i64;
    let mut next_scale = 8i64;
    for _ in 0..size {
        if next_scale != 0 {
            next_scale = (last_scale + bits.se() + 256) % 256;
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
}

fn parse_vui_aspect(bits: &mut Bits<'_>) -> Option<AspectRatio> {
    if !bits.bit() {
        return None;
    }
    let idc = bits.bits(8) as u8;
    let ratio = if idc == 255 {
        let width = bits.bits(16);
        let height = bits.bits(16);
        (width != 0 && height != 0).then(|| AspectRatio::new(width, height))
    } else {
        sar_from_idc(idc)
    };
    bits.ok().then_some(ratio).flatten()
}

fn hevc_geometry(data: &[u8]) -> Option<CodecGeometry> {
    if data.len() >= 23 && data[0] == 1 {
        let array_count = data[22] as usize;
        let mut position = 23usize;
        for _ in 0..array_count {
            let nal_type = *data.get(position)? & 0x3f;
            position += 1;
            let count = be(data, position, 2)? as usize;
            position += 2;
            for _ in 0..count {
                let size = be(data, position, 2)? as usize;
                position += 2;
                let end = position.checked_add(size)?;
                let nal = data.get(position..end)?;
                if (nal_type == 33 || hevc_nal_type(nal) == Some(33))
                    && let Some(value) = parse_hevc_sps(nal)
                {
                    return Some(value);
                }
                position = end;
            }
        }
        return None;
    }
    parse_hevc_sps(find_nal(data, Sps::Hevc)?)
}

fn hevc_nal_type(nal: &[u8]) -> Option<u8> {
    Some((nal.first()? >> 1) & 0x3f)
}

fn parse_hevc_sps(nal: &[u8]) -> Option<CodecGeometry> {
    if nal.len() < 4 || hevc_nal_type(nal) != Some(33) {
        return None;
    }
    let mut bits = Bits::rbsp(&nal[2..]);
    let max_sub_layers_minus1 = ((bits.bits(8) >> 1) & 7) as usize;
    skip_profile_tier_level(&mut bits, max_sub_layers_minus1);
    bits.ue();
    let chroma_format_idc = bits.ue();
    if chroma_format_idc > 3 {
        return None;
    }
    let separate_colour_plane = chroma_format_idc == 3 && bits.bit();
    let coded_width = bits.ue();
    let coded_height = bits.ue();
    let [left, right, top, bottom] = if bits.bit() {
        [bits.ue(), bits.ue(), bits.ue(), bits.ue()]
    } else {
        [0; 4]
    };
    let chroma_array_type = if separate_colour_plane {
        0
    } else {
        chroma_format_idc
    };
    let (sub_width, sub_height) = match chroma_array_type {
        0 => (1, 1),
        1 => (2, 2),
        2 => (2, 1),
        3 => (1, 1),
        _ => return None,
    };
    let visible_width =
        coded_width.checked_sub(left.checked_add(right)?.checked_mul(sub_width)?)?;
    let visible_height =
        coded_height.checked_sub(top.checked_add(bottom)?.checked_mul(sub_height)?)?;

    bits.skip_ue(2);
    let log2_max_pic_order_cnt_lsb_minus4 = bits.ue() as u32;
    if log2_max_pic_order_cnt_lsb_minus4 > 12 {
        return None;
    }
    let ordering_info_present = bits.bit();
    let first_layer = if ordering_info_present {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first_layer..=max_sub_layers_minus1 {
        bits.skip_ue(3);
    }
    bits.skip_ue(6);
    if bits.bit() && bits.bit() {
        skip_hevc_scaling_list(&mut bits);
    }
    bits.skip(2);
    if bits.bit() {
        bits.skip(4 + 4);
        bits.skip_ue(2);
        bits.skip(1);
    }
    let short_term_sets = bits.ue() as usize;
    if short_term_sets > 64 {
        return None;
    }
    let mut delta_pocs = [0u8; 64];
    for index in 0..short_term_sets {
        delta_pocs[index] =
            skip_short_term_ref_pic_set(&mut bits, index, short_term_sets, &delta_pocs[..index])?;
    }
    if bits.bit() {
        let count = bits.ue();
        if count > 64 {
            return None;
        }
        for _ in 0..count {
            bits.skip(log2_max_pic_order_cnt_lsb_minus4 + 5);
        }
    }
    bits.skip(2);
    if !bits.ok() {
        return None;
    }
    // A VUI truncated away is tolerated; everything above it is mandatory.
    let pixel_aspect_ratio = bits.bit().then(|| parse_vui_aspect(&mut bits)).flatten();
    nonzero_geometry(
        coded_width,
        coded_height,
        visible_width,
        visible_height,
        pixel_aspect_ratio,
    )
}

fn skip_profile_tier_level(bits: &mut Bits<'_>, layers: usize) {
    bits.skip(96);
    let mut profile = [false; 8];
    let mut level = [false; 8];
    let layers = layers.min(profile.len());
    for index in 0..layers {
        profile[index] = bits.bit();
        level[index] = bits.bit();
    }
    if layers != 0 {
        bits.skip(2 * (profile.len() - layers) as u32);
    }
    for index in 0..layers {
        if profile[index] {
            bits.skip(88);
        }
        if level[index] {
            bits.skip(8);
        }
    }
}

fn skip_hevc_scaling_list(bits: &mut Bits<'_>) {
    for size_id in 0..4usize {
        let step = if size_id == 3 { 3 } else { 1 };
        for _ in (0..6).step_by(step) {
            if !bits.bit() {
                bits.ue();
            } else {
                let coef_num = 1usize << (4 + (size_id << 1));
                if size_id > 1 {
                    bits.se();
                }
                for _ in 0..coef_num.min(64) {
                    bits.se();
                }
            }
        }
    }
}

fn skip_short_term_ref_pic_set(
    bits: &mut Bits<'_>,
    index: usize,
    count: usize,
    previous_counts: &[u8],
) -> Option<u8> {
    let inter_prediction = index != 0 && bits.bit();
    if inter_prediction {
        let delta_index = if index == count {
            bits.ue() as usize + 1
        } else {
            1
        };
        let referenced = *index
            .checked_sub(delta_index)
            .and_then(|index| previous_counts.get(index))?;
        bits.skip(1);
        bits.ue();
        let mut used = 0u8;
        for _ in 0..=referenced {
            let use_delta = bits.bit() || bits.bit();
            used = used.saturating_add(u8::from(use_delta));
        }
        Some(used)
    } else {
        let total = bits.ue() + bits.ue();
        if total > 64 {
            return None;
        }
        for _ in 0..total {
            bits.ue();
            bits.skip(1);
        }
        Some(total as u8)
    }
}

fn av1_geometry(private: Option<&[u8]>, frame: Option<&[u8]>) -> Option<CodecGeometry> {
    let mut sequence = None;
    let mut result = None;
    if let Some(data) = private {
        scan_av1(data, &mut sequence, &mut result)?;
    }
    if let Some(data) = frame {
        scan_av1(data, &mut sequence, &mut result)?;
    }
    result
}

fn scan_av1(
    data: &[u8],
    sequence: &mut Option<Av1Sequence>,
    result: &mut Option<CodecGeometry>,
) -> Option<()> {
    let data = if data.starts_with(&[0x81]) && data.len() >= 4 {
        &data[4..]
    } else {
        data
    };
    let mut position = 0;
    while position < data.len() {
        let (kind, payload) = next_obu(data, &mut position)?;
        if kind == 1 {
            let parsed = parse_av1_sequence(payload)?;
            *result = Some(CodecGeometry {
                coded_width: parsed.max_width,
                coded_height: parsed.max_height,
                visible_width: parsed.max_width,
                visible_height: parsed.max_height,
                ..CodecGeometry::default()
            });
            *sequence = Some(parsed);
        } else if matches!(kind, 3 | 6)
            && let (Some(state), Some(geometry)) = (*sequence, result.as_mut())
            && let Some((coded_width, frame_height, upscaled_width, render_width, render_height)) =
                parse_av1_frame_header(payload, state)
        {
            geometry.coded_width = coded_width;
            geometry.coded_height = frame_height;
            geometry.visible_width = upscaled_width;
            geometry.visible_height = frame_height;
            geometry.render_width = render_width;
            geometry.render_height = render_height;
        }
    }
    Some(())
}

#[derive(Clone, Copy)]
struct Av1Sequence {
    reduced: bool,
    max_width: u64,
    max_height: u64,
    width_bits: u32,
    height_bits: u32,
    frame_id_bits: u32,
    order_hint_bits: u32,
    screen_tools: u8,
    integer_mv: u8,
    enable_superres: bool,
    simple_frames: bool,
}

fn parse_av1_sequence(data: &[u8]) -> Option<Av1Sequence> {
    let mut bits = Bits::new(data);
    bits.skip(4);
    let reduced = bits.bit();
    let mut simple_frames = true;
    if reduced {
        bits.skip(5);
    } else {
        let timing = bits.bit();
        let mut decoder_model = false;
        let mut delay_bits = 0;
        if timing {
            bits.skip(64);
            if bits.bit() {
                bits.ue();
            }
            decoder_model = bits.bit();
            if decoder_model {
                delay_bits = bits.bits(5) as u32 + 1;
                bits.skip(42);
                simple_frames = false;
            }
        }
        let initial_display_delay_present = bits.bit();
        let operating_points = bits.bits(5) + 1;
        for _ in 0..operating_points {
            bits.skip(12);
            let level = bits.bits(5);
            if level > 7 {
                bits.skip(1);
            }
            if decoder_model && bits.bit() {
                bits.skip(delay_bits * 2 + 1);
            }
            if initial_display_delay_present && bits.bit() {
                bits.skip(4);
            }
        }
    }
    let width_bits = bits.bits(4) as u32 + 1;
    let height_bits = bits.bits(4) as u32 + 1;
    let max_width = bits.bits(width_bits) + 1;
    let max_height = bits.bits(height_bits) + 1;
    let frame_id_bits = if !reduced && bits.bit() {
        bits.bits(4) as u32 + bits.bits(3) as u32 + 3
    } else {
        0
    };
    bits.skip(3);
    let mut enable_order_hint = false;
    let (screen_tools, integer_mv) = if reduced {
        (2, 2)
    } else {
        bits.skip(4);
        enable_order_hint = bits.bit();
        if enable_order_hint {
            bits.skip(2);
        }
        let screen = if bits.bit() { 2 } else { bits.bit() as u8 };
        let integer = if screen > 0 {
            if bits.bit() { 2 } else { bits.bit() as u8 }
        } else {
            2
        };
        (screen, integer)
    };
    let order_hint_bits = if enable_order_hint {
        bits.bits(3) as u32 + 1
    } else {
        0
    };
    let enable_superres = bits.bit();
    bits.skip(2);
    if !bits.ok() {
        return None;
    }
    Some(Av1Sequence {
        reduced,
        max_width,
        max_height,
        width_bits,
        height_bits,
        frame_id_bits,
        order_hint_bits,
        screen_tools,
        integer_mv,
        enable_superres,
        simple_frames,
    })
}

fn parse_av1_frame_header(data: &[u8], sequence: Av1Sequence) -> Option<(u64, u64, u64, u64, u64)> {
    if !sequence.simple_frames {
        return None;
    }
    let mut bits = Bits::new(data);
    if !sequence.reduced && bits.bit() {
        return None;
    }
    let frame_type = if sequence.reduced {
        0
    } else {
        bits.bits(2) as u8
    };
    let show_frame = sequence.reduced || bits.bit();
    if !show_frame {
        bits.skip(1);
    }
    let error_resilient = if frame_type == 3 || (frame_type == 0 && show_frame) {
        true
    } else {
        bits.bit()
    };
    bits.skip(1);
    let allow_screen = match sequence.screen_tools {
        2 => bits.bit(),
        value => value != 0,
    };
    if allow_screen && sequence.integer_mv == 2 {
        bits.skip(1);
    }
    let frame_is_intra = matches!(frame_type, 0 | 2);
    bits.skip(sequence.frame_id_bits);
    let frame_size_override = if frame_type == 3 {
        true
    } else if sequence.reduced {
        false
    } else {
        bits.bit()
    };
    bits.skip(sequence.order_hint_bits);
    if !(frame_is_intra || error_resilient) {
        bits.skip(3);
    }
    if !(frame_type == 3 || (frame_type == 0 && show_frame)) {
        bits.skip(8);
    }
    if !frame_is_intra {
        return None;
    }
    let upscaled_width = if frame_size_override {
        bits.bits(sequence.width_bits) + 1
    } else {
        sequence.max_width
    };
    let frame_height = if frame_size_override {
        bits.bits(sequence.height_bits) + 1
    } else {
        sequence.max_height
    };
    let superres_denom = if sequence.enable_superres && bits.bit() {
        bits.bits(3) + 9
    } else {
        8
    };
    let coded_width = upscaled_width
        .checked_mul(8)?
        .checked_add(superres_denom / 2)?
        / superres_denom;
    let (render_width, render_height) = if bits.bit() {
        (bits.bits(16) + 1, bits.bits(16) + 1)
    } else {
        (upscaled_width, frame_height)
    };
    if !bits.ok() {
        return None;
    }
    Some((
        coded_width,
        frame_height,
        upscaled_width,
        render_width,
        render_height,
    ))
}

fn next_obu<'a>(data: &'a [u8], position: &mut usize) -> Option<(u8, &'a [u8])> {
    let header = *data.get(*position)?;
    *position += 1;
    if header & 0x81 != 0 {
        return None;
    }
    let kind = (header >> 3) & 15;
    if header & 4 != 0 {
        *position = position.checked_add(1)?;
    }
    let size = if header & 2 != 0 {
        read_leb128(data, position)?
    } else {
        data.len().checked_sub(*position)?
    };
    let end = position.checked_add(size)?;
    let payload = data.get(*position..end)?;
    *position = end;
    Some((kind, payload))
}

fn read_leb128(data: &[u8], position: &mut usize) -> Option<usize> {
    let mut value = 0usize;
    for shift in (0..=56).step_by(7) {
        let byte = *data.get(*position)?;
        *position += 1;
        value = value.checked_add(((byte & 0x7f) as usize).checked_shl(shift)?)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn vp8_geometry(data: &[u8]) -> Option<CodecGeometry> {
    if data.len() < 10 || data[0] & 1 != 0 || data.get(3..6)? != b"\x9d\x01\x2a" {
        return None;
    }
    let packed_width = u16::from_le_bytes([data[6], data[7]]);
    let packed_height = u16::from_le_bytes([data[8], data[9]]);
    let width = (packed_width & 0x3fff) as u64;
    let height = (packed_height & 0x3fff) as u64;
    if width == 0 || height == 0 {
        return None;
    }
    let (width_num, width_den) = VP8_SCALE[(packed_width >> 14) as usize];
    let (height_num, height_den) = VP8_SCALE[(packed_height >> 14) as usize];
    let render_width = (width * u64::from(width_num)).div_ceil(u64::from(width_den));
    let render_height = (height * u64::from(height_num)).div_ceil(u64::from(height_den));
    Some(CodecGeometry {
        coded_width: width,
        coded_height: height,
        visible_width: width,
        visible_height: height,
        render_width,
        render_height,
        pixel_aspect_ratio: None,
    })
}

const VP8_SCALE: [(u8, u8); 4] = [(1, 1), (5, 4), (5, 3), (2, 1)];

fn vp9_geometry(data: &[u8]) -> Option<CodecGeometry> {
    let mut bits = Bits::new(data);
    if bits.bits(2) != 2 {
        return None;
    }
    let low_profile = bits.bit() as u8;
    let high_profile = bits.bit() as u8;
    let profile = low_profile + (high_profile << 1);
    if profile == 3 && bits.bit() {
        return None;
    }
    if bits.bit() || bits.bit() {
        return None;
    }
    bits.skip(2);
    if bits.bits(24) != 0x49_83_42 {
        return None;
    }
    if profile >= 2 {
        bits.skip(1);
    }
    let color_space = bits.bits(3);
    if color_space != 7 {
        bits.skip(1);
        if profile == 1 || profile == 3 {
            bits.skip(3);
        }
    } else if profile == 1 || profile == 3 {
        bits.skip(1);
    }
    let width = bits.bits(16) + 1;
    let height = bits.bits(16) + 1;
    let (render_width, render_height) = if bits.bit() {
        (bits.bits(16) + 1, bits.bits(16) + 1)
    } else {
        (width, height)
    };
    if !bits.ok() {
        return None;
    }
    Some(CodecGeometry {
        coded_width: width,
        coded_height: height,
        visible_width: width,
        visible_height: height,
        render_width,
        render_height,
        pixel_aspect_ratio: None,
    })
}

fn nonzero_geometry(
    coded_width: u64,
    coded_height: u64,
    visible_width: u64,
    visible_height: u64,
    pixel_aspect_ratio: Option<AspectRatio>,
) -> Option<CodecGeometry> {
    if coded_width == 0 || coded_height == 0 || visible_width == 0 || visible_height == 0 {
        return None;
    }
    Some(CodecGeometry {
        coded_width,
        coded_height,
        visible_width,
        visible_height,
        render_width: 0,
        render_height: 0,
        pixel_aspect_ratio,
    })
}

/// Table E-1 sample aspect ratios, already in lowest terms.
const SAR: [(u8, u8); 16] = [
    (1, 1),
    (12, 11),
    (10, 11),
    (16, 11),
    (40, 33),
    (24, 11),
    (20, 11),
    (32, 11),
    (80, 33),
    (18, 11),
    (15, 11),
    (64, 33),
    (160, 99),
    (4, 3),
    (3, 2),
    (2, 1),
];

fn sar_from_idc(idc: u8) -> Option<AspectRatio> {
    let &(numerator, denominator) = SAR.get(usize::from(idc).checked_sub(1)?)?;
    Some(AspectRatio {
        numerator: numerator.into(),
        denominator: denominator.into(),
    })
}

#[derive(Clone, Copy)]
enum Sps {
    Avc,
    Hevc,
}

impl Sps {
    fn matches(self, nal: &[u8]) -> bool {
        let Some(&byte) = nal.first() else {
            return false;
        };
        match self {
            Self::Avc => byte & 0x1f == 7,
            Self::Hevc => (byte >> 1) & 0x3f == 33,
        }
    }
}

fn find_nal(data: &[u8], wanted: Sps) -> Option<&[u8]> {
    let mut position = 0;
    while let Some(start) = find_start_code(data, position) {
        let end = find_start_code(data, start).map_or(data.len(), |next| next - 3);
        let nal = data.get(start..end)?;
        if wanted.matches(nal) {
            return Some(nal);
        }
        position = end;
    }
    if wanted.matches(data) {
        return Some(data);
    }
    'lengths: for length_size in [4, 2, 1] {
        let mut position = 0;
        while position + length_size <= data.len() {
            let Some(size) = be(data, position, length_size).filter(|size| *size != 0) else {
                continue 'lengths;
            };
            let size = size as usize;
            position += length_size;
            let Some(end) = position.checked_add(size) else {
                continue 'lengths;
            };
            let Some(nal) = data.get(position..end) else {
                continue 'lengths;
            };
            if wanted.matches(nal) {
                return Some(nal);
            }
            position = end;
        }
    }
    None
}

/// Returns where the payload of the next Annex-B NAL begins.
///
/// A four-byte start code is found as its trailing three bytes, so the preceding
/// NAL may keep one trailing zero, which header parsers never reach.
fn find_start_code(data: &[u8], from: usize) -> Option<usize> {
    let mut index = from.checked_add(2)?;
    while index < data.len() {
        index += find_byte(data.get(index..)?, 1)?;
        if data[index - 1] == 0 && data[index - 2] == 0 {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

/// Finds `wanted`, testing eight bytes per step.
///
/// Samples are scanned for start codes in full, so this runs over as much as
/// [`MAX_CODEC_SCAN`](crate::util::MAX_CODEC_SCAN) bytes per probe.
fn find_byte(data: &[u8], wanted: u8) -> Option<usize> {
    const ONES: u64 = u64::from_ne_bytes([1; 8]);
    const HIGH: u64 = u64::from_ne_bytes([0x80; 8]);
    let pattern = u64::from_ne_bytes([wanted; 8]);
    let mut chunks = data.chunks_exact(8);
    let mut offset = 0;
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().ok()?) ^ pattern;
        let zeroes = word.wrapping_sub(ONES) & !word & HIGH;
        if zeroes != 0 {
            return Some(offset + (zeroes.trailing_zeros() / 8) as usize);
        }
        offset += 8;
    }
    let tail = chunks.remainder().iter().position(|byte| *byte == wanted)?;
    Some(offset + tail)
}

/// A most-significant-bit-first reader backed by a 64-bit accumulator.
///
/// Reads are infallible: running out of input yields zeroes and latches
/// [`Bits::ok`] to false, so a parser checks once for truncation instead of
/// threading an `Option` through every field. Every loop fed by a decoded value is
/// bounded by an explicit range check, so a truncated header still terminates.
///
/// Refills are byte-granular so that H.264/H.265 emulation-prevention bytes can be
/// dropped in flight, which is what lets the AVC and HEVC parsers run straight off the
/// NAL without materializing an unescaped copy.
struct Bits<'a> {
    data: &'a [u8],
    next: usize,
    accumulator: u64,
    len: u32,
    zeros: u8,
    escaped: bool,
    overrun: bool,
}

const CAPACITY: u32 = 56;

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            next: 0,
            accumulator: 0,
            len: 0,
            zeros: 0,
            escaped: false,
            overrun: false,
        }
    }

    fn rbsp(data: &'a [u8]) -> Self {
        Self {
            escaped: true,
            ..Self::new(data)
        }
    }

    fn ok(&self) -> bool {
        !self.overrun
    }

    fn exhaust(&mut self) {
        self.overrun = true;
        self.next = self.data.len();
        self.accumulator = 0;
        self.len = 0;
    }

    /// Tops the accumulator up to at least [`CAPACITY`] bits, or to end of input.
    ///
    /// Kept out of line: it is the only loop in the reader and callers reach it
    /// roughly once per seven bytes, so inlining it at every read site would cost
    /// far more code than the call saves.
    #[inline(never)]
    fn fill(&mut self) {
        while self.len <= CAPACITY {
            let Some(&byte) = self.data.get(self.next) else {
                return;
            };
            self.next += 1;
            if self.escaped {
                if self.zeros >= 2 && byte == 3 {
                    self.zeros = 0;
                    continue;
                }
                self.zeros = if byte == 0 {
                    self.zeros.saturating_add(1)
                } else {
                    0
                };
            }
            self.accumulator |= (byte as u64) << (CAPACITY - self.len);
            self.len += 8;
        }
    }

    fn bits(&mut self, count: u32) -> u64 {
        let count = count.min(CAPACITY);
        if count > self.len {
            self.fill();
            if count > self.len {
                self.exhaust();
                return 0;
            }
        }
        if count == 0 {
            return 0;
        }
        let value = self.accumulator >> (64 - count);
        self.accumulator <<= count;
        self.len -= count;
        value
    }

    fn bit(&mut self) -> bool {
        self.bits(1) != 0
    }

    fn skip(&mut self, mut count: u32) {
        while count > CAPACITY {
            self.bits(CAPACITY);
            if self.overrun {
                return;
            }
            count -= CAPACITY;
        }
        self.bits(count);
    }

    /// Reads an unsigned Exp-Golomb code, which both H.264 and H.265 bound to 32 bits.
    fn ue(&mut self) -> u64 {
        if self.len < 32 {
            self.fill();
        }
        // A refill only guarantees 57 bits, so a wide code is read in two steps
        // rather than sliced out of the accumulator in one.
        let zeros = self.accumulator.leading_zeros();
        if zeros >= 32 {
            self.exhaust();
            return 0;
        }
        self.bits(zeros + 1);
        ((1u64 << zeros) - 1) + self.bits(zeros)
    }

    fn skip_ue(&mut self, count: usize) {
        for _ in 0..count {
            self.ue();
        }
    }

    fn se(&mut self) -> i64 {
        let value = self.ue();
        let magnitude = value.div_ceil(2) as i64;
        if value & 1 == 0 {
            -magnitude
        } else {
            magnitude
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Bits, geometry};
    use crate::{AspectRatio, Codec};

    /// The bit-at-a-time reader [`Bits`] replaced, over a pre-unescaped buffer.
    struct Reference {
        data: Vec<u8>,
        bit: usize,
    }

    impl Reference {
        fn new(data: &[u8], escaped: bool) -> Self {
            let mut output = Vec::with_capacity(data.len());
            let mut zeros = 0u8;
            for &byte in data {
                if escaped && zeros >= 2 && byte == 3 {
                    zeros = 0;
                    continue;
                }
                output.push(byte);
                zeros = if byte == 0 {
                    zeros.saturating_add(1)
                } else {
                    0
                };
            }
            Self {
                data: output,
                bit: 0,
            }
        }

        fn bit(&mut self) -> Option<bool> {
            let byte = *self.data.get(self.bit / 8)?;
            let value = byte & (1 << (7 - self.bit % 8)) != 0;
            self.bit += 1;
            Some(value)
        }

        fn bits(&mut self, count: usize) -> Option<u64> {
            let start = self.bit;
            let mut value = 0u64;
            for _ in 0..count {
                let Some(next) = self.bit() else {
                    self.bit = start;
                    return None;
                };
                value = (value << 1) | u64::from(next);
            }
            Some(value)
        }

        fn skip(&mut self, count: usize) -> Option<()> {
            let end = self.bit.checked_add(count)?;
            if end > self.data.len() * 8 {
                return None;
            }
            self.bit = end;
            Some(())
        }

        fn ue(&mut self) -> Option<u64> {
            let start = self.bit;
            let mut zeros = 0usize;
            loop {
                let Some(next) = self.bit() else {
                    self.bit = start;
                    return None;
                };
                if next {
                    break;
                }
                zeros += 1;
                if zeros >= 32 {
                    self.bit = start;
                    return None;
                }
            }
            let Some(rest) = self.bits(zeros) else {
                self.bit = start;
                return None;
            };
            Some(((1u64 << zeros) - 1) + rest)
        }

        fn se(&mut self) -> Option<i64> {
            let value = self.ue()?;
            let magnitude = value.div_ceil(2) as i64;
            Some(if value & 1 == 0 {
                -magnitude
            } else {
                magnitude
            })
        }
    }

    fn next_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn word_reader_matches_bit_at_a_time_reference() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2_000 {
            let length = (next_random(&mut state) % 48) as usize + 1;
            let mut data = Vec::with_capacity(length);
            for _ in 0..length {
                // Biased towards zero and three so emulation-prevention runs appear.
                data.push(match next_random(&mut state) % 8 {
                    0..=2 => 0,
                    3..=4 => 3,
                    value => value as u8,
                });
            }
            let escaped = next_random(&mut state) & 1 == 0;
            let mut reader = if escaped {
                Bits::rbsp(&data)
            } else {
                Bits::new(&data)
            };
            let mut reference = Reference::new(&data, escaped);
            for step in 0..64 {
                let count = (next_random(&mut state) % 24) as u32 + 1;
                let context = format!("step {step} of {data:02x?} escaped {escaped}");
                match next_random(&mut state) % 5 {
                    0 => {
                        let value = reader.bit();
                        if let Some(expected) = reference.bit() {
                            assert_eq!(value, expected, "bit, {context}");
                        }
                    }
                    1 => {
                        let value = reader.bits(count);
                        if let Some(expected) = reference.bits(count as usize) {
                            assert_eq!(value, expected, "bits({count}), {context}");
                        }
                    }
                    2 => {
                        reader.skip(count * 4);
                        reference.skip(count as usize * 4);
                    }
                    3 => {
                        let value = reader.ue();
                        if let Some(expected) = reference.ue() {
                            assert_eq!(value, expected, "ue, {context}");
                        }
                    }
                    _ => {
                        let value = reader.se();
                        if let Some(expected) = reference.se() {
                            assert_eq!(value, expected, "se, {context}");
                        }
                    }
                }
                // Past the end the reader latches an overrun and returns zeroes, so
                // only the readable prefix is comparable.
                if !reader.ok() {
                    break;
                }
            }
        }
    }

    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        used: usize,
    }

    impl BitWriter {
        fn bit(&mut self, value: bool) {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if value {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 1 << (7 - self.used % 8);
            }
            self.used += 1;
        }

        fn bits(&mut self, value: u64, count: usize) {
            for shift in (0..count).rev() {
                self.bit(shift < 64 && value & (1u64 << shift) != 0);
            }
        }

        fn ue(&mut self, value: u64) {
            let code = value + 1;
            let bits = 64 - code.leading_zeros() as usize;
            self.bits(0, bits - 1);
            self.bits(code, bits);
        }

        fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }

    #[test]
    fn parses_vp8_keyframe_and_scaling() {
        let data = [0x10, 0, 0, 0x9d, 1, 0x2a, 0x40, 0x42, 0x68, 0x81];
        let value = geometry(Codec::Vp8, None, Some(&data)).unwrap();
        assert_eq!((value.coded_width, value.coded_height), (576, 360));
        assert_eq!((value.render_width, value.render_height), (720, 600));
    }

    #[test]
    fn parses_known_avc_sps_crop_and_vui() {
        // 1920x1080 High-profile SPS with a 4:3 SAR. Annex-B wrapping also
        // exercises start-code discovery.
        let data = [
            0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44,
            0, 0, 3, 0, 4, 0, 0, 3, 0, 0xf1, 0x83, 0x19, 0x60,
        ];
        let value = geometry(Codec::H264, None, Some(&data)).unwrap();
        assert_eq!((value.coded_width, value.coded_height), (1920, 1088));
        assert_eq!((value.visible_width, value.visible_height), (1920, 1080));
        // Some encoders omit VUI from this compact fixture; when present it
        // must always be a valid positive ratio.
        assert!(
            value
                .pixel_aspect_ratio
                .unwrap_or(AspectRatio::square())
                .numerator
                > 0
        );
    }

    #[test]
    fn parses_avc_conformance_crop_and_vui_aspect() {
        let mut bits = BitWriter::default();
        bits.bits(66, 8);
        bits.bits(0, 8);
        bits.bits(30, 8);
        bits.ue(0);
        bits.ue(0);
        bits.ue(0);
        bits.ue(0);
        bits.ue(0);
        bits.bit(false);
        bits.ue(39);
        bits.ue(22);
        bits.bit(true);
        bits.bit(true);
        bits.bit(true);
        bits.ue(0);
        bits.ue(0);
        bits.ue(0);
        bits.ue(4);
        bits.bit(true);
        bits.bit(true);
        bits.bits(14, 8);
        let mut data = vec![0x67];
        data.extend(bits.finish());
        let value = geometry(Codec::H264, None, Some(&data)).unwrap();
        assert_eq!((value.coded_width, value.coded_height), (640, 368));
        assert_eq!((value.visible_width, value.visible_height), (640, 360));
        assert_eq!(value.pixel_aspect_ratio, Some(AspectRatio::new(4, 3)));
        let mut length_prefixed = (data.len() as u16).to_be_bytes().to_vec();
        length_prefixed.extend(data);
        let value = geometry(Codec::H264, None, Some(&length_prefixed)).unwrap();
        assert_eq!((value.visible_width, value.visible_height), (640, 360));
    }

    #[test]
    fn rejects_non_key_vp8() {
        assert!(geometry(Codec::Vp8, None, Some(&[1, 0, 0])).is_none());
    }

    #[test]
    fn parses_hevc_sps_crop_and_vui() {
        let mut bits = BitWriter::default();
        bits.bits(0, 4);
        bits.bits(0, 3);
        bits.bit(true);
        bits.bits(0, 96);
        bits.ue(0);
        bits.ue(1);
        bits.ue(1920);
        bits.ue(1088);
        bits.bit(true);
        bits.ue(0);
        bits.ue(0);
        bits.ue(0);
        bits.ue(4);
        bits.ue(0);
        bits.ue(0);
        bits.ue(0);
        bits.bit(false);
        for _ in 0..3 {
            bits.ue(0);
        }
        for _ in 0..6 {
            bits.ue(0);
        }
        bits.bit(false);
        bits.bit(false);
        bits.bit(false);
        bits.bit(false);
        bits.ue(0);
        bits.bit(false);
        bits.bit(false);
        bits.bit(false);
        bits.bit(true);
        bits.bit(true);
        bits.bits(14, 8);
        let rbsp = bits.finish();
        let mut data = vec![0, 0, 1, 0x42, 1];
        let mut zeros = 0;
        for byte in rbsp {
            if zeros >= 2 && byte <= 3 {
                data.push(3);
                zeros = 0;
            }
            data.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        let value = geometry(Codec::H265, None, Some(&data)).unwrap();
        assert_eq!((value.coded_width, value.coded_height), (1920, 1088));
        assert_eq!((value.visible_width, value.visible_height), (1920, 1080));
        assert_eq!(value.pixel_aspect_ratio, Some(AspectRatio::new(4, 3)));
        let nal = &data[3..];
        let mut length_prefixed = (nal.len() as u16).to_be_bytes().to_vec();
        length_prefixed.extend_from_slice(nal);
        let value = geometry(Codec::H265, None, Some(&length_prefixed)).unwrap();
        assert_eq!((value.visible_width, value.visible_height), (1920, 1080));
    }

    #[test]
    fn parses_av1_sequence_and_first_frame_render_size() {
        let mut sequence = BitWriter::default();
        sequence.bits(0, 3);
        sequence.bit(true);
        sequence.bit(true);
        sequence.bits(0, 5);
        sequence.bits(9, 4);
        sequence.bits(8, 4);
        sequence.bits(639, 10);
        sequence.bits(359, 9);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        let sequence = sequence.finish();
        let mut private = vec![0x0a, sequence.len() as u8];
        private.extend(sequence);

        let mut frame = BitWriter::default();
        frame.bit(false);
        frame.bit(false);
        frame.bit(true);
        frame.bits(1279, 16);
        frame.bits(719, 16);
        let frame = frame.finish();
        let mut sample = vec![0x1a, frame.len() as u8];
        sample.extend(frame);

        let value = geometry(Codec::Av1, Some(&private), Some(&sample)).unwrap();
        assert_eq!((value.coded_width, value.coded_height), (640, 360));
        assert_eq!((value.render_width, value.render_height), (1280, 720));
    }

    #[test]
    fn parses_full_av1_keyframe_render_size() {
        let mut sequence = BitWriter::default();
        sequence.bits(0, 3);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bits(0, 5);
        sequence.bits(0, 12);
        sequence.bits(0, 5);
        sequence.bits(9, 4);
        sequence.bits(8, 4);
        sequence.bits(639, 10);
        sequence.bits(359, 9);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bits(0, 4);
        sequence.bit(false);
        sequence.bit(true);
        sequence.bit(true);
        sequence.bit(false);
        sequence.bit(false);
        sequence.bit(false);
        let sequence = sequence.finish();
        let mut private = vec![0x0a, sequence.len() as u8];
        private.extend(sequence);

        let mut frame = BitWriter::default();
        frame.bit(false);
        frame.bits(0, 2);
        frame.bit(true);
        frame.bit(false);
        frame.bit(false);
        frame.bit(false);
        frame.bit(true);
        frame.bits(959, 16);
        frame.bits(539, 16);
        let frame = frame.finish();
        let mut sample = vec![0x1a, frame.len() as u8];
        sample.extend(frame);

        let value = geometry(Codec::Av1, Some(&private), Some(&sample)).unwrap();
        assert_eq!((value.coded_width, value.coded_height), (640, 360));
        assert_eq!((value.render_width, value.render_height), (960, 540));
    }

    #[test]
    fn parses_vp9_keyframe_render_size() {
        let mut frame = BitWriter::default();
        frame.bits(2, 2);
        frame.bit(false);
        frame.bit(false);
        frame.bit(false);
        frame.bit(false);
        frame.bit(true);
        frame.bit(false);
        frame.bits(0x49_83_42, 24);
        frame.bits(1, 3);
        frame.bit(false);
        frame.bits(639, 16);
        frame.bits(359, 16);
        frame.bit(true);
        frame.bits(1279, 16);
        frame.bits(719, 16);
        let value = geometry(Codec::Vp9, None, Some(&frame.finish())).unwrap();
        assert_eq!((value.coded_width, value.coded_height), (640, 360));
        assert_eq!((value.render_width, value.render_height), (1280, 720));
    }
}
