//! Positional file reads, directory durability, and owner-private creation.
//!
//! Reading at an explicit offset is `pread(2)` on Unix and a `ReadFile` with
//! an explicit offset on Windows, which `std` exposes as `FileExt::read_at` and
//! `FileExt::seek_read` respectively. Each read names its own offset, so
//! concurrent positional readers of one handle never disturb each other; only
//! `pread` also leaves the file cursor alone, while `seek_read` moves it past
//! the bytes read. Only the Unix side provides the exact-length form, so it is
//! written out here for both.

use std::fs::File;
#[cfg(unix)]
use std::fs::{DirBuilder, OpenOptions};
use std::io;
use std::path::Path;

/// Reads exactly `buf.len()` bytes starting at `offset`.
///
/// Every read names its offset, so concurrent positional readers of the same
/// handle cannot disturb each other. On Windows the file's cursor still moves,
/// so do not mix this with cursor-based reads of the same handle. A short read
/// before the buffer is filled is reported as [`io::ErrorKind::UnexpectedEof`].
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;

        let mut filled = 0;
        while filled < buf.len() {
            // Each `seek_read` names its own offset, so a concurrent reader
            // moving the handle's cursor cannot redirect this one.
            match file.seek_read(&mut buf[filled..], offset + filled as u64) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ));
                }
                Ok(read) => filled += read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Flushes a directory's own entries to stable storage.
///
/// On POSIX, creating or renaming a file inside a directory is not durable
/// until the *directory* is fsynced as well, so the write-temp-then-rename
/// pattern ends with this call.
///
/// Windows has no equivalent operation to offer: a directory handle cannot be
/// flushed, because opening one requires `FILE_FLAG_BACKUP_SEMANTICS` and
/// `FlushFileBuffers` on it fails with `ERROR_ACCESS_DENIED`. NTFS does journal
/// the directory metadata as part of the rename, so the entry cannot be seen
/// half-updated. Neither `MoveFileExW` nor
/// `SetFileInformationByHandle(FileRenameInfo)` is write-through, though, so a
/// write-temp-then-rename sequence is not durable across power loss there the
/// way it is on Unix. The file's *contents* are safe either way, since the
/// callers `sync_all` before renaming; it is the rename itself that may be
/// lost. This is a successful no-op rather than an error.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(())
    }
}

/// Options for opening a file that, when the open creates it, is readable and
/// writable only by its owner.
///
/// The flags and their combinations mean what they mean on
/// [`std::fs::OpenOptions`]. Unix applies mode `0600` at creation. Windows
/// creates the file through `CreateFileW` with a security descriptor granting
/// only the token user and `SYSTEM`, so the file never exists with the access
/// its directory would have handed it, not even briefly: `OpenOptions` carries
/// no `SECURITY_ATTRIBUTES`, and restricting afterwards leaves a window in
/// which another user's handle can be opened and then kept.
///
/// Opening a file that already exists leaves its access alone on both
/// platforms, since a creation mode only applies to creation. A caller that
/// wants an existing file's access corrected asks for that separately with
/// [`crate::security::restrict_to_owner`].
#[derive(Clone, Debug, Default)]
pub struct PrivateOpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl PrivateOpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Opens `path` with these options.
    pub fn open(&self, path: &Path) -> io::Result<File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new()
                .read(self.read)
                .write(self.write)
                .append(self.append)
                .truncate(self.truncate)
                .create(self.create)
                .create_new(self.create_new)
                .mode(0o600)
                .open(path)
        }
        #[cfg(windows)]
        {
            windows::open_private(self, path)
        }
    }
}

/// Creates `path` and any missing parents, reachable only by their owner.
///
/// Mode `0700` on Unix. On Windows each level created gets a protected DACL
/// granting only the token user and `SYSTEM`, inherited by what is written
/// inside it, supplied at creation so there is no window before it applies.
/// Required wherever the location is user- or operator-chosen and may sit
/// under a world-readable root.
///
/// Only directories this call creates are affected, on both platforms. An
/// existing directory keeps the access it has: rewriting it would propagate a
/// new inheritable DACL through everything beneath it on every call, and could
/// lock other users out of a folder they were deliberately sharing.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(windows)]
    {
        let security = crate::security::OwnerOnlySecurity::for_directory()?;
        windows::create_dir_all(path, &security)
    }
}

/// Reads into `buf` starting at `offset`, returning the number of bytes read.
pub fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }
}

