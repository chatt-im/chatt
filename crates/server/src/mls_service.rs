use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use chatt_mls::{ChattIdentityProvider, PublicGroupState, PublicGroupValidator};
use rpc::{
    identity::{
        RosterCheckpoint, SignedDeviceRoster, roster_checkpoint, validate_roster_transition,
    },
    ids::{AccountId, DeviceId, EventId, RoomId, UserId},
    mls::{
        EncryptedRoomDescriptor, MlsCommitBundle, MlsCommitOutcome, MlsDeliveryEvent,
        MlsSubmitOutcome, MlsWelcome, PublishedKeyPackage,
        validate_commit_bundle, validate_key_packages,
    },
};

use crate::mls_store::{
    AppendApplicationResult, DeviceCredential, GlobalRecord, MlsStore, RoomRecord,
};

const MAX_KEY_PACKAGES_PER_DEVICE: usize = 64;

/// MLS validation and authorization caches. [`MlsStore`] is the durable
/// authority; delivery history is never retained in this long-lived object.
#[derive(Debug)]
pub(super) struct MlsService {
    server_id: Vec<u8>,
    store: MlsStore,
    default_retention_days: u16,
    room_retention_days: HashMap<RoomId, u16>,
    initialized_accounts: HashSet<UserId>,
    identities: ChattIdentityProvider,
    validator: PublicGroupValidator,
    rosters: HashMap<UserId, SignedDeviceRoster>,
    key_packages: HashMap<DeviceId, VecDeque<Vec<u8>>>,
    retired_key_packages: HashSet<[u8; 32]>,
    issued_key_packages: HashSet<Vec<u8>>,
    used_key_packages: HashSet<Vec<u8>>,
    rooms: HashMap<RoomId, RoomRecord>,
    next_welcome_delivery_id: u64,
    credentials: Vec<DeviceCredential>,
    /// Device ids route KeyPackages and Welcome inboxes globally, so they can
    /// never be reassigned to another account, even after revocation.
    device_owners: HashMap<DeviceId, UserId>,
}

#[derive(Clone, Debug)]
struct CacheState {
    global: GlobalRecord,
    rooms: Vec<RoomRecord>,
}

impl MlsService {
    #[allow(dead_code)]
    pub fn new(server_id: Vec<u8>) -> Self {
        Self::open(None, server_id).expect("in-memory MLS store creation must succeed")
    }

    #[allow(dead_code)]
    pub fn open(data_dir: Option<PathBuf>, server_id: Vec<u8>) -> Result<Self, String> {
        Self::open_with_retention(data_dir, server_id, 90, HashMap::new())
    }

    pub fn open_with_retention(
        data_dir: Option<PathBuf>,
        server_id: Vec<u8>,
        default_retention_days: u16,
        room_retention_days: HashMap<RoomId, u16>,
    ) -> Result<Self, String> {
        let identities = ChattIdentityProvider::new(server_id.clone());
        let validator = PublicGroupValidator::new(identities.clone());
        let store = MlsStore::open(data_dir.as_deref())?;
        let loaded = store.load()?;
        let mut service = Self {
            server_id,
            store,
            default_retention_days,
            room_retention_days,
            initialized_accounts: HashSet::new(),
            identities,
            validator,
            rosters: HashMap::new(),
            key_packages: HashMap::new(),
            retired_key_packages: HashSet::new(),
            issued_key_packages: HashSet::new(),
            used_key_packages: HashSet::new(),
            rooms: HashMap::new(),
            next_welcome_delivery_id: 1,
            credentials: Vec::new(),
            device_owners: HashMap::new(),
        };
        service.restore(CacheState {
            global: loaded.global,
            rooms: loaded.rooms,
        })?;
        Ok(service)
    }

    pub fn roster(&self, user_id: UserId) -> Option<&SignedDeviceRoster> {
        self.rosters.get(&user_id)
    }

    pub fn initialized(&self, user_id: UserId) -> bool {
        self.initialized_accounts.contains(&user_id)
    }

    pub fn user_for_account(&self, account_id: AccountId) -> Option<UserId> {
        self.rosters.iter().find_map(|(user_id, roster)| {
            (roster.body.account_id == account_id).then_some(*user_id)
        })
    }

    pub fn authenticate_credential(&self, token: &str) -> Option<(UserId, DeviceId, String)> {
        self.credentials.iter().find_map(|credential| {
            crate::config::verify_secret_hash(&credential.token_hash, token).then(|| {
                (
                    credential.user_id,
                    credential.device_id,
                    credential.token_hash.clone(),
                )
            })
        })
    }

    pub fn redeem_pair(
        &mut self,
        user_id: UserId,
        expected: RosterCheckpoint,
        roster: SignedDeviceRoster,
        device_id: DeviceId,
        token_hash: String,
        packages: Vec<PublishedKeyPackage>,
    ) -> Result<RosterCheckpoint, String> {
        let current = self
            .rosters
            .get(&user_id)
            .ok_or_else(|| "device-link account has no MLS roster".to_string())?;
        validate_roster_transition(
            Some(current),
            &roster,
            Some(expected),
            &self.server_id,
            user_id,
        )?;
        let certificate = roster
            .body
            .active_devices
            .iter()
            .find(|certificate| certificate.body.device_id == device_id)
            .ok_or_else(|| "paired roster does not contain the new MLS device".to_string())?;
        if current
            .body
            .active_devices
            .iter()
            .any(|old| old.body.device_id == device_id)
        {
            return Err("paired roster does not add a fresh MLS device".to_string());
        }
        if self
            .device_owners
            .get(&device_id)
            .is_some_and(|owner| *owner != user_id)
        {
            return Err("MLS device id is already owned by another account".to_string());
        }
        validate_key_packages(&packages)?;
        self.validate_fresh_key_packages(&roster, &packages)?;
        if packages.is_empty()
            || packages
                .iter()
                .any(|package| package.device_id != device_id)
        {
            return Err("paired KeyPackages do not belong to the new device".to_string());
        }
        if certificate.body.mls_client_id.is_empty()
            || self
                .credentials
                .iter()
                .any(|credential| credential.token_hash == token_hash)
        {
            return Err("paired device credential is already registered".to_string());
        }
        let before = self.snapshot();
        self.device_owners.insert(device_id, user_id);
        self.identities
            .install_roster(&roster)
            .map_err(|error| error.to_string())?;
        let checkpoint = roster_checkpoint(&roster);
        self.rosters.insert(user_id, roster);
        self.key_packages.insert(
            device_id,
            packages
                .into_iter()
                .map(|package| package.package)
                .collect(),
        );
        self.credentials.push(DeviceCredential {
            user_id,
            device_id,
            token_hash,
        });
        if let Err(error) = self.persist() {
            return Err(self.rollback(before, error));
        }
        Ok(checkpoint)
    }

