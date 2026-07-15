//! Owner-only local account/device identity and replay state.
//!
//! Large and security-sensitive state deliberately lives outside `chatt.toml`:
//! device private keys, signed account ledgers, sender sequence reservations,
//! peer checkpoints, and the durable event replay journal share one atomic file
//! keyed by server identity and authenticated user id.

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use jsony::Jsony;
use ring::{digest, rand::SecureRandom};
use rpc::{
    crypto::encode_hex,
    e2e::{
        ACCOUNT_AUTHORITY_KEY_LEN, AccountKeyAction, AccountKeyStatement,
        AccountKeyStatementBody, DevicePublicKeys, E2E_SEED_LEN, DEVICE_SIGNING_KEY_LEN,
        MAX_VERIFICATION_SYNC_RECORDS, ValidatedAccountLedger, VerificationSyncCheckpoint,
        VerificationSyncRecord, VerificationSyncSnapshot, VerificationSyncState, account_id,
        account_statement_hash, ed25519_public_key, e2e_public_key, random_device_id,
        sign_account_statement,
    },
    ids::{AccountId, DeviceId, EventId, LedgerHash, RoomId, UserId},
};
use zeroize::{Zeroize, ZeroizeOnDrop};

const STORE_VERSION: u16 = 2;

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
pub(crate) struct StoredPeerLedger {
    pub user_id: UserId,
    pub statements: Vec<AccountKeyStatement>,
    pub independently_verified: bool,
}

#[derive(Clone, Copy, Debug, Jsony)]
#[jsony(Binary, version)]
pub(crate) struct StoredSenderChain {
    pub room_id: RoomId,
    pub key_epoch: u64,
    pub next_sequence: u64,
    pub predecessor: Option<EventId>,
}

#[derive(Clone, Copy, Debug, Jsony)]
#[jsony(Binary, version)]
pub(crate) struct StoredReplayHead {
    pub sender: UserId,
    pub device_id: DeviceId,
    pub key_epoch: u64,
    pub room_id: RoomId,
    pub highest_sequence: u64,
    pub event_id: EventId,
}

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
pub(crate) struct StoredSeenEvent {
    pub event_id: EventId,
    pub digest: Vec<u8>,
    pub applied: bool,
}

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
struct IdentityFile {
    version: u16,
    server_public_key: Vec<u8>,
    user_id: UserId,
    account_id: AccountId,
    authority_seed: Vec<u8>,
    device_id: DeviceId,
    device_name: String,
    device_key_epoch: u64,
    device_signing_seed: Vec<u8>,
    device_encryption_seed: Vec<u8>,
    own_ledger: Vec<AccountKeyStatement>,
    server_registered: bool,
    peers: Vec<StoredPeerLedger>,
    sender_chains: Vec<StoredSenderChain>,
    replay_heads: Vec<StoredReplayHead>,
    seen_events: Vec<StoredSeenEvent>,
    verification_snapshot: VerificationSyncSnapshot,
    verification_checkpoint: Option<VerificationSyncCheckpoint>,
    verification_pending: Vec<VerificationSyncRecord>,
    verification_notices: Vec<VerificationIdentity>,
    verification_republish: bool,
    staged_link_secret_hash: Vec<u8>,
    staged_bearer_token: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
struct VerificationIdentity {
    user_id: UserId,
    account_id: AccountId,
}

#[derive(Jsony, Zeroize, ZeroizeOnDrop)]
#[jsony(Binary, version)]
struct EnrollmentAuthority {
    authority_seed: Vec<u8>,
    #[zeroize(skip)]
    verification_records: Vec<VerificationSyncRecord>,
    #[zeroize(skip)]
    verification_checkpoint: Option<VerificationSyncCheckpoint>,
}

pub(crate) struct LocalE2eIdentity {
    path: PathBuf,
    file: IdentityFile,
}

pub(crate) struct ReservedEvent {
    pub event_id: EventId,
    pub sequence: u64,
    pub predecessor: Option<EventId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplayObservation {
    Fresh,
    Duplicate,
    Stale,
    Fork,
}

impl LocalE2eIdentity {
    pub(crate) fn enrollment_authority(&self) -> Result<Vec<u8>, String> {
        let verification_records = self.next_verification_snapshot()?.records;
        Ok(jsony::to_binary(&EnrollmentAuthority {
            authority_seed: self.file.authority_seed.clone(),
            verification_records,
            verification_checkpoint: self.file.verification_checkpoint,
        }))
    }

