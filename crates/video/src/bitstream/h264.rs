//! H.264 Annex-B parsing: NAL iteration, SPS/PPS extraction, SPS decoding, and
//! the `avcC` descriptor and `avc1.*` codec string the browser decoder needs.

use super::{ExpGolombReader, find_nal_units};

pub const NAL_IDR: u8 = 5;
pub const NAL_SEI: u8 = 6;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// The NAL unit type (low 5 bits of the first byte), or 0 for an empty NAL.
pub fn nal_type(data: &[u8]) -> u8 {
    match data.first() {
        Some(byte) => byte & 0x1F,
        None => 0,
    }
}

/// Strips parameter-set, SEI, and AUD NALs and length-prefixes the remaining
/// NALs of one access unit with a 4-byte big-endian length. This is the `avcC`
/// sample format the browser feeds to `VideoDecoder`, with the parameter sets
/// carried in the `extra_data` descriptor instead of in band.
pub fn annex_b_to_avc(annex_b: &[u8]) -> Vec<u8> {
    let mut avc = Vec::with_capacity(annex_b.len());
    annex_b_to_avc_into(annex_b, &mut avc);
    avc
}

/// Appends the [`annex_b_to_avc`] conversion of one access unit to `out`, for
/// callers that reuse an encode buffer across frames.
pub fn annex_b_to_avc_into(annex_b: &[u8], out: &mut Vec<u8>) {
    for (start, end) in find_nal_units(annex_b) {
        let nal = &annex_b[start..end];
        match nal_type(nal) {
            NAL_SPS | NAL_PPS | NAL_AUD | NAL_SEI => continue,
            _ => {}
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
}

/// Extracts the first SPS and PPS NALs from an Annex-B access unit, or `None`
/// when either is absent.
pub fn extract_sps_pps(annex_b: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut sps = None;
    let mut pps = None;
    for (start, end) in find_nal_units(annex_b) {
        let nal = &annex_b[start..end];
        match nal_type(nal) {
            NAL_SPS if sps.is_none() => sps = Some(nal.to_vec()),
            NAL_PPS if pps.is_none() => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    match (sps, pps) {
        (Some(sps), Some(pps)) => Some((sps, pps)),
        _ => None,
    }
}

/// Fields decoded from an SPS: the profile/level identifiers, the chroma and
/// bit-depth parameters needed to build the `avcC` high-profile extension, and
/// the cropped picture dimensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpsInfo {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    pub width: u32,
    pub height: u32,
}

/// Profiles whose SPS carries the chroma/bit-depth block and whose `avcC` record
/// requires the trailing high-profile extension bytes.
fn is_high_profile(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

/// Decodes an SPS NAL into an [`SpsInfo`], or `None` if it is truncated.
pub fn parse_sps(sps: &[u8]) -> Option<SpsInfo> {
    if sps.len() < 4 {
        return None;
    }
    let profile_idc = sps[1];
    let constraint_flags = sps[2];
    let level_idc = sps[3];

    let mut reader = ExpGolombReader::new(&sps[4..]);
    let _seq_parameter_set_id = reader.read_ue()?;

    let mut chroma_format_idc = 1;
    let mut bit_depth_luma_minus8 = 0;
    let mut bit_depth_chroma_minus8 = 0;
    if is_high_profile(profile_idc) {
        chroma_format_idc = reader.read_ue()?;
        if chroma_format_idc == 3 {
            reader.read_bits(1)?;
        }
        bit_depth_luma_minus8 = reader.read_ue()?;
        bit_depth_chroma_minus8 = reader.read_ue()?;
        let _qpprime_y_zero_transform_bypass_flag = reader.read_bits(1)?;
        let seq_scaling_matrix_present_flag = reader.read_bits(1)?;
        if seq_scaling_matrix_present_flag == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                let present = reader.read_bits(1)?;
                if present == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    skip_scaling_list(&mut reader, size)?;
                }
            }
        }
    }

    let _log2_max_frame_num_minus4 = reader.read_ue()?;
    let pic_order_cnt_type = reader.read_ue()?;
    if pic_order_cnt_type == 0 {
        let _log2_max_pic_order_cnt_lsb_minus4 = reader.read_ue()?;
    } else if pic_order_cnt_type == 1 {
        let _delta_pic_order_always_zero_flag = reader.read_bits(1)?;
        let _offset_for_non_ref_pic = reader.read_se()?;
        let _offset_for_top_to_bottom_field = reader.read_se()?;
        let num_ref_frames_in_pic_order_cnt_cycle = reader.read_ue()?;
        for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
            let _offset_for_ref_frame = reader.read_se()?;
        }
    }

    let _max_num_ref_frames = reader.read_ue()?;
    let _gaps_in_frame_num_value_allowed_flag = reader.read_bits(1)?;
    let pic_width_in_mbs_minus1 = reader.read_ue()?;
    let pic_height_in_map_units_minus1 = reader.read_ue()?;
    let frame_mbs_only_flag = reader.read_bits(1)?;
    if frame_mbs_only_flag == 0 {
        let _mb_adaptive_frame_field_flag = reader.read_bits(1)?;
    }
    let _direct_8x8_inference_flag = reader.read_bits(1)?;
    let frame_cropping_flag = reader.read_bits(1)?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag == 1 {
        (
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
            reader.read_ue()?,
        )
    } else {
        (0, 0, 0, 0)
    };

    let width = (pic_width_in_mbs_minus1 + 1) * 16 - (crop_left + crop_right) * 2;
    let height_multiplier = if frame_mbs_only_flag == 1 { 1 } else { 2 };
    let height = (pic_height_in_map_units_minus1 + 1) * 16 * height_multiplier
        - (crop_top + crop_bottom) * 2;

    Some(SpsInfo {
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc,
        bit_depth_luma_minus8,
        bit_depth_chroma_minus8,
        width,
        height,
    })
}

