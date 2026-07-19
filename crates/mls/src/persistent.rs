//! redb-backed MLS state and application event cache.
//!
//! `mls-rs` deliberately keeps a processed group's mutations in memory until
//! `write_to_storage`. That lets this wrapper durably insert decrypted
//! plaintext first, persist the MLS ratchet second, and advance the delivery
//! cursor last.
//!
//! The redb file is owner-only but is not encrypted. The replaced SQLCipher
//! setup derived its key from key material stored in plaintext in the adjacent
//! owner-only bootstrap file, so it did not provide an independent at-rest
//! security boundary. MLS storage still needs authenticated encryption backed
//! by an externally protected key; that is intentionally deferred.

use std::{collections::HashMap, path::Path, sync::Mutex};

use aws_lc_rs::digest;
use jsony::Jsony;
use mls_rs::{
    CipherSuiteProvider, Client, CryptoProvider, ExtensionList, MlsMessage,
    client_builder::{
        BaseConfig, WithCryptoProvider, WithGroupStateStorage, WithIdentityProvider,
        WithKeyPackageRepo, WithMlsRules, WithPskStore,
    },
    group::{CommitEffect, ReceivedMessage},
    identity::{SigningIdentity, basic::BasicCredential},
};
use mls_rs_core::crypto::{SignaturePublicKey, SignatureSecretKey};
use mls_rs_crypto_awslc::AwsLcCryptoProvider;
use mls_rs_provider_redb::{
    RedbDataStorageEngine, RedbStorageStats,
    storage::{
        Item, RedbApplicationStorage, RedbGroupStateStorage, RedbKeyPackageStorage,
        RedbPreSharedKeyStorage,
    },
};
use rpc::{
    ids::{AccountId, DeviceId, EventId, RoomId},
    mls::{
        EncryptedRoomDescriptor, MlsChattEvent, MlsCommitBundle, MlsDeliveryEvent, MlsWelcome,
        MlsWelcomeBundle, PublishedKeyPackage,
    },
};

use crate::{CIPHER_SUITE, ChattIdentityProvider, ChattMlsPolicy, PublicGroupValidator};

type PersistentConfig = WithMlsRules<
    ChattMlsPolicy,
    WithIdentityProvider<
        ChattIdentityProvider,
        WithCryptoProvider<
            AwsLcCryptoProvider,
            WithGroupStateStorage<
                RedbGroupStateStorage,
                WithPskStore<
                    RedbPreSharedKeyStorage,
                    WithKeyPackageRepo<RedbKeyPackageStorage, BaseConfig>,
                >,
            >,
        >,
    >,
>;