    pub(crate) fn prepare_linked_device(
        data_dir: &Path,
        server_public_key: &[u8],
        user_id: UserId,
        link_secret_hash: &[u8],
        device_name: &str,
        statements: &[AccountKeyStatement],
        enrollment_plaintext: &[u8],
        overwrite_existing: bool,
        rng: &dyn SecureRandom,
    ) -> Result<(Self, AccountKeyStatement), String> {
        let final_path = identity_path(data_dir, server_public_key, user_id);
        if final_path.exists() && !overwrite_existing {
            return Err(
                "this installation already has an E2E identity for the linked account"
                    .to_string(),
            );
        }
        let path = staged_link_path(&final_path, link_secret_hash);
        if device_name.trim().is_empty()
            || device_name.len() > rpc::e2e::MAX_DEVICE_NAME_BYTES
            || device_name.chars().any(char::is_control)
        {
            return Err("device name must be 1-64 bytes with no control characters".to_string());
        }
        let enrollment: EnrollmentAuthority = jsony::from_binary(enrollment_plaintext)
            .map_err(|error| format!("invalid device enrollment bundle: {error}"))?;
        let authority_seed = fixed::<ACCOUNT_AUTHORITY_KEY_LEN>(
            &enrollment.authority_seed,
            "enrollment authority seed",
        )?;
        let ledger = ValidatedAccountLedger::validate(server_public_key, user_id, statements)?;
        if ed25519_public_key(&authority_seed)? != ledger.authority_public_key {
            return Err("enrollment authority does not match the signed account ledger".to_string());
        }
        let verification_notices: Vec<_> = enrollment
            .verification_records
            .iter()
            .filter(|record| record.state == VerificationSyncState::Verified)
            .map(|record| VerificationIdentity {
                user_id: record.user_id,
                account_id: record.account_id,
            })
            .collect();
        let verification_pending = enrollment
            .verification_records
            .iter()
            .map(|record| VerificationSyncRecord {
                modified_at: 0,
                ..*record
            })
            .collect();

        let mut signing_seed = [0u8; DEVICE_SIGNING_KEY_LEN];
        let mut encryption_seed = [0u8; E2E_SEED_LEN];
        rng.fill(&mut signing_seed)
            .map_err(|_| "failed to generate device signing key".to_string())?;
        rng.fill(&mut encryption_seed)
            .map_err(|_| "failed to generate device encryption key".to_string())?;
        let device_id = random_device_id(rng).map_err(|error| error.to_string())?;
        let device = DevicePublicKeys {
            device_id,
            name: device_name.trim().to_string(),
            key_epoch: 1,
            signing_public_key: ed25519_public_key(&signing_seed)?.to_vec(),
            encryption_public_key: e2e_public_key(&encryption_seed).to_vec(),
        };
        let statement = sign_account_statement(
            AccountKeyStatementBody {
                account_id: ledger.account_id,
                account_generation: ledger.account_generation,
                roster_epoch: ledger.roster_epoch.saturating_add(1),
                previous: ledger.head,
                authority_key_epoch: ledger.authority_key_epoch,
                action: AccountKeyAction::AddDevice {
                    device: device.clone(),
                },
            },
            &authority_seed,
            None,
        )?;
        let mut own_ledger = statements.to_vec();
        own_ledger.push(statement.clone());
        ValidatedAccountLedger::validate(server_public_key, user_id, &own_ledger)?;
        let identity = Self {
            path,
            file: IdentityFile {
                version: STORE_VERSION,
                server_public_key: server_public_key.to_vec(),
                user_id,
                account_id: ledger.account_id,
                authority_seed: authority_seed.to_vec(),
                device_id,
                device_name: device.name,
                device_key_epoch: 1,
                device_signing_seed: signing_seed.to_vec(),
                device_encryption_seed: encryption_seed.to_vec(),
                own_ledger,
                server_registered: false,
                peers: Vec::new(),
                sender_chains: Vec::new(),
                replay_heads: Vec::new(),
                seen_events: Vec::new(),
                verification_snapshot: VerificationSyncSnapshot::default(),
                verification_checkpoint: enrollment.verification_checkpoint,
                verification_pending,
                verification_notices,
                verification_republish: true,
                staged_link_secret_hash: Vec::new(),
                staged_bearer_token: String::new(),
            },
        };
        identity.validate(server_public_key, user_id)?;
        Ok((identity, statement))
    }

    pub(crate) fn linked_device_path(
        data_dir: &Path,
        server_public_key: &[u8],
        user_id: UserId,
    ) -> PathBuf {
        identity_path(data_dir, server_public_key, user_id)
    }

    pub(crate) fn stage_linked_device(
        &mut self,
        link_secret_hash: &[u8],
        bearer_token: &str,
    ) -> Result<(), String> {
        self.file.staged_link_secret_hash = link_secret_hash.to_vec();
        self.file.staged_bearer_token = bearer_token.to_string();
        self.persist()
    }

    pub(crate) fn pending_linked_device(
        data_dir: &Path,
        server_public_key: &[u8],
        user_id: UserId,
        link_secret_hash: &[u8],
        account_chain: &[AccountKeyStatement],
    ) -> Result<Option<(Self, AccountKeyStatement, String)>, String> {
        let final_path = identity_path(data_dir, server_public_key, user_id);
        let path = staged_link_path(&final_path, link_secret_hash);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
        };
        let file: IdentityFile = jsony::from_binary(&bytes)
            .map_err(|error| format!("failed to decode staged linked device: {error}"))?;
        let identity = Self { path, file };
        identity.validate(server_public_key, user_id)?;
        if identity.file.staged_link_secret_hash != link_secret_hash
            || identity.file.staged_bearer_token.is_empty()
        {
            return Ok(None);
        }
        let statements = identity.own_statements();
        let Some(statement) = statements.last().cloned() else {
            return Ok(None);
        };
        let chain_matches_before = statements.len() == account_chain.len() + 1
            && statements[..account_chain.len()] == *account_chain;
        let chain_matches_after = account_chain.starts_with(statements);
        if !chain_matches_before && !chain_matches_after {
            return Ok(None);
        }
        let bearer = identity.file.staged_bearer_token.clone();
        Ok(Some((identity, statement, bearer)))
    }

