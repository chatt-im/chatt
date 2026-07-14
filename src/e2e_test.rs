//! Reusable in-process end-to-end scenarios over real loopback sockets.
//!
//! Each [`TestDevice`] owns an independent durable data directory even when
//! several devices authenticate as the same account. The harness runs the real
//! server and client network workers, retains unmatched events for later
//! assertions, and acknowledges the same TOFU persistence handshake as the app.

use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rpc::{
    control::{DeviceLinkTicket, RoomInfo, decode_device_link_ticket},
    crypto::{dev_server_public_key, dev_server_seed_hex},
    ids::{DeviceId, MessageId, RoomId, UserId},
};
use server::{
    Server,
    config::{Config as ServerConfig, RoomPersistenceConfig, TransportModeConfig, UserConfig, hash_secret},
    local_admin::AdminCommand,
};

use crate::{
    app::{AppEvent, NetworkEventSender},
    client_net::{
        ClientConfig, FilePolicy, NetworkClient, NetworkCommand, NetworkEvent,
        UploadFileRequest, spawn_device_pair_once,
    },
    config::{
        CandidatePrivacy, DownloadTarget, E2ePeerPin, E2eTrustLevel, EffectiveFiles,
    },
    e2e_store::LocalE2eIdentity,
    receive_store::Source,
};

const ALICE: UserId = UserId(1);
const BOB: UserId = UserId(2);
const ALICE_TOKEN: &str = "e2e-alice-token";
const BOB_TOKEN: &str = "e2e-bob-token";
const WAIT: Duration = Duration::from_secs(10);

struct TestServer {
    admin: mpsc::Sender<AdminCommand>,
    worker: Option<JoinHandle<()>>,
}

impl TestServer {
    fn spawn(data_dir: PathBuf) -> (Self, String, String) {
        let mut config = ServerConfig::default();
        config.network.tcp_addr = "127.0.0.1:0".parse().unwrap();
        config.network.udp_addr = Some("127.0.0.1:0".parse().unwrap());
        config.network.udp_probe_addr = None;
        config.network.p2p_enabled = false;
        config.security.server_identity_seed = dev_server_seed_hex();
        config.security.transport_mode = TransportModeConfig::NativeEncrypted;
        config.storage.data_dir = Some(data_dir.display().to_string());
        config.rooms[0].persistence = RoomPersistenceConfig::Memory;
        config.rooms[0].memory_limit = Some(64);

        let mut server = Server::bind(config).expect("bind E2E server");
        server.seed_users(vec![
            UserConfig {
                id: ALICE,
                internal_reference: "alice".to_string(),
                username: "Alice".to_string(),
                token_hash: hash_secret(ALICE_TOKEN),
            },
            UserConfig {
                id: BOB,
                internal_reference: "bob".to_string(),
                username: "Bob".to_string(),
                token_hash: hash_secret(BOB_TOKEN),
            },
        ])
        .expect("seed E2E users");
        let tcp = server.tcp_local_addr().unwrap().to_string();
        let udp = server.udp_local_addr().unwrap().to_string();
        let (admin, admin_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("chatt-e2e-server".to_string())
            .spawn(move || {
                server.run(&admin_rx).expect("E2E server event loop");
            })
            .expect("spawn E2E server");
        (
            Self {
                admin,
                worker: Some(worker),
            },
            tcp,
            udp,
        )
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.admin.send(AdminCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("join E2E server");
        }
    }
}

struct TestWorld {
    root: tempfile::TempDir,
    _server: TestServer,
    tcp: String,
    udp: String,
}

impl TestWorld {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("E2E temp dir");
        let (server, tcp, udp) = TestServer::spawn(root.path().join("server"));
        Self {
            root,
            _server: server,
            tcp,
            udp,
        }
    }

    fn device(&self, label: &str, user: UserId, token: &str) -> TestDevice {
        TestDevice::new(
            label,
            self.root.path().join("devices").join(label),
            &self.tcp,
            &self.udp,
            user,
            token,
        )
    }
}

