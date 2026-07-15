//! Live client/server MLS regression matrix.

use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::AtomicU8,
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use rpc::{
    control::decode_device_link_ticket,
    crypto::dev_server_seed_hex,
    ids::{DeviceId, RoomId, StreamId, UserId},
};
use server::{
    Server,
    config::{Config as ServerConfig, RoomConfig, RoomPersistenceConfig, TransportModeConfig, UserConfig, hash_secret},
    local_admin::AdminCommand,
};

use crate::{
    app::{AppEvent, NetworkEventSender},
    audio::{LocalVoiceFrame, VoicePayload},
    client_net::{
        ClientConfig, FilePolicy, NetworkClient, NetworkCommand, NetworkEvent, PAIRING_CANCELABLE,
        UploadFileRequest, spawn_device_pair_once,
    },
    config::{CandidatePrivacy, DownloadTarget, EffectiveFiles},
    receive_store::DownloadStore,
    test_temp::TempDir,
};

const ALICE_TOKEN: &str = "mls-matrix-alice";
const BOB_TOKEN: &str = "mls-matrix-bob";
const CAROL_TOKEN: &str = "mls-matrix-carol";
const PUBLIC_ROOM: RoomId = RoomId(1);
const GROUP_ROOM: RoomId = RoomId(2);

struct Addrs {
    tcp: String,
    udp: String,
    admin: mpsc::Sender<AdminCommand>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for Addrs {
    fn drop(&mut self) {
        let _ = self.admin.send(AdminCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

struct Client {
    handle: NetworkClient,
    events: mpsc::Receiver<AppEvent>,
    backlog: RefCell<VecDeque<NetworkEvent>>,
}

static LIVE_MLS_E2E: Mutex<()> = Mutex::new(());

fn live_mls_e2e_guard() -> std::sync::MutexGuard<'static, ()> {
    LIVE_MLS_E2E.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn temp_dir(label: &str) -> TempDir {
    TempDir::new(&format!("mls-e2e-{label}"))
}

fn start_server(root: &Path) -> Addrs {
    start_server_with(
        root,
        vec![RoomConfig {
            id: PUBLIC_ROOM.0,
            name: "lobby".into(),
            members: None,
            persistence: RoomPersistenceConfig::None,
            memory_limit: None,
            is_default: true,
        }],
        vec![
            UserConfig { id: UserId(1), internal_reference: "alice".into(), username: "Alice".into(), token_hash: hash_secret(ALICE_TOKEN) },
            UserConfig { id: UserId(2), internal_reference: "bob".into(), username: "Bob".into(), token_hash: hash_secret(BOB_TOKEN) },
        ],
    )
}

fn start_server_with(root: &Path, rooms: Vec<RoomConfig>, users: Vec<UserConfig>) -> Addrs {
    let mut config = ServerConfig::default();
    config.network.tcp_addr = "127.0.0.1:0".parse().unwrap();
    config.network.udp_addr = Some("127.0.0.1:0".parse().unwrap());
    config.network.udp_probe_addr = None;
    config.network.public_tcp_addr.clear();
    config.network.public_udp_addr.clear();
    config.network.public_udp_probe_addr = None;
    config.network.p2p_enabled = false;
    config.security.server_identity_seed = dev_server_seed_hex();
    config.security.transport_mode = TransportModeConfig::NativeEncrypted;
    config.storage.data_dir = Some(root.join("server").display().to_string());
    config.rooms = rooms;
    let mut server = Server::bind(config).unwrap();
    server.seed_users(users).unwrap();
    let tcp = server.tcp_local_addr().unwrap().to_string();
    let udp = server.udp_local_addr().unwrap().to_string();
    let (admin, rx) = mpsc::channel();
    let worker = thread::Builder::new().name("mls-e2e-server".into()).spawn(move || {
        let _ = server.run(&rx);
    }).unwrap();
    Addrs { tcp, udp, admin, worker: Some(worker) }
}

fn config(addrs: &Addrs, root: &Path, name: &str, token: &str, receive: bool) -> ClientConfig {
    let target = if receive {
        let path = root.join(format!("{}-downloads", name.to_ascii_lowercase()));
        std::fs::create_dir_all(&path).unwrap();
        DownloadTarget::Persistent(path)
    } else {
        DownloadTarget::Off
    };
    ClientConfig {
        tcp_addr: addrs.tcp.clone(),
        udp_addr: addrs.udp.clone(),
        udp_probe_addr: None,
        username: name.into(),
        token: token.into(),
        server_public_key: None,
        data_dir: Some(root.join(format!("{}-state", name.to_ascii_lowercase()))),
        e2e_peer_pins: Vec::new(),
        require_native_encryption: true,
        file_policy: FilePolicy { default: EffectiveFiles { target, max_download_bytes: 8 * 1024 * 1024 }, rooms: Vec::new() },
        download_store: DownloadStore::new(8 * 1024 * 1024),
        max_upload_bytes: 8 * 1024 * 1024,
        upload_rate_bytes: 0,
        p2p_enabled: false,
        candidate_privacy: CandidatePrivacy::Disabled,
        prefer_ipv6: false,
    }
}

fn spawn(config: ClientConfig) -> Client {
    let (tx, events) = mpsc::channel();
    let handle = NetworkClient::spawn(config, NetworkEventSender::for_test(tx)).unwrap();
    Client { handle, events, backlog: RefCell::new(VecDeque::new()) }
}

fn wait_event<F>(client: &Client, label: &str, timeout: Duration, mut predicate: F) -> NetworkEvent
where F: FnMut(&NetworkEvent) -> bool {
    let backlog_match = client.backlog.borrow().iter().position(&mut predicate);
    if let Some(index) = backlog_match {
        return client.backlog.borrow_mut().remove(index).unwrap();
    }
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("{label}: timed out"));
        match client.events.recv_timeout(remaining) {
            Ok(AppEvent::Network(event)) => {
                match &event {
                    NetworkEvent::AuthFailed { message, .. }
                    | NetworkEvent::WorkerStopped { reason: message }
                    | NetworkEvent::Error(message) => panic!("{label}: {message}"),
                    _ => {}
                }
                if predicate(&event) { return event; }
                client.backlog.borrow_mut().push_back(event);
            }
            Ok(_) => {}
            Err(error) => panic!("{label}: {error}"),
        }
    }
}

fn wait_authenticated(client: &Client, label: &str) -> DeviceId {
    wait_event(client, label, Duration::from_secs(10), |event| matches!(event, NetworkEvent::Authenticated { .. }));
    let bound = wait_event(client, label, Duration::from_secs(10), |event| matches!(event, NetworkEvent::MlsDeviceBound { .. }));
    let NetworkEvent::MlsDeviceBound { device_id } = bound else { unreachable!() };
    device_id
}

fn pair_device(
    mut pair_config: ClientConfig,
    pairing_string: &str,
    transfer_password: &str,
    device_name: &str,
) -> (ClientConfig, Client, DeviceId) {
    let ticket = decode_device_link_ticket(pairing_string).unwrap();
    let cancellation = Arc::new(AtomicU8::new(PAIRING_CANCELABLE));
    let (tx, rx) = mpsc::channel();
    let worker = spawn_device_pair_once(
        pair_config.clone(),
        ticket,
        transfer_password.to_string(),
        device_name.to_string(),
        false,
        cancellation,
        NetworkEventSender::for_test(tx),
    );
    let result = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    let AppEvent::Network(NetworkEvent::DevicePairingSucceeded {
        token,
        username,
        udp_addr,
        udp_probe_addr,
        server_public_key,
    }) = result else {
        panic!("device pairing did not succeed");
    };
    pair_config.token = token;
    pair_config.username = username;
    pair_config.udp_addr = udp_addr;
    pair_config.udp_probe_addr = udp_probe_addr;
    pair_config.server_public_key = Some(server_public_key);
    let client = spawn(pair_config.clone());
    let device_id = wait_authenticated(&client, device_name);
    (pair_config, client, device_id)
}

fn relay_one_frame(source: &mut TcpStream, destination: &mut TcpStream) {
    let mut length = [0u8; 4];
    source.read_exact(&mut length).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(length) as usize];
    source.read_exact(&mut payload).unwrap();
    destination.write_all(&length).unwrap();
    destination.write_all(&payload).unwrap();
}

fn spawn_pair_response_loss_proxy(server_addr: &str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server_addr = server_addr.to_string();
    let worker = thread::Builder::new()
        .name("mls-pair-response-loss".into())
        .spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut server = TcpStream::connect(server_addr).unwrap();
            // Hello, enrollment fetch, then redemption. The server receives
            // the atomic redemption, but its success response is discarded.
            relay_one_frame(&mut client, &mut server);
            relay_one_frame(&mut server, &mut client);
            relay_one_frame(&mut client, &mut server);
            relay_one_frame(&mut server, &mut client);
            relay_one_frame(&mut client, &mut server);
            thread::sleep(Duration::from_millis(200));
            let _ = client.shutdown(Shutdown::Both);
            let _ = server.shutdown(Shutdown::Both);
        })
        .unwrap();
    (addr, worker)
}