    pub(crate) fn commit_linked_device(mut self) -> Result<(), String> {
        self.file.server_registered = true;
        self.file.staged_link_secret_hash.clear();
        self.file.staged_bearer_token.zeroize();
        self.persist()?;
        let data_dir = self
            .path
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| "staged linked-device path has no data directory".to_string())?;
        let final_path = identity_path(
            data_dir,
            &self.file.server_public_key,
            self.file.user_id,
        );
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let archive = final_path.with_extension(format!("pre-link-{stamp}"));
        let archived = if final_path.exists() {
            fs::rename(&final_path, &archive).map_err(|error| {
                format!(
                    "failed to archive {} as {}: {error}",
                    final_path.display(),
                    archive.display()
                )
            })?;
            true
        } else {
            false
        };
        if let Err(error) = fs::rename(&self.path, &final_path) {
            if archived {
                let _ = fs::rename(&archive, &final_path);
            }
            return Err(format!(
                "failed to install linked-device identity {}: {error}",
                final_path.display()
            ));
        }
        self.path = final_path;
        Ok(())
    }

    pub(crate) fn discard_staged_link(self) {
        let _ = fs::remove_file(&self.path);
    }

    pub(crate) fn load_or_create(
        data_dir: &Path,
        server_public_key: &[u8],
        user_id: UserId,
        device_name: &str,
        rng: &dyn SecureRandom,
    ) -> Result<(Self, bool), String> {
        let path = identity_path(data_dir, server_public_key, user_id);
        match fs::read(&path) {
            Ok(bytes) => {
                let file: IdentityFile = jsony::from_binary(&bytes).map_err(|error| {
                    identity_unavailable(&path, &format!("the file could not be decoded: {error}"))
                })?;
                let identity = Self { path, file };
                identity
                    .validate(server_public_key, user_id)
                    .map_err(|error| identity_unavailable(&identity.path, &error))?;
                Ok((identity, false))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut authority_seed = [0u8; ACCOUNT_AUTHORITY_KEY_LEN];
                let mut signing_seed = [0u8; DEVICE_SIGNING_KEY_LEN];
                let mut encryption_seed = [0u8; E2E_SEED_LEN];
                rng.fill(&mut authority_seed)
                    .map_err(|_| "failed to generate account authority".to_string())?;
                rng.fill(&mut signing_seed)
                    .map_err(|_| "failed to generate device signing key".to_string())?;
                rng.fill(&mut encryption_seed)
                    .map_err(|_| "failed to generate device encryption key".to_string())?;
                let device_id = random_device_id(rng).map_err(|error| error.to_string())?;
                let authority_public = ed25519_public_key(&authority_seed)?;
                let account_id = account_id(server_public_key, user_id, &authority_public);
                let genesis = sign_account_statement(
                    AccountKeyStatementBody {
                        account_id,
                        account_generation: 1,
                        roster_epoch: 1,
                        previous: LedgerHash::default(),
                        authority_key_epoch: 1,
                        action: AccountKeyAction::Genesis {
                            authority_public_key: authority_public.to_vec(),
                            first_device: DevicePublicKeys {
                                device_id,
                                name: device_name.to_string(),
                                key_epoch: 1,
                                signing_public_key: ed25519_public_key(&signing_seed)?.to_vec(),
                                encryption_public_key: e2e_public_key(&encryption_seed).to_vec(),
                            },
                        },
                    },
                    &authority_seed,
                    None,
                )?;
                let identity = Self {
                    path,
                    file: IdentityFile {
                        version: STORE_VERSION,
                        server_public_key: server_public_key.to_vec(),
                        user_id,
                        account_id,
                        authority_seed: authority_seed.to_vec(),
                        device_id,
                        device_name: device_name.to_string(),
                        device_key_epoch: 1,
                        device_signing_seed: signing_seed.to_vec(),
                        device_encryption_seed: encryption_seed.to_vec(),
                        own_ledger: vec![genesis],
                        server_registered: false,
                        peers: Vec::new(),
                        sender_chains: Vec::new(),
                        replay_heads: Vec::new(),
                        seen_events: Vec::new(),
                        verification_snapshot: VerificationSyncSnapshot::default(),
                        verification_checkpoint: None,
                        verification_pending: Vec::new(),
                        verification_notices: Vec::new(),
                        verification_republish: false,
                        staged_link_secret_hash: Vec::new(),
                        staged_bearer_token: String::new(),
                    },
                };
                identity.persist()?;
                Ok((identity, true))
            }
            Err(error) => Err(identity_unavailable(
                &path,
                &format!("the file could not be read: {error}"),
            )),
        }
    }

    pub(crate) fn user_id(&self) -> UserId {
        self.file.user_id
    }

    pub(crate) fn account_id(&self) -> AccountId {
        self.file.account_id
    }

    pub(crate) fn server_public_key(&self) -> &[u8] {
        &self.file.server_public_key
    }

    pub(crate) fn device_id(&self) -> DeviceId {
        self.file.device_id
    }

    pub(crate) fn device_key_epoch(&self) -> u64 {
        self.file.device_key_epoch
    }

    pub(crate) fn signing_seed(&self) -> &[u8; DEVICE_SIGNING_KEY_LEN] {
        self.file.device_signing_seed.as_slice().try_into().unwrap()
    }

