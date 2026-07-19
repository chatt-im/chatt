//! Additive, stateful live MLS scenario testing.
//!
//! This module deliberately does not share a harness with `mls_e2e_test`: the
//! enumerated matrix remains an independent regression suite. Scenario traces
//! are stable text artifacts which can be generated for a bounded duration,
//! replayed exactly, and checked into `assets/mls-e2e-regressions`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rpc::{
    control::{ChatMessage, MessageFlags, decode_device_link_ticket},
    crypto::dev_server_seed_hex,
    ids::{DeviceId, FileTransferId, MessageId, RoomId, UserId},
};
use server::{
    Server,
    config::{
        Config as ServerConfig, RoomConfig, RoomPersistenceConfig, TransportModeConfig, UserConfig,
        hash_secret,
    },
    local_admin::{AdminCommand, AdminSender},
};

use crate::{
    app::{AppEvent, NetworkEventSender, PairingEventSender},
    client_net::{
        ClientConfig, FilePolicy, NetworkClient, NetworkCommand, NetworkEvent, PAIRING_CANCELABLE,
        PairingEvent, TerminalVerb, TransferDirection, UploadFileRequest,
        fail_upload_source_after_for_test, spawn_device_pair_once,
    },
    config::{CandidatePrivacy, DownloadTarget, EffectiveFiles},
    e2e::AuthenticatedChat,
    receive_store::DownloadStore,
    room_history::apply_mutation,
    test_temp::TempDir,
};

const PUBLIC_ROOM: RoomId = RoomId(1);
const GROUP_ALL_ROOM: RoomId = RoomId(2);
const GROUP_AB_ROOM: RoomId = RoomId(3);
const TRACE_HEADER: &str = "chatt-mls-scenario-v1";
const DEFAULT_ACTION_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_FILE_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CAMPAIGN_DURATION: Duration = Duration::from_secs(5 * 60);
const DEFAULT_SHRINK_DURATION: Duration = Duration::from_secs(2 * 60);
const DEFAULT_MAX_STEPS: usize = 64;
const DEFAULT_SEED: u64 = 0x7363_656e_6172_696f;
const REGRESSION_DIR: &str = "assets/mls-e2e-regressions";
const DEFAULT_CAMPAIGN_LOG: &str = "/tmp/chatt-mls-scenarios.kvlog";

#[cfg(feature = "nightly-test-spans")]
fn init_scenario_logging(path: &Path) -> kvlog::collector::LoggerGuard {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove previous MLS scenario log {}: {error}", path.display()),
    }
    thread::add_spawn_hook(|_| {
        let parent = kvlog::SpanID::current();
        move || {
            if let Some(parent) = parent {
                kvlog::set_global_span_id(parent);
            }
        }
    });
    kvlog::collector::init_file_logger(
        path.to_str()
            .expect("MLS scenario log path must be valid UTF-8"),
    )
}

#[cfg(not(feature = "nightly-test-spans"))]
fn init_scenario_logging(_path: &Path) {
    panic!(
        "the MLS scenario campaign requires nightly-test-spans; run it with devsm exec test-mls-scenarios"
    );
}

#[cfg(feature = "nightly-test-spans")]
struct ScenarioLogSpan {
    id: u64,
    _entered: kvlog::EnteredSpan,
}

#[cfg(feature = "nightly-test-spans")]
impl ScenarioLogSpan {
    fn enter(seed: u64) -> Self {
        let raw = std::num::NonZeroU64::new(seed).unwrap_or(std::num::NonZeroU64::MIN);
        let id = kvlog::SpanID::from(raw);
        let entered = id.enter();
        kvlog::info!("MLS stateful scenario started", scenario_seed = seed);
        Self {
            id: id.as_u64(),
            _entered: entered,
        }
    }
}

#[cfg(not(feature = "nightly-test-spans"))]
struct ScenarioLogSpan {
    id: u64,
}