struct TestDevice {
    label: String,
    user: UserId,
    config: ClientConfig,
    client: Option<NetworkClient>,
    events: Option<mpsc::Receiver<AppEvent>>,
    backlog: VecDeque<NetworkEvent>,
}

impl TestDevice {
    fn new(
        label: &str,
        data_dir: PathBuf,
        tcp: &str,
        udp: &str,
        user: UserId,
        token: &str,
    ) -> Self {
        let username = match user {
            ALICE => "Alice",
            BOB => "Bob",
            _ => panic!("unknown E2E test user"),
        };
        Self {
            label: label.to_string(),
            user,
            config: ClientConfig {
                tcp_addr: tcp.to_string(),
                udp_addr: udp.to_string(),
                udp_probe_addr: None,
                username: username.to_string(),
                token: token.to_string(),
                server_public_key: None,
                data_dir: Some(data_dir),
                e2e_peer_pins: Vec::new(),
                require_native_encryption: true,
                file_policy: FilePolicy {
                    default: EffectiveFiles::default(),
                    rooms: Vec::new(),
                },
                download_store: crate::receive_store::DownloadStore::new(1024 * 1024),
                max_upload_bytes: 1024 * 1024,
                upload_rate_bytes: 0,
                p2p_enabled: false,
                candidate_privacy: CandidatePrivacy::Disabled,
                prefer_ipv6: false,
            },
            client: None,
            events: None,
            backlog: VecDeque::new(),
        }
    }

    fn connect(&mut self) {
        assert!(self.client.is_none(), "{} is already connected", self.label);
        let (tx, rx) = mpsc::channel();
        self.client = Some(
            NetworkClient::spawn(self.config.clone(), NetworkEventSender::for_test(tx))
                .unwrap_or_else(|error| panic!("{}: spawn client: {error}", self.label)),
        );
        self.events = Some(rx);
        self.backlog.clear();
    }

    fn connect_ready(&mut self) -> DeviceId {
        self.connect();
        let authenticated = self.wait_for("authentication", |event| {
            matches!(event, NetworkEvent::Authenticated { .. })
        });
        let NetworkEvent::Authenticated { user_id, .. } = authenticated else {
            unreachable!()
        };
        assert_eq!(user_id, self.user, "{} authenticated as wrong user", self.label);
        let bound = self.wait_for("device binding", |event| {
            matches!(event, NetworkEvent::E2eDeviceBound { .. })
        });
        let NetworkEvent::E2eDeviceBound { device_id } = bound else {
            unreachable!()
        };
        device_id
    }

    fn stop(&mut self) {
        if let Some(client) = self.client.take() {
            client.stop();
        }
        self.events = None;
        self.backlog.clear();
    }

    fn send(&self, command: NetworkCommand) {
        self.client
            .as_ref()
            .unwrap_or_else(|| panic!("{} is not connected", self.label))
            .try_send(command)
            .unwrap_or_else(|_| panic!("{} network worker stopped", self.label));
    }

    fn wait_for(
        &mut self,
        description: &str,
        mut predicate: impl FnMut(&NetworkEvent) -> bool,
    ) -> NetworkEvent {
        if let Some(index) = self.backlog.iter().position(&mut predicate) {
            return self.backlog.remove(index).unwrap();
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let event = self.recv_until(deadline);
            match &event {
                NetworkEvent::Error(message) => {
                    panic!("{}: error while waiting for {description}: {message}", self.label)
                }
                NetworkEvent::AuthFailed { code, message } => panic!(
                    "{}: authentication failed while waiting for {description} ({code}): {message}",
                    self.label
                ),
                NetworkEvent::WorkerStopped { reason } => panic!(
                    "{}: worker stopped while waiting for {description}: {reason}",
                    self.label
                ),
                _ => {}
            }
            if predicate(&event) {
                return event;
            }
            self.backlog.push_back(event);
        }
    }