const SIGNING_MATERIAL_KEY: &str = "installation/signing-material";
const PENDING_PAIR_KEY_PACKAGES_KEY: &str = "installation/pending-pair-key-packages";

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
struct SigningMaterial {
    client_id: Vec<u8>,
    public_key: Vec<u8>,
    secret_key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct CachedIncomingEvent {
    pub sequence: u64,
    pub sender_client_id: Vec<u8>,
    pub event: MlsChattEvent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedApplicationEvent {
    pub sequence: u64,
    pub event: MlsChattEvent,
}

/// An application event whose durable delivery cursor has advanced but whose
/// UI notification has not yet been acknowledged by the network worker.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct PendingUiDispatch {
    pub sequence: u64,
    pub event: MlsChattEvent,
}

/// The source needed to reconstruct an encrypted file upload after reconnect.
/// The authenticated metadata and content key remain in the matching outbox
/// event, so this record deliberately carries only local source ownership.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DurableFileUpload {
    pub room_id: RoomId,
    pub event_id: EventId,
    pub source_path: Vec<u8>,
    pub delete_after_upload: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct OutboxEntry {
    pub event: MlsChattEvent,
    pub state: OutboxState,
    queued_order: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub enum OutboxState {
    PendingEncryption,
    PendingDelivery {
        epoch: u64,
        ciphertext: Vec<u8>,
    },
    Delivered {
        sequence: u64,
        epoch: u64,
        ciphertext_hash: [u8; 32],
        dispatch_queued: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessedDelivery {
    Application(CachedIncomingEvent),
    /// A locally encrypted application recovered from the ordered delivery
    /// stream. `event` is present only when this stream delivery, rather than
    /// the submit response, first made the outbox entry durable as delivered.
    Outgoing {
        sequence: u64,
        event: Option<MlsChattEvent>,
    },
    Commit {
        sequence: u64,
        epoch: u64,
    },
    /// The MLS application was authentic and advanced the secret tree, but
    /// its Chatt payload was malformed or did not match the public envelope.
    /// Such a message must not pin the ordered delivery cursor forever.
    RejectedApplication {
        sequence: u64,
        reason: String,
    },
    AlreadyProcessed {
        sequence: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
enum DeliveryMarker {
    Application {
        sequence: u64,
        event_id: EventId,
        message_hash: [u8; 32],
    },
    Commit {
        sequence: u64,
        parent_epoch: u64,
        epoch: u64,
        message_hash: [u8; 32],
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
struct PendingCommitRecord {
    descriptor: EncryptedRoomDescriptor,
    parent_epoch: u64,
    epoch: u64,
    commit: Vec<u8>,
    group_info: Vec<u8>,
}

/// One installation's MLS client and encrypted application cache.
pub struct PersistentClient {
    client: Client<PersistentConfig>,
    storage: RedbDataStorageEngine,
    application: RedbApplicationStorage,
    identities: ChattIdentityProvider,
    signing_secret: SignatureSecretKey,
    pending_external_rejoins: Mutex<HashMap<RoomId, mls_rs::group::Group<PersistentConfig>>>,
}

impl std::fmt::Debug for PersistentClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersistentClient").finish_non_exhaustive()
    }
}

impl PersistentClient {
    pub fn open(
        path: &Path,
        identities: ChattIdentityProvider,
        signing_identity: SigningIdentity,
        signing_secret: SignatureSecretKey,
    ) -> Result<Self, String> {
        let storage = open_storage(path)?;
        let application = storage.application_data_storage();
        let client_id = signing_identity
            .credential
            .as_basic()
            .map(|credential| credential.identifier.clone())
            .ok_or_else(|| "local MLS credential is not Basic".to_string())?;
        let material = SigningMaterial {
            client_id,
            public_key: signing_identity.signature_key.as_ref().to_vec(),
            secret_key: signing_secret.as_ref().to_vec(),
        };
        match application
            .get(SIGNING_MATERIAL_KEY)
            .map_err(|error| error.to_string())?
        {
            None => {
                application
                    .insert(SIGNING_MATERIAL_KEY, &jsony::to_binary(&material))
                    .map_err(|error| error.to_string())?;
            }
            Some(stored) => {
                let stored: SigningMaterial =
                    jsony::from_binary(&stored).map_err(|error| error.to_string())?;
                if stored.client_id != material.client_id
                    || stored.public_key != material.public_key
                    || stored.secret_key != material.secret_key
                {
                    return Err("redb database belongs to another MLS credential".to_string());
                }
            }
        }
        Self::build(
            storage,
            application,
            identities,
            signing_identity,
            signing_secret,
        )
    }

    pub fn reopen(path: &Path, identities: ChattIdentityProvider) -> Result<Self, String> {
        let storage = open_storage(path)?;
        let application = storage.application_data_storage();
        let bytes = application
            .get(SIGNING_MATERIAL_KEY)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "redb database has no MLS signing material".to_string())?;
        let material: SigningMaterial =
            jsony::from_binary(&bytes).map_err(|error| error.to_string())?;
        let signing_identity = SigningIdentity::new(
            BasicCredential::new(material.client_id).into_credential(),
            SignaturePublicKey::from(material.public_key),
        );
        let signing_secret = SignatureSecretKey::from(material.secret_key);
        Self::build(
            storage,
            application,
            identities,
            signing_identity,
            signing_secret,
        )
    }

    fn build(
        storage: RedbDataStorageEngine,
        application: RedbApplicationStorage,
        identities: ChattIdentityProvider,
        signing_identity: SigningIdentity,
        signing_secret: SignatureSecretKey,
    ) -> Result<Self, String> {
        let client = Client::builder()
            .key_package_repo(storage.key_package_storage())
            .psk_store(storage.pre_shared_key_storage())
            .group_state_storage(storage.group_state_storage())
            .crypto_provider(AwsLcCryptoProvider::default())
            .identity_provider(identities.historical_group_info_observer())
            .mls_rules(ChattMlsPolicy::new(identities.clone()))
            .signing_identity(signing_identity, signing_secret.clone(), CIPHER_SUITE)
            .build();
        Ok(Self {
            client,
            storage,
            application,
            identities,
            signing_secret,
            pending_external_rejoins: Mutex::new(HashMap::new()),
        })
    }

    pub fn sign_device_binding(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        AwsLcCryptoProvider::default()
            .cipher_suite_provider(CIPHER_SUITE)
            .ok_or_else(|| "mandatory MLS cipher suite is unavailable".to_string())?
            .sign(&self.signing_secret, message)
            .map_err(|error| error.to_string())
    }

    pub fn storage_stats(&self) -> Result<RedbStorageStats, String> {
        self.storage
            .storage_stats()
            .map_err(|error| error.to_string())
    }

    pub fn storage_is_quiescent(&self) -> Result<bool, String> {
        self.pending_external_rejoins
            .lock()
            .map(|groups| groups.is_empty())
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())
    }

    pub fn clear_transient_groups(&self) -> Result<(), String> {
        self.pending_external_rejoins
            .lock()
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())?
            .clear();
        Ok(())
    }

    pub fn generate_key_packages(
        &self,
        device_id: DeviceId,
        count: usize,
    ) -> Result<Vec<PublishedKeyPackage>, String> {
        if count == 0 || count > rpc::mls::MAX_MLS_KEY_PACKAGES_PER_REQUEST {
            return Err("invalid KeyPackage generation count".to_string());
        }
        (0..count)
            .map(|_| {
                let message = self
                    .client
                    .generate_key_package_message(ExtensionList::new(), ExtensionList::new(), None)
                    .map_err(|error| error.to_string())?;
                Ok(PublishedKeyPackage {
                    device_id,
                    package: message.to_bytes().map_err(|error| error.to_string())?,
                })
            })
            .collect()
    }

    /// Returns the exact initial KeyPackage batch for a pending pair retry.
    pub fn pending_pair_key_packages(
        &self,
        device_id: DeviceId,
        count: usize,
    ) -> Result<Vec<PublishedKeyPackage>, String> {
        if let Some(encoded) = self
            .application
            .get(PENDING_PAIR_KEY_PACKAGES_KEY)
            .map_err(|error| error.to_string())?
        {
            let packages: Vec<PublishedKeyPackage> =
                jsony::from_binary(&encoded).map_err(|error| error.to_string())?;
            if packages.len() != count
                || packages
                    .iter()
                    .any(|package| package.device_id != device_id)
            {
                return Err("stored pending-pair KeyPackages are inconsistent".to_string());
            }
            return Ok(packages);
        }
        let packages = self.generate_key_packages(device_id, count)?;
        self.application
            .insert(PENDING_PAIR_KEY_PACKAGES_KEY, &jsony::to_binary(&packages))
            .map_err(|error| error.to_string())?;
        Ok(packages)
    }

    /// Creates an initial pending commit and persists that pending state. The
    /// caller must call `accept_pending_commit` only after server acceptance,
    /// or `reject_pending_commit` after a typed rejection.
    pub fn create_room(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        members: &[(DeviceId, Vec<u8>)],
    ) -> Result<MlsCommitBundle, String> {
        descriptor.validate()?;
        self.identities
            .install_room(descriptor.clone())
            .map_err(|error| error.to_string())?;
        if members.is_empty() {
            return Err("room creation requires at least one peer KeyPackage".to_string());
        }
        let mut group = self
            .client
            .create_group_with_id(
                descriptor.mls_group_id.clone(),
                ExtensionList::new(),
                ExtensionList::new(),
                None,
            )
            .map_err(|error| error.to_string())?;
        let prior_group_info = group
            .group_info_message(true)
            .map_err(|error| error.to_string())?
            .to_bytes()
            .map_err(|error| error.to_string())?;
        let mut builder = group.commit_builder();
        for (_, encoded) in members {
            let key_package = MlsMessage::from_bytes(encoded)
                .map_err(|error| format!("invalid KeyPackage: {error}"))?;
            builder = builder
                .add_member(key_package)
                .map_err(|error| error.to_string())?;
        }
        let output = builder.build().map_err(|error| error.to_string())?;
        if output.welcome_messages.len() != 1 {
            return Err("MLS runtime did not produce one consolidated Welcome".to_string());
        }
        let group_info = output
            .external_commit_group_info
            .as_ref()
            .ok_or_else(|| "MLS runtime did not produce a resulting GroupInfo".to_string())?
            .to_bytes()
            .map_err(|error| error.to_string())?;
        let welcome = Some(MlsWelcomeBundle {
            device_ids: members.iter().map(|(device_id, _)| *device_id).collect(),
            descriptor: descriptor.clone(),
            welcome: output.welcome_messages[0]
                .to_bytes()
                .map_err(|error| error.to_string())?,
        });
        let commit = output
            .commit_message
            .to_bytes()
            .map_err(|error| error.to_string())?;
        let next_epoch = group
            .context()
            .epoch()
            .checked_add(1)
            .ok_or_else(|| "MLS epoch is exhausted".to_string())?;
        self.store_pending_commit(&PendingCommitRecord {
            descriptor: descriptor.clone(),
            parent_epoch: group.context().epoch(),
            epoch: next_epoch,
            commit: commit.clone(),
            group_info: group_info.clone(),
        })?;
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        Ok(MlsCommitBundle {
            prior_group_info: Some(prior_group_info),
            commit,
            welcome,
            group_info,
        })
    }

    pub fn accept_pending_commit(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        sequence: u64,
    ) -> Result<(), String> {
        let cursor = self.cursor(descriptor.room_id)?;
        if cursor.checked_add(1) != Some(sequence) {
            return Err(format!(
                "cannot accept MLS commit at delivery sequence {sequence} with cursor {cursor}"
            ));
        }
        let pending = self
            .pending_commit(descriptor.room_id)?
            .ok_or_else(|| "MLS group has no durable pending commit".to_string())?;
        if pending.descriptor != *descriptor
            || pending.parent_epoch.checked_add(1) != Some(pending.epoch)
        {
            return Err("durable MLS pending commit is inconsistent".to_string());
        }
        self.store_descriptor(descriptor)?;
        let delivery = MlsDeliveryEvent::Commit {
            sequence,
            parent_epoch: pending.parent_epoch,
            epoch: pending.epoch,
            rosters: Vec::new(),
            commit: pending.commit,
        };
        let mut group = self.load_group(descriptor)?;
        if group.has_pending_commit() {
            group
                .apply_pending_commit()
                .map_err(|error| error.to_string())?;
            self.store_delivery_marker(descriptor.room_id, &delivery)?;
            group
                .write_to_storage()
                .map_err(|error| error.to_string())?;
        } else if group.context().epoch() != pending.epoch
            || !self
                .delivery_marker(descriptor.room_id)?
                .is_some_and(|marker| marker.matches(&delivery))
        {
            return Err("MLS group has no matching pending commit to accept".to_string());
        }
        self.set_cursor(descriptor.room_id, sequence)?;
        self.clear_pending_commit_record(descriptor.room_id)
    }

    pub fn reject_pending_commit(
        &self,
        descriptor: &EncryptedRoomDescriptor,
    ) -> Result<(), String> {
        let mut group = self.load_group(descriptor)?;
        group.clear_pending_commit();
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        self.clear_pending_commit_record(descriptor.room_id)
    }

    pub fn join_welcome(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        welcome: &MlsWelcome,
    ) -> Result<(), String> {
        descriptor.validate()?;
        if welcome.descriptor != *descriptor {
            return Err("Welcome room descriptor does not match".to_string());
        }
        if self.cursor(descriptor.room_id)? >= welcome.sequence
            && self.descriptor(descriptor.room_id)?.as_ref() == Some(descriptor)
        {
            return Ok(());
        }
        self.identities
            .install_room(descriptor.clone())
            .map_err(|error| error.to_string())?;
        if welcome.sequence == 0 {
            return Err("delivered Welcome has no commit sequence".to_string());
        }
        let message = MlsMessage::from_bytes(&welcome.welcome)
            .map_err(|error| format!("invalid Welcome: {error}"))?;
        let (mut group, _) = self
            .client
            .join_group(None, &message, None)
            .map_err(|error| error.to_string())?;
        if group.context().group_id() != descriptor.mls_group_id {
            return Err("Welcome MLS group id does not match room descriptor".to_string());
        }
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        self.store_descriptor_with_cursor(descriptor, welcome.sequence)
    }

    /// Builds an external commit without persisting the resulting private
    /// group state. The state remains pending until the delivery service
    /// accepts this exact commit.
    pub fn prepare_external_rejoin(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        encoded_group_info: &[u8],
    ) -> Result<(u64, MlsCommitBundle), String> {
        descriptor.validate()?;
        self.identities
            .install_room(descriptor.clone())
            .map_err(|error| error.to_string())?;
        if self
            .pending_external_rejoins
            .lock()
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())?
            .contains_key(&descriptor.room_id)
        {
            return Err("an external rejoin is already pending for this room".to_string());
        }
        let group_info = MlsMessage::from_bytes(encoded_group_info)
            .map_err(|error| format!("invalid external rejoin GroupInfo: {error}"))?;
        let own_client_id = self
            .client
            .signing_identity()
            .map_err(|error| error.to_string())?
            .0
            .credential
            .as_basic()
            .map(|credential| credential.identifier.clone())
            .ok_or_else(|| "local MLS credential is not Basic".to_string())?;
        let validator = PublicGroupValidator::new_historical_observer(self.identities.clone());
        let observed = validator
            .observe_group_info(encoded_group_info)
            .map_err(|error| error.to_string())?;
        if observed.group_id != descriptor.mls_group_id {
            return Err("external rejoin GroupInfo does not match the room descriptor".to_string());
        }
        let mut builder = self
            .client
            .external_commit_builder()
            .map_err(|error| error.to_string())?;
        if let Some(index) = validator
            .member_index(encoded_group_info, &own_client_id)
            .map_err(|error| error.to_string())?
        {
            builder = builder.with_removal(index);
        }
        let (group, commit) = builder
            .build(group_info)
            .map_err(|error| error.to_string())?;
        let next_epoch = observed
            .epoch
            .checked_add(1)
            .ok_or_else(|| "MLS epoch is exhausted".to_string())?;
        if group.context().group_id() != descriptor.mls_group_id
            || group.context().epoch() != next_epoch
        {
            return Err("external rejoin produced an unexpected MLS group state".to_string());
        }
        let resulting_group_info = group
            .group_info_message_allowing_ext_commit_with_extensions(true, ExtensionList::new())
            .map_err(|error| error.to_string())?
            .to_bytes()
            .map_err(|error| error.to_string())?;
        let commit = commit.to_bytes().map_err(|error| error.to_string())?;
        self.pending_external_rejoins
            .lock()
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())?
            .insert(descriptor.room_id, group);
        Ok((
            observed.epoch,
            MlsCommitBundle {
                prior_group_info: None,
                commit,
                welcome: None,
                group_info: resulting_group_info,
            },
        ))
    }

    pub fn accept_external_rejoin(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        sequence: u64,
    ) -> Result<(), String> {
        descriptor.validate()?;
        let cursor = self.cursor(descriptor.room_id)?;
        if sequence <= cursor {
            return Err("external rejoin does not advance the delivery cursor".to_string());
        }
        let mut pending = self
            .pending_external_rejoins
            .lock()
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())?;
        let group = pending
            .get_mut(&descriptor.room_id)
            .ok_or_else(|| "MLS room has no pending external rejoin".to_string())?;
        if group.context().group_id() != descriptor.mls_group_id {
            return Err("external rejoin group does not match the room descriptor".to_string());
        }
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        self.store_descriptor_with_cursor(descriptor, sequence)?;
        pending.remove(&descriptor.room_id);
        Ok(())
    }

