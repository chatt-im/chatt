//! Reading the system clipboard for the room paste flow.
//!
//! This is the read-side counterpart to [`crate::clipboard`], which only
//! copies text out. A paste is user triggered, so the helpers run synchronously
//! and are bounded by the helper process itself. No polling happens on the
//! render loop.
//!
//! [`classify`] turns raw clipboard access ([`ClipboardBackend`]) into a
//! [`PastePayload`]. Real clipboard access shells out to platform helpers
//! (`wl-paste`, `xclip`, `xsel`, `pbpaste`, `pngpaste`); tests inject a fake
//! backend to exercise the classification rules without a real clipboard.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

/// The result of reading the clipboard for a paste action.
#[derive(Debug)]
pub(crate) enum PastePayload {
    /// Ordinary text to insert into the composer.
    Text(String),
    /// An image or file to upload after filename confirmation.
    Image(ImagePaste),
    /// The clipboard held nothing usable.
    Empty,
    /// The clipboard held data we cannot handle (e.g. an image MIME we do not
    /// support) and no text fallback. The string is a user-facing reason.
    Unsupported(String),
}

/// A pasted image resolved to something the upload pipeline can stream.
#[derive(Debug)]
pub(crate) struct ImagePaste {
    pub source: ImagePasteSource,
    pub default_name: String,
    pub dimensions: Option<(u32, u32)>,
    pub origin: ImagePasteOrigin,
}

/// Where the image bytes live on disk.
#[derive(Debug)]
pub(crate) enum ImagePasteSource {
    /// A file that already existed; upload leaves it in place.
    ExistingPath(PathBuf),
    /// A temp file staged from raw clipboard bytes; upload deletes it after
    /// opening.
    StagedFile(PathBuf),
}

impl ImagePasteSource {
    pub(crate) fn path(&self) -> &PathBuf {
        match self {
            Self::ExistingPath(path) | Self::StagedFile(path) => path,
        }
    }

    /// Whether the upload should delete the source after opening it.
    pub(crate) fn is_staged(&self) -> bool {
        matches!(self, Self::StagedFile(_))
    }
}

/// How the image arrived on the clipboard. Kept for diagnostics and dialog
/// metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePasteOrigin {
    /// Raw image bytes exposed directly by the clipboard.
    ClipboardImageData,
    /// A file entry / `text/uri-list` target on the clipboard.
    ClipboardFile,
    /// Pasted text that was a `file://` URI.
    FileUri,
    /// Pasted text that was a bare filesystem path.
    TextPath,
}

impl ImagePasteOrigin {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ClipboardImageData => "clipboard image",
            Self::ClipboardFile => "clipboard file",
            Self::FileUri => "file URI",
            Self::TextPath => "file path",
        }
    }
}

/// A clipboard read failure with a user-facing message.
#[derive(Debug)]
pub(crate) struct ClipboardPasteError(pub String);

impl std::fmt::Display for ClipboardPasteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reads the clipboard and classifies its contents into a [`PastePayload`].
pub(crate) trait ClipboardPasteProvider {
    fn read_paste(&self) -> Result<PastePayload, ClipboardPasteError>;
}

/// The production provider, backed by platform clipboard helpers.
pub(crate) struct HelperClipboard;

impl ClipboardPasteProvider for HelperClipboard {
    fn read_paste(&self) -> Result<PastePayload, ClipboardPasteError> {
        classify(&PlatformBackend)
    }
}

/// Low-level clipboard access primitives. One method per capability so a fake
/// can return canned results per capability in tests.
trait ClipboardBackend {
    /// Plain text on the clipboard, if any.
    fn text(&self) -> Option<String>;
    /// File paths from a clipboard file list / `text/uri-list` target.
    fn file_uris(&self) -> Vec<PathBuf>;
    /// Available `image/*` MIME types on the clipboard.
    fn image_mimes(&self) -> Vec<String>;
    /// Raw bytes for a specific image MIME type.
    fn image_bytes(&self, mime: &str) -> Option<Vec<u8>>;
}

/// Image MIME types we accept, most preferred first.
const IMAGE_MIME_PRIORITY: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/gif",
    "image/bmp",
];

