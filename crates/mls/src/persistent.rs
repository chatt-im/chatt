//! SQLCipher-backed MLS state and application event cache.
//!
//! `mls-rs` deliberately keeps a processed group's mutations in memory until
//! `write_to_storage`. That lets this wrapper durably insert decrypted
//! plaintext first, persist the MLS ratchet second, and advance the delivery
//! cursor last.

use std::{
    collections::HashMap,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::Mutex,
};

use jsony::Jsony;
use mls_rs::{
    CipherSuiteProvider, Client, CryptoProvider, ExtensionList, MlsMessage,
    client_builder::{
        BaseSqlConfig, ClientBuilder, WithCryptoProvider, WithIdentityProvider, WithMlsRules,
    },
    group::{CommitEffect, ReceivedMessage},
    identity::{SigningIdentity, basic::BasicCredential},
};
use mls_rs_core::crypto::{SignaturePublicKey, SignatureSecretKey};
use mls_rs_crypto_rustcrypto::RustCryptoProvider;
use mls_rs_provider_sqlite::{
    JournalMode, SqLiteDataStorageEngine, SqLiteDataStorageError,
    connection_strategy::{
        CipheredConnectionStrategy, ConnectionStrategy, SqlCipherConfig, SqlCipherKey,
    },
    storage::{Item, SqLiteApplicationStorage},
};
use rusqlite::Connection;
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
        WithCryptoProvider<RustCryptoProvider, BaseSqlConfig>,
    >,
>;

type SqlCipherStorage =
    SqLiteDataStorageEngine<CipheredConnectionStrategy<ChattFileConnectionStrategy>>;

struct ChattFileConnectionStrategy {
    path: PathBuf,
}

impl ChattFileConnectionStrategy {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }
}

