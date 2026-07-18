// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright by contributors to this project.
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use core::ops::{Deref, DerefMut};

use alloc::format;
use rand::RngCore;

use super::*;
use crate::{
    client::{
        test_utils::{
            test_client_with_key_pkg, test_client_with_key_pkg_custom, TEST_CIPHER_SUITE,
            TEST_PROTOCOL_VERSION,
        },
        MlsError,
    },
    client_builder::test_utils::{TestClientBuilder, TestClientConfig},
    crypto::test_utils::test_cipher_suite_provider,
    extension::ExtensionType,
    identity::test_utils::get_test_signing_identity,
    key_package::{KeyPackageGeneration, KeyPackageGenerator},
    mls_rules::{CommitOptions, DefaultMlsRules},
    tree_kem::{leaf_node::test_utils::get_test_capabilities, Lifetime},
};

use crate::extension::RequiredCapabilitiesExt;

#[cfg(not(feature = "by_ref_proposal"))]
use crate::crypto::HpkePublicKey;

pub const TEST_GROUP: &[u8] = b"group";

#[derive(Clone)]
pub(crate) struct TestGroup {
    pub group: Group<TestClientConfig>,
}

impl Deref for TestGroup {
    type Target = Group<TestClientConfig>;

    fn deref(&self) -> &Self::Target {
        &self.group
    }
}

impl DerefMut for TestGroup {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.group
    }
}

impl TestGroup {
    #[cfg(feature = "external_client")]
    pub(crate) fn propose(&mut self, proposal: Proposal) -> MlsMessage {
        self.proposal_message(proposal, vec![]).unwrap()
    }

    #[cfg(feature = "by_ref_proposal")]
    pub(crate) fn update_proposal(&mut self) -> Proposal {
        self.group.update_proposal(None, None, None).unwrap()
    }
    pub(crate) fn join_with_custom_config<F>(
        &mut self,
        name: &str,
        custom_kp: bool,
        mut config: F,
    ) -> Result<(TestGroup, MlsMessage), MlsError>
    where
        F: FnMut(&mut TestClientConfig),
    {
        let (mut new_client, new_key_package) = if custom_kp {
            test_client_with_key_pkg_custom(
                self.protocol_version(),
                self.cipher_suite(),
                name,
                Default::default(),
                Default::default(),
                &mut config,
            )

        } else {
            test_client_with_key_pkg(self.protocol_version(), self.cipher_suite(), name)
        };

        // Add new member to the group
        let CommitOutput {
            welcome_messages,
            ratchet_tree,
            commit_message,
            ..
        } = self
            .commit_builder()
            .add_member(new_key_package)
            .unwrap()
            .build()

            .unwrap();

        // Apply the commit to the original group
        self.apply_pending_commit().unwrap();

        config(&mut new_client.config);

        // Group from new member's perspective
        let (new_group, _) = Group::join(
            &welcome_messages[0],
            ratchet_tree,
            new_client.config.clone(),
            new_client.signer.clone().unwrap(),
            None,
        )
        ?;

        let new_test_group = TestGroup { group: new_group };

        Ok((new_test_group, commit_message))
    }
    pub(crate) fn join(&mut self, name: &str) -> (TestGroup, MlsMessage) {
        self.join_with_custom_config(name, false, |_| ())

            .unwrap()
    }
    pub(crate) fn process_pending_commit(
        &mut self,
    ) -> Result<CommitMessageDescription, MlsError> {
        self.apply_pending_commit()
    }
    pub(crate) fn process_message(
        &mut self,
        message: MlsMessage,
    ) -> Result<ReceivedMessage, MlsError> {
        self.process_incoming_message(message)
    }

    #[cfg(feature = "private_message")]
    pub(crate) fn make_plaintext(&mut self, content: Content) -> MlsMessage {
        let auth_content = AuthenticatedContent::new_signed(
            &self.cipher_suite_provider,
            &self.state.context,
            Sender::Member(*self.private_tree.self_index),
            content,
            &self.signer,
            WireFormat::PublicMessage,
            Vec::new(),
        )

        .unwrap();

        self.format_for_wire(auth_content).unwrap()
    }
}
pub(crate) fn get_test_group_context(epoch: u64, cipher_suite: CipherSuite) -> GroupContext {
    let cs = test_cipher_suite_provider(cipher_suite);

    GroupContext {
        protocol_version: TEST_PROTOCOL_VERSION,
        cipher_suite,
        group_id: TEST_GROUP.to_vec(),
        epoch,
        tree_hash: cs.hash(&[1, 2, 3]).unwrap(),
        confirmed_transcript_hash: cs.hash(&[3, 2, 1]).unwrap().into(),
        extensions: ExtensionList::from(vec![]),
    }
}

