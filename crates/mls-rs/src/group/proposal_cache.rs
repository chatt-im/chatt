// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright by contributors to this project.
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use alloc::vec::Vec;

use super::{
    message_processor::ProvisionalState,
    mls_rules::{CommitDirection, CommitSource, MlsRules},
    proposal_filter::prepare_proposals_for_mls_rules,
    GroupState, ProposalOrRef,
};
use crate::{
    client::MlsError,
    group::{
        proposal_filter::{ProposalApplier, ProposalBundle, ProposalSource},
        Proposal, Sender,
    },
    time::MlsTime,
};

#[cfg(feature = "by_ref_proposal")]
use crate::{
    group::{message_hash::MessageHash, ProposalMessageDescription, ProposalRef, ProtocolVersion},
    MlsMessage,
};

use crate::tree_kem::leaf_node::LeafNode;

#[cfg(feature = "by_ref_proposal")]
use mls_rs_codec::{MlsDecode, MlsEncode, MlsSize};

use mls_rs_core::{
    crypto::CipherSuiteProvider, error::IntoAnyError, identity::IdentityProvider,
    psk::PreSharedKeyStorage,
};

#[cfg(feature = "by_ref_proposal")]
use core::fmt::{self, Debug};

#[cfg(feature = "by_ref_proposal")]
#[derive(Debug, Clone, MlsSize, MlsEncode, MlsDecode, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CachedProposal {
    pub(crate) proposal: Proposal,
    pub(crate) sender: Sender,
}

#[cfg(feature = "by_ref_proposal")]
#[derive(Clone, MlsSize, MlsEncode, MlsDecode)]
pub(crate) struct ProposalCache {
    protocol_version: ProtocolVersion,
    group_id: Vec<u8>,
    pub(crate) proposals: crate::map::SmallMap<ProposalRef, CachedProposal>,
    pub(crate) own_proposals: crate::map::SmallMap<MessageHash, ProposalMessageDescription>,
}

#[cfg(feature = "by_ref_proposal")]
impl PartialEq for ProposalCache {
    fn eq(&self, other: &Self) -> bool {
        self.protocol_version == other.protocol_version
            && self.group_id == other.group_id
            && self.proposals == other.proposals
    }
}

#[cfg(feature = "by_ref_proposal")]
impl Debug for ProposalCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProposalCache")
            .field("protocol_version", &self.protocol_version)
            .field(
                "group_id",
                &mls_rs_core::debug::pretty_group_id(&self.group_id),
            )
            .field("proposals", &self.proposals)
            .finish()
    }
}

#[cfg(feature = "by_ref_proposal")]
impl ProposalCache {
    pub fn new(protocol_version: ProtocolVersion, group_id: Vec<u8>) -> Self {
        Self {
            protocol_version,
            group_id,
            proposals: Default::default(),
            own_proposals: Default::default(),
        }
    }

    pub fn import(
        protocol_version: ProtocolVersion,
        group_id: Vec<u8>,
        proposals: crate::map::SmallMap<ProposalRef, CachedProposal>,
        own_proposals: crate::map::SmallMap<MessageHash, ProposalMessageDescription>,
    ) -> Self {
        Self {
            protocol_version,
            group_id,
            proposals,
            own_proposals,
        }
    }

    pub fn clear(&mut self) {
        self.proposals.clear();
        self.own_proposals.clear();
    }

    #[cfg(feature = "by_ref_proposal")]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.proposals.is_empty()
    }

    pub fn insert(&mut self, proposal_ref: ProposalRef, proposal: Proposal, sender: Sender) {
        let cached_proposal = CachedProposal { proposal, sender };

        #[cfg(feature = "std")]
        self.proposals.insert(proposal_ref, cached_proposal);

        #[cfg(not(feature = "std"))]
        // This may result in dups but it does not matter
        self.proposals.push((proposal_ref, cached_proposal));
    }
    pub fn insert_own<CS: CipherSuiteProvider>(
        &mut self,
        proposal: ProposalMessageDescription,
        message: &MlsMessage,
        sender: Sender,
        cs: &CS,
    ) -> Result<(), MlsError> {
        self.insert(
            proposal.proposal_ref.clone(),
            proposal.proposal.clone(),
            sender,
        );

        let message_hash = MessageHash::compute(cs, message)?;
        self.own_proposals.insert(message_hash, proposal);

        Ok(())
    }

    #[cfg(all(
        feature = "by_ref_proposal",
        feature = "custom_proposal",
        feature = "self_remove_proposal"
    ))]
    pub(crate) fn has_own_self_remove(&self) -> bool {
        self.own_proposals
            .values()
            .any(|p| matches!(p.proposal, Proposal::SelfRemove(_)))
    }

    pub fn prepare_commit(
        &self,
        sender: Sender,
        additional_proposals: Vec<Proposal>,
    ) -> ProposalBundle {
        self.proposals
            .iter()
            .map(|(r, p)| {
                (
                    p.proposal.clone(),
                    p.sender,
                    ProposalSource::ByReference(r.clone()),
                )
            })
            .chain(
                additional_proposals
                    .into_iter()
                    .map(|p| (p, sender, ProposalSource::ByValue)),
            )
            .collect()
    }

    pub fn resolve_for_commit(
        &self,
        sender: Sender,
        proposal_list: Vec<ProposalOrRef>,
    ) -> Result<ProposalBundle, MlsError> {
        let mut proposals = ProposalBundle::default();

        for p in proposal_list {
            match p {
                ProposalOrRef::Proposal(p) => proposals.add(*p, sender, ProposalSource::ByValue),
                ProposalOrRef::Reference(r) => {
                    #[cfg(feature = "std")]
                    let p = self
                        .proposals
                        .get(&r)
                        .ok_or(MlsError::ProposalNotFound)?
                        .clone();
                    #[cfg(not(feature = "std"))]
                    let p = self
                        .proposals
                        .iter()
                        .find_map(|(rr, p)| (rr == &r).then_some(p))
                        .ok_or(MlsError::ProposalNotFound)?
                        .clone();

                    proposals.add(p.proposal, p.sender, ProposalSource::ByReference(r));
                }
            };
        }

        Ok(proposals)
    }
    pub fn get_own<CS: CipherSuiteProvider>(
        &self,
        cs: &CS,
        message: &MlsMessage,
    ) -> Result<Option<ProposalMessageDescription>, MlsError> {
        let message_hash = MessageHash::compute(cs, message)?;

        Ok(self.own_proposals.get(&message_hash).cloned())
    }
}

#[cfg(not(feature = "by_ref_proposal"))]
pub(crate) fn prepare_commit(
    sender: Sender,
    additional_proposals: Vec<Proposal>,
) -> ProposalBundle {
    let mut proposals = ProposalBundle::default();

    for p in additional_proposals.into_iter() {
        proposals.add(p, sender, ProposalSource::ByValue);
    }

    proposals
}

#[cfg(not(feature = "by_ref_proposal"))]
pub(crate) fn resolve_for_commit(
    sender: Sender,
    proposal_list: Vec<ProposalOrRef>,
) -> Result<ProposalBundle, MlsError> {
    let mut proposals = ProposalBundle::default();

    for p in proposal_list {
        let ProposalOrRef::Proposal(p) = p;
        proposals.add(*p, sender, ProposalSource::ByValue);
    }

    Ok(proposals)
}

impl GroupState {
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_resolved<C, F, P, CSP>(
        &self,
        sender: Sender,
        mut proposals: ProposalBundle,
        external_leaf: Option<&LeafNode>,
        identity_provider: &C,
        cipher_suite_provider: &CSP,
        psk_storage: &P,
        user_rules: &F,
        commit_time: Option<MlsTime>,
        direction: CommitDirection,
    ) -> Result<ProvisionalState, MlsError>
    where
        C: IdentityProvider,
        F: MlsRules,
        P: PreSharedKeyStorage,
        CSP: CipherSuiteProvider,
    {
        let roster = self.public_tree.roster();

        #[cfg(feature = "by_ref_proposal")]
        let all_proposals = proposals.clone();

        let origin = match sender {
            Sender::Member(index) => Ok::<_, MlsError>(CommitSource::ExistingMember(
                roster.member_with_index(index)?,
            )),
            #[cfg(feature = "by_ref_proposal")]
            Sender::NewMemberProposal => Err(MlsError::InvalidSender),
            #[cfg(feature = "by_ref_proposal")]
            Sender::External(_) => Err(MlsError::InvalidSender),
            Sender::NewMemberCommit => Ok(CommitSource::NewMember(
                external_leaf
                    .map(|l| l.signing_identity.clone())
                    .ok_or(MlsError::ExternalCommitMustHaveNewLeaf)?,
            )),
        }?;

        prepare_proposals_for_mls_rules(&mut proposals, direction, &self.public_tree)?;

        proposals = user_rules
            .filter_proposals(direction, origin, &roster, &self.context, proposals)

            .map_err(|e| MlsError::MlsRulesError(e.into_any_error()))?;

        let applier = ProposalApplier::new(
            &self.public_tree,
            cipher_suite_provider,
            &self.context,
            external_leaf,
            identity_provider,
            psk_storage,
        );

        #[cfg(feature = "by_ref_proposal")]
        let applier_output = applier
            .apply_proposals(direction.into(), &sender, proposals, commit_time)
            ?;

        #[cfg(not(feature = "by_ref_proposal"))]
        let applier_output = applier
            .apply_proposals(&sender, &proposals, commit_time)
            ?;

        #[cfg(feature = "by_ref_proposal")]
        let unused_proposals = unused_proposals(
            match direction {
                CommitDirection::Send => all_proposals,
                CommitDirection::Receive => self.proposals.proposals.iter().collect(),
            },
            &applier_output.applied_proposals,
        );

        #[cfg(not(feature = "by_ref_proposal"))]
        let unused_proposals = alloc::vec::Vec::default();

        let mut group_context = self.context.clone();
        group_context.epoch += 1;

        if let Some(ext) = applier_output.new_context_extensions {
            group_context.extensions = ext;
        }

        #[cfg(feature = "by_ref_proposal")]
        let proposals = applier_output.applied_proposals;

        Ok(ProvisionalState {
            public_tree: applier_output.new_tree,
            group_context,
            applied_proposals: proposals,
            external_init_index: applier_output.external_init_index,
            indexes_of_added_kpkgs: applier_output.indexes_of_added_kpkgs,
            unused_proposals,
        })
    }
}

#[cfg(feature = "by_ref_proposal")]
impl Extend<(ProposalRef, CachedProposal)> for ProposalCache {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = (ProposalRef, CachedProposal)>,
    {
        self.proposals.extend(iter);
    }
}

#[cfg(feature = "by_ref_proposal")]
fn has_ref(proposals: &ProposalBundle, reference: &ProposalRef) -> bool {
    proposals
        .iter_proposals()
        .any(|p| matches!(&p.source, ProposalSource::ByReference(r) if r == reference))
}

#[cfg(feature = "by_ref_proposal")]
fn unused_proposals(
    all_proposals: ProposalBundle,
    accepted_proposals: &ProposalBundle,
) -> Vec<crate::mls_rules::ProposalInfo<Proposal>> {
    all_proposals
        .into_proposals()
        .filter(|p| {
            matches!(p.source, ProposalSource::ByReference(ref r) if !has_ref(accepted_proposals, r)
            )
        })
        .collect()
}

// TODO add tests for lite version of filtering
#[cfg(all(feature = "by_ref_proposal", test))]
pub(crate) mod test_utils {
    use mls_rs_core::{
        crypto::CipherSuiteProvider, extension::ExtensionList, identity::IdentityProvider,
        psk::PreSharedKeyStorage,
    };

    use crate::{
        client::test_utils::TEST_PROTOCOL_VERSION,
        group::{
            confirmation_tag::ConfirmationTag,
            mls_rules::{CommitDirection, DefaultMlsRules, MlsRules},
            proposal::{Proposal, ProposalOrRef},
            proposal_ref::ProposalRef,
            state::GroupState,
            test_utils::{get_test_group_context, TEST_GROUP},
            GroupContext, LeafIndex, LeafNode, ProvisionalState, Sender, TreeKemPublic,
        },
        identity::{basic::BasicIdentityProvider, test_utils::BasicWithCustomProvider},
        psk::AlwaysFoundPskStorage,
    };