    pub fn put_roster(
        &mut self,
        user_id: UserId,
        expected: Option<RosterCheckpoint>,
        roster: SignedDeviceRoster,
        bootstrap_credential_hash: Option<String>,
    ) -> Result<RosterCheckpoint, PutRosterError> {
        let current = self.rosters.get(&user_id).cloned();
        let initial = current.is_none();
        if expected != current.as_ref().map(roster_checkpoint) {
            return Err(PutRosterError::Conflict(
                current.as_ref().map(roster_checkpoint),
            ));
        }
        validate_roster_transition(
            current.as_ref(),
            &roster,
            expected,
            &self.server_id,
            user_id,
        )
        .map_err(PutRosterError::Invalid)?;
        for certificate in &roster.body.active_devices {
            if self
                .device_owners
                .get(&certificate.body.device_id)
                .is_some_and(|owner| *owner != user_id)
            {
                return Err(PutRosterError::Invalid(
                    "MLS device id is already owned by another account".to_string(),
                ));
            }
        }
        if initial && let Some(token_hash) = bootstrap_credential_hash.as_ref() {
            let [_] = roster.body.active_devices.as_slice() else {
                return Err(PutRosterError::Invalid(
                    "initial MLS roster must contain exactly one device".to_string(),
                ));
            };
            if self
                .credentials
                .iter()
                .any(|credential| credential.token_hash == *token_hash)
            {
                return Err(PutRosterError::Invalid(
                    "initial MLS device credential is already registered".to_string(),
                ));
            }
        }
        let before = self.snapshot();
        if initial {
            self.mark_initialized(user_id)
                .map_err(PutRosterError::Invalid)?;
        }
        for certificate in &roster.body.active_devices {
            self.device_owners
                .insert(certificate.body.device_id, user_id);
        }
        if initial && let Some(token_hash) = bootstrap_credential_hash {
            let [certificate] = roster.body.active_devices.as_slice() else {
                unreachable!("initial credential shape validated above")
            };
            self.credentials.push(DeviceCredential {
                user_id,
                device_id: certificate.body.device_id,
                token_hash,
            });
        }
        let removed: Vec<Vec<u8>> = current
            .iter()
            .flat_map(|roster| &roster.body.active_devices)
            .filter(|old| {
                !roster.body.active_devices.iter().any(|new| {
                    new.body.device_id == old.body.device_id
                        && new.body.mls_client_id == old.body.mls_client_id
                })
            })
            .map(|certificate| certificate.body.mls_client_id.clone())
            .collect();
        let removed_devices = current
            .iter()
            .flat_map(|roster| &roster.body.active_devices)
            .filter(|certificate| removed.contains(&certificate.body.mls_client_id))
            .map(|certificate| certificate.body.device_id)
            .collect::<HashSet<_>>();
        if let Err(error) = self.identities.install_roster(&roster) {
            return Err(PutRosterError::Invalid(
                self.rollback(before, error.to_string()),
            ));
        }
        for certificate in current
            .iter()
            .flat_map(|roster| &roster.body.active_devices)
            .filter(|certificate| removed.contains(&certificate.body.mls_client_id))
        {
            self.key_packages.remove(&certificate.body.device_id);
            self.credentials.retain(|credential| {
                credential.device_id != certificate.body.device_id || credential.user_id != user_id
            });
        }
        if !removed.is_empty() {
            for room in self.rooms.values_mut() {
                if room
                    .public_state
                    .member_client_ids
                    .iter()
                    .any(|client_id| removed.contains(client_id))
                {
                    room.revocation_pending = true;
                    room.required_devices
                        .retain(|device| !removed_devices.contains(device));
                }
            }
        }
        let checkpoint = roster_checkpoint(&roster);
        self.rosters.insert(user_id, roster);
        if let Err(error) = self.persist() {
            return Err(PutRosterError::Invalid(self.rollback(before, error)));
        }
        Ok(checkpoint)
    }

    pub fn publish_key_packages(
        &mut self,
        user_id: UserId,
        device_id: DeviceId,
        packages: Vec<PublishedKeyPackage>,
    ) -> Result<u16, String> {
        validate_key_packages(&packages)?;
        let roster = self
            .rosters
            .get(&user_id)
            .ok_or_else(|| "account has no MLS device roster".to_string())?;
        let certificate = roster
            .body
            .active_devices
            .iter()
            .find(|certificate| certificate.body.device_id == device_id)
            .ok_or_else(|| "KeyPackages do not belong to an active session device".to_string())?;
        if packages
                .iter()
                .any(|package| package.device_id != device_id)
        {
            return Err("KeyPackages do not belong to an active session device".to_string());
        }
        let before = self.snapshot();
        let mut batch_hashes = HashSet::new();
        let mut new_packages = Vec::new();
        for package in packages {
            let (_, client_id) = self
                .validator
                .validate_key_package(&package.package)
                .map_err(|error| error.to_string())?;
            if client_id != certificate.body.mls_client_id {
                return Err("KeyPackage credential does not match its device".to_string());
            }
            let hash = key_package_hash(&package.package);
            if self.retired_key_packages.contains(&hash) {
                return Err("KeyPackage was already consumed".to_string());
            }
            if !batch_hashes.insert(hash) {
                return Err("KeyPackage batch contains a duplicate".to_string());
            }
            match self.key_packages.iter().find_map(|(queued_device, queued)| {
                queued
                    .iter()
                    .any(|queued| queued == &package.package)
                    .then_some(*queued_device)
            }) {
                Some(queued_device) if queued_device == device_id => {}
                Some(_) => return Err("KeyPackage is already published by another device".to_string()),
                None => new_packages.push(package.package),
            }
        }
        let current_len = self.key_packages.get(&device_id).map_or(0, VecDeque::len);
        if current_len + new_packages.len() > MAX_KEY_PACKAGES_PER_DEVICE {
            return Err("device KeyPackage pool is full".to_string());
        }
        let pool = self.key_packages.entry(device_id).or_default();
        for package in new_packages {
            pool.push_back(package);
        }
        let available = pool.len() as u16;
        if let Err(error) = self.persist() {
            return Err(self.rollback(before, error));
        }
        Ok(available)
    }

    pub fn take_key_package(&mut self, device_id: DeviceId) -> Result<Option<Vec<u8>>, String> {
        let before = self.snapshot();
        let package = self
            .key_packages
            .get_mut(&device_id)
            .and_then(VecDeque::pop_front);
        if let Some(package) = &package {
            self.retired_key_packages.insert(key_package_hash(package));
            let reference = match self.validator.key_package_reference(package) {
                Ok(reference) => reference,
                Err(error) => return Err(self.rollback(before, error.to_string())),
            };
            self.issued_key_packages.insert(reference);
        }
        if package.is_some()
            && let Err(error) = self.persist()
        {
            return Err(self.rollback(before, error));
        }
        Ok(package)
    }

    pub fn key_package_count(&self, device_id: DeviceId) -> u16 {
        self.key_packages
            .get(&device_id)
            .map_or(0, |packages| packages.len() as u16)
    }

