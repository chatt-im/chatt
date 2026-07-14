//! End-to-end encryption for DM rooms.
//!
//! Accounts have a stable Ed25519 authority and an append-only signed device
//! ledger. Each installation owns independent Ed25519 authoring and X25519
//! delivery keys. A DM event uses one random content key, wraps that key to
//! every active device on both accounts, and signs the complete event header,
//! recipient set, ciphertext, random sender event id, sequence, and
//! predecessor. The server relays [`DmEventEnvelope`] bytes opaquely.

use jsony::Jsony;
use ring::{
    digest, hkdf,
    rand::SecureRandom,
    signature::{self, KeyPair},
};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::control::FileContentEncoding;
use crate::crypto::{
    CryptoError, KEY_LEN, KeyMaterial, TAG_LEN, expand_key, open_in_place_with_aad,
    seal_in_place_append_tag,
};
use crate::ids::{
    AccountId, DeviceId, EventId, LedgerHash, MessageId, RoomId, UserId,
};

pub const E2E_PUBLIC_KEY_LEN: usize = 32;
pub const E2E_SEED_LEN: usize = 32;
pub const DM_SALT_LEN: usize = 32;
/// Chat envelope plaintexts are zero-padded to the next multiple of this, so
/// the server only learns a coarse length class (Signal pads the same way).
pub const DM_PAD_MULTIPLE: usize = 160;
/// Upper bound for an encoded DM event: an 8 KiB body plus recipient wraps,
/// padding, signatures, and encoding, with generous margin.
pub const MAX_DM_ENVELOPE_BYTES: usize = 16 * 1024;
/// Bytes a sealed file chunk adds over its payload: length prefix plus tag.
pub const DM_CHUNK_OVERHEAD: usize = 4 + TAG_LEN;
const DM_ENVELOPE_VERSION: u8 = 1;
const DM_CHUNK_AAD_LABEL: &[u8; 21] = b"chatt e2e dm chunk v2";

pub const ACCOUNT_AUTHORITY_KEY_LEN: usize = 32;
pub const DEVICE_SIGNING_KEY_LEN: usize = 32;
pub const MAX_ACTIVE_ACCOUNT_DEVICES: usize = 16;
pub const MAX_ACCOUNT_KEY_STATEMENTS: usize = 4096;
pub const MAX_DEVICE_NAME_BYTES: usize = 64;
const ACCOUNT_ID_LABEL: &[u8] = b"chatt account identity v1";
const ACCOUNT_STATEMENT_LABEL: &[u8] = b"chatt account key statement v1";

/// Public signing and encryption keys for one independently keyed installation.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DevicePublicKeys {
    pub device_id: DeviceId,
    /// Human-readable installation name, authenticated by the account ledger.
    pub name: String,
    pub key_epoch: u64,
    pub signing_public_key: Vec<u8>,
    pub encryption_public_key: Vec<u8>,
}

/// Final signed sender-chain head for a graceful device retirement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DeviceEventHead {
    pub room_id: RoomId,
    pub sequence: u64,
    pub event_id: EventId,
}

