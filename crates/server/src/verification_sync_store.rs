//! Bounded compacted E2E verification snapshots.
//!
//! The server retains exactly one opaque envelope per account. Files are
//! separated per user so one update never rewrites another account's state.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use hashbrown::HashMap;
use rpc::{
    e2e::{
        VerificationSyncCheckpoint, decode_verification_sync_envelope,
        verification_sync_checkpoint,
    },
    ids::UserId,
};

const STORE_DIR: &str = "e2e-verification-sync";

#[derive(Debug)]
pub(crate) struct VerificationSyncStore {
    dir: Option<PathBuf>,
    snapshots: HashMap<UserId, Vec<u8>>,
}

impl VerificationSyncStore {
    pub(crate) fn open(data_dir: Option<PathBuf>) -> Self {
        Self {
            dir: data_dir.map(|dir| dir.join(STORE_DIR)),
            snapshots: HashMap::new(),
        }
    }

    pub(crate) fn current(
        &mut self,
        user_id: UserId,
    ) -> Result<Option<(VerificationSyncCheckpoint, Vec<u8>)>, String> {
        if !self.snapshots.contains_key(&user_id) {
            let Some(path) = self.path(user_id) else {
                return Ok(None);
            };
            match fs::read(&path) {
                Ok(bytes) => {
                    decode_verification_sync_envelope(&bytes).map_err(|_| {
                        format!("{} contains an invalid verification snapshot", path.display())
                    })?;
                    self.snapshots.insert(user_id, bytes);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(format!("failed to read {}: {error}", path.display()));
                }
            }
        }
        Ok(self.snapshots.get(&user_id).map(|bytes| {
            (verification_sync_checkpoint(bytes), bytes.clone())
        }))
    }

    pub(crate) fn replace(&mut self, user_id: UserId, envelope: Vec<u8>) -> Result<(), String> {
        decode_verification_sync_envelope(&envelope)
            .map_err(|_| "invalid verification sync envelope".to_string())?;
        if let Some(path) = self.path(user_id) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
            }
            atomic_write(&path, &envelope)?;
        }
        self.snapshots.insert(user_id, envelope);
        Ok(())
    }

    fn path(&self, user_id: UserId) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.bin", user_id.0)))
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("bin.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&tmp) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(&tmp)
                .map_err(|err| format!("failed to remove stale {}: {err}", tmp.display()))?;
            let mut retry = OpenOptions::new();
            retry.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                retry.mode(0o600);
            }
            retry
                .open(&tmp)
                .map_err(|err| format!("failed to create {}: {err}", tmp.display()))?
        }
        Err(error) => return Err(format!("failed to create {}: {error}", tmp.display())),
    };
    file.write_all(bytes)
        .map_err(|error| format!("failed to write {}: {error}", tmp.display()))?;
    file.sync_all()
        .map_err(|error| format!("failed to sync {}: {error}", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("failed to replace {}: {error}", path.display())
    })?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}
