use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, RwLock},
};

use mls_rs::{
    ExtensionList, IdentityProvider, MlsRules,
    group::{
        Roster,
        proposal::{ReInitProposal, RemoveProposal},
    },
    identity::{CredentialType, SigningIdentity, basic::BasicCredential},
    mls_rules::{CommitDirection, CommitOptions, CommitSource, EncryptionOptions, ProposalBundle},
    time::MlsTime,
};
use mls_rs_core::identity::MemberValidationContext;
use rpc::{
    identity::{
        RosterCheckpoint, SignedDeviceCertificate, SignedDeviceRoster, parse_mls_client_id,
        roster_checkpoint, validate_device_roster,
    },
    ids::{AccountId, DeviceId, UserId},
    mls::EncryptedRoomDescriptor,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyError {
    InvalidRoster(String),
    InvalidDescriptor(String),
    UnsupportedCredential,
    UnknownDevice,
    SignatureKeyMismatch,
    UnknownRoom,
    AccountNotInRoom,
    ActiveDeviceRemoval,
    InvalidGroupRoster,
    ReinitializationNotAllowed,
    LockPoisoned,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoster(error) => write!(f, "invalid device roster: {error}"),
            Self::InvalidDescriptor(error) => write!(f, "invalid room descriptor: {error}"),
            Self::UnsupportedCredential => f.write_str("MLS credential is not a Basic credential"),
            Self::UnknownDevice => f.write_str("MLS identity is not an active certified device"),
            Self::SignatureKeyMismatch => {
                f.write_str("MLS signature key does not match the device certificate")
            }
            Self::UnknownRoom => f.write_str("MLS group has no accepted room descriptor"),
            Self::AccountNotInRoom => {
                f.write_str("MLS device account is not a member of the encrypted room")
            }
            Self::ActiveDeviceRemoval => {
                f.write_str("commit removes a device that is still active")
            }
            Self::InvalidGroupRoster => {
                f.write_str("commit would violate the fixed room roster")
            }
            Self::ReinitializationNotAllowed => {
                f.write_str("MLS reinitialization would change the immutable room")
            }
            Self::LockPoisoned => f.write_str("MLS policy state lock is poisoned"),
        }
    }
}

impl std::error::Error for PolicyError {}

impl mls_rs::error::IntoAnyError for PolicyError {
    fn into_dyn_error(self) -> Result<Box<dyn std::error::Error + Send + Sync>, Self> {
        Ok(Box::new(self))
    }
}

#[derive(Clone, Debug)]
struct CertifiedDevice {
    account_id: AccountId,
    user_id: UserId,
    device_id: DeviceId,
    signature_key: Vec<u8>,
}

#[derive(Debug, Default)]
struct PolicyState {
    /// Certificates in the current authority-signed roster, used for all new
    /// MLS membership decisions.
    devices: HashMap<Vec<u8>, CertifiedDevice>,
    /// Highest authenticated roster revision installed for each account.
    ///
    /// The delivery service can replay an older, still-valid signed roster.
    /// Keeping this checkpoint prevents such a replay from reactivating a
    /// revoked credential in the MLS authorization view.
    rosters: HashMap<AccountId, RosterCheckpoint>,
    rooms: HashMap<Vec<u8>, EncryptedRoomDescriptor>,
}

/// Shared, hot-swappable authorization state used by every Chatt MLS group.
#[derive(Clone, Debug)]
pub struct ChattIdentityProvider {
    server_id: Arc<Vec<u8>>,
    state: Arc<RwLock<PolicyState>>,
    allow_historical_group_info: bool,
}

impl ChattIdentityProvider {
    pub fn new(server_id: Vec<u8>) -> Self {
        Self {
            server_id: Arc::new(server_id),
            state: Arc::new(RwLock::new(PolicyState::default())),
            allow_historical_group_info: false,
        }
    }

    /// Returns an observer view for a canonical GroupInfo fetched from the
    /// authenticated Chatt delivery service. Existing historical leaves may
    /// no longer be in the current roster; commit validation remains strict.
    pub fn historical_group_info_observer(&self) -> Self {
        let mut observer = self.clone();
        observer.allow_historical_group_info = true;
        observer
    }