    pub(crate) fn encryption_seed(&self) -> &[u8; E2E_SEED_LEN] {
        self.file.device_encryption_seed.as_slice().try_into().unwrap()
    }

    pub(crate) fn own_statements(&self) -> &[AccountKeyStatement] {
        &self.file.own_ledger
    }

    pub(crate) fn own_ledger(&self) -> ValidatedAccountLedger {
        ValidatedAccountLedger::validate(
            &self.file.server_public_key,
            self.file.user_id,
            &self.file.own_ledger,
        )
        .expect("identity store validated before use")
    }

    pub(crate) fn server_registered(&self) -> bool {
        self.file.server_registered
    }

    pub(crate) fn peer_statements(&self, user_id: UserId) -> Option<&[AccountKeyStatement]> {
        self.file
            .peers
            .iter()
            .find(|peer| peer.user_id == user_id)
            .map(|peer| peer.statements.as_slice())
    }

    pub(crate) fn replace_own_ledger(
        &mut self,
        statements: Vec<AccountKeyStatement>,
    ) -> Result<ValidatedAccountLedger, String> {
        let ledger = ValidatedAccountLedger::validate(
            &self.file.server_public_key,
            self.file.user_id,
            &statements,
        )?;
        if ledger.account_id != self.file.account_id {
            return Err("server account ledger conflicts with the local account identity".to_string());
        }
        require_monotonic_chain(&self.file.own_ledger, &statements)?;
        let device = ledger
            .device_key(self.file.device_id, self.file.device_key_epoch)
            .ok_or_else(|| "local device is absent from the server account ledger".to_string())?;
        if device.status != rpc::e2e::DeviceKeyStatus::Active {
            return Err("local device has been retired or revoked".to_string());
        }
        let roster_changed = self.file.own_ledger.last().map(account_statement_hash)
            != statements.last().map(account_statement_hash);
        self.file.own_ledger = statements;
        self.file.server_registered = true;
        if roster_changed
            && (self.file.verification_checkpoint.is_some()
                || !self.file.verification_snapshot.records.is_empty()
                || !self.file.verification_pending.is_empty())
        {
            self.file.verification_republish = true;
        }
        self.persist()?;
        Ok(ledger)
    }

    pub(crate) fn store_peer_ledger(
        &mut self,
        user_id: UserId,
        statements: Vec<AccountKeyStatement>,
    ) -> Result<ValidatedAccountLedger, String> {
        let ledger = ValidatedAccountLedger::validate(
            &self.file.server_public_key,
            user_id,
            &statements,
        )?;
        let existing = self.file.peers.iter_mut().find(|peer| peer.user_id == user_id);
        match existing {
            Some(peer) => {
                require_monotonic_chain(&peer.statements, &statements)?;
                peer.statements = statements;
            }
            None => self.file.peers.push(StoredPeerLedger {
                user_id,
                statements,
                independently_verified: false,
            }),
        }
        self.persist()?;
        Ok(ledger)
    }

    pub(crate) fn verification_checkpoint(&self) -> Option<VerificationSyncCheckpoint> {
        self.file.verification_checkpoint
    }

    pub(crate) fn verification_dirty(&self) -> bool {
        self.file.verification_republish || !self.file.verification_pending.is_empty()
    }

    pub(crate) fn identity_verified(&self, user_id: UserId, account_id: AccountId) -> bool {
        self.identity_verification(user_id, account_id) == Some(true)
    }

    pub(crate) fn identity_verification(
        &self,
        user_id: UserId,
        account_id: AccountId,
    ) -> Option<bool> {
        effective_verification_record(&self.file, user_id, account_id).map(|record| {
            record.state == VerificationSyncState::Verified
        })
    }

    pub(crate) fn set_identity_verified(
        &mut self,
        user_id: UserId,
        account_id: AccountId,
        verified: bool,
    ) -> Result<(), String> {
        if user_id.0 == 0
            || user_id == self.file.user_id
            || account_id == AccountId::default()
        {
            return Err("verification sync identity is invalid".to_string());
        }
        let state = if verified {
            VerificationSyncState::Verified
        } else {
            VerificationSyncState::Unverified
        };
        if effective_verification_record(&self.file, user_id, account_id)
            .is_some_and(|record| record.state == state)
        {
            return Ok(());
        }
        if let Some(pending) = self
            .file
            .verification_pending
            .iter_mut()
            .find(|record| record.user_id == user_id && record.account_id == account_id)
        {
            pending.state = state;
        } else {
            let already_in_snapshot = self.file.verification_snapshot.records.iter().any(
                |record| record.user_id == user_id && record.account_id == account_id,
            );
            let identities = self.file.verification_snapshot.records.len().saturating_add(
                self.file
                    .verification_pending
                    .iter()
                    .filter(|pending| {
                        !self.file.verification_snapshot.records.iter().any(|record| {
                            record.user_id == pending.user_id
                                && record.account_id == pending.account_id
                        })
                    })
                    .count(),
            );
            if !already_in_snapshot && identities >= MAX_VERIFICATION_SYNC_RECORDS {
                return Err("verification sync has reached its 256-identity limit".to_string());
            }
            self.file.verification_pending.push(VerificationSyncRecord {
                user_id,
                account_id,
                state,
                modified_at: 0,
            });
        }
        if !verified {
            self.file.verification_notices.retain(|identity| {
                identity.user_id != user_id || identity.account_id != account_id
            });
        }
        self.persist()
    }

