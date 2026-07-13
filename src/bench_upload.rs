//! End-to-end loopback upload benchmark.
//!
//! Spins up a real [`server::Server`] and two real [`NetworkClient`]s over
//! loopback, then times a full file relay from the uploader through the server
//! to the receiver. Because loopback has effectively infinite bandwidth, the
//! measured time is pure protocol and sequencing overhead in the client worker,
//! the crypto/framing layer, and the server relay.
//!
//! The test is `#[ignore]`d by default: it spawns threads and moves 50 MB, so
//! it is not part of the normal unit-test run. Invoke it explicitly:
//!
//! ```sh
//! cargo test --release upload_50mb_loopback -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use rpc::control::FileContentEncoding;
use rpc::crypto::dev_server_seed_hex;
use rpc::ids::{RoomId, UserId};

use crate::app::{AppEvent, NetworkEventSender};
use crate::client_net::{
    ClientConfig, FilePolicy, NetworkClient, NetworkCommand, NetworkEvent, UploadFileRequest,
};
use crate::config::{CandidatePrivacy, EffectiveFiles};

use server::Server;
use server::config::{
    Config as ServerConfig, RoomConfig, TransportModeConfig, UserConfig, hash_secret,
};

const ROOM: u32 = 1;
const UPLOAD_TOKEN: &str = "bench-uploader-token";
const RECEIVE_TOKEN: &str = "bench-receiver-token";
const LIMIT_BYTES: u64 = 256 * 1024 * 1024;

/// Concrete loopback addresses the spawned server bound to.
struct ServerAddrs {
    tcp: String,
    udp: String,
}

/// A spawned client plus its private event channel.
struct Client {
    handle: NetworkClient,
    events: mpsc::Receiver<AppEvent>,
}

fn server_config() -> ServerConfig {
    let mut config = ServerConfig::default();
    config.network.tcp_addr = "127.0.0.1:0".parse().expect("valid loopback tcp addr");
    config.network.udp_addr = Some("127.0.0.1:0".parse().expect("valid loopback udp addr"));
    config.network.udp_probe_addr = None;
    config.network.p2p_enabled = false;
    // Use the well-known dev identity so clients with `server_public_key = None`
    // pin the matching key (see `pinned_server_public_key`).
    config.security.server_identity_seed = dev_server_seed_hex();
    config.security.transport_mode = TransportModeConfig::NativeEncrypted;
    config.security.max_file_size_mb = LIMIT_BYTES / crate::config::MIB;
    config.rooms = vec![RoomConfig {
        id: ROOM,
        name: "lobby".to_string(),
        members: None,
        persistence: server::config::RoomPersistenceConfig::None,
        memory_limit: None,
        is_default: true,
    }];
    config
}

fn server_users() -> Vec<UserConfig> {
    vec![
        UserConfig {
            id: UserId(1),
            internal_reference: "uploader".to_string(),
            username: "Uploader".to_string(),
            token_hash: hash_secret(UPLOAD_TOKEN),
        },
        UserConfig {
            id: UserId(2),
            internal_reference: "receiver".to_string(),
            username: "Receiver".to_string(),
            token_hash: hash_secret(RECEIVE_TOKEN),
        },
    ]
}

fn client_config(
    addrs: &ServerAddrs,
    token: &str,
    name: &str,
    receive_dir: Option<PathBuf>,
) -> ClientConfig {
    ClientConfig {
        tcp_addr: addrs.tcp.clone(),
        udp_addr: addrs.udp.clone(),
        udp_probe_addr: None,
        username: name.to_string(),
        token: token.to_string(),
        server_public_key: None,
        e2e_identity_seed: None,
        e2e_local_user_id: None,
        e2e_peer_pins: Vec::new(),
        require_native_encryption: true,
        file_policy: FilePolicy {
            default: EffectiveFiles {
                target: match receive_dir {
                    Some(dir) => crate::config::DownloadTarget::Persistent(dir),
                    None => crate::config::DownloadTarget::Off,
                },
                max_download_bytes: LIMIT_BYTES,
            },
            rooms: Vec::new(),
        },
        download_store: crate::receive_store::DownloadStore::new(LIMIT_BYTES),
        max_upload_bytes: LIMIT_BYTES,
        upload_rate_bytes: 0,
        candidate_privacy: CandidatePrivacy::Disabled,
        p2p_enabled: false,
        prefer_ipv6: false,
    }
}

