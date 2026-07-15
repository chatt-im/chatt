//! Reusable in-process end-to-end scenarios over real loopback sockets.
//!
//! Each [`TestDevice`] owns an independent durable data directory even when
//! several devices authenticate as the same account. The harness runs the real
//! server and client network workers, retains unmatched events for later
//! assertions, and acknowledges the same TOFU persistence handshake as the app.

use std::{
    collections::VecDeque,
    net::{TcpListener, UdpSocket},
    path::PathBuf,
    sync::{Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rpc::{
    control::{DeviceLinkTicket, RoomInfo, decode_device_link_ticket},
    crypto::{dev_server_public_key, dev_server_seed_hex},
    ids::{DeviceId, MessageId, RoomId, StreamId, UserId},
};
use server::{
    Server,
    config::{Config as ServerConfig, RoomPersistenceConfig, TransportModeConfig, UserConfig, hash_secret},
    local_admin::AdminCommand,
};

use crate::{
    app::{AppEvent, NetworkEventSender},
    audio::{LocalVoiceFrame, VoicePayload},
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
        let tcp_reservation = TcpListener::bind("127.0.0.1:0").expect("reserve E2E TCP port");
        let tcp_addr = tcp_reservation.local_addr().unwrap();
        let udp_reservation = UdpSocket::bind("127.0.0.1:0").expect("reserve E2E UDP port");
        let udp_addr = udp_reservation.local_addr().unwrap();
        let mut config = ServerConfig::default();
        config.network.tcp_addr = tcp_addr;
        config.network.udp_addr = Some(udp_addr);
        config.network.udp_probe_addr = None;
        config.network.public_tcp_addr = tcp_addr.to_string();
        config.network.public_udp_addr = udp_addr.to_string();
        config.network.p2p_enabled = false;
        config.security.server_identity_seed = dev_server_seed_hex();
        config.security.transport_mode = TransportModeConfig::NativeEncrypted;
        config.storage.data_dir = Some(data_dir.display().to_string());
        config.rooms[0].persistence = RoomPersistenceConfig::Memory;
        config.rooms[0].memory_limit = Some(64);

        drop((tcp_reservation, udp_reservation));
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
        self.config.e2e_peer_pins.retain(|existing| {
            existing.room_id != pin.room_id && existing.user_id != pin.user_id
        });
        self.config.e2e_peer_pins.push(pin.clone());
        self.send(NetworkCommand::ConfirmE2ePeerPin {
            pin,
            persisted: true,
            manual_verification,
        });
    }

    fn wait_peer_identity(&mut self, peer: UserId) -> crate::e2e::AcceptedPeerIdentity {
        let event = self.wait_for("peer identity", |event| {
            matches!(
                event,
                NetworkEvent::E2ePeerPinMatched { identity }
                    if identity.user_id == peer
            )
        });
        let NetworkEvent::E2ePeerPinMatched { identity } = event else {
            unreachable!()
        };
        identity
    }

    fn wait_peer_ready(&mut self, peer: UserId) {
        self.wait_peer_identity(peer);
    }

    fn wait_peer_refresh(&mut self, peer: UserId) -> crate::e2e::AcceptedPeerIdentity {
        self.wait_for("peer roster refresh start", |event| {
            matches!(
                event,
                NetworkEvent::E2eIdentityFetching { user_id, .. }
                    if *user_id == peer
            )
        });
        self.wait_peer_identity(peer)
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

    fn join_voice(&mut self, room_id: RoomId) -> StreamId {
        self.send(NetworkCommand::JoinVoice(room_id));
        let local_user = self.user;
        let event = self.wait_for("own voice stream", |event| {
            matches!(
                event,
                NetworkEvent::VoiceStarted {
                    room_id: id,
                    user_id,
                    ..
                } if *id == room_id && *user_id == local_user
            )
        });
        let NetworkEvent::VoiceStarted { stream_id, .. } = event else {
            unreachable!()
        };
        stream_id
    }

    fn send_voice(&self, payload: Vec<u8>) {
        // UDP is intentionally lossy. A short burst still exercises encrypted
        // media routing without making one dropped datagram fail 4,096 cases.
        for timestamp in 0..3 {
            self.send(NetworkCommand::LocalVoicePacket(LocalVoiceFrame {
                flags: 0,
                payload: VoicePayload::Opus(payload.clone()),
                timestamp,
            }));
        }
    }

    fn wait_voice(&mut self, stream_id: StreamId, payload_size: usize) {
        self.wait_for("voice packet", |event| {
            matches!(
                event,
                NetworkEvent::VoicePacketObserved {
                    stream_id: observed,
                    payload_size: size,
                } if *observed == stream_id.0 && *size == payload_size
            )
        });
    }

    fn wait_voice_for(
        &mut self,
        stream_id: StreamId,
        payload_size: usize,
        timeout: Duration,
    ) -> bool {
        let matches_packet = |event: &NetworkEvent| {
            matches!(
                event,
                NetworkEvent::VoicePacketObserved {
                    stream_id: observed,
                    payload_size: size,
                } if *observed == stream_id.0 && *size == payload_size
            )
        };
        if let Some(index) = self.backlog.iter().position(matches_packet) {
            self.backlog.remove(index);
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let event = match self
                .events
                .as_ref()
                .expect("connected client event receiver")
                .recv_timeout(remaining)
            {
                Ok(AppEvent::Network(event)) => event,
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => return false,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("{}: network event channel disconnected", self.label)
                }
            };
            match &event {
                NetworkEvent::Error(message) => {
                    panic!("{}: error while waiting for voice packet: {message}", self.label)
                }
                NetworkEvent::AuthFailed { code, message } => panic!(
                    "{}: authentication failed while waiting for voice packet ({code}): {message}",
                    self.label
                ),
                NetworkEvent::WorkerStopped { reason } => panic!(
                    "{}: worker stopped while waiting for voice packet: {reason}",
                    self.label
                ),
                _ => {}
            }
            if matches_packet(&event) {
                return true;
            }
            self.backlog.push_back(event);
        }
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

fn relay_voice(
    sender: &TestDevice,
    receiver: &mut TestDevice,
    stream_id: StreamId,
    payload: Vec<u8>,
) {
    for _ in 0..10 {
        sender.send_voice(payload.clone());
        if receiver.wait_voice_for(stream_id, payload.len(), Duration::from_millis(100)) {
            return;
        }
    }
    receiver.wait_voice(stream_id, payload.len());
}

#[test]
fn unreadable_local_identity_keeps_public_session_without_replacing_it() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-unreadable-identity", ALICE, ALICE_TOKEN);
    let data_dir = alice.config.data_dir.as_deref().unwrap();
    let identity_path =
        LocalE2eIdentity::linked_device_path(data_dir, &dev_server_public_key(), ALICE);
    std::fs::create_dir_all(identity_path.parent().unwrap()).unwrap();
    let original = b"not a valid identity file";
    std::fs::write(&identity_path, original).unwrap();

    alice.connect();
    let event = alice.wait_for("public-only local identity error", |event| {
        matches!(event, NetworkEvent::LocalIdentityUnavailable { .. })
    });
    let NetworkEvent::LocalIdentityUnavailable { message } = event else {
        unreachable!()
    };
    assert!(message.contains(&identity_path.display().to_string()));
    assert!(message.contains("Public rooms remain available"));
    assert!(message.contains("encrypted DMs and device administration are disabled"));
    assert!(message.contains("left the file unchanged"));
    assert_eq!(std::fs::read(&identity_path).unwrap(), original);
    assert!(!alice
        .backlog
        .iter()
        .any(|event| matches!(event, NetworkEvent::ReconnectScheduled { .. })));
    alice.wait_for("public-only authentication", |event| {
        matches!(event, NetworkEvent::Authenticated { .. })
    });
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
    bob.wait_peer_refresh(ALICE);
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
fn linked_device_first_dm_treats_peer_as_unverified_not_changed() {
    let world = TestWorld::new();
    let mut alice_primary = world.device("alice-first-dm-primary", ALICE, ALICE_TOKEN);
    let mut alice_linked = world.device("alice-first-dm-linked", ALICE, "");
    let mut bob = world.device("bob-first-dm", BOB, BOB_TOKEN);
    // A reset server can reuse its first numeric DM id while the client still
    // retains a pin for an unrelated contact from the previous server state.
    alice_linked.config.e2e_peer_pins.push(E2ePeerPin {
        room_id: 0x8000_0000,
        user_id: 3,
        username: "carol".to_string(),
        public_key: "11".repeat(rpc::e2e::E2E_PUBLIC_KEY_LEN),
        trust_level: E2eTrustLevel::Accepted,
        change_from: None,
        previous: Vec::new(),
    });
    bob.connect_ready();
    alice_primary.connect_ready();

    pair_device(&mut alice_primary, &mut alice_linked);
    alice_linked.connect_ready();
    alice_linked.send(NetworkCommand::OpenDm(BOB));
    alice_linked.wait_for("first DM open", |event| {
        matches!(event, NetworkEvent::DmOpened { peer, .. } if *peer == BOB)
    });
    let alice_identity = alice_linked.wait_for("Bob's first identity", |event| {
        matches!(
            event,
            NetworkEvent::E2ePeerPinMatched { identity }
                if identity.user_id == BOB
        )
    });
    let NetworkEvent::E2ePeerPinMatched {
        identity: alice_identity,
    } = alice_identity
    else {
        unreachable!()
    };
    let bob_identity = bob.wait_for("Alice's first identity", |event| {
        matches!(
            event,
            NetworkEvent::E2ePeerPinMatched { identity }
                if identity.user_id == ALICE
        )
    });
    let NetworkEvent::E2ePeerPinMatched {
        identity: bob_identity,
    } = bob_identity
    else {
        unreachable!()
    };

    for identity in [alice_identity, bob_identity] {
        assert_eq!(identity.trust_level, E2eTrustLevel::Accepted);
        assert_eq!(
            identity.change_from, None,
            "a peer first observed after device linking is unverified, not changed"
        );
    }
}

const LINKED_DEVICE_MATRIX_BITS: u32 = 16;
const LINKED_DEVICE_MATRIX_PAIRWISE_ROWS: u32 = 32;
const LINKED_DEVICE_MATRIX_RANDOM_CASES: usize = 16;
const LINKED_DEVICE_MATRIX_WORKERS: usize = 8;
const LINKED_DEVICE_MATRIX_SEED: u64 = 0x6d61_7472_6978_2d31;

#[derive(Clone, Copy, Debug)]
struct LinkedDeviceMatrixCase {
    bits: u32,
    bob_connects_first: bool,
    dm_before_link: bool,
    linked_offline_for_first_message: bool,
    unrelated_reused_room_pin: bool,
    restart_primary_before_text: bool,
    restart_linked_before_text: bool,
    restart_bob_before_text: bool,
    alice_text_from_linked: bool,
    alice_media_from_linked: bool,
    restart_primary_between_media: bool,
    restart_linked_between_media: bool,
    restart_bob_between_media: bool,
    pre_link_message_from_alice: bool,
    pre_link_message_from_bob: bool,
    bob_offline_during_link: bool,
    cross_text_sends_before_draining: bool,
}

impl LinkedDeviceMatrixCase {
    fn from_bits(bits: u32) -> Self {
        let bit = |index| bits & (1u32 << index) != 0u32;
        Self {
            bits,
            bob_connects_first: bit(0),
            dm_before_link: bit(1),
            linked_offline_for_first_message: bit(2),
            unrelated_reused_room_pin: bit(3),
            restart_primary_before_text: bit(4),
            restart_linked_before_text: bit(5),
            restart_bob_before_text: bit(6),
            alice_text_from_linked: bit(7),
            alice_media_from_linked: bit(8),
            restart_primary_between_media: bit(9),
            restart_linked_between_media: bit(10),
            restart_bob_between_media: bit(11),
            pre_link_message_from_alice: bit(12),
            pre_link_message_from_bob: bit(13),
            bob_offline_during_link: bit(14),
            cross_text_sends_before_draining: bit(15),
        }
    }

    fn has_pre_link_messages(self) -> bool {
        self.pre_link_message_from_alice || self.pre_link_message_from_bob
    }
}

#[derive(Clone, Copy)]
struct MatrixRng(u64);

impl MatrixRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, self.next_u64() as usize % (index + 1));
        }
    }
}

