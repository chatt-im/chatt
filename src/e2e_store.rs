//! Owner-only local account/device identity and replay state.
//!
//! Large and security-sensitive state deliberately lives outside `chatt.toml`:
//! device private keys, signed account ledgers, sender sequence reservations,
//! peer checkpoints, and the durable event replay journal share one atomic file
//! keyed by server identity and authenticated user id.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use jsony::Jsony;
use ring::{digest, rand::SecureRandom};
use rpc::{
    crypto::{KEY_LEN, KeyMaterial, open_in_place_with_aad, seal_in_place_append_tag},
    e2e::{
        ACCOUNT_AUTHORITY_KEY_LEN, AccountKeyAction, AccountKeyStatement,
        AccountKeyStatementBody, DevicePublicKeys, E2E_SEED_LEN, DEVICE_SIGNING_KEY_LEN,
        ValidatedAccountLedger, account_id, ed25519_public_key, e2e_public_key,
        random_device_id, sign_account_statement,
    },
    ids::{AccountId, DeviceId, EventId, LedgerHash, RoomId, UserId},
};

const STORE_VERSION: u16 = 1;
const RECOVERY_AAD_LABEL: &[u8] = b"chatt account recovery bundle v1";

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
    recovery_secret: Vec<u8>,
    recovery_bundle: Vec<u8>,
    device_id: DeviceId,
    device_key_epoch: u64,
    device_signing_seed: Vec<u8>,
    device_encryption_seed: Vec<u8>,
    own_ledger: Vec<AccountKeyStatement>,
    server_registered: bool,
    peers: Vec<StoredPeerLedger>,
    sender_chains: Vec<StoredSenderChain>,
    replay_heads: Vec<StoredReplayHead>,
    seen_events: Vec<StoredSeenEvent>,
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
    pub(crate) fn load_or_create(
        server_public_key: &[u8],
        user_id: UserId,
        rng: &dyn SecureRandom,
    ) -> Result<(Self, bool), String> {
        let path = identity_path(server_public_key, user_id)?;
        match fs::read(&path) {
            Ok(bytes) => {
                let file: IdentityFile = jsony::from_binary(&bytes)
                    .map_err(|error| format!("failed to decode {}: {error}", path.display()))?;
                let identity = Self { path, file };
                identity.validate(server_public_key, user_id)?;
                Ok((identity, false))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut authority_seed = [0u8; ACCOUNT_AUTHORITY_KEY_LEN];
                let mut signing_seed = [0u8; DEVICE_SIGNING_KEY_LEN];
                let mut encryption_seed = [0u8; E2E_SEED_LEN];
                let mut recovery_secret = [0u8; KEY_LEN];
                rng.fill(&mut authority_seed)
                    .map_err(|_| "failed to generate account authority".to_string())?;
                rng.fill(&mut signing_seed)
                    .map_err(|_| "failed to generate device signing key".to_string())?;
                rng.fill(&mut encryption_seed)
                    .map_err(|_| "failed to generate device encryption key".to_string())?;
                rng.fill(&mut recovery_secret)
                    .map_err(|_| "failed to generate recovery secret".to_string())?;
                let device_id = random_device_id(rng).map_err(|error| error.to_string())?;
                let authority_public = ed25519_public_key(&authority_seed)?;
                let account_id = account_id(server_public_key, user_id, &authority_public);
                let recovery_bundle = seal_recovery_bundle(
                    server_public_key,
                    user_id,
                    account_id,
                    &authority_seed,
                    &recovery_secret,
                )?;
                let recovery_hash = digest::digest(&digest::SHA256, &recovery_bundle)
                    .as_ref()
                    .to_vec();
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
                                key_epoch: 1,
                                signing_public_key: ed25519_public_key(&signing_seed)?.to_vec(),
                                encryption_public_key: e2e_public_key(&encryption_seed).to_vec(),
                            },
                            recovery_bundle_hash: recovery_hash,
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
                        recovery_secret: recovery_secret.to_vec(),
                        recovery_bundle,
                        device_id,
                        device_key_epoch: 1,
                        device_signing_seed: signing_seed.to_vec(),
                        device_encryption_seed: encryption_seed.to_vec(),
                        own_ledger: vec![genesis],
                        server_registered: false,
                        peers: Vec::new(),
                        sender_chains: Vec::new(),
                        replay_heads: Vec::new(),
                        seen_events: Vec::new(),
                    },
                };
                identity.persist()?;
                Ok((identity, true))
            }
            Err(error) => Err(format!("failed to read {}: {error}", path.display())),
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

    pub(crate) fn recovery_bundle(&self) -> &[u8] {
        &self.file.recovery_bundle
    }

    pub(crate) fn recovery_code(&self) -> String {
        rpc::crypto::encode_hex(&self.file.recovery_secret)
    }

    pub(crate) fn device_public_keys(&self) -> Result<DevicePublicKeys, String> {
        Ok(DevicePublicKeys {
            device_id: self.file.device_id,
            key_epoch: self.file.device_key_epoch,
            signing_public_key: ed25519_public_key(self.signing_seed())?.to_vec(),
            encryption_public_key: e2e_public_key(self.encryption_seed()).to_vec(),
        })
    }

    pub(crate) fn sign_next_account_action(
        &self,
        action: AccountKeyAction,
    ) -> Result<AccountKeyStatement, String> {
        let ledger = self.own_ledger();
        let authority_seed = fixed::<ACCOUNT_AUTHORITY_KEY_LEN>(
            &self.file.authority_seed,
            "account authority seed",
        )?;
        sign_account_statement(
            AccountKeyStatementBody {
                account_id: ledger.account_id,
                account_generation: ledger.account_generation,
                roster_epoch: ledger.roster_epoch.saturating_add(1),
                previous: ledger.head,
                authority_key_epoch: ledger.authority_key_epoch,
                action,
            },
            &authority_seed,
            None,
        )
    }

    pub(crate) fn recover_authority(
        &self,
        ledger: &ValidatedAccountLedger,
        bundle: &[u8],
        recovery_code: &str,
    ) -> Result<([u8; ACCOUNT_AUTHORITY_KEY_LEN], [u8; KEY_LEN]), String> {
        let decoded = rpc::crypto::decode_hex(recovery_code)
            .map_err(|_| "E2E recovery code is not valid hex".to_string())?;
        let recovery_secret = fixed::<KEY_LEN>(&decoded, "E2E recovery code")?;
        let authority_seed = open_recovery_bundle(
            &self.file.server_public_key,
            self.file.user_id,
            ledger.account_id,
            bundle,
            &recovery_secret,
        )?;
        if ed25519_public_key(&authority_seed)? != ledger.authority_public_key {
            return Err("recovery bundle does not contain the current account authority".to_string());
        }
        let bundle_hash = digest::digest(&digest::SHA256, bundle);
        if ledger.recovery_bundle_hash.as_slice() != bundle_hash.as_ref() {
            return Err("recovery bundle does not match the signed account ledger".to_string());
        }
        Ok((authority_seed, recovery_secret))
    }

    pub(crate) fn adopt_linked_account(
        &mut self,
        statements: Vec<AccountKeyStatement>,
        authority_seed: [u8; ACCOUNT_AUTHORITY_KEY_LEN],
        recovery_secret: [u8; KEY_LEN],
        recovery_bundle: Vec<u8>,
    ) -> Result<ValidatedAccountLedger, String> {
        let ledger = ValidatedAccountLedger::validate(
            &self.file.server_public_key,
            self.file.user_id,
            &statements,
        )?;
        if ed25519_public_key(&authority_seed)? != ledger.authority_public_key {
            return Err("linked authority does not match the account ledger".to_string());
        }
        let device = ledger
            .device_key(self.file.device_id, self.file.device_key_epoch)
            .filter(|device| device.status == rpc::e2e::DeviceKeyStatus::Active)
            .ok_or_else(|| "linked account ledger does not authorize this device".to_string())?;
        let local = self.device_public_keys()?;
        if device.keys != local {
            return Err("linked account authorized different device key material".to_string());
        }
        let bundle_hash = digest::digest(&digest::SHA256, &recovery_bundle);
        if ledger.recovery_bundle_hash.as_slice() != bundle_hash.as_ref() {
            return Err("linked recovery bundle does not match the account ledger".to_string());
        }
        self.file.account_id = ledger.account_id;
        self.file.authority_seed = authority_seed.to_vec();
        self.file.recovery_secret = recovery_secret.to_vec();
        self.file.recovery_bundle = recovery_bundle;
        self.file.own_ledger = statements;
        self.file.server_registered = true;
        self.persist()?;
        Ok(ledger)
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
        self.file.own_ledger = statements;
        self.file.server_registered = true;
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
        fixed::<KEY_LEN>(&self.file.recovery_secret, "recovery secret")?;
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
        Ok(())
    }

    fn persist(&self) -> Result<(), String> {
        let bytes = jsony::to_binary(&self.file);
        atomic_write_private(&self.path, &bytes)
    }
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

