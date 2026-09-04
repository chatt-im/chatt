//! Video bitstream helpers shared by client and server.
//!
//! The publisher parses one keyframe's parameter sets to derive the WebCodecs
//! codec string and the `extra_data` descriptor (`avcC` for H.264), then converts
//! every Annex-B access unit to the length-prefixed form a `VideoDecoder`
//! expects. Doing this in Rust keeps the browser a codec-agnostic sink: it
//! configures from the codec string and `extra_data` and feeds frame bodies
//! straight through, with no in-browser NAL parsing.

pub mod h264;
pub mod hevc;

/// The video codec a screen share carries, selected by the capture command. The
/// publisher dispatches parameter-set parsing and frame conversion on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

/// The codec metadata derived from a keyframe: the WebCodecs codec string, the
/// `extra_data` descriptor (`avcC`/`hvcC`), and the cropped picture dimensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamParams {
    pub codec: String,
    pub extra_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Derives the codec string, `extra_data`, and dimensions from one keyframe
/// access unit, or `None` when it carries no parameter sets.
pub fn parse_keyframe(codec: Codec, access_unit: &[u8]) -> Option<StreamParams> {
    match codec {
        Codec::H264 => {
            let (sps, pps) = h264::extract_sps_pps(access_unit)?;
            let info = h264::parse_sps(&sps);
            Some(StreamParams {
                codec: h264::build_codec_string(&sps),
                extra_data: h264::build_avcc_extra_data(&sps, &pps),
                width: info.as_ref().map_or(0, |info| info.width),
                height: info.as_ref().map_or(0, |info| info.height),
            })
        }
        Codec::Hevc => {
            let (vps, sps, pps) = hevc::extract_param_sets(access_unit)?;
            let info = hevc::parse_sps(&sps)?;
            Some(StreamParams {
                codec: hevc::build_codec_string(&info),
                extra_data: hevc::build_hvcc_extra_data(&vps, &sps, &pps, &info),
                width: info.width,
                height: info.height,
            })
        }
    }
}

/// Strips parameter-set/SEI/AUD NALs and length-prefixes the remaining NALs of
/// one Annex-B access unit, the form the browser `VideoDecoder` expects.
pub fn annex_b_to_length_prefixed(codec: Codec, access_unit: &[u8]) -> Vec<u8> {
    match codec {
        Codec::H264 => h264::annex_b_to_avc(access_unit),
        Codec::Hevc => hevc::annex_b_to_hvc(access_unit),
    }
}

/// Appends the [`annex_b_to_length_prefixed`] conversion of one access unit to
/// `out`, for callers that reuse an encode buffer across frames.
pub fn annex_b_to_length_prefixed_into(codec: Codec, access_unit: &[u8], out: &mut Vec<u8>) {
    match codec {
        Codec::H264 => h264::annex_b_to_avc_into(access_unit, out),
        Codec::Hevc => hevc::annex_b_to_hvc_into(access_unit, out),
    }
}

/// Converts a WebCodecs length-prefixed access unit back to Annex-B in place.
///
/// Screen sharing always negotiates four-byte NAL lengths, exactly the width of
/// the Annex-B start code that replaces each length. Callers can therefore read
/// a frame directly into its final output allocation and patch only the length
/// fields, without allocating or copying the video payload.
pub fn length_prefixed_to_annex_b_in_place(access_unit: &mut [u8]) -> Result<(), String> {
    let mut cursor = 0usize;
    while cursor < access_unit.len() {
        let length_bytes = access_unit
            .get(cursor..cursor + 4)
            .ok_or_else(|| "truncated video NAL length".to_string())?;
        let length = u32::from_be_bytes(length_bytes.try_into().unwrap()) as usize;
        access_unit[cursor..cursor + 4].copy_from_slice(&[0, 0, 0, 1]);
        cursor += 4;
        if length == 0 || cursor.saturating_add(length) > access_unit.len() {
            return Err("invalid video NAL length".into());
        }
        cursor += length;
    }
    if access_unit.is_empty() {
        return Err("video access unit contains no NAL units".into());
    }
    Ok(())
}

/// Converts a captured access unit using the NAL length width from its decoder
/// configuration. Four-byte lengths reuse the existing allocation.
pub fn length_prefixed_to_annex_b(
    mut access_unit: Vec<u8>,
    length_size: usize,
) -> Result<Vec<u8>, String> {
    if length_size == 4 {
        length_prefixed_to_annex_b_in_place(&mut access_unit)?;
        return Ok(access_unit);
    }
    if !(1..=3).contains(&length_size) {
        return Err("invalid video NAL length size".into());
    }
    let mut remaining = access_unit.as_slice();
    let mut out = Vec::with_capacity(access_unit.len());
    while !remaining.is_empty() {
        let length_bytes = remaining
            .get(..length_size)
            .ok_or_else(|| "truncated video NAL length".to_string())?;
        let length = length_bytes.iter().fold(0usize, |len, byte| (len << 8) | usize::from(*byte));
        remaining = &remaining[length_size..];
        if length == 0 || length > remaining.len() {
            return Err("invalid video NAL length".into());
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&remaining[..length]);
        remaining = &remaining[length..];
    }
    if out.is_empty() {
        return Err("video access unit contains no NAL units".into());
    }
    Ok(out)
}