/// One transition in an account's append-only device authorization ledger.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum AccountKeyAction {
    Genesis {
        authority_public_key: Vec<u8>,
        first_device: DevicePublicKeys,
        recovery_bundle_hash: Vec<u8>,
    },
    AddDevice {
        device: DevicePublicKeys,
    },
    RotateDeviceKeys {
        device: DevicePublicKeys,
        previous_key_epoch: u64,
        final_event_heads: Vec<DeviceEventHead>,
    },
    RetireDeviceKey {
        device_id: DeviceId,
        key_epoch: u64,
        final_event_heads: Vec<DeviceEventHead>,
    },
    RevokeDevice {
        device_id: DeviceId,
    },
    RotateAccountAuthority {
        new_authority_public_key: Vec<u8>,
    },
    UpdateRecoveryBundle {
        recovery_bundle_hash: Vec<u8>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct AccountKeyStatementBody {
    pub account_id: AccountId,
    pub account_generation: u64,
    pub roster_epoch: u64,
    pub previous: LedgerHash,
    pub authority_key_epoch: u64,
    pub action: AccountKeyAction,
}

/// A ledger transition signed by the current account authority. Graceful
/// device-key rotations additionally carry the retiring device's signature;
/// authority rotations carry the new authority's signature.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct AccountKeyStatement {
    pub body: AccountKeyStatementBody,
    pub authority_signature: Vec<u8>,
    pub co_signature: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKeyStatus {
    Active,
    Retired,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedDeviceKey {
    pub keys: DevicePublicKeys,
    pub status: DeviceKeyStatus,
    pub introduced_at: u64,
    pub ended_at: Option<u64>,
    pub final_event_heads: Vec<DeviceEventHead>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedAccountLedger {
    pub account_id: AccountId,
    pub account_generation: u64,
    pub roster_epoch: u64,
    pub head: LedgerHash,
    pub authority_key_epoch: u64,
    pub authority_public_key: [u8; ACCOUNT_AUTHORITY_KEY_LEN],
    pub device_keys: Vec<ValidatedDeviceKey>,
    pub recovery_bundle_hash: Vec<u8>,
}

impl ValidatedAccountLedger {
    pub fn active_devices(&self) -> impl Iterator<Item = &DevicePublicKeys> {
        self.device_keys
            .iter()
            .filter(|device| device.status == DeviceKeyStatus::Active)
            .map(|device| &device.keys)
    }

    pub fn device_key(&self, device_id: DeviceId, key_epoch: u64) -> Option<&ValidatedDeviceKey> {
        self.device_keys.iter().find(|device| {
            device.keys.device_id == device_id && device.keys.key_epoch == key_epoch
        })
    }

    /// Validates an entire ledger from its genesis, independently of any
    /// server-maintained current state.
    pub fn validate(
        server_public_key: &[u8],
        user_id: UserId,
        statements: &[AccountKeyStatement],
    ) -> Result<Self, String> {
        if statements.is_empty() {
            return Err("account key ledger is empty".to_string());
        }
        if statements.len() > MAX_ACCOUNT_KEY_STATEMENTS {
            return Err("account key ledger has too many statements".to_string());
        }
        let first = &statements[0];
        let AccountKeyAction::Genesis {
            authority_public_key,
            first_device,
            recovery_bundle_hash,
        } = &first.body.action
        else {
            return Err("account key ledger does not begin with genesis".to_string());
        };
        let authority_public_key = fixed_key::<ACCOUNT_AUTHORITY_KEY_LEN>(
            authority_public_key,
            "account authority public key",
        )?;
        validate_device_public_keys(first_device)?;
        if first.body.account_generation == 0
            || first.body.roster_epoch != 1
            || first.body.previous != LedgerHash::default()
            || first.body.authority_key_epoch != 1
            || first_device.key_epoch != 1
            || recovery_bundle_hash.len() != 32
        {
            return Err("account genesis counters are invalid".to_string());
        }
        let expected_account = account_id(server_public_key, user_id, &authority_public_key);
        if first.body.account_id != expected_account {
            return Err("account genesis identity does not match its context".to_string());
        }
        verify_ed25519(
            &authority_public_key,
            &statement_signing_bytes(&first.body),
            &first.authority_signature,
        )?;
        if first.co_signature.is_some() {
            return Err("account genesis carries an unexpected co-signature".to_string());
        }
        let mut state = Self {
            account_id: expected_account,
            account_generation: first.body.account_generation,
            roster_epoch: 1,
            head: account_statement_hash(first),
            authority_key_epoch: 1,
            authority_public_key,
            device_keys: vec![ValidatedDeviceKey {
                keys: first_device.clone(),
                status: DeviceKeyStatus::Active,
                introduced_at: 1,
                ended_at: None,
                final_event_heads: Vec::new(),
            }],
            recovery_bundle_hash: recovery_bundle_hash.clone(),
        };
        for statement in &statements[1..] {
            state.apply(statement)?;
        }
        Ok(state)
    }

    pub fn apply(&mut self, statement: &AccountKeyStatement) -> Result<(), String> {
        let body = &statement.body;
        if body.account_id != self.account_id
            || body.account_generation != self.account_generation
            || body.roster_epoch != self.roster_epoch + 1
            || body.previous != self.head
            || body.authority_key_epoch != self.authority_key_epoch
        {
            return Err("account key statement does not extend the current head".to_string());
        }
        let signing_bytes = statement_signing_bytes(body);
        verify_ed25519(
            &self.authority_public_key,
            &signing_bytes,
            &statement.authority_signature,
        )?;
        match &body.action {
            AccountKeyAction::Genesis { .. } => {
                return Err("account ledger contains a second genesis".to_string());
            }
            AccountKeyAction::AddDevice { device } => {
                if statement.co_signature.is_some() {
                    return Err("device add carries an unexpected co-signature".to_string());
                }
                validate_device_public_keys(device)?;
                if self
                    .device_keys
                    .iter()
                    .any(|known| known.keys.device_id == device.device_id)
                {
                    return Err("device id has already appeared in this account".to_string());
                }
                self.reject_reused_key_material(device)?;
                if self.active_devices().count() == MAX_ACTIVE_ACCOUNT_DEVICES {
                    return Err("account has too many active devices".to_string());
                }
                self.device_keys.push(ValidatedDeviceKey {
                    keys: device.clone(),
                    status: DeviceKeyStatus::Active,
                    introduced_at: body.roster_epoch,
                    ended_at: None,
                    final_event_heads: Vec::new(),
                });
            }
            AccountKeyAction::RotateDeviceKeys {
                device,
                previous_key_epoch,
                final_event_heads,
            } => {
                validate_device_public_keys(device)?;
                if device.key_epoch != previous_key_epoch.saturating_add(1) {
                    return Err("device key rotation skipped an epoch".to_string());
                }
                self.reject_reused_key_material(device)?;
                let previous = self
                    .device_keys
                    .iter_mut()
                    .find(|known| {
                        known.keys.device_id == device.device_id
                            && known.keys.key_epoch == *previous_key_epoch
                    })
                    .ok_or_else(|| "device key rotation has no active predecessor".to_string())?;
                if previous.status != DeviceKeyStatus::Active {
                    return Err("device key rotation predecessor is not active".to_string());
                }
                let co_signature = statement
                    .co_signature
                    .as_deref()
                    .ok_or_else(|| "device key rotation lacks the old device signature".to_string())?;
                verify_ed25519(
                    &fixed_key::<DEVICE_SIGNING_KEY_LEN>(
                        &previous.keys.signing_public_key,
                        "device signing public key",
                    )?,
                    &signing_bytes,
                    co_signature,
                )?;
                previous.status = DeviceKeyStatus::Retired;
                previous.ended_at = Some(body.roster_epoch);
                previous.final_event_heads = final_event_heads.clone();
                self.device_keys.push(ValidatedDeviceKey {
                    keys: device.clone(),
                    status: DeviceKeyStatus::Active,
                    introduced_at: body.roster_epoch,
                    ended_at: None,
                    final_event_heads: Vec::new(),
                });
            }
            AccountKeyAction::RetireDeviceKey {
                device_id,
                key_epoch,
                final_event_heads,
            } => {
                let key = self.active_device_mut(*device_id, *key_epoch)?;
                let co_signature = statement.co_signature.as_deref().ok_or_else(|| {
                    "graceful device retirement lacks the device signature".to_string()
                })?;
                verify_ed25519(
                    &fixed_key::<DEVICE_SIGNING_KEY_LEN>(
                        &key.keys.signing_public_key,
                        "device signing public key",
                    )?,
                    &signing_bytes,
                    co_signature,
                )?;
                key.status = DeviceKeyStatus::Retired;
                key.ended_at = Some(body.roster_epoch);
                key.final_event_heads = final_event_heads.clone();
            }
            AccountKeyAction::RevokeDevice { device_id } => {
                if statement.co_signature.is_some() {
                    return Err("device revocation carries an unexpected co-signature".to_string());
                }
                let mut found = false;
                for key in self
                    .device_keys
                    .iter_mut()
                    .filter(|key| key.keys.device_id == *device_id)
                {
                    found = true;
                    if key.status == DeviceKeyStatus::Active {
                        key.status = DeviceKeyStatus::Revoked;
                        key.ended_at = Some(body.roster_epoch);
                    }
                }
                if !found {
                    return Err("device revocation names an unknown device".to_string());
                }
            }
            AccountKeyAction::RotateAccountAuthority {
                new_authority_public_key,
            } => {
                let new_authority = fixed_key::<ACCOUNT_AUTHORITY_KEY_LEN>(
                    new_authority_public_key,
                    "new account authority public key",
                )?;
                let co_signature = statement.co_signature.as_deref().ok_or_else(|| {
                    "account authority rotation lacks the new authority signature".to_string()
                })?;
                verify_ed25519(&new_authority, &signing_bytes, co_signature)?;
                self.authority_public_key = new_authority;
                self.authority_key_epoch += 1;
            }
            AccountKeyAction::UpdateRecoveryBundle {
                recovery_bundle_hash,
            } => {
                if statement.co_signature.is_some() {
                    return Err(
                        "recovery-bundle update carries an unexpected co-signature".to_string()
                    );
                }
                if recovery_bundle_hash.len() != 32 {
                    return Err("recovery-bundle hash has the wrong length".to_string());
                }
                self.recovery_bundle_hash.clone_from(recovery_bundle_hash);
            }
        }
        self.roster_epoch = body.roster_epoch;
        self.head = account_statement_hash(statement);
        Ok(())
    }

    fn active_device_mut(
        &mut self,
        device_id: DeviceId,
        key_epoch: u64,
    ) -> Result<&mut ValidatedDeviceKey, String> {
        let key = self
            .device_keys
            .iter_mut()
            .find(|key| key.keys.device_id == device_id && key.keys.key_epoch == key_epoch)
            .ok_or_else(|| "account statement names an unknown device key".to_string())?;
        if key.status != DeviceKeyStatus::Active {
            return Err("account statement names a non-active device key".to_string());
        }
        Ok(key)
    }

    fn reject_reused_key_material(&self, device: &DevicePublicKeys) -> Result<(), String> {
        if self.device_keys.iter().any(|known| {
            known.keys.signing_public_key == device.signing_public_key
                || known.keys.encryption_public_key == device.encryption_public_key
        }) {
            return Err("retired account key material cannot be reused".to_string());
        }
        Ok(())
    }
}

pub fn account_id(
    server_public_key: &[u8],
    user_id: UserId,
    authority_public_key: &[u8; ACCOUNT_AUTHORITY_KEY_LEN],
) -> AccountId {
    let mut input = Vec::with_capacity(
        ACCOUNT_ID_LABEL.len() + server_public_key.len() + 8 + authority_public_key.len(),
    );
    input.extend_from_slice(ACCOUNT_ID_LABEL);
    input.extend_from_slice(server_public_key);
    input.extend_from_slice(&user_id.0.to_le_bytes());
    input.extend_from_slice(authority_public_key);
    AccountId(digest::digest(&digest::SHA256, &input).as_ref().try_into().unwrap())
}

pub fn account_statement_hash(statement: &AccountKeyStatement) -> LedgerHash {
    LedgerHash(
        digest::digest(&digest::SHA256, &jsony::to_binary(statement))
            .as_ref()
            .try_into()
            .unwrap(),
    )
}

pub fn sign_account_statement(
    body: AccountKeyStatementBody,
    authority_seed: &[u8; ACCOUNT_AUTHORITY_KEY_LEN],
    co_signer_seed: Option<&[u8; ACCOUNT_AUTHORITY_KEY_LEN]>,
) -> Result<AccountKeyStatement, String> {
    let authority = signature::Ed25519KeyPair::from_seed_unchecked(authority_seed)
        .map_err(|_| "account authority seed is invalid".to_string())?;
    let signing_bytes = statement_signing_bytes(&body);
    let authority_signature = authority.sign(&signing_bytes).as_ref().to_vec();
    let co_signature = co_signer_seed
        .map(|seed| {
            signature::Ed25519KeyPair::from_seed_unchecked(seed)
                .map(|key| key.sign(&signing_bytes).as_ref().to_vec())
                .map_err(|_| "account statement co-signer seed is invalid".to_string())
        })
        .transpose()?;
    Ok(AccountKeyStatement {
        body,
        authority_signature,
        co_signature,
    })
}

pub fn ed25519_public_key(seed: &[u8; DEVICE_SIGNING_KEY_LEN]) -> Result<[u8; 32], String> {
    let key = signature::Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| "Ed25519 seed is invalid".to_string())?;
    Ok(key.public_key().as_ref().try_into().unwrap())
}

fn statement_signing_bytes(body: &AccountKeyStatementBody) -> Vec<u8> {
    let encoded = jsony::to_binary(body);
    let mut bytes = Vec::with_capacity(ACCOUNT_STATEMENT_LABEL.len() + encoded.len());
    bytes.extend_from_slice(ACCOUNT_STATEMENT_LABEL);
    bytes.extend_from_slice(&encoded);
    bytes
}

fn validate_device_public_keys(device: &DevicePublicKeys) -> Result<(), String> {
    let name = device.name.trim();
    if name.is_empty()
        || name.len() > MAX_DEVICE_NAME_BYTES
        || name.chars().any(char::is_control)
        || name != device.name
    {
        return Err(
            "device name must be trimmed, 1-64 bytes, and contain no control characters"
                .to_string(),
        );
    }
    if device.key_epoch == 0 {
        return Err("device key epoch is zero".to_string());
    }
    fixed_key::<DEVICE_SIGNING_KEY_LEN>(
        &device.signing_public_key,
        "device signing public key",
    )?;
    let encryption = fixed_key::<E2E_PUBLIC_KEY_LEN>(
        &device.encryption_public_key,
        "device encryption public key",
    )?;
    if !PublicKey::from(encryption)
        .as_bytes()
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err("device encryption public key is low order".to_string());
    }
    Ok(())
}

fn fixed_key<const N: usize>(value: &[u8], name: &str) -> Result<[u8; N], String> {
    value
        .try_into()
        .map_err(|_| format!("{name} has the wrong length"))
}

fn verify_ed25519(public_key: &[u8; 32], message: &[u8], signed: &[u8]) -> Result<(), String> {
    signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
        .verify(message, signed)
        .map_err(|_| "account key statement signature is invalid".to_string())
}

/// Returns the X25519 public key for an identity seed.
pub fn e2e_public_key(seed: &[u8; E2E_SEED_LEN]) -> [u8; E2E_PUBLIC_KEY_LEN] {
    let secret = StaticSecret::from(*seed);
    PublicKey::from(&secret).to_bytes()
}

/// Directional sealing keys for one DM, mirrored between the two members: one
/// end's `send` equals the other end's `recv`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmPairKeys {
    pub send: KeyMaterial,
    pub recv: KeyMaterial,
}