    /// Replaces all active certificates for an account with one validated
    /// current roster. Historical certificates are intentionally not kept.
    pub fn install_roster(&self, roster: &SignedDeviceRoster) -> Result<(), PolicyError> {
        validate_device_roster(roster, &self.server_id, roster.body.user_id)
            .map_err(PolicyError::InvalidRoster)?;
        let checkpoint = roster_checkpoint(roster);
        let mut state = self.state.write().map_err(|_| PolicyError::LockPoisoned)?;
        if let Some(current) = state.rosters.get(&roster.body.account_id) {
            if *current == checkpoint {
                return Ok(());
            }
            if checkpoint.revision <= current.revision {
                return Err(PolicyError::InvalidRoster(
                    "device roster revision rolls back or equivocates".to_string(),
                ));
            }
        }
        state
            .devices
            .retain(|_, device| device.account_id != roster.body.account_id);
        for certificate in &roster.body.active_devices {
            let client_id = certificate.body.mls_client_id.clone();
            let device = certified_device(certificate);
            state.devices.insert(client_id, device);
        }
        state.rosters.insert(roster.body.account_id, checkpoint);
        debug_assert_eq!(
            state
                .devices
                .values()
                .filter(|device| device.account_id == roster.body.account_id)
                .count(),
            roster.body.active_devices.len(),
        );
        Ok(())
    }

    pub fn install_room(&self, descriptor: EncryptedRoomDescriptor) -> Result<(), PolicyError> {
        descriptor
            .validate()
            .map_err(PolicyError::InvalidDescriptor)?;
        self.state
            .write()
            .map_err(|_| PolicyError::LockPoisoned)?
            .rooms
            .insert(descriptor.mls_group_id.clone(), descriptor);
        Ok(())
    }

    /// Returns the room descriptors currently published to MLS policy.
    /// Server recovery uses this to verify that speculative validation never
    /// leaked a descriptor into the live authorization view.
    pub fn room_descriptors(&self) -> Result<Vec<EncryptedRoomDescriptor>, PolicyError> {
        Ok(self
            .state
            .read()
            .map_err(|_| PolicyError::LockPoisoned)?
            .rooms
            .values()
            .cloned()
            .collect())
    }

    fn certificate_for(
        &self,
        signing_identity: &SigningIdentity,
    ) -> Result<CertifiedDevice, PolicyError> {
        let client_id = basic_client_id(signing_identity)?;
        let state = self.state.read().map_err(|_| PolicyError::LockPoisoned)?;
        let device = state
            .devices
            .get(client_id)
            .ok_or(PolicyError::UnknownDevice)?;
        if signing_identity.signature_key.as_ref() != device.signature_key {
            return Err(PolicyError::SignatureKeyMismatch);
        }
        Ok(device.clone())
    }

    fn validate_in_group(
        &self,
        signing_identity: &SigningIdentity,
        group_id: Option<&[u8]>,
    ) -> Result<(), PolicyError> {
        let device = self.certificate_for(signing_identity)?;
        let Some(group_id) = group_id else {
            return Ok(());
        };
        let state = self.state.read().map_err(|_| PolicyError::LockPoisoned)?;
        let room = state.rooms.get(group_id).ok_or(PolicyError::UnknownRoom)?;
        if room
            .member_accounts
            .binary_search(&device.account_id)
            .is_err()
        {
            return Err(PolicyError::AccountNotInRoom);
        }
        Ok(())
    }

    fn validate_historical_in_group(
        &self,
        signing_identity: &SigningIdentity,
        group_id: &[u8],
    ) -> Result<(), PolicyError> {
        let client_id = basic_client_id(signing_identity)?;
        let account_id = parse_mls_client_id(&self.server_id, client_id)
            .map(|(account_id, _)| account_id)
            .map_err(|_| PolicyError::UnknownDevice)?;
        let state = self.state.read().map_err(|_| PolicyError::LockPoisoned)?;
        let room = state.rooms.get(group_id).ok_or(PolicyError::UnknownRoom)?;
        if room.member_accounts.binary_search(&account_id).is_err() {
            return Err(PolicyError::AccountNotInRoom);
        }
        Ok(())
    }

    fn is_active(&self, signing_identity: &SigningIdentity) -> Result<bool, PolicyError> {
        let client_id = basic_client_id(signing_identity)?;
        Ok(self
            .state
            .read()
            .map_err(|_| PolicyError::LockPoisoned)?
            .devices
            .get(client_id)
            .is_some_and(|device| signing_identity.signature_key.as_ref() == device.signature_key))
    }

    pub fn is_client_active(&self, client_id: &[u8]) -> Result<bool, PolicyError> {
        Ok(self
            .state
            .read()
            .map_err(|_| PolicyError::LockPoisoned)?
            .devices
            .contains_key(client_id))
    }

    pub fn device_for_client(&self, client_id: &[u8]) -> Result<DeviceId, PolicyError> {
        self.state
            .read()
            .map_err(|_| PolicyError::LockPoisoned)?
            .devices
            .get(client_id)
            .map(|device| device.device_id)
            .ok_or(PolicyError::UnknownDevice)
    }