    /// Builds the next canonical snapshot without mutating the durable base.
    /// The caller retains it until the server acknowledges its checkpoint.
    pub(crate) fn next_verification_snapshot(&self) -> Result<VerificationSyncSnapshot, String> {
        let version = self
            .file
            .verification_checkpoint
            .map_or(1, |checkpoint| checkpoint.version.saturating_add(1));
        let mut records = self.file.verification_snapshot.records.clone();
        for pending in &self.file.verification_pending {
            let mut pending = *pending;
            pending.modified_at = version;
            match records.iter_mut().find(|record| {
                record.user_id == pending.user_id && record.account_id == pending.account_id
            }) {
                Some(record) => *record = pending,
                None => records.push(pending),
            }
        }
        records.sort_unstable_by_key(|record| (record.user_id, record.account_id));
        if records.len() > MAX_VERIFICATION_SYNC_RECORDS {
            return Err("verification sync has reached its 256-identity limit".to_string());
        }
        Ok(VerificationSyncSnapshot { records })
    }

    /// Applies an authenticated snapshot. Local unsent changes remain as a
    /// bounded overlay, while only remote Verified transitions create notices.
    pub(crate) fn apply_verification_snapshot(
        &mut self,
        checkpoint: VerificationSyncCheckpoint,
        snapshot: VerificationSyncSnapshot,
    ) -> Result<Vec<(UserId, AccountId)>, String> {
        if let Some(known) = self.file.verification_checkpoint {
            if checkpoint.version < known.version {
                return Err("verification sync attempted a rollback".to_string());
            }
            if checkpoint.version == known.version && checkpoint != known {
                return Err("verification sync forked a known version".to_string());
            }
        }
        let pending = self.file.verification_pending.clone();
        let newly_verified: Vec<_> = snapshot
            .records
            .iter()
            .filter(|record| record.state == VerificationSyncState::Verified)
            .filter(|record| {
                !self.identity_verified(record.user_id, record.account_id)
                    && !pending.iter().any(|local| {
                        local.user_id == record.user_id && local.account_id == record.account_id
                    })
            })
            .map(|record| (record.user_id, record.account_id))
            .collect();
        self.file.verification_snapshot = snapshot;
        self.file.verification_checkpoint = Some(checkpoint);
        for (user_id, account_id) in &newly_verified {
            let identity = VerificationIdentity {
                user_id: *user_id,
                account_id: *account_id,
            };
            if !self.file.verification_notices.contains(&identity) {
                self.file.verification_notices.push(identity);
            }
        }
        let effective: Vec<_> = self
            .file
            .verification_notices
            .iter()
            .copied()
            .filter(|identity| {
                effective_verification_record(&self.file, identity.user_id, identity.account_id)
                    .is_some_and(|record| record.state == VerificationSyncState::Verified)
            })
            .collect();
        self.file.verification_notices = effective;
        self.persist()?;
        Ok(newly_verified)
    }

    pub(crate) fn commit_verification_snapshot(
        &mut self,
        checkpoint: VerificationSyncCheckpoint,
        snapshot: VerificationSyncSnapshot,
    ) -> Result<(), String> {
        self.file.verification_pending.retain(|pending| {
            !snapshot.records.iter().any(|record| {
                record.user_id == pending.user_id
                    && record.account_id == pending.account_id
                    && record.state == pending.state
            })
        });
        self.file.verification_snapshot = snapshot;
        self.file.verification_checkpoint = Some(checkpoint);
        self.file.verification_republish = false;
        self.persist()
    }

    pub(crate) fn verification_notice_pending(
        &self,
        user_id: UserId,
        account_id: AccountId,
    ) -> bool {
        self.file
            .verification_notices
            .contains(&VerificationIdentity { user_id, account_id })
    }

    pub(crate) fn acknowledge_verification_notice(
        &mut self,
        user_id: UserId,
        account_id: AccountId,
    ) -> Result<(), String> {
        self.file.verification_notices.retain(|identity| {
            identity.user_id != user_id || identity.account_id != account_id
        });
        self.persist()
    }

    pub(crate) fn reserve_event(
        &mut self,
        room_id: RoomId,
        rng: &dyn SecureRandom,
    ) -> Result<ReservedEvent, String> {
        let event_id = rpc::e2e::random_event_id(rng).map_err(|error| error.to_string())?;
        let key_epoch = self.file.device_key_epoch;
        let chain = self
            .file
            .sender_chains
            .iter_mut()
            .find(|chain| chain.room_id == room_id && chain.key_epoch == key_epoch);
        let (sequence, predecessor) = match chain {
            Some(chain) => {
                let sequence = chain.next_sequence;
                chain.next_sequence = chain.next_sequence.saturating_add(1);
                (sequence, chain.predecessor)
            }
            None => {
                self.file.sender_chains.push(StoredSenderChain {
                    room_id,
                    key_epoch,
                    next_sequence: 2,
                    predecessor: None,
                });
                (1, None)
            }
        };
        // Persist the reservation before use. A crash may leave a gap, but can
        // never reuse a sequence under the same signing key epoch.
        self.persist()?;
        Ok(ReservedEvent {
            event_id,
            sequence,
            predecessor,
        })
    }