fn linked_device_matrix_seed() -> u64 {
    let Ok(value) = std::env::var("CHATT_E2E_MATRIX_SEED") else {
        return LINKED_DEVICE_MATRIX_SEED;
    };
    let parsed = value
        .strip_prefix("0x")
        .map(|hex| u64::from_str_radix(hex, 16))
        .unwrap_or_else(|| value.parse());
    parsed.unwrap_or_else(|error| panic!("invalid CHATT_E2E_MATRIX_SEED {value:?}: {error}"))
}

fn linked_device_matrix_cases(seed: u64) -> Vec<LinkedDeviceMatrixCase> {
    if let Ok(value) = std::env::var("CHATT_E2E_MATRIX_CASE") {
        let parsed = value
            .strip_prefix("0x")
            .map(|hex| u32::from_str_radix(hex, 16))
            .unwrap_or_else(|| value.parse());
        let bits = parsed
            .unwrap_or_else(|error| panic!("invalid CHATT_E2E_MATRIX_CASE {value:?}: {error}"));
        assert!(bits < 1 << LINKED_DEVICE_MATRIX_BITS);
        return vec![LinkedDeviceMatrixCase::from_bits(bits)];
    }
    assert!(LINKED_DEVICE_MATRIX_BITS < LINKED_DEVICE_MATRIX_PAIRWISE_ROWS);
    let mut rng = MatrixRng::new(seed);
    let mut columns: Vec<u32> = (1..LINKED_DEVICE_MATRIX_PAIRWISE_ROWS).collect();
    rng.shuffle(&mut columns);
    let complements = rng.next_u64() as u32;
    let mut encoded = Vec::with_capacity(
        LINKED_DEVICE_MATRIX_PAIRWISE_ROWS as usize + LINKED_DEVICE_MATRIX_RANDOM_CASES,
    );

    // Distinct non-zero linear forms over five input bits form an orthogonal
    // array: every pair of flags sees 00, 01, 10, and 11 equally often. The
    // shuffled columns and complements change higher-order interactions per
    // seed without weakening that pairwise guarantee.
    for row in 0..LINKED_DEVICE_MATRIX_PAIRWISE_ROWS {
        let mut bits = 0u32;
        for factor in 0..LINKED_DEVICE_MATRIX_BITS as usize {
            let enabled = ((row & columns[factor]).count_ones() % 2 != 0)
                ^ (complements & (1 << factor) != 0);
            bits |= u32::from(enabled) << factor;
        }
        encoded.push(bits);
    }

    let mask = (1u32 << LINKED_DEVICE_MATRIX_BITS) - 1;
    while encoded.len()
        < LINKED_DEVICE_MATRIX_PAIRWISE_ROWS as usize + LINKED_DEVICE_MATRIX_RANDOM_CASES
    {
        let bits = rng.next_u64() as u32 & mask;
        if !encoded.contains(&bits) {
            encoded.push(bits);
        }
    }
    rng.shuffle(&mut encoded);
    encoded
        .into_iter()
        .map(LinkedDeviceMatrixCase::from_bits)
        .collect()
}