    pub fn account_for_client(&self, client_id: &[u8]) -> Result<AccountId, PolicyError> {
        let state = self.state.read().map_err(|_| PolicyError::LockPoisoned)?;
        if let Some(account_id) = state
            .devices
            .get(client_id)
            .map(|device| device.account_id)
        {
            return Ok(account_id);
        }
        parse_mls_client_id(&self.server_id, client_id)
            .map(|(account_id, _)| account_id)
            .map_err(|_| PolicyError::UnknownDevice)
    }

    pub fn user_for_account(&self, account_id: AccountId) -> Result<UserId, PolicyError> {
        let state = self.state.read().map_err(|_| PolicyError::LockPoisoned)?;
        state
            .devices
            .values()
            .find(|device| device.account_id == account_id)
            .map(|device| device.user_id)
            .ok_or(PolicyError::UnknownDevice)
    }
}

impl IdentityProvider for ChattIdentityProvider {
    type Error = PolicyError;

    fn validate_member(
        &self,
        signing_identity: &SigningIdentity,
        _timestamp: Option<MlsTime>,
        context: MemberValidationContext<'_>,
    ) -> Result<(), Self::Error> {
        let historical_group_id = match context {
            MemberValidationContext::ForNewGroup { current_context }
                if self.allow_historical_group_info =>
            {
                Some(current_context.group_id())
            }
            _ => None,
        };
        if let Some(group_id) = historical_group_id {
            return self.validate_in_group(signing_identity, Some(group_id)).or_else(|error| {
                if matches!(
                    &error,
                    PolicyError::UnknownDevice | PolicyError::SignatureKeyMismatch
                )
                {
                    self.validate_historical_in_group(signing_identity, group_id)
                } else {
                    Err(error)
                }
            });
        }
        let group_id = match context {
            MemberValidationContext::ForCommit {
                current_context, ..
            }
            | MemberValidationContext::ForNewGroup { current_context } => {
                Some(current_context.group_id())
            }
            MemberValidationContext::None => None,
            _ => None,
        };
        self.validate_in_group(signing_identity, group_id)
    }

    fn validate_external_sender(
        &self,
        signing_identity: &SigningIdentity,
        _timestamp: Option<MlsTime>,
        _extensions: Option<&ExtensionList>,
    ) -> Result<(), Self::Error> {
        self.validate_in_group(signing_identity, None)
    }

    fn identity(
        &self,
        signing_identity: &SigningIdentity,
        _extensions: &ExtensionList,
    ) -> Result<Vec<u8>, Self::Error> {
        match self.certificate_for(signing_identity) {
            Ok(_) => Ok(basic_client_id(signing_identity)?.to_vec()),
            Err(error) if self.allow_historical_group_info => {
                let client_id = basic_client_id(signing_identity)?;
                parse_mls_client_id(&self.server_id, client_id)
                    .map_err(|_| error)
                    .map(|_| client_id.to_vec())
            }
            Err(error) => Err(error),
        }
    }

    fn valid_successor(
        &self,
        predecessor: &SigningIdentity,
        successor: &SigningIdentity,
        _extensions: &ExtensionList,
    ) -> Result<bool, Self::Error> {
        self.certificate_for(successor)?;
        Ok(basic_client_id(predecessor)? == basic_client_id(successor)?)
    }

    fn supported_types(&self) -> Vec<CredentialType> {
        vec![BasicCredential::credential_type()]
    }
}

/// Fixed-membership commit policy shared by clients and the server observer.
#[derive(Clone, Debug)]
pub struct ChattMlsPolicy {
    identities: ChattIdentityProvider,
}

impl ChattMlsPolicy {
    pub fn new(identities: ChattIdentityProvider) -> Self {
        Self { identities }
    }

    fn validate_group_roster(
        &self,
        roster: &Roster,
        context: &mls_rs::group::GroupContext,
    ) -> Result<(), PolicyError> {
        let state = self
            .identities
            .state
            .read()
            .map_err(|_| PolicyError::LockPoisoned)?;
        let room = state
            .rooms
            .get(context.group_id())
            .ok_or(PolicyError::UnknownRoom)?;
        let mut clients = HashSet::new();
        let mut accounts = HashSet::new();
        for member in roster.members_iter() {
            let client_id = basic_client_id(&member.signing_identity)?;
            if !clients.insert(client_id.to_vec()) {
                return Err(PolicyError::InvalidGroupRoster);
            }
            let account = state
                .devices
                .get(client_id)
                .map(|device| device.account_id)
                .or_else(|| {
                    parse_mls_client_id(&self.identities.server_id, client_id)
                        .ok()
                        .map(|(account, _)| account)
                })
                .ok_or(PolicyError::UnknownDevice)?;
            if room.member_accounts.binary_search(&account).is_err() {
                return Err(PolicyError::AccountNotInRoom);
            }
            accounts.insert(account);
        }
        if accounts.len() != room.member_accounts.len()
            || room
                .member_accounts
                .iter()
                .any(|account| !accounts.contains(account))
        {
            return Err(PolicyError::InvalidGroupRoster);
        }
        Ok(())
    }
}