#[cfg(feature = "prior_epoch")]
pub(crate) fn get_test_group_context_with_id(
    group_id: Vec<u8>,
    epoch: u64,
    cipher_suite: CipherSuite,
) -> GroupContext {
    GroupContext {
        protocol_version: TEST_PROTOCOL_VERSION,
        cipher_suite,
        group_id,
        epoch,
        tree_hash: vec![],
        confirmed_transcript_hash: ConfirmedTranscriptHash::from(vec![]),
        extensions: ExtensionList::from(vec![]),
    }
}

pub(crate) fn group_extensions() -> ExtensionList {
    let required_capabilities = RequiredCapabilitiesExt::default();

    let mut extensions = ExtensionList::new();
    extensions.set_from(required_capabilities).unwrap();
    extensions
}

pub(crate) fn lifetime() -> Lifetime {
    Lifetime::years(1, None).unwrap()
}
pub(crate) fn test_member(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    identifier: &[u8],
) -> (KeyPackageGeneration, SignatureSecretKey) {
    let (signing_identity, signing_key) = get_test_signing_identity(cipher_suite, identifier);

    let key_package_generator = KeyPackageGenerator {
        protocol_version,
        cipher_suite_provider: &test_cipher_suite_provider(cipher_suite),
        signing_identity: &signing_identity,
        signing_key: &signing_key,
    };

    let key_package = key_package_generator
        .generate(
            lifetime(),
            get_test_capabilities(),
            ExtensionList::default(),
            ExtensionList::default(),
        )

        .unwrap();

    (key_package, signing_key)
}
pub(crate) fn test_group_custom(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    extension_types: Vec<ExtensionType>,
    leaf_extensions: Option<ExtensionList>,
    commit_options: Option<CommitOptions>,
) -> TestGroup {
    let leaf_extensions = leaf_extensions.unwrap_or_default();
    let commit_options = commit_options.unwrap_or_default();

    let (signing_identity, secret_key) = get_test_signing_identity(cipher_suite, b"member");
    let group = TestClientBuilder::new_for_test()
        .mls_rules(DefaultMlsRules::default().with_commit_options(commit_options))
        .extension_types(extension_types)
        .protocol_versions(ProtocolVersion::all())
        .used_protocol_version(protocol_version)
        .signing_identity(signing_identity.clone(), secret_key, cipher_suite)
        .build()
        .create_group_with_id(
            TEST_GROUP.to_vec(),
            group_extensions(),
            leaf_extensions,
            None,
        )

        .unwrap();

    TestGroup { group }
}
pub(crate) fn test_group(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
) -> TestGroup {
    test_group_custom(
        protocol_version,
        cipher_suite,
        Default::default(),
        None,
        None,
    )

}
pub(crate) fn test_group_custom_config<F>(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    custom: F,
) -> TestGroup
where
    F: FnOnce(TestClientBuilder) -> TestClientBuilder,
{
    let (signing_identity, secret_key) = get_test_signing_identity(cipher_suite, b"member");

    let client_builder = TestClientBuilder::new_for_test().used_protocol_version(protocol_version);

    let group = custom(client_builder)
        .signing_identity(signing_identity.clone(), secret_key, cipher_suite)
        .build()
        .create_group_with_id(
            TEST_GROUP.to_vec(),
            group_extensions(),
            Default::default(),
            None,
        )

        .unwrap();

    TestGroup { group }
}
pub(crate) fn test_n_member_group(
    protocol_version: ProtocolVersion,
    cipher_suite: CipherSuite,
    num_members: usize,
) -> Vec<TestGroup> {
    let group = test_group(protocol_version, cipher_suite);

    let mut groups = vec![group];

    for i in 1..num_members {
        let (new_group, commit) = groups.get_mut(0).unwrap().join(&format!("name {i}"));
        process_commit(&mut groups, commit, 0);
        groups.push(new_group);
    }

    groups
}
pub(crate) fn process_commit(groups: &mut [TestGroup], commit: MlsMessage, excluded: u32) {
    for g in groups
        .iter_mut()
        .filter(|g| g.current_member_index() != excluded)
    {
        g.process_message(commit.clone()).unwrap();
    }
}

pub(crate) fn get_test_25519_key(key_byte: u8) -> HpkePublicKey {
    vec![key_byte; 32].into()
}
pub(crate) fn get_test_groups_with_features(
    n: usize,
    extensions: ExtensionList,
    leaf_extensions: ExtensionList,
) -> Vec<Group<TestClientConfig>> {
    let mut clients = Vec::new();

    for i in 0..n {
        let (identity, secret_key) =
            get_test_signing_identity(TEST_CIPHER_SUITE, format!("member{i}").as_bytes());

        clients.push(
            TestClientBuilder::new_for_test()
                .extension_type(999.into())
                .signing_identity(identity, secret_key, TEST_CIPHER_SUITE)
                .build(),
        );
    }

    let group = clients[0]
        .create_group_with_id(
            b"TEST GROUP".to_vec(),
            extensions,
            leaf_extensions.clone(),
            None,
        )

        .unwrap();

    let mut groups = vec![group];

    for client in clients.iter().skip(1) {
        let key_package = client
            .generate_key_package_message(Default::default(), leaf_extensions.clone(), None)

            .unwrap();

        let commit_output = groups[0]
            .commit_builder()
            .add_member(key_package)
            .unwrap()
            .build()

            .unwrap();

        groups[0].apply_pending_commit().unwrap();

        for group in groups.iter_mut().skip(1) {
            group
                .process_incoming_message(commit_output.commit_message.clone())

                .unwrap();
        }

        groups.push(
            client
                .join_group(None, &commit_output.welcome_messages[0], None)

                .unwrap()
                .0,
        );
    }

    groups
}