fn assert_linked_device_matrix_is_pairwise(cases: &[LinkedDeviceMatrixCase]) {
    for first in 0..LINKED_DEVICE_MATRIX_BITS {
        for second in first + 1..LINKED_DEVICE_MATRIX_BITS {
            let mut combinations = [false; 4];
            for case in cases {
                let first_enabled = case.bits & (1 << first) != 0;
                let second_enabled = case.bits & (1 << second) != 0;
                combinations[usize::from(first_enabled) * 2 + usize::from(second_enabled)] = true;
            }
            assert!(
                combinations.into_iter().all(|covered| covered),
                "matrix generator missed a combination for factors {first} and {second}"
            );
        }
    }
}

fn assert_standard_unverified(
    case: LinkedDeviceMatrixCase,
    device: &str,
    identity: &crate::e2e::AcceptedPeerIdentity,
) {
    assert_eq!(
        identity.trust_level,
        E2eTrustLevel::Accepted,
        "matrix case {case:?}: {device} unexpectedly verified its first-contact peer"
    );
    assert_eq!(
        identity.change_from, None,
        "matrix case {case:?}: {device} treated a first-contact peer as an identity change"
    );
}

fn restart_with_peer(
    case: LinkedDeviceMatrixCase,
    device: &mut TestDevice,
    peer: UserId,
) {
    device.stop();
    device.connect_ready();
    let label = device.label.clone();
    let identity = device.wait_peer_identity(peer);
    assert_standard_unverified(case, &label, &identity);
}