fn send_until_received(sender: &Client, receiver: &Client, room: RoomId, body: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let backlog_match = receiver.backlog.borrow().iter().position(|event| {
            matches!(event, NetworkEvent::Chat(chat) if chat.message.body == body)
        });
        if let Some(index) = backlog_match {
            receiver.backlog.borrow_mut().remove(index);
            return;
        }
        sender.handle.try_send(NetworkCommand::SendChat { room_id: room, body: body.into() }).unwrap();
        let slice = deadline.checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("message {body:?} was not delivered"))
            .min(Duration::from_millis(250));
        match receiver.events.recv_timeout(slice) {
            Ok(AppEvent::Network(NetworkEvent::Chat(chat))) if chat.message.body == body => return,
            Ok(AppEvent::Network(NetworkEvent::AuthFailed { message, .. })) => panic!("auth failed: {message}"),
            Ok(AppEvent::Network(NetworkEvent::WorkerStopped { reason })) => panic!("worker stopped: {reason}"),
            Ok(AppEvent::Network(NetworkEvent::Error(message))) => panic!("network error: {message}"),
            Ok(AppEvent::Network(event)) => receiver.backlog.borrow_mut().push_back(event),
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("receiver disconnected: {error}"),
        }
    }
}

fn wait_revoked(client: &Client, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        while let Some(event) = client.backlog.borrow_mut().pop_front() {
            match event {
                NetworkEvent::LocalIdentityUnavailable { message }
                    if message.contains("revoked") => return,
                NetworkEvent::AuthFailed { code: 401, .. } => return,
                _ => {}
            }
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("{label}: revoked device remained usable"));
        match client.events.recv_timeout(remaining) {
            Ok(AppEvent::Network(NetworkEvent::LocalIdentityUnavailable { message }))
                if message.contains("revoked") => return,
            Ok(AppEvent::Network(NetworkEvent::AuthFailed { code: 401, .. })) => return,
            Ok(_) => {}
            Err(error) => panic!("{label}: {error}"),
        }
    }
}

fn wait_chat(client: &Client, body: &str) {
    wait_event(client, body, Duration::from_secs(10), |event| {
        matches!(event, NetworkEvent::Chat(chat) if chat.message.body == body)
    });
}

fn restart_client(client: Client, config: &ClientConfig, label: &str) -> Client {
    drop(client);
    let client = spawn(config.clone());
    wait_authenticated(&client, label);
    client
}

fn retarget(mut config: ClientConfig, addrs: &Addrs) -> ClientConfig {
    config.tcp_addr = addrs.tcp.clone();
    config.udp_addr = addrs.udp.clone();
    config
}

fn open_dm(client: &Client) -> RoomId {
    client.handle.try_send(NetworkCommand::OpenDm(UserId(2))).unwrap();
    let opened = wait_event(client, "open dm", Duration::from_secs(10), |event| {
        matches!(event, NetworkEvent::DmOpened { .. })
    });
    let NetworkEvent::DmOpened { room_id, .. } = opened else {
        unreachable!()
    };
    room_id
}

fn link_device(
    addrs: &Addrs,
    root: &Path,
    primary: &Client,
    state_name: &str,
    device_name: &str,
) -> Client {
    link_device_with_id(addrs, root, primary, state_name, device_name).0
}

fn link_device_with_id(
    addrs: &Addrs,
    root: &Path,
    primary: &Client,
    state_name: &str,
    device_name: &str,
) -> (Client, DeviceId) {
    primary
        .handle
        .try_send(NetworkCommand::CreateDeviceLink)
        .unwrap();
    let link = wait_event(primary, device_name, Duration::from_secs(10), |event| {
        matches!(event, NetworkEvent::DeviceLinkCreated { .. })
    });
    let NetworkEvent::DeviceLinkCreated {
        pairing_string,
        transfer_password,
        ..
    } = link
    else {
        unreachable!()
    };
    let pair_config = config(addrs, root, state_name, "unused", false);
    let (_, linked, device_id) = pair_device(
        pair_config,
        &pairing_string,
        &transfer_password,
        device_name,
    );
    (linked, device_id)
}

fn wait_room_upsert(client: &Client, room_id: RoomId, label: &str) {
    wait_event(client, label, Duration::from_secs(10), |event| {
        matches!(event, NetworkEvent::RoomUpserted(room) if room.room_id == room_id)
    });
}

fn send_alice_to_all(sender: &Client, primary: &Client, linked: &Client, bob: &Client, room: RoomId, body: &str) {
    send_until_received(sender, bob, room, body);
    send_until_received(sender, primary, room, body);
    send_until_received(sender, linked, room, body);
}

fn send_bob_to_all(bob: &Client, primary: &Client, linked: &Client, room: RoomId, body: &str) {
    send_until_received(bob, primary, room, body);
    send_until_received(bob, linked, room, body);
    send_until_received(bob, bob, room, body);
}

fn join_voice(client: &Client, room_id: RoomId, user_id: UserId) -> StreamId {
    wait_event(client, "UDP media binding", Duration::from_secs(10), |event| {
        matches!(event, NetworkEvent::Status(message) if message == "udp media bound")
    });
    client
        .handle
        .try_send(NetworkCommand::JoinVoice(room_id))
        .unwrap();
    let started = wait_event(client, "voice stream", Duration::from_secs(10), |event| {
        matches!(event, NetworkEvent::VoiceStarted {
            room_id: observed_room,
            user_id: observed_user,
            ..
        } if *observed_room == room_id && *observed_user == user_id)
    });
    let NetworkEvent::VoiceStarted { stream_id, .. } = started else {
        unreachable!()
    };
    stream_id
}