    fn validate_fresh_key_packages(
        &self,
        roster: &SignedDeviceRoster,
        packages: &[PublishedKeyPackage],
    ) -> Result<(), String> {
        let identities = ChattIdentityProvider::new(self.server_id.clone());
        for current in self.rosters.values() {
            identities
                .install_roster(current)
                .map_err(|error| error.to_string())?;
        }
        identities
            .install_roster(roster)
            .map_err(|error| error.to_string())?;
        let validator = PublicGroupValidator::new(identities);
        let mut hashes = HashSet::new();
        for package in packages {
            let certificate = roster
                .body
                .active_devices
                .iter()
                .find(|certificate| certificate.body.device_id == package.device_id)
                .ok_or_else(|| "KeyPackage does not name a device in the roster".to_string())?;
            let (_, client_id) = validator
                .validate_key_package(&package.package)
                .map_err(|error| error.to_string())?;
            if client_id != certificate.body.mls_client_id {
                return Err("KeyPackage credential does not match its device".to_string());
            }
            let hash = key_package_hash(&package.package);
            if self.retired_key_packages.contains(&hash) {
                return Err("KeyPackage was already consumed".to_string());
            }
            if self
                .key_packages
                .values()
                .any(|queued| queued.iter().any(|queued| queued == &package.package))
            {
                return Err("KeyPackage is already published".to_string());
            }
            if !hashes.insert(hash) {
                return Err("KeyPackage batch contains a duplicate".to_string());
            }
        }
        Ok(())
    }

    pub fn create_room(
        &mut self,
        creator: AccountId,
        creator_client_id: &[u8],
        descriptor: EncryptedRoomDescriptor,
        checkpoints: &[RosterCheckpoint],
        bundle: MlsCommitBundle,
    ) -> Result<u64, String> {
        descriptor.validate()?;
        validate_commit_bundle(&bundle)?;
        if bundle
            .welcome
            .as_ref()
            .is_some_and(|welcome| welcome.descriptor != descriptor)
        {
            return Err("initial Welcome room descriptor does not match".to_string());
        }
        if descriptor.creator != creator || self.rooms.contains_key(&descriptor.room_id) {
            return Err("encrypted room creator or room id is invalid".to_string());
        }
        if checkpoints.len() != descriptor.member_accounts.len() {
            return Err("encrypted room roster checkpoints do not match".to_string());
        }
        for account in &descriptor.member_accounts {
            let roster = self
                .rosters
                .values()
                .find(|roster| roster.body.account_id == *account)
                .ok_or_else(|| "encrypted room account has no current roster".to_string())?;
            if !checkpoints.contains(&roster_checkpoint(roster)) {
                return Err("encrypted room roster checkpoint is stale".to_string());
            }
        }
        // Initial-commit validation needs the descriptor installed in its
        // MLS policy. Keep that speculative state isolated: publishing it to
        // the live provider before validation would let every rejected room
        // attempt leak a caller-chosen group id into long-lived policy state.
        let validation_identities = ChattIdentityProvider::new(self.server_id.clone());
        for roster in self.rosters.values() {
            validation_identities
                .install_roster(roster)
                .map_err(|error| error.to_string())?;
        }
        validation_identities
            .install_room(descriptor.clone())
            .map_err(|error| error.to_string())?;
        let validation_validator = PublicGroupValidator::new(validation_identities.clone());
        if let Some(welcome) = &bundle.welcome {
            validation_validator
                .validate_welcome(&welcome.welcome)
                .map_err(|error| error.to_string())?;
        }
        let prior = bundle
            .prior_group_info
            .as_deref()
            .ok_or_else(|| "room creation is missing prior GroupInfo".to_string())?;
        let parent = validation_validator
            .observe_group_info(prior)
            .map_err(|error| error.to_string())?;
        if parent.epoch != 0 || parent.group_id != descriptor.mls_group_id {
            return Err("room creation GroupInfo does not match descriptor".to_string());
        }
        if parent.member_client_ids.as_slice() != [creator_client_id] {
            return Err("room creation parent must contain only the authenticated creator".to_string());
        }
        let applied = validation_validator
            .apply_commit(&parent, &bundle)
            .map_err(|error| error.to_string())?;
        Self::validate_added_key_packages(
            &self.issued_key_packages,
            &self.used_key_packages,
            &applied.added_key_package_refs,
        )?;
        let next = applied.state;
        if applied.committer_client_id != creator_client_id {
            return Err("initial commit was not signed by the authenticated device".to_string());
        }
        Self::validate_room_accounts(&validation_identities, &descriptor, &next)?;
        Self::validate_welcome_targets(
            &validation_identities,
            &parent,
            &next,
            &applied.committer_client_id,
            &bundle,
        )?;
        let sequence = 1;
        let welcome_delivery = bundle
            .welcome
            .as_ref()
            .map(|welcome| {
                let delivery_id = self.next_welcome_delivery_id;
                let next_delivery_id = self
                .next_welcome_delivery_id
                .checked_add(1)
                .ok_or_else(|| "MLS Welcome delivery id is exhausted".to_string())?;
                Ok::<_, String>((delivery_id, next_delivery_id, welcome.clone()))
            })
            .transpose()?;
        let mut global = self.snapshot().global;
        global.used_key_packages.extend(applied.added_key_package_refs);
        let welcome = welcome_delivery.as_ref().map(|(id, next, bundle)| {
            global.next_welcome_delivery_id = *next;
            (*id, bundle)
        });
        let now = unix_time_ms();
        let required_devices = Self::required_devices(&validation_identities, &next)?;
        let room = RoomRecord {
            descriptor: descriptor.clone(),
            public_state: next.clone(),
            group_info: bundle.group_info,
            revocation_pending: false,
            head_sequence: sequence,
            oldest_available_sequence: 1,
            last_event_time_unix_ms: now,
            retention_days: self
                .room_retention_days
                .get(&descriptor.room_id)
                .copied()
                .unwrap_or(self.default_retention_days),
            required_devices,
        };
        let event = MlsDeliveryEvent::Commit {
            sequence,
            parent_epoch: 0,
            epoch: next.epoch,
            commit: bundle.commit,
        };
        self.store.create_room(&global, &room, &event, welcome)?;
        self.identities
            .install_room(descriptor.clone())
            .map_err(|error| error.to_string())?;
        self.used_key_packages = global.used_key_packages.into_iter().collect();
        self.next_welcome_delivery_id = global.next_welcome_delivery_id;
        self.rooms.insert(descriptor.room_id, room);
        Ok(next.epoch)
    }

    pub fn group_info(&self, room_id: RoomId) -> Option<(&EncryptedRoomDescriptor, u64, &[u8])> {
        self.rooms.get(&room_id).map(|room| {
            (
                &room.descriptor,
                room.public_state.epoch,
                room.group_info.as_slice(),
            )
        })
    }

    #[allow(dead_code)]
    pub fn submit_commit(
        &mut self,
        room_id: RoomId,
        committer_client_id: &[u8],
        expected_epoch: u64,
        bundle: MlsCommitBundle,
    ) -> Result<MlsCommitOutcome, String> {
        self.submit_commit_for_device(
            room_id,
            DeviceId([0; 16]),
            committer_client_id,
            expected_epoch,
            bundle,
        )
    }