fn seal_recovery_bundle(
    server_public_key: &[u8],
    user_id: UserId,
    account_id: AccountId,
    authority_seed: &[u8; ACCOUNT_AUTHORITY_KEY_LEN],
    recovery_secret: &[u8; KEY_LEN],
) -> Result<Vec<u8>, String> {
    let aad = recovery_aad(server_public_key, user_id, account_id);
    let key = KeyMaterial {
        id: 1,
        bytes: *recovery_secret,
    };
    let mut sealed = authority_seed.to_vec();
    seal_in_place_append_tag(&key, 0, &aad, 0, &mut sealed)
        .map_err(|error| error.to_string())?;
    Ok(sealed)
}

fn open_recovery_bundle(
    server_public_key: &[u8],
    user_id: UserId,
    account_id: AccountId,
    bundle: &[u8],
    recovery_secret: &[u8; KEY_LEN],
) -> Result<[u8; ACCOUNT_AUTHORITY_KEY_LEN], String> {
    let aad = recovery_aad(server_public_key, user_id, account_id);
    let key = KeyMaterial {
        id: 1,
        bytes: *recovery_secret,
    };
    let mut sealed = bundle.to_vec();
    let len = open_in_place_with_aad(&key, 0, &aad, &mut sealed)
        .map_err(|error| error.to_string())?;
    sealed[..len]
        .try_into()
        .map_err(|_| "recovery bundle plaintext has the wrong length".to_string())
}

