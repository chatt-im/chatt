use std::io::{self, Cursor, Write};
use std::path::Path;

pub(crate) const MIN_COMPRESSION_SIZE: u64 = 16 * 1024;
pub(crate) const COMPRESSION_PROBE_BYTES: usize = 128 * 1024;
pub(crate) const COMPRESSION_LEVEL: i32 = 3;
const MIN_SAVINGS_NUMERATOR: usize = 7;
const MIN_SAVINGS_DENOMINATOR: usize = 8;
pub(crate) const ZSTD_WINDOW_LOG: u32 = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FastCompressionDecision {
    Probe,
    BelowMinimum,
    ExcludedExtension,
}

pub(crate) fn fast_compression_decision(name: &str, original_size: u64) -> FastCompressionDecision {
    if original_size < MIN_COMPRESSION_SIZE {
        FastCompressionDecision::BelowMinimum
    } else if has_excluded_extension(name) {
        FastCompressionDecision::ExcludedExtension
    } else {
        FastCompressionDecision::Probe
    }
}

pub(crate) fn compressed_probe_len(probe: &[u8]) -> io::Result<usize> {
    zstd::stream::encode_all(Cursor::new(probe), COMPRESSION_LEVEL).map(|encoded| encoded.len())
}

pub(crate) fn probe_has_minimum_savings(raw_len: usize, encoded_len: usize) -> bool {
    encoded_len.saturating_mul(MIN_SAVINGS_DENOMINATOR)
        <= raw_len.saturating_mul(MIN_SAVINGS_NUMERATOR)
}

pub(crate) fn new_encoder<W: Write>(
    sink: W,
    original_size: u64,
) -> io::Result<zstd::stream::write::Encoder<'static, W>> {
    let mut encoder = zstd::stream::write::Encoder::new(sink, COMPRESSION_LEVEL)?;
    encoder.set_pledged_src_size(Some(original_size))?;
    encoder.window_log(ZSTD_WINDOW_LOG)?;
    encoder.include_checksum(true)?;
    Ok(encoder)
}

fn has_excluded_extension(name: &str) -> bool {
    let Some(extension) = Path::new(name).extension().and_then(|value| value.to_str()) else {
        return false;
    };
    is_no_compress_extension(&extension.to_ascii_lowercase())
}

fn is_no_compress_extension(extension: &str) -> bool {
    matches!(
        extension,
        "3gp"
            | "7z"
            | "aac"
            | "apk"
            | "avif"
            | "avi"
            | "br"
            | "bz2"
            | "cab"
            | "deb"
            | "dmg"
            | "docx"
            | "epub"
            | "flac"
            | "gif"
            | "gz"
            | "heic"
            | "heif"
            | "ico"
            | "ipa"
            | "jar"
            | "jpeg"
            | "jpg"
            | "jxl"
            | "lz"
            | "lz4"
            | "lzh"
            | "lzma"
            | "m4a"
            | "m4v"
            | "mkv"
            | "mov"
            | "mp3"
            | "mp4"
            | "mpeg"
            | "mpg"
            | "odg"
            | "odp"
            | "ods"
            | "odt"
            | "ogg"
            | "opus"
            | "pdf"
            | "png"
            | "pptx"
            | "rar"
            | "rpm"
            | "svgz"
            | "tgz"
            | "tif"
            | "tiff"
            | "txz"
            | "webm"
            | "webp"
            | "whl"
            | "wmv"
            | "woff"
            | "woff2"
            | "xlsx"
            | "xz"
            | "zip"
            | "zst"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_size_controls_probe_path() {
        assert_eq!(
            fast_compression_decision("data.bin", 0),
            FastCompressionDecision::BelowMinimum
        );
        assert_eq!(
            fast_compression_decision("data.bin", MIN_COMPRESSION_SIZE - 1),
            FastCompressionDecision::BelowMinimum
        );
        assert_eq!(
            fast_compression_decision("data.bin", MIN_COMPRESSION_SIZE),
            FastCompressionDecision::Probe
        );
    }

    #[test]
    fn compressed_extensions_are_case_insensitive_and_use_final_suffix() {
        for name in [
            "photo.png",
            "movie.MP4",
            "archive.Zip",
            "report.DOCX",
            "recording.opus",
            "backup.tar.zst",
        ] {
            assert_eq!(
                fast_compression_decision(name, MIN_COMPRESSION_SIZE),
                FastCompressionDecision::ExcludedExtension,
                "{name}"
            );
        }
    }

    #[test]
    fn extension_categories_are_covered() {
        let categories = [
            &["7z", "gz", "lz4", "rar", "xz", "zip", "zst"][..],
            &["apk", "deb", "dmg", "ipa", "jar", "rpm", "whl"],
            &["docx", "epub", "odt", "pdf", "pptx", "xlsx"],
            &["avif", "gif", "heic", "jpeg", "png", "tiff", "webp"],
            &["aac", "avi", "flac", "mkv", "mp3", "mp4", "ogg", "opus"],
            &["woff", "woff2"],
        ];
        for extension in categories.into_iter().flatten() {
            assert!(is_no_compress_extension(extension), "{extension}");
        }
    }

    #[test]
    fn useful_and_unknown_extensions_remain_candidates() {
        for name in [
            "notes.txt",
            "data.json",
            "backup.tar",
            "audio.wav",
            "program.bin",
            "README",
            "data.unknown",
        ] {
            assert_eq!(
                fast_compression_decision(name, MIN_COMPRESSION_SIZE),
                FastCompressionDecision::Probe,
                "{name}"
            );
        }
    }

    #[test]
    fn probe_accepts_text_and_rejects_deterministic_noise() {
        let text = "the quick brown fox jumps over the lazy dog\n"
            .repeat(3_000)
            .into_bytes();
        let text_encoded = compressed_probe_len(&text).unwrap();
        assert!(probe_has_minimum_savings(text.len(), text_encoded));

        let mut state = 0x1234_5678_9abc_def0u64;
        let mut noise = vec![0u8; COMPRESSION_PROBE_BYTES];
        for byte in &mut noise {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let noise_encoded = compressed_probe_len(&noise).unwrap();
        assert!(!probe_has_minimum_savings(noise.len(), noise_encoded));
    }

    #[test]
    fn savings_boundary_is_inclusive() {
        assert!(probe_has_minimum_savings(800, 700));
        assert!(!probe_has_minimum_savings(800, 701));
    }
}
