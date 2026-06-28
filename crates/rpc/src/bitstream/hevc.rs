//! HEVC (H.265) Annex-B parsing: NAL iteration, VPS/SPS/PPS extraction, SPS
//! decoding, and the `hvcC` descriptor and `hvc1.*` codec string the browser
//! decoder needs.
//!
//! HEVC differs from H.264 in the NAL header (two bytes, the type is bits 1..7 of
//! the first byte) and in carrying a VPS alongside the SPS and PPS. The SPS
//! parser ports the profile-tier-level and dimension fields from a full HEVC
//! parser, de-emulating the RBSP first so the bit reader sees clean values.

use super::{ExpGolombReader, find_nal_units};

pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;

/// NAL unit types below this are coded slices (VCL); at or above are parameter
/// sets, SEI, AUD, and other non-VCL units stripped from the wire body.
const FIRST_NON_VCL: u8 = 32;

/// The first IRAP (keyframe) NAL type. Types 16..=23 are BLA/IDR/CRA pictures.
pub const FIRST_IRAP: u8 = 16;
/// The last IRAP NAL type.
pub const LAST_IRAP: u8 = 23;

/// The HEVC NAL unit type: bits 1..7 of the first header byte, or 0 when empty.
pub fn nal_type(data: &[u8]) -> u8 {
    match data.first() {
        Some(byte) => (byte >> 1) & 0x3F,
        None => 0,
    }
}

/// True when a NAL type is an IRAP picture, which begins a decodable keyframe.
pub fn is_irap(nal_type: u8) -> bool {
    (FIRST_IRAP..=LAST_IRAP).contains(&nal_type)
}

/// Strips non-VCL NALs (VPS/SPS/PPS/SEI/AUD) and length-prefixes the remaining
/// coded-slice NALs of one access unit with a 4-byte big-endian length. This is
/// the `hvc1` sample format the browser feeds to `VideoDecoder`, with the
/// parameter sets carried in the `hvcC` descriptor instead of in band.
pub fn annex_b_to_hvc(annex_b: &[u8]) -> Vec<u8> {
    let mut hvc = Vec::with_capacity(annex_b.len());
    for (start, end) in find_nal_units(annex_b) {
        let nal = &annex_b[start..end];
        if nal_type(nal) >= FIRST_NON_VCL {
            continue;
        }
        hvc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        hvc.extend_from_slice(nal);
    }
    hvc
}

/// Extracts the first VPS, SPS, and PPS NALs from an Annex-B access unit, or
/// `None` when any is absent.
pub fn extract_param_sets(annex_b: &[u8]) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;
    for (start, end) in find_nal_units(annex_b) {
        let nal = &annex_b[start..end];
        match nal_type(nal) {
            NAL_VPS if vps.is_none() => vps = Some(nal.to_vec()),
            NAL_SPS if sps.is_none() => sps = Some(nal.to_vec()),
            NAL_PPS if pps.is_none() => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    match (vps, sps, pps) {
        (Some(vps), Some(sps), Some(pps)) => Some((vps, sps, pps)),
        _ => None,
    }
}

/// Removes emulation-prevention bytes (`00 00 03` → `00 00`) from a NAL, turning
/// its EBSP into the raw RBSP the bit reader parses.
fn ebsp_to_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u8;
    for &byte in data {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(byte);
    }
    out
}

/// Fields decoded from an HEVC SPS: the raw 12-byte general profile-tier-level
/// block (copied straight into `hvcC` and the codec string), the chroma and
/// bit-depth parameters, temporal-layer info, and the cropped dimensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HevcSpsInfo {
    pub general_ptl: [u8; 12],
    pub chroma_format_idc: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub max_sub_layers_minus1: u8,
    pub temporal_id_nesting: bool,
    pub width: u32,
    pub height: u32,
}