#[cfg(not(feature = "nightly-test-spans"))]
impl ScenarioLogSpan {
    fn enter(seed: u64) -> Self {
        Self { id: seed.max(1) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum User {
    Alice,
    Bob,
    Carol,
}

impl User {
    const ALL: [Self; 3] = [Self::Alice, Self::Bob, Self::Carol];

    fn name(self) -> &'static str {
        match self {
            Self::Alice => "alice",
            Self::Bob => "bob",
            Self::Carol => "carol",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Alice => "Alice",
            Self::Bob => "Bob",
            Self::Carol => "Carol",
        }
    }

    fn id(self) -> UserId {
        match self {
            Self::Alice => UserId(1),
            Self::Bob => UserId(2),
            Self::Carol => UserId(3),
        }
    }

    fn token(self) -> &'static str {
        match self {
            Self::Alice => "mls-scenario-alice",
            Self::Bob => "mls-scenario-bob",
            Self::Carol => "mls-scenario-carol",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "alice" => Ok(Self::Alice),
            "bob" => Ok(Self::Bob),
            "carol" => Ok(Self::Carol),
            _ => Err(format!("unknown scenario user {value:?}")),
        }
    }
}

fn device_user(label: &str) -> Result<User, String> {
    User::parse(
        label
            .split_once('.')
            .map(|(user, _)| user)
            .ok_or_else(|| format!("invalid device label {label:?}"))?,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadOutcome {
    Success,
    Fail,
    Cancel,
    Decline,
    Restart,
}

impl UploadOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Fail => "fail",
            Self::Cancel => "cancel",
            Self::Decline => "decline",
            Self::Restart => "restart",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "success" => Ok(Self::Success),
            "fail" => Ok(Self::Fail),
            "cancel" => Ok(Self::Cancel),
            "decline" => Ok(Self::Decline),
            "restart" => Ok(Self::Restart),
            _ => Err(format!("unknown upload outcome {value:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    OpenDm {
        by: String,
        peer: User,
        room: String,
    },
    View {
        device: String,
        room: String,
    },
    Send {
        device: String,
        room: String,
        event: String,
    },
    Batch {
        room: String,
        sends: Vec<(String, String)>,
    },
    Edit {
        device: String,
        room: String,
        target: String,
        event: String,
    },
    Delete {
        device: String,
        room: String,
        target: String,
        event: String,
    },
    Stop {
        device: String,
    },
    Start {
        device: String,
    },
    Restart {
        device: String,
    },
    CreateLink {
        sponsor: String,
        ticket: String,
    },
    RedeemLink {
        ticket: String,
        device: String,
    },
    CancelLink {
        sponsor: String,
        ticket: String,
    },
    Revoke {
        actor: String,
        target: String,
    },
    Upload {
        device: String,
        room: String,
        event: String,
        outcome: UploadOutcome,
    },
    RestartServerLive,
    RestartServerCold {
        order: Vec<String>,
    },
    Checkpoint,
}

impl Action {
    fn kind(&self) -> &'static str {
        match self {
            Self::OpenDm { .. } => "open-dm",
            Self::View { .. } => "view",
            Self::Send { .. } => "send",
            Self::Batch { .. } => "batch",
            Self::Edit { .. } => "edit",
            Self::Delete { .. } => "delete",
            Self::Stop { .. } => "stop",
            Self::Start { .. } => "start",
            Self::Restart { .. } => "restart",
            Self::CreateLink { .. } => "create-link",
            Self::RedeemLink { .. } => "redeem-link",
            Self::CancelLink { .. } => "cancel-link",
            Self::Revoke { .. } => "revoke",
            Self::Upload { .. } => "upload",
            Self::RestartServerLive => "server-restart-live",
            Self::RestartServerCold { .. } => "server-restart-cold",
            Self::Checkpoint => "checkpoint",
        }
    }

    fn encode(&self) -> String {
        match self {
            Self::OpenDm { by, peer, room } => {
                format!("open-dm {by} {} {room}", peer.name())
            }
            Self::View { device, room } => format!("view {device} {room}"),
            Self::Send {
                device,
                room,
                event,
            } => format!("send {device} {room} {event}"),
            Self::Batch { room, sends } => {
                let tail = sends
                    .iter()
                    .flat_map(|(device, event)| [device.as_str(), event.as_str()])
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("batch {room} {tail}")
            }
            Self::Edit {
                device,
                room,
                target,
                event,
            } => format!("edit {device} {room} {target} {event}"),
            Self::Delete {
                device,
                room,
                target,
                event,
            } => format!("delete {device} {room} {target} {event}"),
            Self::Stop { device } => format!("stop {device}"),
            Self::Start { device } => format!("start {device}"),
            Self::Restart { device } => format!("restart {device}"),
            Self::CreateLink { sponsor, ticket } => format!("create-link {sponsor} {ticket}"),
            Self::RedeemLink { ticket, device } => format!("redeem-link {ticket} {device}"),
            Self::CancelLink { sponsor, ticket } => format!("cancel-link {sponsor} {ticket}"),
            Self::Revoke { actor, target } => format!("revoke {actor} {target}"),
            Self::Upload {
                device,
                room,
                event,
                outcome,
            } => format!("upload {device} {room} {event} {}", outcome.name()),
            Self::RestartServerLive => "server-restart live".to_string(),
            Self::RestartServerCold { order } => {
                format!("server-restart cold {}", order.join(","))
            }
            Self::Checkpoint => "checkpoint".to_string(),
        }
    }

    fn decode(line: &str) -> Result<Self, String> {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        let wrong = || format!("invalid scenario action {line:?}");
        match parts.as_slice() {
            ["open-dm", by, peer, room] => Ok(Self::OpenDm {
                by: (*by).to_string(),
                peer: User::parse(peer)?,
                room: (*room).to_string(),
            }),
            ["view", device, room] => Ok(Self::View {
                device: (*device).to_string(),
                room: (*room).to_string(),
            }),
            ["send", device, room, event] => Ok(Self::Send {
                device: (*device).to_string(),
                room: (*room).to_string(),
                event: (*event).to_string(),
            }),
            ["edit", device, room, target, event] => Ok(Self::Edit {
                device: (*device).to_string(),
                room: (*room).to_string(),
                target: (*target).to_string(),
                event: (*event).to_string(),
            }),
            ["delete", device, room, target, event] => Ok(Self::Delete {
                device: (*device).to_string(),
                room: (*room).to_string(),
                target: (*target).to_string(),
                event: (*event).to_string(),
            }),
            ["stop", device] => Ok(Self::Stop {
                device: (*device).to_string(),
            }),
            ["start", device] => Ok(Self::Start {
                device: (*device).to_string(),
            }),
            ["restart", device] => Ok(Self::Restart {
                device: (*device).to_string(),
            }),
            ["create-link", sponsor, ticket] => Ok(Self::CreateLink {
                sponsor: (*sponsor).to_string(),
                ticket: (*ticket).to_string(),
            }),
            ["redeem-link", ticket, device] => Ok(Self::RedeemLink {
                ticket: (*ticket).to_string(),
                device: (*device).to_string(),
            }),
            ["cancel-link", sponsor, ticket] => Ok(Self::CancelLink {
                sponsor: (*sponsor).to_string(),
                ticket: (*ticket).to_string(),
            }),
            ["revoke", actor, target] => Ok(Self::Revoke {
                actor: (*actor).to_string(),
                target: (*target).to_string(),
            }),
            ["upload", device, room, event, outcome] => Ok(Self::Upload {
                device: (*device).to_string(),
                room: (*room).to_string(),
                event: (*event).to_string(),
                outcome: UploadOutcome::parse(outcome)?,
            }),
            ["server-restart", "live"] => Ok(Self::RestartServerLive),
            ["server-restart", "cold", order] => Ok(Self::RestartServerCold {
                order: order.split(',').map(str::to_string).collect(),
            }),
            ["checkpoint"] => Ok(Self::Checkpoint),
            ["batch", room, tail @ ..] if tail.len() >= 4 && tail.len() % 2 == 0 => {
                let sends = tail
                    .chunks_exact(2)
                    .map(|pair| (pair[0].to_string(), pair[1].to_string()))
                    .collect();
                Ok(Self::Batch {
                    room: (*room).to_string(),
                    sends,
                })
            }
            _ => Err(wrong()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Trace {
    seed: u64,
    actions: Vec<Action>,
}

impl Trace {
    fn encode(&self) -> String {
        let mut output = format!("{TRACE_HEADER}\nseed {:#018x}\n", self.seed);
        for action in &self.actions {
            output.push_str(&action.encode());
            output.push('\n');
        }
        output
    }

    fn decode(input: &str) -> Result<Self, String> {
        let mut lines = input
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        if lines.next() != Some(TRACE_HEADER) {
            return Err(format!("scenario trace must start with {TRACE_HEADER}"));
        }
        let seed_line = lines
            .next()
            .ok_or_else(|| "scenario trace is missing its seed".to_string())?;
        let seed = seed_line
            .strip_prefix("seed ")
            .ok_or_else(|| "scenario trace has an invalid seed line".to_string())?;
        let seed = parse_u64(seed, "scenario trace seed")?;
        let actions = lines.map(Action::decode).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { seed, actions })
    }

    fn read(path: &Path) -> Result<Self, String> {
        Self::decode(
            &fs::read_to_string(path)
                .map_err(|error| format!("reading {}: {error}", path.display()))?,
        )
    }
}

fn parse_u64(value: &str, label: &str) -> Result<u64, String> {
    value
        .strip_prefix("0x")
        .map(|hex| u64::from_str_radix(hex, 16))
        .unwrap_or_else(|| value.parse())
        .map_err(|error| format!("invalid {label} {value:?}: {error}"))
}

#[derive(Clone, Copy)]
struct ScenarioRng(u64);

impl ScenarioRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, len: usize) -> usize {
        self.next() as usize % len
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, self.index(index + 1));
        }
    }
}

struct ScenarioServer {
    tcp: String,
    udp: String,
    admin: AdminSender,
    worker: Option<thread::JoinHandle<()>>,
}

impl ScenarioServer {
    fn stop(mut self) {
        let _ = self.admin.send(AdminCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("scenario server thread panicked");
        }
    }
}

impl Drop for ScenarioServer {
    fn drop(&mut self) {
        let _ = self.admin.send(AdminCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("scenario server thread panicked");
        }
    }
}

fn scenario_rooms() -> Vec<RoomConfig> {
    vec![
        RoomConfig {
            id: PUBLIC_ROOM.0,
            name: "lobby".into(),
            members: None,
            persistence: RoomPersistenceConfig::None,
            memory_limit: None,
            mls_retention_days: None,
            is_default: true,
        },
        RoomConfig {
            id: GROUP_ALL_ROOM.0,
            name: "scenario all".into(),
            members: Some(vec!["1".into(), "2".into(), "3".into()]),
            persistence: RoomPersistenceConfig::None,
            memory_limit: None,
            mls_retention_days: None,
            is_default: false,
        },
        RoomConfig {
            id: GROUP_AB_ROOM.0,
            name: "scenario alice bob".into(),
            members: Some(vec!["1".into(), "2".into()]),
            persistence: RoomPersistenceConfig::None,
            memory_limit: None,
            mls_retention_days: None,
            is_default: false,
        },
    ]
}

fn scenario_users() -> Vec<UserConfig> {
    User::ALL
        .into_iter()
        .map(|user| UserConfig {
            id: user.id(),
            internal_reference: user.name().into(),
            username: user.display_name().into(),
            token_hash: hash_secret(user.token()),
        })
        .collect()
}

fn start_scenario_server(
    root: &Path,
    addresses: Option<(&str, &str)>,
) -> Result<ScenarioServer, String> {
    let mut config = ServerConfig::default();
    config.network.tcp_addr = addresses
        .map(|(tcp, _)| tcp.parse())
        .transpose()
        .map_err(|error| format!("parsing scenario TCP address: {error}"))?
        .unwrap_or_else(|| "127.0.0.1:0".parse().unwrap());
    config.network.udp_addr = Some(
        addresses
            .map(|(_, udp)| udp.parse())
            .transpose()
            .map_err(|error| format!("parsing scenario UDP address: {error}"))?
            .unwrap_or_else(|| "127.0.0.1:0".parse().unwrap()),
    );
    config.network.udp_probe_addr = None;
    config.network.public_tcp_addr.clear();
    config.network.public_udp_addr.clear();
    config.network.public_udp_probe_addr = None;
    config.network.p2p_enabled = false;
    config.security.server_identity_seed = dev_server_seed_hex();
    config.security.transport_mode = TransportModeConfig::NativeEncrypted;
    config.storage.data_dir = Some(root.join("server").display().to_string());
    config.rooms = scenario_rooms();
    let mut server = Server::bind(config).map_err(|error| error.to_string())?;
    server
        .seed_users(scenario_users())
        .map_err(|error| error.to_string())?;
    let tcp = server
        .tcp_local_addr()
        .map_err(|error| error.to_string())?
        .to_string();
    let udp = server
        .udp_local_addr()
        .map_err(|error| error.to_string())?
        .to_string();
    let admin = server.admin_sender();
    let worker = thread::Builder::new()
        .name("mls-scenario-server".into())
        .spawn(move || {
            let _ = server.run();
        })
        .map_err(|error| error.to_string())?;
    Ok(ScenarioServer {
        tcp,
        udp,
        admin,
        worker: Some(worker),
    })
}

fn scenario_client_config(
    server: &ScenarioServer,
    root: &Path,
    label: &str,
    username: &str,
    token: &str,
) -> Result<ClientConfig, String> {
    let safe = label.replace('.', "-");
    let downloads = root.join(format!("{safe}-downloads"));
    fs::create_dir_all(&downloads)
        .map_err(|error| format!("creating {}: {error}", downloads.display()))?;
    Ok(ClientConfig {
        tcp_addr: server.tcp.clone(),
        udp_addr: server.udp.clone(),
        udp_probe_addr: None,
        username: username.into(),
        token: token.into(),
        server_public_key: None,
        data_dir: Some(root.join(format!("{safe}-state"))),
        e2e_peer_pins: Vec::new(),
        require_native_encryption: true,
        file_policy: FilePolicy {
            default: EffectiveFiles {
                target: DownloadTarget::Persistent(downloads),
                max_download_bytes: 8 * 1024 * 1024,
            },
            rooms: Vec::new(),
        },
        download_store: DownloadStore::new(8 * 1024 * 1024),
        max_upload_bytes: 8 * 1024 * 1024,
        upload_rate_bytes: 0,
        p2p_enabled: false,
        candidate_privacy: CandidatePrivacy::Disabled,
        prefer_ipv6: false,
    })
}

struct LiveClient {
    handle: NetworkClient,
    events: mpsc::Receiver<AppEvent>,
    backlog: VecDeque<NetworkEvent>,
}

fn spawn_live_client(config: ClientConfig) -> Result<LiveClient, String> {
    let (sender, events) = mpsc::channel();
    let handle = NetworkClient::spawn(config, NetworkEventSender::for_test(sender))
        .map_err(|error| error.to_string())?;
    Ok(LiveClient {
        handle,
        events,
        backlog: VecDeque::new(),
    })
}

#[derive(Clone)]
struct ModelEvent {
    label: String,
    chat: ChatMessage,
}

struct RoomModel {
    id: RoomId,
    members: BTreeSet<User>,
    events: Vec<ModelEvent>,
}

struct DeviceState {
    user: User,
    config: ClientConfig,
    runtime: Option<LiveClient>,
    device_id: Option<DeviceId>,
    revoked: bool,
    room_starts: BTreeMap<String, usize>,
    seen: BTreeMap<(RoomId, MessageId), ChatMessage>,
    last_sequence: BTreeMap<RoomId, u64>,
}

struct PendingTicket {
    sponsor: String,
    secret_hash: Vec<u8>,
    pairing_string: String,
    canceled: bool,
    redeemed: bool,
}

#[derive(Clone)]
enum PendingKind {
    Text { body: String },
    Edit { target: MessageId, body: String },
    Delete { target: MessageId },
    File { name: String },
}

#[derive(Clone)]
struct PendingEvent {
    room: String,
    sender: User,
    base_index: usize,
    kind: PendingKind,
    canonical: Option<ChatMessage>,
}

impl PendingEvent {
    fn matches(&self, message: &ChatMessage) -> bool {
        match &self.kind {
            PendingKind::Text { body } => {
                message.body == *body
                    && message.target.is_none()
                    && message.file_transfer_id.is_none()
                    && message.flags == MessageFlags::default()
            }
            PendingKind::Edit { target, body } => {
                message.body == *body
                    && message.target == Some(*target)
                    && message.flags.edited()
            }
            PendingKind::Delete { target } => {
                message.body.is_empty()
                    && message.target == Some(*target)
                    && message.flags.deleted()
            }
            PendingKind::File { name } => {
                message.body == *name
                    && message.target.is_none()
                    && message.file_transfer_id.is_some()
            }
        }
    }
}

struct ScenarioRunner {
    root: TempDir,
    server: Option<ScenarioServer>,
    devices: BTreeMap<String, DeviceState>,
    rooms: BTreeMap<String, RoomModel>,
    room_names: HashMap<RoomId, String>,
    tickets: BTreeMap<String, PendingTicket>,
    pending: BTreeMap<String, PendingEvent>,
    trace: Trace,
    deadline: Option<Instant>,
    expected_errors: BTreeSet<String>,
    observed_expected_errors: BTreeSet<String>,
    recent_events: VecDeque<String>,
}

impl ScenarioRunner {
    fn new(seed: u64, deadline: Option<Instant>) -> Result<Self, String> {
        let root = TempDir::new(&format!("mls-scenario-{seed:016x}"));
        fs::create_dir_all(root.join("server"))
            .map_err(|error| format!("creating scenario server root: {error}"))?;
        let server = start_scenario_server(&root, None)?;
        let mut rooms = BTreeMap::new();
        rooms.insert(
            "group-all".to_string(),
            RoomModel {
                id: GROUP_ALL_ROOM,
                members: User::ALL.into_iter().collect(),
                events: Vec::new(),
            },
        );
        rooms.insert(
            "group-ab".to_string(),
            RoomModel {
                id: GROUP_AB_ROOM,
                members: [User::Alice, User::Bob].into_iter().collect(),
                events: Vec::new(),
            },
        );
        let room_names = rooms
            .iter()
            .map(|(name, room)| (room.id, name.clone()))
            .collect();
        let mut runner = Self {
            root,
            server: Some(server),
            devices: BTreeMap::new(),
            rooms,
            room_names,
            tickets: BTreeMap::new(),
            pending: BTreeMap::new(),
            trace: Trace {
                seed,
                actions: Vec::new(),
            },
            deadline,
            expected_errors: BTreeSet::new(),
            observed_expected_errors: BTreeSet::new(),
            recent_events: VecDeque::new(),
        };
        for user in User::ALL {
            let label = format!("{}.0", user.name());
            let config = scenario_client_config(
                runner.server.as_ref().unwrap(),
                &runner.root,
                &label,
                user.display_name(),
                user.token(),
            )?;
            let runtime = spawn_live_client(config.clone())?;
            let room_starts = runner
                .rooms
                .iter()
                .filter(|(_, room)| room.members.contains(&user))
                .map(|(name, _)| (name.clone(), 0))
                .collect();
            runner.devices.insert(
                label.clone(),
                DeviceState {
                    user,
                    config,
                    runtime: Some(runtime),
                    device_id: None,
                    revoked: false,
                    room_starts,
                    seen: BTreeMap::new(),
                    last_sequence: BTreeMap::new(),
                },
            );
            runner.wait_authenticated(&label, "initial authentication")?;
        }
        Ok(runner)
    }

