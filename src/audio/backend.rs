#[cfg(unix)]
use std::{os::fd::AsRawFd, sync::Mutex};

/// Runs `f` with the audio backend's own stderr output discarded.
///
/// ALSA writes configuration warnings straight to stderr while a device is
/// opened, which would scribble over the terminal UI. WASAPI reports through
/// `HRESULT` return values and writes nothing to stderr, so on Windows there is
/// nothing to suppress and this is a plain call.
#[cfg(not(unix))]
pub(crate) fn with_audio_backend_stderr_suppressed<T>(f: impl FnOnce() -> T) -> T {
    f()
}

#[cfg(unix)]
pub(crate) fn with_audio_backend_stderr_suppressed<T>(f: impl FnOnce() -> T) -> T {
    static STDERR_REDIRECT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = STDERR_REDIRECT_LOCK.lock().ok();

    let Ok(dev_null) = std::fs::File::options().write(true).open("/dev/null") else {
        return f();
    };

    unsafe {
        let saved_stderr = libc::dup(libc::STDERR_FILENO);
        if saved_stderr < 0 {
            return f();
        }

        let _ = libc::fflush(std::ptr::null_mut());
        if libc::dup2(dev_null.as_raw_fd(), libc::STDERR_FILENO) < 0 {
            let _ = libc::close(saved_stderr);
            return f();
        }

        let result = f();
        let _ = libc::fflush(std::ptr::null_mut());
        let _ = libc::dup2(saved_stderr, libc::STDERR_FILENO);
        let _ = libc::close(saved_stderr);
        result
    }
}