fn spawn_client(config: ClientConfig) -> Client {
    let (tx, rx) = mpsc::channel();
    let handle = NetworkClient::spawn(config, NetworkEventSender::for_test(tx))
        .expect("spawn network client");
    Client { handle, events: rx }
}

/// Blocks until a `NetworkEvent` matching `pred` arrives, panicking on timeout,
/// worker death, or a surfaced `Error`/`AuthFailed` event.
fn wait_for<F>(
    label: &str,
    rx: &mpsc::Receiver<AppEvent>,
    timeout: Duration,
    mut pred: F,
) -> NetworkEvent
where
    F: FnMut(&NetworkEvent) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("{label}: timed out waiting for network event"));
        match rx.recv_timeout(remaining) {
            Ok(AppEvent::Network(event)) => {
                match &event {
                    NetworkEvent::Error(message) => panic!("{label}: network error: {message}"),
                    NetworkEvent::AuthFailed { code, message } => {
                        panic!("{label}: auth failed ({code}): {message}")
                    }
                    NetworkEvent::WorkerStopped { reason } => {
                        panic!("{label}: worker stopped: {reason}")
                    }
                    _ => {}
                }
                if pred(&event) {
                    return event;
                }
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("{label}: timed out waiting for network event")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("{label}: client worker disconnected")
            }
        }
    }
}

fn unique_temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("chatt-bench-{}-{n}", std::process::id()))
}

fn make_payload(len: usize) -> Vec<u8> {
    let mut buffer = vec![0u8; len];
    for (index, byte) in buffer.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    buffer
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    digest.as_ref().try_into().expect("sha256 is 32 bytes")
}