    fn remaining(&self, maximum: Duration) -> Result<Duration, String> {
        match self.deadline {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .map(|remaining| remaining.min(maximum))
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| "scenario campaign deadline reached".to_string()),
            None => Ok(maximum),
        }
    }

    fn server_ref(&self) -> &ScenarioServer {
        self.server.as_ref().expect("scenario server is running")
    }

    fn record_recent(&mut self, device: &str, event: &NetworkEvent) {
        if self.recent_events.len() == 200 {
            self.recent_events.pop_front();
        }
        self.recent_events
            .push_back(format!("{device}: {}", network_event_name(event)));
    }

    fn recent_tail(&self) -> Vec<&str> {
        let mut tail = self
            .recent_events
            .iter()
            .rev()
            .take(24)
            .map(String::as_str)
            .collect::<Vec<_>>();
        tail.reverse();
        tail
    }

    fn observe_event(&mut self, device: &str, event: &NetworkEvent) -> Result<(), String> {
        self.record_recent(device, event);
        match event {
            NetworkEvent::Error(message) => {
                if !self.expected_errors.remove(device) {
                    return Err(format!("{device} emitted an unexpected error: {message}"));
                }
                self.observed_expected_errors.insert(device.to_string());
            }
            NetworkEvent::WorkerStopped { reason } => {
                return Err(format!("{device} network worker stopped: {reason}"));
            }
            NetworkEvent::NativeEncryptionRequired => {
                return Err(format!("{device} unexpectedly required native encryption"));
            }
            NetworkEvent::Chat(chat) if chat.message.message_id.0 & (1 << 63) != 0 => {
                self.observe_mls_chat(device, chat)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn observe_mls_chat(
        &mut self,
        device: &str,
        chat: &AuthenticatedChat,
    ) -> Result<(), String> {
        let message = &chat.message;
        let room_name = self
            .room_names
            .get(&message.room_id)
            .cloned()
            .ok_or_else(|| {
                format!(
                    "{device} observed MLS message in unknown room {}",
                    message.room_id.0
                )
            })?;
        let sequence = message.message_id.0 & !(1 << 63);

        let existing = self.rooms[&room_name]
            .events
            .iter()
            .enumerate()
            .find(|(_, event)| event.chat.message_id == message.message_id)
            .map(|(index, event)| (index, event.chat.clone()));
        let pending_label = self
            .pending
            .iter()
            .find(|(_, pending)| {
                pending.room == room_name
                    && pending.sender.id() == message.sender
                    && pending.matches(message)
            })
            .map(|(label, _)| label.clone());
        let (event_index, expected) = if let Some((index, expected)) = existing {
            (index, Some(expected))
        } else if let Some(label) = pending_label {
            let pending = self.pending.get_mut(&label).unwrap();
            let expected = match &pending.canonical {
                Some(expected) => Some(expected.clone()),
                None => {
                    pending.canonical = Some(message.clone());
                    None
                }
            };
            (pending.base_index, expected)
        } else {
            return Err(format!(
                "{device} observed unmodeled MLS message {} in {room_name}: {:?}",
                message.message_id.0, message.body
            ));
        };
        if let Some(expected) = expected
            && expected != *message
        {
            return Err(format!(
                "{device} observed a non-canonical copy of MLS message {} in {room_name}",
                message.message_id.0
            ));
        }

        let state = self
            .devices
            .get_mut(device)
            .ok_or_else(|| format!("unknown scenario device {device}"))?;
        if state.revoked {
            return Err(format!("revoked device {device} observed a future MLS message"));
        }
        let start = state.room_starts.get(&room_name).copied().ok_or_else(|| {
            format!("device {device} observed a message from unauthorized room {room_name}")
        })?;
        if event_index < start {
            return Err(format!(
                "device {device} observed pre-membership history event {event_index} in {room_name}; start is {start}"
            ));
        }
        if state
            .seen
            .insert((message.room_id, message.message_id), message.clone())
            .is_some()
        {
            return Err(format!(
                "device {device} emitted MLS message {} more than once across restarts",
                message.message_id.0
            ));
        }
        let previous = state.last_sequence.insert(message.room_id, sequence);
        if previous.is_some_and(|previous| previous >= sequence) {
            return Err(format!(
                "device {device} emitted room {room_name} sequence {sequence} after {previous:?}"
            ));
        }
        state
            .runtime
            .as_ref()
            .ok_or_else(|| format!("offline device {device} emitted a chat"))?
            .handle
            .try_send(NetworkCommand::AcknowledgeMlsUiDispatch {
                room_id: message.room_id,
                sequence,
            })
            .map_err(|error| format!("acknowledging {device} MLS dispatch: {error}"))?;
        Ok(())
    }

    fn wait_event<F>(
        &mut self,
        device: &str,
        label: &str,
        maximum: Duration,
        mut predicate: F,
    ) -> Result<NetworkEvent, String>
    where
        F: FnMut(&NetworkEvent) -> bool,
    {
        let backlog_match = self
            .devices
            .get(device)
            .and_then(|state| state.runtime.as_ref())
            .and_then(|runtime| runtime.backlog.iter().position(&mut predicate));
        if let Some(index) = backlog_match {
            return Ok(self
                .devices
                .get_mut(device)
                .unwrap()
                .runtime
                .as_mut()
                .unwrap()
                .backlog
                .remove(index)
                .unwrap());
        }
        let timeout = self.remaining(maximum)?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
                format!(
                    "{label}: timed out waiting on {device}; recent events: {:?}",
                    self.recent_tail()
                )
            })?;
            let received = {
                let runtime = self
                    .devices
                    .get_mut(device)
                    .and_then(|state| state.runtime.as_mut())
                    .ok_or_else(|| format!("{label}: device {device} is offline"))?;
                runtime.events.recv_timeout(remaining)
            };
            match received {
                Ok(AppEvent::Network(event)) => {
                    self.observe_event(device, &event)?;
                    if predicate(&event) {
                        return Ok(event);
                    }
                    self.devices
                        .get_mut(device)
                        .unwrap()
                        .runtime
                        .as_mut()
                        .unwrap()
                        .backlog
                        .push_back(event);
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(format!(
                        "{label}: {device} event channel failed: {error}; recent events: {:?}",
                        self.recent_tail()
                    ));
                }
            }
        }
    }

    fn wait_authenticated(&mut self, device: &str, label: &str) -> Result<DeviceId, String> {
        self.wait_event(device, label, DEFAULT_ACTION_TIMEOUT, |event| {
            matches!(event, NetworkEvent::Authenticated { .. })
        })?;
        let bound = self.wait_event(device, label, DEFAULT_ACTION_TIMEOUT, |event| {
            matches!(event, NetworkEvent::MlsDeviceBound { .. })
        })?;
        match bound {
            NetworkEvent::MlsDeviceBound { device_id } => {
                self.devices.get_mut(device).unwrap().device_id = Some(device_id);
                Ok(device_id)
            }
            _ => unreachable!(),
        }
    }

    fn send_command(&self, device: &str, command: NetworkCommand) -> Result<(), String> {
        self.devices
            .get(device)
            .and_then(|state| state.runtime.as_ref())
            .ok_or_else(|| format!("device {device} is offline"))?
            .handle
            .try_send(command)
            .map_err(|error| format!("sending command to {device}: {error}"))
    }

    fn room(&self, name: &str) -> Result<&RoomModel, String> {
        self.rooms
            .get(name)
            .ok_or_else(|| format!("unknown scenario room {name:?}"))
    }

    fn event(&self, label: &str) -> Result<(&str, &ModelEvent), String> {
        self.rooms
            .iter()
            .find_map(|(room, state)| {
                state
                    .events
                    .iter()
                    .find(|event| event.label == label)
                    .map(|event| (room.as_str(), event))
            })
            .ok_or_else(|| format!("unknown scenario event {label:?}"))
    }

    fn online_entitled(&self, room: &str, event_index: usize) -> Vec<String> {
        self.devices
            .iter()
            .filter(|(_, device)| {
                !device.revoked
                    && device.runtime.is_some()
                    && device
                        .room_starts
                        .get(room)
                        .is_some_and(|start| *start <= event_index)
            })
            .map(|(label, _)| label.clone())
            .collect()
    }

    fn begin_pending(
        &mut self,
        label: &str,
        room: &str,
        sender: User,
        kind: PendingKind,
    ) -> Result<(), String> {
        if self.event(label).is_ok() || self.pending.contains_key(label) {
            return Err(format!("duplicate scenario event label {label:?}"));
        }
        let base_index = self.room(room)?.events.len();
        self.pending.insert(
            label.to_string(),
            PendingEvent {
                room: room.to_string(),
                sender,
                base_index,
                kind,
                canonical: None,
            },
        );
        Ok(())
    }

    fn pending_matcher(&self, label: &str) -> Result<PendingEvent, String> {
        self.pending
            .get(label)
            .cloned()
            .ok_or_else(|| format!("unknown pending scenario event {label:?}"))
    }