/// Turns raw clipboard access into a [`PastePayload`], preferring a real
/// image/file over text when the clipboard exposes both.
fn classify(backend: &dyn ClipboardBackend) -> Result<PastePayload, ClipboardPasteError> {
    // 1. A file entry pointing at an image wins over everything else.
    for path in backend.file_uris() {
        if path_is_image(&path) && path.is_file() {
            let dimensions = image_dimensions_of_path(&path);
            let default_name = file_name_or(&path, "clipboard");
            return Ok(PastePayload::Image(ImagePaste {
                source: ImagePasteSource::ExistingPath(path),
                default_name,
                dimensions,
                origin: ImagePasteOrigin::ClipboardFile,
            }));
        }
    }

    // 2. Raw image bytes, staged to a temp file for upload.
    let available = backend.image_mimes();
    let had_image_mime = !available.is_empty();
    for mime in IMAGE_MIME_PRIORITY {
        if !available.iter().any(|candidate| candidate == mime) {
            continue;
        }
        let Some(bytes) = backend.image_bytes(mime) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        let dimensions = crate::web_server::image_dimensions(&bytes);
        let extension = extension_for_mime(mime);
        let path = stage_bytes(&bytes, extension).map_err(|error| {
            ClipboardPasteError(format!("failed to stage clipboard image: {error}"))
        })?;
        return Ok(PastePayload::Image(ImagePaste {
            source: ImagePasteSource::StagedFile(path),
            default_name: format!("clipboard.{extension}"),
            dimensions,
            origin: ImagePasteOrigin::ClipboardImageData,
        }));
    }

    // 3. Text, which may itself name an image file.
    if let Some(text) = backend.text() {
        if text.is_empty() {
            return Ok(PastePayload::Empty);
        }
        if let Some((path, from_uri)) = normalize_pasted_path(&text) {
            if path_is_image(&path) && path.is_file() {
                let dimensions = image_dimensions_of_path(&path);
                let default_name = file_name_or(&path, "clipboard");
                let origin = if from_uri {
                    ImagePasteOrigin::FileUri
                } else {
                    ImagePasteOrigin::TextPath
                };
                return Ok(PastePayload::Image(ImagePaste {
                    source: ImagePasteSource::ExistingPath(path),
                    default_name,
                    dimensions,
                    origin,
                }));
            }
        }
        return Ok(PastePayload::Text(text));
    }

    // The clipboard advertised an image type we could not decode and offered no
    // text fallback.
    if had_image_mime {
        return Ok(PastePayload::Unsupported(
            "clipboard image type is not supported".to_string(),
        ));
    }
    Ok(PastePayload::Empty)
}

/// Whether a path's file name classifies as an image by extension.
fn path_is_image(path: &PathBuf) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    crate::web_server::classify(name) == "image"
}

/// The file name of `path`, or `fallback` when it has none.
fn file_name_or(path: &PathBuf, fallback: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback)
        .to_string()
}

/// Reads the header of an on-disk image to recover its dimensions.
fn image_dimensions_of_path(path: &PathBuf) -> Option<(u32, u32)> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = vec![0u8; 64 * 1024];
    let read = file.read(&mut header).ok()?;
    header.truncate(read);
    crate::web_server::image_dimensions(&header)
}

/// The file extension to give a staged image of the given MIME type.
fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        _ => "png",
    }
}

/// Writes clipboard image bytes to a persistent temp file and returns its path.
fn stage_bytes(bytes: &[u8], extension: &str) -> std::io::Result<PathBuf> {
    let file = tempfile::Builder::new()
        .prefix("chatt-clipboard-")
        .suffix(&format!(".{extension}"))
        .tempfile()?;
    std::fs::write(file.path(), bytes)?;
    let (_file, path) = file.keep().map_err(|error| error.error)?;
    Ok(path)
}

/// Normalizes pasted text that may name a filesystem path.
///
/// Returns the path and whether it came from a `file://` URI. Multi-line text
/// is never a path. Only a single trimmed line is considered.
fn normalize_pasted_path(text: &str) -> Option<(PathBuf, bool)> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.contains('\n') {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode(rest);
        if decoded.is_empty() {
            return None;
        }
        return Some((PathBuf::from(decoded), true));
    }
    Some((PathBuf::from(trimmed), false))
}

/// Decodes `%XX` escapes in a percent-encoded string, passing other bytes
/// through unchanged.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Clipboard access via platform command-line helpers.
struct PlatformBackend;

/// Runs `program args`, returning captured stdout when it exits successfully.
/// A missing program or a non-zero exit yields `None`.
fn run_helper(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        Some(output.stdout)
    } else {
        None
    }
}

/// Whether a Wayland compositor is present (prefer `wl-paste`).
#[cfg(target_os = "linux")]
fn on_wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Filters helper output listing MIME types down to supported `image/*` types.
#[cfg(target_os = "linux")]
fn parse_image_mimes(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let mut mimes = Vec::new();
    for line in text.lines() {
        let mime = line.trim();
        if IMAGE_MIME_PRIORITY.contains(&mime) {
            mimes.push(mime.to_string());
        }
    }
    mimes
}