/// Decodes an SPS NAL into a [`HevcSpsInfo`], or `None` if it is truncated or
/// malformed. The 12-byte general profile-tier-level block is byte-aligned right
/// after the one-byte SPS header, so it is sliced directly, and the bit reader
/// resumes past it to reach the chroma, bit-depth, and dimension fields.
pub fn parse_sps(sps: &[u8]) -> Option<HevcSpsInfo> {
    let rbsp = ebsp_to_rbsp(sps);
    // 2-byte NAL header, then sps_video_parameter_set_id(4),
    // sps_max_sub_layers_minus1(3), sps_temporal_id_nesting_flag(1).
    let header = *rbsp.get(2)?;
    let max_sub_layers_minus1 = (header >> 1) & 0x07;
    let temporal_id_nesting = (header & 1) == 1;
    let general_ptl: [u8; 12] = rbsp.get(3..15)?.try_into().ok()?;

    // The general PTL is 96 bits and byte-aligned, so the sub-layer PTL and the
    // rest of the SPS begin at byte 15.
    let mut reader = ExpGolombReader::new(rbsp.get(15..)?);
    if max_sub_layers_minus1 > 0 {
        let mut profile_present = [false; 8];
        let mut level_present = [false; 8];
        for i in 0..max_sub_layers_minus1 as usize {
            profile_present[i] = reader.read_bit()? == 1;
            level_present[i] = reader.read_bit()? == 1;
        }
        for _ in max_sub_layers_minus1..8 {
            reader.skip_bits(2)?;
        }
        for i in 0..max_sub_layers_minus1 as usize {
            if profile_present[i] {
                reader.skip_bits(88)?;
            }
            if level_present[i] {
                reader.skip_bits(8)?;
            }
        }
    }

    let _sps_seq_parameter_set_id = reader.read_ue()?;
    let chroma_format_idc = reader.read_ue()?;
    if chroma_format_idc == 3 {
        reader.read_bit()?;
    }
    let pic_width = reader.read_ue()?;
    let pic_height = reader.read_ue()?;
    let conformance_window_flag = reader.read_bit()? == 1;
    let (left, right, top, bottom) = if conformance_window_flag {
        (
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };
    let bit_depth_luma_minus8 = reader.read_ue()?;
    let bit_depth_chroma_minus8 = reader.read_ue()?;

    let (sub_width_c, sub_height_c) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let width = pic_width - sub_width_c * (left + right);
    let height = pic_height - sub_height_c * (top + bottom);

    Some(HevcSpsInfo {
        general_ptl,
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth_luma_minus8: bit_depth_luma_minus8 as u8,
        bit_depth_chroma_minus8: bit_depth_chroma_minus8 as u8,
        max_sub_layers_minus1,
        temporal_id_nesting,
        width,
        height,
    })
}

/// Builds the `HEVCDecoderConfigurationRecord` (`hvcC`) the browser passes as the
/// decoder `description`, from the VPS, SPS, and PPS NALs plus the SPS fields.
///
/// The general profile-tier-level bytes, chroma format, and bit depths are copied
/// from the decoded SPS so the record matches the `hvcC` box ffmpeg writes. The
/// parameter sets are stored as NAL-unit arrays with `lengthSizeMinusOne = 3`.
pub fn build_hvcc_extra_data(vps: &[u8], sps: &[u8], pps: &[u8], info: &HevcSpsInfo) -> Vec<u8> {
    let mut out = Vec::with_capacity(40 + vps.len() + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.extend_from_slice(&info.general_ptl); // 12 bytes: profile, compat, constraints, level
    out.extend_from_slice(&[0xF0, 0x00]); // reserved + min_spatial_segmentation_idc = 0
    out.push(0xFC); // reserved + parallelismType = 0
    out.push(0xFC | (info.chroma_format_idc & 0x03));
    out.push(0xF8 | (info.bit_depth_luma_minus8 & 0x07));
    out.push(0xF8 | (info.bit_depth_chroma_minus8 & 0x07));
    out.extend_from_slice(&[0x00, 0x00]); // avgFrameRate = 0
    // constantFrameRate(2)=0, numTemporalLayers(3), temporalIdNested(1),
    // lengthSizeMinusOne(2)=3.
    let num_temporal_layers = info.max_sub_layers_minus1 + 1;
    out.push((num_temporal_layers << 3) | (u8::from(info.temporal_id_nesting) << 2) | 0x03);
    out.push(3); // numOfArrays: VPS, SPS, PPS

    for (nal_type, nal) in [(NAL_VPS, vps), (NAL_SPS, sps), (NAL_PPS, pps)] {
        out.push(nal_type); // array_completeness = 0, reserved = 0, NAL_unit_type
        out.extend_from_slice(&1u16.to_be_bytes()); // numNalus = 1
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// Builds the WebCodecs `hvc1.*` codec string from the decoded SPS fields,
/// following ISO 14496-15: profile space and idc, the bit-reversed compatibility
/// flags, the tier and level, and the non-zero constraint bytes.
pub fn build_codec_string(info: &HevcSpsInfo) -> String {
    let ptl = &info.general_ptl;
    let profile_space = (ptl[0] >> 6) & 0x03;
    let tier_flag = (ptl[0] >> 5) & 0x01;
    let profile_idc = ptl[0] & 0x1F;
    let compatibility = u32::from_be_bytes([ptl[1], ptl[2], ptl[3], ptl[4]]);
    let constraints = &ptl[5..11];
    let level_idc = ptl[11];

    let mut codec = String::from("hvc1.");
    if profile_space > 0 {
        codec.push((b'A' + profile_space - 1) as char);
    }
    codec.push_str(&profile_idc.to_string());
    codec.push('.');
    codec.push_str(&format!("{:X}", compatibility.reverse_bits()));
    codec.push('.');
    codec.push(if tier_flag == 0 { 'L' } else { 'H' });
    codec.push_str(&level_idc.to_string());
    if let Some(last) = constraints.iter().rposition(|&byte| byte != 0) {
        for &byte in &constraints[..=last] {
            codec.push('.');
            codec.push_str(&format!("{byte:02X}"));
        }
    }
    codec
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Golden vectors captured from ffmpeg (`libx265 -tag:v hvc1 -f mp4`): the
    // VPS/SPS/PPS from the Annex-B stream, the fixed `hvcC` prefix, and the codec
    // string ffmpeg reports in an HLS master playlist (`hvc1.1.6.L63.90`).
    const VPS: &str = "40010c01ffff01600000030090000003000003003f959809";
    const SPS: &str =
        "42010101600000030090000003000003003fa00502016965959a4932bc05a020000003002000000303c1";
    const PPS: &str = "4401c172b46240";

    #[test]
    fn nal_type_decodes_high_bits() {
        assert_eq!(nal_type(&[0x40, 0x01]), NAL_VPS);
        assert_eq!(nal_type(&[0x42, 0x01]), NAL_SPS);
        assert_eq!(nal_type(&[0x44, 0x01]), NAL_PPS);
        assert_eq!(nal_type(&[0x26, 0x01]), 19); // IDR_W_RADL
        assert!(is_irap(19));
        assert!(!is_irap(1));
    }

    #[test]
    fn parse_sps_reads_profile_and_dimensions() {
        let info = parse_sps(&hex(SPS)).unwrap();
        assert_eq!((info.width, info.height), (640, 360));
        assert_eq!(info.chroma_format_idc, 1);
        assert_eq!(info.bit_depth_luma_minus8, 0);
        assert_eq!(info.max_sub_layers_minus1, 0);
        // general profile-tier-level: Main profile (idc 1), level 63 (2.1).
        assert_eq!(info.general_ptl[0] & 0x1F, 1);
        assert_eq!(info.general_ptl[11], 63);
    }

    #[test]
    fn build_codec_string_matches_ffmpeg() {
        let info = parse_sps(&hex(SPS)).unwrap();
        assert_eq!(build_codec_string(&info), "hvc1.1.6.L63.90");
    }

    #[test]
    fn build_hvcc_prefix_matches_ffmpeg() {
        let info = parse_sps(&hex(SPS)).unwrap();
        let hvcc = build_hvcc_extra_data(&hex(VPS), &hex(SPS), &hex(PPS), &info);
        // The decode-critical fixed prefix (through numTemporalLayers/lengthSize)
        // must match the `hvcC` box ffmpeg writes byte for byte.
        let expected_prefix = hex("0101600000009000000000003ff000fcfdf8f800000f");
        assert_eq!(&hvcc[..expected_prefix.len()], &expected_prefix[..]);
        assert_eq!(hvcc[22], 3); // numOfArrays
        // The three parameter-set arrays follow, in VPS, SPS, PPS order.
        assert_eq!(hvcc[23], NAL_VPS);
    }

    #[test]
    fn annex_b_to_hvc_strips_parameter_sets() {
        // VPS, SPS, PPS, then an IDR slice (type 19, header byte 0x26).
        let mut annex_b = Vec::new();
        for nal in [hex(VPS), hex(SPS), hex(PPS), vec![0x26, 0x01, 0xAA, 0xBB]] {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(&nal);
        }
        let hvc = annex_b_to_hvc(&annex_b);
        // Only the slice survives, prefixed by its 4-byte length.
        assert_eq!(hvc, vec![0, 0, 0, 4, 0x26, 0x01, 0xAA, 0xBB]);
    }
}