    fn wait_pending_on(&mut self, device: &str, label: &str) -> Result<ChatMessage, String> {
        let pending = self.pending_matcher(label)?;
        let event = self.wait_event(device, label, DEFAULT_ACTION_TIMEOUT, move |event| {
            matches!(event, NetworkEvent::Chat(chat) if pending.matches(&chat.message))
        })?;
        match event {
            NetworkEvent::Chat(chat) => Ok(chat.message),
            _ => unreachable!(),
        }
    }

    fn finish_pending(&mut self, labels: &[String]) -> Result<(), String> {
        let mut completed = Vec::with_capacity(labels.len());
        for label in labels {
            let pending = self
                .pending
                .remove(label)
                .ok_or_else(|| format!("missing pending scenario event {label:?}"))?;
            let chat = pending
                .canonical
                .ok_or_else(|| format!("scenario event {label:?} was never observed"))?;
            completed.push((pending.room, label.clone(), chat));
        }
        completed.sort_by_key(|(_, _, chat)| chat.message_id.0);
        for (room, label, chat) in completed {
            self.rooms
                .get_mut(&room)
                .unwrap()
                .events
                .push(ModelEvent { label, chat });
        }
        Ok(())
    }

    fn send_text(&mut self, device: &str, room: &str, event: &str) -> Result<(), String> {
        let sender = self
            .devices
            .get(device)
            .ok_or_else(|| format!("unknown device {device}"))?
            .user;
        let room_id = self.room(room)?.id;
        let body = format!("message:{event}");
        self.begin_pending(event, room, sender, PendingKind::Text { body: body.clone() })?;
        let index = self.pending[event].base_index;
        let receivers = self.online_entitled(room, index);
        self.send_command(
            device,
            NetworkCommand::SendChat {
                room_id,
                body,
            },
        )?;
        for receiver in receivers {
            self.wait_pending_on(&receiver, event)?;
        }
        self.finish_pending(&[event.to_string()])
    }

    fn send_batch(&mut self, room: &str, sends: &[(String, String)]) -> Result<(), String> {
        if sends.len() < 2 {
            return Err("scenario batch requires at least two sends".to_string());
        }
        let room_id = self.room(room)?.id;
        let base = self.room(room)?.events.len();
        for (device, event) in sends {
            let sender = self
                .devices
                .get(device)
                .ok_or_else(|| format!("unknown device {device}"))?
                .user;
            self.begin_pending(
                event,
                room,
                sender,
                PendingKind::Text {
                    body: format!("message:{event}"),
                },
            )?;
        }
        for (device, event) in sends {
            self.send_command(
                device,
                NetworkCommand::SendChat {
                    room_id,
                    body: format!("message:{event}"),
                },
            )?;
        }
        let receivers = self.online_entitled(room, base);
        for receiver in receivers {
            for (_, event) in sends {
                self.wait_pending_on(&receiver, event)?;
            }
        }
        let labels = sends
            .iter()
            .map(|(_, event)| event.clone())
            .collect::<Vec<_>>();
        self.finish_pending(&labels)
    }

    fn mutate(
        &mut self,
        device: &str,
        room: &str,
        target: &str,
        event: &str,
        delete: bool,
    ) -> Result<(), String> {
        let (_, target_event) = self.event(target)?;
        let target_id = target_event.chat.message_id;
        let sender = self
            .devices
            .get(device)
            .ok_or_else(|| format!("unknown device {device}"))?
            .user;
        let room_id = self.room(room)?.id;
        let kind = if delete {
            PendingKind::Delete { target: target_id }
        } else {
            PendingKind::Edit {
                target: target_id,
                body: format!("edit:{event}"),
            }
        };
        self.begin_pending(event, room, sender, kind)?;
        let index = self.pending[event].base_index;
        let receivers = self.online_entitled(room, index);
        let command = if delete {
            NetworkCommand::DeleteChat {
                room_id,
                target: target_id,
            }
        } else {
            NetworkCommand::EditChat {
                room_id,
                target: target_id,
                body: format!("edit:{event}"),
            }
        };
        self.send_command(device, command)?;
        for receiver in receivers {
            self.wait_pending_on(&receiver, event)?;
        }
        self.finish_pending(&[event.to_string()])
    }

    fn view_room(&mut self, device: &str, room: &str) -> Result<(), String> {
        let room_id = self.room(room)?.id;
        self.send_command(device, NetworkCommand::SetActiveRoom(room_id))?;
        self.send_command(
            device,
            NetworkCommand::FetchHistory {
                room_id,
                before: None,
                limit: 512,
            },
        )?;
        let event = self.wait_event(device, "scenario history", DEFAULT_ACTION_TIMEOUT, |event| {
            matches!(event, NetworkEvent::HistoryChunk { room_id: observed, complete: true, .. } if *observed == room_id)
        })?;
        let NetworkEvent::HistoryChunk { messages, .. } = event else {
            unreachable!()
        };
        let state = &self.devices[device];
        let start = state
            .room_starts
            .get(room)
            .copied()
            .ok_or_else(|| format!("device {device} cannot view room {room}"))?;
        let expected = self.rooms[room].events[start..]
            .iter()
            .map(|event| event.chat.clone())
            .collect::<Vec<_>>();
        let observed = messages
            .into_iter()
            .map(|chat| chat.message)
            .collect::<Vec<_>>();
        if observed != expected {
            return Err(format!(
                "device {device} history for {room} diverged: expected {} records, observed {}",
                expected.len(),
                observed.len()
            ));
        }
        let expected_folded = fold_messages(&expected);
        let observed_folded = fold_messages(&observed);
        if observed_folded != expected_folded {
            return Err(format!("device {device} folded history for {room} diverged"));
        }
        Ok(())
    }

    fn stop_device(&mut self, device: &str) -> Result<(), String> {
        let state = self
            .devices
            .get_mut(device)
            .ok_or_else(|| format!("unknown device {device}"))?;
        if state.runtime.take().is_none() {
            return Err(format!("device {device} is already offline"));
        }
        Ok(())
    }

    fn start_device(&mut self, device: &str) -> Result<(), String> {
        let config = {
            let state = self
                .devices
                .get(device)
                .ok_or_else(|| format!("unknown device {device}"))?;
            if state.revoked {
                return Err(format!("cannot normally start revoked device {device}"));
            }
            if state.runtime.is_some() {
                return Err(format!("device {device} is already online"));
            }
            state.config.clone()
        };
        self.devices.get_mut(device).unwrap().runtime = Some(spawn_live_client(config)?);
        self.wait_authenticated(device, "scenario device start")?;
        self.catch_up_device(device)
    }

    fn restart_device(&mut self, device: &str) -> Result<(), String> {
        self.stop_device(device)?;
        self.start_device(device)
    }

    fn catch_up_device(&mut self, device: &str) -> Result<(), String> {
        let expected = {
            let state = &self.devices[device];
            let mut expected = Vec::new();
            for (room_name, start) in &state.room_starts {
                for event in &self.rooms[room_name].events[*start..] {
                    if !state
                        .seen
                        .contains_key(&(event.chat.room_id, event.chat.message_id))
                    {
                        expected.push(event.chat.clone());
                    }
                }
            }
            expected.sort_by_key(|message| (message.room_id.0, message.message_id.0));
            expected
        };
        for message in expected {
            let expected = message.clone();
            self.wait_event(device, "offline MLS catch-up", DEFAULT_ACTION_TIMEOUT, move |event| {
                matches!(event, NetworkEvent::Chat(chat) if chat.message == expected)
            })?;
        }
        Ok(())
    }

    fn open_dm(&mut self, by: &str, peer: User, room: &str) -> Result<(), String> {
        if self.rooms.contains_key(room) {
            return Err(format!("scenario room {room} already exists"));
        }
        let opener = self
            .devices
            .get(by)
            .ok_or_else(|| format!("unknown device {by}"))?
            .user;
        if opener == peer {
            return Err("cannot open a DM with the same account".to_string());
        }
        self.send_command(by, NetworkCommand::OpenDm(peer.id()))?;
        let opened = self.wait_event(by, "open scenario DM", DEFAULT_ACTION_TIMEOUT, |event| {
            matches!(event, NetworkEvent::DmOpened { peer: observed, .. } if *observed == peer.id())
        })?;
        let NetworkEvent::DmOpened { room_id, .. } = opened else {
            unreachable!()
        };
        let members = [opener, peer].into_iter().collect::<BTreeSet<_>>();
        self.rooms.insert(
            room.to_string(),
            RoomModel {
                id: room_id,
                members: members.clone(),
                events: Vec::new(),
            },
        );
        self.room_names.insert(room_id, room.to_string());
        let affected = self
            .devices
            .iter()
            .filter(|(_, device)| members.contains(&device.user) && !device.revoked)
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>();
        for label in &affected {
            self.devices
                .get_mut(label)
                .unwrap()
                .room_starts
                .insert(room.to_string(), 0);
        }
        let online_affected = affected
            .into_iter()
            .filter(|label| self.devices[label].runtime.is_some())
            .collect::<Vec<_>>();
        for label in online_affected {
            self.wait_event(&label, "DM room upsert", DEFAULT_ACTION_TIMEOUT, |event| {
                matches!(event, NetworkEvent::RoomUpserted(info) if info.room_id == room_id)
            })?;
        }
        self.send_text(by, room, &format!("__open-{room}"))
    }

    fn create_link(&mut self, sponsor: &str, ticket: &str) -> Result<(), String> {
        if self.tickets.contains_key(ticket) {
            return Err(format!("duplicate ticket label {ticket}"));
        }
        self.send_command(sponsor, NetworkCommand::CreateDeviceLink)?;
        let created = self.wait_event(
            sponsor,
            "create scenario device link",
            DEFAULT_ACTION_TIMEOUT,
            |event| matches!(event, NetworkEvent::DeviceLinkCreated { .. }),
        )?;
        let NetworkEvent::DeviceLinkCreated {
            redemption_secret_hash,
            pairing_string,
            ..
        } = created
        else {
            unreachable!()
        };
        self.tickets.insert(
            ticket.to_string(),
            PendingTicket {
                sponsor: sponsor.to_string(),
                secret_hash: redemption_secret_hash,
                pairing_string,
                canceled: false,
                redeemed: false,
            },
        );
        Ok(())
    }

    fn cancel_link(&mut self, sponsor: &str, ticket: &str) -> Result<(), String> {
        let pending = self
            .tickets
            .get(ticket)
            .ok_or_else(|| format!("unknown ticket {ticket}"))?;
        if pending.sponsor != sponsor {
            return Err(format!("ticket {ticket} belongs to {}", pending.sponsor));
        }
        if pending.canceled {
            return Err(format!("ticket {ticket} is already canceled"));
        }
        let secret_hash = pending.secret_hash.clone();
        self.send_command(
            sponsor,
            NetworkCommand::CancelDeviceLink {
                redemption_secret_hash: secret_hash,
            },
        )?;
        self.wait_event(
            sponsor,
            "cancel scenario device link",
            DEFAULT_ACTION_TIMEOUT,
            |event| matches!(event, NetworkEvent::DeviceLinkCanceled),
        )?;
        self.tickets.get_mut(ticket).unwrap().canceled = true;
        Ok(())
    }

