use std::fs;
use std::io::Write;
use std::path::Path;

const MAGIC: &[u8; 8] = b"CHTDNS2\0";
const VERSION: u32 = 2;
const HEADER_LEN: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DnnWeightKind {
    I8,
    U8,
    I32,
    F32,
}

pub(crate) struct DnnArtifact<'a> {
    files: Vec<DnnFile<'a>>,
}

pub(crate) struct DnnFile<'a> {
    pub(crate) path: &'a str,
    pub(crate) segments: Vec<DnnSegment<'a>>,
}

#[derive(Clone, Copy)]
pub(crate) enum DnnSegment<'a> {
    Text(&'a [u8]),
    Weights {
        kind: DnnWeightKind,
        element_count: usize,
        data: &'a [u8],
    },
}

impl DnnWeightKind {
    fn from_tag(tag: u8) -> Result<Self, String> {
        match tag {
            1 => Ok(Self::I8),
            2 => Ok(Self::U8),
            3 => Ok(Self::I32),
            4 => Ok(Self::F32),
            _ => Err(format!("unknown DNN segment tag {tag}")),
        }
    }

    fn element_size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I32 | Self::F32 => 4,
        }
    }

    fn alignment(self) -> usize {
        self.element_size()
    }
}

impl<'a> DnnArtifact<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        let mut cursor = 0usize;
        let magic = take(bytes, &mut cursor, MAGIC.len())?;
        if magic != MAGIC {
            return Err("DNN artifact magic does not match".to_string());
        }
        let version = take_u32(bytes, &mut cursor)?;
        if version != VERSION {
            return Err(format!("unsupported DNN artifact version {version}"));
        }
        let file_count = usize_from_u32(take_u32(bytes, &mut cursor)?, "file count")?;
        if cursor != HEADER_LEN {
            return Err("DNN artifact header length mismatch".to_string());
        }

        let mut files = Vec::with_capacity(file_count);
        for _ in 0..file_count {
            let path_len = usize_from_u32(take_u32(bytes, &mut cursor)?, "path length")?;
            let path = take_str(bytes, &mut cursor, path_len)?;
            let segment_count = usize_from_u32(take_u32(bytes, &mut cursor)?, "segment count")?;
            let mut segments = Vec::with_capacity(segment_count);

            for _ in 0..segment_count {
                let tag = take_u8(bytes, &mut cursor)?;
                if tag == 0 {
                    let len = usize_from_u64(take_u64(bytes, &mut cursor)?, "text length")?;
                    segments.push(DnnSegment::Text(take(bytes, &mut cursor, len)?));
                    continue;
                }

                let kind = DnnWeightKind::from_tag(tag)?;
                let element_count = usize_from_u64(take_u64(bytes, &mut cursor)?, "element count")?;
                let data_len = usize_from_u64(take_u64(bytes, &mut cursor)?, "data length")?;
                let expected_len = checked_mul(element_count, kind.element_size(), "data length")?;
                if data_len != expected_len {
                    return Err(format!(
                        "DNN segment has {data_len} bytes, expected {expected_len}"
                    ));
                }
                cursor = align_up(cursor, kind.alignment())?;
                let data_offset = cursor;
                let data = take(bytes, &mut cursor, data_len)?;
                if data_offset % kind.alignment() != 0 {
                    return Err(format!(
                        "DNN segment data offset {data_offset} is not {}-byte aligned",
                        kind.alignment()
                    ));
                }
                segments.push(DnnSegment::Weights {
                    kind,
                    element_count,
                    data,
                });
            }

            files.push(DnnFile { path, segments });
        }

        if cursor != bytes.len() {
            return Err(format!(
                "DNN artifact has {} trailing bytes",
                bytes.len() - cursor
            ));
        }

        Ok(Self { files })
    }

    pub(crate) fn files(&self) -> &[DnnFile<'a>] {
        &self.files
    }
}

#[allow(dead_code)]
pub(crate) fn expand_into(opus_root: &Path, artifact_path: &Path) -> Result<(), String> {
    let bytes = fs::read(artifact_path)
        .map_err(|err| format!("failed to read {}: {err}", artifact_path.display()))?;
    let artifact = DnnArtifact::parse(&bytes)?;
    for file in artifact.files() {
        let expanded = expand_file(file)?;
        let output_path = opus_root.join(file.path);
        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        fs::write(&output_path, expanded)
            .map_err(|err| format!("failed to write {}: {err}", output_path.display()))?;
    }
    Ok(())
}

pub(crate) fn expand_file(file: &DnnFile<'_>) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(expanded_capacity_hint(file)?);
    for segment in &file.segments {
        match *segment {
            DnnSegment::Text(text) => output.extend_from_slice(text),
            DnnSegment::Weights { kind, data, .. } => emit_c_values(&mut output, kind, data)?,
        }
    }
    Ok(output)
}

