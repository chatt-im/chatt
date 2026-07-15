use std::fmt;

use jsony::Jsony;
use mls_rs::{
    CryptoProvider,
    MlsMessage,
    external_client::{
        ExternalClient, ExternalReceivedMessage, ExternalSnapshot,
        builder::{ExternalBaseConfig, WithCryptoProvider, WithIdentityProvider, WithMlsRules},
    },
    group::{CommitEffect, ContentType, proposal::Proposal},
    WireFormat,
};
use mls_rs_crypto_rustcrypto::RustCryptoProvider;
use rpc::mls::MlsCommitBundle;

use crate::{CIPHER_SUITE, ChattIdentityProvider, ChattMlsPolicy};

type ValidatorConfig = WithMlsRules<
    ChattMlsPolicy,
    WithIdentityProvider<
        ChattIdentityProvider,
        WithCryptoProvider<RustCryptoProvider, ExternalBaseConfig>,
    >,
>;

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct PublicGroupState {
    pub epoch: u64,
    pub group_id: Vec<u8>,
    pub tree_hash: Vec<u8>,
    pub transcript_hash: Vec<u8>,
    pub member_client_ids: Vec<Vec<u8>>,
    pub snapshot: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedPublicCommit {
    pub state: PublicGroupState,
    pub committer_client_id: Vec<u8>,
    pub added_key_package_refs: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub enum PublicValidationError {
    Decode(String),
    InvalidGroupInfo(String),
    InvalidCommit(String),
    InvalidKeyPackage(String),
    UnexpectedMessage,
}

impl fmt::Display for PublicValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "invalid MLS encoding: {error}"),
            Self::InvalidGroupInfo(error) => write!(f, "invalid MLS GroupInfo: {error}"),
            Self::InvalidCommit(error) => write!(f, "invalid MLS commit: {error}"),
            Self::InvalidKeyPackage(error) => write!(f, "invalid MLS KeyPackage: {error}"),
            Self::UnexpectedMessage => f.write_str("MLS message has the wrong content type"),
        }
    }
}

impl std::error::Error for PublicValidationError {}

/// Stateless factory for validating and advancing server-visible MLS state.
///
/// A fresh observed group is loaded for every operation. Callers only replace
/// the durable snapshot after the surrounding server transaction commits.
pub struct PublicGroupValidator {
    client: ExternalClient<ValidatorConfig>,
}

impl fmt::Debug for PublicGroupValidator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicGroupValidator")
            .finish_non_exhaustive()
    }
}

impl PublicGroupValidator {
    pub fn new(identities: ChattIdentityProvider) -> Self {
        let client = ExternalClient::builder()
            .crypto_provider(RustCryptoProvider::default())
            .identity_provider(identities.clone())
            .mls_rules(ChattMlsPolicy::new(identities))
            .build();
        Self { client }
    }

    pub fn new_historical_observer(identities: ChattIdentityProvider) -> Self {
        Self::new(identities.historical_group_info_observer())
    }

    pub fn observe_group_info(
        &self,
        group_info: &[u8],
    ) -> Result<PublicGroupState, PublicValidationError> {
        let message = MlsMessage::from_bytes(group_info)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        let group = self
            .client
            .observe_group(message, None, None)
            .map_err(|error| PublicValidationError::InvalidGroupInfo(error.to_string()))?;
        Ok(PublicGroupState {
            epoch: group.group_context().epoch(),
            group_id: group.group_context().group_id().to_vec(),
            tree_hash: group.tree_hash().to_vec(),
            transcript_hash: group.transcript_hash().to_vec(),
            member_client_ids: member_client_ids(group.roster())?,
            snapshot: group
                .snapshot()
                .to_bytes()
                .map_err(|error| PublicValidationError::Decode(error.to_string()))?,
        })
    }

    /// Returns the leaf index for an exact certified MLS client identifier in
    /// a GroupInfo. External rejoin uses this to replace stale local leaf
    /// state instead of adding a duplicate leaf for the same installation.
    pub fn member_index(
        &self,
        group_info: &[u8],
        client_id: &[u8],
    ) -> Result<Option<u32>, PublicValidationError> {
        let message = MlsMessage::from_bytes(group_info)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        let group = self
            .client
            .observe_group(message, None, None)
            .map_err(|error| PublicValidationError::InvalidGroupInfo(error.to_string()))?;
        for member in group.roster().members_iter() {
            let member_client_id = member
                .signing_identity
                .credential
                .as_basic()
                .map(|credential| credential.identifier.as_slice())
                .ok_or_else(|| {
                    PublicValidationError::InvalidGroupInfo(
                        "group member does not use a Basic credential".to_string(),
                    )
                })?;
            if member_client_id == client_id {
                return Ok(Some(member.index));
            }
        }
        Ok(None)
    }

