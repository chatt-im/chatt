use chatt_mls::{CIPHER_SUITE, ChattIdentityProvider, ChattMlsPolicy};
use mls_rs::{
    CipherSuiteProvider, Client, CryptoProvider, ExtensionList,
    client_builder::MlsConfig,
    external_client::{ExternalClient, ExternalReceivedMessage},
    group::ReceivedMessage,
    identity::{SigningIdentity, basic::BasicCredential},
};
use mls_rs_core::crypto::{SignaturePublicKey, SignatureSecretKey};
use mls_rs_crypto_awslc::AwsLcCryptoProvider;
use rpc::{
    identity::{
        DeviceCertificateBody, DeviceRosterBody, SignedDeviceRoster, account_id,
        authority_public_key, mls_client_id, sign_device_certificate, sign_device_roster,
    },
    ids::{DeviceId, RoomId, UserId},
    mls::EncryptedRoomDescriptor,
};

struct Device {
    roster: SignedDeviceRoster,
    signing_identity: SigningIdentity,
    signing_secret: SignatureSecretKey,
}

fn device(server: &[u8], user: u64, authority_byte: u8, device_byte: u8) -> Device {
    let crypto = AwsLcCryptoProvider::default();
    let cipher = crypto.cipher_suite_provider(CIPHER_SUITE).unwrap();
    let (signing_secret, signing_public) = cipher.signature_key_generate().unwrap();
    let authority_seed = [authority_byte; 32];
    let authority_public_key = authority_public_key(&authority_seed).unwrap();
    let user_id = UserId(user);
    let account_id = account_id(server, user_id, &authority_public_key);
    let device_id = DeviceId([device_byte; 16]);
    let client_id = mls_client_id(server, account_id, device_id).unwrap();
    let certificate = sign_device_certificate(
        DeviceCertificateBody {
            user_id,
            account_id,
            authority_public_key,
            device_id,
            device_name: format!("device {device_byte}"),
            mls_client_id: client_id.clone(),
            mls_signature_public_key: signing_public.as_ref().to_vec(),
        },
        &authority_seed,
    )
    .unwrap();
    let roster = sign_device_roster(
        DeviceRosterBody {
            user_id,
            account_id,
            authority_public_key,
            revision: 1,
            active_devices: vec![certificate],
        },
        &authority_seed,
    )
    .unwrap();
    Device {
        roster,
        signing_identity: SigningIdentity::new(
            BasicCredential::new(client_id).into_credential(),
            SignaturePublicKey::from(signing_public.as_ref().to_vec()),
        ),
        signing_secret,
    }
}

fn account_devices(
    server: &[u8],
    user: u64,
    authority_byte: u8,
    device_bytes: &[u8],
) -> Vec<Device> {
    let crypto = AwsLcCryptoProvider::default();
    let cipher = crypto.cipher_suite_provider(CIPHER_SUITE).unwrap();
    let authority_seed = [authority_byte; 32];
    let authority_public_key = authority_public_key(&authority_seed).unwrap();
    let user_id = UserId(user);
    let account_id = account_id(server, user_id, &authority_public_key);
    let mut material = device_bytes
        .iter()
        .map(|device_byte| {
            let (signing_secret, signing_public) = cipher.signature_key_generate().unwrap();
            let device_id = DeviceId([*device_byte; 16]);
            let client_id = mls_client_id(server, account_id, device_id).unwrap();
            let certificate = sign_device_certificate(
                DeviceCertificateBody {
                    user_id,
                    account_id,
                    authority_public_key,
                    device_id,
                    device_name: format!("device {device_byte}"),
                    mls_client_id: client_id.clone(),
                    mls_signature_public_key: signing_public.as_ref().to_vec(),
                },
                &authority_seed,
            )
            .unwrap();
            (certificate, client_id, signing_public, signing_secret)
        })
        .collect::<Vec<_>>();
    material.sort_by_key(|(certificate, ..)| certificate.body.device_id);
    let roster = sign_device_roster(
        DeviceRosterBody {
            user_id,
            account_id,
            authority_public_key,
            revision: 1,
            active_devices: material
                .iter()
                .map(|(certificate, ..)| certificate.clone())
                .collect(),
        },
        &authority_seed,
    )
    .unwrap();
    material
        .into_iter()
        .map(|(_, client_id, signing_public, signing_secret)| Device {
            roster: roster.clone(),
            signing_identity: SigningIdentity::new(
                BasicCredential::new(client_id).into_credential(),
                SignaturePublicKey::from(signing_public.as_ref().to_vec()),
            ),
            signing_secret,
        })
        .collect()
}

fn client(identities: ChattIdentityProvider, device: &Device) -> Client<impl MlsConfig> {
    Client::builder()
        .crypto_provider(AwsLcCryptoProvider::default())
        .identity_provider(identities.clone())
        .mls_rules(ChattMlsPolicy::new(identities))
        .signing_identity(
            device.signing_identity.clone(),
            device.signing_secret.clone(),
            CIPHER_SUITE,
        )
        .build()
}

