//! Bounded application and delivery types for MLS encrypted rooms.
//!
//! Cryptographic MLS objects stay opaque on Chatt's wire. Only routing,
//! authorization and idempotency fields are represented here.

use crate::crypto::{
    CryptoError, KEY_LEN, KeyMaterial, TAG_LEN, open_in_place_with_aad, seal_in_place_append_tag,
};
use aws_lc_rs::digest;
use jsony::Jsony;

use crate::ids::{AccountId, DeviceId, EventId, FileTransferId, MessageId, RoomId};

pub const MLS_PROTOCOL_VERSION: u16 = 1;
pub const MAX_ENCRYPTED_ROOM_ACCOUNTS: usize = 64;
pub const MAX_MLS_GROUP_ID_BYTES: usize = 64;
pub const MAX_MLS_KEY_PACKAGE_BYTES: usize = 64 * 1024;
pub const MAX_MLS_KEY_PACKAGES_PER_REQUEST: usize = 32;
pub const MAX_MLS_MESSAGE_BYTES: usize = 224 * 1024;
pub const MAX_MLS_WELCOMES_PER_COMMIT: usize = 64;
pub const MAX_MLS_EVENT_BATCH: usize = 500;
pub const MAX_MLS_FILE_NAME_BYTES: usize = 255;
pub const FILE_CHUNK_OVERHEAD: usize = 4 + TAG_LEN;

const GROUP_ID_LABEL: &[u8] = b"chatt mls group id v1";