    fn pair_from_ticket(
        &self,
        mut config: ClientConfig,
        pairing_string: &str,
        device_name: &str,
    ) -> Result<(ClientConfig, LiveClient), String> {
        let ticket = decode_device_link_ticket(pairing_string)?;
        let cancellation = Arc::new(AtomicU8::new(PAIRING_CANCELABLE));
        let (sender, receiver) = mpsc::channel();
        let worker = spawn_device_pair_once(
            config.clone(),
            ticket,
            device_name.to_string(),
            false,
            cancellation,
            PairingEventSender::for_test(sender, 1),
        )
        .map_err(|error| format!("spawning scenario device pairing: {error}"))?;
        let result = receiver
            .recv_timeout(DEFAULT_ACTION_TIMEOUT)
            .map_err(|error| format!("waiting for scenario pairing: {error}"))?;
        worker
            .join()
            .map_err(|_| "scenario pairing worker panicked".to_string())?;
        match result {
            AppEvent::Pairing {
                event:
                    PairingEvent::DeviceSucceeded {
                        token,
                        username,
                        udp_addr,
                        udp_probe_addr,
                        server_public_key,
                    },
                ..
            } => {
                config.token = token;
                config.username = username;
                config.udp_addr = udp_addr;
                config.udp_probe_addr = udp_probe_addr;
                config.server_public_key = Some(server_public_key);
                let runtime = spawn_live_client(config.clone())?;
                Ok((config, runtime))
            }
            AppEvent::Pairing {
                event: PairingEvent::DeviceFailed { message },
                ..
            } => Err(format!("device pairing failed: {message}")),
            _ => Err("unexpected scenario pairing result".to_string()),
        }
    }

    fn redeem_link(&mut self, ticket: &str, device: &str) -> Result<(), String> {
        if self.devices.contains_key(device) {
            return Err(format!("duplicate device label {device}"));
        }
        let user = device_user(device)?;
        let pending = self
            .tickets
            .get(ticket)
            .ok_or_else(|| format!("unknown ticket {ticket}"))?;
        let sponsor_user = self.devices[&pending.sponsor].user;
        if user != sponsor_user {
            return Err(format!("ticket {ticket} links {sponsor_user:?}, not {user:?}"));
        }
        let pairing_string = pending.pairing_string.clone();
        let canceled = pending.canceled;
        if pending.redeemed {
            return Err(format!("ticket {ticket} was already redeemed"));
        }
        let pair_config = scenario_client_config(
            self.server_ref(),
            &self.root,
            device,
            &format!("{} scenario linked", user.display_name()),
            "unused",
        )?;
        let result = self.pair_from_ticket(pair_config, &pairing_string, device);
        if canceled {
            if result.is_ok() {
                return Err(format!("canceled ticket {ticket} was redeemed"));
            }
            return Ok(());
        }
        let (config, runtime) = result?;
        let room_starts = self
            .rooms
            .iter()
            .filter(|(_, room)| room.members.contains(&user))
            .map(|(name, room)| (name.clone(), room.events.len()))
            .collect::<BTreeMap<_, _>>();
        self.devices.insert(
            device.to_string(),
            DeviceState {
                user,
                config,
                runtime: Some(runtime),
                device_id: None,
                revoked: false,
                room_starts,
                seen: BTreeMap::new(),
                last_sequence: BTreeMap::new(),
            },
        );
        self.wait_authenticated(device, "redeemed scenario device")?;
        let rooms = self.devices[device]
            .room_starts
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for room in rooms {
            self.send_text(device, &room, &format!("__link-{device}-{room}"))?;
        }
        self.tickets.get_mut(ticket).unwrap().redeemed = true;
        Ok(())
    }

    fn wait_revoked(&mut self, device: &str) -> Result<(), String> {
        self.wait_event(device, "scenario revocation", DEFAULT_ACTION_TIMEOUT, |event| {
            matches!(event, NetworkEvent::AuthFailed { code: 401, .. })
                || matches!(event, NetworkEvent::LocalIdentityUnavailable { message } if message.contains("revoked"))
        })?;
        Ok(())
    }

    fn revoke(&mut self, actor: &str, target: &str) -> Result<(), String> {
        let actor_user = self
            .devices
            .get(actor)
            .ok_or_else(|| format!("unknown device {actor}"))?
            .user;
        let target_state = self
            .devices
            .get(target)
            .ok_or_else(|| format!("unknown device {target}"))?;
        if actor_user != target_state.user || actor == target || target_state.revoked {
            return Err(format!("invalid scenario revocation {actor} -> {target}"));
        }
        let target_id = target_state
            .device_id
            .ok_or_else(|| format!("device {target} has never bound"))?;
        self.send_command(
            actor,
            NetworkCommand::RevokeE2eDevice {
                device_id: target_id,
            },
        )?;
        self.devices.get_mut(target).unwrap().revoked = true;
        if self.devices[target].runtime.is_some() {
            self.wait_revoked(target)?;
            self.devices.get_mut(target).unwrap().runtime.take();
        } else {
            let config = self.devices[target].config.clone();
            self.devices.get_mut(target).unwrap().runtime = Some(spawn_live_client(config)?);
            self.wait_revoked(target)?;
            self.devices.get_mut(target).unwrap().runtime.take();
        }
        let rooms = self.devices[target]
            .room_starts
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for room in rooms {
            self.send_text(
                actor,
                &room,
                &format!("__revoke-{target}-{room}-{}", self.rooms[&room].events.len()),
            )?;
        }
        Ok(())
    }

    fn wait_transfer_progress(
        &mut self,
        device: &str,
        room_id: RoomId,
        direction: TransferDirection,
    ) -> Result<FileTransferId, String> {
        let event = self.wait_event(
            device,
            "scenario transfer progress",
            DEFAULT_FILE_TIMEOUT,
            |event| {
                matches!(
                    event,
                    NetworkEvent::TransferProgress {
                        room_id: observed,
                        direction: observed_direction,
                        ..
                    } if *observed == room_id
                        && *observed_direction == direction
                )
            },
        )?;
        match event {
            NetworkEvent::TransferProgress { transfer_id, .. } => Ok(transfer_id),
            _ => unreachable!(),
        }
    }

    fn clear_transfer_backlogs(&mut self) {
        for state in self.devices.values_mut() {
            if let Some(runtime) = state.runtime.as_mut() {
                runtime.backlog.retain(|event| {
                    !matches!(
                        event,
                        NetworkEvent::TransferProgress { .. }
                            | NetworkEvent::TransferEnded { .. }
                            | NetworkEvent::TransferComplete { .. }
                            | NetworkEvent::FileReceived { .. }
                    )
                });
            }
        }
    }

    fn settle_upload(&mut self, device: &str, room: &str, event: &str) -> Result<(), String> {
        // A chat after the transfer's terminal event is a per-client FIFO barrier. It
        // lets wait_event observe and backlog any late progress/terminal notifications
        // before the next upload starts, where transfer ids may be reused by clients.
        self.send_text(device, room, &format!("__upload-{event}-settled"))?;
        self.clear_transfer_backlogs();
        Ok(())
    }

    fn wait_file_received(
        &mut self,
        device: &str,
        room_id: RoomId,
        name: &str,
        payload: &[u8],
    ) -> Result<(), String> {
        let expected_name = name.to_string();
        let event = self.wait_event(
            device,
            "scenario file receive",
            DEFAULT_FILE_TIMEOUT,
            move |event| {
                matches!(event, NetworkEvent::FileReceived { metadata, .. } if metadata.room_id == room_id && metadata.original_name == expected_name)
            },
        )?;
        let NetworkEvent::FileReceived { served_name, .. } = event else {
            unreachable!()
        };
        let safe = device.replace('.', "-");
        let path = self
            .root
            .join(format!("{safe}-downloads"))
            .join(served_name);
        let observed = fs::read(&path)
            .map_err(|error| format!("reading received file {}: {error}", path.display()))?;
        if observed != payload {
            return Err(format!("device {device} received incorrect bytes for {name}"));
        }
        Ok(())
    }