impl MlsRules for ChattMlsPolicy {
    type Error = PolicyError;

    fn filter_proposals(
        &self,
        _direction: CommitDirection,
        source: CommitSource,
        current_roster: &Roster,
        _current_context: &mls_rs::group::GroupContext,
        proposals: ProposalBundle,
    ) -> Result<ProposalBundle, Self::Error> {
        if proposals.by_type::<ReInitProposal>().next().is_some() {
            return Err(PolicyError::ReinitializationNotAllowed);
        }
        for removal in proposals.by_type::<RemoveProposal>() {
            let removed = current_roster
                .member_with_index(removal.proposal.to_remove())
                .map_err(|_| PolicyError::ActiveDeviceRemoval)?;
            if !self.identities.is_active(&removed.signing_identity)? {
                continue;
            }
            let is_external_replacement = match &source {
                CommitSource::NewMember(successor) => {
                    basic_client_id(successor)? == basic_client_id(&removed.signing_identity)?
                }
                CommitSource::ExistingMember(_) => false,
            };
            if !is_external_replacement {
                return Err(PolicyError::ActiveDeviceRemoval);
            }
        }
        Ok(proposals)
    }

    fn commit_options(
        &self,
        new_roster: &Roster,
        new_context: &mls_rs::group::GroupContext,
        _proposals: &ProposalBundle,
    ) -> Result<CommitOptions, Self::Error> {
        self.validate_group_roster(new_roster, new_context)?;
        Ok(CommitOptions::new()
            .with_path_required(true)
            .with_ratchet_tree_extension(true)
            .with_single_welcome_message(true)
            .with_allow_external_commit(true))
    }

    fn encryption_options(
        &self,
        _current_roster: &Roster,
        _current_context: &mls_rs::group::GroupContext,
    ) -> Result<EncryptionOptions, Self::Error> {
        // The default is public handshake messages and step-function padding
        // for encrypted application messages.
        Ok(EncryptionOptions::default())
    }
}

fn basic_client_id(signing_identity: &SigningIdentity) -> Result<&[u8], PolicyError> {
    signing_identity
        .credential
        .as_basic()
        .map(|credential| credential.identifier.as_slice())
        .ok_or(PolicyError::UnsupportedCredential)
}

fn certified_device(certificate: &SignedDeviceCertificate) -> CertifiedDevice {
    CertifiedDevice {
        account_id: certificate.body.account_id,
        user_id: certificate.body.user_id,
        device_id: certificate.body.device_id,
        signature_key: certificate.body.mls_signature_public_key.clone(),
    }
}

#[cfg(test)]
mod tests {
    use mls_rs_core::{
        crypto::SignaturePublicKey, extension::ExtensionList, identity::SigningIdentity,
    };
    use rpc::{
        identity::{
            DeviceCertificateBody, DeviceRosterBody, account_id, authority_public_key,
            mls_client_id, sign_device_certificate, sign_device_roster,
        },
        ids::{AccountId, DeviceId, RoomId, UserId},
        mls::EncryptedRoomDescriptor,
    };

    use super::*;

    fn roster(
        server: &[u8],
        user_id: UserId,
        authority_seed: &[u8; 32],
        device_id: DeviceId,
        signature_key: &[u8],
    ) -> SignedDeviceRoster {
        let authority_public_key = authority_public_key(authority_seed).unwrap();
        let account_id = account_id(server, user_id, &authority_public_key);
        let certificate = sign_device_certificate(
            DeviceCertificateBody {
                user_id,
                account_id,
                authority_public_key,
                device_id,
                device_name: "test device".to_string(),
                mls_client_id: mls_client_id(server, account_id, device_id).unwrap(),
                mls_signature_public_key: signature_key.to_vec(),
            },
            authority_seed,
        )
        .unwrap();
        sign_device_roster(
            DeviceRosterBody {
                user_id,
                account_id,
                authority_public_key,
                revision: 1,
                active_devices: vec![certificate],
            },
            authority_seed,
        )
        .unwrap()
    }

