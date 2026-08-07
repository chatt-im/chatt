//! Pairing workers against a server that disabled transport encryption.
//!
//! Every pairing kind hands the server a secret in its first control message —
//! an invite code and bearer token, an open-pairing password and recovery
//! token, a device redemption secret — so each worker must abandon the session
//! while only the public handshake has reached the wire.

use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    sync::{Arc, atomic::AtomicU8, mpsc},
    thread,
    time::Duration,
};

use rpc::{
    control::DeviceLinkTicket,
    crypto::{dev_server_public_key, dev_server_seed_hex, encode_hex},
    ids::RoomId,
};
use server::{
    Server,
    config::{Config as ServerConfig, RoomConfig, RoomPersistenceConfig},
    local_admin::{AdminCommand, AdminSender},
};

use crate::{
    app::{AppEvent, PairingEventSender},
    client_net::{
        ClientConfig, FilePolicy, PAIRING_CANCELABLE, PairingEvent, spawn_device_pair_once,
        spawn_open_pair_once, spawn_pair_once,
    },
    config::{CandidatePrivacy, DownloadTarget, EffectiveFiles},
    receive_store::DownloadStore,
    test_temp::TempDir,
};

const LOBBY: RoomId = RoomId(1);

struct Addrs {
    tcp: String,
    udp: String,
    admin: AdminSender,
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

/// Starts a server whose wire mode is plaintext. `public` also enables open
/// pairing, which is what lets a client with no pinned key be answered at all.
fn start_plaintext_server(root: &Path, public: bool) -> Addrs {
    let mut config = ServerConfig::default();
    config.network.bind.tcp = "127.0.0.1:0".parse().unwrap();
    config.network.bind.udp = "127.0.0.1:0".parse().unwrap();
    config.network.udp_probe_addr = None;
    config.network.public_addr.tcp.clear();
    config.network.public_addr.udp.clear();
    config.network.public_udp_probe_addr = None;
    config.network.p2p = false;
    config.security.server_identity_seed = dev_server_seed_hex();
    config.security.transport_encryption = false;
    config.security.public = public;
    config.storage.data_dir = Some(root.join("server").display().to_string());
    config.rooms = vec![RoomConfig {
        id: LOBBY.0,
        name: "lobby".into(),
        members: None,
        persistence: RoomPersistenceConfig::None,
        memory_limit: None,
        mls_retention_days: None,
        is_default: true,
    }];
    let server = Server::bind(config).unwrap();
    let tcp = server.tcp_local_addr().unwrap().to_string();
    let udp = server.udp_local_addr().unwrap().to_string();
    let admin = server.admin_sender();
    let worker = thread::Builder::new()
        .name("pairing-plaintext-server".into())
        .spawn(move || {
            let mut server = server;
            let _ = server.run();
        })
        .unwrap();
    Addrs {
        tcp,
        udp,
        admin,
        worker: Some(worker),
    }
}

fn client_config(tcp_addr: &str, udp_addr: &str, root: &Path, pinned: bool) -> ClientConfig {
    ClientConfig {
        tcp_addr: tcp_addr.to_string(),
        udp_addr: udp_addr.to_string(),
        udp_probe_addr: None,
        username: "Newcomer".into(),
        token: "pairing-bearer".into(),
        server_public_key: pinned.then(|| encode_hex(&dev_server_public_key())),
        data_dir: Some(root.join("client-state")),
        e2e_peer_pins: Vec::new(),
        require_transport_encryption: true,
        file_policy: FilePolicy {
            default: EffectiveFiles {
                target: DownloadTarget::Off,
                max_download_bytes: 0,
            },
            rooms: Vec::new(),
        },
        download_store: DownloadStore::new(1024),
        max_upload_bytes: 1024 * 1024,
        upload_rate_bytes: 0,
        media_transport: crate::config::MediaTransportSetting::Auto,
        p2p_enabled: false,
        candidate_privacy: CandidatePrivacy::Disabled,
        prefer_ipv6: false,
    }
}

fn read_one_frame(source: &mut TcpStream) -> Option<([u8; 4], Vec<u8>)> {
    let mut length = [0u8; 4];
    source.read_exact(&mut length).ok()?;
    let mut payload = vec![0u8; u32::from_le_bytes(length) as usize];
    source.read_exact(&mut payload).ok()?;
    Some((length, payload))
}

fn relay_one_frame(source: &mut TcpStream, destination: &mut TcpStream) {
    let (length, payload) = read_one_frame(source).expect("framed handshake message");
    destination.write_all(&length).unwrap();
    destination.write_all(&payload).unwrap();
}

/// Relays the two handshake frames and then counts what the client writes
/// afterwards. The count is the number of control messages that would have
/// carried pairing secrets in the clear.
fn spawn_handshake_proxy(server_addr: &str) -> (String, thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server_addr = server_addr.to_string();
    let worker = thread::Builder::new()
        .name("pairing-handshake-proxy".into())
        .spawn(move || {
            let (mut client, _) = listener.accept().unwrap();
            let mut server = TcpStream::connect(server_addr).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            relay_one_frame(&mut client, &mut server);
            relay_one_frame(&mut server, &mut client);
            let mut written_after_handshake = 0;
            while read_one_frame(&mut client).is_some() {
                written_after_handshake += 1;
            }
            let _ = client.shutdown(Shutdown::Both);
            let _ = server.shutdown(Shutdown::Both);
            written_after_handshake
        })
        .unwrap();
    (addr, worker)
}

fn await_event(events: &mpsc::Receiver<AppEvent>) -> PairingEvent {
    let event = events.recv_timeout(Duration::from_secs(10)).unwrap();
    let AppEvent::Pairing { event, .. } = event else {
        panic!("expected a pairing event");
    };
    event
}

/// A client-generated open-pairing recovery secret in the shape the server
/// derives a dynamic user id from: the prefix plus 32 hex-encoded bytes.
fn recovery_token() -> String {
    format!(
        "{}{}",
        rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX,
        "ab".repeat(32)
    )
}

fn device_ticket(tcp_addr: &str, udp_addr: &str) -> DeviceLinkTicket {
    DeviceLinkTicket {
        version: 1,
        pairing_secret: [7u8; rpc::crypto::KEY_LEN],
        tcp_addr: tcp_addr.to_string(),
        udp_addr: udp_addr.to_string(),
        udp_probe_addr: None,
        server_public_key: dev_server_public_key(),
    }
}

#[test]
fn invite_pairing_refuses_plaintext_before_sending_the_pairing_code() {
    let root = TempDir::new("pairing-invite-plaintext");
    let addrs = start_plaintext_server(&root, false);
    let (proxy, proxy_worker) = spawn_handshake_proxy(&addrs.tcp);
    let config = client_config(&proxy, &addrs.udp, &root, true);
    let (tx, events) = mpsc::channel();

    let worker = spawn_pair_once(
        config,
        "pairing-code".to_string(),
        PairingEventSender::for_test(tx, 1),
    )
    .unwrap();

    assert!(matches!(
        await_event(&events),
        PairingEvent::TransportEncryptionRequired
    ));
    worker.join().unwrap();
    assert_eq!(proxy_worker.join().unwrap(), 0);
}

#[test]
fn open_pairing_refuses_plaintext_before_sending_the_recovery_token() {
    let root = TempDir::new("pairing-open-plaintext");
    let addrs = start_plaintext_server(&root, true);
    let (proxy, proxy_worker) = spawn_handshake_proxy(&addrs.tcp);
    let config = client_config(&proxy, &addrs.udp, &root, false);
    let (tx, events) = mpsc::channel();

    let worker = spawn_open_pair_once(
        config,
        String::new(),
        recovery_token(),
        PairingEventSender::for_test(tx, 1),
    )
    .unwrap();

    assert!(matches!(
        await_event(&events),
        PairingEvent::TransportEncryptionRequired
    ));
    worker.join().unwrap();
    assert_eq!(proxy_worker.join().unwrap(), 0);
}

#[test]
fn device_pairing_refuses_plaintext_before_fetching_the_device_link() {
    let root = TempDir::new("pairing-device-plaintext");
    let addrs = start_plaintext_server(&root, false);
    let (proxy, proxy_worker) = spawn_handshake_proxy(&addrs.tcp);
    let config = client_config(&proxy, &addrs.udp, &root, true);
    let (tx, events) = mpsc::channel();

    let worker = spawn_device_pair_once(
        config,
        device_ticket(&proxy, &addrs.udp),
        "laptop".to_string(),
        false,
        Arc::new(AtomicU8::new(PAIRING_CANCELABLE)),
        PairingEventSender::for_test(tx, 1),
    )
    .unwrap();

    assert!(matches!(
        await_event(&events),
        PairingEvent::TransportEncryptionRequired
    ));
    worker.join().unwrap();
    assert_eq!(proxy_worker.join().unwrap(), 0);
}

#[test]
fn open_pairing_completes_over_plaintext_once_the_requirement_is_cleared() {
    let root = TempDir::new("pairing-open-consented");
    let addrs = start_plaintext_server(&root, true);
    let mut config = client_config(&addrs.tcp, &addrs.udp, &root, false);
    config.require_transport_encryption = false;
    let (tx, events) = mpsc::channel();

    let worker = spawn_open_pair_once(
        config,
        String::new(),
        recovery_token(),
        PairingEventSender::for_test(tx, 1),
    )
    .unwrap();

    let event = await_event(&events);
    worker.join().unwrap();
    let PairingEvent::OpenSucceeded {
        server_public_key, ..
    } = event
    else {
        panic!("open pairing did not succeed: {event:?}");
    };
    assert_eq!(server_public_key, encode_hex(&dev_server_public_key()));
}
