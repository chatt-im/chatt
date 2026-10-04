//! Restricting local objects to the user that created them.
//!
//! Unix expresses this with mode bits: `0600` on a file, `0700` on a
//! directory. Windows has no mode bits, and the ACL an object inherits from its
//! parent is whatever the parent grants, which under `C:\ProgramData` or a
//! data-drive root includes `BUILTIN\Users: Read`. Chatt writes pairing
//! secrets, identity pins and chat history into operator-chosen directories, so
//! the guarantee has to be expressed explicitly rather than inherited.
//!
//! The Windows form is a *protected* DACL, one that ignores inheritance,
//! granting full control to the process token's user and to `SYSTEM`, and to
//! nobody else. It is built from SDDL rather than assembled ACE by ACE because
//! the string is the part a reader can check against the intent.

use std::io;
use std::path::Path;

/// Restricts an existing file at `path` to the current user.
///
/// Unix sets mode `0600`. Windows applies the protected DACL described above.
/// Callers that are *creating* the file want
/// [`crate::file::PrivateOpenOptions`], which applies the restriction at
/// creation on both platforms so there is no window.
pub fn restrict_to_owner(path: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        set_mode(path, 0o600)
    }
    #[cfg(windows)]
    {
        windows::set_protected_dacl(path, windows::Inheritance::None)
    }
}

