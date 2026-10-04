//! Single-instance exclusion for a named local process.
//!
//! On Unix the client's local control socket already provides this: a second
//! `chatt` finds the socket, attaches to the running one, and never opens the
//! config, room catalog, room history or receive store a second time. Windows
//! publishes no control socket, so nothing stops a second full client from
//! writing the same files concurrently, none of which are locked. This turns
//! that silent corruption into a clear message.

use std::io;

/// A held claim on a name, released when this value drops or the process exits.
pub struct InstanceLock {
    /// Held only for its `Drop`: releasing the claim is all this value does.
    _claim: Inner,
}

/// The outcome of trying to claim a name.
pub enum Claim {
    /// The name was free and is now held.
    Held(InstanceLock),
    /// Another process in this login session holds it.
    Taken,
}

/// Claims `name` for this process.
///
/// The name is scoped to the login session, not the machine, so two users on
/// one desktop do not exclude each other. On Windows the claim is a named
/// mutex, which the kernel releases even if the process is killed. A lock file
/// would instead need a staleness rule.
///
/// Unix has no claim to make, so this always reports [`Claim::Held`] over a
/// guard that does nothing.
pub fn claim(name: &str) -> io::Result<Claim> {
    #[cfg(not(windows))]
    {
        let _ = name;
        Ok(Claim::Held(InstanceLock { _claim: Inner }))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let wide = crate::osstr::wide_null(format!(r"Local\{name}"));
        // The claim is the named object's existence, not ownership of the
        // mutex: it is created unowned, so nothing ever has to be released
        // on the thread that acquired it, and dropping the guard from any
        // thread is just closing the handle.
        //
        // SAFETY: the name is NUL-terminated and the null security attributes
        // request the process token's default, which is adequate: the mutex
        // holds no data and the name is already session-scoped.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // `CreateMutexW` succeeds and returns a handle to the *existing* mutex
        // when the name is taken, so the last error is the only signal.
        if io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
            // SAFETY: the handle was just opened and is closed once.
            unsafe { CloseHandle(handle) };
            return Ok(Claim::Taken);
        }
        Ok(Claim::Held(InstanceLock {
            _claim: Inner(handle),
        }))
    }
}

#[cfg(not(windows))]
struct Inner;

#[cfg(windows)]
struct Inner(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
// SAFETY: the mutex is never acquired, so the handle carries no thread-affine
// ownership; it is only ever closed, which any thread may do.
unsafe impl Send for Inner {}

#[cfg(windows)]
impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: the handle is valid for this value's lifetime and closed once.
        // Closing the last handle destroys the name, which frees the claim.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_free_name_is_claimed() {
        let name = format!("chatt-test-{}", std::process::id());
        assert!(matches!(claim(&name).expect("claim"), Claim::Held(_)));
    }

    #[cfg(windows)]
    #[test]
    fn a_held_name_is_reported_as_taken() {
        let name = format!("chatt-test-taken-{}", std::process::id());
        let Claim::Held(held) = claim(&name).expect("first claim") else {
            panic!("the first claim on a fresh name must succeed");
        };

        assert!(matches!(claim(&name).expect("second claim"), Claim::Taken));

        drop(held);
        assert!(matches!(claim(&name).expect("third claim"), Claim::Held(_)));
    }
}