fn recovery_aad(server_public_key: &[u8], user_id: UserId, account_id: AccountId) -> Vec<u8> {
    let mut aad = Vec::with_capacity(RECOVERY_AAD_LABEL.len() + server_public_key.len() + 40);
    aad.extend_from_slice(RECOVERY_AAD_LABEL);
    aad.extend_from_slice(server_public_key);
    aad.extend_from_slice(&user_id.0.to_le_bytes());
    aad.extend_from_slice(&account_id.0);
    aad
}

fn fixed<const N: usize>(bytes: &[u8], name: &str) -> Result<[u8; N], String> {
    bytes
        .try_into()
        .map_err(|_| format!("{name} has the wrong length"))
}

fn identity_path(server_public_key: &[u8], user_id: UserId) -> Result<PathBuf, String> {
    let base = crate::paths::client_data_dir()
        .ok_or_else(|| "HOME is not set; cannot store the E2E device identity".to_string())?;
    let mut context = Vec::with_capacity(server_public_key.len() + 8);
    context.extend_from_slice(server_public_key);
    context.extend_from_slice(&user_id.0.to_le_bytes());
    let name = rpc::crypto::encode_hex(digest::digest(&digest::SHA256, &context).as_ref());
    Ok(base.join("e2e").join(format!("{name}.bin")))
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
    use rpc::e2e::{DeviceKeyStatus, account_statement_hash};

    fn identity(path: PathBuf, server: &[u8], user_id: UserId, marker: u8) -> LocalE2eIdentity {
        let authority_seed = [marker; ACCOUNT_AUTHORITY_KEY_LEN];
        let signing_seed = [marker.wrapping_add(1); DEVICE_SIGNING_KEY_LEN];
        let encryption_seed = [marker.wrapping_add(2); E2E_SEED_LEN];
        let recovery_secret = [marker.wrapping_add(3); KEY_LEN];
        let device_id = DeviceId([marker; 16]);
        let authority_public = ed25519_public_key(&authority_seed).unwrap();
        let account_id = account_id(server, user_id, &authority_public);
        let recovery_bundle = seal_recovery_bundle(
            server,
            user_id,
            account_id,
            &authority_seed,
            &recovery_secret,
        )
        .unwrap();
        let recovery_hash = digest::digest(&digest::SHA256, &recovery_bundle)
            .as_ref()
            .to_vec();
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
                        key_epoch: 1,
                        signing_public_key: ed25519_public_key(&signing_seed).unwrap().to_vec(),
                        encryption_public_key: e2e_public_key(&encryption_seed).to_vec(),
                    },
                    recovery_bundle_hash: recovery_hash,
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
                recovery_secret: recovery_secret.to_vec(),
                recovery_bundle,
                device_id,
                device_key_epoch: 1,
                device_signing_seed: signing_seed.to_vec(),
                device_encryption_seed: encryption_seed.to_vec(),
                own_ledger: vec![genesis],
                server_registered: false,
                peers: Vec::new(),
                sender_chains: Vec::new(),
                replay_heads: Vec::new(),
                seen_events: Vec::new(),
            },
        }
    }

    #[test]
    fn recovery_enrolls_independent_device_and_revocation_is_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let server = [0x77; 32];
        let user_id = UserId(9);
        let mut primary = identity(temp.path().join("primary.bin"), &server, user_id, 11);
        let mut secondary = identity(temp.path().join("secondary.bin"), &server, user_id, 41);
        let primary_ledger = primary.own_ledger();
        let bundle = primary.recovery_bundle().to_vec();
        let (authority_seed, recovery_secret) = secondary
            .recover_authority(&primary_ledger, &bundle, &primary.recovery_code())
            .unwrap();
        let add = sign_account_statement(
            AccountKeyStatementBody {
                account_id: primary_ledger.account_id,
                account_generation: primary_ledger.account_generation,
                roster_epoch: 2,
                previous: primary_ledger.head,
                authority_key_epoch: primary_ledger.authority_key_epoch,
                action: AccountKeyAction::AddDevice {
                    device: secondary.device_public_keys().unwrap(),
                },
            },
            &authority_seed,
            None,
        )
        .unwrap();
        let mut linked_chain = primary.own_statements().to_vec();
        linked_chain.push(add);
        let linked = secondary
            .adopt_linked_account(
                linked_chain.clone(),
                authority_seed,
                recovery_secret,
                bundle,
            )
            .unwrap();
        assert_eq!(linked.active_devices().count(), 2);
        assert_eq!(secondary.account_id(), primary.account_id());

        primary.replace_own_ledger(linked_chain.clone()).unwrap();
        let revoke = primary
            .sign_next_account_action(AccountKeyAction::RevokeDevice {
                device_id: secondary.device_id(),
            })
            .unwrap();
        assert_eq!(revoke.body.previous, account_statement_hash(linked_chain.last().unwrap()));
        linked_chain.push(revoke);
        let revoked = ValidatedAccountLedger::validate(&server, user_id, &linked_chain).unwrap();
        assert_eq!(
            revoked
                .device_key(secondary.device_id(), secondary.device_key_epoch())
                .unwrap()
                .status,
            DeviceKeyStatus::Revoked
        );
        assert!(secondary.replace_own_ledger(linked_chain).is_err());
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