fn run_linked_device_matrix_case(case: LinkedDeviceMatrixCase) {
    let world = TestWorld::new();
    let suffix = format!("{:04x}", case.bits);
    let mut alice_primary = world.device(
        &format!("alice-matrix-primary-{suffix}"),
        ALICE,
        ALICE_TOKEN,
    );
    let mut alice_linked = world.device(&format!("alice-matrix-linked-{suffix}"), ALICE, "");
    let mut bob = world.device(&format!("bob-matrix-{suffix}"), BOB, BOB_TOKEN);
    alice_primary.receive_files_in_memory();
    alice_linked.receive_files_in_memory();
    bob.receive_files_in_memory();
    if case.unrelated_reused_room_pin {
        alice_linked.config.e2e_peer_pins.push(E2ePeerPin {
            room_id: 0x8000_0000,
            user_id: 3,
            username: "carol".to_string(),
            public_key: "11".repeat(rpc::e2e::E2E_PUBLIC_KEY_LEN),
            trust_level: E2eTrustLevel::Accepted,
            change_from: None,
            previous: Vec::new(),
        });
    }

    if case.bob_connects_first {
        bob.connect_ready();
        alice_primary.connect_ready();
    } else {
        alice_primary.connect_ready();
        bob.connect_ready();
    }

    let dm_before_link = case.dm_before_link || case.has_pre_link_messages();
    let room_id = if dm_before_link {
        open_dm(&mut alice_primary, &mut bob)
    } else {
        RoomId(0)
    };

    let mut pre_link_bodies = Vec::new();
    if case.pre_link_message_from_alice {
        pre_link_bodies.push(format!("matrix pre-link Alice {}", case.bits));
    }
    if case.pre_link_message_from_bob {
        pre_link_bodies.push(format!("matrix pre-link Bob {}", case.bits));
    }
    for (index, body) in pre_link_bodies.iter().enumerate() {
        let sender = if case.pre_link_message_from_alice && index == 0 {
            &alice_primary
        } else {
            &bob
        };
        send_chat(sender, room_id, body);
    }
    for body in &pre_link_bodies {
        alice_primary.wait_chat(body);
        bob.wait_chat(body);
    }

    if case.bob_offline_during_link {
        bob.stop();
    }
    pair_device(&mut alice_primary, &mut alice_linked);

    let room_id = if dm_before_link {
        let bob_identity = if case.bob_offline_during_link {
            bob.connect_ready();
            bob.wait_peer_identity(ALICE)
        } else {
            bob.wait_peer_refresh(ALICE)
        };
        assert_standard_unverified(case, "bob after Alice linked", &bob_identity);
        room_id
    } else {
        alice_linked.connect_ready();
        alice_linked.send(NetworkCommand::OpenDm(BOB));
        let opened = alice_linked.wait_for("matrix DM open", |event| {
            matches!(event, NetworkEvent::DmOpened { peer, .. } if *peer == BOB)
        });
        let NetworkEvent::DmOpened { room_id, .. } = opened else {
            unreachable!()
        };
        if case.bob_offline_during_link {
            bob.connect_ready();
        }
        alice_primary.wait_for("matrix DM room", |event| {
            matches!(
                event,
                NetworkEvent::RoomUpserted(RoomInfo { room_id: id, .. })
                    if *id == room_id
            )
        });
        if !case.bob_offline_during_link {
            bob.wait_for("matrix DM room", |event| {
                matches!(
                    event,
                    NetworkEvent::RoomUpserted(RoomInfo { room_id: id, .. })
                        if *id == room_id
                )
            });
        }
        let linked_identity = alice_linked.wait_peer_identity(BOB);
        let primary_identity = alice_primary.wait_peer_identity(BOB);
        let bob_identity = bob.wait_peer_identity(ALICE);
        assert_standard_unverified(case, "Alice linked", &linked_identity);
        assert_standard_unverified(case, "Alice primary", &primary_identity);
        assert_standard_unverified(case, "bob", &bob_identity);
        room_id
    };

    let bootstrap = format!("matrix bootstrap {}", case.bits);
    if dm_before_link && case.linked_offline_for_first_message {
        send_chat(&bob, room_id, &bootstrap);
        bob.wait_chat(&bootstrap);
        alice_primary.wait_chat(&bootstrap);
        alice_linked.connect_ready();
        let identity = alice_linked.wait_peer_identity(BOB);
        assert_standard_unverified(case, "Alice linked after offline bootstrap", &identity);
        let history = alice_linked.fetch_history(room_id);
        assert!(
            history.iter().any(|body| body == &bootstrap),
            "matrix case {case:?}: linked device missed pre-connect history"
        );
    } else {
        if dm_before_link {
            alice_linked.connect_ready();
            let identity = alice_linked.wait_peer_identity(BOB);
            assert_standard_unverified(case, "Alice linked", &identity);
        } else if case.linked_offline_for_first_message {
            alice_linked.stop();
        }
        send_chat(&bob, room_id, &bootstrap);
        bob.wait_chat(&bootstrap);
        alice_primary.wait_chat(&bootstrap);
        if case.linked_offline_for_first_message {
            alice_linked.connect_ready();
            let identity = alice_linked.wait_peer_identity(BOB);
            assert_standard_unverified(case, "Alice linked after offline bootstrap", &identity);
            let history = alice_linked.fetch_history(room_id);
            assert!(
                history.iter().any(|body| body == &bootstrap),
                "matrix case {case:?}: linked device missed offline history"
            );
        } else {
            alice_linked.wait_chat(&bootstrap);
        }
    }

    if !pre_link_bodies.is_empty() {
        let history = alice_linked.fetch_history(room_id);
        for body in &pre_link_bodies {
            assert!(
                !history.iter().any(|candidate| candidate == body),
                "matrix case {case:?}: linked device decrypted message sent before linkage"
            );
        }
        let placeholders = history
            .iter()
            .filter(|body| {
                body.as_str()
                    == "Encrypted message unavailable on this device (sent before it was linked)."
            })
            .count();
        assert!(
            placeholders >= pre_link_bodies.len(),
            "matrix case {case:?}: expected {} pre-link placeholders, found {placeholders}",
            pre_link_bodies.len()
        );
    }

    if case.restart_primary_before_text {
        restart_with_peer(case, &mut alice_primary, BOB);
    }
    if case.restart_linked_before_text {
        restart_with_peer(case, &mut alice_linked, BOB);
    }
    if case.restart_bob_before_text {
        restart_with_peer(case, &mut bob, ALICE);
    }

    let alice_text = format!("matrix Alice text {}", case.bits);
    if case.alice_text_from_linked {
        send_chat(&alice_linked, room_id, &alice_text);
    } else {
        send_chat(&alice_primary, room_id, &alice_text);
    }
    let bob_text = format!("matrix Bob text {}", case.bits);
    if case.cross_text_sends_before_draining {
        send_chat(&bob, room_id, &bob_text);
    }
    for device in [&mut alice_primary, &mut alice_linked, &mut bob] {
        device.wait_chat(&alice_text);
    }
    if !case.cross_text_sends_before_draining {
        send_chat(&bob, room_id, &bob_text);
    }
    for device in [&mut alice_primary, &mut alice_linked, &mut bob] {
        device.wait_chat(&bob_text);
    }

    if case.restart_primary_between_media {
        restart_with_peer(case, &mut alice_primary, BOB);
    }
    if case.restart_linked_between_media {
        restart_with_peer(case, &mut alice_linked, BOB);
    }
    if case.restart_bob_between_media {
        restart_with_peer(case, &mut bob, ALICE);
    }

    let alice_file_name = format!("matrix-alice-{suffix}.bin");
    let alice_file_contents = format!("Alice matrix media {}", case.bits).into_bytes();
    let alice_file_path = world.root.path().join(&alice_file_name);
    std::fs::write(&alice_file_path, &alice_file_contents).unwrap();
    if case.alice_media_from_linked {
        alice_linked.upload(room_id, alice_file_path);
        alice_primary.wait_file(&alice_file_name, &alice_file_contents);
    } else {
        alice_primary.upload(room_id, alice_file_path);
        alice_linked.wait_file(&alice_file_name, &alice_file_contents);
    }
    bob.wait_file(&alice_file_name, &alice_file_contents);

    let bob_file_name = format!("matrix-bob-{suffix}.bin");
    let bob_file_contents = format!("Bob matrix media {}", case.bits).into_bytes();
    let bob_file_path = world.root.path().join(&bob_file_name);
    std::fs::write(&bob_file_path, &bob_file_contents).unwrap();
    bob.upload(room_id, bob_file_path);
    alice_primary.wait_file(&bob_file_name, &bob_file_contents);
    alice_linked.wait_file(&bob_file_name, &bob_file_contents);

    let alice_voice = if case.alice_media_from_linked {
        &mut alice_linked
    } else {
        &mut alice_primary
    };
    let alice_stream = alice_voice.join_voice(room_id);
    let bob_stream = bob.join_voice(room_id);
    let alice_payload = vec![0xa1, (case.bits & 0xff) as u8, 1, 2, 3];
    relay_voice(alice_voice, &mut bob, alice_stream, alice_payload);
    let bob_payload = vec![0xb0, (case.bits >> 4) as u8, 4, 5, 6, 7, 8];
    relay_voice(&bob, alice_voice, bob_stream, bob_payload);
}