fn relay_voice(sender: &Client, receiver: &Client, stream_id: StreamId, payload: Vec<u8>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(index) = receiver.backlog.borrow().iter().position(|event| {
            matches!(event, NetworkEvent::VoicePacketObserved {
                stream_id: observed,
                payload_size,
            } if *observed == stream_id.0 && *payload_size == payload.len())
        }) {
            receiver.backlog.borrow_mut().remove(index);
            return;
        }
        for timestamp in 0..3 {
            sender
                .handle
                .try_send(NetworkCommand::LocalVoicePacket(LocalVoiceFrame {
                    flags: 0,
                    payload: VoicePayload::Opus(payload.clone()),
                    timestamp,
                }))
                .unwrap();
        }
        let slice = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("voice stream {} was not relayed", stream_id.0))
            .min(Duration::from_millis(100));
        match receiver.events.recv_timeout(slice) {
            Ok(AppEvent::Network(NetworkEvent::VoicePacketObserved {
                stream_id: observed,
                payload_size,
            })) if observed == stream_id.0 && payload_size == payload.len() => return,
            Ok(AppEvent::Network(NetworkEvent::AuthFailed { message, .. })) => {
                panic!("voice receiver authentication failed: {message}")
            }
            Ok(AppEvent::Network(NetworkEvent::WorkerStopped { reason })) => {
                panic!("voice receiver stopped: {reason}")
            }
            Ok(AppEvent::Network(NetworkEvent::Error(message))) => {
                panic!("voice receiver failed: {message}")
            }
            Ok(AppEvent::Network(event)) => receiver.backlog.borrow_mut().push_back(event),
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("voice receiver disconnected: {error}"),
        }
    }
}

const LINKED_MATRIX_BITS: u32 = 16;
const LINKED_MATRIX_PAIRWISE_ROWS: u32 = 32;
const LINKED_MATRIX_RANDOM_CASES: usize = 16;
const LINKED_MATRIX_SEED: u64 = 0x6d61_7472_6978_2d32;

#[derive(Clone, Copy, Debug)]
struct LinkedMatrixCase {
    bits: u32,
    bob_connects_first: bool,
    dm_before_link: bool,
    linked_offline_for_first_message: bool,
    dm_opened_by_linked: bool,
    restart_primary_before_text: bool,
    restart_linked_before_text: bool,
    restart_bob_before_text: bool,
    alice_text_from_linked: bool,
    alice_file_from_linked: bool,
    restart_primary_before_file: bool,
    restart_linked_before_file: bool,
    restart_bob_before_file: bool,
    pre_link_message_from_alice: bool,
    pre_link_message_from_bob: bool,
    bob_offline_during_link: bool,
    crossed_text_sends: bool,
}

impl LinkedMatrixCase {
    fn from_bits(bits: u32) -> Self {
        let bit = |index| bits & (1u32 << index) != 0u32;
        Self {
            bits,
            bob_connects_first: bit(0),
            dm_before_link: bit(1),
            linked_offline_for_first_message: bit(2),
            dm_opened_by_linked: bit(3),
            restart_primary_before_text: bit(4),
            restart_linked_before_text: bit(5),
            restart_bob_before_text: bit(6),
            alice_text_from_linked: bit(7),
            alice_file_from_linked: bit(8),
            restart_primary_before_file: bit(9),
            restart_linked_before_file: bit(10),
            restart_bob_before_file: bit(11),
            pre_link_message_from_alice: bit(12),
            pre_link_message_from_bob: bit(13),
            bob_offline_during_link: bit(14),
            crossed_text_sends: bit(15),
        }
    }
}

#[derive(Clone, Copy)]
struct MatrixRng(u64);

impl MatrixRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, self.next() as usize % (index + 1));
        }
    }
}

fn linked_matrix_cases() -> Vec<LinkedMatrixCase> {
    if let Ok(value) = std::env::var("CHATT_MLS_MATRIX_CASE") {
        let bits = value
            .strip_prefix("0x")
            .map(|hex| u32::from_str_radix(hex, 16))
            .unwrap_or_else(|| value.parse())
            .expect("invalid CHATT_MLS_MATRIX_CASE");
        assert!(bits < 1 << LINKED_MATRIX_BITS);
        return vec![LinkedMatrixCase::from_bits(bits)];
    }
    let seed = std::env::var("CHATT_MLS_MATRIX_SEED")
        .ok()
        .map(|value| {
            value
                .strip_prefix("0x")
                .map(|hex| u64::from_str_radix(hex, 16))
                .unwrap_or_else(|| value.parse())
                .expect("invalid CHATT_MLS_MATRIX_SEED")
        })
        .unwrap_or(LINKED_MATRIX_SEED);
    let mut rng = MatrixRng(seed);
    let mut columns: Vec<u32> = (1..LINKED_MATRIX_PAIRWISE_ROWS).collect();
    rng.shuffle(&mut columns);
    let complements = rng.next() as u32;
    let mut encoded = Vec::with_capacity(
        LINKED_MATRIX_PAIRWISE_ROWS as usize + LINKED_MATRIX_RANDOM_CASES,
    );
    for row in 0..LINKED_MATRIX_PAIRWISE_ROWS {
        let mut bits = 0;
        for factor in 0..LINKED_MATRIX_BITS as usize {
            let enabled = ((row & columns[factor]).count_ones() % 2 != 0)
                ^ (complements & (1 << factor) != 0);
            bits |= u32::from(enabled) << factor;
        }
        encoded.push(bits);
    }
    let mask = (1 << LINKED_MATRIX_BITS) - 1;
    while encoded.len() < LINKED_MATRIX_PAIRWISE_ROWS as usize + LINKED_MATRIX_RANDOM_CASES {
        let bits = rng.next() as u32 & mask;
        if !encoded.contains(&bits) {
            encoded.push(bits);
        }
    }
    rng.shuffle(&mut encoded);
    encoded.into_iter().map(LinkedMatrixCase::from_bits).collect()
}

fn assert_linked_matrix_pairwise(cases: &[LinkedMatrixCase]) {
    for first in 0..LINKED_MATRIX_BITS {
        for second in first + 1..LINKED_MATRIX_BITS {
            let mut covered = [false; 4];
            for case in cases {
                let a = case.bits & (1 << first) != 0;
                let b = case.bits & (1 << second) != 0;
                covered[usize::from(a) * 2 + usize::from(b)] = true;
            }
            assert!(covered.into_iter().all(|value| value));
        }
    }
}