impl ConnectionStrategy for ChattFileConnectionStrategy {
    fn make_connection(&self) -> Result<Connection, SqLiteDataStorageError> {
        let connection = Connection::open(&self.path)
            .map_err(|error| SqLiteDataStorageError::SqlEngineError(error.into()))?;
        // SQLCipher logs every denied `mlock` as an error even though it
        // deliberately continues with ordinary guarded allocations. Sandboxed
        // clients and tests commonly lack CAP_IPC_LOCK, turning each database
        // open into hundreds of misleading stderr lines. SQLCipher also writes
        // expected wrong-key probes straight to stderr. Disable its global
        // diagnostic sink; database and cryptographic failures still propagate
        // through the Rust result returned by each operation.
        connection
            .pragma_update(None, "cipher_log_source", "NONE")
            .map_err(|error| SqLiteDataStorageError::SqlEngineError(error.into()))?;
        Ok(connection)
    }
}

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

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct OutboxEntry {
    pub event: MlsChattEvent,
    pub state: OutboxState,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub enum OutboxState {
    PendingEncryption,
    PendingDelivery { epoch: u64, ciphertext: Vec<u8> },
    Delivered { sequence: u64 },
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
    Commit { sequence: u64, epoch: u64 },
    AlreadyProcessed { sequence: u64 },
}

/// One installation's MLS client and encrypted application cache.
pub struct PersistentClient {
    client: Client<PersistentConfig>,
    application: SqLiteApplicationStorage,
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
        database_key: [u8; 32],
        identities: ChattIdentityProvider,
        signing_identity: SigningIdentity,
        signing_secret: SignatureSecretKey,
    ) -> Result<Self, String> {
        let storage = open_storage(path, database_key)?;
        let application = storage
            .application_data_storage()
            .map_err(|error| error.to_string())?;
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
                    return Err("SQLCipher database belongs to another MLS credential".to_string());
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

    pub fn reopen(
        path: &Path,
        database_key: [u8; 32],
        identities: ChattIdentityProvider,
    ) -> Result<Self, String> {
        let storage = open_storage(path, database_key)?;
        let application = storage
            .application_data_storage()
            .map_err(|error| error.to_string())?;
        let bytes = application
            .get(SIGNING_MATERIAL_KEY)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "SQLCipher database has no MLS signing material".to_string())?;
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
        storage: SqlCipherStorage,
        application: SqLiteApplicationStorage,
        identities: ChattIdentityProvider,
        signing_identity: SigningIdentity,
        signing_secret: SignatureSecretKey,
    ) -> Result<Self, String> {
        let client = ClientBuilder::<BaseSqlConfig>::new_sqlite(storage)
            .map_err(|error| error.to_string())?
            .crypto_provider(RustCryptoProvider::default())
            .identity_provider(identities.clone())
            .mls_rules(ChattMlsPolicy::new(identities.clone()))
            .signing_identity(signing_identity, signing_secret.clone(), CIPHER_SUITE)
            .build();
        Ok(Self {
            client,
            application,
            identities,
            signing_secret,
            pending_external_rejoins: Mutex::new(HashMap::new()),
        })
    }

    pub fn sign_device_binding(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        RustCryptoProvider::default()
            .cipher_suite_provider(CIPHER_SUITE)
            .ok_or_else(|| "mandatory MLS cipher suite is unavailable".to_string())?
            .sign(&self.signing_secret, message)
            .map_err(|error| error.to_string())
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
        let mut group = self.load_group(descriptor)?;
        if !group.has_pending_commit() {
            return Err("MLS group has no pending commit to accept".to_string());
        }
        group
            .apply_pending_commit()
            .map_err(|error| error.to_string())?;
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        self.store_descriptor(descriptor)?;
        self.set_cursor(descriptor.room_id, sequence)
    }

    pub fn reject_pending_commit(
        &self,
        descriptor: &EncryptedRoomDescriptor,
    ) -> Result<(), String> {
        let mut group = self.load_group(descriptor)?;
        group.clear_pending_commit();
        group.write_to_storage().map_err(|error| error.to_string())
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
        self.store_descriptor(descriptor)?;
        self.set_cursor(descriptor.room_id, welcome.sequence)
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
        let validator = PublicGroupValidator::new(self.identities.clone());
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
        if group.context().group_id() != descriptor.mls_group_id
            || group.context().epoch() != observed.epoch.saturating_add(1)
        {
            return Err("external rejoin produced an unexpected MLS group state".to_string());
        }
        let resulting_group_info = group
            .group_info_message_allowing_ext_commit_with_extensions(
                true,
                ExtensionList::new(),
            )
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
        let mut group = self
            .pending_external_rejoins
            .lock()
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())?
            .remove(&descriptor.room_id)
            .ok_or_else(|| "MLS room has no pending external rejoin".to_string())?;
        group
            .write_to_storage()
            .map_err(|error| error.to_string())?;
        self.store_descriptor(descriptor)?;
        self.set_cursor(descriptor.room_id, sequence)
    }

    pub fn reject_external_rejoin(&self, room_id: RoomId) -> Result<(), String> {
        self.pending_external_rejoins
            .lock()
            .map_err(|_| "pending external rejoin lock is poisoned".to_string())?
            .remove(&room_id);
        Ok(())
    }

    pub fn queue_outgoing(&self, event: MlsChattEvent) -> Result<(), String> {
        validate_event(&event, event.room_id, event.event_id, event.sender_account)?;
        let key = outbox_key(event.room_id, event.event_id);
        if self
            .application
            .get(&key)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("outgoing MLS event id already exists".to_string());
        }
        self.application
            .insert(
                &key,
                &jsony::to_binary(&OutboxEntry {
                    event,
                    state: OutboxState::PendingEncryption,
                }),
            )
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
    ) -> Result<(), String> {
        let key = outbox_key(room_id, event_id);
        let mut entry = self.outbox_entry(room_id, event_id)?;
        entry.state = OutboxState::Delivered { sequence };
        self.application
            .insert(&key, &jsony::to_binary(&entry))
            .map_err(|error| error.to_string())?;
        let cursor = self.cursor(room_id)?;
        if sequence == cursor + 1 {
            self.set_cursor(room_id, sequence)?;
        }
        Ok(())
    }

    /// Called only after `StaleEpochNotStored`, which proves the old exact
    /// ciphertext was not appended by the delivery service.
    pub fn retry_stale_outgoing(&self, room_id: RoomId, event_id: EventId) -> Result<(), String> {
        let key = outbox_key(room_id, event_id);
        let mut entry = self.outbox_entry(room_id, event_id)?;
        if !matches!(entry.state, OutboxState::PendingDelivery { .. }) {
            return Err(
                "only a pending-delivery event can be retried after a stale epoch".to_string(),
            );
        }
        entry.state = OutboxState::PendingEncryption;
        self.application
            .insert(&key, &jsony::to_binary(&entry))
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn process_delivery(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        delivery: &MlsDeliveryEvent,
    ) -> Result<ProcessedDelivery, String> {
        self.process_delivery_with_hook(descriptor, delivery, || Ok(()))
    }

    fn process_delivery_with_hook<F>(
        &self,
        descriptor: &EncryptedRoomDescriptor,
        delivery: &MlsDeliveryEvent,
        before_application_group_write: F,
    ) -> Result<ProcessedDelivery, String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        descriptor.validate()?;
        let sequence = delivery.sequence();
        let cursor = self.cursor(descriptor.room_id)?;
        if sequence <= cursor {
            return Ok(ProcessedDelivery::AlreadyProcessed { sequence });
        }
        if sequence != cursor + 1 {
            return Err(format!(
                "MLS delivery gap: expected {}, received {sequence}",
                cursor + 1
            ));
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
                } => (stored_sequence == &sequence, false),
                OutboxState::PendingEncryption => (false, false),
            };
            if is_own_delivery {
                self.mark_outgoing_delivered(descriptor.room_id, *event_id, sequence)?;
                return Ok(ProcessedDelivery::Outgoing {
                    sequence,
                    event: should_emit.then_some(entry.event),
                });
            }
        }
        let mut group = self.load_group(descriptor)?;
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
        let processed = match group.process_incoming_message(message) {
            Ok(processed) => processed,
            Err(error) => {
                if let Some(event_id) = outer_event_id
                    && self.cached_event(descriptor.room_id, event_id)?.is_some()
                {
                    self.set_cursor(descriptor.room_id, sequence)?;
                    return Ok(ProcessedDelivery::AlreadyProcessed { sequence });
                }
                return Err(error.to_string());
            }
        };
        match processed {
            ReceivedMessage::ApplicationMessage(application) => {
                let event_id = outer_event_id.ok_or_else(|| {
                    "commit delivery contained an application message".to_string()
                })?;
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
                let cached = CachedIncomingEvent {
                    sequence,
                    sender_client_id: client_id,
                    event,
                };
                // Cache first. If the MLS write fails, reloading the group
                // rolls decryption back and this idempotent insert is harmless.
                self.application
                    .insert(
                        &incoming_key(descriptor.room_id, event_id),
                        &jsony::to_binary(&cached),
                    )
                    .map_err(|error| error.to_string())?;
                before_application_group_write()?;
                group
                    .write_to_storage()
                    .map_err(|error| error.to_string())?;
                self.set_cursor(descriptor.room_id, sequence)?;
                Ok(ProcessedDelivery::Application(cached))
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
                group
                    .write_to_storage()
                    .map_err(|error| error.to_string())?;
                self.set_cursor(descriptor.room_id, sequence)?;
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
    pub fn cached_history(&self, room_id: RoomId) -> Result<Vec<MlsChattEvent>, String> {
        let mut events = HashMap::new();
        for item in self
            .application
            .get_by_prefix(&format!("incoming/{:08x}/", room_id.0))
            .map_err(|error| error.to_string())?
        {
            let cached: CachedIncomingEvent =
                jsony::from_binary(item.value()).map_err(|error| error.to_string())?;
            events.insert(cached.event.event_id, cached.event);
        }
        for item in self
            .application
            .get_by_prefix(&format!("outbox/{:08x}/", room_id.0))
            .map_err(|error| error.to_string())?
        {
            let outgoing: OutboxEntry =
                jsony::from_binary(item.value()).map_err(|error| error.to_string())?;
            events
                .entry(outgoing.event.event_id)
                .or_insert(outgoing.event);
        }
        let mut events = events.into_values().collect::<Vec<_>>();
        events.sort_by_key(|event| (event.timestamp_ms, event.event_id.0));
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
        entries.sort_by_key(|entry| (entry.event.timestamp_ms, entry.event.event_id.0));
        Ok(entries)
    }

    pub fn descriptor(&self, room_id: RoomId) -> Result<Option<EncryptedRoomDescriptor>, String> {
        self.application
            .get(&descriptor_key(room_id))
            .map_err(|error| error.to_string())?
            .map(|bytes| jsony::from_binary(&bytes).map_err(|error| error.to_string()))
            .transpose()
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
        let mut removals = Vec::new();
        for member in group.roster().members_iter() {
            let client_id = member
                .signing_identity
                .credential
                .as_basic()
                .map(|credential| credential.identifier.as_slice())
                .ok_or_else(|| "MLS member does not use a Basic credential".to_string())?;
            if !self
                .identities
                .is_client_active(client_id)
                .map_err(|error| error.to_string())?
            {
                removals.push(member.index);
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

    fn outbox_entry(&self, room_id: RoomId, event_id: EventId) -> Result<OutboxEntry, String> {
        let bytes = self
            .application
            .get(&outbox_key(room_id, event_id))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "MLS outbox event does not exist".to_string())?;
        jsony::from_binary(&bytes).map_err(|error| error.to_string())
    }

    fn set_cursor(&self, room_id: RoomId, sequence: u64) -> Result<(), String> {
        self.application
            .transact_insert(&[Item::new(cursor_key(room_id), jsony::to_binary(&sequence))])
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

fn prepare_database_file(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("failed to create {}: {error}", path.display())),
    }
}

fn open_storage(path: &Path, database_key: [u8; 32]) -> Result<SqlCipherStorage, String> {
    prepare_database_file(path)?;
    let strategy = CipheredConnectionStrategy::new(
        ChattFileConnectionStrategy::new(path),
        SqlCipherConfig::new(SqlCipherKey::RawKey(database_key)),
    );
    SqLiteDataStorageEngine::new(strategy)
        .map(|storage| storage.with_journal_mode(Some(JournalMode::Wal)))
        .map_err(|error| error.to_string())
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
            .create_room(
                &descriptor,
                &[(bob.bootstrap.device_id, package.package)],
            )
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
            ciphertext,
        };

        assert_eq!(
            bob.client.process_delivery_with_hook(
                &descriptor,
                &delivery,
                || Err("injected receiver crash".to_string()),
            ),
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
        let duplicate_commit = MlsDeliveryEvent::Commit {
            sequence: 2,
            parent_epoch: epoch,
            epoch: epoch + 1,
            commit: vec![0xff],
        };
        assert!(matches!(
            bob.client
                .process_delivery(&descriptor, &duplicate_commit)
                .unwrap(),
            ProcessedDelivery::AlreadyProcessed { sequence: 2 }
        ));
        assert_eq!(bob.client.cached_history(descriptor.room_id).unwrap().len(), 1);
    }
}