fn skip_scaling_list(reader: &mut ExpGolombReader<'_>, size: usize) -> Option<()> {
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for _ in 0..size {
        if next_scale != 0 {
            let delta_scale = reader.read_se()?;
            next_scale = (last_scale + delta_scale + 256) % 256;
        }
        if next_scale != 0 {
            last_scale = next_scale;
        }
    }
    Some(())
}

/// Builds the `AVCDecoderConfigurationRecord` (`avcC`) the browser passes as the
/// decoder `description`, from one SPS and PPS NAL.
///
/// For high profiles the record carries a four-byte extension with the chroma
/// format and bit depths decoded from the SPS, matching the `avcC` box ffmpeg
/// writes. Firefox validates this strictly, so the values are decoded rather
/// than assumed.
pub fn build_avcc_extra_data(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut avcc = Vec::with_capacity(11 + sps.len() + pps.len());
    avcc.push(1); // configurationVersion
    avcc.push(sps.get(1).copied().unwrap_or(0)); // AVCProfileIndication
    avcc.push(sps.get(2).copied().unwrap_or(0)); // profile_compatibility
    avcc.push(sps.get(3).copied().unwrap_or(0)); // AVCLevelIndication
    avcc.push(0xFF); // 6 reserved bits + lengthSizeMinusOne = 3
    avcc.push(0xE1); // 3 reserved bits + numOfSequenceParameterSets = 1
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1); // numOfPictureParameterSets
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);

    if is_high_profile(sps.get(1).copied().unwrap_or(0))
        && let Some(info) = parse_sps(sps)
    {
        avcc.push(0xFC | (info.chroma_format_idc as u8 & 0x03));
        avcc.push(0xF8 | (info.bit_depth_luma_minus8 as u8 & 0x07));
        avcc.push(0xF8 | (info.bit_depth_chroma_minus8 as u8 & 0x07));
        avcc.push(0); // numOfSequenceParameterSetExt
    }
    avcc
}