fn run_linked_device_matrix_case_catching(case: LinkedDeviceMatrixCase) -> Result<(), String> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_linked_device_matrix_case(case)
    }));
    result.map_err(|payload| {
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic");
        format!(
            "linked-device E2E matrix case {:#06x} failed ({case:?}): {message}",
            case.bits
        )
    })
}

#[test]
fn linked_device_offline_history_survives_stale_pin_and_restarts() {
    run_linked_device_matrix_case(LinkedDeviceMatrixCase::from_bits(0x0c2e));
}

#[test]
fn linked_device_dm_opened_while_peer_was_offline() {
    run_linked_device_matrix_case(LinkedDeviceMatrixCase::from_bits(0xcaf9));
}

#[test]
fn linked_device_pairing_and_send_matrix() {
    let seed = linked_device_matrix_seed();
    let cases = linked_device_matrix_cases(seed);
    if cases.len() > 1 {
        assert_linked_device_matrix_is_pairwise(&cases);
    }
    let failure = Mutex::new(None);
    let workers = LINKED_DEVICE_MATRIX_WORKERS.min(cases.len());
    thread::scope(|scope| {
        for worker in 0..workers {
            let failure = &failure;
            let cases = &cases;
            scope.spawn(move || {
                for case in cases.iter().copied().skip(worker).step_by(workers) {
                    if failure.lock().unwrap().is_some() {
                        return;
                    }
                    if let Err(message) = run_linked_device_matrix_case_catching(case) {
                        *failure.lock().unwrap() = Some(message);
                        return;
                    }
                }
            });
        }
    });
    if let Some(message) = failure.into_inner().unwrap() {
        panic!("{message}; reproduce with CHATT_E2E_MATRIX_SEED={seed:#018x}");
    }
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
    bob.wait_peer_refresh(ALICE);
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
    bob.wait_peer_refresh(ALICE);
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
fn linked_device_shows_placeholder_for_messages_sent_before_linking() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-primary", ALICE, ALICE_TOKEN);
    let mut bob = world.device("bob", BOB, BOB_TOKEN);
    let mut alice_second = world.device("alice-secondary", ALICE, "");
    alice.connect_ready();
    bob.connect_ready();
    let room_id = open_dm(&mut alice, &mut bob);

    send_chat(&bob, room_id, "before the second device existed");
    bob.wait_chat("before the second device existed");
    alice.wait_chat("before the second device existed");

    pair_device(&mut alice, &mut alice_second);
    bob.wait_peer_refresh(ALICE);
    alice_second.connect_ready();
    alice_second.wait_peer_ready(BOB);
    let bodies = alice_second.fetch_history(room_id);
    assert!(bodies.iter().any(|body| {
        body == "Encrypted message unavailable on this device (sent before it was linked)."
    }));
    assert!(!bodies.iter().any(|body| body == "before the second device existed"));
}