    use super::{CachedProposal, MlsError, ProposalCache};

    use alloc::vec::Vec;

    impl CachedProposal {
        pub fn new(proposal: Proposal, sender: Sender) -> Self {
            Self { proposal, sender }
        }
    }

    #[derive(Debug)]
    pub(crate) struct CommitReceiver<'a, C, F, P, CSP> {
        tree: &'a TreeKemPublic,
        sender: Sender,
        receiver: LeafIndex,
        cache: ProposalCache,
        identity_provider: C,
        cipher_suite_provider: CSP,
        group_context_extensions: ExtensionList,
        user_rules: F,
        with_psk_storage: P,
    }

    impl<'a, CSP>
        CommitReceiver<'a, BasicWithCustomProvider, DefaultMlsRules, AlwaysFoundPskStorage, CSP>
    {
        pub fn new<S>(
            tree: &'a TreeKemPublic,
            sender: S,
            receiver: LeafIndex,
            cipher_suite_provider: CSP,
        ) -> Self
        where
            S: Into<Sender>,
        {
            Self {
                tree,
                sender: sender.into(),
                receiver,
                cache: make_proposal_cache(),
                identity_provider: BasicWithCustomProvider::new(BasicIdentityProvider),
                group_context_extensions: Default::default(),
                user_rules: pass_through_rules(),
                with_psk_storage: AlwaysFoundPskStorage,
                cipher_suite_provider,
            }
        }
    }

    impl<'a, C, F, P, CSP> CommitReceiver<'a, C, F, P, CSP>
    where
        C: IdentityProvider,
        F: MlsRules,
        P: PreSharedKeyStorage,
        CSP: CipherSuiteProvider,
    {
        #[cfg(feature = "by_ref_proposal")]
        pub fn with_identity_provider<V>(self, validator: V) -> CommitReceiver<'a, V, F, P, CSP>
        where
            V: IdentityProvider,
        {
            CommitReceiver {
                tree: self.tree,
                sender: self.sender,
                receiver: self.receiver,
                cache: self.cache,
                identity_provider: validator,
                group_context_extensions: self.group_context_extensions,
                user_rules: self.user_rules,
                with_psk_storage: self.with_psk_storage,
                cipher_suite_provider: self.cipher_suite_provider,
            }
        }

        pub fn with_user_rules<G>(self, f: G) -> CommitReceiver<'a, C, G, P, CSP>
        where
            G: MlsRules,
        {
            CommitReceiver {
                tree: self.tree,
                sender: self.sender,
                receiver: self.receiver,
                cache: self.cache,
                identity_provider: self.identity_provider,
                group_context_extensions: self.group_context_extensions,
                user_rules: f,
                with_psk_storage: self.with_psk_storage,
                cipher_suite_provider: self.cipher_suite_provider,
            }
        }

        pub fn with_psk_storage<V>(self, v: V) -> CommitReceiver<'a, C, F, V, CSP>
        where
            V: PreSharedKeyStorage,
        {
            CommitReceiver {
                tree: self.tree,
                sender: self.sender,
                receiver: self.receiver,
                cache: self.cache,
                identity_provider: self.identity_provider,
                group_context_extensions: self.group_context_extensions,
                user_rules: self.user_rules,
                with_psk_storage: v,
                cipher_suite_provider: self.cipher_suite_provider,
            }
        }

        #[cfg(feature = "by_ref_proposal")]
        pub fn with_extensions(self, extensions: ExtensionList) -> Self {
            Self {
                group_context_extensions: extensions,
                ..self
            }
        }

        pub fn cache<S>(mut self, r: ProposalRef, p: Proposal, proposer: S) -> Self
        where
            S: Into<Sender>,
        {
            self.cache.insert(r, p, proposer.into());
            self
        }
        pub fn receive<I>(&self, proposals: I) -> Result<ProvisionalState, MlsError>
        where
            I: IntoIterator,
            I::Item: Into<ProposalOrRef>,
        {
            self.cache
                .resolve_for_commit_default(
                    self.sender,
                    proposals.into_iter().map(Into::into).collect(),
                    None,
                    &self.group_context_extensions,
                    &self.identity_provider,
                    &self.cipher_suite_provider,
                    self.tree,
                    &self.with_psk_storage,
                    &self.user_rules,
                )

        }
    }

    pub(crate) fn make_proposal_cache() -> ProposalCache {
        ProposalCache::new(TEST_PROTOCOL_VERSION, TEST_GROUP.to_vec())
    }

    pub fn pass_through_rules() -> DefaultMlsRules {
        DefaultMlsRules::new()
    }

    impl ProposalCache {
        #[allow(clippy::too_many_arguments)]
        pub fn resolve_for_commit_default<C, F, P, CSP>(
            &self,
            sender: Sender,
            proposal_list: Vec<ProposalOrRef>,
            external_leaf: Option<&LeafNode>,
            group_extensions: &ExtensionList,
            identity_provider: &C,
            cipher_suite_provider: &CSP,
            public_tree: &TreeKemPublic,
            psk_storage: &P,
            user_rules: F,
        ) -> Result<ProvisionalState, MlsError>
        where
            C: IdentityProvider,
            F: MlsRules,
            P: PreSharedKeyStorage,
            CSP: CipherSuiteProvider,
        {
            let mut context =
                get_test_group_context(123, cipher_suite_provider.cipher_suite());

            context.extensions = group_extensions.clone();

            let mut state = GroupState::new(
                context,
                public_tree.clone(),
                Vec::new().into(),
                ConfirmationTag::empty(cipher_suite_provider),
            );

            state.proposals.proposals.clone_from(&self.proposals);
            let proposals = self.resolve_for_commit(sender, proposal_list)?;

            state
                .apply_resolved(
                    sender,
                    proposals,
                    external_leaf,
                    identity_provider,
                    cipher_suite_provider,
                    psk_storage,
                    &user_rules,
                    None,
                    CommitDirection::Receive,
                )

        }

        #[allow(clippy::too_many_arguments)]
        pub fn prepare_commit_default<C, F, P, CSP>(
            &self,
            sender: Sender,
            additional_proposals: Vec<Proposal>,
            context: &GroupContext,
            identity_provider: &C,
            cipher_suite_provider: &CSP,
            public_tree: &TreeKemPublic,
            external_leaf: Option<&LeafNode>,
            psk_storage: &P,
            user_rules: F,
        ) -> Result<ProvisionalState, MlsError>
        where
            C: IdentityProvider,
            F: MlsRules,
            P: PreSharedKeyStorage,
            CSP: CipherSuiteProvider,
        {
            let state = GroupState::new(
                context.clone(),
                public_tree.clone(),
                Vec::new().into(),
                ConfirmationTag::empty(cipher_suite_provider),
            );

            let proposals = self.prepare_commit(sender, additional_proposals);

            state
                .apply_resolved(
                    sender,
                    proposals,
                    external_leaf,
                    identity_provider,
                    cipher_suite_provider,
                    psk_storage,
                    &user_rules,
                    None,
                    CommitDirection::Send,
                )

        }
    }
}

// TODO add tests for lite version of filtering
#[cfg(all(feature = "by_ref_proposal", test))]
mod tests {
    use alloc::{boxed::Box, vec, vec::Vec};

    use super::test_utils::{make_proposal_cache, pass_through_rules, CommitReceiver};
    use super::{CachedProposal, ProposalCache};
    use crate::client::MlsError;
    use crate::group::message_processor::ProvisionalState;
    use crate::group::mls_rules::{CommitDirection, CommitSource, EncryptionOptions};
    use crate::group::proposal_filter::{ProposalBundle, ProposalInfo, ProposalSource};
    use crate::group::proposal_ref::test_utils::auth_content_from_proposal;
    use crate::group::proposal_ref::ProposalRef;
    use crate::group::{
        AddProposal, AuthenticatedContent, Content, ExternalInit, GroupContext, Proposal,
        ProposalOrRef, ReInitProposal, RemoveProposal, Roster, Sender, UpdateProposal,
    };
    use crate::key_package::test_utils::test_key_package_with_signer;
    use crate::signer::Signable;
    use crate::tree_kem::leaf_node::LeafNode;
    use crate::tree_kem::node::LeafIndex;
    use crate::tree_kem::TreeKemPublic;
    use crate::{
        client::test_utils::{TEST_CIPHER_SUITE, TEST_PROTOCOL_VERSION},
        crypto::{self, test_utils::test_cipher_suite_provider},
        extension::test_utils::TestExtension,
        group::{
            message_processor::path_update_required,
            proposal_filter::proposer_can_propose,
            test_utils::{get_test_group_context, random_bytes, test_group, TEST_GROUP},
        },
        identity::basic::BasicIdentityProvider,
        identity::test_utils::{get_test_signing_identity, BasicWithCustomProvider},
        key_package::{test_utils::test_key_package, KeyPackageGenerator},
        mls_rules::{CommitOptions, DefaultMlsRules},
        psk::AlwaysFoundPskStorage,
        tree_kem::{
            leaf_node::{
                test_utils::{
                    default_properties, get_basic_test_node, get_basic_test_node_capabilities,
                    get_basic_test_node_sig_key, get_test_capabilities,
                },
                ConfigProperties, LeafNodeSigningContext, LeafNodeSource,
            },
            Lifetime,
        },
    };
    use crate::{KeyPackage, MlsRules};

    use crate::extension::RequiredCapabilitiesExt;

    #[cfg(feature = "by_ref_proposal")]
    use crate::{
        extension::ExternalSendersExt,
        tree_kem::leaf_node_validator::test_utils::FailureIdentityProvider,
    };

    #[cfg(feature = "psk")]
    use crate::{
        group::proposal::PreSharedKeyProposal,
        psk::{
            ExternalPskId, JustPreSharedKeyID, PreSharedKeyID, PskGroupId, PskNonce,
            ResumptionPSKUsage, ResumptionPsk,
        },
    };

    #[cfg(feature = "custom_proposal")]
    use crate::group::proposal::CustomProposal;

    use assert_matches::assert_matches;
    use core::convert::Infallible;
    use itertools::Itertools;
    use mls_rs_core::crypto::{CipherSuite, CipherSuiteProvider};
    use mls_rs_core::extension::ExtensionList;
    use mls_rs_core::group::{Capabilities, ProposalType};
    use mls_rs_core::identity::IdentityProvider;
    use mls_rs_core::protocol_version::ProtocolVersion;
    use mls_rs_core::psk::{PreSharedKey, PreSharedKeyStorage};
    use mls_rs_core::{
        extension::MlsExtension,
        identity::{Credential, CredentialType, CustomCredential},
    };

    fn test_sender() -> u32 {
        1
    }
    fn new_tree_custom_proposals(
        name: &str,
        proposal_types: Vec<ProposalType>,
    ) -> (LeafIndex, TreeKemPublic) {
        let (leaf, secret, _) = get_basic_test_node_capabilities(
            TEST_CIPHER_SUITE,
            name,
            Capabilities {
                proposals: proposal_types,
                ..get_test_capabilities()
            },
        )
        ;

        let (pub_tree, priv_tree) =
            TreeKemPublic::derive(leaf, secret, &BasicIdentityProvider, &Default::default())

                .unwrap();

        (priv_tree.self_index, pub_tree)
    }
    fn new_tree(name: &str) -> (LeafIndex, TreeKemPublic) {
        new_tree_custom_proposals(name, vec![])
    }
    fn add_member(tree: &mut TreeKemPublic, name: &str) -> LeafIndex {
        let test_node = get_basic_test_node(TEST_CIPHER_SUITE, name);

        tree.add_leaves(
            vec![test_node],
            &BasicIdentityProvider,
            &test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )

        .unwrap()[0]
    }
    fn update_leaf_node(name: &str, leaf_index: u32) -> LeafNode {
        let (mut leaf, _, signer) = get_basic_test_node_sig_key(TEST_CIPHER_SUITE, name);

        leaf.update(
            &test_cipher_suite_provider(TEST_CIPHER_SUITE),
            TEST_GROUP,
            leaf_index,
            Some(default_properties()),
            None,
            &signer,
        )

        .unwrap();

        leaf
    }