/// Derives the [`DmPairKeys`] for a DM from our identity seed and the peer's
/// public key.
///
/// The HKDF transcript binds both user ids and both public keys in id order,
/// so the derivation is symmetric and a key can never be confused across user
/// pairs. Low-order peer keys are rejected before any material is derived.
///
/// # Errors
///
/// [`CryptoError::InvalidKey`] when the peer key is low-order (the shared
/// secret would not depend on our seed) or the two user ids are equal.
pub fn dm_pair_keys(
    my_seed: &[u8; E2E_SEED_LEN],
    my_id: UserId,
    peer_public: &[u8; E2E_PUBLIC_KEY_LEN],
    peer_id: UserId,
) -> Result<DmPairKeys, CryptoError> {
    if my_id == peer_id {
        return Err(CryptoError::InvalidKey);
    }
    let secret = StaticSecret::from(*my_seed);
    let my_public = PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&PublicKey::from(*peer_public));
    if !shared.was_contributory() {
        return Err(CryptoError::InvalidKey);
    }
    let ((low_id, low_public), (high_id, high_public)) = if my_id < peer_id {
        ((my_id, my_public), (peer_id, *peer_public))
    } else {
        ((peer_id, *peer_public), (my_id, my_public))
    };
    let mut transcript = Vec::with_capacity(15 + 2 * (8 + E2E_PUBLIC_KEY_LEN));
    transcript.extend_from_slice(b"chatt e2e dm v1");
    transcript.extend_from_slice(&low_id.0.to_le_bytes());
    transcript.extend_from_slice(&low_public);
    transcript.extend_from_slice(&high_id.0.to_le_bytes());
    transcript.extend_from_slice(&high_public);
    let transcript = digest::digest(&digest::SHA256, &transcript);
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, transcript.as_ref()).extract(shared.as_bytes());
    let low_high = KeyMaterial {
        id: 1,
        bytes: expand_key(&prk, b"chatt e2e dm low-high key v1"),
    };
    let high_low = KeyMaterial {
        id: 1,
        bytes: expand_key(&prk, b"chatt e2e dm high-low key v1"),
    };
    if my_id < peer_id {
        Ok(DmPairKeys {
            send: low_high,
            recv: high_low,
        })
    } else {
        Ok(DmPairKeys {
            send: high_low,
            recv: low_high,
        })
    }
}

/// The sealed unit relayed in place of DM message text: a fresh salt and the
/// AEAD output over the padded [`DmPlaintext`].
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DmEnvelope {
    pub salt: Vec<u8>,
    pub sealed: Vec<u8>,
}

/// What a DM envelope protects: the content plus the sender's wall clock, kept
/// inside the ciphertext so a relaying server rewriting `timestamp_ms` is
/// detectable.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DmPlaintext {
    pub sent_at_ms: u64,
    pub content: DmContent,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DmContent {
    Text { body: String },
    Edit { target: MessageId, body: String },
    Delete { target: MessageId },
    FileAnnounce { file: DmFileMeta },
}

/// The real metadata of a sealed file transfer, hidden from the server. The
/// wire-visible transfer carries a placeholder name and a padded size; this
/// struct restores the truth on the receiving client and hands over the
/// symmetric key its chunks are sealed with.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DmFileMeta {
    pub original_name: String,
    /// True decompressed byte length.
    pub size: u64,
    /// Encoding of the sealed chunk payloads.
    pub encoding: FileContentEncoding,
    /// Per-transfer chunk sealing key, [`KEY_LEN`] bytes.
    pub content_key: Vec<u8>,
}

/// The envelope class bound into the AAD, so the server cannot re-deliver a
/// mutation as a fresh message, change an edit into a delete, or present a file
/// announcement as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub enum DmContentKind {
    Text,
    Edit,
    Delete,
    FileAnnounce,
}

/// A validated account-ledger head named by an event. Both participants'
/// checkpoints are signed so a relaying directory cannot silently change the
/// recipient device set after the event was created.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RosterCheckpoint {
    pub account_id: AccountId,
    pub account_generation: u64,
    pub roster_epoch: u64,
    pub head: LedgerHash,
}

impl From<&ValidatedAccountLedger> for RosterCheckpoint {
    fn from(ledger: &ValidatedAccountLedger) -> Self {
        Self {
            account_id: ledger.account_id,
            account_generation: ledger.account_generation,
            roster_epoch: ledger.roster_epoch,
            head: ledger.head,
        }
    }
}