    pub(crate) fn commit_sent_event(
        &mut self,
        room_id: RoomId,
        event_id: EventId,
    ) -> Result<(), String> {
        let key_epoch = self.file.device_key_epoch;
        let chain = self
            .file
            .sender_chains
            .iter_mut()
            .find(|chain| chain.room_id == room_id && chain.key_epoch == key_epoch)
            .ok_or_else(|| "sender chain disappeared after event reservation".to_string())?;
        chain.predecessor = Some(event_id);
        self.persist()
    }

    pub(crate) fn observe_event(
        &mut self,
        sender: UserId,
        device_id: DeviceId,
        key_epoch: u64,
        room_id: RoomId,
        sequence: u64,
        predecessor: Option<EventId>,
        event_id: EventId,
        encoded: &[u8],
        live: bool,
    ) -> Result<ReplayObservation, String> {
        let event_digest = digest::digest(&digest::SHA256, encoded).as_ref().to_vec();
        if let Some(seen) = self
            .file
            .seen_events
            .iter()
            .find(|seen| seen.event_id == event_id)
        {
            return Ok(if seen.digest == event_digest {
                if seen.applied {
                    ReplayObservation::Duplicate
                } else {
                    // The journal write is deliberately before application.
                    // An unapplied matching record is crash recovery, not a
                    // replay: complete the idempotent effect and then mark it.
                    ReplayObservation::Fresh
                }
            } else {
                ReplayObservation::Fork
            });
        }
        let head = self.file.replay_heads.iter_mut().find(|head| {
            head.sender == sender
                && head.device_id == device_id
                && head.key_epoch == key_epoch
                && head.room_id == room_id
        });
        if let Some(head) = head {
            if sequence == head.highest_sequence && event_id != head.event_id {
                return Ok(ReplayObservation::Fork);
            }
            if live && sequence <= head.highest_sequence {
                return Ok(ReplayObservation::Stale);
            }
            if live
                && sequence == head.highest_sequence.saturating_add(1)
                && predecessor != Some(head.event_id)
            {
                return Ok(ReplayObservation::Fork);
            }
            if sequence > head.highest_sequence {
                head.highest_sequence = sequence;
                head.event_id = event_id;
            }
        } else {
            self.file.replay_heads.push(StoredReplayHead {
                sender,
                device_id,
                key_epoch,
                room_id,
                highest_sequence: sequence,
                event_id,
            });
        }
        self.file.seen_events.push(StoredSeenEvent {
            event_id,
            digest: event_digest,
            applied: false,
        });
        self.persist()?;
        Ok(ReplayObservation::Fresh)
    }

    pub(crate) fn mark_event_applied(&mut self, event_id: EventId) -> Result<(), String> {
        let seen = self
            .file
            .seen_events
            .iter_mut()
            .find(|seen| seen.event_id == event_id)
            .ok_or_else(|| "applied event is absent from the replay journal".to_string())?;
        seen.applied = true;
        self.persist()
    }

    fn validate(&self, server_public_key: &[u8], user_id: UserId) -> Result<(), String> {
        if self.file.version != STORE_VERSION
            || self.file.server_public_key != server_public_key
            || self.file.user_id != user_id
            || self.file.device_key_epoch == 0
            || self.file.device_name.trim().is_empty()
        {
            return Err("local E2E identity context is invalid".to_string());
        }
        let authority_seed = fixed::<ACCOUNT_AUTHORITY_KEY_LEN>(
            &self.file.authority_seed,
            "account authority seed",
        )?;
        let signing_seed = fixed::<DEVICE_SIGNING_KEY_LEN>(
            &self.file.device_signing_seed,
            "device signing seed",
        )?;
        let encryption_seed = fixed::<E2E_SEED_LEN>(
            &self.file.device_encryption_seed,
            "device encryption seed",
        )?;
        let ledger = ValidatedAccountLedger::validate(
            server_public_key,
            user_id,
            &self.file.own_ledger,
        )?;
        if ledger.account_id != self.file.account_id
            || account_id(
                server_public_key,
                user_id,
                &ed25519_public_key(&authority_seed)?,
            ) != self.file.account_id
        {
            return Err("local account authority does not match the ledger".to_string());
        }
        let device = ledger
            .device_key(self.file.device_id, self.file.device_key_epoch)
            .ok_or_else(|| "local device is absent from its account ledger".to_string())?;
        if device.keys.signing_public_key != ed25519_public_key(&signing_seed)?.to_vec()
            || device.keys.encryption_public_key != e2e_public_key(&encryption_seed)
        {
            return Err("local private device keys do not match the ledger".to_string());
        }
        let checkpoint_version = self.file.verification_checkpoint.map(|known| known.version);
        if checkpoint_version == Some(0)
            || (checkpoint_version.is_none()
                && !self.file.verification_snapshot.records.is_empty())
            || self.file.verification_snapshot.records.len() > MAX_VERIFICATION_SYNC_RECORDS
            || self.file.verification_snapshot.records.iter().any(|record| {
                record.user_id.0 == 0
                    || record.user_id == user_id
                    || record.account_id == AccountId::default()
                    || record.modified_at == 0
                    || Some(record.modified_at) > checkpoint_version
            })
            || self
                .file
                .verification_snapshot
                .records
                .windows(2)
                .any(|pair| {
                    (pair[0].user_id, pair[0].account_id)
                        >= (pair[1].user_id, pair[1].account_id)
                })
            || self.file.verification_pending.len() > MAX_VERIFICATION_SYNC_RECORDS
            || self.file.verification_pending.iter().any(|record| {
                record.user_id.0 == 0
                    || record.user_id == user_id
                    || record.account_id == AccountId::default()
                    || record.modified_at != 0
            })
        {
            return Err("local verification sync state is invalid".to_string());
        }
        let mut identities = HashSet::new();
        for record in self
            .file
            .verification_snapshot
            .records
            .iter()
            .chain(&self.file.verification_pending)
        {
            identities.insert((record.user_id, record.account_id));
        }
        let notice_identities: HashSet<_> = self
            .file
            .verification_notices
            .iter()
            .map(|identity| (identity.user_id, identity.account_id))
            .collect();
        if identities.len() > MAX_VERIFICATION_SYNC_RECORDS
            || notice_identities.len() != self.file.verification_notices.len()
            || self.file.verification_notices.len() > MAX_VERIFICATION_SYNC_RECORDS
            || self.file.verification_notices.iter().any(|notice| {
                !self.identity_verified(notice.user_id, notice.account_id)
            })
        {
            return Err("local verification sync metadata is invalid".to_string());
        }
        Ok(())
    }