    struct TestProposals {
        test_sender: u32,
        test_proposals: Vec<AuthenticatedContent>,
        expected_effects: ProvisionalState,
        tree: TreeKemPublic,
    }
    fn test_proposals(
        protocol_version: ProtocolVersion,
        cipher_suite: CipherSuite,
    ) -> TestProposals {
        let cipher_suite_provider = test_cipher_suite_provider(cipher_suite);

        let (sender_leaf, sender_leaf_secret, _) =
            get_basic_test_node_sig_key(cipher_suite, "alice");

        let sender = LeafIndex::unchecked(0);

        let (mut tree, _) = TreeKemPublic::derive(
            sender_leaf,
            sender_leaf_secret,
            &BasicIdentityProvider,
            &Default::default(),
        )

        .unwrap();

        let add_package = test_key_package(protocol_version, cipher_suite, "dave");

        let remove_leaf_index = add_member(&mut tree, "carol");

        let add = Proposal::Add(Box::new(AddProposal {
            key_package: add_package.clone(),
        }));

        let remove = Proposal::Remove(RemoveProposal {
            to_remove: remove_leaf_index,
        });

        let extensions = Proposal::GroupContextExtensions(ExtensionList::new());

        let proposals = [add, remove, extensions];

        let test_node = get_basic_test_node(cipher_suite, "charlie");

        let test_sender = *tree
            .add_leaves(
                vec![test_node],
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )

            .unwrap()[0];

        let mut expected_tree = tree.clone();

        let mut bundle = ProposalBundle::default();

        let plaintext = proposals
            .iter()
            .cloned()
            .map(|p| auth_content_from_proposal(p, sender))
            .collect_vec();

        for i in 0..proposals.len() {
            let pref = ProposalRef::from_content(&cipher_suite_provider, &plaintext[i])

                .unwrap();

            bundle.add(
                proposals[i].clone(),
                Sender::Member(test_sender),
                ProposalSource::ByReference(pref),
            )
        }

        expected_tree
            .batch_edit(
                &mut bundle,
                &Default::default(),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                true,
            )

            .unwrap();

        let expected_effects = ProvisionalState {
            public_tree: expected_tree,
            group_context: get_test_group_context(1, cipher_suite),
            external_init_index: None,
            indexes_of_added_kpkgs: vec![LeafIndex::unchecked(1)],
            unused_proposals: vec![],
            applied_proposals: bundle,
        };

        TestProposals {
            test_sender,
            test_proposals: plaintext,
            expected_effects,
            tree,
        }
    }
    fn filter_proposals(
        cipher_suite: CipherSuite,
        proposals: Vec<AuthenticatedContent>,
    ) -> Vec<(ProposalRef, CachedProposal)> {
        let mut contents = Vec::new();

        for p in proposals {
            if let Content::Proposal(proposal) = &p.content.content {
                let proposal_ref =
                    ProposalRef::from_content(&test_cipher_suite_provider(cipher_suite), &p)

                        .unwrap();
                contents.push((
                    proposal_ref,
                    CachedProposal::new(proposal.as_ref().clone(), p.content.sender),
                ));
            }
        }

        contents
    }
    fn make_proposal_ref<S>(p: &Proposal, sender: S) -> ProposalRef
    where
        S: Into<Sender>,
    {
        ProposalRef::from_content(
            &test_cipher_suite_provider(TEST_CIPHER_SUITE),
            &auth_content_from_proposal(p.clone(), sender),
        )

        .unwrap()
    }
    fn make_proposal_info<S>(p: &Proposal, sender: S) -> ProposalInfo<Proposal>
    where
        S: Into<Sender> + Clone,
    {
        ProposalInfo {
            proposal: p.clone(),
            sender: sender.clone().into(),
            source: ProposalSource::ByReference(make_proposal_ref(p, sender)),
        }
    }
    fn test_proposal_cache_setup(proposals: Vec<AuthenticatedContent>) -> ProposalCache {
        let mut cache = make_proposal_cache();
        cache.extend(filter_proposals(TEST_CIPHER_SUITE, proposals));
        cache
    }

    fn assert_matches(mut expected_state: ProvisionalState, state: ProvisionalState) {
        let expected_proposals = expected_state.applied_proposals.proposals_or_refs();
        let proposals = state.applied_proposals.proposals_or_refs();

        assert_eq!(proposals.len(), expected_proposals.len());

        // Determine there are no duplicates in the proposals returned
        assert!(!proposals.iter().enumerate().any(|(i, p1)| proposals
            .iter()
            .enumerate()
            .any(|(j, p2)| p1 == p2 && i != j)),);

        // Proposal order may change so we just compare the length and contents are the same
        expected_proposals
            .iter()
            .for_each(|p| assert!(proposals.contains(p)));

        assert_eq!(
            expected_state.external_init_index,
            state.external_init_index
        );

        // We don't compare the epoch in this test.
        expected_state.group_context.epoch = state.group_context.epoch;
        assert_eq!(expected_state.group_context, state.group_context);

        assert_eq!(
            expected_state.indexes_of_added_kpkgs,
            state.indexes_of_added_kpkgs
        );

        assert_eq!(expected_state.public_tree, state.public_tree);

        assert_eq!(expected_state.unused_proposals, state.unused_proposals);
    }

    #[test]
    fn test_proposal_cache_commit_all_cached() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let TestProposals {
            test_sender,
            test_proposals,
            expected_effects,
            tree,
            ..
        } = test_proposals(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);

        let cache = test_proposal_cache_setup(test_proposals.clone());

        let provisional_state = cache
            .prepare_commit_default(
                Sender::Member(test_sender),
                vec![],
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert_matches(expected_effects, provisional_state)
    }

    #[test]
    fn test_proposal_cache_commit_additional() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let TestProposals {
            test_sender,
            test_proposals,
            mut expected_effects,
            tree,
            ..
        } = test_proposals(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);