/// Parses a `text/uri-list` body into local file paths, dropping comments and
/// non-`file://` entries.
#[cfg(target_os = "linux")]
fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    let text = String::from_utf8_lossy(bytes);
    let mut paths = Vec::new();
    for line in text.lines() {
        let entry = line.trim();
        if entry.is_empty() || entry.starts_with('#') {
            continue;
        }
        let Some(rest) = entry.strip_prefix("file://") else {
            continue;
        };
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        let decoded = percent_decode(rest);
        if !decoded.is_empty() {
            paths.push(PathBuf::from(decoded));
        }
    }
    paths
}

#[cfg(target_os = "linux")]
impl ClipboardBackend for PlatformBackend {
    fn text(&self) -> Option<String> {
        if on_wayland() {
            if let Some(bytes) = run_helper("wl-paste", &["--no-newline"]) {
                return String::from_utf8(bytes).ok();
            }
        }
        if let Some(bytes) = run_helper("xclip", &["-selection", "clipboard", "-o"]) {
            return String::from_utf8(bytes).ok();
        }
        if let Some(bytes) = run_helper("xsel", &["--clipboard", "--output"]) {
            return String::from_utf8(bytes).ok();
        }
        None
    }

    fn file_uris(&self) -> Vec<PathBuf> {
        if on_wayland() {
            if let Some(bytes) = run_helper("wl-paste", &["--type", "text/uri-list"]) {
                return parse_uri_list(&bytes);
            }
        }
        if let Some(bytes) = run_helper(
            "xclip",
            &["-selection", "clipboard", "-t", "text/uri-list", "-o"],
        ) {
            return parse_uri_list(&bytes);
        }
        Vec::new()
    }

    fn image_mimes(&self) -> Vec<String> {
        if on_wayland() {
            if let Some(bytes) = run_helper("wl-paste", &["--list-types"]) {
                return parse_image_mimes(&bytes);
            }
        }
        if let Some(bytes) =
            run_helper("xclip", &["-selection", "clipboard", "-t", "TARGETS", "-o"])
        {
            return parse_image_mimes(&bytes);
        }
        Vec::new()
    }

    fn image_bytes(&self, mime: &str) -> Option<Vec<u8>> {
        if on_wayland() {
            if let Some(bytes) = run_helper("wl-paste", &["--type", mime]) {
                if !bytes.is_empty() {
                    return Some(bytes);
                }
            }
        }
        if let Some(bytes) = run_helper("xclip", &["-selection", "clipboard", "-t", mime, "-o"]) {
            if !bytes.is_empty() {
                return Some(bytes);
            }
        }
        None
    }
}

#[cfg(target_os = "macos")]
impl ClipboardBackend for PlatformBackend {
    fn text(&self) -> Option<String> {
        let bytes = run_helper("pbpaste", &[])?;
        String::from_utf8(bytes).ok()
    }

    fn file_uris(&self) -> Vec<PathBuf> {
        // Best effort: ask the pasteboard for file references via AppleScript.
        let script = "set the clipboard to (the clipboard as «class furl») as text";
        let _ = script;
        Vec::new()
    }

    fn image_mimes(&self) -> Vec<String> {
        // `pngpaste` extracts PNG bytes when the pasteboard holds an image.
        // Advertise PNG optimistically and let `image_bytes` confirm.
        if run_helper("pngpaste", &["--version"]).is_some()
            || Command::new("pngpaste").arg("--help").output().is_ok()
        {
            return vec!["image/png".to_string()];
        }
        Vec::new()
    }