    pub fn reject_external_rejoin(&self, room_id: RoomId) -> Result<(), String> {
        self.pending_external_rejoins
            .lock()
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())?
            .remove(&room_id);
        Ok(())
    }

    pub fn queue_outgoing(&self, event: MlsChattEvent) -> Result<(), String> {
        self.queue_outgoing_with(event, None)
    }

    /// Atomically records a file announcement and the local source required to
    /// fulfill it. An accepted announcement can therefore never outlive the
    /// only record from which the in-memory upload queue is reconstructed.
    pub fn queue_file_outgoing(
        &self,
        event: MlsChattEvent,
        source_path: Vec<u8>,
        delete_after_upload: bool,
    ) -> Result<(), String> {
        if !matches!(event.content, rpc::mls::ChattEventContent::File(_)) {
            return Err("durable file upload requires an MLS file event".to_string());
        }
        let upload = DurableFileUpload {
            room_id: event.room_id,
            event_id: event.event_id,
            source_path,
            delete_after_upload,
        };
        self.queue_outgoing_with(event, Some(upload))
    }

    fn queue_outgoing_with(
        &self,
        event: MlsChattEvent,
        upload: Option<DurableFileUpload>,
    ) -> Result<(), String> {
        let own_client_id = self
            .client
            .signing_identity()
            .map_err(|error| error.to_string())?
            .0
            .credential
            .as_basic()
            .map(|credential| credential.identifier.as_slice())
            .ok_or_else(|| "local MLS credential is not Basic".to_string())?;
        let own_account = self
            .identities
            .account_for_client(own_client_id)
            .map_err(|error| error.to_string())?;
        validate_event(&event, event.room_id, event.event_id, own_account)?;
        let key = outbox_key(event.room_id, event.event_id);
        if self
            .application
            .get(&key)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("outgoing MLS event id already exists".to_string());
        }
        let queued_order = self
            .application
            .get(outbox_order_key())
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()?
            .unwrap_or(0_u64)
            .checked_add(1)
            .ok_or_else(|| "MLS outbox enqueue order is exhausted".to_string())?;
        let mut items = vec![
            Item::new(
                key,
                jsony::to_binary(&OutboxEntry {
                    event,
                    state: OutboxState::PendingEncryption,
                    queued_order,
                }),
            ),
            Item::new(
                outbox_order_key().to_string(),
                jsony::to_binary(&queued_order),
            ),
        ];
        if let Some(upload) = upload {
            items.push(Item::new(
                file_upload_key(upload.room_id, upload.event_id),
                jsony::to_binary(&upload),
            ));
        }
        self.application
            .transact_insert(&items)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Advances the sender ratchet before storing the exact ciphertext. A
    /// crash between those writes leaves `PendingEncryption`, so retry uses a
    /// fresh generation and no ciphertext from the lost generation was sent.
    pub fn encrypt_outgoing(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        event_id: EventId,
    ) -> Result<(u64, Vec<u8>), String> {
        let key = outbox_key(descriptor.room_id, event_id);
        let mut entry = self.outbox_entry(descriptor.room_id, event_id)?;
        match &entry.state {
            OutboxState::PendingDelivery { epoch, ciphertext } => {
                return Ok((*epoch, ciphertext.clone()));
            }
            OutboxState::Delivered { .. } => {
                return Err("MLS event is already delivered".to_string());
            }
            OutboxState::PendingEncryption => {}
        }
        let mut group = self.load_group(descriptor)?;
        if group.has_pending_commit() {
            return Err("cannot encrypt an MLS application with a pending commit".to_string());
        }
        let epoch = group.context().epoch();
        let ciphertext = group
            .encrypt_application_message(&jsony::to_binary(&entry.event), Vec::new())
            .map_err(|error| error.to_string())?
            .to_bytes()
            .map_err(|error| error.to_string())?;
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        entry.state = OutboxState::PendingDelivery {
            epoch,
            ciphertext: ciphertext.clone(),
        };
        self.application
            .insert(&key, &jsony::to_binary(&entry))
            .map_err(|error| error.to_string())?;
        Ok((epoch, ciphertext))
    }

    pub fn mark_outgoing_delivered(
        &self,
        room_id: RoomId,
        event_id: EventId,
        sequence: u64,
    ) -> Result<bool, String> {
        let key = outbox_key(room_id, event_id);
        let mut entry = self.outbox_entry(room_id, event_id)?;
        let cursor = self.cursor(room_id)?;
        let at_ordered_head = cursor.checked_add(1) == Some(sequence);
        let (epoch, ciphertext_hash, dispatch_already_queued) = match entry.state {
            OutboxState::Delivered {
                sequence: stored_sequence,
                epoch,
                ciphertext_hash,
                dispatch_queued,
            } => {
                if stored_sequence != sequence {
                    return Err("MLS event was acknowledged at two delivery sequences".to_string());
                }
                (epoch, ciphertext_hash, dispatch_queued)
            }
            OutboxState::PendingDelivery { epoch, ciphertext } => {
                (epoch, message_hash(&ciphertext), false)
            }
            OutboxState::PendingEncryption => {
                return Err("unencrypted MLS event was acknowledged as delivered".to_string());
            }
        };
        if dispatch_already_queued && cursor < sequence {
            return Err("queued MLS UI dispatch is ahead of the delivery cursor".to_string());
        }
        if !dispatch_already_queued && cursor >= sequence {
            return Err(
                "MLS outbox event lacks a UI dispatch behind the delivery cursor".to_string(),
            );
        }
        let emit_now = at_ordered_head && !dispatch_already_queued;
        debug_assert!(!emit_now || cursor.checked_add(1) == Some(sequence));
        entry.state = OutboxState::Delivered {
            sequence,
            epoch,
            ciphertext_hash,
            dispatch_queued: dispatch_already_queued || emit_now,
        };
        let dispatch = PendingUiDispatch {
            sequence,
            event: entry.event.clone(),
        };
        let encoded = jsony::to_binary(&entry);
        if at_ordered_head {
            self.application
                .transact_insert(&[
                    Item::new(key, encoded),
                    Item::new(cursor_key(room_id), jsony::to_binary(&sequence)),
                    Item::new(
                        ui_dispatch_key(room_id, sequence),
                        jsony::to_binary(&dispatch),
                    ),
                ])
                .map_err(|error| error.to_string())?;
        } else {
            self.application
                .insert(&key, &encoded)
                .map_err(|error| error.to_string())?;
        }
        Ok(emit_now)
    }

    /// Handles `StaleEpochNotStored`, which proves a ciphertext from an epoch
    /// older than `current_epoch` was not appended by the delivery service.
    /// A delayed response must not reset ciphertext already re-encrypted for
    /// the epoch named by that response (or a later epoch).
    pub fn retry_stale_outgoing(
        &self,
        room_id: RoomId,
        event_id: EventId,
        current_epoch: u64,
    ) -> Result<(), String> {
        let key = outbox_key(room_id, event_id);
        let mut entry = self.outbox_entry(room_id, event_id)?;
        match entry.state {
            OutboxState::PendingDelivery { epoch, .. } if epoch < current_epoch => {
                entry.state = OutboxState::PendingEncryption;
                self.application
                    .insert(&key, &jsony::to_binary(&entry))
                    .map_err(|error| error.to_string())?;
            }
            OutboxState::PendingDelivery { .. }
            | OutboxState::PendingEncryption
            | OutboxState::Delivered { .. } => {}
        }
        Ok(())
    }

    pub fn process_delivery(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        delivery: &MlsDeliveryEvent,
    ) -> Result<ProcessedDelivery, String> {
        self.process_delivery_with_hooks(descriptor, delivery, || Ok(()), || Ok(()))
    }

    #[cfg(test)]
    fn process_delivery_with_hook<F>(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        delivery: &MlsDeliveryEvent,
        before_application_group_write: F,
    ) -> Result<ProcessedDelivery, String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        self.process_delivery_with_hooks(
            descriptor,
            delivery,
            before_application_group_write,
            || Ok(()),
        )
    }

    fn process_delivery_with_hooks<F, G>(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        delivery: &MlsDeliveryEvent,
        before_application_group_write: F,
        after_group_write: G,
    ) -> Result<ProcessedDelivery, String>
    where
        F: FnOnce() -> Result<(), String>,
        G: FnOnce() -> Result<(), String>,
    {
        descriptor.validate()?;
        self.install_delivery_rosters(descriptor, delivery)?;
        let sequence = delivery.sequence();
        let cursor = self.cursor(descriptor.room_id)?;
        if sequence <= cursor {
            return Ok(ProcessedDelivery::AlreadyProcessed { sequence });
        }
        let expected_sequence = cursor
            .checked_add(1)
            .ok_or_else(|| "MLS delivery cursor is exhausted".to_string())?;
        if sequence != expected_sequence {
            return Err(format!(
                "MLS delivery gap: expected {}, received {sequence}",
                expected_sequence
            ));
        }
        let mut group = self.load_group(descriptor)?;
        if let MlsDeliveryEvent::Application { epoch, .. } = delivery
            && *epoch != group.context().epoch()
        {
            return Err("ordered MLS application does not belong to the current epoch".to_string());
        }
        if let MlsDeliveryEvent::Application {
            epoch,
            event_id,
            ciphertext,
            ..
        } = delivery
            && let Some(bytes) = self
                .application
                .get(&outbox_key(descriptor.room_id, *event_id))
                .map_err(|error| error.to_string())?
        {
            let entry: OutboxEntry =
                jsony::from_binary(&bytes).map_err(|error| error.to_string())?;
            let (is_own_delivery, should_emit) = match &entry.state {
                OutboxState::PendingDelivery {
                    epoch: stored_epoch,
                    ciphertext: stored_ciphertext,
                } => (
                    stored_epoch == epoch && stored_ciphertext == ciphertext,
                    true,
                ),
                OutboxState::Delivered {
                    sequence: stored_sequence,
                    epoch: stored_epoch,
                    ciphertext_hash,
                    dispatch_queued,
                } => (
                    stored_sequence == &sequence
                        && stored_epoch == epoch
                        && ciphertext_hash == &message_hash(ciphertext),
                    !dispatch_queued,
                ),
                OutboxState::PendingEncryption => (false, false),
            };
            if is_own_delivery {
                let dispatch_queued =
                    self.mark_outgoing_delivered(descriptor.room_id, *event_id, sequence)?;
                return Ok(ProcessedDelivery::Outgoing {
                    sequence,
                    event: (should_emit && dispatch_queued).then_some(entry.event),
                });
            }
        }
        if let MlsDeliveryEvent::Commit {
            parent_epoch,
            epoch,
            ..
        } = delivery
            && parent_epoch.checked_add(1) != Some(*epoch)
        {
            return Err("MLS commit delivery does not advance exactly one epoch".to_string());
        }
        let (encoded, outer_event_id, advertised_epoch) = match delivery {
            MlsDeliveryEvent::Commit { commit, epoch, .. } => (commit, None, *epoch),
            MlsDeliveryEvent::Application {
                ciphertext,
                event_id,
                epoch,
                ..
            } => (ciphertext, Some(*event_id), *epoch),
        };
        let message = MlsMessage::from_bytes(encoded)
            .map_err(|error| format!("invalid MLS delivery: {error}"))?;
        let expected_message_epoch = match delivery {
            MlsDeliveryEvent::Commit { parent_epoch, .. } => *parent_epoch,
            MlsDeliveryEvent::Application { epoch, .. } => *epoch,
        };
        if message.group_id() != Some(descriptor.mls_group_id.as_slice())
            || message.epoch() != Some(expected_message_epoch)
        {
            return Err("MLS delivery envelope does not match the encoded message".to_string());
        }
        let processed = match group.process_incoming_message(message) {
            Ok(processed) => processed,
            Err(error) => {
                if let Some(marker) = self.delivery_marker(descriptor.room_id)?
                    && marker.matches(delivery)
                {
                    match marker {
                        DeliveryMarker::Commit { epoch, .. }
                            if group.context().epoch() == epoch =>
                        {
                            self.set_cursor(descriptor.room_id, sequence)?;
                            self.clear_pending_commit_record(descriptor.room_id)?;
                            return Ok(ProcessedDelivery::Commit { sequence, epoch });
                        }
                        DeliveryMarker::Application { .. } => {
                            let event_id = outer_event_id.ok_or_else(|| {
                                "application delivery marker matched a commit".to_string()
                            })?;
                            let cached = self.cached_event(descriptor.room_id, event_id)?;
                            if let Some(cached) = cached {
                                if cached.sequence != sequence
                                    || cached.event.room_id != descriptor.room_id
                                    || cached.event.event_id != event_id
                                {
                                    return Err(
                                        "cached MLS application does not match its delivery marker"
                                            .to_string(),
                                    );
                                }
                                self.set_cursor_with_dispatch(
                                    descriptor.room_id,
                                    sequence,
                                    &cached.event,
                                )?;
                                return Ok(ProcessedDelivery::Application(cached));
                            }
                            self.set_cursor(descriptor.room_id, sequence)?;
                            return Ok(ProcessedDelivery::AlreadyProcessed { sequence });
                        }
                        DeliveryMarker::Commit { .. } => {}
                    }
                }
                if outer_event_id.is_some() {
                    // The delivery service cannot authenticate PrivateMessage
                    // sender data or detect secret-tree replays. A member can
                    // therefore resubmit a copied ciphertext under another
                    // outer event id. It is not an epoch transition and must
                    // not wedge the ordered cursor for every recipient.
                    self.set_cursor(descriptor.room_id, sequence)?;
                    return Ok(ProcessedDelivery::RejectedApplication {
                        sequence,
                        reason: format!("invalid MLS application: {error}"),
                    });
                }
                return Err(error.to_string());
            }
        };
        match processed {
            ReceivedMessage::ApplicationMessage(application) => {
                let event_id = outer_event_id.ok_or_else(|| {
                    "commit delivery contained an application message".to_string()
                })?;
                let opened = (|| {
                    let member = group
                        .roster()
                        .member_with_index(application.sender_index)
                        .map_err(|error| error.to_string())?;
                    let client_id = member
                        .signing_identity
                        .credential
                        .as_basic()
                        .map(|credential| credential.identifier.clone())
                        .ok_or_else(|| "MLS sender does not use a Basic credential".to_string())?;
                    let sender_account = self
                        .identities
                        .account_for_client(&client_id)
                        .map_err(|error| error.to_string())?;
                    let event: MlsChattEvent = jsony::from_binary(application.data())
                        .map_err(|error| format!("invalid Chatt MLS event: {error}"))?;
                    validate_event(&event, descriptor.room_id, event_id, sender_account)?;
                    Ok::<_, String>(CachedIncomingEvent {
                        sequence,
                        sender_client_id: client_id,
                        event,
                    })
                })();
                // Cache and mark first. If the MLS write fails, reloading the
                // group rolls decryption back and these inserts are harmless.
                if let Ok(cached) = &opened {
                    self.application
                        .insert(
                            &incoming_key(descriptor.room_id, event_id),
                            &jsony::to_binary(cached),
                        )
                        .map_err(|error| error.to_string())?;
                }
                self.store_delivery_marker(descriptor.room_id, delivery)?;
                before_application_group_write()?;
                group
                    .write_to_storage()
                    .map_err(|error| error.to_string())?;
                after_group_write()?;
                if let Ok(cached) = &opened {
                    self.set_cursor_with_dispatch(descriptor.room_id, sequence, &cached.event)?;
                } else {
                    self.set_cursor(descriptor.room_id, sequence)?;
                }
                match opened {
                    Ok(cached) => Ok(ProcessedDelivery::Application(cached)),
                    Err(reason) => Ok(ProcessedDelivery::RejectedApplication { sequence, reason }),
                }
            }
            ReceivedMessage::Commit(commit) => {
                if outer_event_id.is_some() {
                    return Err("application delivery contained an MLS commit".to_string());
                }
                let epoch = group.context().epoch();
                let effect = match &commit.effect {
                    CommitEffect::NewEpoch(_) => "new epoch",
                    CommitEffect::Removed { .. } => "removed",
                    _ => "unsupported",
                };
                if epoch != advertised_epoch || effect == "unsupported" {
                    return Err(format!(
                        "MLS commit delivery epoch does not match its result (local {epoch}, advertised {advertised_epoch}, effect {effect})"
                    ));
                }
                self.store_delivery_marker(descriptor.room_id, delivery)?;
                group
                    .write_to_storage()
                    .map_err(|error| error.to_string())?;
                after_group_write()?;
                self.set_cursor(descriptor.room_id, sequence)?;
                self.clear_pending_commit_record(descriptor.room_id)?;
                Ok(ProcessedDelivery::Commit { sequence, epoch })
            }
            _ => Err("ordered MLS stream contained an unsupported message type".to_string()),
        }
    }

    pub fn cursor(&self, room_id: RoomId) -> Result<u64, String> {
        self.application
            .get(&cursor_key(room_id))
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()
            .map(|cursor| cursor.unwrap_or(0))
    }

    pub fn welcome_cursor(&self) -> Result<u64, String> {
        self.application
            .get("cursor/welcomes")
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()
            .map(|cursor| cursor.unwrap_or(0))
    }

    pub fn set_welcome_cursor(&self, cursor: u64) -> Result<(), String> {
        if cursor <= self.welcome_cursor()? {
            return Ok(());
        }
        self.application
            .insert("cursor/welcomes", &jsony::to_binary(&cursor))
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn cached_event(
        &self,
        room_id: RoomId,
        event_id: EventId,
    ) -> Result<Option<CachedIncomingEvent>, String> {
        self.application
            .get(&incoming_key(room_id, event_id))
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    /// Normalized local plaintext history. Incoming and outgoing records share
    /// the same event-id namespace and are merged idempotently.
    pub fn cached_history(&self, room_id: RoomId) -> Result<Vec<CachedApplicationEvent>, String> {
        let mut events = HashMap::new();
        for item in self
            .application
            .get_by_prefix(&format!("incoming/{:08x}/", room_id.0))
            .map_err(|error| error.to_string())?
        {
            let cached: CachedIncomingEvent =
                jsony::from_binary(item.value()).map_err(|error| error.to_string())?;
            events.insert(
                cached.event.event_id,
                CachedApplicationEvent {
                    sequence: cached.sequence,
                    event: cached.event,
                },
            );
        }
        for item in self
            .application
            .get_by_prefix(&format!("outbox/{:08x}/", room_id.0))
            .map_err(|error| error.to_string())?
        {
            let outgoing: OutboxEntry =
                jsony::from_binary(item.value()).map_err(|error| error.to_string())?;
            if let OutboxState::Delivered {
                sequence,
                dispatch_queued: true,
                ..
            } = outgoing.state
            {
                events
                    .entry(outgoing.event.event_id)
                    .or_insert(CachedApplicationEvent {
                        sequence,
                        event: outgoing.event,
                    });
            }
        }
        let mut events = events.into_values().collect::<Vec<_>>();
        events.sort_by_key(|event| event.sequence);
        Ok(events)
    }

    /// Undelivered application events across all rooms. Pending ciphertext is
    /// returned byte-for-byte so reconnect never changes an already encrypted
    /// MLS message.
    pub fn pending_outbox(&self) -> Result<Vec<OutboxEntry>, String> {
        let mut entries = self
            .application
            .get_by_prefix("outbox/")
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|item| jsony::from_binary(item.value()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<OutboxEntry>, String>>()?;
        entries.retain(|entry| !matches!(entry.state, OutboxState::Delivered { .. }));
        entries.sort_by_key(|entry| entry.queued_order);
        debug_assert!(
            entries
                .windows(2)
                .all(|entries| { entries[0].queued_order < entries[1].queued_order })
        );
        Ok(entries)
    }

    /// UI work is removed only after the network worker successfully queues it
    /// to the app. Reopening the database therefore yields every cursor-committed
    /// application whose dispatch was interrupted by a crash.
    pub fn pending_ui_dispatches(&self) -> Result<Vec<PendingUiDispatch>, String> {
        let mut pending = self
            .application
            .get_by_prefix("ui-dispatch/")
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|item| jsony::from_binary(item.value()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<PendingUiDispatch>, String>>()?;
        pending.sort_by_key(|dispatch| dispatch.sequence);
        Ok(pending)
    }

    pub fn acknowledge_ui_dispatch(&self, room_id: RoomId, sequence: u64) -> Result<(), String> {
        self.application
            .delete(&ui_dispatch_key(room_id, sequence))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn pending_file_uploads(&self) -> Result<Vec<DurableFileUpload>, String> {
        self.application
            .get_by_prefix("file-upload/")
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|item| jsony::from_binary(item.value()).map_err(|error| error.to_string()))
            .collect()
    }

    pub fn finish_file_upload(&self, room_id: RoomId, event_id: EventId) -> Result<(), String> {
        self.application
            .delete(&file_upload_key(room_id, event_id))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn descriptor(&self, room_id: RoomId) -> Result<Option<EncryptedRoomDescriptor>, String> {
        self.application
            .get(&descriptor_key(room_id))
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    /// Recovers room creation when the server durably accepted the exact
    /// initial commit but its direct response was lost. The canonical
    /// GroupInfo is byte-for-byte the one stored with our pending commit.
    pub fn recover_accepted_room_creation(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        group_info: &[u8],
    ) -> Result<bool, String> {
        let Some(pending) = self.pending_commit(descriptor.room_id)? else {
            return Ok(false);
        };
        if pending.parent_epoch != 0
            || pending.epoch != 1
            || pending.descriptor != *descriptor
            || pending.group_info != group_info
        {
            return Ok(false);
        }
        self.accept_pending_commit(descriptor, 1)?;
        Ok(true)
    }

    pub fn descriptors(&self) -> Result<Vec<EncryptedRoomDescriptor>, String> {
        self.application
            .get_by_prefix("descriptor/")
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|item| jsony::from_binary(item.value()).map_err(|error| error.to_string()))
            .collect()
    }

    pub fn prepare_revocation_commit(
        &self,
        descriptor: &EncryptedRoomDescriptor,
    ) -> Result<Option<(u64, MlsCommitBundle)>, String> {
        let mut group = self.load_group(descriptor)?;
        if group.has_pending_commit() {
            return Ok(None);
        }
        let members = group
            .roster()
            .members_iter()
            .map(|member| {
                let client_id = member
                    .signing_identity
                    .credential
                    .as_basic()
                    .map(|credential| credential.identifier.clone())
                    .ok_or_else(|| "MLS member does not use a Basic credential".to_string())?;
                let account = self
                    .identities
                    .account_for_client(&client_id)
                    .map_err(|error| error.to_string())?;
                Ok((member.index, client_id, account))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut account_leaves = HashMap::new();
        for (_, _, account) in &members {
            *account_leaves.entry(*account).or_insert(0usize) += 1;
        }
        let mut removals = Vec::new();
        for (index, client_id, account) in members {
            if !self
                .identities
                .is_client_active(&client_id)
                .map_err(|error| error.to_string())?
                && account_leaves[&account] > 1
            {
                removals.push(index);
                *account_leaves.get_mut(&account).unwrap() -= 1;
            }
        }
        if removals.is_empty() {
            return Ok(None);
        }
        let epoch = group.context().epoch();
        let mut builder = group.commit_builder();
        for index in removals {
            builder = builder
                .remove_member(index)
                .map_err(|error| error.to_string())?;
        }
        let output = builder.build().map_err(|error| error.to_string())?;
        let group_info = output
            .external_commit_group_info
            .as_ref()
            .ok_or_else(|| "MLS runtime did not produce a resulting GroupInfo".to_string())?
            .to_bytes()
            .map_err(|error| error.to_string())?;
        let commit = output
            .commit_message
            .to_bytes()
            .map_err(|error| error.to_string())?;
        self.store_pending_commit(&PendingCommitRecord {
            descriptor: descriptor.clone(),
            parent_epoch: epoch,
            epoch: epoch
                .checked_add(1)
                .ok_or_else(|| "MLS epoch is exhausted".to_string())?,
            commit: commit.clone(),
            group_info: group_info.clone(),
        })?;
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        Ok(Some((
            epoch,
            MlsCommitBundle {
                prior_group_info: None,
                commit,
                welcome: None,
                group_info,
            },
        )))
    }

    pub fn has_pending_commit(&self, descriptor: &EncryptedRoomDescriptor) -> Result<bool, String> {
        self.load_group(descriptor)
            .map(|group| group.has_pending_commit())
    }

    pub fn outbox(&self, room_id: RoomId, event_id: EventId) -> Result<OutboxEntry, String> {
        self.outbox_entry(room_id, event_id)
    }

    fn load_group(
        &self,
        descriptor: &EncryptedRoomDescriptor,
    ) -> Result<mls_rs::group::Group<PersistentConfig>, String> {
        self.identities
            .install_room(descriptor.clone())
            .map_err(|error| error.to_string())?;
        self.client
            .load_group(&descriptor.mls_group_id)
            .map_err(|error| error.to_string())
    }

    fn install_delivery_rosters(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        delivery: &MlsDeliveryEvent,
    ) -> Result<(), String> {
        let rosters = match delivery {
            MlsDeliveryEvent::Commit { rosters, .. }
            | MlsDeliveryEvent::Application { rosters, .. } => rosters,
        };
        // The server emits snapshots in the descriptor's canonical account
        // order. Validate that order directly instead of allocating and
        // sorting for every event in a delivery batch.
        if rosters.len() != descriptor.member_accounts.len()
            || rosters
                .iter()
                .map(|roster| roster.body.account_id)
                .ne(descriptor.member_accounts.iter().copied())
        {
            return Err("MLS delivery roster snapshots do not match the room".to_string());
        }
        for roster in rosters {
            self.identities
                .install_historical_roster(roster)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn outbox_entry(&self, room_id: RoomId, event_id: EventId) -> Result<OutboxEntry, String> {
        let bytes = self
            .application
            .get(&outbox_key(room_id, event_id))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "MLS outbox event does not exist".to_string())?;
        jsony::from_binary(&bytes).map_err(|error| error.to_string())
    }

    fn set_cursor(&self, room_id: RoomId, sequence: u64) -> Result<(), String> {
        let current = self.cursor(room_id)?;
        if sequence < current {
            return Err(format!(
                "MLS delivery cursor cannot regress from {current} to {sequence}"
            ));
        }
        if sequence > current && current.checked_add(1) != Some(sequence) {
            return Err(format!(
                "MLS delivery cursor cannot skip from {current} to {sequence}"
            ));
        }
        self.application
            .transact_insert(&[Item::new(cursor_key(room_id), jsony::to_binary(&sequence))])
            .map_err(|error| error.to_string())?;
        debug_assert_eq!(self.cursor(room_id).ok(), Some(sequence));
        Ok(())
    }

    fn set_cursor_with_dispatch(
        &self,
        room_id: RoomId,
        sequence: u64,
        event: &MlsChattEvent,
    ) -> Result<(), String> {
        let current = self.cursor(room_id)?;
        if current.checked_add(1) != Some(sequence) {
            return Err(format!(
                "MLS UI dispatch cursor cannot skip from {current} to {sequence}"
            ));
        }
        let dispatch = PendingUiDispatch {
            sequence,
            event: event.clone(),
        };
        self.application
            .transact_insert(&[
                Item::new(cursor_key(room_id), jsony::to_binary(&sequence)),
                Item::new(
                    ui_dispatch_key(room_id, sequence),
                    jsony::to_binary(&dispatch),
                ),
            ])
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn delivery_marker(&self, room_id: RoomId) -> Result<Option<DeliveryMarker>, String> {
        self.application
            .get(&delivery_marker_key(room_id))
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    fn store_delivery_marker(
        &self,
        room_id: RoomId,
        delivery: &MlsDeliveryEvent,
    ) -> Result<(), String> {
        let marker = DeliveryMarker::from_delivery(delivery);
        self.application
            .insert(&delivery_marker_key(room_id), &jsony::to_binary(&marker))
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn pending_commit(&self, room_id: RoomId) -> Result<Option<PendingCommitRecord>, String> {
        self.application
            .get(&pending_commit_key(room_id))
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()
    }

    fn store_pending_commit(&self, pending: &PendingCommitRecord) -> Result<(), String> {
        self.application
            .insert(
                &pending_commit_key(pending.descriptor.room_id),
                &jsony::to_binary(pending),
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn clear_pending_commit_record(&self, room_id: RoomId) -> Result<(), String> {
        self.application
            .delete(&pending_commit_key(room_id))
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn store_descriptor(&self, descriptor: &EncryptedRoomDescriptor) -> Result<(), String> {
        match self.descriptor(descriptor.room_id)? {
            Some(current) if current != *descriptor => {
                Err("encrypted room descriptor is immutable".to_string())
            }
            Some(_) => Ok(()),
            None => {
                self.application
                    .insert(
                        &descriptor_key(descriptor.room_id),
                        &jsony::to_binary(descriptor),
                    )
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
        }
    }

    fn store_descriptor_with_cursor(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        sequence: u64,
    ) -> Result<(), String> {
        if let Some(current) = self.descriptor(descriptor.room_id)?
            && current != *descriptor
        {
            return Err("encrypted room descriptor is immutable".to_string());
        }
        let current = self.cursor(descriptor.room_id)?;
        if sequence < current {
            return Err(format!(
                "MLS room installation cannot regress cursor from {current} to {sequence}"
            ));
        }
        self.application
            .transact_insert(&[
                Item::new(
                    descriptor_key(descriptor.room_id),
                    jsony::to_binary(descriptor),
                ),
                Item::new(cursor_key(descriptor.room_id), jsony::to_binary(&sequence)),
            ])
            .map_err(|error| error.to_string())?;
        debug_assert_eq!(self.cursor(descriptor.room_id).ok(), Some(sequence));
        Ok(())
    }
}

fn validate_event(
    event: &MlsChattEvent,
    room_id: RoomId,
    event_id: EventId,
    sender_account: AccountId,
) -> Result<(), String> {
    event.validate()?;
    if event.room_id != room_id
        || event.event_id != event_id
        || event.sender_account != sender_account
    {
        return Err("Chatt MLS event context is invalid".to_string());
    }
    Ok(())
}

fn open_storage(path: &Path) -> Result<RedbDataStorageEngine, String> {
    RedbDataStorageEngine::open(path).map_err(|error| error.to_string())
}

fn cursor_key(room_id: RoomId) -> String {
    format!("cursor/{:08x}", room_id.0)
}

fn descriptor_key(room_id: RoomId) -> String {
    format!("descriptor/{:08x}", room_id.0)
}

fn incoming_key(room_id: RoomId, event_id: EventId) -> String {
    format!("incoming/{:08x}/{}", room_id.0, hex(&event_id.0))
}

fn outbox_key(room_id: RoomId, event_id: EventId) -> String {
    format!("outbox/{:08x}/{}", room_id.0, hex(&event_id.0))
}

fn file_upload_key(room_id: RoomId, event_id: EventId) -> String {
    format!("file-upload/{:08x}/{}", room_id.0, hex(&event_id.0))
}

fn ui_dispatch_key(room_id: RoomId, sequence: u64) -> String {
    format!("ui-dispatch/{:08x}/{sequence:016x}", room_id.0)
}

fn delivery_marker_key(room_id: RoomId) -> String {
    format!("delivery-marker/{:08x}", room_id.0)
}

fn pending_commit_key(room_id: RoomId) -> String {
    format!("pending-commit/{:08x}", room_id.0)
}

fn outbox_order_key() -> &'static str {
    "counter/outbox-order"
}

impl DeliveryMarker {
    fn from_delivery(delivery: &MlsDeliveryEvent) -> Self {
        match delivery {
            MlsDeliveryEvent::Application {
                sequence,
                event_id,
                ciphertext,
                ..
            } => Self::Application {
                sequence: *sequence,
                event_id: *event_id,
                message_hash: message_hash(ciphertext),
            },
            MlsDeliveryEvent::Commit {
                sequence,
                parent_epoch,
                epoch,
                commit,
                ..
            } => Self::Commit {
                sequence: *sequence,
                parent_epoch: *parent_epoch,
                epoch: *epoch,
                message_hash: message_hash(commit),
            },
        }
    }

    fn matches(&self, delivery: &MlsDeliveryEvent) -> bool {
        self == &Self::from_delivery(delivery)
    }
}

fn message_hash(message: &[u8]) -> [u8; 32] {
    let mut hash = [0; 32];
    hash.copy_from_slice(digest::digest(&digest::SHA256, message).as_ref());
    hash
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[(byte >> 4) as usize] as char);
        encoded.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use rpc::{
        ids::{EventId, RoomId, UserId},
        mls::{
            ChattEventContent, EncryptedRoomDescriptor, MLS_PROTOCOL_VERSION, MlsChattEvent,
            MlsDeliveryEvent, MlsWelcome,
        },
    };

    use crate::{LocalInstallation, ProcessedDelivery};

    fn delivery_rosters(
        installations: &[&LocalInstallation],
    ) -> Vec<rpc::identity::SignedDeviceRoster> {
        let mut rosters = installations
            .iter()
            .map(|installation| installation.bootstrap.own_roster.clone())
            .collect::<Vec<_>>();
        rosters.sort_by_key(|roster| roster.body.account_id);
        rosters
    }

    #[test]
    fn receiver_crash_after_plaintext_cache_reprocesses_once() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [41; 32];
        let (alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("alice"),
            server_id,
            UserId(1),
            "Alice",
        )
        .unwrap();
        let (bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("bob"),
            server_id,
            UserId(2),
            "Bob",
        )
        .unwrap();
        alice.install_roster(&bob.bootstrap.own_roster).unwrap();
        bob.install_roster(&alice.bootstrap.own_roster).unwrap();
        let descriptor = EncryptedRoomDescriptor::new(
            RoomId(91),
            alice.bootstrap.account_id,
            vec![alice.bootstrap.account_id, bob.bootstrap.account_id],
            1,
        )
        .unwrap();
        let package = bob
            .client
            .generate_key_packages(bob.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        let bundle = alice
            .client
            .create_room(&descriptor, &[(bob.bootstrap.device_id, package.package)])
            .unwrap();
        alice.client.accept_pending_commit(&descriptor, 1).unwrap();
        let shared = bundle.welcome.as_ref().unwrap();
        bob.client
            .join_welcome(
                &descriptor,
                &MlsWelcome {
                    delivery_id: 1,
                    sequence: 1,
                    device_id: bob.bootstrap.device_id,
                    descriptor: shared.descriptor.clone(),
                    welcome: shared.welcome.clone(),
                },
            )
            .unwrap();

        let event = MlsChattEvent {
            version: MLS_PROTOCOL_VERSION,
            room_id: descriptor.room_id,
            event_id: EventId([42; 16]),
            sender_account: alice.bootstrap.account_id,
            timestamp_ms: 2,
            content: ChattEventContent::Text {
                body: "cache before ratchet".to_string(),
            },
        };
        alice.client.queue_outgoing(event.clone()).unwrap();
        let (epoch, ciphertext) = alice
            .client
            .encrypt_outgoing(&descriptor, event.event_id)
            .unwrap();
        let delivery = MlsDeliveryEvent::Application {
            sequence: 2,
            epoch,
            event_id: event.event_id,
            rosters: delivery_rosters(&[&alice, &bob]),
            ciphertext,
        };

        let mut mismatched_envelope = delivery.clone();
        let MlsDeliveryEvent::Application { epoch, .. } = &mut mismatched_envelope else {
            unreachable!()
        };
        *epoch += 1;
        assert_eq!(
            bob.client
                .process_delivery(&descriptor, &mismatched_envelope),
            Err("ordered MLS application does not belong to the current epoch".to_string())
        );
        assert_eq!(bob.client.cursor(descriptor.room_id).unwrap(), 1);

        assert_eq!(
            bob.client
                .process_delivery_with_hook(&descriptor, &delivery, || Err(
                    "injected receiver crash".to_string()
                ),),
            Err("injected receiver crash".to_string()),
        );
        assert_eq!(bob.client.cursor(descriptor.room_id).unwrap(), 1);
        assert_eq!(
            bob.client
                .cached_event(descriptor.room_id, event.event_id)
                .unwrap()
                .unwrap()
                .event,
            event,
        );
        assert!(matches!(
            bob.client.process_delivery(&descriptor, &delivery).unwrap(),
            ProcessedDelivery::Application(_)
        ));
        assert!(matches!(
            bob.client.process_delivery(&descriptor, &delivery).unwrap(),
            ProcessedDelivery::AlreadyProcessed { sequence: 2 }
        ));
        let crash_after_write = MlsChattEvent {
            event_id: EventId([46; 16]),
            timestamp_ms: 3,
            content: ChattEventContent::Text {
                body: "recover dispatch after ratchet write".to_string(),
            },
            ..event.clone()
        };
        alice
            .client
            .queue_outgoing(crash_after_write.clone())
            .unwrap();
        let (epoch, ciphertext) = alice
            .client
            .encrypt_outgoing(&descriptor, crash_after_write.event_id)
            .unwrap();
        let crash_after_write_delivery = MlsDeliveryEvent::Application {
            sequence: 3,
            epoch,
            event_id: crash_after_write.event_id,
            rosters: delivery_rosters(&[&alice, &bob]),
            ciphertext,
        };
        assert_eq!(
            bob.client.process_delivery_with_hooks(
                &descriptor,
                &crash_after_write_delivery,
                || Ok(()),
                || Err("injected crash after application group write".to_string()),
            ),
            Err("injected crash after application group write".to_string()),
        );
        assert_eq!(bob.client.cursor(descriptor.room_id).unwrap(), 2);
        assert!(matches!(
            bob.client
                .process_delivery(&descriptor, &crash_after_write_delivery)
                .unwrap(),
            ProcessedDelivery::Application(cached) if cached.event == crash_after_write
        ));
        assert_eq!(bob.client.cursor(descriptor.room_id).unwrap(), 3);
        let mismatched = MlsChattEvent {
            event_id: EventId([43; 16]),
            timestamp_ms: 4,
            content: ChattEventContent::Text {
                body: "valid MLS, mismatched envelope".to_string(),
            },
            ..event.clone()
        };
        alice.client.queue_outgoing(mismatched.clone()).unwrap();
        let (epoch, ciphertext) = alice
            .client
            .encrypt_outgoing(&descriptor, mismatched.event_id)
            .unwrap();
        assert!(matches!(
            bob.client
                .process_delivery(
                    &descriptor,
                    &MlsDeliveryEvent::Application {
                        sequence: 4,
                        epoch,
                        event_id: EventId([44; 16]),
                        rosters: delivery_rosters(&[&alice, &bob]),
                        ciphertext,
                    },
                )
                .unwrap(),
            ProcessedDelivery::RejectedApplication { sequence: 4, .. }
        ));
        assert_eq!(bob.client.cursor(descriptor.room_id).unwrap(), 4);
        let MlsDeliveryEvent::Application {
            ciphertext: replayed,
            ..
        } = &delivery
        else {
            unreachable!()
        };
        assert!(matches!(
            bob.client
                .process_delivery(
                    &descriptor,
                    &MlsDeliveryEvent::Application {
                        sequence: 5,
                        epoch,
                        event_id: EventId([45; 16]),
                        rosters: delivery_rosters(&[&alice, &bob]),
                        ciphertext: replayed.clone(),
                    },
                )
                .unwrap(),
            ProcessedDelivery::RejectedApplication { sequence: 5, .. }
        ));
        assert_eq!(bob.client.cursor(descriptor.room_id).unwrap(), 5);
        let duplicate_commit = MlsDeliveryEvent::Commit {
            sequence: 5,
            parent_epoch: epoch,
            epoch: epoch + 1,
            rosters: delivery_rosters(&[&alice, &bob]),
            commit: vec![0xff],
        };
        assert!(matches!(
            bob.client
                .process_delivery(&descriptor, &duplicate_commit)
                .unwrap(),
            ProcessedDelivery::AlreadyProcessed { sequence: 5 }
        ));
        let history = bob.client.cached_history(descriptor.room_id).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].sequence, 2);
        assert_eq!(history[0].event, event);
        assert_eq!(history[1].sequence, 3);
        assert_eq!(history[1].event, crash_after_write);
    }

    #[test]
    fn receiver_crash_after_commit_write_recovers_cursor_from_exact_marker() {
        let temp = tempfile::tempdir().unwrap();
        let server_id = [51; 32];
        let (alice, _) = LocalInstallation::open_or_create(
            &temp.path().join("alice"),
            server_id,
            UserId(1),
            "Alice",
        )
        .unwrap();
        let (bob, _) = LocalInstallation::open_or_create(
            &temp.path().join("bob"),
            server_id,
            UserId(2),
            "Bob",
        )
        .unwrap();
        alice.install_roster(&bob.bootstrap.own_roster).unwrap();
        bob.install_roster(&alice.bootstrap.own_roster).unwrap();
        let descriptor = EncryptedRoomDescriptor::new(
            RoomId(92),
            alice.bootstrap.account_id,
            vec![alice.bootstrap.account_id, bob.bootstrap.account_id],
            1,
        )
        .unwrap();
        let package = bob
            .client
            .generate_key_packages(bob.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        let initial = alice
            .client
            .create_room(&descriptor, &[(bob.bootstrap.device_id, package.package)])
            .unwrap();
        alice.client.accept_pending_commit(&descriptor, 1).unwrap();
        let shared = initial.welcome.as_ref().unwrap();
        bob.client
            .join_welcome(
                &descriptor,
                &MlsWelcome {
                    delivery_id: 1,
                    sequence: 1,
                    device_id: bob.bootstrap.device_id,
                    descriptor: shared.descriptor.clone(),
                    welcome: shared.welcome.clone(),
                },
            )
            .unwrap();
        let (_, rejoin) = bob
            .client
            .prepare_external_rejoin(&descriptor, &initial.group_info)
            .unwrap();
        let delivery = MlsDeliveryEvent::Commit {
            sequence: 2,
            parent_epoch: 1,
            epoch: 2,
            rosters: delivery_rosters(&[&alice, &bob]),
            commit: rejoin.commit,
        };
        assert_eq!(
            alice.client.process_delivery_with_hooks(
                &descriptor,
                &delivery,
                || Ok(()),
                || Err("injected crash after commit write".to_string()),
            ),
            Err("injected crash after commit write".to_string())
        );
        assert_eq!(alice.client.cursor(descriptor.room_id).unwrap(), 1);
        assert!(matches!(
            alice
                .client
                .process_delivery(&descriptor, &delivery)
                .unwrap(),
            ProcessedDelivery::Commit {
                sequence: 2,
                epoch: 2
            }
        ));
        assert_eq!(alice.client.cursor(descriptor.room_id).unwrap(), 2);
    }
}