fn run_linked_matrix_case(case: LinkedMatrixCase) {
    let root = temp_dir(&format!("linked-{:04x}", case.bits));
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server(&root);
    let primary_config = config(&addrs, &root, "Alice", ALICE_TOKEN, true);
    let bob_config = config(&addrs, &root, "Bob", BOB_TOKEN, true);
    let linked_pair_config = config(&addrs, &root, "Alice-linked", "unused", true);

    let (mut primary, mut bob) = if case.bob_connects_first {
        let bob = spawn(bob_config.clone());
        wait_authenticated(&bob, "matrix bob first");
        let primary = spawn(primary_config.clone());
        wait_authenticated(&primary, "matrix primary second");
        (primary, bob)
    } else {
        let primary = spawn(primary_config.clone());
        wait_authenticated(&primary, "matrix primary first");
        let bob = spawn(bob_config.clone());
        wait_authenticated(&bob, "matrix bob second");
        (primary, bob)
    };

    let needs_pre_link_dm = case.dm_before_link
        || case.pre_link_message_from_alice
        || case.pre_link_message_from_bob;
    let mut room_id = needs_pre_link_dm.then(|| open_dm(&primary));
    if let Some(room_id) = room_id {
        // A peer cannot address a newly created room until its authenticated
        // RoomUpserted notification has installed the room's MLS policy.
        wait_room_upsert(&bob, room_id, "matrix Bob pre-link room");
    }
    let mut pre_link_bodies = Vec::new();
    if case.pre_link_message_from_alice {
        let body = format!("pre-link Alice {:04x}", case.bits);
        send_until_received(&primary, &bob, room_id.unwrap(), &body);
        wait_chat(&primary, &body);
        pre_link_bodies.push(body);
    }
    if case.pre_link_message_from_bob {
        let body = format!("pre-link Bob {:04x}", case.bits);
        send_until_received(&bob, &primary, room_id.unwrap(), &body);
        wait_chat(&bob, &body);
        pre_link_bodies.push(body);
    }

    let mut bob_slot = Some(bob);
    if case.bob_offline_during_link {
        drop(bob_slot.take());
    }
    primary.handle.try_send(NetworkCommand::CreateDeviceLink).unwrap();
    let link = wait_event(&primary, "matrix create link", Duration::from_secs(10), |event| {
        matches!(event, NetworkEvent::DeviceLinkCreated { .. })
    });
    let NetworkEvent::DeviceLinkCreated {
        pairing_string,
        transfer_password,
        ..
    } = link
    else {
        unreachable!()
    };
    let (linked_config, mut linked, _) = pair_device(
        linked_pair_config,
        &pairing_string,
        &transfer_password,
        "matrix linked",
    );
    for body in &pre_link_bodies {
        assert!(
            !linked.backlog.borrow().iter().any(
                |event| matches!(event, NetworkEvent::Chat(chat) if chat.message.body == *body)
            ),
            "case {case:?}: linked device decrypted pre-link history"
        );
    }

    if room_id.is_none() {
        room_id = Some(if case.dm_opened_by_linked {
            open_dm(&linked)
        } else {
            open_dm(&primary)
        });
        if let Some(bob) = bob_slot.as_ref() {
            wait_room_upsert(bob, room_id.unwrap(), "matrix Bob post-link room");
        }
    }
    let room_id = room_id.unwrap();
    if bob_slot.is_none() {
        let reconnected = spawn(bob_config.clone());
        wait_authenticated(&reconnected, "matrix bob after link");
        bob_slot = Some(reconnected);
    }
    bob = bob_slot.unwrap();

    // This delivery is also a membership barrier: pairing is not considered
    // settled until the linked installation has externally joined the room.
    let joined = format!("linked membership barrier {:04x}", case.bits);
    send_bob_to_all(&bob, &primary, &linked, room_id, &joined);
    if case.linked_offline_for_first_message {
        drop(linked);
        let offline = format!("linked offline {:04x}", case.bits);
        send_until_received(&bob, &primary, room_id, &offline);
        wait_chat(&bob, &offline);
        linked = spawn(linked_config.clone());
        wait_authenticated(&linked, "matrix linked offline restart");
        wait_chat(&linked, &offline);
    }

    if case.restart_primary_before_text {
        primary = restart_client(primary, &primary_config, "matrix primary text restart");
    }
    if case.restart_linked_before_text {
        linked = restart_client(linked, &linked_config, "matrix linked text restart");
    }
    if case.restart_bob_before_text {
        bob = restart_client(bob, &bob_config, "matrix bob text restart");
    }
    let text_barrier = format!("text barrier {:04x}", case.bits);
    send_bob_to_all(&bob, &primary, &linked, room_id, &text_barrier);

    let alice_text = format!("Alice text {:04x}", case.bits);
    let bob_text = format!("Bob text {:04x}", case.bits);
    let alice_sender = if case.alice_text_from_linked {
        &linked
    } else {
        &primary
    };
    if case.crossed_text_sends {
        alice_sender
            .handle
            .try_send(NetworkCommand::SendChat {
                room_id,
                body: alice_text.clone(),
            })
            .unwrap();
        bob.handle
            .try_send(NetworkCommand::SendChat {
                room_id,
                body: bob_text.clone(),
            })
            .unwrap();
        for client in [&primary, &linked, &bob] {
            wait_chat(client, &alice_text);
            wait_chat(client, &bob_text);
        }
    } else {
        send_alice_to_all(alice_sender, &primary, &linked, &bob, room_id, &alice_text);
        send_bob_to_all(&bob, &primary, &linked, room_id, &bob_text);
    }

    if case.restart_primary_before_file {
        primary = restart_client(primary, &primary_config, "matrix primary file restart");
    }
    if case.restart_linked_before_file {
        linked = restart_client(linked, &linked_config, "matrix linked file restart");
    }
    if case.restart_bob_before_file {
        bob = restart_client(bob, &bob_config, "matrix bob file restart");
    }
    let file_barrier = format!("file barrier {:04x}", case.bits);
    send_bob_to_all(&bob, &primary, &linked, room_id, &file_barrier);

    let file_name = format!("matrix-{:04x}.bin", case.bits);
    let payload = format!("MLS linked-device file payload for {:04x}", case.bits)
        .repeat(128)
        .into_bytes();
    let source = root.join(&file_name);
    std::fs::write(&source, &payload).unwrap();
    let file_sender = if case.alice_file_from_linked {
        &linked
    } else {
        &primary
    };
    file_sender
        .handle
        .try_send(NetworkCommand::UploadFile {
            room_id: Some(room_id),
            request: UploadFileRequest::new(source),
        })
        .unwrap();
    let other_alice = if case.alice_file_from_linked {
        &primary
    } else {
        &linked
    };
    for (client, download_dir) in [
        (other_alice, root.join(if case.alice_file_from_linked { "alice-downloads" } else { "alice-linked-downloads" })),
        (&bob, root.join("bob-downloads")),
    ] {
        let event = wait_event(client, "matrix file", Duration::from_secs(20), |event| {
            matches!(event, NetworkEvent::FileReceived { served_name, .. } if served_name == &file_name)
        });
        let NetworkEvent::FileReceived { served_name, .. } = event else {
            unreachable!()
        };
        assert_eq!(std::fs::read(download_dir.join(served_name)).unwrap(), payload);
    }

    let bob_file_name = format!("matrix-bob-{:04x}.bin", case.bits);
    let bob_payload = format!("Bob MLS linked-device file payload for {:04x}", case.bits)
        .repeat(128)
        .into_bytes();
    let bob_source = root.join(&bob_file_name);
    std::fs::write(&bob_source, &bob_payload).unwrap();
    bob.handle
        .try_send(NetworkCommand::UploadFile {
            room_id: Some(room_id),
            request: UploadFileRequest::new(bob_source),
        })
        .unwrap();
    for (client, download_dir) in [
        (&primary, root.join("alice-downloads")),
        (&linked, root.join("alice-linked-downloads")),
    ] {
        let event = wait_event(client, "matrix Bob file", Duration::from_secs(20), |event| {
            matches!(event, NetworkEvent::FileReceived { served_name, .. } if served_name == &bob_file_name)
        });
        let NetworkEvent::FileReceived { served_name, .. } = event else {
            unreachable!()
        };
        assert_eq!(
            std::fs::read(download_dir.join(served_name)).unwrap(),
            bob_payload
        );
    }

    let alice_voice = if case.alice_file_from_linked {
        &linked
    } else {
        &primary
    };
    let alice_stream = join_voice(alice_voice, room_id, UserId(1));
    let bob_stream = join_voice(&bob, room_id, UserId(2));
    relay_voice(
        alice_voice,
        &bob,
        alice_stream,
        vec![0xa1, (case.bits & 0xff) as u8, 1, 2, 3],
    );
    relay_voice(
        &bob,
        alice_voice,
        bob_stream,
        vec![0xb0, ((case.bits >> 4) & 0xff) as u8, 4, 5, 6, 7, 8],
    );
}