/// Authenticated sender metadata. A device maintains one sequence and
/// predecessor chain per room and key epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SenderEventHeader {
    pub event_id: EventId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub sender_account: AccountId,
    pub author_device: DeviceId,
    pub author_key_epoch: u64,
    pub sequence: u64,
    pub predecessor: Option<EventId>,
    pub kind: DmContentKind,
    pub created_at_ms: u64,
    pub semantic_target: Option<EventId>,
    pub sender_roster: RosterCheckpoint,
    pub recipient_roster: RosterCheckpoint,
}

/// One active device to which a sender should wrap the event content key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventRecipient {
    pub user_id: UserId,
    pub account_id: AccountId,
    pub device_id: DeviceId,
    pub key_epoch: u64,
    pub encryption_public_key: [u8; E2E_PUBLIC_KEY_LEN],
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct EventKeyWrap {
    pub user_id: UserId,
    pub account_id: AccountId,
    pub device_id: DeviceId,
    pub key_epoch: u64,
    pub sealed_key: Vec<u8>,
}

/// A signed, multi-recipient DM event. The plaintext is encrypted once; only
/// its random content key is wrapped independently to each active device.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DmEventEnvelope {
    pub header: SenderEventHeader,
    pub ephemeral_public_key: Vec<u8>,
    pub recipient_keys: Vec<EventKeyWrap>,
    pub sealed: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedDmEvent {
    pub header: SenderEventHeader,
    pub plaintext: DmPlaintext,
}

#[derive(Jsony)]
#[jsony(Binary, version)]
struct EventSignatureInput {
    label: Vec<u8>,
    header: SenderEventHeader,
    ephemeral_public_key: Vec<u8>,
    recipient_keys: Vec<EventKeyWrap>,
    sealed: Vec<u8>,
}

#[derive(Jsony)]
#[jsony(Binary, version)]
struct EventWrapContext {
    label: Vec<u8>,
    header: SenderEventHeader,
    ephemeral_public_key: Vec<u8>,
    user_id: UserId,
    account_id: AccountId,
    device_id: DeviceId,
    key_epoch: u64,
}

const DM_EVENT_AAD_LABEL: &[u8] = b"chatt dm event aad v2";
const DM_EVENT_SIGNATURE_LABEL: &[u8] = b"chatt dm event signature v2";
const DM_EVENT_WRAP_LABEL: &[u8] = b"chatt dm event key wrap v2";
const DEVICE_BINDING_LABEL: &[u8] = b"chatt e2e device session binding v1";

#[derive(Jsony)]
#[jsony(Binary, version)]
struct DeviceBindingInput {
    label: Vec<u8>,
    session_id: crate::ids::SessionId,
    user_id: UserId,
    device_id: DeviceId,
    key_epoch: u64,
    ledger_head: LedgerHash,
}

pub fn sign_device_binding(
    signing_seed: &[u8; DEVICE_SIGNING_KEY_LEN],
    session_id: crate::ids::SessionId,
    user_id: UserId,
    device_id: DeviceId,
    key_epoch: u64,
    ledger_head: LedgerHash,
) -> Result<Vec<u8>, CryptoError> {
    let key = signature::Ed25519KeyPair::from_seed_unchecked(signing_seed)
        .map_err(|_| CryptoError::InvalidKey)?;
    Ok(key
        .sign(&device_binding_bytes(
            session_id,
            user_id,
            device_id,
            key_epoch,
            ledger_head,
        ))
        .as_ref()
        .to_vec())
}

pub fn verify_device_binding(
    signing_public_key: &[u8; DEVICE_SIGNING_KEY_LEN],
    session_id: crate::ids::SessionId,
    user_id: UserId,
    device_id: DeviceId,
    key_epoch: u64,
    ledger_head: LedgerHash,
    signed: &[u8],
) -> Result<(), CryptoError> {
    signature::UnparsedPublicKey::new(&signature::ED25519, signing_public_key)
        .verify(
            &device_binding_bytes(
                session_id,
                user_id,
                device_id,
                key_epoch,
                ledger_head,
            ),
            signed,
        )
        .map_err(|_| CryptoError::InvalidSignature)
}

fn device_binding_bytes(
    session_id: crate::ids::SessionId,
    user_id: UserId,
    device_id: DeviceId,
    key_epoch: u64,
    ledger_head: LedgerHash,
) -> Vec<u8> {
    jsony::to_binary(&DeviceBindingInput {
        label: DEVICE_BINDING_LABEL.to_vec(),
        session_id,
        user_id,
        device_id,
        key_epoch,
        ledger_head,
    })
}

pub fn random_event_id(rng: &dyn SecureRandom) -> Result<EventId, CryptoError> {
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes).map_err(|_| CryptoError::Random)?;
    Ok(EventId(bytes))
}

pub fn random_device_id(rng: &dyn SecureRandom) -> Result<DeviceId, CryptoError> {
    let mut bytes = [0u8; 16];
    rng.fill(&mut bytes).map_err(|_| CryptoError::Random)?;
    Ok(DeviceId(bytes))
}

/// Seals and signs one multi-device event.
pub fn seal_dm_event(
    signing_seed: &[u8; DEVICE_SIGNING_KEY_LEN],
    header: SenderEventHeader,
    plaintext: &DmPlaintext,
    recipients: &[EventRecipient],
    rng: &dyn SecureRandom,
) -> Result<Vec<u8>, CryptoError> {
    if plaintext.content.kind() != header.kind
        || recipients.is_empty()
        || recipients.len() > MAX_ACTIVE_ACCOUNT_DEVICES * 2
    {
        return Err(CryptoError::InvalidEncoding);
    }
    let mut recipients = recipients.to_vec();
    recipients.sort_by_key(|recipient| {
        (
            recipient.user_id,
            recipient.account_id,
            recipient.device_id,
            recipient.key_epoch,
        )
    });
    if recipients.windows(2).any(|pair| {
        pair[0].user_id == pair[1].user_id
            && pair[0].account_id == pair[1].account_id
            && pair[0].device_id == pair[1].device_id
            && pair[0].key_epoch == pair[1].key_epoch
    }) {
        return Err(CryptoError::InvalidEncoding);
    }

    let mut event_key_bytes = [0u8; KEY_LEN];
    rng.fill(&mut event_key_bytes)
        .map_err(|_| CryptoError::Random)?;
    let event_key = KeyMaterial {
        id: 1,
        bytes: event_key_bytes,
    };
    let mut ephemeral_seed = [0u8; E2E_SEED_LEN];
    rng.fill(&mut ephemeral_seed)
        .map_err(|_| CryptoError::Random)?;
    let ephemeral_secret = StaticSecret::from(ephemeral_seed);
    let ephemeral_public_key = PublicKey::from(&ephemeral_secret).to_bytes();

    let inner = jsony::to_binary(plaintext);
    let padded_len = padded_envelope_len(inner.len());
    let mut sealed = Vec::with_capacity(padded_len + TAG_LEN);
    sealed.extend_from_slice(&(inner.len() as u32).to_le_bytes());
    sealed.extend_from_slice(&inner);
    sealed.resize(padded_len, 0);
    let aad = event_aad(&header);
    seal_in_place_append_tag(&event_key, 0, &aad, 0, &mut sealed)?;

    let mut recipient_keys = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        let shared = ephemeral_secret.diffie_hellman(&PublicKey::from(
            recipient.encryption_public_key,
        ));
        if !shared.was_contributory() {
            return Err(CryptoError::InvalidKey);
        }
        let context = EventWrapContext {
            label: DM_EVENT_WRAP_LABEL.to_vec(),
            header,
            ephemeral_public_key: ephemeral_public_key.to_vec(),
            user_id: recipient.user_id,
            account_id: recipient.account_id,
            device_id: recipient.device_id,
            key_epoch: recipient.key_epoch,
        };
        let context = jsony::to_binary(&context);
        let wrap_key = event_wrap_key(shared.as_bytes(), &context)?;
        let mut sealed_key = event_key.bytes.to_vec();
        seal_in_place_append_tag(&wrap_key, 0, &context, 0, &mut sealed_key)?;
        recipient_keys.push(EventKeyWrap {
            user_id: recipient.user_id,
            account_id: recipient.account_id,
            device_id: recipient.device_id,
            key_epoch: recipient.key_epoch,
            sealed_key,
        });
    }

    let signature_input = EventSignatureInput {
        label: DM_EVENT_SIGNATURE_LABEL.to_vec(),
        header,
        ephemeral_public_key: ephemeral_public_key.to_vec(),
        recipient_keys: recipient_keys.clone(),
        sealed: sealed.clone(),
    };
    let signing_key = signature::Ed25519KeyPair::from_seed_unchecked(signing_seed)
        .map_err(|_| CryptoError::InvalidKey)?;
    let signature = signing_key
        .sign(&jsony::to_binary(&signature_input))
        .as_ref()
        .to_vec();
    Ok(jsony::to_binary(&DmEventEnvelope {
        header,
        ephemeral_public_key: ephemeral_public_key.to_vec(),
        recipient_keys,
        sealed,
        signature,
    }))
}