/// Narrows `path`'s mode to `mode`, and only then.
///
/// The check first is not an optimisation: `chmod` fails outright on the
/// non-POSIX filesystems people do put a data directory on (exFAT, NTFS-3G, an
/// SMB mount), where the mode is a mount option rather than a property of the
/// file. Attempting it unconditionally would turn a directory that was always
/// usable into a startup failure.
#[cfg(not(windows))]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let permissions = std::fs::metadata(path)?.permissions();
    if permissions.mode() & 0o777 & !mode == 0 {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Whether `path` is reachable only by its owner.
///
/// The inverse of what [`restrict_to_owner`] and
/// [`crate::file::create_private_dir`] establish, so the crates that create private files can assert the guarantee
/// they document on both platforms rather than only on Unix. Unix reads the
/// mode bits; Windows walks the DACL and rejects any allow-ACE naming someone
/// other than the token user or `SYSTEM`.
pub fn is_owner_only(path: &Path) -> io::Result<bool> {
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o077 == 0)
    }
    #[cfg(windows)]
    {
        windows::dacl_is_owner_only(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "chatt-security-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    /// Grants every local user full access, the state the callers exist to
    /// prevent.
    fn grant_everyone(path: &Path) {
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
                .expect("loosen mode");
        }
        #[cfg(windows)]
        {
            let path = path.display().to_string();
            let status = std::process::Command::new("icacls")
                .args([path.as_str(), "/grant", "*S-1-1-0:(F)"])
                .status()
                .expect("run icacls");
            assert!(status.success(), "icacls must grant Everyone access");
        }
    }

    /// Without this the positive assertions prove nothing: an
    /// [`is_owner_only`] that answered `true` unconditionally would satisfy
    /// every other test in the workspace.
    #[test]
    fn a_world_accessible_file_is_not_owner_only() {
        let path = temp_path("loose");
        std::fs::write(&path, b"secret").expect("write");
        grant_everyone(&path);

        assert!(
            !is_owner_only(&path).expect("inspect access control"),
            "a file every local user can open must not pass as owner-only"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn restrict_to_owner_narrows_a_world_accessible_file() {
        let path = temp_path("narrowed");
        std::fs::write(&path, b"secret").expect("write");
        grant_everyone(&path);

        restrict_to_owner(&path).expect("restrict");

        assert!(
            is_owner_only(&path).expect("inspect access control"),
            "restricting must revoke the access granted above"
        );
        let _ = std::fs::remove_file(&path);
    }
}

/// A security descriptor granting the current user and `SYSTEM` full control,
/// ready to hand to an object-creating Win32 call.
///
/// This is the same grant [`restrict_to_owner`] applies after the fact, for the
/// APIs that do accept a `SECURITY_ATTRIBUTES` and so can start out correct.
#[cfg(windows)]
pub use windows::OwnerOnlySecurity;

/// The current process token's user SID, in SDDL string form (`S-1-5-...`).
///
/// Names that have to be unique per user, the server's control pipe among
/// them, build on it rather than on `%USERNAME%`, which is neither unique by
/// construction nor safe to embed without sanitising.
#[cfg(windows)]
pub use windows::current_user_sid;

/// The user SID of the token of process `pid`, in the same form as
/// [`current_user_sid`].
///
/// Lets a client check who is on the other end of a named pipe before trusting
/// it: a pipe name is predictable, so another local user can create it first.
#[cfg(windows)]
pub use windows::process_user_sid;

#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use crate::osstr::wide_null;

    /// Whether the grant propagates to objects created inside.
    #[derive(Clone, Copy)]
    pub(super) enum Inheritance {
        None,
        ObjectsAndContainers,
    }

    impl Inheritance {
        /// The SDDL ACE flags field.
        fn ace_flags(self) -> &'static str {
            match self {
                Self::None => "",
                Self::ObjectsAndContainers => "OICI",
            }
        }
    }

    /// A LocalAlloc'd security descriptor, freed on drop.
    struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: the pointer came from
            // `ConvertStringSecurityDescriptorToSecurityDescriptorW`, which
            // documents `LocalFree` as its release, and is freed once.
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }

    /// A `SECURITY_ATTRIBUTES` granting only the token user and `SYSTEM`,
    /// borrowing a descriptor that lives as long as this value.
    pub struct OwnerOnlySecurity {
        /// Held only for its `Drop`; `attributes` carries the pointer that is
        /// actually read.
        _descriptor: SecurityDescriptor,
        attributes: SECURITY_ATTRIBUTES,
    }

    impl OwnerOnlySecurity {
        /// Builds the descriptor for the calling process's token user.
        pub fn new() -> io::Result<Self> {
            Self::with_inheritance(Inheritance::None)
        }

        /// As [`Self::new`], with the grant inherited by files and directories
        /// created inside: the descriptor for creating a private directory.
        pub fn for_directory() -> io::Result<Self> {
            Self::with_inheritance(Inheritance::ObjectsAndContainers)
        }

        fn with_inheritance(inheritance: Inheritance) -> io::Result<Self> {
            let descriptor = owner_only_descriptor(inheritance)?;
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: 0,
            };
            Ok(Self {
                _descriptor: descriptor,
                attributes,
            })
        }

        /// The `lpSecurityAttributes` argument to pass.
        ///
        /// The returned pointer borrows `self`, so the call it is handed to
        /// must complete before this value is dropped.
        pub fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
            ptr::from_ref(&self.attributes)
        }
    }

    // SAFETY: the descriptor is `LocalAlloc`'d process memory with no thread
    // affinity, and the struct holds a second copy of that pointer rather than
    // one into itself, so moving it between threads is sound. Needed because
    // the descriptor is built before the control worker starts and then used
    // from inside it.
    unsafe impl Send for OwnerOnlySecurity {}

    fn owner_only_descriptor(inheritance: Inheritance) -> io::Result<SecurityDescriptor> {
        let user = current_user_sid()?;
        let flags = inheritance.ace_flags();
        // `D:P` protects the DACL from inherited ACEs; `GA` is GENERIC_ALL.
        // No ACE for `Everyone` or the anonymous account, which is what the
        // default descriptors for both files and named pipes would grant.
        let sddl = wide_null(format!("D:P(A;{flags};GA;;;{user})(A;{flags};GA;;;SY)"));
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: the SDDL string is NUL-terminated and outlives the call; the
        // out-parameter is written with a `LocalAlloc`'d descriptor on success.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(SecurityDescriptor(descriptor))
    }

    pub(super) fn set_protected_dacl(path: &Path, inheritance: Inheritance) -> io::Result<()> {
        let descriptor = owner_only_descriptor(inheritance)?;
        let mut dacl = ptr::null_mut();
        let mut present = 0;
        let mut defaulted = 0;
        // SAFETY: the descriptor is valid and the out-parameters are owned here.
        let ok = unsafe {
            windows_sys::Win32::Security::GetSecurityDescriptorDacl(
                descriptor.0,
                &mut present,
                &mut dacl,
                &mut defaulted,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let path = wide_null(path);
        // SAFETY: the path and the DACL both outlive the call, and the
        // information mask names exactly the one pointer being passed.
        let status = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                dacl,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    /// The SIDs an owner-private object may grant: `SYSTEM`, plus the token
    /// user, which is added at the call site.
    const SYSTEM_SID: &str = "S-1-5-18";

    pub(super) fn dacl_is_owner_only(path: &Path) -> io::Result<bool> {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
        use windows_sys::Win32::Security::{ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, GetAce, PSID};

        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

        let user = current_user_sid()?;
        let wide = wide_null(path);
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: every out-parameter is owned here; the descriptor backs the
        // DACL pointer and is freed below, after the walk.
        let status = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let descriptor = SecurityDescriptor(descriptor);
        // A null DACL grants everyone everything, which is the opposite of the
        // guarantee. A present but empty one denies everyone, which satisfies it.
        if dacl.is_null() {
            return Ok(false);
        }

        // SAFETY: the DACL is valid for the descriptor's lifetime.
        let aces = unsafe { (*dacl).AceCount };
        for index in 0..u32::from(aces) {
            let mut ace = ptr::null_mut();
            // SAFETY: `index` is below the ACE count just read.
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `GetAce` yielded a pointer to an ACE within the DACL.
            let header = unsafe { *ace.cast::<ACE_HEADER>() };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                continue;
            }
            // SAFETY: an allow-ACE is an `ACCESS_ALLOWED_ACE` whose `SidStart`
            // field is the first word of the SID it carries.
            let sid = unsafe { ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart) };
            let sid = sid_to_string(sid as PSID)?;
            if sid != user && sid != SYSTEM_SID {
                drop(descriptor);
                return Ok(false);
            }
        }
        drop(descriptor);
        Ok(true)
    }

    struct OwnedHandle(windows_sys::Win32::Foundation::HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: opened by this module and closed once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }

    /// The current process token's user SID, in SDDL string form.
    pub fn current_user_sid() -> io::Result<String> {
        // SAFETY: the pseudo-handle from `GetCurrentProcess` needs no release.
        process_token_user_sid(unsafe { GetCurrentProcess() })
    }

    /// The user SID of process `pid`'s token, in SDDL string form.
    pub fn process_user_sid(pid: u32) -> io::Result<String> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        // SAFETY: a plain open by id; the result is checked for null.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = OwnedHandle(process);
        process_token_user_sid(process.0)
    }

    fn process_token_user_sid(
        process: windows_sys::Win32::Foundation::HANDLE,
    ) -> io::Result<String> {
        let mut token = ptr::null_mut();
        // SAFETY: the process handle is valid for the call and the
        // out-parameter is owned here.
        let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle(token);

        let mut len = 0;
        // SAFETY: the first call deliberately passes a null buffer to learn the
        // size; it is expected to fail with ERROR_INSUFFICIENT_BUFFER.
        unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // `u64` storage keeps the TOKEN_USER read below aligned.
        let mut buffer = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        // SAFETY: the buffer is at least `len` bytes, the size the call just
        // asked for.
        let ok = unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                len,
                &mut len,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        // SAFETY: on success the buffer holds a TOKEN_USER whose SID pointer
        // points into that same buffer, which outlives the conversion below.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        sid_to_string(sid)
    }

    /// Renders a SID in the `S-1-5-...` form the SDDL grammar and the DACL walk
    /// both compare against.
    fn sid_to_string(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
        let mut text = ptr::null_mut();
        // SAFETY: the caller passes a valid SID; the out-parameter receives a
        // `LocalAlloc`'d string, freed below.
        let ok = unsafe { ConvertSidToStringSidW(sid, &mut text) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: on success the string is NUL-terminated, and is this
        // function's to free.
        unsafe {
            let mut end = text;
            while *end != 0 {
                end = end.add(1);
            }
            let len = end.offset_from(text) as usize;
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text as HLOCAL);
            Ok(sid)
        }
    }
}
