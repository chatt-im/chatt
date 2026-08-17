//! Positional file reads, directory durability, and owner-private creation.
//!
//! Reading at an explicit offset without disturbing the file cursor is
//! `pread(2)` on Unix and an overlapped `ReadFile` on Windows, which `std`
//! exposes as `FileExt::read_at` and `FileExt::seek_read` respectively. Only
//! the Unix side also provides the exact-length form, so it is written out here
//! for both.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io;
use std::path::Path;

/// Reads exactly `buf.len()` bytes starting at `offset`.
///
/// Unlike a plain read, this does not move the file's cursor, so concurrent
/// readers of the same handle cannot disturb each other. A short read before
/// the buffer is filled is reported as [`io::ErrorKind::UnexpectedEof`].
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
            // `seek_read` is positional: the offset advances here, not on the
            // handle, so this stays safe against a concurrent reader.
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

/// Opens `path` through `options`, readable and writable only by its owner.
///
/// Unix applies mode `0600` at creation, so a file created here is never
/// visible to anyone else. Windows has no mode bits and `OpenOptions` carries
/// no `SECURITY_ATTRIBUTES`, so the DACL is applied immediately after the open
/// instead; see [`crate::security::restrict_to_owner`] for what that window
/// costs and why it is still worth doing.
///
/// `options` must be one that creates the file. An `options` that reopens an
/// existing file gets the same treatment Unix gives it, where the creation
/// mode simply does not apply, so a caller that wants an existing file's
/// access corrected asks for that separately with
/// [`crate::security::restrict_to_owner`].
pub fn open_private(options: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.open(path)
    }
    #[cfg(not(unix))]
    {
        let file = options.open(path)?;
        crate::security::restrict_to_owner(path)?;
        Ok(file)
    }
}

/// Creates `path` and any missing parents, reachable only by their owner.
///
/// Mode `0700` on Unix. On Windows each level created gets a protected DACL
/// granting only the token user and `SYSTEM`, inherited by what is written
/// inside it. Required wherever the location is user- or operator-chosen and
/// may sit under a world-readable root.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        builder.create(path)?;
        crate::security::restrict_dir_to_owner(path)
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

        let mut file = open_private(OpenOptions::new().write(true).create_new(true), &path)
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
}