/// Verifies the author's device signature and opens the content-key wrap
/// addressed to one local device key epoch.
pub fn open_dm_event(
    encoded: &[u8],
    author_signing_public_key: &[u8; DEVICE_SIGNING_KEY_LEN],
    local_user: UserId,
    local_account: AccountId,
    local_device: DeviceId,
    local_key_epoch: u64,
    local_encryption_seed: &[u8; E2E_SEED_LEN],
) -> Result<OpenedDmEvent, CryptoError> {
    if encoded.len() > MAX_DM_ENVELOPE_BYTES {
        return Err(CryptoError::InvalidEncoding);
    }
    let envelope: DmEventEnvelope =
        jsony::from_binary(encoded).map_err(|_| CryptoError::InvalidEncoding)?;
    if envelope.ephemeral_public_key.len() != E2E_PUBLIC_KEY_LEN
        || envelope.recipient_keys.is_empty()
        || envelope.recipient_keys.len() > MAX_ACTIVE_ACCOUNT_DEVICES * 2
    {
        return Err(CryptoError::InvalidEncoding);
    }
    let signature_input = EventSignatureInput {
        label: DM_EVENT_SIGNATURE_LABEL.to_vec(),
        header: envelope.header,
        ephemeral_public_key: envelope.ephemeral_public_key.clone(),
        recipient_keys: envelope.recipient_keys.clone(),
        sealed: envelope.sealed.clone(),
    };
    signature::UnparsedPublicKey::new(&signature::ED25519, author_signing_public_key)
        .verify(
            &jsony::to_binary(&signature_input),
            &envelope.signature,
        )
        .map_err(|_| CryptoError::InvalidSignature)?;

    let addressed = envelope
        .recipient_keys
        .iter()
        .find(|recipient| {
            recipient.user_id == local_user
                && recipient.account_id == local_account
                && recipient.device_id == local_device
                && recipient.key_epoch == local_key_epoch
        })
        .ok_or(CryptoError::WrongKeyId)?;
    let ephemeral_public = <[u8; E2E_PUBLIC_KEY_LEN]>::try_from(
        envelope.ephemeral_public_key.as_slice(),
    )
    .map_err(|_| CryptoError::InvalidEncoding)?;
    let secret = StaticSecret::from(*local_encryption_seed);
    let shared = secret.diffie_hellman(&PublicKey::from(ephemeral_public));
    if !shared.was_contributory() {
        return Err(CryptoError::InvalidKey);
    }
    let context = EventWrapContext {
        label: DM_EVENT_WRAP_LABEL.to_vec(),
        header: envelope.header,
        ephemeral_public_key: envelope.ephemeral_public_key,
        user_id: addressed.user_id,
        account_id: addressed.account_id,
        device_id: addressed.device_id,
        key_epoch: addressed.key_epoch,
    };
    let context = jsony::to_binary(&context);
    let wrap_key = event_wrap_key(shared.as_bytes(), &context)?;
    let mut sealed_key = addressed.sealed_key.clone();
    let key_len = open_in_place_with_aad(&wrap_key, 0, &context, &mut sealed_key)?;
    let key_bytes = <[u8; KEY_LEN]>::try_from(&sealed_key[..key_len])
        .map_err(|_| CryptoError::InvalidEncoding)?;
    let event_key = KeyMaterial {
        id: 1,
        bytes: key_bytes,
    };
    let mut sealed = envelope.sealed;
    let plain_len = open_in_place_with_aad(
        &event_key,
        0,
        &event_aad(&envelope.header),
        &mut sealed,
    )?;
    let plain = &sealed[..plain_len];
    let inner_len = plain
        .first_chunk::<4>()
        .map(|bytes| u32::from_le_bytes(*bytes) as usize)
        .ok_or(CryptoError::InvalidEncoding)?;
    let inner = plain
        .get(4..4 + inner_len)
        .ok_or(CryptoError::InvalidEncoding)?;
    let plaintext: DmPlaintext =
        jsony::from_binary(inner).map_err(|_| CryptoError::InvalidEncoding)?;
    if plaintext.content.kind() != envelope.header.kind {
        return Err(CryptoError::Cipher);
    }
    Ok(OpenedDmEvent {
        header: envelope.header,
        plaintext,
    })
}

fn event_aad(header: &SenderEventHeader) -> Vec<u8> {
    let encoded = jsony::to_binary(header);
    let mut aad = Vec::with_capacity(DM_EVENT_AAD_LABEL.len() + encoded.len());
    aad.extend_from_slice(DM_EVENT_AAD_LABEL);
    aad.extend_from_slice(&encoded);
    aad
}

struct EventHkdfLen;

impl hkdf::KeyType for EventHkdfLen {
    fn len(&self) -> usize {
        KEY_LEN
    }
}

fn event_wrap_key(shared: &[u8], context: &[u8]) -> Result<KeyMaterial, CryptoError> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, context);
    let prk = salt.extract(shared);
    let info = [DM_EVENT_WRAP_LABEL, context];
    let okm = prk
        .expand(&info, EventHkdfLen)
        .map_err(|_| CryptoError::InvalidKey)?;
    let mut bytes = [0u8; KEY_LEN];
    okm.fill(&mut bytes)
        .map_err(|_| CryptoError::InvalidKey)?;
    Ok(KeyMaterial { id: 1, bytes })
}

impl DmContentKind {
    const fn wire_id(self) -> u8 {
        match self {
            DmContentKind::Text => 0,
            DmContentKind::Edit => 1,
            DmContentKind::Delete => 2,
            DmContentKind::FileAnnounce => 3,
        }
    }
}

impl DmContent {
    pub fn kind(&self) -> DmContentKind {
        match self {
            DmContent::Text { .. } => DmContentKind::Text,
            DmContent::Edit { .. } => DmContentKind::Edit,
            DmContent::Delete { .. } => DmContentKind::Delete,
            DmContent::FileAnnounce { .. } => DmContentKind::FileAnnounce,
        }
    }
}

fn dm_aad(kind: DmContentKind, room_id: RoomId, sender: UserId) -> [u8; 14] {
    let mut aad = [0u8; 14];
    aad[0] = DM_ENVELOPE_VERSION;
    aad[1] = kind.wire_id();
    aad[2..6].copy_from_slice(&room_id.0.to_le_bytes());
    aad[6..14].copy_from_slice(&sender.0.to_le_bytes());
    aad
}