        let additional_key_package =
            test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "frank");

        let additional = AddProposal {
            key_package: additional_key_package.clone(),
        };

        let cache = test_proposal_cache_setup(test_proposals.clone());

        let provisional_state = cache
            .prepare_commit_default(
                Sender::Member(test_sender),
                vec![Proposal::Add(Box::new(additional.clone()))],
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        expected_effects.applied_proposals.add(
            Proposal::Add(Box::new(additional.clone())),
            Sender::Member(test_sender),
            ProposalSource::ByValue,
        );

        let leaf = vec![additional_key_package.leaf_node.clone()];

        expected_effects
            .public_tree
            .add_leaves(leaf, &BasicIdentityProvider, &cipher_suite_provider)

            .unwrap();

        expected_effects
            .indexes_of_added_kpkgs
            .push(LeafIndex::unchecked(3));

        assert_matches(expected_effects, provisional_state);
    }

    #[test]
    fn test_proposal_cache_update_filter() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let TestProposals {
            test_proposals,
            tree,
            ..
        } = test_proposals(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);

        let update_proposal = make_update_proposal("foo");

        let additional = vec![Proposal::Update(update_proposal)];

        let cache = test_proposal_cache_setup(test_proposals);

        let res = cache
            .prepare_commit_default(
                Sender::Member(test_sender()),
                additional,
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(res, Err(MlsError::InvalidProposalTypeForSender));
    }

    #[test]
    fn test_proposal_cache_removal_override_update() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let TestProposals {
            test_sender,
            test_proposals,
            tree,
            ..
        } = test_proposals(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);

        let update = Proposal::Update(make_update_proposal("foo"));
        let update_proposal_ref = make_proposal_ref(&update, LeafIndex::unchecked(1));
        let mut cache = test_proposal_cache_setup(test_proposals);

        cache.insert(update_proposal_ref.clone(), update, Sender::Member(1));

        let provisional_state = cache
            .prepare_commit_default(
                Sender::Member(test_sender),
                vec![],
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert!(provisional_state
            .applied_proposals
            .removals
            .iter()
            .any(|p| *p.proposal.to_remove == 1));

        assert!(!provisional_state
            .applied_proposals
            .proposals_or_refs()
            .contains(&ProposalOrRef::Reference(update_proposal_ref)))
    }

    #[test]
    fn test_proposal_cache_filter_duplicates_insert() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let TestProposals {
            test_sender,
            test_proposals,
            expected_effects,
            tree,
            ..
        } = test_proposals(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);

        let mut cache = test_proposal_cache_setup(test_proposals.clone());
        cache.extend(filter_proposals(TEST_CIPHER_SUITE, test_proposals.clone()));

        let provisional_state = cache
            .prepare_commit_default(
                Sender::Member(test_sender),
                vec![],
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert_matches(expected_effects, provisional_state)
    }

    #[test]
    fn test_proposal_cache_filter_duplicates_additional() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let TestProposals {
            test_proposals,
            expected_effects,
            tree,
            ..
        } = test_proposals(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);

        let mut cache = test_proposal_cache_setup(test_proposals.clone());

        // Updates from different senders will be allowed so we test duplicates for add / remove
        let additional = test_proposals
            .clone()
            .into_iter()
            .filter_map(|plaintext| match plaintext.content.content {
                Content::Proposal(p) if p.proposal_type() == ProposalType::UPDATE => None,
                Content::Proposal(_) => Some(plaintext),
                _ => None,
            })
            .collect::<Vec<_>>();

        cache.extend(filter_proposals(TEST_CIPHER_SUITE, additional));

        let provisional_state = cache
            .prepare_commit_default(
                Sender::Member(2),
                Vec::new(),
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert_matches(expected_effects, provisional_state)
    }

    #[cfg(feature = "private_message")]
    #[test]
    fn test_proposal_cache_is_empty() {
        let mut cache = make_proposal_cache();
        assert!(cache.is_empty());

        let test_proposal = Proposal::Remove(RemoveProposal {
            to_remove: LeafIndex::unchecked(test_sender()),
        });

        let proposer = test_sender();
        let test_proposal_ref =
            make_proposal_ref(&test_proposal, LeafIndex::unchecked(proposer));
        cache.insert(test_proposal_ref, test_proposal, Sender::Member(proposer));

        assert!(!cache.is_empty())
    }

    #[test]
    fn test_proposal_cache_resolve() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let TestProposals {
            test_sender,
            test_proposals,
            tree,
            ..
        } = test_proposals(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);

        let cache = test_proposal_cache_setup(test_proposals);

        let proposal = Proposal::Add(Box::new(AddProposal {
            key_package: test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "frank"),
        }));

        let additional = vec![proposal];

        let expected_effects = cache
            .prepare_commit_default(
                Sender::Member(test_sender),
                additional,
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        let proposals = expected_effects
            .applied_proposals
            .clone()
            .proposals_or_refs();

        let resolution = cache
            .resolve_for_commit_default(
                Sender::Member(test_sender),
                proposals,
                None,
                &ExtensionList::new(),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert_matches(expected_effects, resolution);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn proposal_cache_filters_duplicate_psk_ids() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let (alice, tree) = new_tree("alice");
        let cache = make_proposal_cache();

        let proposal = Proposal::Psk(make_external_psk(
            b"ted",
            crate::psk::PskNonce::random(&test_cipher_suite_provider(TEST_CIPHER_SUITE)).unwrap(),
        ));

        let res = cache
            .prepare_commit_default(
                Sender::Member(*alice),
                vec![proposal.clone(), proposal],
                &get_test_group_context(0, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(res, Err(MlsError::DuplicatePskIds));
    }
    fn test_node() -> LeafNode {
        let (mut leaf_node, _, signer) =
            get_basic_test_node_sig_key(TEST_CIPHER_SUITE, "foo");

        leaf_node
            .commit(
                &test_cipher_suite_provider(TEST_CIPHER_SUITE),
                TEST_GROUP,
                0,
                Some(default_properties()),
                None,
                &signer,
            )

            .unwrap();

        leaf_node
    }

    #[test]
    fn external_commit_must_have_new_leaf() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let kem_output = vec![0; cipher_suite_provider.kdf_extract_size()];
        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let public_tree = &group.state.public_tree;

        let res = cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                vec![ProposalOrRef::Proposal(Box::new(Proposal::ExternalInit(
                    ExternalInit { kem_output },
                )))],
                None,
                &group.context().extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(res, Err(MlsError::ExternalCommitMustHaveNewLeaf));
    }

    #[test]
    fn proposal_cache_rejects_proposals_by_ref_for_new_member() {
        let mut cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let proposal = {
            let kem_output = vec![0; cipher_suite_provider.kdf_extract_size()];
            Proposal::ExternalInit(ExternalInit { kem_output })
        };

        let proposal_ref = make_proposal_ref(&proposal, test_sender());

        cache.insert(
            proposal_ref.clone(),
            proposal,
            Sender::Member(test_sender()),
        );

        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let public_tree = &group.state.public_tree;

        let res = cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                vec![ProposalOrRef::Reference(proposal_ref)],
                Some(&test_node()),
                &group.context().extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(res, Err(MlsError::OnlyMembersCanCommitProposalsByRef));
    }

    #[test]
    fn proposal_cache_rejects_multiple_external_init_proposals_in_commit() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let kem_output = vec![0; cipher_suite_provider.kdf_extract_size()];
        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let public_tree = &group.state.public_tree;

        let res = cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                [
                    Proposal::ExternalInit(ExternalInit {
                        kem_output: kem_output.clone(),
                    }),
                    Proposal::ExternalInit(ExternalInit { kem_output }),
                ]
                .into_iter()
                .map(|p| ProposalOrRef::Proposal(Box::new(p)))
                .collect(),
                Some(&test_node()),
                &group.context().extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(
            res,
            Err(MlsError::ExternalCommitMustHaveExactlyOneExternalInit)
        );
    }
    fn new_member_commits_proposal(proposal: Proposal) -> Result<ProvisionalState, MlsError> {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let kem_output = vec![0; cipher_suite_provider.kdf_extract_size()];
        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let public_tree = &group.state.public_tree;

        cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                [
                    Proposal::ExternalInit(ExternalInit { kem_output }),
                    proposal,
                ]
                .into_iter()
                .map(|p| ProposalOrRef::Proposal(Box::new(p)))
                .collect(),
                Some(&test_node()),
                &group.context().extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

    }

    #[test]
    fn new_member_cannot_commit_add_proposal() {
        let res = new_member_commits_proposal(Proposal::Add(Box::new(AddProposal {
            key_package: test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "frank"),
        })))
        ;

        assert_matches!(
            res,
            Err(MlsError::InvalidProposalTypeInExternalCommit(
                ProposalType::ADD
            ))
        );
    }

    #[test]
    fn new_member_cannot_commit_more_than_one_remove_proposal() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let kem_output = vec![0; cipher_suite_provider.kdf_extract_size()];
        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let group_extensions = group.context().extensions.clone();
        let mut public_tree = group.group.state.public_tree;

        let foo = get_basic_test_node(TEST_CIPHER_SUITE, "foo");

        let bar = get_basic_test_node(TEST_CIPHER_SUITE, "bar");

        let test_leaf_nodes = vec![foo, bar];

        let test_leaf_node_indexes = public_tree
            .add_leaves(
                test_leaf_nodes,
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )

            .unwrap();

        let proposals = vec![
            Proposal::ExternalInit(ExternalInit { kem_output }),
            Proposal::Remove(RemoveProposal {
                to_remove: test_leaf_node_indexes[0],
            }),
            Proposal::Remove(RemoveProposal {
                to_remove: test_leaf_node_indexes[1],
            }),
        ];

        let res = cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                proposals
                    .into_iter()
                    .map(|p| ProposalOrRef::Proposal(Box::new(p)))
                    .collect(),
                Some(&test_node()),
                &group_extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(res, Err(MlsError::ExternalCommitWithMoreThanOneRemove));
    }

    #[test]
    fn new_member_remove_proposal_invalid_credential() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let kem_output = vec![0; cipher_suite_provider.kdf_extract_size()];
        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let group_extensions = group.context().extensions.clone();
        let mut public_tree = group.group.state.public_tree;

        let node = get_basic_test_node(TEST_CIPHER_SUITE, "bar");

        let test_leaf_nodes = vec![node];

        let test_leaf_node_indexes = public_tree
            .add_leaves(
                test_leaf_nodes,
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )

            .unwrap();

        let proposals = vec![
            Proposal::ExternalInit(ExternalInit { kem_output }),
            Proposal::Remove(RemoveProposal {
                to_remove: test_leaf_node_indexes[0],
            }),
        ];

        let res = cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                proposals
                    .into_iter()
                    .map(|p| ProposalOrRef::Proposal(Box::new(p)))
                    .collect(),
                Some(&test_node()),
                &group_extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(res, Err(MlsError::ExternalCommitRemovesOtherIdentity));
    }

    #[test]
    fn new_member_remove_proposal_valid_credential() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let kem_output = vec![0; cipher_suite_provider.kdf_extract_size()];
        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let group_extensions = group.context().extensions.clone();
        let mut public_tree = group.group.state.public_tree;

        let node = get_basic_test_node(TEST_CIPHER_SUITE, "foo");

        let test_leaf_nodes = vec![node];

        let test_leaf_node_indexes = public_tree
            .add_leaves(
                test_leaf_nodes,
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )

            .unwrap();

        let proposals = vec![
            Proposal::ExternalInit(ExternalInit { kem_output }),
            Proposal::Remove(RemoveProposal {
                to_remove: test_leaf_node_indexes[0],
            }),
        ];

        let res = cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                proposals
                    .into_iter()
                    .map(|p| ProposalOrRef::Proposal(Box::new(p)))
                    .collect(),
                Some(&test_node()),
                &group_extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(res, Ok(_));
    }

    #[test]
    fn new_member_cannot_commit_update_proposal() {
        let res = new_member_commits_proposal(Proposal::Update(UpdateProposal {
            leaf_node: get_basic_test_node(TEST_CIPHER_SUITE, "foo"),
        }))
        ;

        assert_matches!(
            res,
            Err(MlsError::InvalidProposalTypeInExternalCommit(
                ProposalType::UPDATE
            ))
        );
    }

    #[test]
    fn new_member_cannot_commit_group_extensions_proposal() {
        let res =
            new_member_commits_proposal(Proposal::GroupContextExtensions(ExtensionList::new()))
                ;

        assert_matches!(
            res,
            Err(MlsError::InvalidProposalTypeInExternalCommit(
                ProposalType::GROUP_CONTEXT_EXTENSIONS,
            ))
        );
    }

    #[test]
    fn new_member_cannot_commit_reinit_proposal() {
        let res = new_member_commits_proposal(Proposal::ReInit(ReInitProposal {
            group_id: b"foo".to_vec(),
            version: TEST_PROTOCOL_VERSION,
            cipher_suite: TEST_CIPHER_SUITE,
            extensions: ExtensionList::new(),
        }))
        ;

        assert_matches!(
            res,
            Err(MlsError::InvalidProposalTypeInExternalCommit(
                ProposalType::RE_INIT
            ))
        );
    }

    #[test]
    fn new_member_commit_must_contain_an_external_init_proposal() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let group = test_group(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE);
        let public_tree = &group.state.public_tree;

        let res = cache
            .resolve_for_commit_default(
                Sender::NewMemberCommit,
                Vec::new(),
                Some(&test_node()),
                &group.context().extensions,
                &BasicIdentityProvider,
                &cipher_suite_provider,
                public_tree,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )
            ;

        assert_matches!(
            res,
            Err(MlsError::ExternalCommitMustHaveExactlyOneExternalInit)
        );
    }

    #[test]
    fn test_path_update_required_empty() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let mut tree = TreeKemPublic::new();
        add_member(&mut tree, "alice");
        add_member(&mut tree, "bob");

        let effects = cache
            .prepare_commit_default(
                Sender::Member(test_sender()),
                vec![],
                &get_test_group_context(1, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert!(path_update_required(&effects.applied_proposals))
    }

    #[test]
    fn test_path_update_required_updates() {
        let mut cache = make_proposal_cache();
        let update = Proposal::Update(make_update_proposal("bar"));
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        cache.insert(
            make_proposal_ref(&update, LeafIndex::unchecked(2)),
            update,
            Sender::Member(2),
        );

        let mut tree = TreeKemPublic::new();
        add_member(&mut tree, "alice");
        add_member(&mut tree, "bob");
        add_member(&mut tree, "carol");

        let effects = cache
            .prepare_commit_default(
                Sender::Member(test_sender()),
                Vec::new(),
                &get_test_group_context(1, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert!(path_update_required(&effects.applied_proposals))
    }

    #[test]
    fn test_path_update_required_removes() {
        let cache = make_proposal_cache();
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let (alice_leaf, alice_secret, _) =
            get_basic_test_node_sig_key(TEST_CIPHER_SUITE, "alice");
        let alice = 0;

        let (mut tree, _) = TreeKemPublic::derive(
            alice_leaf,
            alice_secret,
            &BasicIdentityProvider,
            &Default::default(),
        )

        .unwrap();

        let bob_node = get_basic_test_node(TEST_CIPHER_SUITE, "bob");

        let bob = tree
            .add_leaves(
                vec![bob_node],
                &BasicIdentityProvider,
                &cipher_suite_provider,
            )

            .unwrap()[0];

        let remove = Proposal::Remove(RemoveProposal { to_remove: bob });

        let effects = cache
            .prepare_commit_default(
                Sender::Member(alice),
                vec![remove],
                &get_test_group_context(1, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert!(path_update_required(&effects.applied_proposals))
    }

    #[cfg(feature = "psk")]
    #[test]
    fn test_path_update_not_required() {
        let (alice, tree) = new_tree("alice");
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let cache = make_proposal_cache();

        let psk = Proposal::Psk(PreSharedKeyProposal {
            psk: PreSharedKeyID::new(
                JustPreSharedKeyID::External(ExternalPskId::new(vec![])),
                &test_cipher_suite_provider(TEST_CIPHER_SUITE),
            )
            .unwrap(),
        });

        let add = Proposal::Add(Box::new(AddProposal {
            key_package: test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "bob"),
        }));

        let effects = cache
            .prepare_commit_default(
                Sender::Member(*alice),
                vec![psk, add],
                &get_test_group_context(1, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert!(!path_update_required(&effects.applied_proposals))
    }

    #[test]
    fn path_update_is_not_required_for_re_init() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);
        let (alice, tree) = new_tree("alice");
        let cache = make_proposal_cache();

        let reinit = Proposal::ReInit(ReInitProposal {
            group_id: vec![],
            version: TEST_PROTOCOL_VERSION,
            cipher_suite: TEST_CIPHER_SUITE,
            extensions: Default::default(),
        });

        let effects = cache
            .prepare_commit_default(
                Sender::Member(*alice),
                vec![reinit],
                &get_test_group_context(1, TEST_CIPHER_SUITE),
                &BasicIdentityProvider,
                &cipher_suite_provider,
                &tree,
                None,
                &AlwaysFoundPskStorage,
                pass_through_rules(),
            )

            .unwrap();

        assert!(!path_update_required(&effects.applied_proposals))
    }

    #[derive(Debug)]
    struct CommitSender<'a, C, F, P, CSP> {
        cipher_suite_provider: CSP,
        tree: &'a TreeKemPublic,
        sender: LeafIndex,
        cache: ProposalCache,
        additional_proposals: Vec<Proposal>,
        identity_provider: C,
        user_rules: F,
        psk_storage: P,
    }

    impl<'a, CSP>
        CommitSender<'a, BasicWithCustomProvider, DefaultMlsRules, AlwaysFoundPskStorage, CSP>
    {
        fn new(tree: &'a TreeKemPublic, sender: LeafIndex, cipher_suite_provider: CSP) -> Self {
            Self {
                tree,
                sender,
                cache: make_proposal_cache(),
                additional_proposals: Vec::new(),
                identity_provider: BasicWithCustomProvider::new(BasicIdentityProvider::new()),
                user_rules: pass_through_rules(),
                psk_storage: AlwaysFoundPskStorage,
                cipher_suite_provider,
            }
        }
    }

    impl<'a, C, F, P, CSP> CommitSender<'a, C, F, P, CSP>
    where
        C: IdentityProvider,
        F: MlsRules,
        P: PreSharedKeyStorage,
        CSP: CipherSuiteProvider,
    {
        #[cfg(feature = "by_ref_proposal")]
        fn with_identity_provider<V>(self, identity_provider: V) -> CommitSender<'a, V, F, P, CSP>
        where
            V: IdentityProvider,
        {
            CommitSender {
                identity_provider,
                cipher_suite_provider: self.cipher_suite_provider,
                tree: self.tree,
                sender: self.sender,
                cache: self.cache,
                additional_proposals: self.additional_proposals,
                user_rules: self.user_rules,
                psk_storage: self.psk_storage,
            }
        }

        fn cache<S>(mut self, r: ProposalRef, p: Proposal, proposer: S) -> Self
        where
            S: Into<Sender>,
        {
            self.cache.insert(r, p, proposer.into());
            self
        }

        fn with_additional<I>(mut self, proposals: I) -> Self
        where
            I: IntoIterator<Item = Proposal>,
        {
            self.additional_proposals.extend(proposals);
            self
        }

        fn with_user_rules<G>(self, f: G) -> CommitSender<'a, C, G, P, CSP>
        where
            G: MlsRules,
        {
            CommitSender {
                tree: self.tree,
                sender: self.sender,
                cache: self.cache,
                additional_proposals: self.additional_proposals,
                identity_provider: self.identity_provider,
                user_rules: f,
                psk_storage: self.psk_storage,
                cipher_suite_provider: self.cipher_suite_provider,
            }
        }

        fn with_psk_storage<V>(self, v: V) -> CommitSender<'a, C, F, V, CSP>
        where
            V: PreSharedKeyStorage,
        {
            CommitSender {
                tree: self.tree,
                sender: self.sender,
                cache: self.cache,
                additional_proposals: self.additional_proposals,
                identity_provider: self.identity_provider,
                user_rules: self.user_rules,
                psk_storage: v,
                cipher_suite_provider: self.cipher_suite_provider,
            }
        }
        fn send(&self) -> Result<(Vec<ProposalOrRef>, ProvisionalState), MlsError> {
            let state = self
                .cache
                .prepare_commit_default(
                    Sender::Member(*self.sender),
                    self.additional_proposals.clone(),
                    &get_test_group_context(1, TEST_CIPHER_SUITE),
                    &self.identity_provider,
                    &self.cipher_suite_provider,
                    self.tree,
                    None,
                    &self.psk_storage,
                    &self.user_rules,
                )
                ?;

            let proposals = state.applied_proposals.clone().proposals_or_refs();

            Ok((proposals, state))
        }
    }
    fn key_package_with_invalid_signature() -> KeyPackage {
        let mut kp = test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "mallory");
        kp.signature = vec![1, 2, 3];
        kp
    }
    fn key_package_with_public_key(key: crypto::HpkePublicKey) -> KeyPackage {
        let cs = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let (mut key_package, signer) =
            test_key_package_with_signer(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "test");

        key_package.leaf_node.public_key = key;

        key_package
            .leaf_node
            .sign(
                &cs,
                &signer,
                &LeafNodeSigningContext {
                    group_id: None,
                    leaf_index: None,
                },
            )

            .unwrap();

        key_package.sign(&cs, &signer, &()).unwrap();

        key_package
    }

    #[test]
    fn receiving_add_with_invalid_key_package_fails() {
        let (alice, tree) = new_tree("alice");
        let kp = key_package_with_invalid_signature();

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::Add(Box::new(AddProposal { key_package: kp }))])
        ;

        assert_matches!(res, Err(MlsError::InvalidSignature));
    }

    #[test]
    fn sending_additional_add_with_invalid_key_package_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Add(Box::new(AddProposal {
                key_package: key_package_with_invalid_signature(),
            }))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidSignature));
    }

    #[test]
    fn sending_add_with_invalid_key_package_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let proposal = Proposal::Add(Box::new(AddProposal {
            key_package: key_package_with_invalid_signature(),
        }));

        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn sending_add_with_hpke_key_of_another_member_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Add(Box::new(AddProposal {
                key_package: key_package_with_public_key(
                    tree.get_leaf_node(alice).unwrap().public_key.clone(),
                )
                ,
            }))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::DuplicateLeafData(_)));
    }

    #[test]
    fn sending_add_with_hpke_key_of_another_member_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let proposal = Proposal::Add(Box::new(AddProposal {
            key_package: key_package_with_public_key(
                tree.get_leaf_node(alice).unwrap().public_key.clone(),
            )
            ,
        }));

        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn receiving_update_with_invalid_leaf_node_fails() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        let proposal = Proposal::Update(UpdateProposal {
            leaf_node: get_basic_test_node(TEST_CIPHER_SUITE, "alice"),
        });

        let proposal_ref = make_proposal_ref(&proposal, bob);

        let res = CommitReceiver::new(
            &tree,
            alice,
            bob,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(proposal_ref.clone(), proposal, bob)
        .receive([proposal_ref])
        ;

        assert_matches!(res, Err(MlsError::InvalidLeafNodeSource));
    }

    #[test]
    fn sending_update_with_invalid_leaf_node_filters_it_out() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        let proposal = Proposal::Update(UpdateProposal {
            leaf_node: get_basic_test_node(TEST_CIPHER_SUITE, "alice"),
        });

        let proposal_info = make_proposal_info(&proposal, bob);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(proposal_info.proposal_ref().unwrap().clone(), proposal, bob)
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn receiving_remove_with_invalid_index_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::Remove(RemoveProposal {
            to_remove: LeafIndex::unchecked(10),
        })])
        ;

        assert_matches!(res, Err(MlsError::InvalidNodeIndex(20)));
    }

    #[test]
    fn sending_additional_remove_with_invalid_index_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Remove(RemoveProposal {
                to_remove: LeafIndex::unchecked(10),
            })])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidNodeIndex(20)));
    }

    #[test]
    fn sending_remove_with_invalid_index_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let proposal = Proposal::Remove(RemoveProposal {
            to_remove: LeafIndex::unchecked(10),
        });

        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[cfg(feature = "psk")]
    fn make_external_psk(id: &[u8], nonce: PskNonce) -> PreSharedKeyProposal {
        PreSharedKeyProposal {
            psk: PreSharedKeyID {
                key_id: JustPreSharedKeyID::External(ExternalPskId::new(id.to_vec())),
                psk_nonce: nonce,
            },
        }
    }

    #[cfg(feature = "psk")]
    fn new_external_psk(id: &[u8]) -> PreSharedKeyProposal {
        make_external_psk(
            id,
            PskNonce::random(&test_cipher_suite_provider(TEST_CIPHER_SUITE)).unwrap(),
        )
    }

    #[cfg(feature = "psk")]
    #[test]
    fn receiving_psk_with_invalid_nonce_fails() {
        let invalid_nonce = PskNonce(vec![0, 1, 2]);
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::Psk(make_external_psk(
            b"foo",
            invalid_nonce.clone(),
        ))])
        ;

        assert_matches!(res, Err(MlsError::InvalidPskNonceLength,));
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_additional_psk_with_invalid_nonce_fails() {
        let invalid_nonce = PskNonce(vec![0, 1, 2]);
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Psk(make_external_psk(
                b"foo",
                invalid_nonce.clone(),
            ))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidPskNonceLength));
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_psk_with_invalid_nonce_filters_it_out() {
        let invalid_nonce = PskNonce(vec![0, 1, 2]);
        let (alice, tree) = new_tree("alice");
        let proposal = Proposal::Psk(make_external_psk(b"foo", invalid_nonce));

        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[cfg(feature = "psk")]
    fn make_resumption_psk(usage: ResumptionPSKUsage) -> PreSharedKeyProposal {
        PreSharedKeyProposal {
            psk: PreSharedKeyID {
                key_id: JustPreSharedKeyID::Resumption(ResumptionPsk {
                    usage,
                    psk_group_id: PskGroupId(TEST_GROUP.to_vec()),
                    psk_epoch: 1,
                }),
                psk_nonce: PskNonce::random(&test_cipher_suite_provider(TEST_CIPHER_SUITE))
                    .unwrap(),
            },
        }
    }

    #[cfg(feature = "psk")]
    fn receiving_resumption_psk_with_bad_usage_fails(usage: ResumptionPSKUsage) {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::Psk(make_resumption_psk(usage))])
        ;

        assert_matches!(res, Err(MlsError::InvalidTypeOrUsageInPreSharedKeyProposal));
    }

    #[cfg(feature = "psk")]
    fn sending_additional_resumption_psk_with_bad_usage_fails(usage: ResumptionPSKUsage) {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Psk(make_resumption_psk(usage))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidTypeOrUsageInPreSharedKeyProposal));
    }

    #[cfg(feature = "psk")]
    fn sending_resumption_psk_with_bad_usage_filters_it_out(usage: ResumptionPSKUsage) {
        let (alice, tree) = new_tree("alice");
        let proposal = Proposal::Psk(make_resumption_psk(usage));
        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn receiving_resumption_psk_with_reinit_usage_fails() {
        receiving_resumption_psk_with_bad_usage_fails(ResumptionPSKUsage::Reinit);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_additional_resumption_psk_with_reinit_usage_fails() {
        sending_additional_resumption_psk_with_bad_usage_fails(ResumptionPSKUsage::Reinit);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_resumption_psk_with_reinit_usage_filters_it_out() {
        sending_resumption_psk_with_bad_usage_filters_it_out(ResumptionPSKUsage::Reinit);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn receiving_resumption_psk_with_branch_usage_fails() {
        receiving_resumption_psk_with_bad_usage_fails(ResumptionPSKUsage::Branch);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_additional_resumption_psk_with_branch_usage_fails() {
        sending_additional_resumption_psk_with_bad_usage_fails(ResumptionPSKUsage::Branch);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_resumption_psk_with_branch_usage_filters_it_out() {
        sending_resumption_psk_with_bad_usage_filters_it_out(ResumptionPSKUsage::Branch);
    }

    fn make_reinit(version: ProtocolVersion) -> ReInitProposal {
        ReInitProposal {
            group_id: TEST_GROUP.to_vec(),
            version,
            cipher_suite: TEST_CIPHER_SUITE,
            extensions: ExtensionList::new(),
        }
    }

    #[test]
    fn receiving_reinit_downgrading_version_fails() {
        let smaller_protocol_version = ProtocolVersion::from(0);
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::ReInit(make_reinit(smaller_protocol_version))])
        ;

        assert_matches!(res, Err(MlsError::InvalidProtocolVersionInReInit));
    }

    #[test]
    fn sending_additional_reinit_downgrading_version_fails() {
        let smaller_protocol_version = ProtocolVersion::from(0);
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::ReInit(make_reinit(smaller_protocol_version))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidProtocolVersionInReInit));
    }

    #[test]
    fn sending_reinit_downgrading_version_filters_it_out() {
        let smaller_protocol_version = ProtocolVersion::from(0);
        let (alice, tree) = new_tree("alice");
        let proposal = Proposal::ReInit(make_reinit(smaller_protocol_version));
        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn receiving_update_for_committer_fails() {
        let (alice, tree) = new_tree("alice");
        let update = Proposal::Update(make_update_proposal("alice"));
        let update_ref = make_proposal_ref(&update, alice);

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(update_ref.clone(), update, alice)
        .receive([update_ref])
        ;

        assert_matches!(res, Err(MlsError::InvalidCommitSelfUpdate));
    }

    #[test]
    fn sending_additional_update_for_committer_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Update(make_update_proposal("alice"))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidProposalTypeForSender));
    }

    #[test]
    fn sending_update_for_committer_filters_it_out() {
        let (alice, tree) = new_tree("alice");
        let proposal = Proposal::Update(make_update_proposal("alice"));
        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn receiving_remove_for_committer_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::Remove(RemoveProposal { to_remove: alice })])
        ;

        assert_matches!(res, Err(MlsError::CommitterSelfRemoval));
    }

    #[test]
    fn sending_additional_remove_for_committer_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Remove(RemoveProposal { to_remove: alice })])
            .send()
            ;

        assert_matches!(res, Err(MlsError::CommitterSelfRemoval));
    }

    #[test]
    fn sending_remove_for_committer_filters_it_out() {
        let (alice, tree) = new_tree("alice");
        let proposal = Proposal::Remove(RemoveProposal { to_remove: alice });
        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn receiving_update_and_remove_for_same_leaf_fails() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        let update = Proposal::Update(make_update_proposal("bob"));
        let update_ref = make_proposal_ref(&update, bob);

        let remove = Proposal::Remove(RemoveProposal { to_remove: bob });
        let remove_ref = make_proposal_ref(&remove, bob);

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(update_ref.clone(), update, bob)
        .cache(remove_ref.clone(), remove, bob)
        .receive([update_ref, remove_ref])
        ;

        assert_matches!(res, Err(MlsError::UpdatingNonExistingMember));
    }

    #[test]
    fn sending_update_and_remove_for_same_leaf_filters_update_out() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        let update = Proposal::Update(make_update_proposal("bob"));
        let update_info = make_proposal_info(&update, alice);

        let remove = Proposal::Remove(RemoveProposal { to_remove: bob });
        let remove_ref = make_proposal_ref(&remove, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    update_info.proposal_ref().unwrap().clone(),
                    update.clone(),
                    alice,
                )
                .cache(remove_ref.clone(), remove, alice)
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, vec![remove_ref.into()]);

        assert_eq!(processed_proposals.1.unused_proposals, vec![update_info]);
    }
    fn make_add_proposal() -> Box<AddProposal> {
        Box::new(AddProposal {
            key_package: test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "frank"),
        })
    }

    #[test]
    fn receiving_add_proposals_for_same_client_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([
            Proposal::Add(make_add_proposal()),
            Proposal::Add(make_add_proposal()),
        ])
        ;

        assert_matches!(res, Err(MlsError::DuplicateLeafData(1)));
    }

    #[test]
    fn sending_additional_add_proposals_for_same_client_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([
                Proposal::Add(make_add_proposal()),
                Proposal::Add(make_add_proposal()),
            ])
            .send()
            ;

        assert_matches!(res, Err(MlsError::DuplicateLeafData(1)));
    }

    #[test]
    fn sending_add_proposals_for_same_client_keeps_only_one() {
        let (alice, tree) = new_tree("alice");

        let add_one = Proposal::Add(make_add_proposal());
        let add_two = Proposal::Add(make_add_proposal());
        let add_ref_one = make_proposal_ref(&add_one, alice);
        let add_ref_two = make_proposal_ref(&add_two, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(add_ref_one.clone(), add_one.clone(), alice)
                .cache(add_ref_two.clone(), add_two.clone(), alice)
                .send()

                .unwrap();

        let committed_add_ref = match &*processed_proposals.0 {
            [ProposalOrRef::Reference(add_ref)] => add_ref,
            _ => panic!("committed proposals list does not contain exactly one reference"),
        };

        let add_refs = [add_ref_one, add_ref_two];
        assert!(add_refs.contains(committed_add_ref));

        assert_matches!(
            &*processed_proposals.1.unused_proposals,
            [rejected_add_info] if committed_add_ref != rejected_add_info.proposal_ref().unwrap() && add_refs.contains(rejected_add_info.proposal_ref().unwrap())
        );
    }

    #[test]
    fn receiving_update_for_different_identity_fails() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        let update = Proposal::Update(make_update_proposal_custom("carol", 1));
        let update_ref = make_proposal_ref(&update, bob);

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(update_ref.clone(), update, bob)
        .receive([update_ref])
        ;

        assert_matches!(res, Err(MlsError::InvalidSuccessor));
    }

    #[test]
    fn sending_update_for_different_identity_filters_it_out() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        let update = Proposal::Update(make_update_proposal("carol"));
        let update_info = make_proposal_info(&update, bob);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(update_info.proposal_ref().unwrap().clone(), update, bob)
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        // Bob proposed the update, so it is not listed as rejected when Alice commits it because
        // she didn't propose it.
        assert_eq!(processed_proposals.1.unused_proposals, vec![update_info]);
    }

    #[test]
    fn receiving_add_for_same_client_as_existing_member_fails() {
        let (alice, public_tree) = new_tree("alice");
        let add = Proposal::Add(make_add_proposal());

        let ProvisionalState { public_tree, .. } = CommitReceiver::new(
            &public_tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([add.clone()])

        .unwrap();

        let res = CommitReceiver::new(
            &public_tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([add])
        ;

        assert_matches!(res, Err(MlsError::DuplicateLeafData(1)));
    }

    #[test]
    fn sending_additional_add_for_same_client_as_existing_member_fails() {
        let (alice, public_tree) = new_tree("alice");
        let add = Proposal::Add(make_add_proposal());

        let ProvisionalState { public_tree, .. } = CommitReceiver::new(
            &public_tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([add.clone()])

        .unwrap();

        let res = CommitSender::new(
            &public_tree,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .with_additional([add])
        .send()
        ;

        assert_matches!(res, Err(MlsError::DuplicateLeafData(1)));
    }

    #[test]
    fn sending_add_for_same_client_as_existing_member_filters_it_out() {
        let (alice, public_tree) = new_tree("alice");
        let add = Proposal::Add(make_add_proposal());

        let ProvisionalState { public_tree, .. } = CommitReceiver::new(
            &public_tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([add.clone()])

        .unwrap();

        let proposal_info = make_proposal_info(&add, alice);

        let processed_proposals = CommitSender::new(
            &public_tree,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(
            proposal_info.proposal_ref().unwrap().clone(),
            add.clone(),
            alice,
        )
        .send()

        .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[cfg(feature = "psk")]
    #[test]
    fn receiving_psk_proposals_with_same_psk_id_fails() {
        let (alice, tree) = new_tree("alice");
        let psk_proposal = Proposal::Psk(new_external_psk(b"foo"));

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([psk_proposal.clone(), psk_proposal])
        ;

        assert_matches!(res, Err(MlsError::DuplicatePskIds));
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_additional_psk_proposals_with_same_psk_id_fails() {
        let (alice, tree) = new_tree("alice");
        let psk_proposal = Proposal::Psk(new_external_psk(b"foo"));

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([psk_proposal.clone(), psk_proposal])
            .send()
            ;

        assert_matches!(res, Err(MlsError::DuplicatePskIds));
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_psk_proposals_with_same_psk_id_keeps_only_one() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        let proposal = Proposal::Psk(new_external_psk(b"foo"));

        let proposal_info = [
            make_proposal_info(&proposal, alice),
            make_proposal_info(&proposal, bob),
        ];

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info[0].proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .cache(
                    proposal_info[1].proposal_ref().unwrap().clone(),
                    proposal,
                    bob,
                )
                .send()

                .unwrap();

        let committed_info = match processed_proposals
            .1
            .applied_proposals
            .clone()
            .into_proposals()
            .collect_vec()
            .as_slice()
        {
            [r] => r.clone(),
            _ => panic!("Expected single proposal reference in {processed_proposals:?}"),
        };

        assert!(proposal_info.contains(&committed_info));

        match &*processed_proposals.1.unused_proposals {
            [r] => {
                assert_ne!(*r, committed_info);
                assert!(proposal_info.contains(r));
            }
            _ => panic!(
                "Expected one proposal reference in {:?}",
                processed_proposals.1.unused_proposals
            ),
        }
    }

    #[test]
    fn receiving_multiple_group_context_extensions_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([
            Proposal::GroupContextExtensions(ExtensionList::new()),
            Proposal::GroupContextExtensions(ExtensionList::new()),
        ])
        ;

        assert_matches!(
            res,
            Err(MlsError::MoreThanOneGroupContextExtensionsProposal)
        );
    }

    #[test]
    fn sending_multiple_additional_group_context_extensions_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([
                Proposal::GroupContextExtensions(ExtensionList::new()),
                Proposal::GroupContextExtensions(ExtensionList::new()),
            ])
            .send()
            ;

        assert_matches!(
            res,
            Err(MlsError::MoreThanOneGroupContextExtensionsProposal)
        );
    }

    fn make_extension_list(something: u8) -> ExtensionList {
        vec![TestExtension { foo: something }.into_extension().unwrap()].into()
    }

    #[test]
    fn sending_multiple_group_context_extensions_keeps_only_one() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let (alice, tree) = {
            let (signing_identity, signature_key) =
                get_test_signing_identity(TEST_CIPHER_SUITE, b"alice");

            let properties = ConfigProperties {
                capabilities: Capabilities {
                    extensions: vec![42.into()],
                    ..Capabilities::default()
                },
                extensions: Default::default(),
            };

            let (leaf, secret) = LeafNode::generate(
                &cipher_suite_provider,
                properties,
                signing_identity,
                &signature_key,
                Lifetime::years(1, None).unwrap(),
            )

            .unwrap();

            let (pub_tree, priv_tree) =
                TreeKemPublic::derive(leaf, secret, &BasicIdentityProvider, &Default::default())

                    .unwrap();

            (priv_tree.self_index, pub_tree)
        };

        let proposals = [
            Proposal::GroupContextExtensions(make_extension_list(0)),
            Proposal::GroupContextExtensions(make_extension_list(1)),
        ];

        let gce_info = [
            make_proposal_info(&proposals[0], alice),
            make_proposal_info(&proposals[1], alice),
        ];

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    gce_info[0].proposal_ref().unwrap().clone(),
                    proposals[0].clone(),
                    alice,
                )
                .cache(
                    gce_info[1].proposal_ref().unwrap().clone(),
                    proposals[1].clone(),
                    alice,
                )
                .send()

                .unwrap();

        let committed_gce_info = match processed_proposals
            .1
            .applied_proposals
            .clone()
            .into_proposals()
            .collect_vec()
            .as_slice()
        {
            [gce_info] => gce_info.clone(),
            _ => panic!("committed proposals list does not contain exactly one reference"),
        };

        assert!(gce_info.contains(&committed_gce_info));

        assert_matches!(
            &*processed_proposals.1.unused_proposals,
            [rejected_gce_info] if committed_gce_info != *rejected_gce_info && gce_info.contains(rejected_gce_info)
        );
    }

    #[cfg(feature = "by_ref_proposal")]
    fn make_external_senders_extension() -> ExtensionList {
        let identity = get_test_signing_identity(TEST_CIPHER_SUITE, b"alice")

            .0;

        vec![ExternalSendersExt::new(vec![identity])
            .into_extension()
            .unwrap()]
        .into()
    }

    #[cfg(feature = "by_ref_proposal")]
    #[test]
    fn receiving_invalid_external_senders_extension_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .with_identity_provider(FailureIdentityProvider::new())
        .receive([Proposal::GroupContextExtensions(
            make_external_senders_extension(),
        )])
        ;

        assert_matches!(res, Err(MlsError::IdentityProviderError(_)));
    }

    #[cfg(feature = "by_ref_proposal")]
    #[test]
    fn sending_additional_invalid_external_senders_extension_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_identity_provider(FailureIdentityProvider::new())
            .with_additional([Proposal::GroupContextExtensions(
                make_external_senders_extension(),
            )])
            .send()
            ;

        assert_matches!(res, Err(MlsError::IdentityProviderError(_)));
    }

    #[cfg(feature = "by_ref_proposal")]
    #[test]
    fn sending_invalid_external_senders_extension_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let proposal = Proposal::GroupContextExtensions(make_external_senders_extension());

        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .with_identity_provider(FailureIdentityProvider::new())
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn receiving_reinit_with_other_proposals_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([
            Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION)),
            Proposal::Add(make_add_proposal()),
        ])
        ;

        assert_matches!(res, Err(MlsError::OtherProposalWithReInit));
    }

    #[test]
    fn sending_additional_reinit_with_other_proposals_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([
                Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION)),
                Proposal::Add(make_add_proposal()),
            ])
            .send()
            ;

        assert_matches!(res, Err(MlsError::OtherProposalWithReInit));
    }

    #[test]
    fn sending_reinit_with_other_proposals_filters_it_out() {
        let (alice, tree) = new_tree("alice");
        let reinit = Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION));
        let reinit_info = make_proposal_info(&reinit, alice);
        let add = Proposal::Add(make_add_proposal());
        let add_ref = make_proposal_ref(&add, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    reinit_info.proposal_ref().unwrap().clone(),
                    reinit.clone(),
                    alice,
                )
                .cache(add_ref.clone(), add, alice)
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, vec![add_ref.into()]);

        assert_eq!(processed_proposals.1.unused_proposals, vec![reinit_info]);
    }

    #[test]
    fn receiving_multiple_reinits_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([
            Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION)),
            Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION)),
        ])
        ;

        assert_matches!(res, Err(MlsError::OtherProposalWithReInit));
    }

    #[test]
    fn sending_additional_multiple_reinits_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([
                Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION)),
                Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION)),
            ])
            .send()
            ;

        assert_matches!(res, Err(MlsError::OtherProposalWithReInit));
    }

    #[test]
    fn sending_multiple_reinits_keeps_only_one() {
        let (alice, tree) = new_tree("alice");
        let reinit = Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION));
        let reinit_ref = make_proposal_ref(&reinit, alice);
        let other_reinit = Proposal::ReInit(ReInitProposal {
            group_id: b"other_group".to_vec(),
            ..make_reinit(TEST_PROTOCOL_VERSION)
        });
        let other_reinit_ref = make_proposal_ref(&other_reinit, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(reinit_ref.clone(), reinit.clone(), alice)
                .cache(other_reinit_ref.clone(), other_reinit.clone(), alice)
                .send()

                .unwrap();

        let processed_ref = match &*processed_proposals.0 {
            [ProposalOrRef::Reference(r)] => r,
            p => panic!("Expected single proposal reference but found {p:?}"),
        };

        assert!(*processed_ref == reinit_ref || *processed_ref == other_reinit_ref);

        {
            let (rejected_ref, unused_proposal) = match &*processed_proposals.1.unused_proposals {
                [r] => (r.proposal_ref().unwrap().clone(), r.proposal.clone()),
                p => panic!("Expected single proposal but found {p:?}"),
            };

            assert_ne!(rejected_ref, *processed_ref);
            assert!(rejected_ref == reinit_ref || rejected_ref == other_reinit_ref);
            assert!(unused_proposal == reinit || unused_proposal == other_reinit);
        }
    }

    fn make_external_init() -> ExternalInit {
        ExternalInit {
            kem_output: vec![33; test_cipher_suite_provider(TEST_CIPHER_SUITE).kdf_extract_size()],
        }
    }

    #[test]
    fn receiving_external_init_from_member_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::ExternalInit(make_external_init())])
        ;

        assert_matches!(res, Err(MlsError::InvalidProposalTypeForSender));
    }

    #[test]
    fn sending_additional_external_init_from_member_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::ExternalInit(make_external_init())])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidProposalTypeForSender));
    }

    #[test]
    fn sending_external_init_from_member_filters_it_out() {
        let (alice, tree) = new_tree("alice");
        let external_init = Proposal::ExternalInit(make_external_init());
        let external_init_info = make_proposal_info(&external_init, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    external_init_info.proposal_ref().unwrap().clone(),
                    external_init.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(
            processed_proposals.1.unused_proposals,
            vec![external_init_info]
        );
    }

    fn required_capabilities_proposal(extension: u16) -> Proposal {
        let required_capabilities = RequiredCapabilitiesExt {
            extensions: vec![extension.into()],
            ..Default::default()
        };

        let ext = vec![required_capabilities.into_extension().unwrap()];

        Proposal::GroupContextExtensions(ext.into())
    }

    #[test]
    fn receiving_required_capabilities_not_supported_by_member_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([required_capabilities_proposal(33)])
        ;

        assert_matches!(
            res,
            Err(MlsError::RequiredExtensionNotFound(v)) if v == 33.into()
        );
    }

    #[test]
    fn sending_required_capabilities_not_supported_by_member_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([required_capabilities_proposal(33)])
            .send()
            ;

        assert_matches!(
            res,
            Err(MlsError::RequiredExtensionNotFound(v)) if v == 33.into()
        );
    }

    #[test]
    fn sending_additional_required_capabilities_not_supported_by_member_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let proposal = required_capabilities_proposal(33);
        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn committing_update_from_pk1_to_pk2_and_update_from_pk2_to_pk3_works() {
        let (alice_leaf, alice_secret, alice_signer) =
            get_basic_test_node_sig_key(TEST_CIPHER_SUITE, "alice");

        let (mut tree, priv_tree) = TreeKemPublic::derive(
            alice_leaf.clone(),
            alice_secret,
            &BasicIdentityProvider,
            &Default::default(),
        )

        .unwrap();

        let alice = priv_tree.self_index;

        let bob = add_member(&mut tree, "bob");
        let carol = add_member(&mut tree, "carol");

        let bob_current_leaf = tree.get_leaf_node(bob).unwrap();

        let mut alice_new_leaf = LeafNode {
            public_key: bob_current_leaf.public_key.clone(),
            leaf_node_source: LeafNodeSource::Update,
            ..alice_leaf
        };

        alice_new_leaf
            .sign(
                &test_cipher_suite_provider(TEST_CIPHER_SUITE),
                &alice_signer,
                &(TEST_GROUP, 0).into(),
            )

            .unwrap();

        let bob_new_leaf = update_leaf_node("bob", 1);

        let pk1_to_pk2 = Proposal::Update(UpdateProposal {
            leaf_node: alice_new_leaf.clone(),
        });

        let pk1_to_pk2_ref = make_proposal_ref(&pk1_to_pk2, alice);

        let pk2_to_pk3 = Proposal::Update(UpdateProposal {
            leaf_node: bob_new_leaf.clone(),
        });

        let pk2_to_pk3_ref = make_proposal_ref(&pk2_to_pk3, bob);

        let effects = CommitReceiver::new(
            &tree,
            carol,
            carol,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(pk1_to_pk2_ref.clone(), pk1_to_pk2, alice)
        .cache(pk2_to_pk3_ref.clone(), pk2_to_pk3, bob)
        .receive([pk1_to_pk2_ref, pk2_to_pk3_ref])

        .unwrap();

        assert_eq!(effects.applied_proposals.update_senders, vec![alice, bob]);

        assert_eq!(
            effects
                .applied_proposals
                .updates
                .into_iter()
                .map(|p| p.proposal.leaf_node)
                .collect_vec(),
            vec![alice_new_leaf, bob_new_leaf]
        );
    }

    #[test]
    fn committing_update_from_pk1_to_pk2_and_removal_of_pk2_works() {
        let cipher_suite_provider = test_cipher_suite_provider(TEST_CIPHER_SUITE);

        let (alice_leaf, alice_secret, alice_signer) =
            get_basic_test_node_sig_key(TEST_CIPHER_SUITE, "alice");

        let (mut tree, priv_tree) = TreeKemPublic::derive(
            alice_leaf.clone(),
            alice_secret,
            &BasicIdentityProvider,
            &Default::default(),
        )

        .unwrap();

        let alice = priv_tree.self_index;

        let bob = add_member(&mut tree, "bob");
        let carol = add_member(&mut tree, "carol");

        let bob_current_leaf = tree.get_leaf_node(bob).unwrap();

        let mut alice_new_leaf = LeafNode {
            public_key: bob_current_leaf.public_key.clone(),
            leaf_node_source: LeafNodeSource::Update,
            ..alice_leaf
        };

        alice_new_leaf
            .sign(
                &cipher_suite_provider,
                &alice_signer,
                &(TEST_GROUP, 0).into(),
            )

            .unwrap();

        let pk1_to_pk2 = Proposal::Update(UpdateProposal {
            leaf_node: alice_new_leaf.clone(),
        });

        let pk1_to_pk2_ref = make_proposal_ref(&pk1_to_pk2, alice);

        let remove_pk2 = Proposal::Remove(RemoveProposal { to_remove: bob });

        let remove_pk2_ref = make_proposal_ref(&remove_pk2, bob);

        let effects = CommitReceiver::new(
            &tree,
            carol,
            carol,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(pk1_to_pk2_ref.clone(), pk1_to_pk2, alice)
        .cache(remove_pk2_ref.clone(), remove_pk2, bob)
        .receive([pk1_to_pk2_ref, remove_pk2_ref])

        .unwrap();

        assert_eq!(effects.applied_proposals.update_senders, vec![alice]);

        assert_eq!(
            effects
                .applied_proposals
                .updates
                .into_iter()
                .map(|p| p.proposal.leaf_node)
                .collect_vec(),
            vec![alice_new_leaf]
        );

        assert_eq!(
            effects
                .applied_proposals
                .removals
                .into_iter()
                .map(|p| p.proposal.to_remove)
                .collect_vec(),
            vec![bob]
        );
    }
    fn unsupported_credential_key_package(name: &str) -> KeyPackage {
        let (mut signing_identity, secret_key) =
            get_test_signing_identity(TEST_CIPHER_SUITE, name.as_bytes());

        signing_identity.credential = Credential::Custom(CustomCredential::new(
            CredentialType::new(BasicWithCustomProvider::CUSTOM_CREDENTIAL_TYPE),
            random_bytes(32),
        ));

        let generator = KeyPackageGenerator {
            protocol_version: TEST_PROTOCOL_VERSION,
            cipher_suite_provider: &test_cipher_suite_provider(TEST_CIPHER_SUITE),
            signing_identity: &signing_identity,
            signing_key: &secret_key,
        };

        generator
            .generate(
                Lifetime::years(1, None).unwrap(),
                Capabilities {
                    credentials: vec![42.into()],
                    ..Default::default()
                },
                Default::default(),
                Default::default(),
            )

            .unwrap()
            .key_package
    }

    #[test]
    fn receiving_add_with_leaf_not_supporting_credential_type_of_other_leaf_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::Add(Box::new(AddProposal {
            key_package: unsupported_credential_key_package("bob"),
        }))])
        ;

        assert_matches!(res, Err(MlsError::InUseCredentialTypeUnsupportedByNewLeaf));
    }

    #[test]
    fn sending_additional_add_with_leaf_not_supporting_credential_type_of_other_leaf_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::Add(Box::new(AddProposal {
                key_package: unsupported_credential_key_package("bob"),
            }))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::InUseCredentialTypeUnsupportedByNewLeaf));
    }

    #[test]
    fn sending_add_with_leaf_not_supporting_credential_type_of_other_leaf_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let add = Proposal::Add(Box::new(AddProposal {
            key_package: unsupported_credential_key_package("bob"),
        }));

        let add_info = make_proposal_info(&add, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(add_info.proposal_ref().unwrap().clone(), add.clone(), alice)
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![add_info]);
    }

    #[cfg(feature = "custom_proposal")]
    #[test]
    fn sending_custom_proposal_with_member_not_supporting_proposal_type_fails() {
        let (alice, tree) = new_tree("alice");

        let custom_proposal = Proposal::Custom(CustomProposal::new(ProposalType::new(42), vec![]));

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([custom_proposal.clone()])
            .send()
            ;

        assert_matches!(
            res,
            Err(
                MlsError::UnsupportedCustomProposal(c)
            ) if c == custom_proposal.proposal_type()
        );
    }

    #[cfg(feature = "custom_proposal")]
    #[test]
    fn sending_custom_proposal_with_member_not_supporting_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let custom_proposal = Proposal::Custom(CustomProposal::new(ProposalType::new(42), vec![]));

        let custom_info = make_proposal_info(&custom_proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    custom_info.proposal_ref().unwrap().clone(),
                    custom_proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![custom_info]);
    }

    #[cfg(feature = "custom_proposal")]
    #[test]
    fn receiving_custom_proposal_with_member_not_supporting_fails() {
        let (alice, tree) = new_tree("alice");

        let custom_proposal = Proposal::Custom(CustomProposal::new(ProposalType::new(42), vec![]));

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([custom_proposal.clone()])
        ;

        assert_matches!(
            res,
            Err(MlsError::UnsupportedCustomProposal(c)) if c == custom_proposal.proposal_type()
        );
    }

    #[test]
    fn receiving_group_extension_unsupported_by_leaf_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .receive([Proposal::GroupContextExtensions(make_extension_list(0))])
        ;

        assert_matches!(
            res,
            Err(
                MlsError::UnsupportedGroupExtension(v)
            ) if v == 42.into()
        );
    }

    #[test]
    fn sending_additional_group_extension_unsupported_by_leaf_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::GroupContextExtensions(make_extension_list(0))])
            .send()
            ;

        assert_matches!(
            res,
            Err(
                MlsError::UnsupportedGroupExtension(v)
            ) if v == 42.into()
        );
    }

    #[test]
    fn sending_group_extension_unsupported_by_leaf_filters_it_out() {
        let (alice, tree) = new_tree("alice");

        let proposal = Proposal::GroupContextExtensions(make_extension_list(0));
        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[cfg(feature = "psk")]
    #[derive(Debug)]
    struct AlwaysNotFoundPskStorage;

    #[cfg(feature = "psk")]
    impl PreSharedKeyStorage for AlwaysNotFoundPskStorage {
        type Error = Infallible;

        fn get(&self, _: &ExternalPskId) -> Result<Option<PreSharedKey>, Self::Error> {
            Ok(None)
        }
    }

    #[cfg(feature = "psk")]
    #[test]
    fn receiving_external_psk_with_unknown_id_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .with_psk_storage(AlwaysNotFoundPskStorage)
        .receive([Proposal::Psk(new_external_psk(b"abc"))])
        ;

        assert_matches!(res, Err(MlsError::MissingRequiredPsk));
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_additional_external_psk_with_unknown_id_fails() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_psk_storage(AlwaysNotFoundPskStorage)
            .with_additional([Proposal::Psk(new_external_psk(b"abc"))])
            .send()
            ;

        assert_matches!(res, Err(MlsError::MissingRequiredPsk));
    }

    #[cfg(feature = "psk")]
    #[test]
    fn sending_external_psk_with_unknown_id_filters_it_out() {
        let (alice, tree) = new_tree("alice");
        let proposal = Proposal::Psk(new_external_psk(b"abc"));
        let proposal_info = make_proposal_info(&proposal, alice);

        let processed_proposals =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .with_psk_storage(AlwaysNotFoundPskStorage)
                .cache(
                    proposal_info.proposal_ref().unwrap().clone(),
                    proposal.clone(),
                    alice,
                )
                .send()

                .unwrap();

        assert_eq!(processed_proposals.0, Vec::new());

        assert_eq!(processed_proposals.1.unused_proposals, vec![proposal_info]);
    }

    #[test]
    fn user_defined_filter_can_remove_proposals() {
        struct RemoveGroupContextExtensions;
        impl MlsRules for RemoveGroupContextExtensions {
            type Error = Infallible;

            fn filter_proposals(
                &self,
                _: CommitDirection,
                _: CommitSource,
                _: &Roster,
                _: &GroupContext,
                mut proposals: ProposalBundle,
            ) -> Result<ProposalBundle, Self::Error> {
                proposals.group_context_extensions.clear();
                Ok(proposals)
            }

            #[cfg_attr(coverage_nightly, coverage(off))]
            fn commit_options(
                &self,
                _: &Roster,
                _: &GroupContext,
                _: &ProposalBundle,
            ) -> Result<CommitOptions, Self::Error> {
                Ok(Default::default())
            }

            #[cfg_attr(coverage_nightly, coverage(off))]
            fn encryption_options(
                &self,
                _: &Roster,
                _: &GroupContext,
            ) -> Result<EncryptionOptions, Self::Error> {
                Ok(Default::default())
            }
        }

        let (alice, tree) = new_tree("alice");

        let (committed, _) =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .with_additional([Proposal::GroupContextExtensions(Default::default())])
                .with_user_rules(RemoveGroupContextExtensions)
                .send()

                .unwrap();

        assert_eq!(committed, Vec::new());
    }

    struct FailureMlsRules;
    impl MlsRules for FailureMlsRules {
        type Error = MlsError;

        fn filter_proposals(
            &self,
            _: CommitDirection,
            _: CommitSource,
            _: &Roster,
            _: &GroupContext,
            _: ProposalBundle,
        ) -> Result<ProposalBundle, Self::Error> {
            Err(MlsError::InvalidSignature)
        }

        #[cfg_attr(coverage_nightly, coverage(off))]
        fn commit_options(
            &self,
            _: &Roster,
            _: &GroupContext,
            _: &ProposalBundle,
        ) -> Result<CommitOptions, Self::Error> {
            Ok(Default::default())
        }

        #[cfg_attr(coverage_nightly, coverage(off))]
        fn encryption_options(
            &self,
            _: &Roster,
            _: &GroupContext,
        ) -> Result<EncryptionOptions, Self::Error> {
            Ok(Default::default())
        }
    }

    struct InjectMlsRules {
        to_inject: Vec<Proposal>,
        source: ProposalSource,
    }
    impl MlsRules for InjectMlsRules {
        type Error = MlsError;

        fn filter_proposals(
            &self,
            _: CommitDirection,
            _: CommitSource,
            _: &Roster,
            _: &GroupContext,
            mut proposals: ProposalBundle,
        ) -> Result<ProposalBundle, Self::Error> {
            for proposal in self.to_inject.iter().cloned() {
                proposals.add(proposal, Sender::Member(0), self.source.clone());
            }

            Ok(proposals)
        }

        #[cfg_attr(coverage_nightly, coverage(off))]
        fn commit_options(
            &self,
            _: &Roster,
            _: &GroupContext,
            _: &ProposalBundle,
        ) -> Result<CommitOptions, Self::Error> {
            Ok(Default::default())
        }

        #[cfg_attr(coverage_nightly, coverage(off))]
        fn encryption_options(
            &self,
            _: &Roster,
            _: &GroupContext,
        ) -> Result<EncryptionOptions, Self::Error> {
            Ok(Default::default())
        }
    }

    #[test]
    fn user_defined_filter_can_inject_proposals() {
        let (alice, tree) = new_tree("alice");

        let test_proposal = Proposal::GroupContextExtensions(Default::default());

        let (committed, _) =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .with_user_rules(InjectMlsRules {
                    to_inject: vec![test_proposal.clone()],
                    source: ProposalSource::ByValue,
                })
                .send()

                .unwrap();

        assert_eq!(
            committed,
            vec![ProposalOrRef::Proposal(test_proposal.into())]
        );
    }

    #[test]
    fn user_defined_filter_can_inject_local_only_proposals() {
        let (alice, tree) = new_tree("alice");

        let test_proposal = Proposal::GroupContextExtensions(Default::default());

        let (committed, _) =
            CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
                .with_user_rules(InjectMlsRules {
                    to_inject: vec![test_proposal.clone()],
                    source: ProposalSource::Local,
                })
                .send()

                .unwrap();

        assert_eq!(committed, vec![]);
    }

    #[test]
    fn user_defined_filter_cant_break_base_rules() {
        let (alice, tree) = new_tree("alice");

        let test_proposal = Proposal::Update(UpdateProposal {
            leaf_node: get_basic_test_node(TEST_CIPHER_SUITE, "leaf"),
        });

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_user_rules(InjectMlsRules {
                to_inject: vec![test_proposal.clone()],
                source: ProposalSource::ByValue,
            })
            .send()
            ;

        assert_matches!(res, Err(MlsError::InvalidProposalTypeForSender))
    }

    #[test]
    fn sending_invalid_local_proposal_fails() {
        let (alice, tree) = new_tree("alice");
        let gce_proposal = Proposal::GroupContextExtensions(Default::default());

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_user_rules(InjectMlsRules {
                to_inject: vec![gce_proposal.clone(), gce_proposal],
                source: ProposalSource::Local,
            })
            .send()
            ;

        assert_matches!(
            res,
            Err(MlsError::MoreThanOneGroupContextExtensionsProposal)
        );
    }

    #[test]
    fn user_defined_filter_can_refuse_to_send_commit() {
        let (alice, tree) = new_tree("alice");

        let res = CommitSender::new(&tree, alice, test_cipher_suite_provider(TEST_CIPHER_SUITE))
            .with_additional([Proposal::GroupContextExtensions(Default::default())])
            .with_user_rules(FailureMlsRules)
            .send()
            ;

        assert_matches!(res, Err(MlsError::MlsRulesError(_)));
    }

    #[test]
    fn user_defined_filter_can_reject_incoming_commit() {
        let (alice, tree) = new_tree("alice");

        let res = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .with_user_rules(FailureMlsRules)
        .receive([Proposal::GroupContextExtensions(Default::default())])
        ;

        assert_matches!(res, Err(MlsError::MlsRulesError(_)));
    }

    #[test]
    fn proposers_are_verified() {
        let (alice, mut tree) = new_tree("alice");
        let bob = add_member(&mut tree, "bob");

        #[cfg(feature = "by_ref_proposal")]
        let identity = get_test_signing_identity(TEST_CIPHER_SUITE, b"carol")

            .0;

        #[cfg(feature = "by_ref_proposal")]
        let external_senders = ExternalSendersExt::new(vec![identity]);

        let proposals: &[Proposal] = &[
            Proposal::Add(make_add_proposal()),
            Proposal::Update(make_update_proposal("alice")),
            Proposal::Remove(RemoveProposal { to_remove: bob }),
            #[cfg(feature = "psk")]
            Proposal::Psk(make_external_psk(
                b"ted",
                PskNonce::random(&test_cipher_suite_provider(TEST_CIPHER_SUITE)).unwrap(),
            )),
            Proposal::ReInit(make_reinit(TEST_PROTOCOL_VERSION)),
            Proposal::ExternalInit(make_external_init()),
            Proposal::GroupContextExtensions(Default::default()),
        ];

        let proposers = [
            Sender::Member(*alice),
            #[cfg(feature = "by_ref_proposal")]
            Sender::External(0),
            Sender::NewMemberCommit,
            Sender::NewMemberProposal,
        ];

        for ((proposer, proposal), by_ref) in proposers
            .into_iter()
            .cartesian_product(proposals)
            .cartesian_product([true])
        {
            let committer = Sender::Member(*alice);

            let receiver = CommitReceiver::new(
                &tree,
                committer,
                alice,
                test_cipher_suite_provider(TEST_CIPHER_SUITE),
            );

            #[cfg(feature = "by_ref_proposal")]
            let extensions: ExtensionList =
                vec![external_senders.clone().into_extension().unwrap()].into();

            #[cfg(feature = "by_ref_proposal")]
            let receiver = receiver.with_extensions(extensions);

            let (receiver, proposals, proposer, source) = if by_ref {
                let proposal_ref = make_proposal_ref(proposal, proposer);
                let receiver = receiver.cache(proposal_ref.clone(), proposal.clone(), proposer);
                (
                    receiver,
                    vec![ProposalOrRef::from(proposal_ref.clone())],
                    proposer,
                    ProposalSource::ByReference(proposal_ref),
                )
            } else {
                (
                    receiver,
                    vec![proposal.clone().into()],
                    committer,
                    ProposalSource::Local,
                )
            };

            let res = receiver.receive(proposals);

            if proposer_can_propose(proposer, proposal.proposal_type(), &source).is_err() {
                assert_matches!(res, Err(MlsError::InvalidProposalTypeForSender));
            } else {
                let is_self_update = proposal.proposal_type() == ProposalType::UPDATE
                    && by_ref
                    && matches!(proposer, Sender::Member(_));

                if !is_self_update {
                    res.unwrap();
                }
            }
        }
    }
    fn make_update_proposal(name: &str) -> UpdateProposal {
        UpdateProposal {
            leaf_node: update_leaf_node(name, 1),
        }
    }
    fn make_update_proposal_custom(name: &str, leaf_index: u32) -> UpdateProposal {
        UpdateProposal {
            leaf_node: update_leaf_node(name, leaf_index),
        }
    }

    #[test]
    fn when_receiving_commit_unused_proposals_are_proposals_in_cache_but_not_in_commit() {
        let (alice, tree) = new_tree("alice");

        let proposal = Proposal::GroupContextExtensions(Default::default());
        let proposal_ref = make_proposal_ref(&proposal, alice);

        let state = CommitReceiver::new(
            &tree,
            alice,
            alice,
            test_cipher_suite_provider(TEST_CIPHER_SUITE),
        )
        .cache(proposal_ref.clone(), proposal, alice)
        .receive([Proposal::Add(Box::new(AddProposal {
            key_package: test_key_package(TEST_PROTOCOL_VERSION, TEST_CIPHER_SUITE, "bob"),
        }))])

        .unwrap();

        let [p] = &state.unused_proposals[..] else {
            panic!(
                "Expected single unused proposal but got {:?}",
                state.unused_proposals
            );
        };

        assert_eq!(p.proposal_ref(), Some(&proposal_ref));
    }
}
