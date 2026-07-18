use std::{
    fs,
    path::{Path, PathBuf},
};

use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use mls_rs::{
    CipherSuiteProvider, CryptoProvider,
    identity::{SigningIdentity, basic::BasicCredential},
};
use mls_rs_core::crypto::SignaturePublicKey;
use mls_rs_crypto_awslc::AwsLcCryptoProvider;
use rpc::{
    identity::{
        DeviceCertificateBody, DeviceRosterBody, RosterCheckpoint, SignedDeviceRoster, account_id,
        authority_public_key, mls_client_id, roster_checkpoint, sign_device_certificate,
        sign_device_roster,
    },
    ids::{DeviceId, PairAttemptId, SessionId, UserId},
    mls::EncryptedRoomDescriptor,
};

use crate::{
    BOOTSTRAP_VERSION, BootstrapLoad, BootstrapState, CIPHER_SUITE, ChattIdentityProvider,
    E2eBootstrap, PersistentClient, load_bootstrap,
};

use mls_rs_provider_redb::{RedbDataStorageEngine, RedbStorageStats};

pub struct LocalInstallation {
    pub bootstrap: E2eBootstrap,
    pub client: PersistentClient,
    identities: ChattIdentityProvider,
    bootstrap_path: PathBuf,
    database_path: PathBuf,
}

pub enum StorageCompactionOutcome {
    Complete {
        installation: LocalInstallation,
        compacted: bool,
    },
    Failed {
        installation: Option<LocalInstallation>,
        error: String,
    },
}

impl std::fmt::Debug for LocalInstallation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalInstallation")
            .field("user_id", &self.bootstrap.user_id)
            .field("device_id", &self.bootstrap.device_id)
            .finish_non_exhaustive()
    }
}

impl LocalInstallation {
    pub fn open_or_create(
        data_dir: &Path,
        server_public_key: [u8; 32],
        user_id: UserId,
        device_name: &str,
    ) -> Result<(Self, bool), String> {
        fs::create_dir_all(data_dir)
            .map_err(|error| format!("failed to create {}: {error}", data_dir.display()))?;
        let bootstrap_path = data_dir.join("mls-bootstrap.bin");
        let database_path = data_dir.join("mls.redb");
        let (bootstrap, created, signing) = match load_bootstrap(&bootstrap_path) {
            BootstrapLoad::Missing => {
                let (bootstrap, identity, secret) =
                    create_bootstrap(server_public_key, user_id, device_name)?;
                bootstrap.store_atomic(&bootstrap_path)?;
                (bootstrap, true, Some((identity, secret)))
            }
            BootstrapLoad::Loaded(bootstrap) => {
                if bootstrap.server_public_key != server_public_key || bootstrap.user_id != user_id
                {
                    return Err(
                        "MLS bootstrap belongs to a different server or account".to_string()
                    );
                }
                (bootstrap, false, None)
            }
            BootstrapLoad::Unreadable(error) => {
                return Err(format!("MLS bootstrap is unreadable: {error}"));
            }
        };
        let identities = ChattIdentityProvider::new(server_public_key.to_vec());
        identities
            .install_roster(&bootstrap.own_roster)
            .map_err(|error| error.to_string())?;
        let client = match signing {
            Some((identity, secret)) => {
                PersistentClient::open(&database_path, identities.clone(), identity, secret)?
            }
            None => PersistentClient::reopen(&database_path, identities.clone())?,
        };
        Ok((
            Self {
                bootstrap,
                client,
                identities,
                bootstrap_path,
                database_path,
            },
            created,
        ))
    }

    pub fn roster_checkpoint(&self) -> RosterCheckpoint {
        roster_checkpoint(&self.bootstrap.own_roster)
    }

    pub fn binding_proof(&self, session_id: SessionId) -> Result<Vec<u8>, String> {
        let message = rpc::identity::mls_device_binding_message(
            session_id,
            self.bootstrap.device_id,
            self.roster_checkpoint(),
        );
        self.client.sign_device_binding(&message)
    }