fn dm_message_key(direction_key: &KeyMaterial, salt: &[u8]) -> KeyMaterial {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&direction_key.bytes);
    KeyMaterial {
        id: 1,
        bytes: expand_key(&prk, b"chatt e2e dm message key v1"),
    }
}

fn padded_envelope_len(inner_len: usize) -> usize {
    (4 + inner_len).next_multiple_of(DM_PAD_MULTIPLE)
}

/// Seals `plaintext` for the DM `(room_id, sender)` context, returning encoded
/// [`DmEnvelope`] bytes ready for the wire.
///
/// Every call draws a fresh salt and derives a one-shot message key from it,
/// so sealing is stateless: no counters, no reuse hazard across restarts or
/// concurrent sessions holding the same identity. The plaintext is
/// length-prefixed and zero-padded to [`DM_PAD_MULTIPLE`].
///
/// # Errors
///
/// [`CryptoError::Random`] when salt generation fails, [`CryptoError::Cipher`]
/// when sealing fails.
pub fn seal_dm_envelope(
    keys: &DmPairKeys,
    room_id: RoomId,
    sender: UserId,
    plaintext: &DmPlaintext,
    rng: &dyn SecureRandom,
) -> Result<Vec<u8>, CryptoError> {
    let mut salt = [0u8; DM_SALT_LEN];
    rng.fill(&mut salt).map_err(|_| CryptoError::Random)?;
    let inner = jsony::to_binary(plaintext);
    let padded_len = padded_envelope_len(inner.len());
    let mut sealed = Vec::with_capacity(padded_len + TAG_LEN);
    sealed.extend_from_slice(&(inner.len() as u32).to_le_bytes());
    sealed.extend_from_slice(&inner);
    sealed.resize(padded_len, 0);
    let key = dm_message_key(&keys.send, &salt);
    let aad = dm_aad(plaintext.content.kind(), room_id, sender);
    seal_in_place_append_tag(&key, 0, &aad, 0, &mut sealed)?;
    let envelope = DmEnvelope {
        salt: salt.to_vec(),
        sealed,
    };
    Ok(jsony::to_binary(&envelope))
}

/// Opens encoded [`DmEnvelope`] bytes sealed by the peer for the
/// `(room_id, sender, kind)` context.
///
/// `kind` is what the outer message metadata claims the envelope to be (an
/// edit record, a file announcement, plain text); authentication fails if the
/// sealed class disagrees, and the decoded content is checked to match too.
///
/// # Errors
///
/// [`CryptoError::Cipher`] when authentication fails (wrong context, tampering,
/// or class mismatch), [`CryptoError::InvalidEncoding`] when the envelope or
/// its padding framing does not decode.
pub fn open_dm_envelope(
    keys: &DmPairKeys,
    room_id: RoomId,
    sender: UserId,
    kind: DmContentKind,
    envelope: &[u8],
) -> Result<DmPlaintext, CryptoError> {
    if envelope.len() > MAX_DM_ENVELOPE_BYTES {
        return Err(CryptoError::InvalidEncoding);
    }
    let Ok(envelope) = jsony::from_binary::<DmEnvelope>(envelope) else {
        return Err(CryptoError::InvalidEncoding);
    };
    let DmEnvelope { salt, mut sealed } = envelope;
    if salt.len() != DM_SALT_LEN {
        return Err(CryptoError::InvalidEncoding);
    }
    let key = dm_message_key(&keys.recv, &salt);
    let aad = dm_aad(kind, room_id, sender);
    let padded_len = open_in_place_with_aad(&key, 0, &aad, &mut sealed)?;
    let padded = &sealed[..padded_len];
    let Some(prefix) = padded.first_chunk::<4>() else {
        return Err(CryptoError::InvalidEncoding);
    };
    let inner_len = u32::from_le_bytes(*prefix) as usize;
    let Some(inner) = padded[4..].get(..inner_len) else {
        return Err(CryptoError::InvalidEncoding);
    };
    let Ok(plaintext) = jsony::from_binary::<DmPlaintext>(inner) else {
        return Err(CryptoError::InvalidEncoding);
    };
    if plaintext.content.kind().wire_id() != kind.wire_id() {
        return Err(CryptoError::Cipher);
    }
    Ok(plaintext)
}

/// Padmé padded length for a blob of `len` bytes (the PURB padding function):
/// the next length whose low `⌊log₂ len⌋ − ⌊log₂⌊log₂ len⌋⌋ − 1` bits are
/// zero. Overhead is at most ~12% and shrinks as `len` grows, while leaking
/// only O(log log len) bits of the true size.
pub fn padme_len(len: u64) -> u64 {
    if len < 2 {
        return len;
    }
    let e = 63 - len.leading_zeros() as u64;
    let s = 64 - e.leading_zeros() as u64;
    let mask = (1u64 << (e - s)) - 1;
    (len + mask) & !mask
}

fn dm_chunk_aad(room_id: RoomId, sender: UserId, event_id: EventId) -> [u8; 49] {
    let mut aad = [0u8; 49];
    aad[0..21].copy_from_slice(DM_CHUNK_AAD_LABEL);
    aad[21..25].copy_from_slice(&room_id.0.to_le_bytes());
    aad[25..33].copy_from_slice(&sender.0.to_le_bytes());
    aad[33..49].copy_from_slice(&event_id.0);
    aad
}

fn dm_content_key(content_key: &[u8]) -> Result<KeyMaterial, CryptoError> {
    let Ok(bytes) = <[u8; KEY_LEN]>::try_from(content_key) else {
        return Err(CryptoError::InvalidKey);
    };
    Ok(KeyMaterial { id: 1, bytes })
}

/// Seals one file-transfer chunk under the per-transfer content key.
///
/// The frame is `payload_len || payload || pad_len zero bytes`, encrypted with
/// the chunk `index` as the nonce counter so reordered or dropped chunks fail
/// to open. `pad_len` is zero except on trailing padding that conceals the
/// stream's true length.
///
/// # Errors
///
/// [`CryptoError::InvalidKey`] when `content_key` is not [`KEY_LEN`] bytes,
/// [`CryptoError::Cipher`] when sealing fails.
pub fn seal_dm_chunk(
    content_key: &[u8],
    room_id: RoomId,
    sender: UserId,
    event_id: EventId,
    index: u64,
    payload: &[u8],
    pad_len: usize,
) -> Result<Vec<u8>, CryptoError> {
    let key = dm_content_key(content_key)?;
    let mut frame = Vec::with_capacity(DM_CHUNK_OVERHEAD + payload.len() + pad_len);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame.resize(4 + payload.len() + pad_len, 0);
    let aad = dm_chunk_aad(room_id, sender, event_id);
    seal_in_place_append_tag(&key, index, &aad, 0, &mut frame)?;
    Ok(frame)
}