    fn upload(
        &mut self,
        device: &str,
        room: &str,
        event: &str,
        outcome: UploadOutcome,
    ) -> Result<(), String> {
        self.clear_transfer_backlogs();
        let sender = self
            .devices
            .get(device)
            .ok_or_else(|| format!("unknown device {device}"))?
            .user;
        let room_id = self.room(room)?.id;
        let name = format!("file-{event}.bin");
        let payload_seed = event
            .bytes()
            .fold(0x6669_6c65_2d65_3265_u64, |seed, byte| {
                seed.rotate_left(7) ^ u64::from(byte)
            });
        let mut payload_rng = ScenarioRng(payload_seed);
        let mut payload = vec![0u8; 256 * 1024];
        for chunk in payload.chunks_mut(8) {
            let bytes = payload_rng.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        let path = self.root.join(&name);
        fs::write(&path, &payload)
            .map_err(|error| format!("writing scenario upload {}: {error}", path.display()))?;
        if outcome != UploadOutcome::Success {
            self.send_command(device, NetworkCommand::SetUploadRate(4096))?;
        }
        if outcome == UploadOutcome::Fail {
            fail_upload_source_after_for_test(path.clone(), 4096);
            self.expected_errors.insert(device.to_string());
        }
        self.begin_pending(
            event,
            room,
            sender,
            PendingKind::File { name: name.clone() },
        )?;
        let index = self.pending[event].base_index;
        let announcement_receivers = self.online_entitled(room, index);
        self.send_command(
            device,
            NetworkCommand::UploadFile {
                room_id: Some(room_id),
                request: UploadFileRequest::new(path),
            },
        )?;
        for receiver in &announcement_receivers {
            self.wait_pending_on(receiver, event)?;
        }
        self.finish_pending(&[event.to_string()])?;

        match outcome {
            UploadOutcome::Success => {
                for receiver in announcement_receivers {
                    self.wait_file_received(&receiver, room_id, &name, &payload)?;
                }
            }
            UploadOutcome::Fail => {
                if !self.observed_expected_errors.remove(device) {
                    self.wait_event(
                        device,
                        "failed scenario upload error",
                        DEFAULT_FILE_TIMEOUT,
                        |event| matches!(event, NetworkEvent::Error(message) if message.contains("injected upload source read failure")),
                    )?;
                    self.observed_expected_errors.remove(device);
                }
                self.send_command(device, NetworkCommand::SetUploadRate(0))?;
            }
            UploadOutcome::Cancel => {
                let transfer_id = self.wait_transfer_progress(
                    device,
                    room_id,
                    TransferDirection::Outgoing,
                )?;
                self.send_command(device, NetworkCommand::CancelTransfer { transfer_id })?;
                self.wait_event(
                    device,
                    "canceled scenario upload",
                    DEFAULT_FILE_TIMEOUT,
                    |event| {
                        matches!(event, NetworkEvent::TransferEnded { transfer_id: observed, verb: TerminalVerb::Cancelled, .. } if *observed == transfer_id)
                    },
                )?;
                self.send_command(device, NetworkCommand::SetUploadRate(0))?;
            }
            UploadOutcome::Decline => {
                let recipients = announcement_receivers
                    .iter()
                    .filter(|receiver| receiver.as_str() != device)
                    .cloned()
                    .collect::<Vec<_>>();
                if recipients.is_empty() {
                    return Err("declined upload requires an online recipient".to_string());
                }
                let mut transfer_id = None;
                for receiver in recipients {
                    let observed = self.wait_transfer_progress(
                        &receiver,
                        room_id,
                        TransferDirection::Incoming,
                    )?;
                    if transfer_id.is_some_and(|expected| expected != observed) {
                        return Err("upload recipients observed different transfer ids".to_string());
                    }
                    transfer_id = Some(observed);
                    self.send_command(
                        &receiver,
                        NetworkCommand::CancelTransfer {
                            transfer_id: observed,
                        },
                    )?;
                }
                let transfer_id = transfer_id.unwrap();
                self.wait_event(
                    device,
                    "declined scenario upload",
                    DEFAULT_FILE_TIMEOUT,
                    |event| {
                        matches!(event, NetworkEvent::TransferEnded { transfer_id: observed, verb: TerminalVerb::Cancelled, reason: Some(reason), .. } if *observed == transfer_id && reason.contains("declined"))
                    },
                )?;
                self.send_command(device, NetworkCommand::SetUploadRate(0))?;
            }
            UploadOutcome::Restart => {
                self.wait_transfer_progress(
                    device,
                    room_id,
                    TransferDirection::Outgoing,
                )?;
                self.restart_device(device)?;
                for receiver in announcement_receivers
                    .into_iter()
                    .filter(|receiver| receiver != device)
                {
                    self.wait_file_received(&receiver, room_id, &name, &payload)?;
                }
            }
        }
        self.settle_upload(device, room, event)
    }

    fn restart_server_live(&mut self) -> Result<(), String> {
        let old = self.server.take().expect("scenario server exists");
        let tcp = old.tcp.clone();
        let udp = old.udp.clone();
        old.stop();
        self.server = Some(start_scenario_server(&self.root, Some((&tcp, &udp)))?);
        let online = self
            .devices
            .iter()
            .filter(|(_, device)| device.runtime.is_some() && !device.revoked)
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>();
        for device in online {
            self.wait_authenticated(&device, "live server restart")?;
            self.catch_up_device(&device)?;
        }
        Ok(())
    }

    fn restart_server_cold(&mut self, order: &[String]) -> Result<(), String> {
        let online = self
            .devices
            .iter()
            .filter(|(_, device)| device.runtime.is_some() && !device.revoked)
            .map(|(label, _)| label.clone())
            .collect::<BTreeSet<_>>();
        let requested = order.iter().cloned().collect::<BTreeSet<_>>();
        if online != requested || order.len() != requested.len() {
            return Err(format!(
                "cold restart order must name each online device once; online={online:?}, order={order:?}"
            ));
        }
        for device in order {
            self.stop_device(device)?;
        }
        let old = self.server.take().expect("scenario server exists");
        let tcp = old.tcp.clone();
        let udp = old.udp.clone();
        old.stop();
        self.server = Some(start_scenario_server(&self.root, Some((&tcp, &udp)))?);
        for device in order {
            self.start_device(device)?;
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<(), String> {
        let online = self
            .devices
            .iter()
            .filter(|(_, device)| device.runtime.is_some() && !device.revoked)
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>();
        for device in online {
            let rooms = self.devices[&device]
                .room_starts
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            for room in rooms {
                self.view_room(&device, &room)?;
            }
        }
        Ok(())
    }

    fn execute(&mut self, action: &Action) -> Result<(), String> {
        match action {
            Action::OpenDm { by, peer, room } => self.open_dm(by, *peer, room),
            Action::View { device, room } => self.view_room(device, room),
            Action::Send {
                device,
                room,
                event,
            } => self.send_text(device, room, event),
            Action::Batch { room, sends } => self.send_batch(room, sends),
            Action::Edit {
                device,
                room,
                target,
                event,
            } => self.mutate(device, room, target, event, false),
            Action::Delete {
                device,
                room,
                target,
                event,
            } => self.mutate(device, room, target, event, true),
            Action::Stop { device } => self.stop_device(device),
            Action::Start { device } => self.start_device(device),
            Action::Restart { device } => self.restart_device(device),
            Action::CreateLink { sponsor, ticket } => self.create_link(sponsor, ticket),
            Action::RedeemLink { ticket, device } => self.redeem_link(ticket, device),
            Action::CancelLink { sponsor, ticket } => self.cancel_link(sponsor, ticket),
            Action::Revoke { actor, target } => self.revoke(actor, target),
            Action::Upload {
                device,
                room,
                event,
                outcome,
            } => self.upload(device, room, event, *outcome),
            Action::RestartServerLive => self.restart_server_live(),
            Action::RestartServerCold { order } => self.restart_server_cold(order),
            Action::Checkpoint => self.checkpoint(),
        }
    }

    fn run_trace(&mut self, trace: &Trace) -> Result<(), String> {
        for (index, action) in trace.actions.iter().enumerate() {
            self.execute(action)
                .map_err(|error| format!("step {index} ({}): {error}", action.encode()))?;
            self.trace.actions.push(action.clone());
        }
        if !self.pending.is_empty() {
            return Err(format!("scenario ended with pending events: {:?}", self.pending.keys()));
        }
        self.checkpoint()
    }

    fn execute_generated(&mut self, action: Action) -> Result<(), String> {
        // Keep the attempted action in a failure trace. Without it a replay
        // would stop immediately before the operation which exposed the bug.
        self.trace.actions.push(action.clone());
        self.execute(&action)
    }

    fn write_failure(&self, output_dir: &Path, suffix: &str, error: &str) -> Result<PathBuf, String> {
        fs::create_dir_all(output_dir)
            .map_err(|io_error| format!("creating {}: {io_error}", output_dir.display()))?;
        let path = output_dir.join(format!("{:016x}-{suffix}.trace", self.trace.seed));
        fs::write(&path, self.trace.encode())
            .map_err(|io_error| format!("writing {}: {io_error}", path.display()))?;
        let events = output_dir.join(format!("{:016x}-{suffix}.events", self.trace.seed));
        let mut diagnostic = format!("failure: {error}\n");
        for event in &self.recent_events {
            diagnostic.push_str(event);
            diagnostic.push('\n');
        }
        fs::write(&events, diagnostic)
            .map_err(|io_error| format!("writing {}: {io_error}", events.display()))?;
        Ok(path)
    }
}

fn network_event_name(event: &NetworkEvent) -> &'static str {
    match event {
        NetworkEvent::Authenticated { .. } => "authenticated",
        NetworkEvent::MlsDeviceBound { .. } => "mls-device-bound",
        NetworkEvent::RoomUpserted(_) => "room-upserted",
        NetworkEvent::DmOpened { .. } => "dm-opened",
        NetworkEvent::Chat(_) => "chat",
        NetworkEvent::HistoryChunk { .. } => "history",
        NetworkEvent::DeviceLinkCreated { .. } => "device-link-created",
        NetworkEvent::DeviceLinkRedeemed { .. } => "device-link-redeemed",
        NetworkEvent::DeviceLinkCanceled => "device-link-canceled",
        NetworkEvent::FileReceived { .. } => "file-received",
        NetworkEvent::TransferProgress { .. } => "transfer-progress",
        NetworkEvent::TransferEnded { .. } => "transfer-ended",
        NetworkEvent::TransferComplete { .. } => "transfer-complete",
        NetworkEvent::ReconnectScheduled { .. } => "reconnect-scheduled",
        NetworkEvent::AuthFailed { .. } => "auth-failed",
        NetworkEvent::LocalIdentityUnavailable { .. } => "identity-unavailable",
        NetworkEvent::Error(_) => "error",
        NetworkEvent::WorkerStopped { .. } => "worker-stopped",
        _ => "other",
    }
}

fn fold_messages(raw: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut messages = raw
        .iter()
        .filter(|message| message.target.is_none())
        .cloned()
        .map(|message| (message.message_id, message))
        .collect::<BTreeMap<_, _>>();
    let mut mutations = raw
        .iter()
        .filter(|message| message.target.is_some())
        .cloned()
        .collect::<Vec<_>>();
    mutations.sort_by_key(|message| message.message_id.0);
    for mutation in mutations {
        if let Some(target) = mutation.target
            && let Some(message) = messages.get_mut(&target)
        {
            apply_mutation(message, &mutation);
        }
    }
    messages.into_values().collect()
}

#[derive(Default)]
struct GeneratorCounters {
    event: usize,
    ticket: usize,
}

#[derive(Default)]
struct Coverage {
    features: BTreeSet<String>,
    actions: BTreeMap<String, usize>,
    previous: Option<String>,
}

impl Coverage {
    fn feature(&self, runner: &ScenarioRunner, action: &Action) -> String {
        let online = runner
            .devices
            .values()
            .filter(|device| device.runtime.is_some() && !device.revoked)
            .count();
        let linked = runner.devices.len().saturating_sub(User::ALL.len());
        let previous = self.previous.as_deref().unwrap_or("start");
        format!(
            "{previous}>{}|online={online}|linked={linked}|rooms={}",
            action.kind(),
            runner.rooms.len()
        )
    }

    fn score(&self, runner: &ScenarioRunner, action: &Action) -> usize {
        usize::from(!self.features.contains(&self.feature(runner, action))) * 4
            + usize::from(!self.actions.contains_key(action.kind())) * 2
    }

    fn observe(&mut self, runner: &ScenarioRunner, action: &Action) {
        self.features.insert(self.feature(runner, action));
        *self.actions.entry(action.kind().to_string()).or_default() += 1;
        self.previous = Some(action.kind().to_string());
    }

    fn summary(&self) -> String {
        format!(
            "{} transition/topology features; action counts {:?}",
            self.features.len(),
            self.actions
        )
    }
}

fn rooms_for_online_device(runner: &ScenarioRunner, device: &str) -> Vec<String> {
    runner.devices[device]
        .room_starts
        .keys()
        .cloned()
        .collect()
}

fn next_linked_device(runner: &ScenarioRunner, user: User) -> String {
    let next = runner
        .devices
        .keys()
        .filter(|label| device_user(label).ok() == Some(user))
        .filter_map(|label| label.split_once('.').and_then(|(_, index)| index.parse::<usize>().ok()))
        .max()
        .unwrap_or(0)
        + 1;
    format!("{}.{next}", user.name())
}

fn candidate_actions(
    runner: &ScenarioRunner,
    counters: &GeneratorCounters,
    rng: &mut ScenarioRng,
) -> Vec<Action> {
    let mut candidates = Vec::new();
    let online = runner
        .devices
        .iter()
        .filter(|(_, device)| device.runtime.is_some() && !device.revoked)
        .map(|(label, _)| label.clone())
        .collect::<Vec<_>>();
    let offline = runner
        .devices
        .iter()
        .filter(|(_, device)| device.runtime.is_none() && !device.revoked)
        .map(|(label, _)| label.clone())
        .collect::<Vec<_>>();
    let event = format!("e{}", counters.event);

    if let Some(device) = online.get(rng.index(online.len().max(1))).cloned() {
        let rooms = rooms_for_online_device(runner, &device);
        if let Some(room) = rooms.get(rng.index(rooms.len().max(1))).cloned() {
            candidates.push(Action::Send {
                device: device.clone(),
                room: room.clone(),
                event: event.clone(),
            });
            candidates.push(Action::View {
                device: device.clone(),
                room: room.clone(),
            });
            let outcomes = [
                UploadOutcome::Success,
                UploadOutcome::Fail,
                UploadOutcome::Cancel,
                UploadOutcome::Decline,
                UploadOutcome::Restart,
            ];
            let outcome = outcomes[rng.index(outcomes.len())];
            if outcome != UploadOutcome::Decline
                || runner.online_entitled(&room, runner.rooms[&room].events.len()).len() > 1
            {
                candidates.push(Action::Upload {
                    device: device.clone(),
                    room: room.clone(),
                    event: event.clone(),
                    outcome,
                });
            }
        }
        candidates.push(Action::Restart {
            device: device.clone(),
        });
        if online.len() > 1 {
            candidates.push(Action::Stop {
                device: device.clone(),
            });
        }
        let user = runner.devices[&device].user;
        if runner
            .devices
            .values()
            .filter(|state| state.user == user && !state.revoked)
            .count()
            < 3
            && !runner.tickets.values().any(|ticket| {
                runner.devices[&ticket.sponsor].user == user
                    && !ticket.canceled
                    && !ticket.redeemed
            })
        {
            candidates.push(Action::CreateLink {
                sponsor: device,
                ticket: format!("t{}", counters.ticket),
            });
        }
    }

    if let Some(device) = offline.get(rng.index(offline.len().max(1))).cloned() {
        candidates.push(Action::Start { device });
    }

    for (ticket, pending) in &runner.tickets {
        if !pending.redeemed {
            let user = runner.devices[&pending.sponsor].user;
            candidates.push(Action::RedeemLink {
                ticket: ticket.clone(),
                device: next_linked_device(runner, user),
            });
            if !pending.canceled
                && runner.devices[&pending.sponsor].runtime.is_some()
            {
                candidates.push(Action::CancelLink {
                    sponsor: pending.sponsor.clone(),
                    ticket: ticket.clone(),
                });
            }
        }
    }

    for first in User::ALL {
        for second in User::ALL {
            if first >= second {
                continue;
            }
            let room = format!("dm-{}-{}", first.name(), second.name());
            if runner.rooms.contains_key(&room) {
                continue;
            }
            if let Some(by) = online
                .iter()
                .find(|device| runner.devices[*device].user == first)
            {
                candidates.push(Action::OpenDm {
                    by: by.clone(),
                    peer: second,
                    room,
                });
            }
        }
    }

    let rooms_with_events = runner
        .rooms
        .iter()
        .filter(|(_, room)| !room.events.is_empty())
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if let Some(room) = rooms_with_events
        .get(rng.index(rooms_with_events.len().max(1)))
        .cloned()
    {
        let target = &runner.rooms[&room].events[rng.index(runner.rooms[&room].events.len())];
        let senders = online
            .iter()
            .filter(|device| runner.devices[*device].room_starts.contains_key(&room))
            .cloned()
            .collect::<Vec<_>>();
        if let Some(device) = senders.get(rng.index(senders.len().max(1))).cloned() {
            candidates.push(Action::Edit {
                device: device.clone(),
                room: room.clone(),
                target: target.label.clone(),
                event: event.clone(),
            });
            candidates.push(Action::Delete {
                device,
                room,
                target: target.label.clone(),
                event: event.clone(),
            });
        }
    }

    for user in User::ALL {
        let actors = online
            .iter()
            .filter(|device| runner.devices[*device].user == user)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(actor) = actors.get(rng.index(actors.len().max(1))) {
            let targets = runner
                .devices
                .iter()
                .filter(|(label, device)| {
                    device.user == user && !device.revoked && *label != actor
                })
                .map(|(label, _)| label.clone())
                .collect::<Vec<_>>();
            let Some(target) = targets.get(rng.index(targets.len().max(1))) else {
                continue;
            };
            candidates.push(Action::Revoke {
                actor: actor.clone(),
                target: target.clone(),
            });
        }
    }

    for (room, model) in &runner.rooms {
        let senders = online
            .iter()
            .filter(|device| {
                runner.devices[*device]
                    .room_starts
                    .get(room)
                    .is_some_and(|start| *start <= model.events.len())
            })
            .cloned()
            .collect::<Vec<_>>();
        if senders.len() >= 2 {
            candidates.push(Action::Batch {
                room: room.clone(),
                sends: vec![
                    (senders[0].clone(), event.clone()),
                    (senders[1].clone(), format!("e{}", counters.event + 1)),
                ],
            });
            break;
        }
    }

    if !online.is_empty() {
        candidates.push(Action::RestartServerLive);
        let mut order = online.clone();
        rng.shuffle(&mut order);
        candidates.push(Action::RestartServerCold { order });
    }
    candidates.push(Action::Checkpoint);
    candidates
}

fn choose_action(
    runner: &ScenarioRunner,
    counters: &GeneratorCounters,
    coverage: &Coverage,
    rng: &mut ScenarioRng,
) -> Action {
    let candidates = candidate_actions(runner, counters, rng);
    let explore_randomly = rng.next().is_multiple_of(5);
    if explore_randomly {
        return candidates[rng.index(candidates.len())].clone();
    }
    let best = candidates
        .iter()
        .map(|action| coverage.score(runner, action))
        .max()
        .unwrap_or(0);
    let best = candidates
        .into_iter()
        .filter(|action| coverage.score(runner, action) == best)
        .collect::<Vec<_>>();
    best[rng.index(best.len())].clone()
}

fn advance_counters(counters: &mut GeneratorCounters, action: &Action) {
    match action {
        Action::Send { .. }
        | Action::Edit { .. }
        | Action::Delete { .. }
        | Action::Upload { .. } => counters.event += 1,
        Action::Batch { sends, .. } => counters.event += sends.len(),
        Action::CreateLink { .. } => counters.ticket += 1,
        _ => {}
    }
}

#[derive(Clone)]
struct CampaignConfig {
    duration: Duration,
    shrink_duration: Duration,
    seed: u64,
    max_steps: usize,
    jobs: usize,
    replay: Option<PathBuf>,
    output_dir: PathBuf,
    log_path: PathBuf,
}

impl CampaignConfig {
    fn from_env() -> Result<Self, String> {
        let duration = std::env::var("CHATT_MLS_SCENARIO_DURATION")
            .ok()
            .map(|value| parse_duration(&value))
            .transpose()?
            .unwrap_or(DEFAULT_CAMPAIGN_DURATION);
        let shrink_duration = std::env::var("CHATT_MLS_SCENARIO_SHRINK_DURATION")
            .ok()
            .map(|value| parse_duration(&value))
            .transpose()?
            .unwrap_or(DEFAULT_SHRINK_DURATION);
        let seed = std::env::var("CHATT_MLS_SCENARIO_SEED")
            .ok()
            .map(|value| parse_u64(&value, "scenario campaign seed"))
            .transpose()?
            .unwrap_or_else(fresh_seed);
        let max_steps = std::env::var("CHATT_MLS_SCENARIO_MAX_STEPS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|error| format!("invalid scenario max steps: {error}"))?
            .unwrap_or(DEFAULT_MAX_STEPS)
            .max(1);
        let jobs = std::env::var("CHATT_MLS_SCENARIO_JOBS")
            .ok()
            .map(|value| value.parse::<usize>())
            .transpose()
            .map_err(|error| format!("invalid scenario jobs: {error}"))?
            .unwrap_or_else(|| {
                thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1)
                    .min(4)
            })
            .max(1);
        Ok(Self {
            duration,
            shrink_duration,
            seed,
            max_steps,
            jobs,
            replay: std::env::var_os("CHATT_MLS_SCENARIO_REPLAY").map(PathBuf::from),
            output_dir: std::env::var_os("CHATT_MLS_SCENARIO_OUTPUT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp/chatt-mls-e2e")),
            log_path: std::env::var_os("CHATT_MLS_SCENARIO_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_CAMPAIGN_LOG)),
        })
    }
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 0.001)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1.0)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60.0)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3600.0)
    } else {
        (value, 1.0)
    };
    let number = number
        .parse::<f64>()
        .map_err(|error| format!("invalid scenario duration {value:?}: {error}"))?;
    if !number.is_finite() || number <= 0.0 {
        return Err(format!("scenario duration must be positive: {value:?}"));
    }
    Ok(Duration::from_secs_f64(number * multiplier))
}

