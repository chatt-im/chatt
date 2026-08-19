//! Win32 clipboard access, for both the copy and the paste path.
//!
//! Unix reaches the clipboard through helper processes ([`crate::clipboard`]
//! spawns them, [`crate::clipboard_paste`] captures their output). Windows has
//! the clipboard in the API, so both directions land here instead.
//!
//! There is nothing to reap afterwards. An X11 selection belongs to a live
//! client, which is why the Unix write path keeps its helper alive and polls
//! it; a Windows clipboard write hands a `GMEM_MOVEABLE` allocation to the
//! system, which owns it from then on and keeps serving it after this process
//! exits.
//!
//! What is deliberately not read: `CF_DIB` and `CF_BITMAP`. Capture tools and
//! browsers offer the registered `PNG` format, and synthesizing a BMP from a
//! device-independent bitmap would stage a multi-megabyte upload for what is
//! usually a screenshot. A DIB-only clipboard is reported as an image type
//! this does not support, so the paste says so rather than claiming the
//! clipboard is empty.

use std::{
    borrow::Cow, ffi::OsString, io, os::windows::ffi::OsStringExt, path::PathBuf, sync::OnceLock,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::GlobalFree,
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
            OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT},
    },
};

/// The MIME reported for a clipboard holding only a device-independent bitmap.
///
/// Outside the paste classifier's supported list on purpose: it makes an
/// unreadable image visible as one instead of as an empty clipboard.
const DIB_MIME: &str = "image/x-dib";

const OPEN_ATTEMPTS: u32 = 5;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// The clipboard, open for this thread until the value is dropped.
struct ClipboardGuard;

impl ClipboardGuard {
    /// Opens the clipboard, retrying while another process holds it.
    ///
    /// Only one process can have the clipboard open at a time, so losing the
    /// first attempt to whichever application the user just copied from is
    /// ordinary rather than a failure.
    fn open() -> io::Result<Self> {
        for attempt in 0..OPEN_ATTEMPTS {
            // SAFETY: a null window handle associates the clipboard with the
            // current task, which is what a process without windows wants.
            if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
                return Ok(Self);
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
        }
        Err(io::Error::last_os_error())
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard, and closes it once.
        unsafe { CloseClipboard() };
    }
}

/// Replaces the clipboard with `text`.
pub(crate) fn set_text(text: &str) -> io::Result<()> {
    let text = to_crlf(text);
    let mut payload = Vec::with_capacity((text.len() + 1) * 2);
    for unit in text.encode_utf16().chain(std::iter::once(0)) {
        payload.extend_from_slice(&unit.to_le_bytes());
    }
    set_format(CF_UNICODETEXT as u32, &payload)
}

/// Replaces the clipboard with `bytes`, offered under a single format.
fn set_format(format: u32, bytes: &[u8]) -> io::Result<()> {
    let _guard = ClipboardGuard::open()?;
    // SAFETY: the clipboard is open, which is what taking ownership of it
    // requires.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a moveable allocation is what a clipboard format handle must be.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` was just allocated here and is locked once.
    let target = unsafe { GlobalLock(handle) };
    if target.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: the allocation is unlocked and still owned by this process.
        unsafe { GlobalFree(handle) };
        return Err(error);
    }
    // SAFETY: `target` addresses the allocation just sized for `bytes`, and a
    // fresh allocation cannot overlap the source.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast::<u8>(), bytes.len()) };
    // SAFETY: pairs with the lock above. A zero return means the lock count
    // reached zero, not that the call failed.
    unsafe { GlobalUnlock(handle) };
    // SAFETY: the clipboard is open and owned, and the handle is the moveable
    // allocation the format requires. On success the system owns it.
    if unsafe { SetClipboardData(format, handle) }.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: the handle was not transferred, so freeing it is this
        // process's job.
        unsafe { GlobalFree(handle) };
        return Err(error);
    }
    Ok(())
}

/// The clipboard's plain text, if it holds any.
pub(crate) fn text() -> Option<String> {
    let bytes = format_bytes(CF_UNICODETEXT as u32)?;
    let text: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let end = text
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(text.len());
    Some(String::from_utf16_lossy(&text[..end]))
}

/// The paths from a clipboard file list, as Explorer's copy leaves behind.
pub(crate) fn file_paths() -> Vec<PathBuf> {
    let Some(bytes) = format_bytes(CF_HDROP as u32) else {
        return Vec::new();
    };
    paths_from_dropfiles(&bytes)
}

