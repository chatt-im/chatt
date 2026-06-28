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
