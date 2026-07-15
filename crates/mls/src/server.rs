use std::fmt;

use jsony::Jsony;
use mls_rs::{
    MlsMessage,
    external_client::{
        ExternalClient, ExternalReceivedMessage, ExternalSnapshot,
        builder::{ExternalBaseConfig, WithCryptoProvider, WithIdentityProvider, WithMlsRules},
    },
    group::ContentType,
};
use mls_rs_crypto_rustcrypto::RustCryptoProvider;
use rpc::mls::MlsCommitBundle;

use crate::{ChattIdentityProvider, ChattMlsPolicy};

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
}

#[derive(Debug)]
pub enum PublicValidationError {
    Decode(String),
    InvalidGroupInfo(String),
    InvalidCommit(String),
    UnexpectedMessage,
}

impl fmt::Display for PublicValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "invalid MLS encoding: {error}"),
            Self::InvalidGroupInfo(error) => write!(f, "invalid MLS GroupInfo: {error}"),
            Self::InvalidCommit(error) => write!(f, "invalid MLS commit: {error}"),
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
        match group.process_incoming_message(message) {
            Ok(ExternalReceivedMessage::Ciphertext(ContentType::Application)) => Ok(()),
            Ok(_) => Err(PublicValidationError::UnexpectedMessage),
            Err(error) => Err(PublicValidationError::InvalidCommit(error.to_string())),
        }
    }
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