#[test]
fn mls_live_linked_device_pairwise_matrix() {
    let _guard = live_mls_e2e_guard();
    let cases = linked_matrix_cases();
    if cases.len() > 1 {
        assert_eq!(cases.len(), 48);
        assert_linked_matrix_pairwise(&cases);
    }
    let failure = Mutex::new(None);
    let workers = 4.min(cases.len());
    thread::scope(|scope| {
        for worker in 0..workers {
            let cases = &cases;
            let failure = &failure;
            scope.spawn(move || {
                for case in cases.iter().copied().skip(worker).step_by(workers) {
                    if failure.lock().unwrap().is_some() {
                        return;
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        run_linked_matrix_case(case)
                    }));
                    if let Err(payload) = result {
                        let message = payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| payload.downcast_ref::<&str>().copied())
                            .unwrap_or("non-string panic");
                        *failure.lock().unwrap() = Some(format!(
                            "MLS linked-device matrix case {:#06x} failed ({case:?}): {message}",
                            case.bits
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
fn mls_live_restart_offline_direction_matrix_and_file_round_trip() {
    let _guard = live_mls_e2e_guard();
    let root = temp_dir("matrix");
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server(&root);
    let alice_config = config(&addrs, &root, "Alice", ALICE_TOKEN, false);
    let bob_config = config(&addrs, &root, "Bob", BOB_TOKEN, true);
    let mut alice = Some(spawn(alice_config.clone()));
    let mut bob = Some(spawn(bob_config.clone()));
    wait_authenticated(alice.as_ref().unwrap(), "alice initial");
    wait_authenticated(bob.as_ref().unwrap(), "bob initial");
    alice.as_ref().unwrap().handle.try_send(NetworkCommand::OpenDm(UserId(2))).unwrap();
    let opened = wait_event(alice.as_ref().unwrap(), "open dm", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DmOpened { .. }));
    let NetworkEvent::DmOpened { room_id, .. } = opened else { unreachable!() };
    send_until_received(alice.as_ref().unwrap(), bob.as_ref().unwrap(), room_id, "matrix bootstrap");

    // Exhaust every combination of direction, sender restart, receiver
    // restart, and receiver-offline delivery. Each restart reopens the exact
    // SQLCipher MLS database and bootstrap used by the previous process.
    for bits in 0u8..16 {
        let alice_sends = bits & 1 == 0;
        let restart_sender = bits & 2 != 0;
        let restart_receiver = bits & 4 != 0;
        let offline_receiver = bits & 8 != 0;
        if restart_sender || restart_receiver {
            if (alice_sends && restart_sender) || (!alice_sends && restart_receiver) {
                drop(alice.take());
                alice = Some(spawn(alice_config.clone()));
                wait_authenticated(alice.as_ref().unwrap(), "alice restart");
            }
            if (!alice_sends && restart_sender) || (alice_sends && restart_receiver) {
                drop(bob.take());
                bob = Some(spawn(bob_config.clone()));
                wait_authenticated(bob.as_ref().unwrap(), "bob restart");
            }
        }
        let body = format!("mls matrix case {bits:02x}");
        if offline_receiver {
            if alice_sends {
                drop(bob.take());
                alice.as_ref().unwrap().handle.try_send(NetworkCommand::SendChat { room_id, body: body.clone() }).unwrap();
                thread::sleep(Duration::from_millis(150));
                bob = Some(spawn(bob_config.clone()));
                wait_authenticated(bob.as_ref().unwrap(), "bob offline restart");
                wait_event(bob.as_ref().unwrap(), &body, Duration::from_secs(10), |event| matches!(event, NetworkEvent::Chat(chat) if chat.message.body == body));
            } else {
                drop(alice.take());
                bob.as_ref().unwrap().handle.try_send(NetworkCommand::SendChat { room_id, body: body.clone() }).unwrap();
                thread::sleep(Duration::from_millis(150));
                alice = Some(spawn(alice_config.clone()));
                wait_authenticated(alice.as_ref().unwrap(), "alice offline restart");
                wait_event(alice.as_ref().unwrap(), &body, Duration::from_secs(10), |event| matches!(event, NetworkEvent::Chat(chat) if chat.message.body == body));
            }
        } else if alice_sends {
            send_until_received(alice.as_ref().unwrap(), bob.as_ref().unwrap(), room_id, &body);
        } else {
            send_until_received(bob.as_ref().unwrap(), alice.as_ref().unwrap(), room_id, &body);
        }
    }

    let payload = b"MLS file payload survives encrypted chunk relay".repeat(4096);
    let source = root.join("payload.bin");
    std::fs::write(&source, &payload).unwrap();
    alice.as_ref().unwrap().handle.try_send(NetworkCommand::UploadFile {
        room_id: Some(room_id), request: UploadFileRequest::new(source),
    }).unwrap();
    let received = wait_event(bob.as_ref().unwrap(), "MLS file", Duration::from_secs(20), |event| matches!(event, NetworkEvent::FileReceived { .. }));
    let NetworkEvent::FileReceived { served_name, .. } = received else { unreachable!() };
    assert_eq!(std::fs::read(root.join("bob-downloads").join(served_name)).unwrap(), payload);
}

#[test]
fn mls_live_maximum_linked_devices_receive_crossed_message_burst() {
    let _guard = live_mls_e2e_guard();
    let root = temp_dir("maximum-fanout");
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server(&root);
    let alice_config = config(&addrs, &root, "Alice-fanout-primary", ALICE_TOKEN, false);
    let bob_config = config(&addrs, &root, "Bob-fanout-primary", BOB_TOKEN, false);
    let alice = spawn(alice_config);
    let bob = spawn(bob_config);
    wait_authenticated(&alice, "fanout Alice primary");
    wait_authenticated(&bob, "fanout Bob primary");

    let linked_count = rpc::identity::MAX_ACTIVE_DEVICES - 1;
    let mut alice_linked = Vec::with_capacity(linked_count);
    let mut bob_linked = Vec::with_capacity(linked_count);
    for index in 0..linked_count {
        alice
            .handle
            .try_send(NetworkCommand::CreateDeviceLink)
            .unwrap();
        let link = wait_event(&alice, "fanout Alice link", Duration::from_secs(10), |event| {
            matches!(event, NetworkEvent::DeviceLinkCreated { .. })
        });
        let NetworkEvent::DeviceLinkCreated {
            pairing_string,
            transfer_password,
            ..
        } = link
        else {
            unreachable!()
        };
        let pair_config = config(
            &addrs,
            &root,
            &format!("Alice-fanout-{index:02}"),
            "unused",
            false,
        );
        let (_, linked, _) = pair_device(
            pair_config,
            &pairing_string,
            &transfer_password,
            &format!("Alice fanout {index:02}"),
        );
        alice_linked.push(linked);
    }
    for index in 0..linked_count {
        bob.handle
            .try_send(NetworkCommand::CreateDeviceLink)
            .unwrap();
        let link = wait_event(&bob, "fanout Bob link", Duration::from_secs(10), |event| {
            matches!(event, NetworkEvent::DeviceLinkCreated { .. })
        });
        let NetworkEvent::DeviceLinkCreated {
            pairing_string,
            transfer_password,
            ..
        } = link
        else {
            unreachable!()
        };
        let pair_config = config(
            &addrs,
            &root,
            &format!("Bob-fanout-{index:02}"),
            "unused",
            false,
        );
        let (_, linked, _) = pair_device(
            pair_config,
            &pairing_string,
            &transfer_password,
            &format!("Bob fanout {index:02}"),
        );
        bob_linked.push(linked);
    }

    let room_id = open_dm(&alice);
    wait_room_upsert(&bob, room_id, "fanout Bob room");
    let barrier = "fanout membership barrier";
    send_until_received(&alice, &alice, room_id, barrier);
    send_until_received(&alice, &bob, room_id, barrier);
    for client in alice_linked.iter().chain(&bob_linked) {
        send_until_received(&alice, client, room_id, barrier);
    }

    let mut bodies = Vec::with_capacity(rpc::identity::MAX_ACTIVE_DEVICES * 2);
    bodies.push(format!("fanout-event-{:02}-{}", 0, "x".repeat(32)));
    alice
        .handle
        .try_send(NetworkCommand::SendChat {
            room_id,
            body: bodies.last().unwrap().clone(),
        })
        .unwrap();
    for (index, client) in alice_linked.iter().enumerate() {
        bodies.push(format!("fanout-event-{:02}-{}", index + 1, "x".repeat(32)));
        client
            .handle
            .try_send(NetworkCommand::SendChat {
                room_id,
                body: bodies.last().unwrap().clone(),
            })
            .unwrap();
    }
    bodies.push(format!(
        "fanout-event-{:02}-{}",
        rpc::identity::MAX_ACTIVE_DEVICES,
        "x".repeat(32)
    ));
    bob.handle
        .try_send(NetworkCommand::SendChat {
            room_id,
            body: bodies.last().unwrap().clone(),
        })
        .unwrap();
    for (index, client) in bob_linked.iter().enumerate() {
        bodies.push(format!(
            "fanout-event-{:02}-{}",
            rpc::identity::MAX_ACTIVE_DEVICES + index + 1,
            "x".repeat(32)
        ));
        client
            .handle
            .try_send(NetworkCommand::SendChat {
                room_id,
                body: bodies.last().unwrap().clone(),
            })
            .unwrap();
    }

    for receiver in std::iter::once(&alice)
        .chain(&alice_linked)
        .chain(std::iter::once(&bob))
        .chain(&bob_linked)
    {
        for body in &bodies {
            wait_chat(receiver, body);
        }
    }
}

#[test]
fn mls_live_fixed_group_three_accounts_with_heterogeneous_devices() {
    let _guard = live_mls_e2e_guard();
    let root = temp_dir("fixed-group");
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server_with(
        &root,
        vec![
            RoomConfig {
                id: PUBLIC_ROOM.0,
                name: "lobby".into(),
                members: None,
                persistence: RoomPersistenceConfig::None,
                memory_limit: None,
                is_default: true,
            },
            RoomConfig {
                id: GROUP_ROOM.0,
                name: "fixed group".into(),
                members: Some(vec!["1".into(), "2".into(), "3".into()]),
                persistence: RoomPersistenceConfig::None,
                memory_limit: None,
                is_default: false,
            },
        ],
        vec![
            UserConfig { id: UserId(1), internal_reference: "alice".into(), username: "Alice".into(), token_hash: hash_secret(ALICE_TOKEN) },
            UserConfig { id: UserId(2), internal_reference: "bob".into(), username: "Bob".into(), token_hash: hash_secret(BOB_TOKEN) },
            UserConfig { id: UserId(3), internal_reference: "carol".into(), username: "Carol".into(), token_hash: hash_secret(CAROL_TOKEN) },
        ],
    );
    let alice = spawn(config(&addrs, &root, "group-Alice-primary", ALICE_TOKEN, false));
    let bob = spawn(config(&addrs, &root, "group-Bob-primary", BOB_TOKEN, false));
    let carol = spawn(config(&addrs, &root, "group-Carol-primary", CAROL_TOKEN, false));
    wait_authenticated(&alice, "group Alice primary");
    wait_authenticated(&bob, "group Bob primary");
    wait_authenticated(&carol, "group Carol primary");

    let alice_second = link_device(
        &addrs,
        &root,
        &alice,
        "group-Alice-second",
        "Alice group second",
    );
    let alice_third = link_device(
        &addrs,
        &root,
        &alice,
        "group-Alice-third",
        "Alice group third",
    );
    let bob_second = link_device(
        &addrs,
        &root,
        &bob,
        "group-Bob-second",
        "Bob group second",
    );

    let receivers = [
        &alice,
        &alice_second,
        &alice_third,
        &bob,
        &bob_second,
        &carol,
    ];
    for (sender, body) in [
        (&alice, "fixed group from Alice"),
        (&bob, "fixed group from Bob"),
        (&carol, "fixed group from Carol"),
    ] {
        for receiver in receivers {
            send_until_received(sender, receiver, GROUP_ROOM, body);
        }
    }
}

#[test]
fn mls_live_server_restart_preserves_room_and_bidirectional_delivery() {
    let _guard = live_mls_e2e_guard();
    let root = temp_dir("server-restart");
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server(&root);
    let mut alice_config = config(&addrs, &root, "server-restart-Alice", ALICE_TOKEN, false);
    let mut bob_config = config(&addrs, &root, "server-restart-Bob", BOB_TOKEN, false);
    let alice = spawn(alice_config.clone());
    let bob = spawn(bob_config.clone());
    wait_authenticated(&alice, "server restart Alice initial");
    wait_authenticated(&bob, "server restart Bob initial");
    let room_id = open_dm(&alice);
    wait_room_upsert(&bob, room_id, "server restart Bob room");
    send_until_received(&alice, &bob, room_id, "before server restart");
    drop(alice);
    drop(bob);
    drop(addrs);

    let addrs = start_server(&root);
    alice_config = retarget(alice_config, &addrs);
    bob_config = retarget(bob_config, &addrs);
    let alice = spawn(alice_config);
    let bob = spawn(bob_config);
    wait_authenticated(&alice, "server restart Alice recovered");
    wait_authenticated(&bob, "server restart Bob recovered");
    send_until_received(&alice, &bob, room_id, "Alice after server restart");
    send_until_received(&bob, &alice, room_id, "Bob after server restart");
}

#[test]
fn mls_live_pair_response_loss_reconciles_exact_device() {
    let _guard = live_mls_e2e_guard();
    let root = temp_dir("pair-response-loss");
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server(&root);
    let alice = spawn(config(&addrs, &root, "pair-loss-Alice", ALICE_TOKEN, false));
    wait_authenticated(&alice, "pair response loss Alice");
    alice
        .handle
        .try_send(NetworkCommand::CreateDeviceLink)
        .unwrap();
    let link = wait_event(
        &alice,
        "pair response loss link",
        Duration::from_secs(10),
        |event| matches!(event, NetworkEvent::DeviceLinkCreated { .. }),
    );
    let NetworkEvent::DeviceLinkCreated {
        pairing_string,
        transfer_password,
        ..
    } = link
    else {
        unreachable!()
    };
    let ticket = decode_device_link_ticket(&pairing_string).unwrap();
    let mut pair_config = config(&addrs, &root, "pair-loss-linked", "unused", false);
    let (proxy_addr, proxy) = spawn_pair_response_loss_proxy(&addrs.tcp);
    pair_config.tcp_addr = proxy_addr;
    let (tx, rx) = mpsc::channel();
    let worker = spawn_device_pair_once(
        pair_config.clone(),
        ticket.clone(),
        transfer_password.clone(),
        "pair loss linked".to_string(),
        false,
        Arc::new(AtomicU8::new(PAIRING_CANCELABLE)),
        NetworkEventSender::for_test(tx),
    );
    let failed = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    proxy.join().unwrap();
    assert!(matches!(
        failed,
        AppEvent::Network(NetworkEvent::DevicePairingFailed { .. })
    ));

    pair_config.tcp_addr = addrs.tcp.clone();
    let (tx, rx) = mpsc::channel();
    let worker = spawn_device_pair_once(
        pair_config.clone(),
        ticket,
        transfer_password,
        "pair loss linked".to_string(),
        false,
        Arc::new(AtomicU8::new(PAIRING_CANCELABLE)),
        NetworkEventSender::for_test(tx),
    );
    let recovered = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    let AppEvent::Network(NetworkEvent::DevicePairingSucceeded {
        token,
        username,
        udp_addr,
        udp_probe_addr,
        server_public_key,
    }) = recovered
    else {
        panic!("pair response loss retry did not reconcile")
    };
    pair_config.token = token;
    pair_config.username = username;
    pair_config.udp_addr = udp_addr;
    pair_config.udp_probe_addr = udp_probe_addr;
    pair_config.server_public_key = Some(server_public_key);
    let linked = spawn(pair_config);
    let linked_device = wait_authenticated(&linked, "pair response loss linked recovery");
    let redeemed = wait_event(
        &alice,
        "pair response loss exact device",
        Duration::from_secs(10),
        |event| {
            matches!(
                event,
                NetworkEvent::DeviceLinkRedeemed { device_id, .. }
                    if *device_id == linked_device
            )
        },
    );
    assert!(matches!(
        redeemed,
        NetworkEvent::DeviceLinkRedeemed { device_id, .. } if device_id == linked_device
    ));
}

#[test]
fn mls_live_missing_and_corrupt_state_remain_public_only() {
    let _guard = live_mls_e2e_guard();
    for corrupt_database in [false, true] {
        let label = if corrupt_database { "corrupt" } else { "missing" };
        let root = temp_dir(label);
        std::fs::create_dir_all(root.join("server")).unwrap();
        let addrs = start_server(&root);
        let alice_config = config(
            &addrs,
            &root,
            &format!("state-{label}-Alice"),
            ALICE_TOKEN,
            false,
        );
        let bob_config = config(
            &addrs,
            &root,
            &format!("state-{label}-Bob"),
            BOB_TOKEN,
            false,
        );
        let alice = spawn(alice_config.clone());
        wait_authenticated(&alice, &format!("state {label} Alice initial"));
        drop(alice);
        let mls_dir = alice_config.data_dir.as_ref().unwrap().join("mls");
        let database = mls_dir.join("mls.db");
        let corrupt_bytes = b"deliberately corrupt SQLCipher database";
        if corrupt_database {
            std::fs::write(&database, corrupt_bytes).unwrap();
        } else {
            std::fs::remove_dir_all(&mls_dir).unwrap();
        }

        let alice = spawn(alice_config);
        let bob = spawn(bob_config);
        wait_event(
            &alice,
            &format!("state {label} authenticated"),
            Duration::from_secs(10),
            |event| matches!(event, NetworkEvent::Authenticated { .. }),
        );
        wait_event(
            &alice,
            &format!("state {label} public only"),
            Duration::from_secs(10),
            |event| matches!(event, NetworkEvent::LocalIdentityUnavailable { .. }),
        );
        wait_authenticated(&bob, &format!("state {label} Bob"));
        send_until_received(
            &alice,
            &bob,
            PUBLIC_ROOM,
            &format!("public message with {label} MLS state"),
        );
        if corrupt_database {
            assert_eq!(std::fs::read(database).unwrap(), corrupt_bytes);
        } else {
            assert!(!mls_dir.join("mls-bootstrap.bin").exists());
        }
    }
}

#[test]
fn mls_live_pair_close_cancel_future_history_and_revocation() {
    let _guard = live_mls_e2e_guard();
    let root = temp_dir("pair-revoke");
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server(&root);
    let alice_config = config(&addrs, &root, "Alice", ALICE_TOKEN, false);
    let bob_config = config(&addrs, &root, "Bob", BOB_TOKEN, false);
    let mut alice = spawn(alice_config.clone());
    let bob = spawn(bob_config);
    let original_alice_device = wait_authenticated(&alice, "alice initial");
    wait_authenticated(&bob, "bob initial");
    alice.handle.try_send(NetworkCommand::OpenDm(UserId(2))).unwrap();
    let opened = wait_event(&alice, "open dm", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DmOpened { .. }));
    let NetworkEvent::DmOpened { room_id, .. } = opened else { unreachable!() };
    send_until_received(&alice, &bob, room_id, "history before pairing");

    alice.handle.try_send(NetworkCommand::CreateDeviceLink).unwrap();
    let link = wait_event(&alice, "create device link", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DeviceLinkCreated { .. }));
    let NetworkEvent::DeviceLinkCreated { pairing_string, transfer_password, .. } = link else { unreachable!() };

    // Closing the creator is deliberately not Cancel. Redemption must remain
    // possible after the generating UI/process disappears (dual boot).
    drop(alice);
    let linked_config = config(&addrs, &root, "Alice-linked", "unused", false);
    let (linked_config, linked, linked_device) = pair_device(
        linked_config,
        &pairing_string,
        &transfer_password,
        "Alice linked",
    );
    assert_ne!(linked_device, original_alice_device);
    send_until_received(&bob, &linked, room_id, "future after pairing");

    // Reopen the original installation, revoke it from the linked device, and
    // prove the affected room automatically removes the old leaf and resumes.
    alice = spawn(alice_config.clone());
    assert_eq!(wait_authenticated(&alice, "alice original reopen"), original_alice_device);
    linked.handle.try_send(NetworkCommand::RevokeE2eDevice { device_id: original_alice_device }).unwrap();
    send_until_received(&linked, &bob, room_id, "after revocation removal");
    drop(alice);

    // Explicit Cancel has the opposite contract: once acknowledged, the
    // exposed ticket cannot be redeemed.
    linked.handle.try_send(NetworkCommand::CreateDeviceLink).unwrap();
    let canceled_link = wait_event(&linked, "create canceled link", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DeviceLinkCreated { .. }));
    let NetworkEvent::DeviceLinkCreated {
        redemption_secret_hash,
        pairing_string,
        transfer_password,
        ..
    } = canceled_link else { unreachable!() };
    linked.handle.try_send(NetworkCommand::CancelDeviceLink { redemption_secret_hash }).unwrap();
    wait_event(&linked, "cancel device link", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DeviceLinkCanceled));

    let canceled_config = config(&addrs, &root, "Alice-canceled", "unused", false);
    let ticket = decode_device_link_ticket(&pairing_string).unwrap();
    let (tx, rx) = mpsc::channel();
    let worker = spawn_device_pair_once(
        canceled_config,
        ticket,
        transfer_password,
        "must not link".into(),
        false,
        Arc::new(AtomicU8::new(PAIRING_CANCELABLE)),
        NetworkEventSender::for_test(tx),
    );
    let result = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    assert!(matches!(result, AppEvent::Network(NetworkEvent::DevicePairingFailed { .. })));

    // The surviving paired installation remains restartable after all roster
    // and membership changes.
    drop(linked);
    let linked = spawn(linked_config);
    assert_eq!(wait_authenticated(&linked, "linked restart"), linked_device);
    send_until_received(&bob, &linked, room_id, "future after linked restart");
}

#[test]
fn mls_live_revocation_exhaustive_matrix() {
    let _guard = live_mls_e2e_guard();
    // Exhaust three independent revocation dimensions: room created
    // before/after linking, victim online/offline, and traffic present/absent
    // immediately before revocation.
    let only_case = std::env::var("CHATT_MLS_REVOCATION_CASE")
        .ok()
        .map(|value| value.parse::<u8>().expect("invalid revocation case"));
    for bits in 0u8..8 {
        if only_case.is_some_and(|only| only != bits) {
            continue;
        }
        let root = temp_dir(&format!("revoke-{bits}"));
        std::fs::create_dir_all(root.join("server")).unwrap();
        let addrs = start_server(&root);
        let alice_config = config(&addrs, &root, "Alice", ALICE_TOKEN, false);
        let bob_config = config(&addrs, &root, "Bob", BOB_TOKEN, false);
        let alice = spawn(alice_config);
        let bob = spawn(bob_config);
        wait_authenticated(&alice, "revocation alice");
        wait_authenticated(&bob, "revocation bob");

        let dm_before_link = bits & 1 != 0;
        let victim_connected = bits & 2 != 0;
        let message_before_revocation = bits & 4 != 0;
        let mut room_id = None;
        if dm_before_link {
            alice.handle.try_send(NetworkCommand::OpenDm(UserId(2))).unwrap();
            let opened = wait_event(&alice, "pre-link dm", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DmOpened { .. }));
            let NetworkEvent::DmOpened { room_id: opened, .. } = opened else { unreachable!() };
            send_until_received(&alice, &bob, opened, &format!("pre-link {bits}"));
            room_id = Some(opened);
        }

        alice.handle.try_send(NetworkCommand::CreateDeviceLink).unwrap();
        let link = wait_event(&alice, "revocation link", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DeviceLinkCreated { .. }));
        let NetworkEvent::DeviceLinkCreated { pairing_string, transfer_password, .. } = link else { unreachable!() };
        let victim_config = config(&addrs, &root, "Alice-victim", "unused", false);
        let (victim_config, victim, victim_id) = pair_device(
            victim_config,
            &pairing_string,
            &transfer_password,
            "revocation victim",
        );
        let mut victim = Some(victim);

        let room_id = match room_id {
            Some(room_id) => room_id,
            None => {
                alice.handle.try_send(NetworkCommand::OpenDm(UserId(2))).unwrap();
                let opened = wait_event(&alice, "post-link dm", Duration::from_secs(10), |event| matches!(event, NetworkEvent::DmOpened { .. }));
                let NetworkEvent::DmOpened { room_id, .. } = opened else { unreachable!() };
                send_until_received(&alice, &bob, room_id, &format!("post-link {bits}"));
                room_id
            }
        };
        send_until_received(
            &bob,
            victim.as_ref().unwrap(),
            room_id,
            &format!("victim joined {bits}"),
        );
        if !victim_connected {
            drop(victim.take());
        }
        if message_before_revocation {
            send_until_received(&bob, &alice, room_id, &format!("before revoke {bits}"));
        }

        alice.handle.try_send(NetworkCommand::RevokeE2eDevice { device_id: victim_id }).unwrap();
        send_until_received(&alice, &bob, room_id, &format!("after revoke {bits}"));
        let revoked = match victim {
            Some(victim) => victim,
            None => spawn(victim_config),
        };
        wait_revoked(&revoked, &format!("revocation case {bits}"));
    }
}