fn expanded_capacity_hint(file: &DnnFile<'_>) -> Result<usize, String> {
    let mut capacity = 0usize;
    for segment in &file.segments {
        match *segment {
            DnnSegment::Text(text) => {
                capacity = capacity
                    .checked_add(text.len())
                    .ok_or_else(|| "expanded file capacity overflows usize".to_string())?;
            }
            DnnSegment::Weights {
                kind,
                element_count,
                ..
            } => {
                let per_value = match kind {
                    DnnWeightKind::I8 => 7,
                    DnnWeightKind::U8 => 6,
                    DnnWeightKind::I32 => 14,
                    DnnWeightKind::F32 => 20,
                };
                let values = element_count
                    .checked_mul(per_value)
                    .ok_or_else(|| "expanded value capacity overflows usize".to_string())?;
                let lines = element_count
                    .checked_div(match kind {
                        DnnWeightKind::I8 | DnnWeightKind::U8 => 16,
                        DnnWeightKind::I32 | DnnWeightKind::F32 => 8,
                    })
                    .unwrap_or(0)
                    .checked_mul(5)
                    .ok_or_else(|| "expanded line capacity overflows usize".to_string())?;
                capacity = capacity
                    .checked_add(values)
                    .and_then(|value| value.checked_add(lines))
                    .ok_or_else(|| "expanded file capacity overflows usize".to_string())?;
            }
        }
    }
    Ok(capacity)
}

fn emit_c_values(output: &mut Vec<u8>, kind: DnnWeightKind, data: &[u8]) -> Result<(), String> {
    let element_size = kind.element_size();
    if data.len() % element_size != 0 {
        return Err("array data length is not divisible by element size".to_string());
    }
    let columns = match kind {
        DnnWeightKind::I8 | DnnWeightKind::U8 => 16,
        DnnWeightKind::I32 | DnnWeightKind::F32 => 8,
    };
    for (index, chunk) in data.chunks_exact(element_size).enumerate() {
        if index % columns == 0 {
            output.extend_from_slice(b"    ");
        } else {
            output.push(b' ');
        }
        match kind {
            DnnWeightKind::I8 => push_i32(output, i32::from(chunk[0] as i8)),
            DnnWeightKind::U8 => push_u32(output, u32::from(chunk[0])),
            DnnWeightKind::I32 => {
                let value = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                push_i32(output, value);
            }
            DnnWeightKind::F32 => {
                let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                write!(output, "{value:?}f")
                    .map_err(|err| format!("failed to format DNN float: {err}"))?;
            }
        }
        output.push(b',');
        if index % columns == columns - 1 {
            output.push(b'\n');
        }
    }
    if !output.ends_with(b"\n") {
        output.push(b'\n');
    }
    Ok(())
}

fn push_i32(output: &mut Vec<u8>, value: i32) {
    if value < 0 {
        output.push(b'-');
    }
    push_u32(output, value.unsigned_abs());
}

fn push_u32(output: &mut Vec<u8>, mut value: u32) {
    let mut buf = [0u8; 10];
    let mut cursor = buf.len();
    loop {
        cursor -= 1;
        buf[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    output.extend_from_slice(&buf[cursor..]);
}

fn take<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| "DNN artifact cursor overflow".to_string())?;
    if end > bytes.len() {
        return Err(format!(
            "DNN artifact range {}..{end} exceeds length {}",
            *cursor,
            bytes.len()
        ));
    }
    let slice = &bytes[*cursor..end];
    *cursor = end;
    Ok(slice)
}

fn take_str<'a>(bytes: &'a [u8], cursor: &mut usize, len: usize) -> Result<&'a str, String> {
    let raw = take(bytes, cursor, len)?;
    std::str::from_utf8(raw).map_err(|err| format!("DNN artifact text is not UTF-8: {err}"))
}

fn take_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8, String> {
    Ok(take(bytes, cursor, 1)?[0])
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    let raw = take(bytes, cursor, 4)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let raw = take(bytes, cursor, 8)?;
    Ok(u64::from_le_bytes([
        raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
    ]))
}

fn align_up(value: usize, alignment: usize) -> Result<usize, String> {
    if !alignment.is_power_of_two() {
        return Err(format!("alignment {alignment} is not a power of two"));
    }
    let mask = alignment - 1;
    value
        .checked_add(mask)
        .map(|with_mask| with_mask & !mask)
        .ok_or_else(|| "aligned value overflows usize".to_string())
}

fn checked_mul(left: usize, right: usize, what: &str) -> Result<usize, String> {
    left.checked_mul(right)
        .ok_or_else(|| format!("{what} overflows usize"))
}

fn usize_from_u32(value: u32, what: &str) -> Result<usize, String> {
    let converted = value as usize;
    if converted as u32 == value {
        Ok(converted)
    } else {
        Err(format!("{what} does not fit in usize"))
    }
}

fn usize_from_u64(value: u64, what: &str) -> Result<usize, String> {
    if value > usize::MAX as u64 {
        Err(format!("{what} does not fit in usize"))
    } else {
        Ok(value as usize)
    }
}