    pub fn install_roster(&self, roster: &SignedDeviceRoster) -> Result<(), String> {
        self.identities
            .install_roster(roster)
            .map_err(|error| error.to_string())
    }

    pub fn install_room(&self, descriptor: EncryptedRoomDescriptor) -> Result<(), String> {
        self.identities
            .install_room(descriptor)
            .map_err(|error| error.to_string())
    }

    pub fn user_for_account(&self, account_id: rpc::ids::AccountId) -> Result<UserId, String> {
        self.identities
            .user_for_account(account_id)
            .map_err(|error| error.to_string())
    }

    pub fn replace_own_roster(&mut self, roster: SignedDeviceRoster) -> Result<(), String> {
        if roster_checkpoint(&roster) == self.roster_checkpoint() {
            return Ok(());
        }
        self.install_roster(&roster)?;
        self.bootstrap.own_roster = roster;
        self.bootstrap.store_atomic(&self.bootstrap_path)
    }

    pub fn roster_without(&self, device_id: DeviceId) -> Result<SignedDeviceRoster, String> {
        let mut devices = self.bootstrap.own_roster.body.active_devices.clone();
        let old_len = devices.len();
        devices.retain(|certificate| certificate.body.device_id != device_id);
        if devices.len() == old_len {
            return Err("MLS device is not active".to_string());
        }
        if devices.is_empty() {
            return Err("cannot revoke the last active MLS device".to_string());
        }
        sign_device_roster(
            DeviceRosterBody {
                user_id: self.bootstrap.user_id,
                account_id: self.bootstrap.account_id,
                authority_public_key: self.bootstrap.own_roster.body.authority_public_key,
                revision: self
                    .bootstrap
                    .own_roster
                    .body
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| "MLS roster revision exhausted".to_string())?,
                active_devices: devices,
            },
            &self.bootstrap.authority_seed,
        )
    }

    /// Creates the complete replacement installation before a device-link is
    /// redeemed. An exact retry reopens the same pending bootstrap/database;
    /// it never generates a second credential for the attempt.
    pub fn create_pending_pair(
        data_dir: &Path,
        server_public_key: [u8; 32],
        user_id: UserId,
        device_name: &str,
        authority_seed: [u8; 32],
        current_roster: &SignedDeviceRoster,
        attempt_id: PairAttemptId,
        redemption_secret: &str,
        bearer_token: &str,
    ) -> Result<Self, String> {
        let bootstrap_path = data_dir.join("mls-bootstrap.bin");
        match load_bootstrap(&bootstrap_path) {
            BootstrapLoad::Loaded(bootstrap) => {
                let exact = matches!(
                    &bootstrap.state,
                    BootstrapState::PendingPair {
                        attempt_id: stored_attempt,
                        redemption_secret: stored_secret,
                        bearer_token: stored_bearer,
                    } if *stored_attempt == attempt_id
                        && stored_secret == redemption_secret
                        && stored_bearer == bearer_token
                );
                if !exact {
                    return Err(
                        "another MLS installation already exists at the pairing destination"
                            .to_string(),
                    );
                }
                return Self::open_or_create(data_dir, server_public_key, user_id, device_name)
                    .map(|(installation, _)| installation);
            }
            BootstrapLoad::Unreadable(error) => {
                return Err(format!("existing MLS bootstrap is unreadable: {error}"));
            }
            BootstrapLoad::Missing => {}
        }
        rpc::identity::validate_device_roster(current_roster, &server_public_key, user_id)?;
        let expected_authority = authority_public_key(&authority_seed)?;
        if current_roster.body.authority_public_key != expected_authority
            || current_roster.body.account_id
                != account_id(&server_public_key, user_id, &expected_authority)
        {
            return Err("pairing authority does not match the current device roster".to_string());
        }
        fs::create_dir_all(data_dir)
            .map_err(|error| format!("failed to create {}: {error}", data_dir.display()))?;
        let (mut bootstrap, identity, secret) = create_paired_bootstrap(
            server_public_key,
            user_id,
            device_name,
            authority_seed,
            current_roster,
        )?;
        bootstrap.state = BootstrapState::PendingPair {
            attempt_id,
            redemption_secret: redemption_secret.to_string(),
            bearer_token: bearer_token.to_string(),
        };
        bootstrap.store_atomic(&bootstrap_path)?;
        let identities = ChattIdentityProvider::new(server_public_key.to_vec());
        identities
            .install_roster(&bootstrap.own_roster)
            .map_err(|error| error.to_string())?;
        let client = PersistentClient::open(
            &data_dir.join("mls.redb"),
            identities.clone(),
            identity,
            secret,
        )?;
        Ok(Self {
            bootstrap,
            client,
            identities,
            bootstrap_path,
            database_path: data_dir.join("mls.redb"),
        })
    }

    pub fn mark_pair_active(&mut self) -> Result<(), String> {
        if !matches!(self.bootstrap.state, BootstrapState::PendingPair { .. }) {
            return Err("MLS installation has no pending pair operation".to_string());
        }
        self.bootstrap.state = BootstrapState::Active;
        self.bootstrap.store_atomic(&self.bootstrap_path)
    }

    pub fn storage_stats(&self) -> Result<RedbStorageStats, String> {
        self.client.storage_stats()
    }

    pub fn storage_is_quiescent(&self) -> Result<bool, String> {
        self.client.storage_is_quiescent()
    }

    pub fn clear_transient_storage_state(&self) -> Result<(), String> {
        self.client.clear_transient_groups()
    }

    /// Drops every live redb adapter, compacts the uniquely opened file, and
    /// rebuilds the persistent MLS client with the existing authorization
    /// state.
    pub fn compact_storage(self) -> StorageCompactionOutcome {
        let Self {
            bootstrap,
            client,
            identities,
            bootstrap_path,
            database_path,
        } = self;
        drop(client);

        let compacted = RedbDataStorageEngine::compact_file(&database_path)
            .map_err(|error| error.to_string());
        let reopened = PersistentClient::reopen(&database_path, identities.clone()).map(|client| {
            Self {
                bootstrap,
                client,
                identities,
                bootstrap_path,
                database_path,
            }
        });

        match (compacted, reopened) {
            (Ok(compacted), Ok(installation)) => StorageCompactionOutcome::Complete {
                installation,
                compacted,
            },
            (compaction, reopened) => {
                let error = match (compaction, &reopened) {
                    (Err(compaction), Ok(_)) => {
                        format!("MLS database compaction failed: {compaction}")
                    }
                    (Ok(_), Err(reopen)) => {
                        format!("MLS database reopen after compaction failed: {reopen}")
                    }
                    (Err(compaction), Err(reopen)) => format!(
                        "MLS database compaction failed: {compaction}; reopen failed: {reopen}"
                    ),
                    (Ok(_), Ok(_)) => unreachable!(),
                };
                StorageCompactionOutcome::Failed {
                    installation: reopened.ok(),
                    error,
                }
            }
        }
    }
}

