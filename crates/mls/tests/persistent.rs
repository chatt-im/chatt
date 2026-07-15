use chatt_mls::{CIPHER_SUITE, ChattIdentityProvider, PersistentClient, ProcessedDelivery};
use mls_rs::{
    CipherSuiteProvider, CryptoProvider,
    identity::{SigningIdentity, basic::BasicCredential},
};
use mls_rs_core::crypto::{SignaturePublicKey, SignatureSecretKey};
use mls_rs_crypto_rustcrypto::RustCryptoProvider;
use rpc::{
    identity::{
        DeviceCertificateBody, DeviceRosterBody, SignedDeviceRoster, account_id,
        authority_public_key, mls_client_id, sign_device_certificate, sign_device_roster,
    },
    ids::{DeviceId, EventId, RoomId, UserId},
    mls::{
        ChattEventContent, EncryptedRoomDescriptor, MLS_PROTOCOL_VERSION, MlsChattEvent,
        MlsDeliveryEvent, MlsWelcome,
    },
};

struct Device {
    id: DeviceId,
    roster: SignedDeviceRoster,
    signing_identity: SigningIdentity,
    signing_secret: SignatureSecretKey,
}

fn device(server: &[u8], user: u64, authority_byte: u8, device_byte: u8) -> Device {
    let cipher = RustCryptoProvider::default()
        .cipher_suite_provider(CIPHER_SUITE)
        .unwrap();
    let (signing_secret, signing_public) = cipher.signature_key_generate().unwrap();
    let authority_seed = [authority_byte; 32];
    let authority_public_key = authority_public_key(&authority_seed).unwrap();
    let user_id = UserId(user);
    let account_id = account_id(server, user_id, &authority_public_key);
    let id = DeviceId([device_byte; 16]);
    let client_id = mls_client_id(server, account_id, id).unwrap();
    let certificate = sign_device_certificate(
        DeviceCertificateBody {
            user_id,
            account_id,
            authority_public_key,
            device_id: id,
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
        id,
        roster,
        signing_identity: SigningIdentity::new(
            BasicCredential::new(client_id).into_credential(),
            SignaturePublicKey::from(signing_public.as_ref().to_vec()),
        ),
        signing_secret,
    }
}

#[test]
fn outbox_rejects_sender_account_not_bound_to_local_credential() {
    let temp = tempfile::tempdir().unwrap();
    let server = b"outbox sender binding test server";
    let alice = device(server, 1, 1, 1);
    let bob = device(server, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let client = PersistentClient::open(
        &temp.path().join("alice.db"),
        [11; 32],
        identities,
        alice.signing_identity,
        alice.signing_secret,
    )
    .unwrap();
    let event = MlsChattEvent {
        version: MLS_PROTOCOL_VERSION,
        room_id: RoomId(77),
        event_id: EventId([9; 16]),
        sender_account: bob.roster.body.account_id,
        timestamp_ms: 200,
        content: ChattEventContent::Text {
            body: "wrong sender".to_string(),
        },
    };

    assert!(
        client
            .queue_outgoing(event)
            .unwrap_err()
            .contains("event context is invalid")
    );
    assert!(client.pending_outbox().unwrap().is_empty());
}

#[test]
fn sqlcipher_reopen_preserves_exact_outbox_and_received_history() {
    let temp = tempfile::tempdir().unwrap();
    let server = b"persistent test server";
    let alice = device(server, 1, 1, 1);
    let bob = device(server, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(77),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();

    let alice_path = temp.path().join("alice.db");
    let bob_path = temp.path().join("bob.db");
    let alice_client = PersistentClient::open(
        &alice_path,
        [11; 32],
        identities.clone(),
        alice.signing_identity,
        alice.signing_secret,
    )
    .unwrap();
    let bob_client = PersistentClient::open(
        &bob_path,
        [22; 32],
        identities.clone(),
        bob.signing_identity,
        bob.signing_secret,
    )
    .unwrap();
    let bob_package = bob_client
        .generate_key_packages(bob.id, 1)
        .unwrap()
        .remove(0);
    let bundle = alice_client
        .create_room(&descriptor, &[(bob.id, bob_package.package)])
        .unwrap();
    let mut unrelated_group_info = bundle.group_info.clone();
    unrelated_group_info[0] ^= 1;
    assert!(
        !alice_client
            .recover_accepted_room_creation(&descriptor, &unrelated_group_info)
            .unwrap()
    );
    assert!(
        alice_client
            .recover_accepted_room_creation(&descriptor, &bundle.group_info)
            .unwrap()
    );
    let shared_welcome = bundle.welcome.as_ref().unwrap();
    let welcome = MlsWelcome {
        delivery_id: 1,
        sequence: 1,
        device_id: bob.id,
        descriptor: shared_welcome.descriptor.clone(),
        welcome: shared_welcome.welcome.clone(),
    };
    bob_client.join_welcome(&descriptor, &welcome).unwrap();

    let event = MlsChattEvent {
        version: MLS_PROTOCOL_VERSION,
        room_id: descriptor.room_id,
        event_id: EventId([9; 16]),
        sender_account: alice.roster.body.account_id,
        timestamp_ms: 200,
        content: ChattEventContent::Text {
            body: "persistent hello".to_string(),
        },
    };
    alice_client.queue_outgoing(event.clone()).unwrap();
    let (epoch, ciphertext) = alice_client
        .encrypt_outgoing(&descriptor, event.event_id)
        .unwrap();
    let same = alice_client
        .encrypt_outgoing(&descriptor, event.event_id)
        .unwrap();
    assert_eq!(same, (epoch, ciphertext.clone()));
    assert_eq!(alice_client.pending_outbox().unwrap().len(), 1);
    let delivery = MlsDeliveryEvent::Application {
        sequence: 2,
        epoch,
        event_id: event.event_id,
        ciphertext,
    };
    let ProcessedDelivery::Application(received) =
        bob_client.process_delivery(&descriptor, &delivery).unwrap()
    else {
        panic!("expected an application event");
    };
    assert_eq!(received.event, event);
    drop(alice_client);
    drop(bob_client);

    let alice_client = PersistentClient::reopen(&alice_path, [11; 32], identities.clone()).unwrap();
    let bob_client = PersistentClient::reopen(&bob_path, [22; 32], identities.clone()).unwrap();
    assert_eq!(bob_client.cursor(descriptor.room_id).unwrap(), 2);
    assert_eq!(
        bob_client
            .cached_event(descriptor.room_id, event.event_id)
            .unwrap()
            .unwrap()
            .event,
        event,
    );
    assert!(
        alice_client
            .encrypt_outgoing(&descriptor, event.event_id)
            .is_ok()
    );
    let resumed = alice_client.pending_outbox().unwrap();
    assert_eq!(resumed.len(), 1);
    assert!(matches!(
        &resumed[0].state,
        chatt_mls::OutboxState::PendingDelivery { ciphertext: stored, .. }
            if stored == &same.1
    ));
    assert!(matches!(
        alice_client.process_delivery(&descriptor, &delivery).unwrap(),
        ProcessedDelivery::Outgoing {
            sequence: 2,
            event: Some(ref recovered),
        } if recovered == &event
    ));
    assert_eq!(alice_client.cursor(descriptor.room_id).unwrap(), 2);
    assert!(alice_client.pending_outbox().unwrap().is_empty());
    assert!(PersistentClient::reopen(&bob_path, [23; 32], identities).is_err());
}

#[test]
fn sender_crash_boundaries_resume_plaintext_and_reuse_exact_ciphertext() {
    let temp = tempfile::tempdir().unwrap();
    let server = b"sender crash test server";
    let alice = device(server, 1, 1, 1);
    let bob = device(server, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(78),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();
    let alice_path = temp.path().join("alice.db");
    let alice_client = PersistentClient::open(
        &alice_path,
        [31; 32],
        identities.clone(),
        alice.signing_identity.clone(),
        alice.signing_secret.clone(),
    )
    .unwrap();
    let bob_client = PersistentClient::open(
        &temp.path().join("bob.db"),
        [32; 32],
        identities.clone(),
        bob.signing_identity.clone(),
        bob.signing_secret.clone(),
    )
    .unwrap();
    let package = bob_client
        .generate_key_packages(bob.id, 1)
        .unwrap()
        .remove(0);
    let initial = alice_client
        .create_room(&descriptor, &[(bob.id, package.package)])
        .unwrap();
    alice_client.accept_pending_commit(&descriptor, 1).unwrap();
    let shared = initial.welcome.as_ref().unwrap();
    bob_client
        .join_welcome(
            &descriptor,
            &MlsWelcome {
                delivery_id: 1,
                sequence: 1,
                device_id: bob.id,
                descriptor: shared.descriptor.clone(),
                welcome: shared.welcome.clone(),
            },
        )
        .unwrap();
    let event = MlsChattEvent {
        version: MLS_PROTOCOL_VERSION,
        room_id: descriptor.room_id,
        event_id: EventId([10; 16]),
        sender_account: alice.roster.body.account_id,
        timestamp_ms: 201,
        content: ChattEventContent::Text {
            body: "persist before encryption".to_string(),
        },
    };
    alice_client.queue_outgoing(event.clone()).unwrap();
    drop(alice_client);

    let alice_client = PersistentClient::reopen(&alice_path, [31; 32], identities.clone()).unwrap();
    let pending = alice_client.pending_outbox().unwrap();
    assert!(matches!(
        pending.as_slice(),
        [entry] if entry.event == event
            && matches!(entry.state, chatt_mls::OutboxState::PendingEncryption)
    ));
    let encrypted = alice_client
        .encrypt_outgoing(&descriptor, event.event_id)
        .unwrap();
    drop(alice_client);

    let alice_client = PersistentClient::reopen(&alice_path, [31; 32], identities).unwrap();
    assert_eq!(
        alice_client
            .encrypt_outgoing(&descriptor, event.event_id)
            .unwrap(),
        encrypted,
    );
    alice_client
        .retry_stale_outgoing(descriptor.room_id, event.event_id, encrypted.0 + 1)
        .unwrap();
    alice_client
        .retry_stale_outgoing(descriptor.room_id, event.event_id, encrypted.0 + 1)
        .unwrap();
    assert!(matches!(
        alice_client.outbox(descriptor.room_id, event.event_id).unwrap().state,
        chatt_mls::OutboxState::PendingEncryption
    ));
    let reencrypted = alice_client
        .encrypt_outgoing(&descriptor, event.event_id)
        .unwrap();
    alice_client
        .retry_stale_outgoing(descriptor.room_id, event.event_id, reencrypted.0)
        .unwrap();
    assert!(matches!(
        alice_client.outbox(descriptor.room_id, event.event_id).unwrap().state,
        chatt_mls::OutboxState::PendingDelivery { epoch, .. } if epoch == reencrypted.0
    ));
    assert!(alice_client
        .mark_outgoing_delivered(descriptor.room_id, event.event_id, 2)
        .unwrap());
    alice_client
        .retry_stale_outgoing(descriptor.room_id, event.event_id, encrypted.0 + 1)
        .unwrap();
    assert!(matches!(
        alice_client.outbox(descriptor.room_id, event.event_id).unwrap().state,
        chatt_mls::OutboxState::Delivered {
            sequence: 2,
            epoch: _,
            ciphertext_hash: _,
            emitted: true
        }
    ));

    let delayed = MlsChattEvent {
        event_id: EventId([11; 16]),
        timestamp_ms: 202,
        content: ChattEventContent::Text {
            body: "acknowledged behind a delivery gap".to_string(),
        },
        ..event.clone()
    };
    alice_client.queue_outgoing(delayed.clone()).unwrap();
    let (delayed_epoch, delayed_ciphertext) = alice_client
        .encrypt_outgoing(&descriptor, delayed.event_id)
        .unwrap();
    assert!(!alice_client
        .mark_outgoing_delivered(descriptor.room_id, delayed.event_id, 4)
        .unwrap());
    assert_eq!(alice_client.cursor(descriptor.room_id).unwrap(), 2);
    assert!(matches!(
        alice_client
            .outbox(descriptor.room_id, delayed.event_id)
            .unwrap()
            .state,
        chatt_mls::OutboxState::Delivered {
            sequence: 4,
            epoch: _,
            ciphertext_hash: _,
            emitted: false
        }
    ));
    assert!(
        alice_client
            .cached_history(descriptor.room_id)
            .unwrap()
            .iter()
            .all(|cached| cached.event.event_id != delayed.event_id)
    );

    let preceding = MlsChattEvent {
        version: MLS_PROTOCOL_VERSION,
        room_id: descriptor.room_id,
        event_id: EventId([12; 16]),
        sender_account: bob.roster.body.account_id,
        timestamp_ms: 203,
        content: ChattEventContent::Text {
            body: "the missing predecessor".to_string(),
        },
    };
    bob_client.queue_outgoing(preceding.clone()).unwrap();
    let (preceding_epoch, preceding_ciphertext) = bob_client
        .encrypt_outgoing(&descriptor, preceding.event_id)
        .unwrap();
    assert!(matches!(
        alice_client
            .process_delivery(
                &descriptor,
                &MlsDeliveryEvent::Application {
                    sequence: 3,
                    epoch: preceding_epoch,
                    event_id: preceding.event_id,
                    ciphertext: preceding_ciphertext,
                },
            )
            .unwrap(),
        ProcessedDelivery::Application(ref cached) if cached.event == preceding
    ));
    assert!(matches!(
        alice_client
            .process_delivery(
                &descriptor,
                &MlsDeliveryEvent::Application {
                    sequence: 4,
                    epoch: delayed_epoch,
                    event_id: delayed.event_id,
                    ciphertext: delayed_ciphertext,
                },
            )
            .unwrap(),
        ProcessedDelivery::Outgoing {
            sequence: 4,
            event: Some(ref emitted)
        } if emitted == &delayed
    ));
    assert!(
        alice_client
            .cached_history(descriptor.room_id)
            .unwrap()
            .iter()
            .any(|cached| cached.event == delayed)
    );

    let first_same_timestamp = MlsChattEvent {
        event_id: EventId([250; 16]),
        timestamp_ms: 204,
        content: ChattEventContent::Text {
            body: "first same-millisecond command".to_string(),
        },
        ..event.clone()
    };
    let second_same_timestamp = MlsChattEvent {
        event_id: EventId([1; 16]),
        content: ChattEventContent::Text {
            body: "second same-millisecond command".to_string(),
        },
        ..first_same_timestamp.clone()
    };
    alice_client
        .queue_outgoing(first_same_timestamp.clone())
        .unwrap();
    alice_client
        .queue_outgoing(second_same_timestamp.clone())
        .unwrap();
    let pending = alice_client.pending_outbox().unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|entry| &entry.event)
            .collect::<Vec<_>>(),
        vec![&first_same_timestamp, &second_same_timestamp],
    );
}

#[test]
fn accepted_local_commit_waits_for_earlier_same_epoch_applications() {
    let temp = tempfile::tempdir().unwrap();
    let server = b"ordered local commit test server";
    let alice = device(server, 1, 1, 1);
    let bob = device(server, 2, 2, 2);
    let identities = ChattIdentityProvider::new(server.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(79),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();
    let alice_client = PersistentClient::open(
        &temp.path().join("alice-ordered.db"),
        [41; 32],
        identities.clone(),
        alice.signing_identity,
        alice.signing_secret,
    )
    .unwrap();
    let bob_client = PersistentClient::open(
        &temp.path().join("bob-ordered.db"),
        [42; 32],
        identities.clone(),
        bob.signing_identity,
        bob.signing_secret,
    )
    .unwrap();
    let package = bob_client
        .generate_key_packages(bob.id, 1)
        .unwrap()
        .remove(0);
    let initial = alice_client
        .create_room(&descriptor, &[(bob.id, package.package)])
        .unwrap();
    alice_client.accept_pending_commit(&descriptor, 1).unwrap();
    let shared = initial.welcome.as_ref().unwrap();
    bob_client
        .join_welcome(
            &descriptor,
            &MlsWelcome {
                delivery_id: 1,
                sequence: 1,
                device_id: bob.id,
                descriptor: shared.descriptor.clone(),
                welcome: shared.welcome.clone(),
            },
        )
        .unwrap();

    let replacement = device(server, 2, 2, 3);
    let mut replacement_roster = replacement.roster.clone();
    replacement_roster.body.revision = 2;
    replacement_roster = sign_device_roster(replacement_roster.body, &[2; 32]).unwrap();
    identities.install_roster(&replacement_roster).unwrap();
    assert!(
        alice_client
            .prepare_revocation_commit(&descriptor)
            .unwrap()
            .is_none()
    );
    let replacement_client = PersistentClient::open(
        &temp.path().join("bob-replacement-ordered.db"),
        [43; 32],
        identities.clone(),
        replacement.signing_identity,
        replacement.signing_secret,
    )
    .unwrap();
    let (join_parent, join) = replacement_client
        .prepare_external_rejoin(&descriptor, &initial.group_info)
        .unwrap();
    let join_delivery = MlsDeliveryEvent::Commit {
        sequence: 2,
        parent_epoch: join_parent,
        epoch: join_parent + 1,
        commit: join.commit,
    };
    assert!(matches!(
        alice_client
            .process_delivery(&descriptor, &join_delivery)
            .unwrap(),
        ProcessedDelivery::Commit {
            sequence: 2,
            epoch: 2
        }
    ));
    assert!(matches!(
        bob_client
            .process_delivery(&descriptor, &join_delivery)
            .unwrap(),
        ProcessedDelivery::Commit {
            sequence: 2,
            epoch: 2
        }
    ));
    replacement_client
        .accept_external_rejoin(&descriptor, 2)
        .unwrap();
    let (parent_epoch, commit) = alice_client
        .prepare_revocation_commit(&descriptor)
        .unwrap()
        .expect("old Bob leaf requires removal");
    assert_eq!(parent_epoch, 2);
    assert!(alice_client.accept_pending_commit(&descriptor, 4).is_err());

    let event = MlsChattEvent {
        version: MLS_PROTOCOL_VERSION,
        room_id: descriptor.room_id,
        event_id: EventId([11; 16]),
        sender_account: bob.roster.body.account_id,
        timestamp_ms: 202,
        content: ChattEventContent::Text {
            body: "queued before the accepted commit".to_string(),
        },
    };
    bob_client.queue_outgoing(event.clone()).unwrap();
    let (epoch, ciphertext) = bob_client
        .encrypt_outgoing(&descriptor, event.event_id)
        .unwrap();
    assert!(matches!(
        alice_client
            .process_delivery(
                &descriptor,
                &MlsDeliveryEvent::Application {
                    sequence: 3,
                    epoch,
                    event_id: event.event_id,
                    ciphertext,
                },
            )
            .unwrap(),
        ProcessedDelivery::Application(ref cached) if cached.event == event
    ));
    assert!(matches!(
        alice_client
            .process_delivery(
                &descriptor,
                &MlsDeliveryEvent::Commit {
                    sequence: 4,
                    parent_epoch,
                    epoch: parent_epoch + 1,
                    commit: commit.commit,
                },
            )
            .unwrap(),
        ProcessedDelivery::Commit {
            sequence: 4,
            epoch: 3
        }
    ));
    assert_eq!(alice_client.cursor(descriptor.room_id).unwrap(), 4);
}

#[test]
fn external_rejoin_replaces_lost_local_state_and_opens_future_messages() {
    let temp = tempfile::tempdir().unwrap();
    let server = b"external rejoin test server";
    let alice = device(server, 1, 3, 3);
    let bob = device(server, 2, 4, 4);
    let identities = ChattIdentityProvider::new(server.to_vec());
    identities.install_roster(&alice.roster).unwrap();
    identities.install_roster(&bob.roster).unwrap();
    let descriptor = EncryptedRoomDescriptor::new(
        RoomId(88),
        alice.roster.body.account_id,
        vec![alice.roster.body.account_id, bob.roster.body.account_id],
        100,
    )
    .unwrap();
    identities.install_room(descriptor.clone()).unwrap();
    let alice_client = PersistentClient::open(
        &temp.path().join("alice-rejoin.db"),
        [31; 32],
        identities.clone(),
        alice.signing_identity,
        alice.signing_secret,
    )
    .unwrap();
    let bob_identity = bob.signing_identity.clone();
    let bob_secret = bob.signing_secret.clone();
    let bob_client = PersistentClient::open(
        &temp.path().join("bob-original.db"),
        [32; 32],
        identities.clone(),
        bob.signing_identity,
        bob.signing_secret,
    )
    .unwrap();
    let bob_package = bob_client
        .generate_key_packages(bob.id, 1)
        .unwrap()
        .remove(0);
    let initial = alice_client
        .create_room(&descriptor, &[(bob.id, bob_package.package)])
        .unwrap();
    alice_client.accept_pending_commit(&descriptor, 1).unwrap();
    let shared_welcome = initial.welcome.as_ref().unwrap();
    let welcome = MlsWelcome {
        delivery_id: 1,
        sequence: 1,
        device_id: bob.id,
        descriptor: shared_welcome.descriptor.clone(),
        welcome: shared_welcome.welcome.clone(),
    };
    bob_client.join_welcome(&descriptor, &welcome).unwrap();
    drop(bob_client);

    let replacement = PersistentClient::open(
        &temp.path().join("bob-reconstructed.db"),
        [33; 32],
        identities,
        bob_identity,
        bob_secret,
    )
    .unwrap();
    let (parent_epoch, rejoin) = replacement
        .prepare_external_rejoin(&descriptor, &initial.group_info)
        .unwrap();
    assert_eq!(parent_epoch, 1);
    let commit = MlsDeliveryEvent::Commit {
        sequence: 2,
        parent_epoch: 1,
        epoch: 2,
        commit: rejoin.commit.clone(),
    };
    assert!(matches!(
        alice_client.process_delivery(&descriptor, &commit).unwrap(),
        ProcessedDelivery::Commit { epoch: 2, .. }
    ));
    replacement.accept_external_rejoin(&descriptor, 2).unwrap();

    let event = MlsChattEvent {
        version: MLS_PROTOCOL_VERSION,
        room_id: descriptor.room_id,
        event_id: EventId([10; 16]),
        sender_account: alice.roster.body.account_id,
        timestamp_ms: 300,
        content: ChattEventContent::Text {
            body: "future only".to_string(),
        },
    };
    alice_client.queue_outgoing(event.clone()).unwrap();
    let (epoch, ciphertext) = alice_client
        .encrypt_outgoing(&descriptor, event.event_id)
        .unwrap();
    assert_eq!(epoch, 2);
    let received = replacement
        .process_delivery(
            &descriptor,
            &MlsDeliveryEvent::Application {
                sequence: 3,
                epoch,
                event_id: event.event_id,
                ciphertext,
            },
        )
        .unwrap();
    let ProcessedDelivery::Application(received) = received else {
        panic!("expected future application after external rejoin");
    };
    assert_eq!(received.event, event);
}