/// Reads the packet NAL length width from an `avcC` or `hvcC` descriptor.
pub fn configuration_nal_length_size(codec: Codec, configuration: &[u8]) -> Result<usize, String> {
    let (minimum_len, offset) = match codec {
        Codec::H264 => (7, 4),
        Codec::Hevc => (23, 21),
    };
    if configuration.len() < minimum_len || configuration[0] != 1 {
        return Err("invalid video decoder configuration".into());
    }
    Ok(usize::from(configuration[offset] & 3) + 1)
}

/// Extracts parameter sets from an `avcC` or `hvcC` decoder descriptor and
/// emits the Annex-B extradata expected by NUT/FFmpeg.
pub fn configuration_to_annex_b(codec: Codec, configuration: &[u8]) -> Result<Vec<u8>, String> {
    match codec {
        Codec::H264 => avcc_to_annex_b(configuration),
        Codec::Hevc => hvcc_to_annex_b(configuration),
    }
}

fn avcc_to_annex_b(configuration: &[u8]) -> Result<Vec<u8>, String> {
    configuration_nal_length_size(Codec::H264, configuration)?;
    let mut cursor = 6usize;
    let sps_count = configuration[5] & 0x1f;
    let mut out = Vec::new();
    for _ in 0..sps_count {
        append_configuration_nal(configuration, &mut cursor, &mut out)?;
    }
    let pps_count = *configuration
        .get(cursor)
        .ok_or_else(|| "truncated avcC PPS count".to_string())?;
    cursor += 1;
    for _ in 0..pps_count {
        append_configuration_nal(configuration, &mut cursor, &mut out)?;
    }
    if out.is_empty() {
        return Err("avcC contains no parameter sets".into());
    }
    Ok(out)
}

fn hvcc_to_annex_b(configuration: &[u8]) -> Result<Vec<u8>, String> {
    configuration_nal_length_size(Codec::Hevc, configuration)?;
    let arrays = configuration[22] as usize;
    let mut cursor = 23usize;
    let mut out = Vec::new();
    for _ in 0..arrays {
        cursor = cursor
            .checked_add(1)
            .filter(|cursor| *cursor + 2 <= configuration.len())
            .ok_or_else(|| "truncated hvcC array".to_string())?;
        let count = u16::from_be_bytes(configuration[cursor..cursor + 2].try_into().unwrap());
        cursor += 2;
        for _ in 0..count {
            append_configuration_nal(configuration, &mut cursor, &mut out)?;
        }
    }
    if out.is_empty() {
        return Err("hvcC contains no parameter sets".into());
    }
    Ok(out)
}

fn append_configuration_nal(
    configuration: &[u8],
    cursor: &mut usize,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let length_bytes = configuration
        .get(*cursor..*cursor + 2)
        .ok_or_else(|| "truncated decoder configuration NAL length".to_string())?;
    let length = u16::from_be_bytes(length_bytes.try_into().unwrap()) as usize;
    *cursor += 2;
    let nal = configuration
        .get(*cursor..*cursor + length)
        .ok_or_else(|| "truncated decoder configuration NAL".to_string())?;
    if nal.is_empty() {
        return Err("empty decoder configuration NAL".into());
    }
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal);
    *cursor += length;
    Ok(())
}

/// Iterates the `(start, end)` byte ranges of each NAL in an Annex-B stream,
/// handling both 3- and 4-byte start codes. The ranges exclude the start codes.
/// The NAL header layout differs by codec, so the type is decoded per codec.
pub(crate) fn find_nal_units(data: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    NalIterator {
        data,
        pos: 0,
        start: None,
    }
}

struct NalIterator<'a> {
    data: &'a [u8],
    pos: usize,
    start: Option<usize>,
}