    pub fn apply_commit(
        &self,
        current: &PublicGroupState,
        bundle: &MlsCommitBundle,
    ) -> Result<AppliedPublicCommit, PublicValidationError> {
        let stored = ExternalSnapshot::from_bytes(&current.snapshot)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        if stored.context().epoch() != current.epoch {
            return Err(PublicValidationError::Decode(
                "stored epoch does not match public snapshot".to_string(),
            ));
        }
        let mut group = self
            .client
            .load_group(stored)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        let commit = MlsMessage::from_bytes(&bundle.commit)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        match group.process_incoming_message(commit) {
            Ok(ExternalReceivedMessage::Commit(description)) => {
                let added_key_package_refs = added_key_package_refs(&description.effect)?;
                let committer_client_id = group
                    .roster()
                    .member_with_index(description.committer)
                    .map_err(|error| PublicValidationError::InvalidCommit(error.to_string()))?
                    .signing_identity
                    .credential
                    .as_basic()
                    .map(|credential| credential.identifier.clone())
                    .ok_or_else(|| {
                        PublicValidationError::InvalidCommit(
                            "committer does not use a Basic credential".to_string(),
                        )
                    })?;
                let next = PublicGroupState {
                    epoch: group.group_context().epoch(),
                    group_id: group.group_context().group_id().to_vec(),
                    tree_hash: group.tree_hash().to_vec(),
                    transcript_hash: group.transcript_hash().to_vec(),
                    member_client_ids: member_client_ids(group.roster())?,
                    snapshot: group
                        .snapshot()
                        .to_bytes()
                        .map_err(|error| PublicValidationError::Decode(error.to_string()))?,
                };
                let advertised = self.observe_group_info(&bundle.group_info)?;
                if next.epoch != advertised.epoch
                    || next.group_id != advertised.group_id
                    || next.tree_hash != advertised.tree_hash
                    || next.transcript_hash != advertised.transcript_hash
                {
                    return Err(PublicValidationError::InvalidCommit(
                        "resulting GroupInfo does not match the commit".to_string(),
                    ));
                }
                Ok(AppliedPublicCommit {
                    state: next,
                    committer_client_id,
                    added_key_package_refs,
                })
            }
            Ok(_) => Err(PublicValidationError::UnexpectedMessage),
            Err(error) => Err(PublicValidationError::InvalidCommit(error.to_string())),
        }
    }

    pub fn validate_application(
        &self,
        current: &PublicGroupState,
        encoded: &[u8],
    ) -> Result<(), PublicValidationError> {
        let stored = ExternalSnapshot::from_bytes(&current.snapshot)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        let mut group = self
            .client
            .load_group(stored)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        let message = MlsMessage::from_bytes(encoded)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        // `mls-rs` deliberately accepts retained-epoch application messages so
        // recipients can tolerate an unordered delivery service.  Submission
        // is a different boundary: accepting an old ciphertext after a commit
        // would let a sender bypass Chatt's stale-epoch retry and revocation
        // ordering simply by labelling the outer request with the current
        // epoch.
        if message.epoch() != Some(current.epoch)
            || message.group_id() != Some(current.group_id.as_slice())
        {
            return Err(PublicValidationError::InvalidCommit(
                "application message does not belong to the current group epoch".to_string(),
            ));
        }
        match group.process_incoming_message(message) {
            Ok(ExternalReceivedMessage::Ciphertext(ContentType::Application)) => Ok(()),
            Ok(_) => Err(PublicValidationError::UnexpectedMessage),
            Err(error) => Err(PublicValidationError::InvalidCommit(error.to_string())),
        }
    }

    pub fn validate_welcome(&self, encoded: &[u8]) -> Result<(), PublicValidationError> {
        let message = MlsMessage::from_bytes(encoded)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        if message.wire_format() != WireFormat::Welcome {
            return Err(PublicValidationError::UnexpectedMessage);
        }
        Ok(())
    }

    pub fn key_package_reference(
        &self,
        encoded: &[u8],
    ) -> Result<Vec<u8>, PublicValidationError> {
        self.validate_key_package(encoded)
            .map(|(reference, _)| reference)
    }

    /// Fully validates a KeyPackage and returns its reference plus the Basic
    /// credential client id authenticated by its leaf-node signature.
    pub fn validate_key_package(
        &self,
        encoded: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), PublicValidationError> {
        let message = MlsMessage::from_bytes(encoded)
            .map_err(|error| PublicValidationError::Decode(error.to_string()))?;
        let key_package = self
            .client
            .validate_key_package(message, None)
            .map_err(|error| PublicValidationError::InvalidKeyPackage(error.to_string()))?;
        let client_id = key_package
            .signing_identity()
            .credential
            .as_basic()
            .map(|credential| credential.identifier.clone())
            .ok_or(PublicValidationError::UnexpectedMessage)?;
        Ok((key_package_reference(&key_package)?, client_id))
    }
}

fn added_key_package_refs(
    effect: &CommitEffect,
) -> Result<Vec<Vec<u8>>, PublicValidationError> {
    let epoch = match effect {
        CommitEffect::NewEpoch(epoch) | CommitEffect::Removed { new_epoch: epoch, .. } => epoch,
        CommitEffect::ReInit(_) => return Ok(Vec::new()),
    };
    epoch
        .applied_proposals()
        .iter()
        .filter_map(|proposal| match &proposal.proposal {
            Proposal::Add(add) => Some(add.key_package()),
            _ => None,
        })
        .map(key_package_reference)
        .collect()
}

fn key_package_reference(
    key_package: &mls_rs::KeyPackage,
) -> Result<Vec<u8>, PublicValidationError> {
    let cipher = RustCryptoProvider::default()
        .cipher_suite_provider(CIPHER_SUITE)
        .ok_or_else(|| {
            PublicValidationError::Decode("mandatory cipher suite is unavailable".to_string())
        })?;
    key_package
        .to_reference(&cipher)
        .map(|reference| reference.to_vec())
        .map_err(|error| PublicValidationError::Decode(error.to_string()))
}

fn member_client_ids(
    roster: mls_rs::group::Roster<'_>,
) -> Result<Vec<Vec<u8>>, PublicValidationError> {
    roster
        .member_identities_iter()
        .map(|identity| {
            identity
                .credential
                .as_basic()
                .map(|credential| credential.identifier.clone())
                .ok_or(PublicValidationError::InvalidCommit(
                    "group member does not use a Basic credential".to_string(),
                ))
        })
        .collect()
}
