#[path = "../dnn_weights.rs"]
mod dnn_weights;

use dnn_weights::{DnnArtifact, DnnSegment, DnnWeightKind, expand_file};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const DNN_DATA_FILES: &[&str] = &[
    "dnn/fargan_data.c",
    "dnn/plc_data.c",
    "dnn/pitchdnn_data.c",
    "dnn/dred_rdovae_enc_data.c",
    "dnn/dred_rdovae_dec_data.c",
    "dnn/dred_rdovae_stats_data.c",
];

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let crate_dir = match env::args_os().nth(1) {
        Some(path) => PathBuf::from(path),
        None => default_crate_dir()?,
    };
    let opus_dir = crate_dir.join("opus");
    let artifact_path = crate_dir.join("dnn-weights/dnn_weights.bin");

    let mut generated_files = Vec::<GeneratedDnnFile>::new();
    let mut generated_arrays = Vec::<GeneratedDnnArray>::new();
    let mut original_sources = Vec::<(String, String)>::new();

    for (file_index, relative_path) in DNN_DATA_FILES.iter().enumerate() {
        let source_path = opus_dir.join(relative_path);
        let source = fs::read_to_string(&source_path)
            .map_err(|err| format!("failed to read {}: {err}", source_path.display()))?;
        let (generated_file, arrays) =
            make_template_and_arrays(file_index, relative_path, &source)?;
        if arrays.is_empty() {
            return Err(format!(
                "{} did not contain any DNN arrays",
                source_path.display()
            ));
        }
        original_sources.push((relative_path.to_string(), source));
        generated_arrays.extend(arrays);
        generated_files.push(generated_file);
    }

    if let Some(parent) = artifact_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    write_artifact(&artifact_path, &generated_files)?;

    let artifact_bytes = fs::read(&artifact_path)
        .map_err(|err| format!("failed to read {}: {err}", artifact_path.display()))?;
    let artifact = DnnArtifact::parse(&artifact_bytes)?;
    if artifact.files().len() != generated_files.len() {
        return Err(format!(
            "artifact has {} files, expected {}",
            artifact.files().len(),
            generated_files.len()
        ));
    }
    let artifact_array_count = artifact_array_count(&artifact);
    if artifact_array_count != generated_arrays.len() {
        return Err(format!(
            "artifact has {} arrays, expected {}",
            artifact_array_count,
            generated_arrays.len()
        ));
    }
    validate_artifact_weights(&artifact, &generated_files, &generated_arrays)?;

    for (file_index, (relative_path, original_source)) in original_sources.iter().enumerate() {
        let file = artifact
            .files()
            .get(file_index)
            .ok_or_else(|| format!("artifact is missing file {file_index}"))?;
        if file.path != relative_path {
            return Err(format!(
                "artifact file {file_index} path is {}, expected {relative_path}",
                file.path
            ));
        }
        let expanded = expand_file(file)?;
        if !expanded_contains_same_arrays(original_source, &expanded) {
            return Err(format!(
                "expanded {} does not preserve the original declarations and array count",
                relative_path
            ));
        }
    }

    println!(
        "wrote {} with {} arrays across {} files",
        artifact_path.display(),
        generated_arrays.len(),
        generated_files.len()
    );

    for relative_path in DNN_DATA_FILES {
        let source_path = opus_dir.join(relative_path);
        fs::remove_file(&source_path)
            .map_err(|err| format!("failed to remove {}: {err}", source_path.display()))?;
    }
    Ok(())
}