impl Iterator for NalIterator<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        while self.pos < self.data.len() {
            if self.pos + 2 < self.data.len()
                && self.data[self.pos] == 0x00
                && self.data[self.pos + 1] == 0x00
            {
                let (is_start_code, code_len) = if self.data[self.pos + 2] == 0x01 {
                    (true, 3)
                } else if self.pos + 3 < self.data.len()
                    && self.data[self.pos + 2] == 0x00
                    && self.data[self.pos + 3] == 0x01
                {
                    (true, 4)
                } else {
                    (false, 0)
                };

                if is_start_code {
                    let end_pos = self.pos;
                    let nal_start = self.pos + code_len;
                    self.pos = nal_start;
                    if let Some(start) = self.start.take() {
                        self.start = Some(nal_start);
                        return Some((start, end_pos));
                    }
                    self.start = Some(nal_start);
                    continue;
                }
            }
            self.pos += 1;
        }
        self.start.take().map(|start| (start, self.data.len()))
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    #[test]
    fn converts_capture_nal_lengths_of_each_width() {
        for length_size in 1..=4 {
            let mut packet = Vec::new();
            for nal in [&[0x65, 0x88, 0x84][..], &[0x41, 0x9a]] {
                let length = (nal.len() as u32).to_be_bytes();
                packet.extend_from_slice(&length[4 - length_size..]);
                packet.extend_from_slice(nal);
            }
            assert_eq!(
                length_prefixed_to_annex_b(packet, length_size).unwrap(),
                [0, 0, 0, 1, 0x65, 0x88, 0x84, 0, 0, 0, 1, 0x41, 0x9a],
            );
            assert!(length_prefixed_to_annex_b(Vec::new(), length_size).is_err());
            assert!(length_prefixed_to_annex_b(vec![0; length_size], length_size).is_err());
            let mut truncated = vec![0; length_size];
            truncated[length_size - 1] = 2;
            truncated.push(0x65);
            assert!(length_prefixed_to_annex_b(truncated, length_size).is_err());
            assert!(length_prefixed_to_annex_b(vec![0; length_size - 1], length_size).is_err());
        }
        for length_size in [0, 5, usize::MAX] {
            assert!(length_prefixed_to_annex_b(vec![1], length_size).is_err());
        }
    }

    #[test]
    fn converts_nal_length_that_resembles_an_annex_b_start_code() {
        let mut packet = vec![0, 0, 1, 0];
        packet.extend_from_slice(&[0x65; 256]);
        let converted = length_prefixed_to_annex_b(packet, 4).unwrap();
        assert_eq!(&converted[..4], &[0, 0, 0, 1]);
        assert_eq!(&converted[4..], &[0x65; 256]);
    }

    #[test]
    fn length_prefixed_access_unit_round_trips_to_annex_b() {
        let annex_b = [0, 0, 0, 1, 0x65, 0x88, 0x84];
        let mut length_prefixed = annex_b_to_length_prefixed(Codec::H264, &annex_b);
        let pointer = length_prefixed.as_ptr();
        let capacity = length_prefixed.capacity();
        length_prefixed_to_annex_b_in_place(&mut length_prefixed).unwrap();
        assert_eq!(length_prefixed, annex_b);
        assert_eq!(length_prefixed.as_ptr(), pointer);
        assert_eq!(length_prefixed.capacity(), capacity);

        let mut truncated = vec![0, 0, 0, 8, 1];
        assert!(length_prefixed_to_annex_b_in_place(&mut truncated).is_err());
        let mut zero_length = vec![0, 0, 0, 0];
        assert!(length_prefixed_to_annex_b_in_place(&mut zero_length).is_err());
        assert!(length_prefixed_to_annex_b_in_place(&mut []).is_err());
    }

    #[test]
    fn avcc_parameter_sets_convert_to_annex_b() {
        let sps = [0x67, 0x42, 0xc0, 0x1e];
        let pps = [0x68, 0xce, 0x06];
        let avcc = h264::build_avcc_extra_data(&sps, &pps);
        assert_eq!(
            configuration_to_annex_b(Codec::H264, &avcc).unwrap(),
            [&[0, 0, 0, 1], sps.as_slice(), &[0, 0, 0, 1], pps.as_slice(),].concat()
        );
    }
}

/// Reads bits, Exp-Golomb `ue(v)`, and signed `se(v)` fields from a NAL's RBSP,
/// most-significant bit first. Emulation-prevention bytes are read as-is, which
/// is sound for the leading fields the parameter-set parsers consume.
pub(crate) struct ExpGolombReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> ExpGolombReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
        }
    }

    pub(crate) fn read_bit(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.byte_pos)?;
        let bit = (byte >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Some(bit)
    }

    pub(crate) fn read_bits(&mut self, n: u8) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..n {
            value = (value << 1) | self.read_bit()? as u32;
        }
        Some(value)
    }

    /// Discards `n` bits, for fields whose value is not needed.
    pub(crate) fn skip_bits(&mut self, n: u32) -> Option<()> {
        for _ in 0..n {
            self.read_bit()?;
        }
        Some(())
    }

    pub(crate) fn read_ue(&mut self) -> Option<u32> {
        let mut leading_zeros = 0u8;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return None;
            }
        }
        if leading_zeros == 0 {
            return Some(0);
        }
        let suffix = self.read_bits(leading_zeros)?;
        Some((1 << leading_zeros) - 1 + suffix)
    }

    pub(crate) fn read_se(&mut self) -> Option<i32> {
        let ue = self.read_ue()?;
        let value = if ue % 2 == 0 {
            -((ue / 2) as i32)
        } else {
            (ue.wrapping_add(1) / 2) as i32
        };
        Some(value)
    }
}
