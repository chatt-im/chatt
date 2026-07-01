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

use rpc::crypto::dev_server_seed_hex;
use rpc::ids::{RoomId, UserId};

use crate::app::{AppEvent, EventSender};
use crate::client_net::{ClientConfig, NetworkClient, NetworkCommand, NetworkEvent};
use crate::config::CandidatePrivacy;

use server::Server;
use server::config::{Config as ServerConfig, RoomConfig, UserConfig, hash_secret};

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
    config.network.udp_addr = "127.0.0.1:0".parse().expect("valid loopback udp addr");
    config.network.udp_probe_addr = None;
    config.network.p2p_enabled = false;
    // Use the well-known dev identity so clients with `server_public_key = None`
    // pin the matching key (see `pinned_server_public_key`).
    config.security.server_identity_seed = dev_server_seed_hex();
    config.security.encryption = true;
    config.security.max_file_size_bytes = LIMIT_BYTES;
    config.rooms = vec![RoomConfig {
        id: ROOM,
        name: "lobby".to_string(),
    }];
    config.users = vec![
        UserConfig {
            id: UserId(1),
            name: "uploader".to_string(),
            display_name: "Uploader".to_string(),
            token_hash: hash_secret(UPLOAD_TOKEN),
        },
        UserConfig {
            id: UserId(2),
            name: "receiver".to_string(),
            display_name: "Receiver".to_string(),
            token_hash: hash_secret(RECEIVE_TOKEN),
        },
    ];
    config
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
        display_name: name.to_string(),
        token: token.to_string(),
        server_public_key: None,
        room_id: RoomId(ROOM),
        file_receive_dir: receive_dir,
        max_upload_bytes: LIMIT_BYTES,
        max_receive_bytes: LIMIT_BYTES,
        candidate_privacy: CandidatePrivacy::Disabled,
        prefer_ipv6: false,
    }
}

fn spawn_client(config: ClientConfig) -> Client {
    let (tx, rx) = mpsc::channel();
    let handle = NetworkClient::spawn(config, EventSender(tx)).expect("spawn network client");
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
        matches!(event, NetworkEvent::RoomJoined { .. })
    });
    wait_for("receiver", &receiver.events, joined, |event| {
        matches!(event, NetworkEvent::RoomJoined { .. })
    });

    let start = Instant::now();
    uploader
        .handle
        .send(NetworkCommand::UploadFile(source.clone()));

    let received = wait_for(
        "receiver",
        &receiver.events,
        Duration::from_secs(180),
        |event| matches!(event, NetworkEvent::FileReceived { .. }),
    );
    let elapsed = start.elapsed();

    let NetworkEvent::FileReceived { path, .. } = received else {
        unreachable!("predicate guarantees FileReceived")
    };

    let got = std::fs::read(&path).expect("read received file");
    assert_eq!(got.len(), PAYLOAD_BYTES, "received size mismatch");
    assert_eq!(sha256(&got), expected_digest, "received content mismatch");

    let mib = PAYLOAD_BYTES as f64 / (1024.0 * 1024.0);
    let secs = elapsed.as_secs_f64();
    println!(
        "upload_50mb_loopback: {mib:.0} MiB relayed uploader -> server -> receiver in {secs:.3}s = {:.1} MiB/s",
        mib / secs
    );

    uploader.handle.stop();
    receiver.handle.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