pub fn random_bytes(count: usize) -> Vec<u8> {
    let mut buf = vec![0; count];
    rand::rng().fill_bytes(&mut buf);
    buf
}

pub(crate) struct GroupWithoutKeySchedule {
    inner: Group<TestClientConfig>,
    pub secrets: Option<(TreeKemPrivate, PathSecret)>,
    pub provisional_public_state: Option<ProvisionalState>,
}

impl Deref for GroupWithoutKeySchedule {
    type Target = Group<TestClientConfig>;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for GroupWithoutKeySchedule {
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

#[cfg(feature = "rfc_compliant")]
impl GroupWithoutKeySchedule {
    pub fn new(cs: CipherSuite) -> Self {
        Self {
            inner: test_group(TEST_PROTOCOL_VERSION, cs).group,
            secrets: None,
            provisional_public_state: None,
        }
    }
}
impl MessageProcessor for GroupWithoutKeySchedule {
    type CipherSuiteProvider = <Group<TestClientConfig> as MessageProcessor>::CipherSuiteProvider;
    type OutputType = <Group<TestClientConfig> as MessageProcessor>::OutputType;
    type PreSharedKeyStorage = <Group<TestClientConfig> as MessageProcessor>::PreSharedKeyStorage;
    type IdentityProvider = <Group<TestClientConfig> as MessageProcessor>::IdentityProvider;
    type MlsRules = <Group<TestClientConfig> as MessageProcessor>::MlsRules;

    fn group_state(&self) -> &GroupState {
        self.inner.group_state()
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn group_state_mut(&mut self) -> &mut GroupState {
        self.inner.group_state_mut()
    }

    fn mls_rules(&self) -> Self::MlsRules {
        self.inner.mls_rules()
    }

    fn identity_provider(&self) -> Self::IdentityProvider {
        self.inner.identity_provider()
    }

    fn cipher_suite_provider(&self) -> &Self::CipherSuiteProvider {
        self.inner.cipher_suite_provider()
    }

    fn psk_storage(&self) -> Self::PreSharedKeyStorage {
        self.inner.psk_storage()
    }

    fn removal_proposal(
        &self,
        provisional_state: &ProvisionalState,
    ) -> Option<ProposalInfo<RemoveProposal>> {
        self.inner.removal_proposal(provisional_state)
    }

    #[cfg(all(
        feature = "by_ref_proposal",
        feature = "custom_proposal",
        feature = "self_remove_proposal"
    ))]
    fn self_removal_proposal(
        &self,
        provisional_state: &ProvisionalState,
    ) -> Option<ProposalInfo<SelfRemoveProposal>> {
        self.inner.self_removal_proposal(provisional_state)
    }

    #[cfg(feature = "private_message")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn min_epoch_available(&self) -> Option<u64> {
        self.inner.min_epoch_available()
    }

    fn apply_update_path(
        &mut self,
        sender: LeafIndex,
        update_path: &ValidatedUpdatePath,
        provisional_state: &mut ProvisionalState,
    ) -> Result<Option<(TreeKemPrivate, PathSecret)>, MlsError> {
        self.inner
            .apply_update_path(sender, update_path, provisional_state)

    }

    #[cfg(feature = "private_message")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn process_ciphertext(
        &mut self,
        cipher_text: &PrivateMessage,
    ) -> Result<EventOrContent<Self::OutputType>, MlsError> {
        self.inner.process_ciphertext(cipher_text)
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn verify_plaintext_authentication(
        &self,
        message: PublicMessage,
    ) -> Result<EventOrContent<Self::OutputType>, MlsError> {
        self.inner.verify_plaintext_authentication(message)
    }

    fn update_key_schedule(
        &mut self,
        secrets: Option<(TreeKemPrivate, PathSecret)>,
        _interim_transcript_hash: InterimTranscriptHash,
        _confirmation_tag: &ConfirmationTag,
        provisional_public_state: ProvisionalState,
    ) -> Result<(), MlsError> {
        self.provisional_public_state = Some(provisional_public_state);
        self.secrets = secrets;
        Ok(())
    }

    #[cfg(all(feature = "export_key_generation", feature = "private_message"))]
    fn get_unauthenticated_key_generation_from_sender_data(
        &mut self,
        cipher_text: &PrivateMessage,
    ) -> Result<Option<u32>, MlsError> {
        self.inner
            .get_unauthenticated_key_generation_from_sender_data(cipher_text)

    }
}