#[test]
fn same_epoch_application_generations_decrypt_in_reverse_order() {
    let server_id = b"test server";
    let alice = device(server_id, 1, 1, 1);
    let bob = device(server_id, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server_id.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(10),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();

    let alice_client = client(identities.clone(), &alice);
    let bob_client = client(identities.clone(), &bob);
    let mut alice_group = alice_client
        .create_group_with_id(
            descriptor.mls_group_id,
            ExtensionList::new(),
            ExtensionList::new(),
            None,
        )
        .unwrap();
    let bob_key_package = bob_client
        .generate_key_package_message(ExtensionList::new(), ExtensionList::new(), None)
        .unwrap();
    let initial = alice_group
        .commit_builder()
        .add_member(bob_key_package)
        .unwrap()
        .build()
        .unwrap();
    let (mut bob_group, _) = bob_client
        .join_group(None, &initial.welcome_messages[0], None)
        .unwrap();
    alice_group.apply_pending_commit().unwrap();

    let first = alice_group
        .encrypt_application_message(b"first", Vec::new())
        .unwrap();
    let second = alice_group
        .encrypt_application_message(b"second", Vec::new())
        .unwrap();

    // These are consecutive secret-tree generations from the same sender in
    // the same epoch. Opening the later generation first requires mls-rs's
    // `out_of_order` support to retain the skipped generation key.
    assert_eq!(first.epoch(), second.epoch());

    let ReceivedMessage::ApplicationMessage(opened_second) =
        bob_group.process_incoming_message(second).unwrap()
    else {
        panic!("expected application message");
    };
    assert_eq!(opened_second.data(), b"second");
    let ReceivedMessage::ApplicationMessage(opened_first) =
        bob_group.process_incoming_message(first.clone()).unwrap()
    else {
        panic!("expected application message");
    };
    assert_eq!(opened_first.data(), b"first");
    assert!(bob_group.process_incoming_message(first).is_err());
}

#[test]
fn public_server_observer_accepts_only_one_commit_for_an_epoch() {
    let server_id = b"test server";
    let alice = device(server_id, 1, 1, 1);
    let bob = device(server_id, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server_id.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(10),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();

    let alice_client = client(identities.clone(), &alice);
    let bob_client = client(identities.clone(), &bob);
    let mut alice_group = alice_client
        .create_group_with_id(
            descriptor.mls_group_id,
            ExtensionList::new(),
            ExtensionList::new(),
            None,
        )
        .unwrap();
    let group_info = alice_group.group_info_message(true).unwrap();
    let mut observer = ExternalClient::builder()
        .crypto_provider(AwsLcCryptoProvider::default())
        .identity_provider(identities.clone())
        .mls_rules(ChattMlsPolicy::new(identities.clone()))
        .build()
        .observe_group(group_info, None, None)
        .unwrap();

    let bob_key_package = bob_client
        .generate_key_package_message(ExtensionList::new(), ExtensionList::new(), None)
        .unwrap();
    let accepted = alice_group
        .commit_builder()
        .add_member(bob_key_package)
        .unwrap()
        .build()
        .unwrap();
    let ExternalReceivedMessage::Commit(_) = observer
        .process_incoming_message(accepted.commit_message.clone())
        .unwrap()
    else {
        panic!("expected observed commit");
    };
    assert!(
        observer
            .process_incoming_message(accepted.commit_message)
            .is_err()
    );
}

#[test]
fn fixed_room_policy_rejects_removing_an_accounts_last_leaf() {
    let server_id = b"fixed account leaf test server";
    let alice = device(server_id, 1, 1, 1);
    let bob = device(server_id, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server_id.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(11),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();

    let alice_client = client(identities.clone(), &alice);
    let bob_client = client(identities.clone(), &bob);
    let mut alice_group = alice_client
        .create_group_with_id(
            descriptor.mls_group_id.clone(),
            ExtensionList::new(),
            ExtensionList::new(),
            None,
        )
        .unwrap();
    let bob_key_package = bob_client
        .generate_key_package_message(ExtensionList::new(), ExtensionList::new(), None)
        .unwrap();
    let _initial = alice_group
        .commit_builder()
        .add_member(bob_key_package)
        .unwrap()
        .build()
        .unwrap();
    alice_group.apply_pending_commit().unwrap();

    // Once Bob's old device is revoked it may be removed, but not until a
    // replacement leaf represents Bob's fixed room account.
    let replacement = device(server_id, 2, 2, 3);
    let mut replacement_roster = replacement.roster;
    replacement_roster.body.revision = 2;
    replacement_roster = sign_device_roster(replacement_roster.body, &[2; 32]).unwrap();
    identities.install_roster(&replacement_roster).unwrap();
    assert!(
        alice_group
            .commit_builder()
            .remove_member(1)
            .unwrap()
            .build()
            .is_err()
    );
}

#[test]
fn two_multi_device_accounts_receive_constant_size_application_messages() {
    let server_id = b"four device test server";
    let alice = account_devices(server_id, 1, 1, &[1, 2]);
    let bob = account_devices(server_id, 2, 2, &[3, 4]);
    let identities = ChattIdentityProvider::new(server_id.to_vec());
    identities.install_roster(&alice[0].roster).unwrap();
    identities.install_roster(&bob[0].roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(20),
        alice[0].roster.body.account_id,
        vec![alice[0].roster.body.account_id, bob[0].roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();
    let clients = [&alice[0], &alice[1], &bob[0], &bob[1]]
        .map(|device| client(identities.clone(), device));
    let mut group = clients[0]
        .create_group_with_id(
            descriptor.mls_group_id,
            ExtensionList::new(),
            ExtensionList::new(),
            None,
        )
        .unwrap();
    let packages = clients[1..]
        .iter()
        .map(|client| {
            client
                .generate_key_package_message(ExtensionList::new(), ExtensionList::new(), None)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let mut builder = group.commit_builder();
    for package in packages {
        builder = builder.add_member(package).unwrap();
    }
    let initial = builder.build().unwrap();
    assert_eq!(initial.welcome_messages.len(), 1);
    let mut receivers = clients[1..]
        .iter()
        .map(|client| {
            client
                .join_group(None, &initial.welcome_messages[0], None)
                .unwrap()
                .0
        })
        .collect::<Vec<_>>();
    group.apply_pending_commit().unwrap();
    let payload = b"same plaintext independent of leaf count";
    let message = group
        .encrypt_application_message(payload, Vec::new())
        .unwrap();
    let four_device_size = message.to_bytes().unwrap().len();
    for receiver in &mut receivers {
        let ReceivedMessage::ApplicationMessage(opened) =
            receiver.process_incoming_message(message.clone()).unwrap()
        else {
            panic!("expected application message");
        };
        assert_eq!(opened.data(), payload);
    }

    let single_alice = device(server_id, 11, 11, 11);
    let single_bob = device(server_id, 12, 12, 12);
    let two_identities = ChattIdentityProvider::new(server_id.to_vec());
    two_identities.install_roster(&single_alice.roster).unwrap();
    two_identities.install_roster(&single_bob.roster).unwrap();
    let two_descriptor = EncryptedRoomDescriptor::new(
        RoomId(21),
        single_alice.roster.body.account_id,
        vec![single_alice.roster.body.account_id, single_bob.roster.body.account_id],
        100,
    )
    .unwrap();
    two_identities.install_room(two_descriptor.clone()).unwrap();
    let two_alice = client(two_identities.clone(), &single_alice);
    let two_bob = client(two_identities, &single_bob);
    let mut two_group = two_alice
        .create_group_with_id(
            two_descriptor.mls_group_id,
            ExtensionList::new(),
            ExtensionList::new(),
            None,
        )
        .unwrap();
    let package = two_bob
        .generate_key_package_message(ExtensionList::new(), ExtensionList::new(), None)
        .unwrap();
    let commit = two_group
        .commit_builder()
        .add_member(package)
        .unwrap()
        .build()
        .unwrap();
    two_group.apply_pending_commit().unwrap();
    let two_device_size = two_group
        .encrypt_application_message(payload, Vec::new())
        .unwrap()
        .to_bytes()
        .unwrap()
        .len();
    assert_eq!(two_device_size, four_device_size);
    assert!(!commit.welcome_messages.is_empty());
}

#[test]
fn future_epoch_application_retries_after_commit_and_duplicate_commit_is_harmless() {
    let server_id = b"future epoch test server";
    let alice = device(server_id, 1, 1, 1);
    let bob = device(server_id, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server_id.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(30),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();
    let alice_client = client(identities.clone(), &alice);
    let bob_client = client(identities, &bob);
    let mut alice_group = alice_client
        .create_group_with_id(
            descriptor.mls_group_id,
            ExtensionList::new(),
            ExtensionList::new(),
            None,
        )
        .unwrap();
    let package = bob_client
        .generate_key_package_message(ExtensionList::new(), ExtensionList::new(), None)
        .unwrap();
    let initial = alice_group
        .commit_builder()
        .add_member(package)
        .unwrap()
        .build()
        .unwrap();
    let (mut bob_group, _) = bob_client
        .join_group(None, &initial.welcome_messages[0], None)
        .unwrap();
    alice_group.apply_pending_commit().unwrap();

    let update = alice_group.commit_builder().build().unwrap();
    alice_group.apply_pending_commit().unwrap();
    let future = alice_group
        .encrypt_application_message(b"future epoch", Vec::new())
        .unwrap();
    assert!(bob_group.process_incoming_message(future.clone()).is_err());
    assert!(matches!(
        bob_group
            .process_incoming_message(update.commit_message.clone())
            .unwrap(),
        ReceivedMessage::Commit(_)
    ));
    let ReceivedMessage::ApplicationMessage(opened) =
        bob_group.process_incoming_message(future).unwrap()
    else {
        panic!("expected buffered future application retry");
    };
    assert_eq!(opened.data(), b"future epoch");
    assert!(
        bob_group
            .process_incoming_message(update.commit_message)
            .is_err()
    );
}