    pub fn submit_commit_for_device(
        &mut self,
        room_id: RoomId,
        device_id: DeviceId,
        committer_client_id: &[u8],
        expected_epoch: u64,
        bundle: MlsCommitBundle,
    ) -> Result<MlsCommitOutcome, String> {
        validate_commit_bundle(&bundle)?;
        if device_id != DeviceId([0; 16]) && !self.device_is_active(device_id) {
            return Ok(MlsCommitOutcome::PolicyRejected);
        }
        if let Some(welcome) = &bundle.welcome
            && self.validator.validate_welcome(&welcome.welcome).is_err()
        {
            return Ok(MlsCommitOutcome::PolicyRejected);
        }
        if bundle.prior_group_info.is_some() {
            return Err("non-creation commit contains prior GroupInfo".to_string());
        }
        let before = self.snapshot();
        if let Some(room) = self.rooms.get(&room_id)
            && expected_epoch != room.public_state.epoch
        {
            if let Some((sequence, epoch)) =
                self.store.find_commit(room_id, expected_epoch, &bundle.commit)?
            {
                return Ok(MlsCommitOutcome::AlreadyAccepted { sequence, epoch });
            }
            return Ok(MlsCommitOutcome::StaleEpoch {
                current_epoch: room.public_state.epoch,
            });
        }
        let previous = self
            .rooms
            .get(&room_id)
            .cloned()
            .ok_or_else(|| "encrypted room does not exist".to_string())?;
        if bundle
            .welcome
            .as_ref()
            .is_some_and(|welcome| welcome.descriptor != previous.descriptor)
        {
            return Err("Welcome room descriptor does not match".to_string());
        }
        let applied = match self.validator.apply_commit(&previous.public_state, &bundle) {
            Ok(applied) => applied,
            Err(_) => return Ok(MlsCommitOutcome::PolicyRejected),
        };
        if !applied.is_external
            && device_id != DeviceId([0; 16])
            && self.store.requires_rejoin(room_id, device_id)?
        {
            return Ok(MlsCommitOutcome::RejoinRequired);
        }
        if Self::validate_added_key_packages(
            &self.issued_key_packages,
            &self.used_key_packages,
            &applied.added_key_package_refs,
        )
        .is_err()
        {
            return Ok(MlsCommitOutcome::PolicyRejected);
        }
        if applied.committer_client_id != committer_client_id {
            return Ok(MlsCommitOutcome::PolicyRejected);
        }
        let next = applied.state;
        if Self::validate_room_accounts(&self.identities, &previous.descriptor, &next).is_err() {
            return Ok(MlsCommitOutcome::PolicyRejected);
        }
        if Self::validate_welcome_targets(
                &self.identities,
                &previous.public_state,
                &next,
                &applied.committer_client_id,
                &bundle,
            )
            .is_err()
        {
            return Ok(MlsCommitOutcome::PolicyRejected);
        }
        let revocation_pending = next
            .member_client_ids
            .iter()
            .map(|client_id| self.identities.is_client_active(client_id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .any(|active| !active);
        let sequence = previous
            .head_sequence
            .checked_add(1)
            .ok_or_else(|| "MLS delivery sequence is exhausted".to_string())?;
        let mut global = before.global.clone();
        let welcome_delivery = if let Some(welcome) = &bundle.welcome {
            let delivery_id = global.next_welcome_delivery_id;
            global.next_welcome_delivery_id = global
                .next_welcome_delivery_id
                .checked_add(1)
                .ok_or_else(|| "MLS Welcome delivery id is exhausted".to_string())?;
            Some((delivery_id, welcome))
        } else {
            None
        };
        global.used_key_packages.extend(applied.added_key_package_refs);
        let now = unix_time_ms().max(previous.last_event_time_unix_ms);
        let mut required_devices = Self::required_devices(&self.identities, &next)?;
        required_devices.sort_unstable();
        let room = RoomRecord {
            descriptor: previous.descriptor.clone(),
            public_state: next,
            group_info: bundle.group_info,
            revocation_pending,
            head_sequence: sequence,
            oldest_available_sequence: previous.oldest_available_sequence,
            last_event_time_unix_ms: now,
            retention_days: previous.retention_days,
            required_devices,
        };
        let event = MlsDeliveryEvent::Commit {
            sequence,
            parent_epoch: expected_epoch,
            epoch: room.public_state.epoch,
            commit: bundle.commit,
        };
        self.store.append_commit(
            &global,
            &previous,
            &room,
            &event,
            welcome_delivery,
        )?;
        self.used_key_packages = global.used_key_packages.into_iter().collect();
        self.next_welcome_delivery_id = global.next_welcome_delivery_id;
        self.rooms.insert(room_id, room.clone());
        kvlog::info!(
            "MLS commit durably appended",
            room_id = room_id.0,
            sequence,
            epoch = room.public_state.epoch,
            revocation_pending = room.revocation_pending,
            external = applied.is_external,
            required_devices = room.required_devices.len(),
        );
        let outcome = MlsCommitOutcome::Accepted {
            sequence,
            epoch: room.public_state.epoch,
        };
        Ok(outcome)
    }

    fn validate_welcome_targets(
        identities: &ChattIdentityProvider,
        current: &PublicGroupState,
        next: &PublicGroupState,
        committer_client_id: &[u8],
        bundle: &MlsCommitBundle,
    ) -> Result<(), String> {
        let current_members = current
            .member_client_ids
            .iter()
            .map(Vec::as_slice)
            .collect::<HashSet<_>>();
        let committer_is_external = !current_members.contains(committer_client_id);
        let mut expected = next
            .member_client_ids
            .iter()
            .filter(|client_id| {
                !current_members.contains(client_id.as_slice())
                    && !(committer_is_external
                        && client_id.as_slice() == committer_client_id)
            })
            .map(|client_id| {
                identities
                    .device_for_client(client_id)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        expected.sort_unstable();

        let mut actual = bundle
            .welcome
            .as_ref()
            .map(|welcome| welcome.device_ids.clone())
            .unwrap_or_default();
        actual.sort_unstable();
        if actual != expected {
            return Err("MLS Welcome targets do not match the added leaves".to_string());
        }
        Ok(())
    }

    fn validate_room_accounts(
        identities: &ChattIdentityProvider,
        descriptor: &EncryptedRoomDescriptor,
        state: &PublicGroupState,
    ) -> Result<(), String> {
        let unique_clients = state
            .member_client_ids
            .iter()
            .map(Vec::as_slice)
            .collect::<HashSet<_>>();
        if unique_clients.len() != state.member_client_ids.len() {
            return Err("MLS roster contains a duplicate client leaf".to_string());
        }
        let represented = state
            .member_client_ids
            .iter()
            .map(|client_id| {
                identities
                    .account_for_client(client_id)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<HashSet<_>, _>>()?;
        if represented.len() != descriptor.member_accounts.len()
            || descriptor
                .member_accounts
                .iter()
                .any(|account| !represented.contains(account))
        {
            return Err("MLS roster does not represent every room account".to_string());
        }
        Ok(())
    }

    fn validate_added_key_packages(
        issued: &HashSet<Vec<u8>>,
        used: &HashSet<Vec<u8>>,
        additions: &[Vec<u8>],
    ) -> Result<(), String> {
        let mut unique = HashSet::new();
        for reference in additions {
            if !unique.insert(reference.as_slice()) {
                return Err("MLS commit reuses a KeyPackage within one commit".to_string());
            }
            if !issued.contains(reference) {
                return Err("MLS commit adds a KeyPackage not issued by this server".to_string());
            }
            if used.contains(reference) {
                return Err("MLS commit reuses an already committed KeyPackage".to_string());
            }
        }
        Ok(())
    }

    fn required_devices(
        identities: &ChattIdentityProvider,
        state: &PublicGroupState,
    ) -> Result<Vec<DeviceId>, String> {
        let mut devices = state
            .member_client_ids
            .iter()
            .filter_map(|client_id| match identities.is_client_active(client_id) {
                Ok(true) => Some(Ok(client_id)),
                Ok(false) => None,
                Err(error) => Some(Err(error.to_string())),
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|client_id| {
                identities
                    .device_for_client(client_id)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        devices.sort_unstable();
        devices.dedup();
        Ok(devices)
    }

    #[allow(dead_code)]
    pub fn submit_application(
        &mut self,
        room_id: RoomId,
        epoch: u64,
        event_id: EventId,
        ciphertext: Vec<u8>,
    ) -> Result<MlsSubmitOutcome, String> {
        self.submit_application_for_device(
            room_id,
            DeviceId([0; 16]),
            epoch,
            event_id,
            ciphertext,
        )
    }

    pub fn submit_application_for_device(
        &mut self,
        room_id: RoomId,
        device_id: DeviceId,
        epoch: u64,
        event_id: EventId,
        ciphertext: Vec<u8>,
    ) -> Result<MlsSubmitOutcome, String> {
        if ciphertext.is_empty() || ciphertext.len() > rpc::mls::MAX_MLS_MESSAGE_BYTES {
            return Err("invalid MLS application message length".to_string());
        }
        if device_id != DeviceId([0; 16]) && !self.device_is_active(device_id) {
            return Ok(MlsSubmitOutcome::RevocationPending);
        }
        if let Some(retry) = self
            .store
            .application_retry(room_id, event_id, epoch, &ciphertext)?
        {
            return retry
                .map(|sequence| MlsSubmitOutcome::AlreadyStored { sequence })
                .map_err(|()| "MLS event id was reused for a different application".to_string());
        }
        let room = self
            .rooms
            .get(&room_id)
            .ok_or_else(|| "encrypted room does not exist".to_string())?;
        if device_id != DeviceId([0; 16]) && self.store.requires_rejoin(room_id, device_id)? {
            return Ok(MlsSubmitOutcome::RejoinRequired);
        }
        if room.revocation_pending {
            return Ok(MlsSubmitOutcome::RevocationPending);
        }
        if epoch != room.public_state.epoch {
            return Ok(MlsSubmitOutcome::StaleEpochNotStored {
                current_epoch: room.public_state.epoch,
            });
        }
        self.validator
            .validate_application(&room.public_state, &ciphertext)
            .map_err(|error| error.to_string())?;
        match self.store.append_application(
            room_id,
            device_id,
            epoch,
            event_id,
            &ciphertext,
            unix_time_ms(),
        )? {
            AppendApplicationResult::Stored { sequence, room } => {
                self.rooms.insert(room_id, room);
                Ok(MlsSubmitOutcome::Stored { sequence })
            }
            AppendApplicationResult::AlreadyStored { sequence } => {
                Ok(MlsSubmitOutcome::AlreadyStored { sequence })
            }
            AppendApplicationResult::Conflict => {
                Err("MLS event id was reused for a different application".to_string())
            }
            AppendApplicationResult::StaleEpoch { current_epoch } => {
                Ok(MlsSubmitOutcome::StaleEpochNotStored { current_epoch })
            }
            AppendApplicationResult::RevocationPending => Ok(MlsSubmitOutcome::RevocationPending),
            AppendApplicationResult::RejoinRequired => Ok(MlsSubmitOutcome::RejoinRequired),
        }
    }

    #[allow(dead_code)]
    pub fn events(
        &self,
        room_id: RoomId,
        after: u64,
        limit: usize,
    ) -> Option<Vec<MlsDeliveryEvent>> {
        self.rooms.get(&room_id)?;
        self.store.events(room_id, after, limit).ok().map(|batch| batch.events)
    }

    pub fn event_batch(
        &self,
        room_id: RoomId,
        after: u64,
        limit: usize,
    ) -> Result<crate::mls_store::EventBatch, String> {
        self.store.events(room_id, after, limit)
    }

    pub fn welcomes(&self, device_id: DeviceId, after: u64) -> Result<Vec<MlsWelcome>, String> {
        self.store.welcomes(device_id, after)
    }

    pub fn welcome_head(&self, device_id: DeviceId) -> Result<u64, String> {
        self.store.welcome_head(device_id)
    }

    pub fn acknowledge(
        &self,
        room_id: RoomId,
        device_id: DeviceId,
        sequence: u64,
    ) -> Result<(), String> {
        self.store
            .acknowledge(room_id, device_id, sequence, unix_time_ms())
    }

    pub fn acknowledge_welcome(
        &self,
        device_id: DeviceId,
        delivery_id: u64,
    ) -> Result<(), String> {
        self.store.acknowledge_welcome(device_id, delivery_id)
    }

    pub fn cleanup(&mut self, batch_events: usize) -> Result<crate::mls_store::CleanupReport, String> {
        let report = self.store.cleanup(unix_time_ms(), batch_events)?;
        if report.deleted_events > 0 {
            self.rooms = self
                .store
                .load()?
                .rooms
                .into_iter()
                .map(|room| (room.descriptor.room_id, room))
                .collect();
        }
        Ok(report)
    }

    pub fn storage_status(&self) -> Result<crate::mls_store::StorageStatus, String> {
        self.store.status()
    }

    pub fn compact(&mut self) -> Result<bool, String> {
        self.store.compact()
    }

    pub fn room_account_member(&self, room_id: RoomId, account_id: AccountId) -> bool {
        self.rooms.get(&room_id).is_some_and(|room| {
            room.descriptor
                .member_accounts
                .binary_search(&account_id)
                .is_ok()
        })
    }

    fn device_is_active(&self, device_id: DeviceId) -> bool {
        self.device_owners
            .get(&device_id)
            .and_then(|user_id| self.rosters.get(user_id))
            .is_some_and(|roster| {
                roster
                    .body
                    .active_devices
                    .iter()
                    .any(|certificate| certificate.body.device_id == device_id)
            })
    }

    fn snapshot(&self) -> CacheState {
        CacheState {
            global: GlobalRecord {
            initialized_accounts: {
                let mut users = self.initialized_accounts.iter().copied().collect::<Vec<_>>();
                users.sort_unstable();
                users
            },
            rosters: self
                .rosters
                .iter()
                .map(|(user_id, roster)| (*user_id, roster.clone()))
                .collect(),
            key_packages: self
                .key_packages
                .iter()
                .map(|(device_id, packages)| (*device_id, packages.iter().cloned().collect()))
                .collect(),
            retired_key_packages: {
                let mut hashes = self.retired_key_packages.iter().copied().collect::<Vec<_>>();
                hashes.sort_unstable();
                hashes
            },
            issued_key_packages: self.issued_key_packages.iter().cloned().collect(),
            used_key_packages: self.used_key_packages.iter().cloned().collect(),
            next_welcome_delivery_id: self.next_welcome_delivery_id,
            credentials: self.credentials.clone(),
            device_owners: self
                .device_owners
                .iter()
                .map(|(device_id, user_id)| (*device_id, *user_id))
                .collect(),
            },
            rooms: self.rooms.values().cloned().collect(),
        }
    }

    fn restore(&mut self, state: CacheState) -> Result<(), String> {
        let identities = ChattIdentityProvider::new(self.server_id.clone());
        let roster_count = state.global.rosters.len();
        let rosters: HashMap<_, _> = state.global.rosters.into_iter().collect();
        if rosters.len() != roster_count {
            return Err("persisted MLS state contains duplicate roster users".to_string());
        }
        for roster in rosters.values() {
            identities
                .install_roster(roster)
                .map_err(|error| format!("invalid persisted MLS roster: {error}"))?;
        }
        let room_count = state.rooms.len();
        let rooms: HashMap<_, _> = state
            .rooms
            .into_iter()
            .map(|room| (room.descriptor.room_id, room))
            .collect();
        if rooms.len() != room_count {
            return Err("persisted MLS state contains duplicate room ids".to_string());
        }
        for room in rooms.values() {
            identities
                .install_room(room.descriptor.clone())
                .map_err(|error| format!("invalid persisted MLS room: {error}"))?;
        }
        self.validator = PublicGroupValidator::new(identities.clone());
        self.identities = identities;
        self.initialized_accounts = state.global.initialized_accounts.into_iter().collect();
        self.rosters = rosters;
        self.key_packages = state.global
            .key_packages
            .into_iter()
            .map(|(device_id, packages)| (device_id, packages.into()))
            .collect();
        self.retired_key_packages = state.global.retired_key_packages.into_iter().collect();
        self.issued_key_packages = state.global.issued_key_packages.into_iter().collect();
        self.used_key_packages = state.global.used_key_packages.into_iter().collect();
        if !self.used_key_packages.is_subset(&self.issued_key_packages) {
            return Err("persisted MLS KeyPackage usage is inconsistent".to_string());
        }
        self.rooms = rooms;
        self.next_welcome_delivery_id = state.global.next_welcome_delivery_id.max(1);
        self.credentials = state.global.credentials;
        let device_owner_count = state.global.device_owners.len();
        self.device_owners = state.global.device_owners.into_iter().collect();
        if self.device_owners.len() != device_owner_count {
            return Err("persisted MLS device ownership contains duplicates".to_string());
        }
        for (user_id, roster) in &self.rosters {
            for certificate in &roster.body.active_devices {
                if self.device_owners.get(&certificate.body.device_id) != Some(user_id) {
                    return Err("persisted MLS device ownership does not match its roster".to_string());
                }
            }
        }
        self.validate_invariants()
    }

    fn persist(&self) -> Result<(), String> {
        debug_assert!(
            self.validate_invariants().is_ok(),
            "in-memory MLS service state violated its structural invariants"
        );
        let snapshot = self.snapshot();
        self.store
            .replace_global_and_rooms(&snapshot.global, &snapshot.rooms)
    }

    fn validate_invariants(&self) -> Result<(), String> {
        if !self.used_key_packages.is_subset(&self.issued_key_packages) {
            return Err("MLS KeyPackage usage is inconsistent".to_string());
        }
        for (device_id, packages) in &self.key_packages {
            if packages.len() > MAX_KEY_PACKAGES_PER_DEVICE {
                return Err("MLS device has too many queued KeyPackages".to_string());
            }
            if !self.device_owners.contains_key(device_id) {
                return Err("MLS KeyPackage queue has no device owner".to_string());
            }
        }
        let mut account_ids = HashSet::new();
        for (user_id, roster) in &self.rosters {
            if !account_ids.insert(roster.body.account_id) {
                return Err("MLS rosters reuse an account id".to_string());
            }
            for certificate in &roster.body.active_devices {
                if certificate.body.user_id != *user_id
                    || self.device_owners.get(&certificate.body.device_id) != Some(user_id)
                {
                    return Err("MLS roster device ownership is inconsistent".to_string());
                }
            }
        }
        let mut credential_devices = HashSet::new();
        for credential in &self.credentials {
            if !credential_devices.insert(credential.device_id)
                || self.device_owners.get(&credential.device_id) != Some(&credential.user_id)
            {
                return Err("MLS device credential ownership is inconsistent".to_string());
            }
        }
        let policy_rooms = self
            .identities
            .room_descriptors()
            .map_err(|error| error.to_string())?;
        if policy_rooms.len() != self.rooms.len()
            || policy_rooms.iter().any(|descriptor| {
                self.rooms
                    .get(&descriptor.room_id)
                    .is_none_or(|room| room.descriptor != *descriptor)
            })
        {
            return Err("MLS policy room descriptors do not match durable rooms".to_string());
        }
        for (room_id, room) in &self.rooms {
            if room.descriptor.room_id != *room_id {
                return Err("MLS room map key does not match its descriptor".to_string());
            }
            if room.oldest_available_sequence == 0
                || room.oldest_available_sequence > room.head_sequence.saturating_add(1)
                || room.required_devices.windows(2).any(|pair| pair[0] >= pair[1])
            {
                return Err("MLS room durable metadata is inconsistent".to_string());
            }
        }
        Ok(())
    }

    fn mark_initialized(&mut self, user_id: UserId) -> Result<(), String> {
        self.initialized_accounts.insert(user_id);
        Ok(())
    }

    fn rollback(&mut self, state: CacheState, persistence_error: String) -> String {
        match self.restore(state) {
            Ok(()) => persistence_error,
            Err(restore_error) => {
                format!(
                    "{persistence_error}; failed to restore in-memory MLS state: {restore_error}"
                )
            }
        }
    }
}

#[derive(Debug)]
pub(super) enum PutRosterError {
    Conflict(Option<RosterCheckpoint>),
    Invalid(String),
}

fn key_package_hash(package: &[u8]) -> [u8; 32] {
    ring::digest::digest(&ring::digest::SHA256, package)
        .as_ref()
        .try_into()
        .expect("SHA-256 output has a fixed length")
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use chatt_mls::LocalInstallation;
    use rpc::{
        identity::{
            mls_client_id, roster_checkpoint, sign_device_certificate, sign_device_roster,
        },
        ids::{EventId, RoomId, UserId},
        mls::{
            ChattEventContent, EncryptedRoomDescriptor, MLS_PROTOCOL_VERSION, MlsChattEvent,
            MlsSubmitOutcome,
        },
    };

    use super::*;

    #[test]
    fn initialized_fact_and_roster_share_the_redb_transaction() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [6u8; 32];
        let (installation, _) = LocalInstallation::open_or_create(
            &temp.path().join("client"),
            server_id,
            UserId(9),
            "client",
        )
        .unwrap();
        let state_dir = temp.path().join("server-continuity");
        std::fs::create_dir(&state_dir).unwrap();
        let mut service = MlsService::open(Some(state_dir.clone()), server_id.to_vec()).unwrap();
        service
            .put_roster(UserId(9), None, installation.bootstrap.own_roster, None)
            .unwrap();
        drop(service);

        let service = MlsService::open(Some(state_dir), server_id.to_vec()).unwrap();
        assert!(service.initialized(UserId(9)));
        assert!(service.roster(UserId(9)).is_some());
    }

    #[test]
    fn initial_device_credential_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [16u8; 32];
        let user_id = UserId(9);
        let (installation, _) = LocalInstallation::open_or_create(
            &temp.path().join("client"),
            server_id,
            user_id,
            "client",
        )
        .unwrap();
        let device_id = installation.bootstrap.device_id;
        let bearer = "initial-mls-device-credential";
        let state_dir = temp.path().join("server");
        std::fs::create_dir(&state_dir).unwrap();
        let mut service = MlsService::open(Some(state_dir.clone()), server_id.to_vec()).unwrap();
        service
            .put_roster(
                user_id,
                None,
                installation.bootstrap.own_roster,
                Some(crate::config::hash_secret(bearer)),
            )
            .unwrap();
        assert_eq!(
            service.authenticate_credential(bearer).map(|value| value.0),
            Some(user_id)
        );
        drop(service);

        let service = MlsService::open(Some(state_dir), server_id.to_vec()).unwrap();
        assert_eq!(
            service.authenticate_credential(bearer).map(|value| value.0),
            Some(user_id)
        );
        assert_eq!(
            service.authenticate_credential(bearer).map(|value| value.1),
            Some(device_id)
        );
    }

    #[test]
    fn device_ids_are_globally_reserved_across_accounts_and_restarts() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [17u8; 32];
        let (alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("device-owner-alice"),
            server_id,
            UserId(20),
            "alice",
        )
        .unwrap();
        let (bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("device-owner-bob"),
            server_id,
            UserId(21),
            "bob",
        )
        .unwrap();
        let mut bob_certificate = bob.bootstrap.device_certificate.body.clone();
        bob_certificate.device_id = alice.bootstrap.device_id;
        bob_certificate.mls_client_id = mls_client_id(
            &server_id,
            bob.bootstrap.account_id,
            alice.bootstrap.device_id,
        )
        .unwrap();
        let bob_certificate =
            sign_device_certificate(bob_certificate, &bob.bootstrap.authority_seed).unwrap();
        let mut bob_roster = bob.bootstrap.own_roster.body.clone();
        bob_roster.active_devices = vec![bob_certificate];
        let bob_roster = sign_device_roster(bob_roster, &bob.bootstrap.authority_seed).unwrap();

        let state_dir = temp.path().join("device-owner-server");
        std::fs::create_dir(&state_dir).unwrap();
        let mut service = MlsService::open(Some(state_dir.clone()), server_id.to_vec()).unwrap();
        service
            .put_roster(
                UserId(20),
                None,
                alice.bootstrap.own_roster.clone(),
                None,
            )
            .unwrap();
        drop(service);

        let mut service = MlsService::open(Some(state_dir), server_id.to_vec()).unwrap();
        assert!(matches!(
            service.put_roster(UserId(21), None, bob_roster, None),
            Err(PutRosterError::Invalid(ref error))
                if error.contains("already owned by another account")
        ));
    }

    #[test]
    fn key_package_publication_is_idempotent_and_consumption_is_permanent() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [26u8; 32];
        let user_id = UserId(10);
        let (installation, _) = LocalInstallation::open_or_create(
            &temp.path().join("client"),
            server_id,
            user_id,
            "client",
        )
        .unwrap();
        let state_dir = temp.path().join("server");
        std::fs::create_dir(&state_dir).unwrap();
        let mut service = MlsService::open(Some(state_dir.clone()), server_id.to_vec()).unwrap();
        service
            .put_roster(
                user_id,
                None,
                installation.bootstrap.own_roster.clone(),
                None,
            )
            .unwrap();
        let package = installation
            .client
            .generate_key_packages(installation.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        assert_eq!(
            service
                .publish_key_packages(
                    user_id,
                    installation.bootstrap.device_id,
                    vec![package.clone()],
                )
                .unwrap(),
            1
        );
        assert_eq!(
            service
                .publish_key_packages(
                    user_id,
                    installation.bootstrap.device_id,
                    vec![package.clone()],
                )
                .unwrap(),
            1
        );
        assert_eq!(
            service
                .take_key_package(installation.bootstrap.device_id)
                .unwrap(),
            Some(package.package.clone())
        );
        drop(service);

        let mut service = MlsService::open(Some(state_dir), server_id.to_vec()).unwrap();
        assert!(
            service
                .publish_key_packages(
                    user_id,
                    installation.bootstrap.device_id,
                    vec![package],
                )
                .is_err()
        );
    }

    #[test]
    fn restart_preserves_commit_bundle_and_idempotent_application_event() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [7u8; 32];
        let (alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("alice"),
            server_id,
            UserId(1),
            "alice",
        )
        .unwrap();
        let (bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("bob"),
            server_id,
            UserId(2),
            "bob",
        )
        .unwrap();
        alice.install_roster(&bob.bootstrap.own_roster).unwrap();
        bob.install_roster(&alice.bootstrap.own_roster).unwrap();
        let state_dir = temp.path().join("server");
        std::fs::create_dir(&state_dir).unwrap();
        let mut service = MlsService::open(Some(state_dir.clone()), server_id.to_vec()).unwrap();
        service
            .put_roster(UserId(1), None, alice.bootstrap.own_roster.clone(), None)
            .unwrap();
        service
            .put_roster(UserId(2), None, bob.bootstrap.own_roster.clone(), None)
            .unwrap();
        let published_package = bob
            .client
            .generate_key_packages(bob.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        let mut mislabeled_package = published_package.clone();
        mislabeled_package.device_id = alice.bootstrap.device_id;
        assert!(
            service
                .publish_key_packages(
                    UserId(1),
                    alice.bootstrap.device_id,
                    vec![mislabeled_package],
                )
                .unwrap_err()
                .contains("credential does not match")
        );
        assert_eq!(service.key_package_count(alice.bootstrap.device_id), 0);
        service
            .publish_key_packages(
                UserId(2),
                bob.bootstrap.device_id,
                vec![published_package.clone()],
            )
            .unwrap();
        let package = service
            .take_key_package(bob.bootstrap.device_id)
            .unwrap()
            .unwrap();
        let descriptor = EncryptedRoomDescriptor::new(
            RoomId(50),
            alice.bootstrap.account_id,
            vec![alice.bootstrap.account_id, bob.bootstrap.account_id],
            10,
        )
        .unwrap();
        let bundle = alice
            .client
            .create_room(&descriptor, &[(bob.bootstrap.device_id, package.clone())])
            .unwrap();
        let initial_group_info = bundle.group_info.clone();
        service
            .create_room(
                alice.bootstrap.account_id,
                &alice.bootstrap.device_certificate.body.mls_client_id,
                descriptor.clone(),
                &[
                    roster_checkpoint(&alice.bootstrap.own_roster),
                    roster_checkpoint(&bob.bootstrap.own_roster),
                ],
                bundle,
            )
            .unwrap();
        alice.client.accept_pending_commit(&descriptor, 1).unwrap();
        let reused_descriptor = EncryptedRoomDescriptor::new(
            RoomId(51),
            alice.bootstrap.account_id,
            vec![alice.bootstrap.account_id, bob.bootstrap.account_id],
            11,
        )
        .unwrap();
        let reused_bundle = alice
            .client
            .create_room(
                &reused_descriptor,
                &[(bob.bootstrap.device_id, package)],
            )
            .unwrap();
        assert!(
            service
                .create_room(
                    alice.bootstrap.account_id,
                    &alice.bootstrap.device_certificate.body.mls_client_id,
                    reused_descriptor,
                    &[
                        roster_checkpoint(&alice.bootstrap.own_roster),
                        roster_checkpoint(&bob.bootstrap.own_roster),
                    ],
                    reused_bundle,
                )
                .unwrap_err()
                .contains("already committed KeyPackage")
        );
        let event = MlsChattEvent {
            version: MLS_PROTOCOL_VERSION,
            room_id: descriptor.room_id,
            event_id: EventId([5; 16]),
            sender_account: alice.bootstrap.account_id,
            timestamp_ms: 20,
            content: ChattEventContent::Text {
                body: "durable".to_string(),
            },
        };
        alice.client.queue_outgoing(event.clone()).unwrap();
        let (epoch, ciphertext) = alice
            .client
            .encrypt_outgoing(&descriptor, event.event_id)
            .unwrap();
        assert_eq!(
            service
                .submit_application(
                    descriptor.room_id,
                    epoch,
                    event.event_id,
                    ciphertext.clone(),
                )
                .unwrap(),
            MlsSubmitOutcome::Stored { sequence: 2 },
        );
        drop(service);

        let mut service = MlsService::open(Some(state_dir), server_id.to_vec()).unwrap();
        assert_eq!(service.events(descriptor.room_id, 0, 10).unwrap().len(), 2);
        assert_eq!(
            service
                .submit_application(
                    descriptor.room_id,
                    epoch,
                    event.event_id,
                    ciphertext.clone(),
                )
                .unwrap(),
            MlsSubmitOutcome::AlreadyStored { sequence: 2 },
        );
        let mut conflicting_ciphertext = ciphertext.clone();
        *conflicting_ciphertext.last_mut().unwrap() ^= 1;
        assert!(
            service
                .submit_application(
                    descriptor.room_id,
                    epoch,
                    event.event_id,
                    conflicting_ciphertext,
                )
                .unwrap_err()
                .contains("event id was reused")
        );
        assert_eq!(
            service.roster(UserId(1)).map(roster_checkpoint),
            Some(roster_checkpoint(&alice.bootstrap.own_roster)),
        );
        let (expected_epoch, rejoin) = alice
            .client
            .prepare_external_rejoin(&descriptor, &initial_group_info)
            .unwrap();
        assert_eq!(expected_epoch, 1);
        assert_eq!(
            service
                .submit_commit(
                    descriptor.room_id,
                    &alice.bootstrap.device_certificate.body.mls_client_id,
                    expected_epoch,
                    rejoin.clone(),
                )
                .unwrap(),
            MlsCommitOutcome::Accepted {
                sequence: 3,
                epoch: 2,
            }
        );
        assert!(
            service
                .submit_application(
                    descriptor.room_id,
                    2,
                    EventId([6; 16]),
                    ciphertext,
                )
                .unwrap_err()
                .contains("current group epoch")
        );
        assert_eq!(
            service
                .submit_commit(
                    descriptor.room_id,
                    &alice.bootstrap.device_certificate.body.mls_client_id,
                    expected_epoch,
                    rejoin,
                )
                .unwrap(),
            MlsCommitOutcome::AlreadyAccepted {
                sequence: 3,
                epoch: 2,
            }
        );
    }

    #[test]
    fn fixed_group_creation_adds_three_accounts_with_available_devices() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [8u8; 32];
        let (alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("group-alice"),
            server_id,
            UserId(11),
            "alice",
        )
        .unwrap();
        let (bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("group-bob"),
            server_id,
            UserId(12),
            "bob",
        )
        .unwrap();
        let (carol, _) = LocalInstallation::open_or_create(
            &temp.path().join("group-carol"),
            server_id,
            UserId(13),
            "carol",
        )
        .unwrap();
        for installation in [&alice, &bob, &carol] {
            for roster in [
                &alice.bootstrap.own_roster,
                &bob.bootstrap.own_roster,
                &carol.bootstrap.own_roster,
            ] {
                installation.install_roster(roster).unwrap();
            }
        }
        let mut service = MlsService::new(server_id.to_vec());
        for (user, roster) in [
            (UserId(11), alice.bootstrap.own_roster.clone()),
            (UserId(12), bob.bootstrap.own_roster.clone()),
            (UserId(13), carol.bootstrap.own_roster.clone()),
        ] {
            service.put_roster(user, None, roster, None).unwrap();
        }
        let bob_package = bob
            .client
            .generate_key_packages(bob.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        let carol_package = carol
            .client
            .generate_key_packages(carol.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        service
            .publish_key_packages(
                UserId(12),
                bob.bootstrap.device_id,
                vec![bob_package],
            )
            .unwrap();
        service
            .publish_key_packages(
                UserId(13),
                carol.bootstrap.device_id,
                vec![carol_package],
            )
            .unwrap();
        let bob_package = service
            .take_key_package(bob.bootstrap.device_id)
            .unwrap()
            .unwrap();
        let carol_package = service
            .take_key_package(carol.bootstrap.device_id)
            .unwrap()
            .unwrap();
        let descriptor = EncryptedRoomDescriptor::new(
            RoomId(60),
            alice.bootstrap.account_id,
            vec![
                alice.bootstrap.account_id,
                bob.bootstrap.account_id,
                carol.bootstrap.account_id,
            ],
            10,
        )
        .unwrap();
        let bundle = alice
            .client
            .create_room(
                &descriptor,
                &[
                    (bob.bootstrap.device_id, bob_package),
                    (carol.bootstrap.device_id, carol_package),
                ],
            )
            .unwrap();
        assert_eq!(bundle.welcome.as_ref().unwrap().device_ids.len(), 2);
        let mut wrong_targets = bundle.clone();
        wrong_targets
            .welcome
            .as_mut()
            .unwrap()
            .device_ids
            .pop();
        assert!(
            service
                .create_room(
                    alice.bootstrap.account_id,
                    &alice.bootstrap.device_certificate.body.mls_client_id,
                    descriptor.clone(),
                    &[
                        roster_checkpoint(&alice.bootstrap.own_roster),
                        roster_checkpoint(&bob.bootstrap.own_roster),
                        roster_checkpoint(&carol.bootstrap.own_roster),
                    ],
                    wrong_targets,
                )
                .is_err()
        );
        service.validate_invariants().unwrap();
        assert!(service.identities.room_descriptors().unwrap().is_empty());
        let state = service
            .create_room(
                alice.bootstrap.account_id,
                &alice.bootstrap.device_certificate.body.mls_client_id,
                descriptor,
                &[
                    roster_checkpoint(&alice.bootstrap.own_roster),
                    roster_checkpoint(&bob.bootstrap.own_roster),
                    roster_checkpoint(&carol.bootstrap.own_roster),
                ],
                bundle,
            )
            .unwrap();
        assert_eq!(state, 1);
        assert_eq!(
            service.rooms[&RoomId(60)]
                .public_state
                .member_client_ids
                .len(),
            3
        );
    }
}