fn default_crate_dir() -> Result<PathBuf, String> {
    let exe = env::current_exe().map_err(|err| format!("failed to locate current exe: {err}"))?;
    let cwd = env::current_dir().map_err(|err| format!("failed to locate cwd: {err}"))?;
    if cwd.join("opus/dnn").is_dir() && cwd.join("Cargo.toml").is_file() {
        return Ok(cwd);
    }
    let source_dir = Path::new(file!())
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "failed to infer crate directory from source path".to_string())?;
    let source_dir = if source_dir.is_absolute() {
        source_dir.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|err| format!("failed to locate cwd: {err}"))?
            .join(source_dir)
    };
    if source_dir.join("opus/dnn").is_dir() && source_dir.join("Cargo.toml").is_file() {
        return Ok(source_dir);
    }
    Err(format!(
        "failed to infer crate directory from {} or {}",
        exe.display(),
        source_dir.display()
    ))
}

fn artifact_array_count(artifact: &DnnArtifact<'_>) -> usize {
    artifact
        .files()
        .iter()
        .flat_map(|file| &file.segments)
        .filter(|segment| matches!(segment, DnnSegment::Weights { .. }))
        .count()
}

fn validate_artifact_weights(
    artifact: &DnnArtifact<'_>,
    generated_files: &[GeneratedDnnFile],
    generated_arrays: &[GeneratedDnnArray],
) -> Result<(), String> {
    let mut expected_arrays = generated_arrays.iter();
    for (file_index, (artifact_file, generated_file)) in
        artifact.files().iter().zip(generated_files).enumerate()
    {
        for (artifact_segment, generated_segment) in
            artifact_file.segments.iter().zip(&generated_file.segments)
        {
            match (artifact_segment, generated_segment) {
                (DnnSegment::Text(actual), GeneratedDnnSegment::Text(expected)) => {
                    if *actual != expected.as_bytes() {
                        return Err(format!("text segment mismatch in {}", artifact_file.path));
                    }
                }
                (
                    DnnSegment::Weights {
                        kind,
                        element_count,
                        data,
                    },
                    GeneratedDnnSegment::Weights {
                        kind: expected_kind,
                        name,
                        element_count: expected_count,
                        data: expected_data,
                    },
                ) => {
                    let expected_array = expected_arrays
                        .next()
                        .ok_or_else(|| "artifact has more arrays than expected".to_string())?;
                    let actual_kind = GenWeightKind::from_artifact(*kind);
                    if expected_array.file_index != file_index
                        || expected_array.name != *name
                        || actual_kind != *expected_kind
                        || *element_count != *expected_count
                        || data != expected_data
                        || expected_array.kind != actual_kind
                        || expected_array.element_count != *element_count
                        || expected_array.data != *data
                    {
                        return Err(format!(
                            "weight segment mismatch for {} in {}",
                            name, artifact_file.path
                        ));
                    }
                }
                _ => {
                    return Err(format!(
                        "segment kind mismatch in artifact file {}",
                        artifact_file.path
                    ));
                }
            }
        }
    }
    if expected_arrays.next().is_some() {
        return Err("artifact has fewer arrays than expected".to_string());
    }
    Ok(())
}

fn expanded_contains_same_arrays(original: &str, expanded: &[u8]) -> bool {
    let Ok(expanded) = std::str::from_utf8(expanded) else {
        return false;
    };
    count_array_decls(original) == count_array_decls(expanded)
}

fn count_array_decls(source: &str) -> usize {
    source
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            (line.starts_with("static const ") || line.starts_with("const "))
                && line.contains("] = {")
        })
        .count()
}

struct GeneratedDnnFile {
    path: String,
    segments: Vec<GeneratedDnnSegment>,
}

enum GeneratedDnnSegment {
    Text(String),
    Weights {
        kind: GenWeightKind,
        name: String,
        element_count: usize,
        data: Vec<u8>,
    },
}

struct GeneratedDnnArray {
    file_index: usize,
    kind: GenWeightKind,
    name: String,
    element_count: usize,
    data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GenWeightKind {
    I8,
    U8,
    I32,
    F32,
}

#[derive(Clone, Copy)]
struct ArrayDecl<'a> {
    kind: GenWeightKind,
    name: &'a str,
    count: usize,
}