    fn wait_for_auth_failure(&mut self) -> (u16, String) {
        let deadline = Instant::now() + WAIT;
        loop {
            let event = self.recv_until(deadline);
            match event {
                NetworkEvent::AuthFailed { code, message } => return (code, message),
                NetworkEvent::Error(message) => {
                    panic!("{}: error before authentication rejection: {message}", self.label)
                }
                NetworkEvent::WorkerStopped { reason } => {
                    panic!("{}: worker stopped before authentication rejection: {reason}", self.label)
                }
                event => self.backlog.push_back(event),
            }
        }
    }

    fn recv_until(&mut self, deadline: Instant) -> NetworkEvent {
        let remaining = deadline.checked_duration_since(Instant::now()).unwrap_or_else(|| {
            panic!("{}: timed out waiting for network event", self.label)
        });
        let event = match self
            .events
            .as_ref()
            .expect("connected client event receiver")
            .recv_timeout(remaining)
        {
            Ok(AppEvent::Network(event)) => event,
            Ok(_) => return self.recv_until(deadline),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("{}: timed out waiting for network event", self.label)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("{}: network event channel disconnected", self.label)
            }
        };
        if let NetworkEvent::E2ePeerPinProposed {
            pin,
            manual_verification,
        } = &event
        {
            self.persist_and_confirm_pin(pin.clone(), *manual_verification);
        }
        event
    }

    fn persist_and_confirm_pin(&mut self, pin: E2ePeerPin, manual_verification: bool) {
        if let Some(existing) = self
            .config
            .e2e_peer_pins
            .iter_mut()
            .find(|existing| existing.user_id == pin.user_id)
        {
            *existing = pin.clone();
        } else {
            self.config.e2e_peer_pins.push(pin.clone());
        }
        self.send(NetworkCommand::ConfirmE2ePeerPin {
            pin,
            persisted: true,
            manual_verification,
        });
    }

    fn wait_peer_ready(&mut self, peer: UserId) {
        self.wait_for("peer identity", |event| {
            matches!(
                event,
                NetworkEvent::E2ePeerPinMatched { identity }
                    if identity.user_id == peer
            )
        });
    }

    fn wait_chat(&mut self, body: &str) -> MessageId {
        let event = self.wait_for("chat message", |event| {
            matches!(event, NetworkEvent::Chat(chat) if chat.message.body == body)
        });
        let NetworkEvent::Chat(chat) = event else {
            unreachable!()
        };
        chat.message.message_id
    }

    fn receive_files_in_memory(&mut self) {
        self.config.file_policy.default = EffectiveFiles {
            target: DownloadTarget::Memory,
            max_download_bytes: 1024 * 1024,
        };
    }

    fn upload(&self, room_id: RoomId, path: PathBuf) {
        self.send(NetworkCommand::UploadFile {
            room_id: Some(room_id),
            request: UploadFileRequest::new(path),
        });
    }

    fn wait_file(&mut self, name: &str, expected: &[u8]) {
        let event = self.wait_for("received file", |event| {
            matches!(
                event,
                NetworkEvent::FileReceived { metadata, .. }
                    if metadata.original_name == name
            )
        });
        let NetworkEvent::FileReceived { served_name, .. } = event else {
            unreachable!()
        };
        let Some(Source::Memory { bytes, .. }) = self.config.download_store.resolve(&served_name)
        else {
            panic!("{}: received file is absent from memory", self.label)
        };
        assert_eq!(bytes.as_slice(), expected, "{}: file contents", self.label);
    }

    fn fetch_history(&mut self, room_id: RoomId) -> Vec<String> {
        self.send(NetworkCommand::FetchHistory {
            room_id,
            before: None,
            limit: 64,
        });
        let mut bodies = Vec::new();
        loop {
            let event = self.wait_for("history chunk", |event| {
                matches!(event, NetworkEvent::HistoryChunk { room_id: id, .. } if *id == room_id)
            });
            let NetworkEvent::HistoryChunk {
                messages, complete, ..
            } = event
            else {
                unreachable!()
            };
            bodies.extend(messages.into_iter().map(|chat| chat.message.body));
            if complete {
                return bodies;
            }
        }
    }
}

impl Drop for TestDevice {
    fn drop(&mut self) {
        self.stop();
    }
}