    fn persist(&self) -> Result<(), String> {
        let bytes = jsony::to_binary(&self.file);
        atomic_write_private(&self.path, &bytes)
    }
}

fn effective_verification_record(
    file: &IdentityFile,
    user_id: UserId,
    account_id: AccountId,
) -> Option<VerificationSyncRecord> {
    file.verification_pending
        .iter()
        .find(|record| record.user_id == user_id && record.account_id == account_id)
        .copied()
        .or_else(|| {
            file.verification_snapshot
                .records
                .iter()
                .find(|record| record.user_id == user_id && record.account_id == account_id)
                .copied()
        })
}

fn require_monotonic_chain(
    current: &[AccountKeyStatement],
    candidate: &[AccountKeyStatement],
) -> Result<(), String> {
    if candidate.len() < current.len() || candidate[..current.len()] != *current {
        return Err(
            "account key directory attempted a rollback or fork from a persisted checkpoint"
                .to_string(),
        );
    }
    Ok(())
}

fn fixed<const N: usize>(bytes: &[u8], name: &str) -> Result<[u8; N], String> {
    bytes
        .try_into()
        .map_err(|_| format!("{name} has the wrong length"))
}

fn identity_path(data_dir: &Path, server_public_key: &[u8], user_id: UserId) -> PathBuf {
    let mut context = Vec::with_capacity(server_public_key.len() + 8);
    context.extend_from_slice(server_public_key);
    context.extend_from_slice(&user_id.0.to_le_bytes());
    let name = rpc::crypto::encode_hex(digest::digest(&digest::SHA256, &context).as_ref());
    data_dir.join("e2e").join(format!("{name}.bin"))
}

fn staged_link_path(final_path: &Path, link_secret_hash: &[u8]) -> PathBuf {
    final_path.with_extension(format!("link-{}", encode_hex(link_secret_hash)))
}