/// The image types on the clipboard, most preferred first.
pub(crate) fn image_mimes() -> Vec<String> {
    let png = png_format();
    // SAFETY: availability is a property of the clipboard rather than of a
    // handle, and needs no open.
    if png != 0 && unsafe { IsClipboardFormatAvailable(png) } != 0 {
        return vec!["image/png".to_string()];
    }
    for format in [CF_DIB, CF_DIBV5] {
        // SAFETY: as above.
        if unsafe { IsClipboardFormatAvailable(format as u32) } != 0 {
            return vec![DIB_MIME.to_string()];
        }
    }
    Vec::new()
}

/// The raw bytes of a clipboard image of type `mime`.
pub(crate) fn image_bytes(mime: &str) -> Option<Vec<u8>> {
    if mime != "image/png" {
        return None;
    }
    let png = png_format();
    if png == 0 {
        return None;
    }
    let bytes = format_bytes(png)?;
    if bytes.is_empty() { None } else { Some(bytes) }
}

/// The id of the registered `PNG` clipboard format, or 0 if it has none.
///
/// PNG has no predefined `CF_` constant; every application that offers it
/// registers the same name.
fn png_format() -> u32 {
    static FORMAT: OnceLock<u32> = OnceLock::new();
    *FORMAT.get_or_init(|| {
        let name: Vec<u16> = "PNG\0".encode_utf16().collect();
        // SAFETY: the name is NUL terminated and outlives the call.
        unsafe { RegisterClipboardFormatW(name.as_ptr()) }
    })
}

/// Copies out the clipboard's data for one format.
///
/// The length is the allocation's, which the allocator may have rounded up past
/// what the writer asked for. Every format read here ends at a terminator its
/// own parse honours, and an image decoder ignores what follows the last chunk.
fn format_bytes(format: u32) -> Option<Vec<u8>> {
    let _guard = ClipboardGuard::open().ok()?;
    // SAFETY: the clipboard is open. The handle belongs to whoever set it and
    // is only borrowed until the clipboard is closed, so it is never freed
    // here.
    let handle = unsafe { GetClipboardData(format) };
    if handle.is_null() {
        return None;
    }
    // SAFETY: every format read here is delivered as a global allocation.
    let len = unsafe { GlobalSize(handle) };
    if len == 0 {
        return None;
    }
    // SAFETY: as above; the lock is released below.
    let source = unsafe { GlobalLock(handle) };
    if source.is_null() {
        return None;
    }
    // SAFETY: `source` addresses the `len` bytes `GlobalSize` reported, and
    // nothing can change the clipboard while it is open here.
    let bytes = unsafe { std::slice::from_raw_parts(source.cast::<u8>(), len) }.to_vec();
    // SAFETY: pairs with the lock above.
    unsafe { GlobalUnlock(handle) };
    Some(bytes)
}

/// Byte length of the `DROPFILES` header a `CF_HDROP` block starts with:
/// `pFiles` (u32), `pt` (two i32), `fNC` (i32), `fWide` (i32).
const DROPFILES_HEADER: usize = 20;
/// Offset of `fWide` within that header.
const DROPFILES_WIDE: usize = 16;

/// Parses the paths out of a `CF_HDROP` block.
///
/// The names follow the header at the offset it carries, NUL terminated, with
/// an empty name ending the list. Only the UTF-16 form is read: an ANSI list
/// would have to be decoded with the active code page, and everything that
/// puts files on the clipboard sets the wide flag.
fn paths_from_dropfiles(bytes: &[u8]) -> Vec<PathBuf> {
    let Some(header) = bytes.get(..DROPFILES_HEADER) else {
        return Vec::new();
    };
    let offset = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let wide = u32::from_le_bytes([
        header[DROPFILES_WIDE],
        header[DROPFILES_WIDE + 1],
        header[DROPFILES_WIDE + 2],
        header[DROPFILES_WIDE + 3],
    ]);
    if wide == 0 {
        return Vec::new();
    }
    let Some(list) = bytes.get(offset..) else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    let mut name = Vec::new();
    for unit in list.chunks_exact(2) {
        let unit = u16::from_le_bytes([unit[0], unit[1]]);
        if unit != 0 {
            name.push(unit);
            continue;
        }
        if name.is_empty() {
            break;
        }
        paths.push(PathBuf::from(OsString::from_wide(&name)));
        name.clear();
    }
    paths
}