#[test]
fn mls_live_competing_membership_commits_roll_back_loser_and_resume() {
    let _guard = live_mls_e2e_guard();
    let root = temp_dir("competing-commits");
    std::fs::create_dir_all(root.join("server")).unwrap();
    let addrs = start_server(&root);
    let alice = spawn(config(&addrs, &root, "compete-Alice-primary", ALICE_TOKEN, false));
    let bob = spawn(config(&addrs, &root, "compete-Bob", BOB_TOKEN, false));
    wait_authenticated(&alice, "competing Alice primary");
    wait_authenticated(&bob, "competing Bob");
    let alice_second = link_device(
        &addrs,
        &root,
        &alice,
        "compete-Alice-second",
        "competing Alice second",
    );
    let (alice_victim, victim_id) = link_device_with_id(
        &addrs,
        &root,
        &alice,
        "compete-Alice-victim",
        "competing Alice victim",
    );
    let room_id = open_dm(&alice);
    wait_room_upsert(&bob, room_id, "competing Bob room");
    for receiver in [&alice, &alice_second, &alice_victim, &bob] {
        send_until_received(&alice, receiver, room_id, "competing commit barrier");
    }
    alice
        .handle
        .try_send(NetworkCommand::RevokeE2eDevice {
            device_id: victim_id,
        })
        .unwrap();
    alice_second
        .handle
        .try_send(NetworkCommand::RevokeE2eDevice {
            device_id: victim_id,
        })
        .unwrap();
    send_until_received(
        &alice,
        &bob,
        room_id,
        "primary resumed after competing commit",
    );
    send_until_received(
        &alice_second,
        &bob,
        room_id,
        "second resumed after competing commit",
    );
    send_until_received(
        &bob,
        &alice,
        room_id,
        "primary receives after competing commit",
    );
    send_until_received(
        &bob,
        &alice_second,
        room_id,
        "second receives after competing commit",
    );
    wait_revoked(&alice_victim, "competing commit victim");
}