    fn signing_identity(roster: &SignedDeviceRoster) -> SigningIdentity {
        let certificate = &roster.body.active_devices[0].body;
        SigningIdentity::new(
            BasicCredential::new(certificate.mls_client_id.clone()).into_credential(),
            SignaturePublicKey::from(certificate.mls_signature_public_key.clone()),
        )
    }

    #[test]
    fn certified_signature_key_and_fixed_room_account_are_required() {
        let server = b"server";
        let alice = roster(server, UserId(1), &[1; 32], DeviceId([1; 16]), &[11; 32]);
        let bob = roster(server, UserId(2), &[2; 32], DeviceId([2; 16]), &[12; 32]);
        let outsider = roster(server, UserId(3), &[3; 32], DeviceId([3; 16]), &[13; 32]);
        let identities = ChattIdentityProvider::new(server.to_vec());
        identities.install_roster(&alice).unwrap();
        identities.install_roster(&bob).unwrap();
        identities.install_roster(&outsider).unwrap();
        let room = EncryptedRoomDescriptor::new(
            RoomId(4),
            alice.body.account_id,
            vec![alice.body.account_id, bob.body.account_id],
            10,
        )
        .unwrap();
        identities.install_room(room.clone()).unwrap();

        let context = mls_rs::group::GroupContext::new(
            mls_rs_core::protocol_version::ProtocolVersion::MLS_10,
            crate::CIPHER_SUITE,
            room.mls_group_id,
            Vec::new(),
            ExtensionList::new(),
        );
        identities
            .validate_member(
                &signing_identity(&alice),
                None,
                MemberValidationContext::ForNewGroup {
                    current_context: &context,
                },
            )
            .unwrap();
        assert_eq!(
            identities
                .validate_member(
                    &signing_identity(&outsider),
                    None,
                    MemberValidationContext::ForNewGroup {
                        current_context: &context,
                    },
                )
                .unwrap_err(),
            PolicyError::AccountNotInRoom
        );

        let mut wrong_key = signing_identity(&alice);
        wrong_key.signature_key = SignaturePublicKey::from(vec![99; 32]);
        assert_eq!(
            identities
                .validate_member(&wrong_key, None, MemberValidationContext::None)
                .unwrap_err(),
            PolicyError::SignatureKeyMismatch
        );
    }

    #[test]
    fn installing_new_roster_revision_revokes_old_leaf_immediately() {
        let server = b"server";
        let first = roster(server, UserId(1), &[1; 32], DeviceId([1; 16]), &[11; 32]);
        let identities = ChattIdentityProvider::new(server.to_vec());
        identities.install_roster(&first).unwrap();
        let old_identity = signing_identity(&first);
        assert!(identities.is_active(&old_identity).unwrap());

        let mut second = roster(server, UserId(1), &[1; 32], DeviceId([2; 16]), &[12; 32]);
        second.body.revision = 2;
        second = sign_device_roster(second.body, &[1; 32]).unwrap();
        identities.install_roster(&second).unwrap();
        identities.install_roster(&second).unwrap();
        assert!(!identities.is_active(&old_identity).unwrap());
        let current_identity = signing_identity(&second);
        assert!(identities.is_active(&current_identity).unwrap());
        assert!(matches!(
            identities.install_roster(&first),
            Err(PolicyError::InvalidRoster(message))
                if message.contains("rolls back")
        ));
        assert!(!identities.is_active(&old_identity).unwrap());
        assert!(identities.is_active(&current_identity).unwrap());

        let mut equivocation = roster(
            server,
            UserId(1),
            &[1; 32],
            DeviceId([3; 16]),
            &[13; 32],
        );
        equivocation.body.revision = 2;
        let equivocation = sign_device_roster(equivocation.body, &[1; 32]).unwrap();
        assert!(matches!(
            identities.install_roster(&equivocation),
            Err(PolicyError::InvalidRoster(message))
                if message.contains("equivocates")
        ));
        assert!(identities.is_active(&current_identity).unwrap());
        assert_eq!(
            identities
                .account_for_client(basic_client_id(&old_identity).unwrap())
                .unwrap(),
            first.body.account_id
        );
        let reopened = ChattIdentityProvider::new(server.to_vec());
        reopened.install_roster(&second).unwrap();
        assert_eq!(
            reopened
                .account_for_client(basic_client_id(&old_identity).unwrap())
                .unwrap(),
            first.body.account_id
        );
    }

    #[test]
    fn descriptor_type_remains_account_based() {
        let account = AccountId([1; 32]);
        assert!(
            EncryptedRoomDescriptor::new(RoomId(1), account, vec![account, AccountId([2; 32])], 1,)
                .is_ok()
        );
    }
}