fn fresh_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(DEFAULT_SEED)
        ^ u64::from(std::process::id())
}

struct CampaignFailure {
    trace: Trace,
    error: String,
    path: PathBuf,
    span: u64,
}

fn scenario_seed(campaign_seed: u64, index: u64) -> u64 {
    let mut rng = ScenarioRng(campaign_seed ^ index.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    rng.next()
}

fn run_generated_scenario(
    seed: u64,
    maximum_steps: usize,
    deadline: Instant,
    coverage: &Arc<Mutex<Coverage>>,
    output_dir: &Path,
) -> Result<(), CampaignFailure> {
    let log_span = ScenarioLogSpan::enter(seed);
    let mut runner = ScenarioRunner::new(seed, Some(deadline)).map_err(|error| CampaignFailure {
        trace: Trace {
            seed,
            actions: Vec::new(),
        },
        error,
        path: output_dir.join(format!("{seed:016x}-setup.trace")),
        span: log_span.id,
    })?;
    let mut rng = ScenarioRng(seed);
    let mut counters = GeneratorCounters::default();
    for step in 0..maximum_steps {
        if Instant::now() >= deadline {
            return Ok(());
        }
        let action = {
            let coverage = coverage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            choose_action(&runner, &counters, &coverage, &mut rng)
        };
        if let Err(error) = runner.execute_generated(action.clone()) {
            if error == "scenario campaign deadline reached" {
                return Ok(());
            }
            let error = format!("generated step {step} ({}): {error}", action.encode());
            let path = runner
                .write_failure(output_dir, "failure", &error)
                .unwrap_or_else(|_| output_dir.join(format!("{seed:016x}-failure.trace")));
            return Err(CampaignFailure {
                trace: runner.trace.clone(),
                error,
                path,
                span: log_span.id,
            });
        }
        advance_counters(&mut counters, &action);
        coverage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observe(&runner, &action);
    }
    if let Err(error) = runner.checkpoint() {
        let path = runner
            .write_failure(output_dir, "failure", &error)
            .unwrap_or_else(|_| output_dir.join(format!("{seed:016x}-failure.trace")));
        return Err(CampaignFailure {
            trace: runner.trace.clone(),
            error,
            path,
            span: log_span.id,
        });
    }
    Ok(())
}

fn failure_class(error: &str) -> &'static str {
    for (needle, class) in [
        ("timed out", "timeout"),
        ("more than once", "duplicate"),
        ("pre-membership", "history-window"),
        ("unauthorized room", "room-auth"),
        ("non-canonical", "canonical"),
        ("history for", "history"),
        ("folded history", "fold"),
        ("unexpected error", "worker-error"),
        ("network worker stopped", "worker-stop"),
        ("revoked device", "revocation"),
        ("incorrect bytes", "file-bytes"),
        ("pairing", "pairing"),
    ] {
        if error.contains(needle) {
            return class;
        }
    }
    "other"
}