/// Builds the WebCodecs `avc1.PPCCLL` codec string from an SPS NAL, where the
/// fields are the profile, constraint flags, and level in uppercase hex.
pub fn build_codec_string(sps: &[u8]) -> String {
    if sps.len() < 4 {
        return "avc1.000000".to_string();
    }
    format!("avc1.{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nals_with_3byte_start_codes() {
        let data = [0x00, 0x00, 0x01, 0x67, 0xAA, 0x00, 0x00, 0x01, 0x68, 0xBB];
        let nals: Vec<_> = find_nal_units(&data).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(&data[nals[0].0..nals[0].1], &[0x67, 0xAA]);
        assert_eq!(&data[nals[1].0..nals[1].1], &[0x68, 0xBB]);
    }

    #[test]
    fn finds_nals_with_mixed_start_codes() {
        let data = [
            0x00, 0x00, 0x00, 0x01, 0x67, 0xCC, 0x00, 0x00, 0x01, 0x68, 0xDD,
        ];
        let nals: Vec<_> = find_nal_units(&data).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(&data[nals[0].0..nals[0].1], &[0x67, 0xCC]);
        assert_eq!(&data[nals[1].0..nals[1].1], &[0x68, 0xDD]);
    }

    #[test]
    fn annex_b_to_avc_strips_parameter_sets_and_length_prefixes() {
        let annex_b = [
            0x00, 0x00, 0x01, 0x67, 0xAA, 0x00, 0x00, 0x01, 0x68, 0xBB, 0x00, 0x00, 0x01, 0x65,
            0xCC, 0xDD,
        ];
        let avc = annex_b_to_avc(&annex_b);
        // Only the IDR slice survives, prefixed by its 4-byte big-endian length.
        assert_eq!(avc, [0x00, 0x00, 0x00, 0x03, 0x65, 0xCC, 0xDD]);
    }

    #[test]
    fn extract_sps_pps_finds_both() {
        let data = [
            0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, 0xD9, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE,
            0x06, 0xE2,
        ];
        let (sps, pps) = extract_sps_pps(&data).unwrap();
        assert_eq!(nal_type(&sps), NAL_SPS);
        assert_eq!(nal_type(&pps), NAL_PPS);
    }

    #[test]
    fn build_codec_string_formats_sps_fields() {
        let sps = [0x67, 0x42, 0xC0, 0x1E];
        assert_eq!(build_codec_string(&sps), "avc1.42C01E");
    }

    #[test]
    fn build_avcc_baseline_has_no_high_profile_extension() {
        // profile_idc 0x42 (Baseline) carries no trailing extension.
        let sps = [0x67, 0x42, 0xC0, 0x1E];
        let pps = [0x68, 0xCE, 0x06];
        let avcc = build_avcc_extra_data(&sps, &pps);
        assert_eq!(avcc[0], 1);
        assert_eq!(&avcc[1..4], &[0x42, 0xC0, 0x1E]);
        assert_eq!(avcc[4], 0xFF);
        assert_eq!(avcc[5], 0xE1);
        assert_eq!(u16::from_be_bytes([avcc[6], avcc[7]]), sps.len() as u16);
        // 8 header bytes + sps + 1 (count) + 2 (len) + pps, no extension trailer.
        assert_eq!(avcc.len(), 8 + sps.len() + 3 + pps.len());
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Golden vectors captured from ffmpeg (`libx264 -f mp4`): the `avcC` box and
    // the SPS/PPS extracted from the matching Annex-B stream. The builder must
    // reproduce the box ffmpeg writes byte for byte, the reference Firefox accepts.

    #[test]
    fn build_avcc_matches_ffmpeg_baseline() {
        let sps = hex("6742c00dda0507ec0440000003004000000f03c50aa8");
        let pps = hex("68ce0fc8");
        let avcc =
            hex("0142c00dffe100166742c00dda0507ec0440000003004000000f03c50aa801000468ce0fc8");
        assert_eq!(build_avcc_extra_data(&sps, &pps), avcc);
        assert_eq!(build_codec_string(&sps), "avc1.42C00D");
        let info = parse_sps(&sps).unwrap();
        assert_eq!((info.width, info.height), (320, 240));
    }

    #[test]
    fn build_avcc_matches_ffmpeg_high_profile() {
        let sps = hex("6764001eacd940a02ff970110000030001000003003c0f162d96");
        let pps = hex("68ebe3cb22c0");
        let avcc = hex(
            "0164001effe1001a6764001eacd940a02ff970110000030001000003003c0f162d9601000668ebe3cb22c0fdf8f800",
        );
        // The trailing fd f8 f8 00 is the high-profile extension (4:2:0, 8-bit).
        assert_eq!(build_avcc_extra_data(&sps, &pps), avcc);
        assert_eq!(build_codec_string(&sps), "avc1.64001E");
        let info = parse_sps(&sps).unwrap();
        assert_eq!((info.width, info.height), (640, 360));
        assert_eq!(info.chroma_format_idc, 1);
        assert_eq!(info.bit_depth_luma_minus8, 0);
    }

    #[test]
    fn exp_golomb_reads_unsigned() {
        let mut reader = ExpGolombReader::new(&[0b1000_0000]);
        assert_eq!(reader.read_ue(), Some(0));
        let mut reader = ExpGolombReader::new(&[0b0100_0000]);
        assert_eq!(reader.read_ue(), Some(1));
        let mut reader = ExpGolombReader::new(&[0b0110_0000]);
        assert_eq!(reader.read_ue(), Some(2));
    }
}