    fn image_bytes(&self, mime: &str) -> Option<Vec<u8>> {
        if mime != "image/png" {
            return None;
        }
        let bytes = run_helper("pngpaste", &["-"])?;
        if bytes.is_empty() { None } else { Some(bytes) }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl ClipboardBackend for PlatformBackend {
    fn text(&self) -> Option<String> {
        None
    }
    fn file_uris(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn image_mimes(&self) -> Vec<String> {
        Vec::new()
    }
    fn image_bytes(&self, _mime: &str) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A minimal valid 1x1 PNG so `imagesize` reports real dimensions.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[derive(Default)]
    struct FakeBackend {
        text: Option<String>,
        uris: Vec<PathBuf>,
        mimes: Vec<String>,
        images: HashMap<String, Vec<u8>>,
    }

    impl ClipboardBackend for FakeBackend {
        fn text(&self) -> Option<String> {
            self.text.clone()
        }
        fn file_uris(&self) -> Vec<PathBuf> {
            self.uris.clone()
        }
        fn image_mimes(&self) -> Vec<String> {
            self.mimes.clone()
        }
        fn image_bytes(&self, mime: &str) -> Option<Vec<u8>> {
            self.images.get(mime).cloned()
        }
    }

    /// Writes an image file into a fresh temp dir and returns its path.
    fn write_temp_image(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, PNG_1X1).unwrap();
        (dir, path)
    }

    #[test]
    fn plain_text_is_text() {
        let backend = FakeBackend {
            text: Some("hello world".to_string()),
            ..Default::default()
        };
        match classify(&backend).unwrap() {
            PastePayload::Text(text) => assert_eq!(text, "hello world"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn file_uri_image_is_image() {
        let (_dir, path) = write_temp_image("shot.png");
        let uri = format!("file://{}", path.display());
        let backend = FakeBackend {
            text: Some(uri),
            ..Default::default()
        };
        match classify(&backend).unwrap() {
            PastePayload::Image(image) => {
                assert_eq!(image.origin, ImagePasteOrigin::FileUri);
                assert!(matches!(image.source, ImagePasteSource::ExistingPath(_)));
                assert_eq!(image.dimensions, Some((1, 1)));
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn text_path_image_is_image() {
        let (_dir, path) = write_temp_image("pic.png");
        let backend = FakeBackend {
            text: Some(path.display().to_string()),
            ..Default::default()
        };
        match classify(&backend).unwrap() {
            PastePayload::Image(image) => {
                assert_eq!(image.origin, ImagePasteOrigin::TextPath);
                assert_eq!(image.default_name, "pic.png");
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn clipboard_file_list_image_is_image() {
        let (_dir, path) = write_temp_image("dropped.png");
        let backend = FakeBackend {
            uris: vec![path],
            ..Default::default()
        };
        match classify(&backend).unwrap() {
            PastePayload::Image(image) => {
                assert_eq!(image.origin, ImagePasteOrigin::ClipboardFile);
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn direct_image_bytes_are_staged() {
        let mut images = HashMap::new();
        images.insert("image/png".to_string(), PNG_1X1.to_vec());
        let backend = FakeBackend {
            mimes: vec!["image/png".to_string()],
            images,
            ..Default::default()
        };
        match classify(&backend).unwrap() {
            PastePayload::Image(image) => {
                assert_eq!(image.origin, ImagePasteOrigin::ClipboardImageData);
                assert_eq!(image.default_name, "clipboard.png");
                let ImagePasteSource::StagedFile(path) = &image.source else {
                    panic!("expected staged file");
                };
                assert!(path.is_file());
                assert_eq!(std::fs::read(path).unwrap(), PNG_1X1);
                std::fs::remove_file(path).unwrap();
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn image_bytes_win_over_text() {
        let mut images = HashMap::new();
        images.insert("image/png".to_string(), PNG_1X1.to_vec());
        let backend = FakeBackend {
            text: Some("some text too".to_string()),
            mimes: vec!["image/png".to_string()],
            images,
            ..Default::default()
        };
        match classify(&backend).unwrap() {
            PastePayload::Image(image) => {
                let ImagePasteSource::StagedFile(path) = &image.source else {
                    panic!("expected staged file");
                };
                std::fs::remove_file(path).unwrap();
            }
            other => panic!("expected image preferred over text, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_image_without_text_reports_unsupported() {
        let backend = FakeBackend {
            mimes: vec!["image/tiff".to_string()],
            ..Default::default()
        };
        assert!(matches!(
            classify(&backend).unwrap(),
            PastePayload::Unsupported(_)
        ));
    }

    #[test]
    fn unsupported_image_with_text_falls_back_to_text() {
        let backend = FakeBackend {
            text: Some("fallback".to_string()),
            mimes: vec!["image/tiff".to_string()],
            ..Default::default()
        };
        match classify(&backend).unwrap() {
            PastePayload::Text(text) => assert_eq!(text, "fallback"),
            other => panic!("expected text fallback, got {other:?}"),
        }
    }

    #[test]
    fn empty_clipboard_is_empty() {
        assert!(matches!(
            classify(&FakeBackend::default()).unwrap(),
            PastePayload::Empty
        ));
    }

    #[test]
    fn percent_decode_handles_spaces() {
        assert_eq!(percent_decode("/tmp/a%20b.png"), "/tmp/a b.png");
    }
}