fn trace_reproduces(trace: &Trace, class: &str, deadline: Instant) -> bool {
    if Instant::now() >= deadline {
        return false;
    }
    let _log_span = ScenarioLogSpan::enter(trace.seed);
    let mut runner = match ScenarioRunner::new(trace.seed, Some(deadline)) {
        Ok(runner) => runner,
        Err(error) => return failure_class(&error) == class,
    };
    runner
        .run_trace(trace)
        .is_err_and(|error| failure_class(&error) == class)
}

fn shrink_trace(trace: &Trace, class: &str, duration: Duration) -> Trace {
    let deadline = Instant::now() + duration;
    let mut best = trace.clone();
    let mut granularity = 2usize;
    while best.actions.len() >= 2 && Instant::now() < deadline {
        let chunk = best.actions.len().div_ceil(granularity);
        let mut reduced = false;
        for start in (0..best.actions.len()).step_by(chunk) {
            if Instant::now() >= deadline {
                break;
            }
            let end = (start + chunk).min(best.actions.len());
            let mut candidate = best.clone();
            candidate.actions.drain(start..end);
            if candidate.actions.is_empty() {
                continue;
            }
            if trace_reproduces(&candidate, class, deadline) {
                best = candidate;
                granularity = 2;
                reduced = true;
                break;
            }
        }
        if !reduced {
            if granularity >= best.actions.len() {
                break;
            }
            granularity = (granularity * 2).min(best.actions.len());
        }
    }
    best
}

fn run_campaign(config: CampaignConfig) -> Result<(), String> {
    eprintln!(
        "MLS scenario campaign seed={:#018x}, duration={:?}, jobs={}, max_steps={}, kvlog={}",
        config.seed,
        config.duration,
        config.jobs,
        config.max_steps,
        config.log_path.display()
    );
    if let Some(path) = &config.replay {
        let trace = Trace::read(path)?;
        let _log_span = ScenarioLogSpan::enter(trace.seed);
        let mut runner = ScenarioRunner::new(trace.seed, None)?;
        return runner
            .run_trace(&trace)
            .map_err(|error| format!("replaying {}: {error}", path.display()));
    }

    let deadline = Instant::now() + config.duration;
    let next = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let failure = Mutex::new(None::<CampaignFailure>);
    let coverage = Arc::new(Mutex::new(Coverage::default()));
    thread::scope(|scope| {
        for _ in 0..config.jobs {
            let coverage = Arc::clone(&coverage);
            let config = config.clone();
            let next = &next;
            let stop = &stop;
            let failure = &failure;
            scope.spawn(move || {
                while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let seed = scenario_seed(config.seed, index);
                    if let Err(error) = run_generated_scenario(
                        seed,
                        config.max_steps,
                        deadline,
                        &coverage,
                        &config.output_dir,
                    ) {
                        if !stop.swap(true, Ordering::SeqCst) {
                            *failure
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
                        }
                        return;
                    }
                }
            });
        }
    });

    let summary = coverage
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .summary();
    eprintln!(
        "MLS scenario campaign completed {} scenarios; {summary}",
        next.load(Ordering::Relaxed)
    );
    let Some(failure) = failure
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    else {
        return Ok(());
    };
    let class = failure_class(&failure.error);
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO)
        .min(config.shrink_duration);
    let minimized = if remaining.is_zero() {
        failure.trace.clone()
    } else {
        shrink_trace(&failure.trace, class, remaining)
    };
    let minimized_path = config
        .output_dir
        .join(format!("{:016x}-minimized.trace", minimized.seed));
    fs::create_dir_all(&config.output_dir)
        .map_err(|error| format!("creating {}: {error}", config.output_dir.display()))?;
    fs::write(&minimized_path, minimized.encode())
        .map_err(|error| format!("writing {}: {error}", minimized_path.display()))?;
    Err(format!(
        "{}; original trace {}; minimized trace {}; kvlog span {:#x} in {}; inspect with `kvlog --span {:#x} {} --json`; replay with CHATT_MLS_SCENARIO_REPLAY={} devsm exec test-mls-scenarios",
        failure.error,
        failure.path.display(),
        minimized_path.display(),
        failure.span,
        config.log_path.display(),
        failure.span,
        config.log_path.display(),
        minimized_path.display()
    ))
}

fn regression_paths() -> Result<Vec<PathBuf>, String> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(REGRESSION_DIR);
    let mut paths = fs::read_dir(&directory)
        .map_err(|error| format!("reading {}: {error}", directory.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "trace"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

#[test]
fn mls_live_stateful_scenario_regression_corpus() {
    let paths = regression_paths().expect("read MLS scenario regression corpus");
    assert!(!paths.is_empty(), "MLS scenario regression corpus is empty");
    for path in paths {
        let trace = Trace::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let mut runner = ScenarioRunner::new(trace.seed, None)
            .unwrap_or_else(|error| panic!("{} setup: {error}", path.display()));
        runner
            .run_trace(&trace)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }
}

#[test]
#[ignore = "stateful live MLS exploration; configure CHATT_MLS_SCENARIO_DURATION"]
fn mls_live_stateful_scenario_campaign() {
    let config = CampaignConfig::from_env().expect("valid MLS scenario campaign configuration");
    let _logger = init_scenario_logging(&config.log_path);
    run_campaign(config).unwrap_or_else(|error| panic!("{error}"));
}

#[test]
#[ignore = "fixed-seed smoke test for the state-aware scenario generator"]
fn mls_live_stateful_scenario_generator_smoke() {
    #[cfg(feature = "nightly-test-spans")]
    let _logger = init_scenario_logging(Path::new("/tmp/chatt-mls-scenarios-smoke.kvlog"));
    let coverage = Arc::new(Mutex::new(Coverage::default()));
    run_generated_scenario(
        DEFAULT_SEED,
        16,
        Instant::now() + Duration::from_secs(60),
        &coverage,
        Path::new("/tmp/chatt-mls-e2e-smoke"),
    )
    .map_err(|failure| format!("{}; trace {}", failure.error, failure.path.display()))
    .unwrap();
    let coverage = coverage
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(
        coverage.actions.len() >= 8,
        "fixed generator seed did not explore enough action shapes: {}",
        coverage.summary()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_format_round_trips_every_action_shape() {
        let trace = Trace {
            seed: 7,
            actions: vec![
                Action::OpenDm {
                    by: "alice.0".into(),
                    peer: User::Bob,
                    room: "dm-alice-bob".into(),
                },
                Action::View {
                    device: "alice.0".into(),
                    room: "group-all".into(),
                },
                Action::Send {
                    device: "alice.0".into(),
                    room: "group-all".into(),
                    event: "e0".into(),
                },
                Action::Batch {
                    room: "group-all".into(),
                    sends: vec![
                        ("alice.0".into(), "e1".into()),
                        ("bob.0".into(), "e2".into()),
                    ],
                },
                Action::Edit {
                    device: "alice.0".into(),
                    room: "group-all".into(),
                    target: "e0".into(),
                    event: "e3".into(),
                },
                Action::Delete {
                    device: "bob.0".into(),
                    room: "group-all".into(),
                    target: "e0".into(),
                    event: "e4".into(),
                },
                Action::Stop {
                    device: "carol.0".into(),
                },
                Action::Start {
                    device: "carol.0".into(),
                },
                Action::Restart {
                    device: "bob.0".into(),
                },
                Action::CreateLink {
                    sponsor: "alice.0".into(),
                    ticket: "t0".into(),
                },
                Action::RedeemLink {
                    ticket: "t0".into(),
                    device: "alice.1".into(),
                },
                Action::CancelLink {
                    sponsor: "alice.0".into(),
                    ticket: "t1".into(),
                },
                Action::Revoke {
                    actor: "alice.0".into(),
                    target: "alice.1".into(),
                },
                Action::Upload {
                    device: "alice.0".into(),
                    room: "group-all".into(),
                    event: "e5".into(),
                    outcome: UploadOutcome::Fail,
                },
                Action::RestartServerLive,
                Action::RestartServerCold {
                    order: vec!["bob.0".into(), "alice.0".into()],
                },
                Action::Checkpoint,
            ],
        };
        assert_eq!(Trace::decode(&trace.encode()).unwrap(), trace);
    }

    #[test]
    fn duration_parser_accepts_campaign_units() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("0s").is_err());
    }

    #[test]
    fn folded_model_ignores_wrong_author_and_keeps_delete_sticky() {
        let mut original = ChatMessage {
            message_id: MessageId(1),
            room_id: GROUP_ALL_ROOM,
            sender: UserId(1),
            sender_name: "Alice".into(),
            timestamp_ms: 1,
            body: "before".into(),
            file_transfer_id: None,
            flags: MessageFlags::default(),
            target: None,
        };
        let wrong = ChatMessage {
            message_id: MessageId(2),
            sender: UserId(2),
            body: "wrong".into(),
            flags: MessageFlags(MessageFlags::EDITED),
            target: Some(original.message_id),
            ..original.clone()
        };
        let delete = ChatMessage {
            message_id: MessageId(3),
            body: String::new(),
            flags: MessageFlags(MessageFlags::DELETED),
            target: Some(original.message_id),
            ..original.clone()
        };
        let late_edit = ChatMessage {
            message_id: MessageId(4),
            body: "too late".into(),
            flags: MessageFlags(MessageFlags::EDITED),
            target: Some(original.message_id),
            ..original.clone()
        };
        assert!(!apply_mutation(&mut original, &wrong));
        let folded = fold_messages(&[original, wrong, delete, late_edit]);
        assert!(folded[0].flags.deleted());
        assert!(folded[0].body.is_empty());
    }
}