#[derive(Clone, Copy, Debug)]
struct RevocationMatrixCase {
    bits: u8,
    dm_before_link: bool,
    victim_connected: bool,
    message_before_revocation: bool,
}

impl RevocationMatrixCase {
    fn from_bits(bits: u8) -> Self {
        Self {
            bits,
            dm_before_link: bits & 1 != 0,
            victim_connected: bits & 2 != 0,
            message_before_revocation: bits & 4 != 0,
        }
    }
}

fn run_revocation_matrix_case(case: RevocationMatrixCase) {
    let world = TestWorld::new();
    let suffix = case.bits.to_string();
    let mut alice = world.device(
        &format!("alice-revoke-primary-{suffix}"),
        ALICE,
        ALICE_TOKEN,
    );
    let mut bob = world.device(&format!("bob-revoke-{suffix}"), BOB, BOB_TOKEN);
    let mut victim = world.device(&format!("alice-revoke-victim-{suffix}"), ALICE, "");
    alice.connect_ready();
    bob.connect_ready();
    let room_id = if case.dm_before_link {
        open_dm(&mut alice, &mut bob)
    } else {
        RoomId(0)
    };

    let victim_id = pair_device(&mut alice, &mut victim);
    if case.dm_before_link {
        bob.wait_peer_refresh(ALICE);
    }
    if case.victim_connected {
        victim.connect_ready();
        if case.dm_before_link {
            victim.wait_peer_ready(BOB);
        }
    }

    let room_id = if case.dm_before_link {
        room_id
    } else {
        let room_id = open_dm(&mut alice, &mut bob);
        if case.victim_connected {
            victim.wait_for("revocation matrix DM room", |event| {
                matches!(
                    event,
                    NetworkEvent::RoomUpserted(RoomInfo { room_id: id, .. })
                        if *id == room_id
                )
            });
            victim.wait_peer_ready(BOB);
        }
        room_id
    };

    if case.message_before_revocation {
        let body = format!("before revocation {}", case.bits);
        send_chat(&bob, room_id, &body);
        alice.wait_chat(&body);
        bob.wait_chat(&body);
        if case.victim_connected {
            victim.wait_chat(&body);
        }
    }

    alice.send(NetworkCommand::RevokeE2eDevice {
        device_id: victim_id,
    });
    if !case.victim_connected {
        victim.connect();
    }
    let (code, rejection) = victim.wait_for_auth_failure();
    assert_eq!(code, 401);
    assert!(
        rejection.contains("not valid")
            || rejection.contains("credential")
            || rejection.contains("revoked"),
        "revocation matrix case {case:?}: unexpected rejection: {rejection}"
    );
    alice.wait_for("primary rebind after revocation", |event| {
        matches!(event, NetworkEvent::E2eDeviceBound { .. })
    });
    bob.wait_peer_refresh(ALICE);

    let from_alice = format!("from remaining Alice device {}", case.bits);
    let from_bob = format!("to remaining Alice device {}", case.bits);
    send_chat(&alice, room_id, &from_alice);
    send_chat(&bob, room_id, &from_bob);
    for body in [&from_alice, &from_bob] {
        alice.wait_chat(body);
        bob.wait_chat(body);
    }
}