fn create_paired_bootstrap(
    server_public_key: [u8; 32],
    user_id: UserId,
    device_name: &str,
    authority_seed: [u8; 32],
    current_roster: &SignedDeviceRoster,
) -> Result<
    (
        E2eBootstrap,
        SigningIdentity,
        mls_rs_core::crypto::SignatureSecretKey,
    ),
    String,
> {
    let rng = SystemRandom::new();
    let mut device_bytes = [0u8; 16];
    rng.fill(&mut device_bytes)
        .map_err(|_| "failed to generate MLS device id".to_string())?;
    let authority_public_key = authority_public_key(&authority_seed)?;
    let account_id = account_id(&server_public_key, user_id, &authority_public_key);
    let device_id = DeviceId(device_bytes);
    let client_id = mls_client_id(&server_public_key, account_id, device_id)?;
    let cipher = AwsLcCryptoProvider::default()
        .cipher_suite_provider(CIPHER_SUITE)
        .ok_or_else(|| "mandatory MLS cipher suite is unavailable".to_string())?;
    let (signing_secret, signing_public) = cipher
        .signature_key_generate()
        .map_err(|error| error.to_string())?;
    let certificate = sign_device_certificate(
        DeviceCertificateBody {
            user_id,
            account_id,
            authority_public_key,
            device_id,
            device_name: device_name.to_string(),
            mls_client_id: client_id.clone(),
            mls_signature_public_key: signing_public.as_ref().to_vec(),
        },
        &authority_seed,
    )?;
    let mut active_devices = current_roster.body.active_devices.clone();
    active_devices.push(certificate.clone());
    active_devices.sort_by_key(|certificate| certificate.body.device_id);
    let roster = sign_device_roster(
        DeviceRosterBody {
            user_id,
            account_id,
            authority_public_key,
            revision: current_roster
                .body
                .revision
                .checked_add(1)
                .ok_or_else(|| "MLS roster revision exhausted".to_string())?,
            active_devices,
        },
        &authority_seed,
    )?;
    let bootstrap = E2eBootstrap {
        version: BOOTSTRAP_VERSION,
        server_public_key,
        user_id,
        account_id,
        authority_seed,
        device_id,
        device_name: device_name.to_string(),
        device_certificate: certificate,
        own_roster: roster,
        state: BootstrapState::Active,
    };
    let identity = SigningIdentity::new(
        BasicCredential::new(client_id).into_credential(),
        SignaturePublicKey::from(signing_public.as_ref().to_vec()),
    );
    Ok((bootstrap, identity, signing_secret))
}