fn pair_device(primary: &mut TestDevice, linked: &mut TestDevice) -> DeviceId {
    primary.send(NetworkCommand::CreateDeviceLink);
    let created = primary.wait_for("device link", |event| {
        matches!(event, NetworkEvent::DeviceLinkCreated { .. })
    });
    let NetworkEvent::DeviceLinkCreated {
        pairing_string,
        transfer_password,
        ..
    } = created
    else {
        unreachable!()
    };
    let ticket: DeviceLinkTicket = decode_device_link_ticket(&pairing_string).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = spawn_device_pair_once(
        linked.config.clone(),
        ticket,
        transfer_password,
        linked.label.clone(),
        false,
        NetworkEventSender::for_test(tx),
    );
    let paired = match rx.recv_timeout(WAIT) {
        Ok(AppEvent::Network(NetworkEvent::DevicePairingSucceeded {
            token,
            username,
            udp_addr,
            udp_probe_addr,
            server_public_key,
        })) => (token, username, udp_addr, udp_probe_addr, server_public_key),
        Ok(AppEvent::Network(event)) => {
            panic!("{}: unexpected device-pairing event: {event:?}", linked.label)
        }
        Ok(_) => panic!("{}: unexpected app event during pairing", linked.label),
        Err(error) => panic!("{}: device pairing did not finish: {error}", linked.label),
    };
    worker.join().expect("join device-pair worker");
    linked.config.token = paired.0;
    linked.config.username = paired.1;
    linked.config.udp_addr = paired.2;
    linked.config.udp_probe_addr = paired.3;
    linked.config.server_public_key = Some(paired.4);

    let redeemed = primary.wait_for("device-link redemption", |event| {
        matches!(event, NetworkEvent::DeviceLinkRedeemed { .. })
    });
    let NetworkEvent::DeviceLinkRedeemed { device_id, .. } = redeemed else {
        unreachable!()
    };
    primary.wait_for("primary rebind after linking", |event| {
        matches!(event, NetworkEvent::E2eDeviceBound { .. })
    });
    device_id
}

fn open_dm(alice: &mut TestDevice, bob: &mut TestDevice) -> RoomId {
    alice.send(NetworkCommand::OpenDm(BOB));
    let opened = alice.wait_for("DM open", |event| {
        matches!(event, NetworkEvent::DmOpened { peer, .. } if *peer == BOB)
    });
    let NetworkEvent::DmOpened { room_id, .. } = opened else {
        unreachable!()
    };
    bob.wait_for("DM room", |event| {
        matches!(event, NetworkEvent::RoomUpserted(RoomInfo { room_id: id, .. }) if *id == room_id)
    });
    alice.wait_peer_ready(BOB);
    bob.wait_peer_ready(ALICE);
    room_id
}

fn send_chat(sender: &TestDevice, room_id: RoomId, body: &str) {
    sender.send(NetworkCommand::SendChat {
        room_id,
        body: body.to_string(),
    });
}

#[test]
fn unreadable_local_identity_stops_without_reconnecting_or_replacing_it() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-unreadable-identity", ALICE, ALICE_TOKEN);
    let data_dir = alice.config.data_dir.as_deref().unwrap();
    let identity_path =
        LocalE2eIdentity::linked_device_path(data_dir, &dev_server_public_key(), ALICE);
    std::fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
    let original = b"not a valid identity file";
    std::fs::write(&identity_path, original).unwrap();

    alice.connect();
    let event = alice.wait_for("fatal local identity error", |event| {
        matches!(event, NetworkEvent::LocalIdentityUnavailable { .. })
    });
    let NetworkEvent::LocalIdentityUnavailable { message } = event else {
        unreachable!()
    };
    assert!(message.contains(&identity_path.display().to_string()));
    assert!(message.contains("stopped reconnecting"));
    assert!(message.contains("left the file unchanged"));
    assert_eq!(std::fs::read(&identity_path).unwrap(), original);
    assert!(!alice
        .backlog
        .iter()
        .any(|event| matches!(event, NetworkEvent::ReconnectScheduled { .. })));
    match alice
        .events
        .as_ref()
        .unwrap()
        .recv_timeout(Duration::from_millis(200))
    {
        Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {}
        Ok(AppEvent::Network(NetworkEvent::ReconnectScheduled { .. })) => {
            panic!("unreadable identity scheduled a reconnect")
        }
        Ok(_) => panic!("unexpected event after fatal identity failure"),
    }
}