#[test]
fn linked_device_revocation_matrix() {
    let failure = Mutex::new(None);
    thread::scope(|scope| {
        for worker in 0..4u8 {
            let failure = &failure;
            scope.spawn(move || {
                for bits in (worker..8).step_by(4) {
                    let case = RevocationMatrixCase::from_bits(bits);
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_revocation_matrix_case(case)
                    }));
                    if let Err(payload) = result {
                        let message = payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| payload.downcast_ref::<&str>().copied())
                            .unwrap_or("non-string panic");
                        *failure.lock().unwrap() = Some(format!(
                            "linked-device revocation matrix case {case:?} failed: {message}"
                        ));
                        return;
                    }
                }
            });
        }
    });
    if let Some(message) = failure.into_inner().unwrap() {
        panic!("{message}");
    }
}

#[test]
fn maximum_linked_devices_receive_crossed_message_burst() {
    let world = TestWorld::new();
    let mut alice = world.device("alice-fanout-primary", ALICE, ALICE_TOKEN);
    let mut bob = world.device("bob-fanout-primary", BOB, BOB_TOKEN);
    alice.connect_ready();
    bob.connect_ready();

    let linked_count = rpc::e2e::MAX_ACTIVE_ACCOUNT_DEVICES - 1;
    let mut alice_linked: Vec<_> = (0..linked_count)
        .map(|index| world.device(&format!("alice-fanout-{index:02}"), ALICE, ""))
        .collect();
    let mut bob_linked: Vec<_> = (0..linked_count)
        .map(|index| world.device(&format!("bob-fanout-{index:02}"), BOB, ""))
        .collect();
    for device in &mut alice_linked {
        pair_device(&mut alice, device);
    }
    for device in &mut bob_linked {
        pair_device(&mut bob, device);
    }
    for device in alice_linked.iter_mut().chain(&mut bob_linked) {
        device.connect_ready();
    }

    let room_id = open_dm(&mut alice, &mut bob);
    for device in alice_linked.iter_mut().chain(&mut bob_linked) {
        device.wait_for("maximum-fanout DM room", |event| {
            matches!(
                event,
                NetworkEvent::RoomUpserted(RoomInfo { room_id: id, .. })
                    if *id == room_id
            )
        });
        device.wait_peer_ready(if device.user == ALICE { BOB } else { ALICE });
    }

    let mut bodies = Vec::with_capacity(rpc::e2e::MAX_ACTIVE_ACCOUNT_DEVICES * 2);
    bodies.push("fanout Alice primary".to_string());
    send_chat(&alice, room_id, bodies.last().unwrap());
    for (index, device) in alice_linked.iter().enumerate() {
        bodies.push(format!("fanout Alice linked {index:02}"));
        send_chat(device, room_id, bodies.last().unwrap());
    }
    bodies.push("fanout Bob primary".to_string());
    send_chat(&bob, room_id, bodies.last().unwrap());
    for (index, device) in bob_linked.iter().enumerate() {
        bodies.push(format!("fanout Bob linked {index:02}"));
        send_chat(device, room_id, bodies.last().unwrap());
    }

    for body in &bodies {
        alice.wait_chat(body);
        bob.wait_chat(body);
        for device in alice_linked.iter_mut().chain(&mut bob_linked) {
            device.wait_chat(body);
        }
    }
}