fn create_bootstrap(
    server_public_key: [u8; 32],
    user_id: UserId,
    device_name: &str,
) -> Result<
    (
        E2eBootstrap,
        SigningIdentity,
        mls_rs_core::crypto::SignatureSecretKey,
    ),
    String,
> {
    let rng = SystemRandom::new();
    let mut authority_seed = [0u8; 32];
    let mut device_bytes = [0u8; 16];
    rng.fill(&mut authority_seed)
        .map_err(|_| "failed to generate MLS account authority".to_string())?;
    rng.fill(&mut device_bytes)
        .map_err(|_| "failed to generate MLS device id".to_string())?;
    let authority_public_key = authority_public_key(&authority_seed)?;
    let account_id = account_id(&server_public_key, user_id, &authority_public_key);
    let device_id = DeviceId(device_bytes);
    let client_id = mls_client_id(&server_public_key, account_id, device_id)?;
    let cipher = AwsLcCryptoProvider::default()
        .cipher_suite_provider(CIPHER_SUITE)
        .ok_or_else(|| "mandatory MLS cipher suite is unavailable".to_string())?;
    let (signing_secret, signing_public) = cipher
        .signature_key_generate()
        .map_err(|error| error.to_string())?;
    let certificate = sign_device_certificate(
        DeviceCertificateBody {
            user_id,
            account_id,
            authority_public_key,
            device_id,
            device_name: device_name.to_string(),
            mls_client_id: client_id.clone(),
            mls_signature_public_key: signing_public.as_ref().to_vec(),
        },
        &authority_seed,
    )?;
    let roster = sign_device_roster(
        DeviceRosterBody {
            user_id,
            account_id,
            authority_public_key,
            revision: 1,
            active_devices: vec![certificate.clone()],
        },
        &authority_seed,
    )?;
    let bootstrap = E2eBootstrap {
        version: BOOTSTRAP_VERSION,
        server_public_key,
        user_id,
        account_id,
        authority_seed,
        device_id,
        device_name: device_name.to_string(),
        device_certificate: certificate,
        own_roster: roster,
        state: BootstrapState::Active,
    };
    let identity = SigningIdentity::new(
        BasicCredential::new(client_id).into_credential(),
        SignaturePublicKey::from(signing_public.as_ref().to_vec()),
    );
    Ok((bootstrap, identity, signing_secret))
}