#[test]
fn sealed_dm_files_reach_every_device_after_pairing() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-file-primary", ALICE, ALICE_TOKEN);
    let mut alice_second = world.device("alice-file-secondary", ALICE, "");
    let mut bob = world.device("bob-file", BOB, BOB_TOKEN);
    alice.receive_files_in_memory();
    alice_second.receive_files_in_memory();
    bob.receive_files_in_memory();
    alice.connect_ready();
    bob.connect_ready();
    let room_id = open_dm(&mut alice, &mut bob);

    pair_device(&mut alice, &mut alice_second);
    bob.wait_peer_ready(ALICE);
    alice_second.connect_ready();
    alice_second.wait_peer_ready(BOB);

    let from_bob = b"sealed file from bob";
    let bob_path = world.root.path().join("from-bob.txt");
    std::fs::write(&bob_path, from_bob).unwrap();
    bob.upload(room_id, bob_path);
    alice.wait_file("from-bob.txt", from_bob);
    alice_second.wait_file("from-bob.txt", from_bob);

    let from_primary = b"sealed file from alice's primary device";
    let primary_path = world.root.path().join("from-primary.txt");
    std::fs::write(&primary_path, from_primary).unwrap();
    alice.upload(room_id, primary_path);
    alice_second.wait_file("from-primary.txt", from_primary);
    bob.wait_file("from-primary.txt", from_primary);

    let from_linked = b"sealed file from alice's linked device";
    let linked_path = world.root.path().join("from-linked.txt");
    std::fs::write(&linked_path, from_linked).unwrap();
    alice_second.upload(room_id, linked_path);
    alice.wait_file("from-linked.txt", from_linked);
    bob.wait_file("from-linked.txt", from_linked);
}

#[test]
fn manual_verification_syncs_to_an_existing_linked_device() {
    let world = TestWorld::new();
    let mut alice_primary = world.device("alice-sync-primary", ALICE, ALICE_TOKEN);
    let mut alice_linked = world.device("alice-sync-linked", ALICE, ALICE_TOKEN);
    let mut bob = world.device("bob-sync", BOB, BOB_TOKEN);
    alice_primary.connect_ready();
    bob.connect_ready();
    open_dm(&mut alice_primary, &mut bob);
    pair_device(&mut alice_primary, &mut alice_linked);
    alice_linked.connect_ready();
    alice_linked.wait_peer_ready(BOB);

    alice_primary.send(NetworkCommand::ReviewPeerIdentity { user_id: BOB });
    let review = alice_primary.wait_for("identity review", |event| {
        matches!(event, NetworkEvent::E2ePeerPinMatched { identity } if identity.user_id == BOB)
    });
    let NetworkEvent::E2ePeerPinMatched { identity: review } = review else {
        unreachable!()
    };
    alice_primary.send(NetworkCommand::VerifyPeerIdentity { expected: review });
    let verified = alice_primary.wait_for("local verification", |event| {
        matches!(
            event,
            NetworkEvent::E2ePeerPinMatched { identity }
                if identity.user_id == BOB
                    && identity.trust_level == E2eTrustLevel::Verified
                    && !identity.synced_verification_notice
        )
    });
    alice_linked.wait_for("synced verification", |event| {
        matches!(
            event,
            NetworkEvent::E2ePeerPinMatched { identity }
                if identity.user_id == BOB
                    && identity.trust_level == E2eTrustLevel::Verified
                    && identity.synced_verification_notice
        )
    });
    let NetworkEvent::E2ePeerPinMatched {
        identity: verified,
    } = verified
    else {
        unreachable!()
    };
    alice_primary.send(NetworkCommand::ForgetPeerIdentity { expected: verified });
    alice_primary.wait_for("local verification removal", |event| {
        matches!(
            event,
            NetworkEvent::E2ePeerPinMatched { identity }
                if identity.user_id == BOB
                    && identity.trust_level == E2eTrustLevel::Accepted
        )
    });
    alice_linked.wait_for("synced verification removal", |event| {
        matches!(
            event,
            NetworkEvent::E2ePeerPinMatched { identity }
                if identity.user_id == BOB
                    && identity.trust_level == E2eTrustLevel::Accepted
                    && !identity.synced_verification_notice
        )
    });
}