impl GenWeightKind {
    fn from_c_type(c_type: &str) -> Option<Self> {
        match c_type {
            "opus_int8" => Some(Self::I8),
            "opus_uint8" => Some(Self::U8),
            "int" => Some(Self::I32),
            "float" => Some(Self::F32),
            _ => None,
        }
    }

    fn from_artifact(kind: DnnWeightKind) -> Self {
        match kind {
            DnnWeightKind::I8 => Self::I8,
            DnnWeightKind::U8 => Self::U8,
            DnnWeightKind::I32 => Self::I32,
            DnnWeightKind::F32 => Self::F32,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::I8 => 1,
            Self::U8 => 2,
            Self::I32 => 3,
            Self::F32 => 4,
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

fn make_template_and_arrays(
    file_index: usize,
    path: &str,
    source: &str,
) -> Result<(GeneratedDnnFile, Vec<GeneratedDnnArray>), String> {
    let mut segments = Vec::new();
    let mut arrays = Vec::new();
    let mut copied_until = 0usize;
    let mut cursor = 0usize;

    while let Some(decl_start) = find_next_decl(source, cursor) {
        let line_end = source[decl_start..]
            .find('\n')
            .map(|relative| decl_start + relative + 1)
            .ok_or_else(|| "array declaration is not newline terminated".to_string())?;
        let decl_line = &source[decl_start..line_end];
        let decl = parse_decl_line(decl_line)
            .ok_or_else(|| format!("failed to parse DNN declaration line: {decl_line:?}"))?;
        let close_start = source[line_end..]
            .find("\n};")
            .map(|relative| line_end + relative)
            .ok_or_else(|| format!("array {} does not have a closing initializer", decl.name))?;
        let body = &source[line_end..close_start];
        let data = encode_values(decl.kind, decl.name, decl.count, body)?;

        push_text_segment(&mut segments, &source[copied_until..line_end]);
        segments.push(GeneratedDnnSegment::Weights {
            kind: decl.kind,
            name: decl.name.to_string(),
            element_count: decl.count,
            data: data.clone(),
        });
        arrays.push(GeneratedDnnArray {
            file_index,
            kind: decl.kind,
            name: decl.name.to_string(),
            element_count: decl.count,
            data,
        });

        copied_until = close_start;
        cursor = close_start + 1;
    }

    push_text_segment(&mut segments, &source[copied_until..]);

    Ok((
        GeneratedDnnFile {
            path: path.to_string(),
            segments,
        },
        arrays,
    ))
}

fn write_artifact(output: &Path, files: &[GeneratedDnnFile]) -> Result<(), String> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"CHTDNS2\0");
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&u32_from_usize(files.len(), "file count")?.to_le_bytes());

    for file in files {
        put_u32_len_prefixed_bytes(&mut bytes, file.path.as_bytes(), "path length")?;
        bytes.extend_from_slice(
            &u32_from_usize(file.segments.len(), "segment count")?.to_le_bytes(),
        );
        for segment in &file.segments {
            match segment {
                GeneratedDnnSegment::Text(text) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&(text.len() as u64).to_le_bytes());
                    bytes.extend_from_slice(text.as_bytes());
                }
                GeneratedDnnSegment::Weights {
                    kind,
                    element_count,
                    data,
                    ..
                } => {
                    let expected_len = checked_mul(*element_count, kind.element_size())?;
                    if data.len() != expected_len {
                        return Err(format!(
                            "weight segment has {} bytes, expected {expected_len}",
                            data.len()
                        ));
                    }
                    bytes.push(kind.tag());
                    bytes.extend_from_slice(&(*element_count as u64).to_le_bytes());
                    bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
                    bytes.resize(align_up(bytes.len(), kind.alignment())?, 0);
                    bytes.extend_from_slice(data);
                }
            }
        }
    }

    fs::write(output, bytes).map_err(|err| format!("failed to write {}: {err}", output.display()))
}