/// Padmé padded length for encrypted file streams.
pub fn padme_len(len: u64) -> u64 {
    if len < 2 {
        return len;
    }
    let e = 63 - len.leading_zeros() as u64;
    let s = 64 - e.leading_zeros() as u64;
    let mask = (1u64 << (e - s)) - 1;
    (len + mask) & !mask
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct EncryptedRoomDescriptor {
    pub room_id: RoomId,
    pub mls_group_id: Vec<u8>,
    pub creator: AccountId,
    pub member_accounts: Vec<AccountId>,
    pub created_at_ms: u64,
    pub protocol_version: u16,
}

#[derive(Jsony)]
#[jsony(Binary, version)]
struct GroupIdInput {
    room_id: RoomId,
    creator: AccountId,
    member_accounts: Vec<AccountId>,
    created_at_ms: u64,
    protocol_version: u16,
}

impl EncryptedRoomDescriptor {
    pub fn new(
        room_id: RoomId,
        creator: AccountId,
        mut member_accounts: Vec<AccountId>,
        created_at_ms: u64,
    ) -> Result<Self, String> {
        member_accounts.sort_unstable();
        if member_accounts
            .windows(2)
            .any(|members| members[0] == members[1])
        {
            return Err("encrypted room contains a duplicate account".to_string());
        }
        let mut descriptor = Self {
            room_id,
            mls_group_id: Vec::new(),
            creator,
            member_accounts,
            created_at_ms,
            protocol_version: MLS_PROTOCOL_VERSION,
        };
        descriptor.mls_group_id = descriptor.derived_group_id();
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != MLS_PROTOCOL_VERSION {
            return Err("encrypted room protocol version is unsupported".to_string());
        }
        if !(2..=MAX_ENCRYPTED_ROOM_ACCOUNTS).contains(&self.member_accounts.len()) {
            return Err("encrypted room has an invalid account count".to_string());
        }
        if self
            .member_accounts
            .windows(2)
            .any(|members| members[0] >= members[1])
        {
            return Err("encrypted room accounts are not uniquely sorted".to_string());
        }
        if self.member_accounts.binary_search(&self.creator).is_err() {
            return Err("encrypted room creator is not a member".to_string());
        }
        if self.mls_group_id.len() > MAX_MLS_GROUP_ID_BYTES
            || self.mls_group_id != self.derived_group_id()
        {
            return Err("encrypted room MLS group id is invalid".to_string());
        }
        Ok(())
    }

    pub fn derived_group_id(&self) -> Vec<u8> {
        let input = GroupIdInput {
            room_id: self.room_id,
            creator: self.creator,
            member_accounts: self.member_accounts.clone(),
            created_at_ms: self.created_at_ms,
            protocol_version: self.protocol_version,
        };
        let encoded = jsony::to_binary(&input);
        let mut bytes = Vec::with_capacity(GROUP_ID_LABEL.len() + encoded.len());
        bytes.extend_from_slice(GROUP_ID_LABEL);
        bytes.extend_from_slice(&encoded);
        digest::digest(&digest::SHA256, &bytes).as_ref().to_vec()
    }

    pub fn is_dm(&self) -> bool {
        self.member_accounts.len() == 2
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct MlsChattEvent {
    pub version: u16,
    pub room_id: RoomId,
    pub event_id: EventId,
    pub sender_account: AccountId,
    pub timestamp_ms: u64,
    pub content: ChattEventContent,
}

impl MlsChattEvent {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != MLS_PROTOCOL_VERSION {
            return Err("unsupported Chatt MLS event version".to_string());
        }
        match &self.content {
            ChattEventContent::Text { body } | ChattEventContent::Edit { body, .. } => {
                if body.trim().is_empty() || body.len() > crate::control::MAX_CHAT_BODY_BYTES {
                    return Err("invalid Chatt MLS text length".to_string());
                }
            }
            ChattEventContent::Delete { .. } => {}
            ChattEventContent::Reaction { reaction, .. } => {
                if reaction.trim().is_empty() || reaction.len() > 64 {
                    return Err("invalid Chatt MLS reaction length".to_string());
                }
            }
            ChattEventContent::File(file) => {
                if file.name.is_empty()
                    || file.name.len() > MAX_MLS_FILE_NAME_BYTES
                    || file.name.contains(['/', '\\'])
                    || file.name.chars().any(char::is_control)
                    || file.size == 0
                    || file.chunk_size == 0
                {
                    return Err("invalid Chatt MLS file announcement".to_string());
                }
            }
        }
        if jsony::to_binary(self).len() > MAX_MLS_MESSAGE_BYTES {
            return Err("Chatt MLS event is too large".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ChattEventContent {
    Text { body: String },
    Edit { target: MessageId, body: String },
    Delete { target: MessageId },
    Reaction { target: MessageId, reaction: String },
    File(MlsFileAnnouncement),
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct MlsFileAnnouncement {
    pub transfer_id: FileTransferId,
    pub name: String,
    pub size: u64,
    pub chunk_size: u32,
    pub encoding: crate::control::FileContentEncoding,
    pub file_key: [u8; 32],
    pub digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct PublishedKeyPackage {
    pub device_id: DeviceId,
    pub package: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct MlsWelcome {
    /// Per-device Welcome inbox cursor. Zero inside an unsubmitted bundle.
    pub delivery_id: u64,
    /// Delivery sequence of the commit that created this Welcome. Zero only
    /// while the Welcome is inside an unsubmitted commit bundle.
    pub sequence: u64,
    pub device_id: DeviceId,
    pub descriptor: EncryptedRoomDescriptor,
    pub welcome: Vec<u8>,
}

/// One MLS Welcome shared by every KeyPackage added by an atomic commit.
///
/// MLS encrypts a separate GroupSecrets entry for each target inside this one
/// message. Keeping the target list beside the shared message avoids repeating
/// the ratchet tree once per device on the submit path.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct MlsWelcomeBundle {
    pub device_ids: Vec<DeviceId>,
    pub descriptor: EncryptedRoomDescriptor,
    pub welcome: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct MlsCommitBundle {
    /// GroupInfo for the parent state, required only for room creation.
    pub prior_group_info: Option<Vec<u8>>,
    pub commit: Vec<u8>,
    pub welcome: Option<MlsWelcomeBundle>,
    pub group_info: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum MlsDeliveryEvent {
    Commit {
        sequence: u64,
        parent_epoch: u64,
        epoch: u64,
        rosters: Vec<crate::identity::SignedDeviceRoster>,
        commit: Vec<u8>,
    },
    Application {
        sequence: u64,
        epoch: u64,
        event_id: EventId,
        rosters: Vec<crate::identity::SignedDeviceRoster>,
        ciphertext: Vec<u8>,
    },
}

impl MlsDeliveryEvent {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::Commit { sequence, .. } | Self::Application { sequence, .. } => *sequence,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub enum MlsSubmitOutcome {
    Stored { sequence: u64 },
    AlreadyStored { sequence: u64 },
    StaleEpochNotStored { current_epoch: u64 },
    RevocationPending,
    RejoinRequired,
    TemporarilyBlocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub enum MlsCommitOutcome {
    Accepted { sequence: u64, epoch: u64 },
    AlreadyAccepted { sequence: u64, epoch: u64 },
    StaleEpoch { current_epoch: u64 },
    MissingKeyPackage { device_id: DeviceId },
    RejoinRequired,
    TemporarilyBlocked,
    PolicyRejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub enum RoomE2eAvailability {
    Ready,
    Joining,
    WaitingForKeyPackage,
    WaitingForCommit,
    RevocationPending,
    NeedsRejoin,
    HistoryUnavailable,
    PolicyError(String),
}

pub fn validate_key_packages(packages: &[PublishedKeyPackage]) -> Result<(), String> {
    if packages.is_empty() || packages.len() > MAX_MLS_KEY_PACKAGES_PER_REQUEST {
        return Err("invalid MLS KeyPackage count".to_string());
    }
    for package in packages {
        if package.package.is_empty() || package.package.len() > MAX_MLS_KEY_PACKAGE_BYTES {
            return Err("invalid MLS KeyPackage length".to_string());
        }
    }
    Ok(())
}

pub fn validate_commit_bundle(bundle: &MlsCommitBundle) -> Result<(), String> {
    if let Some(group_info) = &bundle.prior_group_info {
        validate_mls_bytes(group_info, "prior GroupInfo")?;
    }
    validate_mls_bytes(&bundle.commit, "commit")?;
    if let Some(welcome) = &bundle.welcome {
        if welcome.device_ids.is_empty() || welcome.device_ids.len() > MAX_MLS_WELCOMES_PER_COMMIT {
            return Err("MLS commit contains an invalid Welcome target count".to_string());
        }
        let mut targets = welcome.device_ids.clone();
        targets.sort_unstable();
        if targets.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("MLS commit contains duplicate Welcome targets".to_string());
        }
        welcome.descriptor.validate()?;
        validate_mls_bytes(&welcome.welcome, "Welcome")?;
    }
    validate_mls_bytes(&bundle.group_info, "GroupInfo")
}

pub fn validate_event_batch(events: &[MlsDeliveryEvent]) -> Result<(), String> {
    if events.len() > MAX_MLS_EVENT_BATCH {
        return Err("MLS event batch is too large".to_string());
    }
    let mut previous = None;
    for event in events {
        if previous.is_some_and(|sequence| sequence >= event.sequence()) {
            return Err("MLS events are not strictly ordered".to_string());
        }
        previous = Some(event.sequence());
        match event {
            MlsDeliveryEvent::Commit { commit, .. } => validate_mls_bytes(commit, "commit")?,
            MlsDeliveryEvent::Application { ciphertext, .. } => {
                validate_mls_bytes(ciphertext, "application message")?
            }
        }
    }
    Ok(())
}

fn file_chunk_aad(
    room_id: RoomId,
    sender: crate::ids::UserId,
    event_id: EventId,
    transfer_id: FileTransferId,
    total_size: u64,
    digest: &[u8; 32],
) -> Vec<u8> {
    let mut aad = b"chatt mls file chunk v1".to_vec();
    aad.extend_from_slice(&room_id.0.to_le_bytes());
    aad.extend_from_slice(&sender.0.to_le_bytes());
    aad.extend_from_slice(&event_id.0);
    aad.extend_from_slice(&transfer_id.0.to_le_bytes());
    aad.extend_from_slice(&total_size.to_le_bytes());
    aad.extend_from_slice(digest);
    aad
}

pub fn seal_file_chunk(
    file_key: &[u8; 32],
    room_id: RoomId,
    sender: crate::ids::UserId,
    event_id: EventId,
    transfer_id: FileTransferId,
    total_size: u64,
    digest: &[u8; 32],
    index: u64,
    payload: &[u8],
    pad_len: usize,
) -> Result<Vec<u8>, CryptoError> {
    let key = KeyMaterial {
        id: 1,
        bytes: *file_key,
    };
    let mut frame = Vec::with_capacity(FILE_CHUNK_OVERHEAD + payload.len() + pad_len);
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(payload);
    frame.resize(4 + payload.len() + pad_len, 0);
    let aad = file_chunk_aad(room_id, sender, event_id, transfer_id, total_size, digest);
    seal_in_place_append_tag(&key, index, &aad, 0, &mut frame)?;
    Ok(frame)
}

pub fn open_file_chunk(
    file_key: &[u8; KEY_LEN],
    room_id: RoomId,
    sender: crate::ids::UserId,
    event_id: EventId,
    transfer_id: FileTransferId,
    total_size: u64,
    digest: &[u8; 32],
    index: u64,
    frame: &mut Vec<u8>,
) -> Result<Vec<u8>, CryptoError> {
    let key = KeyMaterial {
        id: 1,
        bytes: *file_key,
    };
    let aad = file_chunk_aad(room_id, sender, event_id, transfer_id, total_size, digest);
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

fn validate_mls_bytes(bytes: &[u8], kind: &str) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_MLS_MESSAGE_BYTES {
        return Err(format!("invalid MLS {kind} length"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_canonicalizes_accounts_and_binds_group_id() {
        let first = AccountId([1; 32]);
        let second = AccountId([2; 32]);
        let descriptor =
            EncryptedRoomDescriptor::new(RoomId(9), first, vec![second, first], 123).unwrap();
        assert_eq!(descriptor.member_accounts, vec![first, second]);
        descriptor.validate().unwrap();

        let mut substituted = descriptor.clone();
        substituted.member_accounts[1] = AccountId([3; 32]);
        assert!(substituted.validate().is_err());
    }

    #[test]
    fn duplicate_accounts_are_rejected() {
        let account = AccountId([1; 32]);
        assert!(
            EncryptedRoomDescriptor::new(RoomId(9), account, vec![account, account], 123,).is_err()
        );
    }

    #[test]
    fn delivery_batches_must_be_bounded_and_ordered() {
        let event = |sequence| MlsDeliveryEvent::Application {
            sequence,
            epoch: 1,
            event_id: EventId([sequence as u8; 16]),
            rosters: Vec::new(),
            ciphertext: vec![1],
        };
        validate_event_batch(&[event(1), event(2)]).unwrap();
        assert!(validate_event_batch(&[event(2), event(1)]).is_err());
    }

    #[test]
    fn file_chunks_bind_transfer_room_order_size_and_digest() {
        let key = [1; 32];
        let digest = [2; 32];
        let mut frame = seal_file_chunk(
            &key,
            RoomId(3),
            crate::ids::UserId(4),
            EventId([5; 16]),
            FileTransferId(6),
            7,
            &digest,
            8,
            b"payload",
            9,
        )
        .unwrap();
        assert_eq!(
            open_file_chunk(
                &key,
                RoomId(3),
                crate::ids::UserId(4),
                EventId([5; 16]),
                FileTransferId(6),
                7,
                &digest,
                8,
                &mut frame,
            )
            .unwrap(),
            b"payload"
        );

        let failures = [
            (RoomId(9), FileTransferId(6), 7, digest, 8),
            (RoomId(3), FileTransferId(9), 7, digest, 8),
            (RoomId(3), FileTransferId(6), 9, digest, 8),
            (RoomId(3), FileTransferId(6), 7, [9; 32], 8),
            (RoomId(3), FileTransferId(6), 7, digest, 9),
        ];
        for (room, transfer, size, digest, index) in failures {
            let mut frame = seal_file_chunk(
                &key,
                RoomId(3),
                crate::ids::UserId(4),
                EventId([5; 16]),
                FileTransferId(6),
                7,
                &[2; 32],
                8,
                b"payload",
                0,
            )
            .unwrap();
            assert!(
                open_file_chunk(
                    &key,
                    room,
                    crate::ids::UserId(4),
                    EventId([5; 16]),
                    transfer,
                    size,
                    &digest,
                    index,
                    &mut frame,
                )
                .is_err()
            );
        }
        let mut truncated = seal_file_chunk(
            &key,
            RoomId(3),
            crate::ids::UserId(4),
            EventId([5; 16]),
            FileTransferId(6),
            7,
            &[2; 32],
            8,
            b"payload",
            0,
        )
        .unwrap();
        truncated.pop();
        assert!(
            open_file_chunk(
                &key,
                RoomId(3),
                crate::ids::UserId(4),
                EventId([5; 16]),
                FileTransferId(6),
                7,
                &[2; 32],
                8,
                &mut truncated,
            )
            .is_err()
        );
    }
}