#[test]
fn linked_device_rebuilds_live_dm_messages_from_history_after_restart() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-primary", ALICE, ALICE_TOKEN);
    let mut bob = world.device("bob", BOB, BOB_TOKEN);
    let mut alice_second = world.device("alice-secondary", ALICE, "");
    alice.connect_ready();
    bob.connect_ready();
    let room_id = open_dm(&mut alice, &mut bob);

    pair_device(&mut alice, &mut alice_second);
    bob.wait_peer_ready(ALICE);
    alice_second.connect_ready();
    alice_second.wait_peer_ready(BOB);

    send_chat(&bob, room_id, "from bob after pairing");
    alice_second.wait_chat("from bob after pairing");
    send_chat(&alice, room_id, "from alice after pairing");
    alice_second.wait_chat("from alice after pairing");

    alice_second.stop();
    alice_second.connect_ready();
    alice_second.wait_peer_ready(BOB);
    let bodies = alice_second.fetch_history(room_id);
    assert!(bodies.iter().any(|body| body == "from bob after pairing"));
    assert!(bodies.iter().any(|body| body == "from alice after pairing"));
}

#[test]
fn linked_device_opens_messages_sent_while_it_was_offline() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-primary", ALICE, ALICE_TOKEN);
    let mut bob = world.device("bob", BOB, BOB_TOKEN);
    let mut alice_second = world.device("alice-secondary", ALICE, "");
    alice.connect_ready();
    bob.connect_ready();
    let room_id = open_dm(&mut alice, &mut bob);

    pair_device(&mut alice, &mut alice_second);
    bob.wait_peer_ready(ALICE);
    send_chat(&bob, room_id, "peer message while secondary offline");
    bob.wait_chat("peer message while secondary offline");
    send_chat(&alice, room_id, "own message while secondary offline");
    alice.wait_chat("own message while secondary offline");

    alice_second.connect_ready();
    alice_second.wait_peer_ready(BOB);
    let bodies = alice_second.fetch_history(room_id);
    assert!(
        bodies
            .iter()
            .any(|body| body == "peer message while secondary offline")
    );
    assert!(
        bodies
            .iter()
            .any(|body| body == "own message while secondary offline")
    );
}

#[test]
fn revoked_linked_device_is_disconnected_and_remaining_devices_keep_messaging() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-primary", ALICE, ALICE_TOKEN);
    let mut bob = world.device("bob", BOB, BOB_TOKEN);
    let mut alice_second = world.device("alice-secondary", ALICE, "");
    alice.connect_ready();
    bob.connect_ready();
    let room_id = open_dm(&mut alice, &mut bob);

    let second_id = pair_device(&mut alice, &mut alice_second);
    bob.wait_peer_ready(ALICE);
    alice_second.connect_ready();
    alice_second.wait_peer_ready(BOB);

    alice.send(NetworkCommand::RevokeE2eDevice {
        device_id: second_id,
    });
    let (code, rejection) = alice_second.wait_for_auth_failure();
    assert_eq!(code, 401);
    assert!(
        rejection.contains("not valid")
            || rejection.contains("credential")
            || rejection.contains("revoked"),
        "unexpected revoked-device rejection: {rejection}"
    );
    alice.wait_for("primary rebind after revocation", |event| {
        matches!(event, NetworkEvent::E2eDeviceBound { .. })
    });
    bob.wait_peer_ready(ALICE);

    send_chat(&alice, room_id, "from remaining alice device");
    bob.wait_chat("from remaining alice device");
    send_chat(&bob, room_id, "to remaining alice device");
    alice.wait_chat("to remaining alice device");
}