fn identity_unavailable(path: &Path, reason: &str) -> String {
    format!(
        "Local E2E identity state at {} is unreadable or incompatible: {reason}. Chatt left the file unchanged. Restore it from a backup, or, if another linked device still works, move this file aside (do not delete it) and link this installation again. If neither is possible, preserve the file for diagnosis.",
        path.display()
    )
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("identity path {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let temp = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("failed to create {}: {error}", temp.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to write {}: {error}", temp.display()))?;
    fs::rename(&temp, path)
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::ids::VerificationSyncHash;

    fn identity(path: PathBuf, server: &[u8], user_id: UserId, marker: u8) -> LocalE2eIdentity {
        let authority_seed = [marker; ACCOUNT_AUTHORITY_KEY_LEN];
        let signing_seed = [marker.wrapping_add(1); DEVICE_SIGNING_KEY_LEN];
        let encryption_seed = [marker.wrapping_add(2); E2E_SEED_LEN];
        let device_id = DeviceId([marker; 16]);
        let authority_public = ed25519_public_key(&authority_seed).unwrap();
        let account_id = account_id(server, user_id, &authority_public);
        let genesis = sign_account_statement(
            AccountKeyStatementBody {
                account_id,
                account_generation: 1,
                roster_epoch: 1,
                previous: LedgerHash::default(),
                authority_key_epoch: 1,
                action: AccountKeyAction::Genesis {
                    authority_public_key: authority_public.to_vec(),
                    first_device: DevicePublicKeys {
                        device_id,
                        name: format!("device-{marker}"),
                        key_epoch: 1,
                        signing_public_key: ed25519_public_key(&signing_seed).unwrap().to_vec(),
                        encryption_public_key: e2e_public_key(&encryption_seed).to_vec(),
                    },
                },
            },
            &authority_seed,
            None,
        )
        .unwrap();
        LocalE2eIdentity {
            path,
            file: IdentityFile {
                version: STORE_VERSION,
                server_public_key: server.to_vec(),
                user_id,
                account_id,
                authority_seed: authority_seed.to_vec(),
                device_id,
                device_name: format!("device-{marker}"),
                device_key_epoch: 1,
                device_signing_seed: signing_seed.to_vec(),
                device_encryption_seed: encryption_seed.to_vec(),
                own_ledger: vec![genesis],
                server_registered: false,
                peers: Vec::new(),
                sender_chains: Vec::new(),
                replay_heads: Vec::new(),
                seen_events: Vec::new(),
                verification_snapshot: VerificationSyncSnapshot::default(),
                verification_checkpoint: None,
                verification_pending: Vec::new(),
                verification_notices: Vec::new(),
                verification_republish: false,
                staged_link_secret_hash: Vec::new(),
                staged_bearer_token: String::new(),
            },
        }
    }

    #[test]
    fn verification_changes_coalesce_and_remote_verification_raises_one_notice() {
        let temp = tempfile::tempdir().unwrap();
        let server = [0x33; 32];
        let mut source = identity(
            temp.path().join("source.bin"),
            &server,
            UserId(1),
            10,
        );
        let peer = UserId(2);
        let peer_account = AccountId([8; 32]);
        source
            .set_identity_verified(peer, peer_account, true)
            .unwrap();
        source
            .set_identity_verified(peer, peer_account, false)
            .unwrap();
        source
            .set_identity_verified(peer, peer_account, true)
            .unwrap();
        assert_eq!(source.file.verification_pending.len(), 1);
        let snapshot = source.next_verification_snapshot().unwrap();
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].state, VerificationSyncState::Verified);

        let mut receiver = identity(
            temp.path().join("receiver.bin"),
            &server,
            UserId(1),
            20,
        );
        let checkpoint = VerificationSyncCheckpoint {
            version: 1,
            digest: VerificationSyncHash([4; 32]),
        };
        assert_eq!(
            receiver
                .apply_verification_snapshot(checkpoint, snapshot.clone())
                .unwrap(),
            vec![(peer, peer_account)]
        );
        assert!(receiver.identity_verified(peer, peer_account));
        assert!(receiver.verification_notice_pending(peer, peer_account));
        receiver
            .acknowledge_verification_notice(peer, peer_account)
            .unwrap();
        assert!(!receiver.verification_notice_pending(peer, peer_account));

        let mut forgotten = snapshot;
        forgotten.records[0].state = VerificationSyncState::Unverified;
        forgotten.records[0].modified_at = 2;
        receiver
            .apply_verification_snapshot(
                VerificationSyncCheckpoint {
                    version: 2,
                    digest: VerificationSyncHash([5; 32]),
                },
                forgotten,
            )
            .unwrap();
        assert!(!receiver.identity_verified(peer, peer_account));
        assert!(!receiver.verification_notice_pending(peer, peer_account));
    }

    #[test]
    fn device_enrollment_bootstraps_effective_verifications_as_pending_and_noticed() {
        let temp = tempfile::tempdir().unwrap();
        let server = [0x44; 32];
        let user_id = UserId(3);
        let mut source = identity(
            temp.path().join("source-enrollment.bin"),
            &server,
            user_id,
            30,
        );
        let peer = UserId(4);
        let peer_account = AccountId([9; 32]);
        source
            .set_identity_verified(peer, peer_account, true)
            .unwrap();
        let enrollment = source.enrollment_authority().unwrap();
        assert!(enrollment.len() < 16 * 1024);

        let (linked, _) = LocalE2eIdentity::prepare_linked_device(
            temp.path(),
            &server,
            user_id,
            &[7; 32],
            "linked-device",
            source.own_statements(),
            &enrollment,
            false,
            &ring::rand::SystemRandom::new(),
        )
        .unwrap();
        assert!(linked.identity_verified(peer, peer_account));
        assert!(linked.verification_dirty());
        assert!(linked.verification_notice_pending(peer, peer_account));
    }

    #[test]
    fn replay_and_predecessor_checks_survive_restart() {
        let temp = tempfile::tempdir().unwrap();
        let server = [0x55; 32];
        let user_id = UserId(4);
        let path = temp.path().join("identity.bin");
        let mut identity = identity(path.clone(), &server, user_id, 7);
        let first = EventId([1; 16]);
        assert_eq!(
            identity
                .observe_event(
                    user_id,
                    identity.device_id(),
                    1,
                    RoomId(3),
                    1,
                    None,
                    first,
                    b"first envelope",
                    true,
                )
                .unwrap(),
            ReplayObservation::Fresh
        );
        identity.mark_event_applied(first).unwrap();
        drop(identity);

        let file: IdentityFile = jsony::from_binary(&fs::read(&path).unwrap()).unwrap();
        let mut identity = LocalE2eIdentity { path, file };
        identity.validate(&server, user_id).unwrap();
        assert_eq!(
            identity
                .observe_event(
                    user_id,
                    identity.device_id(),
                    1,
                    RoomId(3),
                    1,
                    None,
                    first,
                    b"first envelope",
                    true,
                )
                .unwrap(),
            ReplayObservation::Duplicate
        );
        assert_eq!(
            identity
                .observe_event(
                    user_id,
                    identity.device_id(),
                    1,
                    RoomId(3),
                    2,
                    Some(EventId([9; 16])),
                    EventId([2; 16]),
                    b"forked successor",
                    true,
                )
                .unwrap(),
            ReplayObservation::Fork
        );
    }
}
