use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};

use jsony::Jsony;
use rpc::{
    identity::{SignedDeviceCertificate, SignedDeviceRoster},
    ids::{AccountId, DeviceId, PairAttemptId, UserId},
};

pub const BOOTSTRAP_VERSION: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct E2eBootstrap {
    pub version: u16,
    pub server_public_key: [u8; 32],
    pub user_id: UserId,
    pub account_id: AccountId,
    pub authority_seed: [u8; 32],
    pub device_id: DeviceId,
    pub device_name: String,
    pub device_certificate: SignedDeviceCertificate,
    pub own_roster: SignedDeviceRoster,
    pub state: BootstrapState,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum BootstrapState {
    Active,
    PendingPair {
        attempt_id: PairAttemptId,
        redemption_secret: String,
        bearer_token: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BootstrapLoad {
    Missing,
    Loaded(E2eBootstrap),
    Unreadable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallationState {
    Missing,
    Loaded(E2eBootstrap),
    Unreadable(String),
    PendingPair(E2eBootstrap),
    Active(E2eBootstrap),
    Revoked(E2eBootstrap),
    BrokenInstallation {
        bootstrap: E2eBootstrap,
        reason: String,
    },
}

impl E2eBootstrap {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != BOOTSTRAP_VERSION {
            return Err("unsupported MLS bootstrap version".to_string());
        }
        if self.device_name.trim().is_empty()
            || self.device_name.len() > rpc::identity::MAX_DEVICE_NAME_BYTES
        {
            return Err("invalid MLS bootstrap device name".to_string());
        }
        if self.device_certificate.body.user_id != self.user_id
            || self.device_certificate.body.account_id != self.account_id
            || self.device_certificate.body.device_id != self.device_id
            || self.own_roster.body.user_id != self.user_id
            || self.own_roster.body.account_id != self.account_id
        {
            return Err("MLS bootstrap identity context is inconsistent".to_string());
        }
        rpc::identity::validate_device_roster(
            &self.own_roster,
            &self.server_public_key,
            self.user_id,
        )?;
        Ok(())
    }

    pub fn store_atomic(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        atomic_write(path, &jsony::to_binary(self))
    }
}

pub fn load_bootstrap(path: &Path) -> BootstrapLoad {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return BootstrapLoad::Missing;
        }
        Err(error) => return BootstrapLoad::Unreadable(error.to_string()),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(metadata) if metadata.permissions().mode() & 0o077 == 0 => {}
            Ok(_) => {
                return BootstrapLoad::Unreadable(
                    "MLS bootstrap permissions are not owner-only".to_string(),
                );
            }
            Err(error) => return BootstrapLoad::Unreadable(error.to_string()),
        }
    }
    match jsony::from_binary::<E2eBootstrap>(&bytes) {
        Ok(bootstrap) => match bootstrap.validate() {
            Ok(()) => BootstrapLoad::Loaded(bootstrap),
            Err(error) => BootstrapLoad::Unreadable(error),
        },
        Err(error) => BootstrapLoad::Unreadable(error.to_string()),
    }
}

pub fn classify_installation(
    loaded: BootstrapLoad,
    server_public_key: &[u8; 32],
    user_id: UserId,
    current_roster: Option<&SignedDeviceRoster>,
) -> InstallationState {
    let bootstrap = match loaded {
        BootstrapLoad::Missing => return InstallationState::Missing,
        BootstrapLoad::Unreadable(error) => return InstallationState::Unreadable(error),
        BootstrapLoad::Loaded(bootstrap) => bootstrap,
    };
    if &bootstrap.server_public_key != server_public_key || bootstrap.user_id != user_id {
        return InstallationState::BrokenInstallation {
            bootstrap,
            reason: "MLS bootstrap belongs to a different server or account".to_string(),
        };
    }
    if matches!(bootstrap.state, BootstrapState::PendingPair { .. }) {
        return InstallationState::PendingPair(bootstrap);
    }
    let Some(roster) = current_roster else {
        return InstallationState::Loaded(bootstrap);
    };
    if roster.body.account_id != bootstrap.account_id {
        return InstallationState::BrokenInstallation {
            bootstrap,
            reason: "server MLS account continuity does not match local authority".to_string(),
        };
    }
    if roster
        .body
        .active_devices
        .iter()
        .any(|certificate| certificate.body.device_id == bootstrap.device_id)
    {
        InstallationState::Active(bootstrap)
    } else {
        InstallationState::Revoked(bootstrap)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
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
                .map_err(|error| format!("failed to remove stale {}: {error}", tmp.display()))?;
            options
                .open(&tmp)
                .map_err(|error| format!("failed to create {}: {error}", tmp.display()))?
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
        && let Ok(directory) = File::open(parent)
    {
        directory
            .sync_all()
            .map_err(|error| format!("failed to sync {}: {error}", parent.display()))?;
    }
    Ok(())
}