/// Opens one sealed file-transfer chunk, returning the payload with any
/// padding stripped.
///
/// # Errors
///
/// [`CryptoError::InvalidKey`] when `content_key` is not [`KEY_LEN`] bytes,
/// [`CryptoError::Cipher`] when authentication fails (tampering or a chunk
/// index mismatch), [`CryptoError::InvalidEncoding`] when the length framing
/// is inconsistent.
pub fn open_dm_chunk(
    content_key: &[u8],
    room_id: RoomId,
    sender: UserId,
    event_id: EventId,
    index: u64,
    frame: &mut Vec<u8>,
) -> Result<Vec<u8>, CryptoError> {
    let key = dm_content_key(content_key)?;
    let aad = dm_chunk_aad(room_id, sender, event_id);
    let plain_len = open_in_place_with_aad(&key, index, &aad, frame)?;
    let plain = &frame[..plain_len];
    let Some(prefix) = plain.first_chunk::<4>() else {
        return Err(CryptoError::InvalidEncoding);
    };
    let payload_len = u32::from_le_bytes(*prefix) as usize;
    let Some(payload) = plain[4..].get(..payload_len) else {
        return Err(CryptoError::InvalidEncoding);
    };
    Ok(payload.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::decode_hex;
    use ring::rand::SystemRandom;

    fn seed(hex: &str) -> [u8; E2E_SEED_LEN] {
        decode_hex(hex).unwrap().try_into().unwrap()
    }

    fn test_pair() -> (DmPairKeys, DmPairKeys) {
        let alice_seed = [1u8; E2E_SEED_LEN];
        let bob_seed = [2u8; E2E_SEED_LEN];
        let alice = dm_pair_keys(
            &alice_seed,
            UserId(7),
            &e2e_public_key(&bob_seed),
            UserId(9),
        )
        .unwrap();
        let bob = dm_pair_keys(
            &bob_seed,
            UserId(9),
            &e2e_public_key(&alice_seed),
            UserId(7),
        )
        .unwrap();
        (alice, bob)
    }

    #[test]
    fn x25519_matches_rfc7748_test_vector() {
        let alice = seed("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob = seed("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
        assert_eq!(
            e2e_public_key(&alice).to_vec(),
            decode_hex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a").unwrap()
        );
        assert_eq!(
            e2e_public_key(&bob).to_vec(),
            decode_hex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f").unwrap()
        );
        let secret = StaticSecret::from(alice);
        let shared = secret.diffie_hellman(&PublicKey::from(e2e_public_key(&bob)));
        assert_eq!(
            shared.as_bytes().to_vec(),
            decode_hex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742").unwrap()
        );
    }

    #[test]
    fn both_ends_derive_mirrored_dm_pair_keys() {
        let (alice, bob) = test_pair();
        assert_eq!(alice.send, bob.recv);
        assert_eq!(alice.recv, bob.send);
        assert_ne!(alice.send, alice.recv);
    }

    #[test]
    fn different_user_pairs_derive_distinct_keys() {
        let alice_seed = [1u8; E2E_SEED_LEN];
        let bob_public = e2e_public_key(&[2u8; E2E_SEED_LEN]);
        let a = dm_pair_keys(&alice_seed, UserId(7), &bob_public, UserId(9)).unwrap();
        let b = dm_pair_keys(&alice_seed, UserId(7), &bob_public, UserId(10)).unwrap();
        assert_ne!(a.send, b.send);
        assert_ne!(a.recv, b.recv);
    }

    #[test]
    fn rejects_low_order_peer_public_key() {
        let result = dm_pair_keys(&[1u8; E2E_SEED_LEN], UserId(7), &[0u8; 32], UserId(9));
        assert!(matches!(result, Err(CryptoError::InvalidKey)));
    }

    #[test]
    fn rejects_matching_user_ids() {
        let public = e2e_public_key(&[2u8; E2E_SEED_LEN]);
        let result = dm_pair_keys(&[1u8; E2E_SEED_LEN], UserId(7), &public, UserId(7));
        assert!(matches!(result, Err(CryptoError::InvalidKey)));
    }

    #[test]
    fn dm_envelope_round_trips_text_mutations_and_file_announce() {
        let (alice, bob) = test_pair();
        let rng = SystemRandom::new();
        let contents = [
            DmContent::Text {
                body: "hello there".to_string(),
            },
            DmContent::Edit {
                target: MessageId(41),
                body: "hello, there".to_string(),
            },
            DmContent::Delete {
                target: MessageId(42),
            },
            DmContent::FileAnnounce {
                file: DmFileMeta {
                    original_name: "cat.png".to_string(),
                    size: 12345,
                    encoding: FileContentEncoding::Zstd,
                    content_key: vec![3u8; KEY_LEN],
                },
            },
        ];
        for content in contents {
            let kind = content.kind();
            let plaintext = DmPlaintext {
                sent_at_ms: 1_720_000_000_000,
                content,
            };
            let envelope =
                seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
            let opened = open_dm_envelope(&bob, RoomId(4), UserId(7), kind, &envelope).unwrap();
            assert_eq!(opened, plaintext);
        }
    }

    #[test]
    fn dm_envelope_rejects_wrong_room_sender_kind_and_tampering() {
        let (alice, bob) = test_pair();
        let rng = SystemRandom::new();
        let plaintext = DmPlaintext {
            sent_at_ms: 1,
            content: DmContent::Text {
                body: "secret".to_string(),
            },
        };
        let envelope = seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
        let open =
            |room, sender, kind, bytes: &[u8]| open_dm_envelope(&bob, room, sender, kind, bytes);
        assert!(open(RoomId(5), UserId(7), DmContentKind::Text, &envelope).is_err());
        assert!(open(RoomId(4), UserId(8), DmContentKind::Text, &envelope).is_err());
        assert!(open(RoomId(4), UserId(7), DmContentKind::Edit, &envelope).is_err());
        let mut tampered = envelope.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(RoomId(4), UserId(7), DmContentKind::Text, &tampered).is_err());
        assert!(
            open_dm_envelope(&alice, RoomId(4), UserId(7), DmContentKind::Text, &envelope).is_err(),
            "sender's own recv key must not open its send-direction envelope"
        );
        assert!(open(RoomId(4), UserId(7), DmContentKind::Text, &envelope).is_ok());
    }

    #[test]
    fn dm_envelope_pads_to_160_byte_multiples() {
        let (alice, _) = test_pair();
        let rng = SystemRandom::new();
        let sealed_len = |body: &str| {
            let plaintext = DmPlaintext {
                sent_at_ms: 1,
                content: DmContent::Text {
                    body: body.to_string(),
                },
            };
            let envelope =
                seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
            jsony::from_binary::<DmEnvelope>(&envelope)
                .unwrap()
                .sealed
                .len()
        };
        assert_eq!(sealed_len("a"), DM_PAD_MULTIPLE + TAG_LEN);
        assert_eq!(sealed_len(&"a".repeat(100)), DM_PAD_MULTIPLE + TAG_LEN);
        assert_eq!(sealed_len(&"a".repeat(200)), 2 * DM_PAD_MULTIPLE + TAG_LEN);
        assert_eq!(sealed_len("b"), sealed_len(&"b".repeat(100)));
    }

    #[test]
    fn padme_len_matches_reference_values() {
        assert_eq!(padme_len(0), 0);
        assert_eq!(padme_len(1), 1);
        assert_eq!(padme_len(2), 2);
        assert_eq!(padme_len(9), 10);
        assert_eq!(padme_len(100), 104);
        assert_eq!(padme_len(1000), 1024);
        assert_eq!(padme_len(1024), 1024);
        assert_eq!(padme_len(1025), 1088);
        assert_eq!(padme_len(1_000_000), 1_015_808);
        for len in 2..4096u64 {
            let padded = padme_len(len);
            assert!(padded >= len);
            assert!((padded - len) as f64 / len as f64 <= 0.12);
            assert_eq!(padme_len(padded), padded, "padmé must be idempotent");
        }
    }

    #[test]
    fn dm_chunk_round_trips_and_rejects_reordered_index() {
        let content_key = vec![5u8; KEY_LEN];
        let payload = vec![9u8; 1000];
        let event_id = EventId([9; 16]);
        let frame =
            seal_dm_chunk(&content_key, RoomId(4), UserId(7), event_id, 3, &payload, 24).unwrap();
        assert_eq!(frame.len(), DM_CHUNK_OVERHEAD + payload.len() + 24);
        let mut opened = frame.clone();
        assert_eq!(
            open_dm_chunk(&content_key, RoomId(4), UserId(7), event_id, 3, &mut opened).unwrap(),
            payload
        );
        let mut reordered = frame.clone();
        assert!(
            open_dm_chunk(&content_key, RoomId(4), UserId(7), event_id, 4, &mut reordered).is_err()
        );
        let mut wrong_room = frame.clone();
        assert!(
            open_dm_chunk(&content_key, RoomId(5), UserId(7), event_id, 3, &mut wrong_room).is_err()
        );
        let mut wrong_event = frame;
        assert!(
            open_dm_chunk(
                &content_key,
                RoomId(4),
                UserId(7),
                EventId([8; 16]),
                3,
                &mut wrong_event,
            )
            .is_err()
        );
    }

    #[test]
    fn open_rejects_truncated_and_oversized_envelopes() {
        let (alice, bob) = test_pair();
        let rng = SystemRandom::new();
        let plaintext = DmPlaintext {
            sent_at_ms: 1,
            content: DmContent::Text {
                body: "x".to_string(),
            },
        };
        let envelope = seal_dm_envelope(&alice, RoomId(4), UserId(7), &plaintext, &rng).unwrap();
        for len in [0, 1, envelope.len() / 2] {
            assert!(
                open_dm_envelope(
                    &bob,
                    RoomId(4),
                    UserId(7),
                    DmContentKind::Text,
                    &envelope[..len]
                )
                .is_err()
            );
        }
        let oversized = vec![0u8; MAX_DM_ENVELOPE_BYTES + 1];
        assert!(
            open_dm_envelope(&bob, RoomId(4), UserId(7), DmContentKind::Text, &oversized).is_err()
        );
    }

    fn test_device(device: u8, key_epoch: u64) -> (DevicePublicKeys, [u8; 32], [u8; 32]) {
        let signing_seed = [device; 32];
        let encryption_seed = [device.wrapping_add(64); 32];
        (
            DevicePublicKeys {
                device_id: DeviceId([device; 16]),
                name: format!("device-{device}"),
                key_epoch,
                signing_public_key: ed25519_public_key(&signing_seed).unwrap().to_vec(),
                encryption_public_key: e2e_public_key(&encryption_seed).to_vec(),
            },
            signing_seed,
            encryption_seed,
        )
    }

    #[test]
    fn account_ledger_rejects_rollback_reactivation_and_unsigned_rotation() {
        let server = [9u8; 32];
        let user = UserId(7);
        let authority = [1u8; 32];
        let authority_public = ed25519_public_key(&authority).unwrap();
        let (first, first_signing, _) = test_device(2, 1);
        let identity = account_id(&server, user, &authority_public);
        let genesis = sign_account_statement(
            AccountKeyStatementBody {
                account_id: identity,
                account_generation: 1,
                roster_epoch: 1,
                previous: LedgerHash::default(),
                authority_key_epoch: 1,
                action: AccountKeyAction::Genesis {
                    authority_public_key: authority_public.to_vec(),
                    first_device: first.clone(),
                    recovery_bundle_hash: vec![3; 32],
                },
            },
            &authority,
            None,
        )
        .unwrap();
        let retire = sign_account_statement(
            AccountKeyStatementBody {
                account_id: identity,
                account_generation: 1,
                roster_epoch: 2,
                previous: account_statement_hash(&genesis),
                authority_key_epoch: 1,
                action: AccountKeyAction::RetireDeviceKey {
                    device_id: first.device_id,
                    key_epoch: 1,
                    final_event_heads: Vec::new(),
                },
            },
            &authority,
            Some(&first_signing),
        )
        .unwrap();
        let ledger = ValidatedAccountLedger::validate(&server, user, &[genesis.clone(), retire])
            .unwrap();
        assert_eq!(
            ledger.device_key(first.device_id, 1).unwrap().status,
            DeviceKeyStatus::Retired
        );

        let replay = sign_account_statement(
            AccountKeyStatementBody {
                account_id: identity,
                account_generation: 1,
                roster_epoch: 3,
                previous: ledger.head,
                authority_key_epoch: 1,
                action: AccountKeyAction::AddDevice { device: first },
            },
            &authority,
            None,
        )
        .unwrap();
        let mut ledger = ledger;
        assert!(ledger.apply(&replay).is_err());

        let new_authority = [4u8; 32];
        let unsigned_rotation = sign_account_statement(
            AccountKeyStatementBody {
                account_id: identity,
                account_generation: 1,
                roster_epoch: 3,
                previous: ledger.head,
                authority_key_epoch: 1,
                action: AccountKeyAction::RotateAccountAuthority {
                    new_authority_public_key: ed25519_public_key(&new_authority)
                        .unwrap()
                        .to_vec(),
                },
            },
            &authority,
            None,
        )
        .unwrap();
        assert!(ledger.apply(&unsigned_rotation).is_err());
    }

    #[test]
    fn multi_recipient_event_opens_on_every_device_and_detects_tampering() {
        let rng = SystemRandom::new();
        let (author, author_signing, author_encryption) = test_device(1, 1);
        let (other_sender_device, _, other_sender_encryption) = test_device(2, 1);
        let (recipient_device, _, recipient_encryption) = test_device(3, 1);
        let sender_account = AccountId([11; 32]);
        let recipient_account = AccountId([22; 32]);
        let sender_roster = RosterCheckpoint {
            account_id: sender_account,
            account_generation: 1,
            roster_epoch: 2,
            head: LedgerHash([1; 32]),
        };
        let recipient_roster = RosterCheckpoint {
            account_id: recipient_account,
            account_generation: 1,
            roster_epoch: 1,
            head: LedgerHash([2; 32]),
        };
        let header = SenderEventHeader {
            event_id: EventId([7; 16]),
            room_id: RoomId(4),
            sender: UserId(10),
            sender_account,
            author_device: author.device_id,
            author_key_epoch: 1,
            sequence: 8,
            predecessor: Some(EventId([6; 16])),
            kind: DmContentKind::Text,
            created_at_ms: 123,
            semantic_target: None,
            sender_roster,
            recipient_roster,
        };
        let plaintext = DmPlaintext {
            sent_at_ms: 123,
            content: DmContent::Text {
                body: "all devices".to_string(),
            },
        };
        let recipients = [
            EventRecipient {
                user_id: UserId(10),
                account_id: sender_account,
                device_id: author.device_id,
                key_epoch: 1,
                encryption_public_key: e2e_public_key(&author_encryption),
            },
            EventRecipient {
                user_id: UserId(10),
                account_id: sender_account,
                device_id: other_sender_device.device_id,
                key_epoch: 1,
                encryption_public_key: e2e_public_key(&other_sender_encryption),
            },
            EventRecipient {
                user_id: UserId(20),
                account_id: recipient_account,
                device_id: recipient_device.device_id,
                key_epoch: 1,
                encryption_public_key: e2e_public_key(&recipient_encryption),
            },
        ];
        let encoded = seal_dm_event(&author_signing, header, &plaintext, &recipients, &rng).unwrap();
        let author_public = <[u8; 32]>::try_from(author.signing_public_key.as_slice()).unwrap();
        for (user, account, device, encryption) in [
            (UserId(10), sender_account, author.device_id, author_encryption),
            (
                UserId(10),
                sender_account,
                other_sender_device.device_id,
                other_sender_encryption,
            ),
            (
                UserId(20),
                recipient_account,
                recipient_device.device_id,
                recipient_encryption,
            ),
        ] {
            let opened = open_dm_event(
                &encoded,
                &author_public,
                user,
                account,
                device,
                1,
                &encryption,
            )
            .unwrap();
            assert_eq!(opened.plaintext, plaintext);
            assert_eq!(opened.header.event_id, EventId([7; 16]));
        }
        let mut tampered: DmEventEnvelope = jsony::from_binary(&encoded).unwrap();
        tampered.recipient_keys.pop();
        assert!(
            open_dm_event(
                &jsony::to_binary(&tampered),
                &author_public,
                UserId(10),
                sender_account,
                author.device_id,
                1,
                &author_encryption,
            )
            .is_err()
        );
    }
}