fn push_text_segment(segments: &mut Vec<GeneratedDnnSegment>, text: &str) {
    if !text.is_empty() {
        segments.push(GeneratedDnnSegment::Text(text.to_string()));
    }
}

fn find_next_decl(source: &str, cursor: usize) -> Option<usize> {
    let mut offset = cursor;
    while offset < source.len() {
        let line_end = source[offset..]
            .find('\n')
            .map(|relative| offset + relative + 1)
            .unwrap_or(source.len());
        let line = &source[offset..line_end];
        if parse_decl_line(line).is_some() {
            return Some(offset);
        }
        offset = line_end;
    }
    None
}

fn parse_decl_line(line: &str) -> Option<ArrayDecl<'_>> {
    let trimmed = line.trim();
    let rest = trimmed
        .strip_prefix("static const ")
        .or_else(|| trimmed.strip_prefix("const "))?;
    let mut parts = rest.split_ascii_whitespace();
    let c_type = parts.next()?;
    let name_and_count = parts.next()?;
    if parts.next()? != "=" || parts.next()? != "{" || parts.next().is_some() {
        return None;
    }
    let kind = GenWeightKind::from_c_type(c_type)?;
    let bracket = name_and_count.find('[')?;
    if !name_and_count.ends_with(']') {
        return None;
    }
    let name = &name_and_count[..bracket];
    let count = name_and_count[bracket + 1..name_and_count.len() - 1]
        .parse::<usize>()
        .ok()?;
    if name.is_empty() {
        return None;
    }
    Some(ArrayDecl { kind, name, count })
}

fn encode_values(
    kind: GenWeightKind,
    name: &str,
    expected_count: usize,
    body: &str,
) -> Result<Vec<u8>, String> {
    let mut data = Vec::with_capacity(expected_count * kind.element_size());
    let mut count = 0usize;
    for raw in body.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            continue;
        }
        count += 1;
        match kind {
            GenWeightKind::I8 => {
                let value = token
                    .parse::<i8>()
                    .map_err(|err| format!("failed to parse {name} value {token:?}: {err}"))?;
                data.push(value as u8);
            }
            GenWeightKind::U8 => {
                let value = token
                    .parse::<u8>()
                    .map_err(|err| format!("failed to parse {name} value {token:?}: {err}"))?;
                data.push(value);
            }
            GenWeightKind::I32 => {
                let value = token
                    .parse::<i32>()
                    .map_err(|err| format!("failed to parse {name} value {token:?}: {err}"))?;
                data.extend_from_slice(&value.to_le_bytes());
            }
            GenWeightKind::F32 => {
                let value = token
                    .parse::<f64>()
                    .map_err(|err| format!("failed to parse {name} value {token:?}: {err}"))?
                    as f32;
                if !value.is_finite() {
                    return Err(format!("array {name} has non-finite float value {token:?}"));
                }
                data.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
    if count != expected_count {
        return Err(format!(
            "array {name} has {count} values, expected {expected_count}"
        ));
    }
    Ok(data)
}

fn put_u32_len_prefixed_bytes(bytes: &mut Vec<u8>, value: &[u8], what: &str) -> Result<(), String> {
    bytes.extend_from_slice(&u32_from_usize(value.len(), what)?.to_le_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

fn checked_mul(left: usize, right: usize) -> Result<usize, String> {
    left.checked_mul(right)
        .ok_or_else(|| "value length overflows usize".to_string())
}

fn align_up(value: usize, alignment: usize) -> Result<usize, String> {
    let mask = alignment - 1;
    value
        .checked_add(mask)
        .map(|with_mask| with_mask & !mask)
        .ok_or_else(|| "aligned value overflows usize".to_string())
}

fn u32_from_usize(value: usize, what: &str) -> Result<u32, String> {
    if value > u32::MAX as usize {
        Err(format!("{what} does not fit in u32"))
    } else {
        Ok(value as u32)
    }
}