#[test]
#[ignore = "spawns a live server + two clients and moves 50 MB; run explicitly"]
fn upload_50mb_loopback() {
    const PAYLOAD_BYTES: usize = 50 * 1024 * 1024;

    let mut server = Server::bind(server_config()).expect("bind server");
    server.users.users = server_users();
    let tcp = server
        .tcp_local_addr()
        .expect("server tcp addr")
        .to_string();
    let udp = server
        .udp_local_addr()
        .expect("server udp addr")
        .to_string();
    thread::Builder::new()
        .name("bench-server".to_string())
        .spawn(move || {
            // The admin channel is unused here; keep the sender alive for the
            // thread's lifetime so `run` never sees a disconnected receiver.
            let (_admin_tx, admin_rx) = mpsc::channel();
            let _ = server.run(&admin_rx);
        })
        .expect("spawn server thread");
    let addrs = ServerAddrs { tcp, udp };

    let dir = unique_temp_dir();
    let receive_dir = dir.join("recv");
    std::fs::create_dir_all(&receive_dir).expect("create receive dir");
    let source = dir.join("payload.bin");
    let payload = make_payload(PAYLOAD_BYTES);
    let expected_digest = sha256(&payload);
    std::fs::write(&source, &payload).expect("write source payload");

    let uploader = spawn_client(client_config(&addrs, UPLOAD_TOKEN, "Uploader", None));
    let receiver = spawn_client(client_config(
        &addrs,
        RECEIVE_TOKEN,
        "Receiver",
        Some(receive_dir.clone()),
    ));

    let joined = Duration::from_secs(15);
    wait_for("uploader", &uploader.events, joined, |event| {
        matches!(event, NetworkEvent::Authenticated { .. })
    });
    wait_for("receiver", &receiver.events, joined, |event| {
        matches!(event, NetworkEvent::Authenticated { .. })
    });
    uploader
        .handle
        .try_send(NetworkCommand::SetActiveRoom(RoomId(ROOM)))
        .expect("set active room");

    let start = Instant::now();
    uploader
        .handle
        .try_send(NetworkCommand::UploadFile {
            room_id: Some(RoomId(ROOM)),
            request: UploadFileRequest::new(source.clone()),
        })
        .expect("upload compressible file");

    let received = wait_for(
        "receiver",
        &receiver.events,
        Duration::from_secs(180),
        |event| matches!(event, NetworkEvent::FileReceived { .. }),
    );
    let elapsed = start.elapsed();

    let NetworkEvent::FileReceived {
        metadata,
        served_name,
        ..
    } = received
    else {
        unreachable!("predicate guarantees FileReceived")
    };

    let got = std::fs::read(receive_dir.join(&served_name)).expect("read received file");
    assert_eq!(got.len(), PAYLOAD_BYTES, "received size mismatch");
    assert_eq!(sha256(&got), expected_digest, "received content mismatch");
    assert_eq!(metadata.size, PAYLOAD_BYTES as u64);
    assert_eq!(metadata.encoding, FileContentEncoding::Zstd);

    let mib = PAYLOAD_BYTES as f64 / (1024.0 * 1024.0);
    let secs = elapsed.as_secs_f64();
    let wire_bytes = crate::client_net::last_received_file_wire_bytes();
    assert!(wire_bytes < PAYLOAD_BYTES as u64);
    println!(
        "upload_50mb_loopback: {mib:.0} logical MiB relayed in {secs:.3}s = {:.1} logical MiB/s; wire {:.3} MiB ({:.2}% of original)",
        mib / secs,
        wire_bytes as f64 / (1024.0 * 1024.0),
        wire_bytes as f64 * 100.0 / PAYLOAD_BYTES as f64
    );

    drop(got);
    drop(payload);

    let incompressible_source = dir.join("incompressible.bin");
    let mut incompressible = vec![0u8; PAYLOAD_BYTES];
    let mut state = 0x1234_5678_9abc_def0u64;
    for byte in &mut incompressible {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
    let expected_digest = sha256(&incompressible);
    std::fs::write(&incompressible_source, &incompressible).expect("write incompressible payload");
    let identity_start = Instant::now();
    uploader
        .handle
        .try_send(NetworkCommand::UploadFile {
            room_id: Some(RoomId(ROOM)),
            request: UploadFileRequest::new(incompressible_source),
        })
        .expect("upload incompressible file");
    let received = wait_for(
        "receiver identity",
        &receiver.events,
        Duration::from_secs(180),
        |event| matches!(event, NetworkEvent::FileReceived { .. }),
    );
    let identity_elapsed = identity_start.elapsed();
    let NetworkEvent::FileReceived {
        metadata,
        served_name,
        ..
    } = received
    else {
        unreachable!("predicate guarantees FileReceived")
    };
    let got = std::fs::read(receive_dir.join(&served_name)).expect("read identity file");
    assert_eq!(sha256(&got), expected_digest);
    assert_eq!(metadata.encoding, FileContentEncoding::Identity);
    let identity_wire_bytes = crate::client_net::last_received_file_wire_bytes();
    assert_eq!(identity_wire_bytes, PAYLOAD_BYTES as u64);
    println!(
        "upload_50mb_loopback identity: {mib:.0} MiB in {:.3}s = {:.1} MiB/s; wire ratio 100.00%",
        identity_elapsed.as_secs_f64(),
        mib / identity_elapsed.as_secs_f64()
    );

    let excluded_source = dir.join("excluded.png");
    let excluded = vec![b'p'; 1024 * 1024];
    std::fs::write(&excluded_source, &excluded).expect("write excluded payload");
    uploader
        .handle
        .try_send(NetworkCommand::UploadFile {
            room_id: Some(RoomId(ROOM)),
            request: UploadFileRequest::new(excluded_source),
        })
        .expect("upload excluded file");
    let received = wait_for(
        "receiver excluded",
        &receiver.events,
        Duration::from_secs(30),
        |event| matches!(event, NetworkEvent::FileReceived { .. }),
    );
    let NetworkEvent::FileReceived {
        metadata,
        served_name,
        ..
    } = received
    else {
        unreachable!("predicate guarantees FileReceived")
    };
    assert_eq!(
        std::fs::read(receive_dir.join(&served_name)).unwrap(),
        excluded
    );
    assert_eq!(metadata.encoding, FileContentEncoding::Identity);
    assert_eq!(
        crate::client_net::last_received_file_wire_bytes(),
        excluded.len() as u64
    );

    uploader.handle.stop();
    receiver.handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
