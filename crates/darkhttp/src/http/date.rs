use std::cell::Cell;
use std::ffi::CStr;
use std::mem;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

const HTTP_DATE_FMT: &[u8] = b"%a, %d %b %Y %H:%M:%S GMT\0";

#[derive(Clone, Copy)]
pub(crate) struct HttpDate {
    bytes: [u8; 64],
    len: usize,
}

impl HttpDate {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

thread_local! {
    /// Per-thread cache of the formatted `Date:` header, keyed by the wall-clock
    /// second it was generated for. Both the poll loop and the I/O-pool workers
    /// build responses, so caching per-thread keeps it lock-free.
    static DATE_CACHE: Cell<(libc::time_t, HttpDate)> =
        const { Cell::new((0, HttpDate { bytes: [0; 64], len: 0 })) };
}

/// The current time as an HTTP `Date` value.
///
/// `gmtime_r` + `strftime` cost ~1µs and ran on every response. HTTP dates have
/// one-second resolution, so this caches the formatted string per thread and
/// only reformats when the wall-clock second changes. `time(2)` is a vDSO call
/// on Linux/x86-64, so the common (cache-hit) path is a cheap read.
pub(crate) fn now_http_date() -> HttpDate {
    let now = now_unix();
    DATE_CACHE.with(|cache| {
        let (cached_secs, cached_date) = cache.get();
        if cached_secs == now && cached_date.len != 0 {
            return cached_date;
        }
        let date = format_http_date(now);
        cache.set((now, date));
        date
    })
}

pub(crate) fn system_time_http_date(time: SystemTime) -> Option<HttpDate> {
    let secs = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format_http_date(secs as libc::time_t))
}

fn now_unix() -> libc::time_t {
    unsafe { libc::time(ptr::null_mut()) }
}

fn format_http_date(when: libc::time_t) -> HttpDate {
    let mut when_copy = when;
    let mut tm: libc::tm = unsafe { mem::zeroed() };
    let tm_ptr = unsafe { libc::gmtime_r(&mut when_copy, &mut tm) };
    if tm_ptr.is_null() {
        return HttpDate {
            bytes: [0; 64],
            len: 0,
        };
    }
    let mut out = HttpDate {
        bytes: [0; 64],
        len: 0,
    };
    let len = unsafe {
        libc::strftime(
            out.bytes.as_mut_ptr() as *mut libc::c_char,
            out.bytes.len(),
            HTTP_DATE_FMT.as_ptr() as *const libc::c_char,
            &tm,
        )
    };
    if len != 0 {
        out.len = unsafe { CStr::from_ptr(out.bytes.as_ptr() as *const libc::c_char) }
            .to_bytes()
            .len();
    }
    out
}