/// Rewrites lone line feeds as CRLF pairs.
///
/// Clipboard text on Windows ends its lines with CRLF. Applications that paste
/// a bare line feed as a box are still common enough that the copy side has to
/// state it.
fn to_crlf(text: &str) -> Cow<'_, str> {
    if !text.contains('\n') {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut previous = '\0';
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            out.push('\r');
        }
        out.push(character);
        previous = character;
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A 1x1 32-bit `CF_DIB`: a `BITMAPINFOHEADER` and one pixel.
    const DIB_1X1: &[u8] = &[
        40, 0, 0, 0, // biSize
        1, 0, 0, 0, // biWidth
        1, 0, 0, 0, // biHeight
        1, 0, // biPlanes
        32, 0, // biBitCount
        0, 0, 0, 0, // biCompression: BI_RGB
        4, 0, 0, 0, // biSizeImage
        0, 0, 0, 0, 0, 0, 0, 0, // biXPelsPerMeter, biYPelsPerMeter
        0, 0, 0, 0, 0, 0, 0, 0, // biClrUsed, biClrImportant
        0, 0, 0, 255, // the pixel
    ];

    /// Stand-in for clipboard PNG data, which this module never decodes.
    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nnot really an image";

    /// Serializes the tests that write to the clipboard, which is machine-wide
    /// state that concurrent cases would take from each other.
    fn clipboard_turn() -> std::sync::MutexGuard<'static, ()> {
        static TURN: Mutex<()> = Mutex::new(());
        TURN.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Builds a `CF_HDROP` block naming `names`.
    fn dropfiles(wide: bool, names: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(DROPFILES_HEADER as u32).to_le_bytes());
        bytes.extend_from_slice(&[0u8; 12]);
        bytes.extend_from_slice(&u32::from(wide).to_le_bytes());
        for name in names {
            if wide {
                for unit in name.encode_utf16().chain(std::iter::once(0)) {
                    bytes.extend_from_slice(&unit.to_le_bytes());
                }
            } else {
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(0);
            }
        }
        bytes.extend_from_slice(if wide { &[0, 0] } else { &[0] });
        bytes
    }

    #[test]
    fn lone_line_feeds_become_carriage_return_pairs() {
        assert_eq!(to_crlf("one\ntwo\n"), "one\r\ntwo\r\n");
    }

    #[test]
    fn existing_carriage_returns_are_left_alone() {
        assert_eq!(to_crlf("one\r\ntwo"), "one\r\ntwo");
        assert_eq!(to_crlf("no breaks"), "no breaks");
    }

    #[test]
    fn wide_dropfiles_list_yields_every_path() {
        let bytes = dropfiles(true, &[r"C:\pictures\shot.png", r"D:\a b\second.txt"]);
        assert_eq!(
            paths_from_dropfiles(&bytes),
            vec![
                PathBuf::from(r"C:\pictures\shot.png"),
                PathBuf::from(r"D:\a b\second.txt"),
            ]
        );
    }

    #[test]
    fn ansi_dropfiles_list_is_ignored() {
        let bytes = dropfiles(false, &[r"C:\pictures\shot.png"]);
        assert!(paths_from_dropfiles(&bytes).is_empty());
    }

    #[test]
    fn truncated_dropfiles_block_yields_nothing() {
        assert!(paths_from_dropfiles(&[0; DROPFILES_HEADER - 1]).is_empty());
    }

    #[test]
    fn text_round_trips_through_the_system_clipboard() {
        let _turn = clipboard_turn();
        set_text("clipboard round trip\nsecond line").expect("the session reaches the clipboard");

        assert_eq!(
            text().as_deref(),
            Some("clipboard round trip\r\nsecond line")
        );
        assert!(image_mimes().is_empty());
        assert!(file_paths().is_empty());
    }

    #[test]
    fn a_clipboard_file_list_reads_back_as_paths() {
        let _turn = clipboard_turn();
        let names = [r"C:\pictures\shot.png", r"D:\a b\second.txt"];
        set_format(CF_HDROP as u32, &dropfiles(true, &names))
            .expect("the session reaches the clipboard");

        assert_eq!(
            file_paths(),
            names.map(PathBuf::from).to_vec(),
            "a file list is the CF_HDROP Explorer leaves behind"
        );
        assert!(text().is_none());
    }

    #[test]
    fn a_clipboard_png_is_offered_as_image_png() {
        let _turn = clipboard_turn();
        let png = png_format();
        assert_ne!(png, 0, "the PNG clipboard format registers");
        set_format(png, PNG_BYTES).expect("the session reaches the clipboard");

        assert_eq!(image_mimes(), vec!["image/png".to_string()]);
        assert_eq!(image_bytes("image/png").as_deref(), Some(PNG_BYTES));
        assert!(image_bytes("image/webp").is_none());
    }

    #[test]
    fn a_bitmap_only_clipboard_reports_an_unsupported_image() {
        let _turn = clipboard_turn();
        set_format(CF_DIB as u32, DIB_1X1).expect("the session reaches the clipboard");

        assert_eq!(image_mimes(), vec![DIB_MIME.to_string()]);
        assert!(image_bytes("image/png").is_none());
    }
}