#[cfg(windows)]
mod windows {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
        OPEN_ALWAYS, OPEN_EXISTING, TRUNCATE_EXISTING,
    };

    use super::PrivateOpenOptions;
    use crate::osstr::wide_null;
    use crate::security::OwnerOnlySecurity;

    fn invalid(reason: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, reason)
    }

    /// `CreateFileW` with the access, disposition and share mode `std` derives
    /// from the same flags, plus an owner-only security descriptor that applies
    /// if and only if the call creates the file.
    pub(super) fn open_private(options: &PrivateOpenOptions, path: &Path) -> io::Result<File> {
        // An append handle withholds FILE_WRITE_DATA so every write lands at
        // the end, as `std` arranges it.
        let append_access = FILE_GENERIC_WRITE & !FILE_WRITE_DATA;
        let read_access = if options.read { GENERIC_READ } else { 0 };
        let access = match (options.read, options.write, options.append) {
            (false, false, false) => return Err(invalid("no access mode was requested")),
            (_, true, false) => read_access | GENERIC_WRITE,
            (_, _, true) => read_access | append_access,
            (true, false, false) => GENERIC_READ,
        };
        match (options.write, options.append) {
            (true, false) => {}
            (false, false) => {
                if options.truncate || options.create || options.create_new {
                    return Err(invalid("creating or truncating needs write access"));
                }
            }
            (_, true) => {
                if options.truncate && !options.create_new {
                    return Err(invalid("an append handle cannot truncate"));
                }
            }
        }
        // Create-and-truncate opens rather than replaces, and truncates below
        // only if the file already existed, which is what `std` does too.
        let (disposition, truncate_existing) =
            match (options.create, options.truncate, options.create_new) {
                (_, _, true) => (CREATE_NEW, false),
                (false, false, false) => (OPEN_EXISTING, false),
                (true, false, false) => (OPEN_ALWAYS, false),
                (false, true, false) => (TRUNCATE_EXISTING, false),
                (true, true, false) => (OPEN_ALWAYS, true),
            };
        // A new file must not be created through a dangling link to
        // somewhere else.
        let flags = if options.create_new {
            FILE_FLAG_OPEN_REPARSE_POINT
        } else {
            0
        };

        let security = OwnerOnlySecurity::new()?;
        let wide = wide_null(path);
        // SAFETY: the path is NUL-terminated and the security attributes
        // outlive the call; the template handle is optional.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                security.as_ptr(),
                disposition,
                flags,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // Read before anything else can overwrite the thread's last error.
        let existed =
            io::Error::last_os_error().raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32);
        // SAFETY: the handle was just opened, is valid, and is owned by the
        // `File` from here on.
        let file = unsafe { File::from_raw_handle(handle) };
        if truncate_existing && existed {
            file.set_len(0)?;
        }
        Ok(file)
    }

    /// `create_dir_all`, creating each missing level with `security`.
    ///
    /// A level that already exists is left as it is, including when a
    /// concurrent creator wins the race for it.
    pub(super) fn create_dir_all(path: &Path, security: &OwnerOnlySecurity) -> io::Result<()> {
        if path.as_os_str().is_empty() || path.is_dir() {
            return Ok(());
        }
        match create_dir(path, security) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) if path.is_dir() => return Ok(()),
            Err(error) => return Err(error),
        }
        let Some(parent) = path.parent() else {
            return Err(io::Error::other("failed to create whole tree"));
        };
        create_dir_all(parent, security)?;
        match create_dir(path, security) {
            Ok(()) => Ok(()),
            Err(_) if path.is_dir() => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn create_dir(path: &Path, security: &OwnerOnlySecurity) -> io::Result<()> {
        let wide = wide_null(path);
        // SAFETY: the path is NUL-terminated and the security attributes
        // outlive the call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), security.as_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "chatt-platform-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn open_private_creates_an_owner_only_file() {
        let path = temp_path("private-file");
        let _ = std::fs::remove_file(&path);

        let mut file = PrivateOpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create private file");
        file.write_all(b"secret").expect("write");
        drop(file);

        assert!(
            crate::security::is_owner_only(&path).expect("inspect access control"),
            "a file created for secrets must not be reachable by other local users"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_private_dir_is_owner_only_and_idempotent() {
        let path = temp_path("private-dir").join("nested");
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));

        create_private_dir(&path).expect("create private dir");
        create_private_dir(&path).expect("recreating an existing private dir");

        assert!(
            crate::security::is_owner_only(&path).expect("inspect access control"),
            "a history tree under a world-writable root must stay owner-only"
        );
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn private_open_options_keep_open_options_semantics() {
        let path = temp_path("private-semantics");
        let _ = std::fs::remove_file(&path);

        let mut file = PrivateOpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .expect("create for append");
        file.write_all(b"one").expect("append");
        drop(file);

        let mut file = PrivateOpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("reopen for append");
        file.write_all(b"two").expect("append");
        drop(file);
        assert_eq!(std::fs::read(&path).expect("read"), b"onetwo");

        let error = PrivateOpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect_err("create_new over an existing file");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        let mut file = PrivateOpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .expect("truncate an existing file");
        file.write_all(b"3").expect("write");
        drop(file);
        assert_eq!(std::fs::read(&path).expect("read"), b"3");

        let missing = temp_path("private-missing");
        let _ = std::fs::remove_file(&missing);
        let error = PrivateOpenOptions::new()
            .read(true)
            .open(&missing)
            .expect_err("opening without create");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);

        let _ = std::fs::remove_file(&path);
    }

    /// An existing directory may be one the user shares on purpose, and
    /// rewriting its DACL would also propagate through everything below it.
    #[cfg(windows)]
    #[test]
    fn create_private_dir_leaves_an_existing_directory_alone() {
        let path = temp_path("shared-dir");
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("create shared dir");
        let display = path.display().to_string();
        let status = std::process::Command::new("icacls")
            .args([display.as_str(), "/grant", "*S-1-1-0:(F)"])
            .status()
            .expect("run icacls");
        assert!(status.success(), "icacls must grant Everyone access");

        create_private_dir(&path).expect("existing dir");
        create_private_dir(&path.join("child")).expect("child dir");

        assert!(!crate::security::is_owner_only(&path).expect("inspect parent"));
        assert!(crate::security::is_owner_only(&path.join("child")).expect("inspect child"));
        let _ = std::fs::remove_dir_all(&path);
    }
}
