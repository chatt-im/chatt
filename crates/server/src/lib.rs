use hashbrown::{HashMap, HashSet};
use std::{
    collections::VecDeque,
    io,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::mpsc,
    sync::{Arc, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub mod config;
mod bug_report_writer;
mod config_diagnostics;
mod event_queue;
mod history_reader;
mod identity_writer;
pub mod local_admin;
mod mls_delivery;
mod mls_service;
mod mls_store;
pub mod room_store;
mod room_state;
pub mod user_store;
mod username_registry;
mod voice_relay;

use mio::{
    Events, Interest, Poll, Token, Waker,
    net::{TcpListener, TcpStream, UdpSocket},
};
use aws_lc_rs::rand::SecureRandom;
use aws_lc_rs::signature::KeyPair;
use rpc::{
    control::{
        self, ChatMessage, ChatMutationKind, ClientControl, ERROR_AUTH_REJECTED,
        ERROR_BUG_REPORT_REJECTED, ERROR_DEVICE_LINK_UNAVAILABLE, ERROR_OPEN_PAIR_RATE_LIMITED,
        ERROR_PAIRING_INVALID_REQUEST, ERROR_PAIRING_NOT_ACTIVE, ERROR_PASSWORD_MISMATCH,
        ERROR_PASSWORD_REQUIRED, ERROR_PUBLIC_DISABLED, ERROR_TOKEN_STALE_EPOCH,
        ERROR_USERNAME_TAKEN, FileContentEncoding, FileMetadata, InviteTicket,
        MAX_BUG_REPORT_BYTES, MAX_CONTROL_PAYLOAD_BYTES, MAX_FILE_CHUNK_BYTES, MessageFlags,
        P2pCandidate, P2pKey, P2pNatKind, P2pPeerInfo, P2pRole, RoomInfo, RoomKind, ServerControl,
        UserSummary, decode_client_control, decode_client_hello, encode_invite_ticket,
        encode_server_control, encode_server_hello, max_file_wire_bytes,
    },
    crypto::{
        CHANNEL_CONTROL, CHANNEL_VIDEO, DYNAMIC_TOKEN_PREFIX, DynamicTokenClaims, KEY_LEN,
        KeyMaterial, OPEN_PAIR_RECOVERY_PREFIX, RecordProtection, SessionTransport, TransportMode,
        VideoKeyRole, derive_video_keys, dynamic_user_id_from_recovery_token, encode_hex,
        issue_dynamic_token, respond_to_client_hello, verify_dynamic_token,
    },
    evented::{
        MioReady, ReadLimit, Readiness, WriteQueue, is_interrupted_io_error, read_into_buffer,
        write_queue_to,
    },
    frame, identity as mls_identity,
    ids::{
        BugReportId, DeviceId, FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId,
    },
    media,
    recv::RecvBuffer,
    video::{self, SharedVideoFrame, VideoAck, VideoHello, VideoRecordReader, VideoRole},
};

use config::{
    Config as ServerConfig, UserConfig, hash_secret, valid_username, value_arg, verify_secret_hash,
};
use bug_report_writer::{
    BugReportWriteReply, BugReportWriteRequest, BugReportWriter,
    EnqueueError as BugReportEnqueueError,
};
use event_queue::{
    ADMIN_EVENTS, BUG_REPORT_EVENTS, EventNotifier, EventQueue, HISTORY_EVENTS, IDENTITY_EVENTS,
    MLS_EVENTS, ROOM_LOG_EVENTS, ROOM_STATE_EVENTS, VOICE_EVENTS,
};
use history_reader::{HistoryReadReply, HistoryReader};
use identity_writer::{
    EnqueueError as IdentityEnqueueError, IdentityWrite, IdentityWriteReply,
    IdentityWriteRequest, IdentityWriter,
};
use local_admin::{AdminCommand, AdminSender, AdminSocket};
use mls_delivery::MlsEventQueue;
use mls_service::{CacheState as MlsCacheState, MlsService, PutRosterError};
use room_store::{MutationKind, OpenDmResult, RoomStore};
use user_store::UserStore;
use username_registry::UsernameRegistry;
use voice_relay::{VoiceCommand, VoiceEventBatch, VoiceRelayHandle, VoiceRoute};

#[cfg(test)]
use rpc::evented::{ReadPumpOutcome, recv_datagram_with};
#[cfg(test)]
use rpc::history;

const LISTENER: Token = Token(0);
/// Wake-only token the history reader thread signals after queueing a reply.
const WAKER: Token = Token(1);
const FIRST_CLIENT: usize = 2;
/// The default config's lobby room id, used by tests asserting lobby state.
#[cfg(test)]
const DEFAULT_ROOM: RoomId = RoomId(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const SLOW_EVENT_LOOP_WORK: Duration = Duration::from_millis(20);
const EVENT_LOOP_STATS_INTERVAL: Duration = Duration::from_secs(30);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(20);
const MAX_CLIENTS: usize = 1024;
const ACCEPT_BUDGET: usize = 64;
/// Maximum distinct sockets serviced in each direction during one loop pass.
/// Per-connection byte/record budgets still apply; these caps bound aggregate
/// work before worker completions and timers get another turn. Remaining
/// tokens stay in their FIFO queues, so zero-timeout passes continue at full
/// throughput while rotating fairly across busy connections.
const CLIENT_READS_PER_PASS: usize = 64;
const CLIENT_WRITES_PER_PASS: usize = 64;
const TCP_WRITE_ATTEMPTS: usize = 32;
/// Scheduling budget for bytes read from one TCP connection per readiness pass:
/// enough for one maximum framed control record, while still preventing one
/// fast sender from monopolizing the single-threaded loop. mio events are
/// edge-triggered, so a connection cut off by the budget is queued for a
/// re-read on the next loop pass rather than waiting for a new readiness event.
/// This is also the maximum bytes requested by one TCP read syscall.
const READ_BUDGET_BYTES: usize = frame::LENGTH_PREFIX_LEN + frame::MAX_FRAME_LEN;
/// Maximum complete control frames processed for one connection in one loop
/// pass after its socket read. Bulk file chunks are already byte-budgeted by
/// `READ_BUDGET_BYTES`; this bounds tiny pipelined controls.
const CONTROL_STEPS_PER_READ: usize = 64;
/// Maximum complete video records processed for one connection in one loop
/// pass after its socket read. One record can still be large; this only bounds
/// already-buffered batches of records.
const VIDEO_RECORDS_PER_READ: usize = 8;
/// A connection that has not finished classification, handshake, and auth
/// within this bound is dropped; without it a socket that connects and sends
/// nothing occupies one of the [`MAX_CLIENTS`] slots forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// An established connection with no socket traffic for this long is dropped.
/// The client has no TCP keepalive, but from the moment it authenticates it
/// sends the server a UDP media ping every 5 s (`RTT_PROBE_INTERVAL` in
/// `client_net.rs`) which refreshes its control connection's activity, so a
/// healthy session never approaches this bound even when silent on TCP.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a gracefully closing connection may take to drain its final queued
/// frames (an auth-rejection error) before it is force-closed; a rejected peer
/// that stops reading must not pin its slot.
const CLOSE_FLUSH_GRACE: Duration = Duration::from_secs(5);
const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const MEDIA_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const RTT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5);
const RTT_STALE_AFTER: Duration = Duration::from_secs(15);
const INVITE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const DEVICE_LINK_TTL: Duration = Duration::from_secs(10 * 60);
const OPEN_PAIR_RATE_WINDOW: Duration = Duration::from_secs(60);
const OPEN_PAIR_PER_IP_LIMIT: usize = 6;
const OPEN_PAIR_GLOBAL_LIMIT: usize = 30;
/// Soft target for retained encoded history carried by one control chunk.
/// The protocol hard cap is higher; this leaves room for framing, encryption,
/// and future metadata while allowing one fetch to stream over several chunks.
const HISTORY_CHUNK_TARGET_BYTES: usize = 192 * 1024;
/// Most disk-paged history fetches one session may have queued on the reader
/// thread; a well-behaved client keeps at most one in flight per room.
const MAX_PENDING_DISK_HISTORY_FETCHES: u8 = 8;
const MAX_PENDING_HISTORY_FETCHES: usize = 32;
const HISTORY_DELIVERY_BUDGET_BYTES: usize = 384 * 1024;
const HISTORY_DELIVERY_BUDGET_CHUNKS: usize = 4;
/// Most concurrent file uploads one session may hold open. A well-behaved
/// client streams uploads one at a time, so this only bounds a client that
/// opens transfers and never finishes them.
const MAX_ACTIVE_UPLOADS_PER_SESSION: usize = 8;
const MAX_ACTIVE_BUG_REPORTS: usize = 32;
const MAX_ACTIVE_BUG_REPORTS_PER_SESSION: usize = 1;
/// Cap on a stream's fast-start ring. Keyframes reset the ring, so this only
/// bounds a pathologically long GOP rather than normal operation.
const VIDEO_RING_MAX_BYTES: usize = 8 * 1024 * 1024;
/// A subscriber whose queued bytes exceed this after a flush is too slow to keep
/// up and is dropped. It reconnects and fast-starts from the latest keyframe.
const VIDEO_SUBSCRIBER_HIGH_WATER: usize = 8 * 1024 * 1024;
const VIDEO_FANOUT_BUDGET_BYTES: usize = 8 * 1024 * 1024;
const VIDEO_FANOUT_BUDGET_RECIPIENTS: usize = 8;
const VIDEO_FANOUT_QUEUE_MAX_BYTES: usize = 32 * 1024 * 1024;
/// A control connection whose queued bytes exceed this after a flush is not
/// draining its socket and is dropped, so a stalled file-relay recipient or a
/// history-pipelining client cannot pin megabytes of server memory.
const CONTROL_WRITE_HIGH_WATER: usize = 8 * 1024 * 1024;
/// Aggregate pending controls plus sealed TCP output across control and video
/// connections. This bounds the server even when many peers stall together.
const OUTBOUND_GLOBAL_HIGH_WATER: usize = 64 * 1024 * 1024;
const CONTROL_SEAL_BUDGET_BYTES: usize = 512 * 1024;
const CONTROL_SEAL_BUDGET_RECORDS: usize = 32;
/// Relay flow control: while any recipient of a session's active upload has
/// more than this queued, the uploader's connection is parked (its socket is
/// not read and its buffered frames are not processed). TCP backpressure then
/// propagates to the uploading client, which paces its own file reads. The
/// uploader resumes through [`LoopWork`] once the recipient drains below
/// the cap, so a fast uploader can no longer push a slower recipient into the
/// [`CONTROL_WRITE_HIGH_WATER`] disconnect. One processed read batch can
/// overshoot the cap by at most [`READ_BUDGET_BYTES`] plus one chunk, far
/// under the high water.
const FILE_RELAY_WRITE_SOFT_CAP: usize = 1024 * 1024;
const AUDIO_POP_LOG_ENV: &str = "CHATT_AUDIO_POP_LOG";
const AUDIO_POP_PACKET_FLAG_OPUS_RESET: u8 = 0x01;
const AUDIO_POP_PACKET_FLAG_SILENCE_HINT: u8 = 0x02;
const AUDIO_POP_PACKET_FLAG_SILENCE_RESUME: u8 = 0x04;
const AUDIO_POP_PACKET_FLAG_MUTE: u8 = 0x08;

#[derive(Debug, Default)]
struct LoopWork {
    accept_clients: bool,
    client_reads: VecDeque<Token>,
    client_read_set: HashSet<Token>,
    client_writes: VecDeque<Token>,
    client_write_set: HashSet<Token>,
    background_work: bool,
}

struct EventLoopStats {
    window_started: Instant,
    passes: u64,
    work_micros: u64,
    max_work_micros: u64,
    slow_passes: u64,
    slow_controls: u64,
    max_control_micros: u64,
    max_events: usize,
    max_client_reads: usize,
    max_client_writes: usize,
    slow_control_maxima: HashMap<&'static str, u64>,
}

impl EventLoopStats {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            passes: 0,
            work_micros: 0,
            max_work_micros: 0,
            slow_passes: 0,
            slow_controls: 0,
            max_control_micros: 0,
            max_events: 0,
            max_client_reads: 0,
            max_client_writes: 0,
            slow_control_maxima: HashMap::new(),
        }
    }

    fn observe_control(&mut self, kind: &'static str, elapsed: Duration) {
        if elapsed < SLOW_EVENT_LOOP_WORK {
            return;
        }
        let elapsed_micros = duration_micros(elapsed);
        self.slow_controls = self.slow_controls.saturating_add(1);
        self.max_control_micros = self.max_control_micros.max(elapsed_micros);
        let maximum = self.slow_control_maxima.entry(kind).or_default();
        if elapsed_micros > *maximum {
            *maximum = elapsed_micros;
            kvlog::warn!(
                "slow server control handler",
                kind,
                elapsed_micros
            );
        }
    }

    fn observe_pass(
        &mut self,
        elapsed: Duration,
        event_count: usize,
        client_reads: usize,
        client_writes: usize,
        immediate_work_remaining: bool,
    ) {
        let elapsed_micros = duration_micros(elapsed);
        let previous_maximum = self.max_work_micros;
        self.passes = self.passes.saturating_add(1);
        self.work_micros = self.work_micros.saturating_add(elapsed_micros);
        self.max_work_micros = self.max_work_micros.max(elapsed_micros);
        self.max_events = self.max_events.max(event_count);
        self.max_client_reads = self.max_client_reads.max(client_reads);
        self.max_client_writes = self.max_client_writes.max(client_writes);
        if elapsed >= SLOW_EVENT_LOOP_WORK {
            self.slow_passes = self.slow_passes.saturating_add(1);
            if elapsed_micros > previous_maximum {
                kvlog::warn!(
                    "slow server event-loop pass",
                    elapsed_micros,
                    event_count,
                    client_reads,
                    client_writes,
                    immediate_work_remaining
                );
            }
        }
        if self.window_started.elapsed() >= EVENT_LOOP_STATS_INTERVAL {
            kvlog::info!(
                "server event-loop work summary",
                passes = self.passes,
                work_micros = self.work_micros,
                max_work_micros = self.max_work_micros,
                slow_passes = self.slow_passes,
                slow_controls = self.slow_controls,
                max_control_micros = self.max_control_micros,
                max_events = self.max_events,
                max_client_reads = self.max_client_reads,
                max_client_writes = self.max_client_writes
            );
            *self = Self::new();
        }
    }
}

fn duration_micros(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

impl LoopWork {
    #[inline]
    fn poll_timeout(&self, idle: Duration) -> Duration {
        if self.has_immediate_work() {
            Duration::ZERO
        } else {
            idle
        }
    }

    #[inline]
    fn has_immediate_work(&self) -> bool {
        self.accept_clients
            || !self.client_reads.is_empty()
            || !self.client_writes.is_empty()
            || self.background_work
    }

    #[inline]
    fn queue_client_read(&mut self, token: Token) {
        if self.client_read_set.insert(token) {
            self.client_reads.push_back(token);
        }
    }

    #[inline]
    fn queue_client_write(&mut self, token: Token) {
        if self.client_write_set.insert(token) {
            self.client_writes.push_back(token);
        }
    }

    #[inline]
    fn queue_accept_clients(&mut self) {
        self.accept_clients = true;
    }

    #[inline]
    fn set_background_work(&mut self, pending: bool) {
        self.background_work = pending;
    }

    #[inline]
    fn has_client_read(&self, token: Token) -> bool {
        self.client_read_set.contains(&token)
    }

    #[inline]
    fn take_accept_clients(&mut self) -> bool {
        let accept_clients = self.accept_clients;
        self.accept_clients = false;
        accept_clients
    }

    #[inline]
    #[cfg(test)]
    fn client_read_count(&self) -> usize {
        self.client_reads.len()
    }

    #[inline]
    fn client_read_batch_len(&self) -> usize {
        self.client_reads.len().min(CLIENT_READS_PER_PASS)
    }

    #[inline]
    fn pop_client_read(&mut self) -> Option<Token> {
        let token = self.client_reads.pop_front()?;
        self.client_read_set.remove(&token);
        Some(token)
    }

    #[inline]
    #[cfg(test)]
    fn client_write_count(&self) -> usize {
        self.client_writes.len()
    }

    #[inline]
    fn client_write_batch_len(&self) -> usize {
        self.client_writes.len().min(CLIENT_WRITES_PER_PASS)
    }

    #[inline]
    fn pop_client_write(&mut self) -> Option<Token> {
        let token = self.client_writes.pop_front()?;
        self.client_write_set.remove(&token);
        Some(token)
    }

    #[cfg(test)]
    fn queued_client_reads(&self) -> Vec<Token> {
        self.client_reads.iter().copied().collect()
    }

    #[cfg(test)]
    fn queued_client_writes(&self) -> Vec<Token> {
        self.client_writes.iter().copied().collect()
    }
}

/// Runs the server command-line entry point: parses `std::env::args`, handles
/// the `invite` / `init-config` / `serve` subcommands, and drives the event
/// loop for `serve`.
pub fn run_cli() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    // `--logfile PATH` (or CHATT_LOGFILE) writes the same kvlog stream the
    // client `--logfile` produces, so server and client traces can be analyzed
    // together. Without it, logging stays on the env-configured collector.
    let logfile = value_arg(&args, "--logfile").or_else(|| std::env::var("CHATT_LOGFILE").ok());
    let _logger = match logfile {
        Some(logfile) => kvlog::collector::init_file_logger(&logfile),
        None => kvlog::spawn_collector_from_env(Some("chatt-server"), false),
    };

    let command = positional_args(&args);
    let config = match command.as_slice() {
        ["invite", user] if !user.trim().is_empty() => {
            let join_string = local_admin::send_invite(user).map_err(invalid_config)?;
            println!("{join_string}");
            return Ok(());
        }
        ["mls", "storage-status"] => {
            println!("{}", local_admin::mls_storage_status().map_err(invalid_config)?);
            return Ok(());
        }
        ["mls", "compact"] => {
            println!("{}", local_admin::mls_compact().map_err(invalid_config)?);
            return Ok(());
        }
        ["init-config", path] if !path.trim().is_empty() => {
            config::write_generated_template(Path::new(path)).map_err(invalid_config)?;
            println!("wrote chatt server config template to {path}");
            return Ok(());
        }
        ["serve", path] if !path.trim().is_empty() => {
            ServerConfig::load(Path::new(path)).map_err(invalid_config)?
        }
        _ => return Err(invalid_config(server_usage()).into()),
    };
    let server_public_key = config.server_public_key_hex().map_err(invalid_config)?;
    let udp_probe_addr = config.network.udp_probe_addr;
    let udp_probe_label = udp_probe_addr
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "disabled".to_string());
    kvlog::info!(
        "server starting",
        tcp_addr = %config.network.tcp_addr,
        udp_addr = %config.network.udp_addr(),
        udp_probe_addr = udp_probe_label.as_str(),
        server_public_key = server_public_key.as_str(),
        transport_mode = config.transport_mode().as_str(),
        p2p_enabled = config.network.p2p_enabled
    );
    let tcp_addr = config.network.tcp_addr;
    let udp_addr = config.network.udp_addr();
    let public_tcp_addr = config.network.public_tcp_addr.clone();
    let public_udp_addr = config.network.public_udp_addr.clone();
    let public_udp_probe_addr = config
        .network
        .public_udp_probe_addr
        .clone()
        .unwrap_or_else(|| "disabled".to_string());
    let p2p_enabled = config.network.p2p_enabled;
    let mut server = Server::bind(config)?;
    let admin_socket = AdminSocket::spawn(server.admin_sender()).map_err(invalid_config)?;
    if p2p_enabled && udp_probe_addr.is_some() {
        println!(
            "chatt server listening on tcp {tcp_addr}, udp {udp_addr}, probe {udp_probe_label}"
        );
    } else if p2p_enabled {
        println!("chatt server listening on tcp {tcp_addr}, udp {udp_addr}");
    } else {
        println!("chatt server listening on tcp {tcp_addr}, udp {udp_addr} (P2P disabled)");
    }
    println!(
        "chatt invite endpoints: tcp {public_tcp_addr}, udp {public_udp_addr}, probe {public_udp_probe_addr}"
    );
    println!("chatt server public key: {server_public_key}");
    println!(
        "chatt transport mode: {}",
        server.config.transport_mode().as_str()
    );
    if server.config.transport_mode() == rpc::crypto::TransportMode::ExternalSecureLink {
        println!("chatt is relying on the outer secure link for wire security; P2P disabled");
    }
    println!(
        "chatt P2P support: {}",
        if server.config.network.p2p_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!(
        "chatt server control socket: {}",
        admin_socket.path().display()
    );
    kvlog::info!(
        "server listening",
        tcp_addr = %tcp_addr,
        udp_addr = %udp_addr,
        udp_probe_addr = udp_probe_label.as_str(),
        transport_mode = server.config.transport_mode().as_str(),
        p2p_enabled = server.config.network.p2p_enabled
    );
    let _admin_socket = admin_socket;
    server.run()
}

pub struct Server {
    config: ServerConfig,
    /// The server-managed user registry, persisted under the data dir. Public
    /// so harnesses (`bench_upload`) can seed users directly.
    pub users: UserStore,
    /// Server-wide username uniqueness index, backed by `usernames.log.bin` for
    /// dynamic users and seeded from `users` for explicit users.
    usernames: UsernameRegistry,
    mls: MlsService,
    mls_worker: MlsWorker,
    mls_events: Arc<MlsEventQueue>,
    /// Control connections waiting for a durability-sensitive MLS operation.
    /// Their already-buffered later frames remain parked until the ordered
    /// worker reply is applied.
    pending_mls: HashSet<Token>,
    pending_identity: Option<(Token, PendingIdentity)>,
    /// Authentication controls parked behind the single ordered identity
    /// transaction. Preparing multiple identity snapshots concurrently would
    /// let later completions overwrite claims made by earlier ones.
    deferred_identity_controls: VecDeque<(Token, ClientControl)>,
    deferred_identity_tokens: HashSet<Token>,
    identity_writer: IdentityWriter,
    identity_events: Arc<EventQueue<IdentityWriteReply>>,
    event_notifier: Arc<EventNotifier>,
    admin_events: Arc<EventQueue<AdminCommand>>,
    poll: Poll,
    listener: TcpListener,
    voice_relay: VoiceRelayHandle,
    voice_events: VoiceEventBatch,
    clients: HashMap<Token, ClientConn>,
    pending_controls: HashMap<Token, VecDeque<PendingControl>>,
    pending_control_bytes: HashMap<Token, usize>,
    pending_control_total_bytes: usize,
    /// Bytes already sealed into all TCP connection write queues.
    write_queue_total_bytes: usize,
    control_send_tokens: VecDeque<Token>,
    control_send_set: HashSet<Token>,
    sessions: HashMap<SessionId, Session>,
    /// Control-plane reservation of live media route ids. The voice thread has
    /// the independently owned map used for actual UDP demultiplexing.
    reserved_media_routes: HashMap<u32, SessionId>,
    peer_links: HashMap<(SessionId, SessionId), PeerLink>,
    rooms: HashMap<RoomId, RoomState>,
    streams: HashMap<StreamId, VideoStream>,
    video_fanouts: VecDeque<VideoFanout>,
    video_fanout_bytes: usize,
    next_token: usize,
    next_session: u64,
    next_stream: u32,
    next_connection_id: u64,
    accept_retry_at: Option<Instant>,
    next_file_transfer: u64,
    active_uploads: HashMap<(SessionId, FileTransferId), ServerUpload>,
    active_bug_reports: HashMap<(SessionId, BugReportId), ServerBugReport>,
    pending_bug_reports: HashSet<(SessionId, BugReportId)>,
    bug_report_writer: BugReportWriter,
    bug_report_events: Arc<EventQueue<BugReportWriteReply>>,
    reserved_file_names: HashSet<String>,
    default_room: RoomId,
    store: RoomStore,
    file_size_limit_bytes: u64,
    invites: HashMap<String, InviteState>,
    pending_dm_waiters: HashMap<u64, Vec<(SessionId, UserId)>>,
    device_links: HashMap<Vec<u8>, DeviceLinkState>,
    open_pair_global_allocations: VecDeque<Instant>,
    open_pair_ip_allocations: HashMap<IpAddr, VecDeque<Instant>>,
    rng: aws_lc_rs::rand::SystemRandom,
    server_key_pair: aws_lc_rs::signature::Ed25519KeyPair,
    next_media_sweep_at: Option<Instant>,
    next_rtt_snapshot_at: Instant,
    next_idle_sweep_at: Instant,
    next_mls_cleanup_at: Instant,
    next_mls_compaction_at: Instant,
    mls_cleanup_pending: bool,
    mls_compaction_pending: bool,
    /// Local work that must run before the loop parks in `poll`. Producers
    /// register here instead of teaching the core loop each local no-sleep
    /// condition.
    loop_work: LoopWork,
    loop_stats: EventLoopStats,
    history_reader: HistoryReader,
    history_events: Arc<EventQueue<HistoryReadReply>>,
    history_deliveries: VecDeque<HistoryDelivery>,
    pending_history_fetches: usize,
    video_copy_stats: VideoCopyStats,
}

struct PendingEstablish {
    user: UserConfig,
    receive_files: bool,
    file_receive_limit_bytes: u64,
    announce: bool,
    issued_token: Option<IssuedSessionToken>,
    bootstrap_credential_hash: Option<String>,
}

struct HistoryDelivery {
    session_id: SessionId,
    token: Token,
    room_id: RoomId,
    source: HistoryDeliverySource,
}

struct VideoFanout {
    stream_id: StreamId,
    data: SharedVideoFrame,
    subscribers: Vec<Token>,
    next_subscriber: usize,
    charged_bytes: usize,
}

struct PendingControl {
    kind: &'static str,
    payload: Arc<[u8]>,
}

enum HistoryDeliverySource {
    Resident {
        plan: room_store::HistoryFetchPlan,
        next_chunk: usize,
    },
    Encoded(VecDeque<Vec<u8>>),
}

enum PendingIdentity {
    DynamicAuthentication {
        username: String,
        establish: PendingEstablish,
    },
    ExplicitRename {
        users: Vec<UserConfig>,
        updated: PendingEstablish,
        fallback: PendingEstablish,
    },
    InvitePairing {
        invite_name: String,
        users: Vec<UserConfig>,
        establish: PendingEstablish,
    },
    OpenPairing {
        username: String,
        establish: PendingEstablish,
    },
}

enum MlsReply {
    RosterStored {
        token: Token,
        session_id: SessionId,
        user_id: UserId,
        initial: bool,
        roster: mls_identity::SignedDeviceRoster,
        result: Result<(mls_identity::RosterCheckpoint, MlsCacheState), PutRosterError>,
    },
    RoomCreated {
        token: Token,
        room_id: RoomId,
        result: Result<RoomCreationReply, String>,
    },
    CommitSubmitted {
        token: Token,
        room_id: RoomId,
        result: Result<(
            rpc::mls::MlsCommitOutcome,
            Option<mls_store::RoomRecord>,
            Option<(
                mls_store::EventBatch,
                Vec<(DeviceId, Vec<rpc::mls::MlsWelcome>, u64)>,
            )>,
        ), String>,
    },
    DeviceLinkRedeemed {
        token: Token,
        secret_hash: Vec<u8>,
        user_id: UserId,
        username: String,
        device_id: DeviceId,
        device_name: String,
        credential_hash: String,
        attempt_id: rpc::ids::PairAttemptId,
        bearer_token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
        result: Result<MlsCacheState, String>,
    },
    ApplicationSubmitted {
        token: Token,
        room_id: RoomId,
        event_id: rpc::ids::EventId,
        result: Result<(
            rpc::mls::MlsSubmitOutcome,
            Option<mls_store::EventBatch>,
        ), String>,
    },
    Compacted {
        admin_reply: Option<mpsc::Sender<Result<String, String>>>,
        result: Result<Option<(bool, Option<u64>, Option<u64>)>, String>,
        started: Instant,
    },
    Cleanup {
        interval: Duration,
        result: Result<mls_store::CleanupReport, String>,
    },
    StorageStatus {
        reply: mpsc::Sender<Result<String, String>>,
        result: Result<String, String>,
    },
    KeyPackagesPublished {
        token: Token,
        device_id: DeviceId,
        result: Result<(u16, Vec<Vec<u8>>), String>,
    },
    KeyPackageTaken {
        token: Token,
        device_id: DeviceId,
        result: Result<(Option<Vec<u8>>, u16, Vec<Vec<u8>>), String>,
    },
}

enum RoomCreationReply {
    Created {
        epoch: u64,
        room: mls_store::RoomRecord,
        deliveries: Vec<(DeviceId, Vec<rpc::mls::MlsWelcome>, u64)>,
    },
    Existing {
        room: mls_store::RoomRecord,
    },
}

/// Serializes authoritative MLS mutations and all durable storage I/O away
/// from the mio loop. Readers use the shared in-memory store directly.
struct MlsWorker {
    requests: Option<mpsc::Sender<MlsWriteRequest>>,
    thread: Option<thread::JoinHandle<()>>,
    pending_acknowledgements: Arc<std::sync::Mutex<HashSet<(RoomId, DeviceId)>>>,
    pending_welcome_acknowledgements: Arc<std::sync::Mutex<HashSet<DeviceId>>>,
}

enum MlsWriteRequest {
    PutRoster {
        token: Token,
        session_id: SessionId,
        user_id: UserId,
        expected: Option<mls_identity::RosterCheckpoint>,
        roster: mls_identity::SignedDeviceRoster,
        bootstrap_credential_hash: Option<String>,
    },
    CreateRoom {
        token: Token,
        creator: rpc::ids::AccountId,
        creator_client_id: Vec<u8>,
        descriptor: rpc::mls::EncryptedRoomDescriptor,
        checkpoints: Vec<mls_identity::RosterCheckpoint>,
        bundle: rpc::mls::MlsCommitBundle,
        welcome_devices: Vec<DeviceId>,
    },
    SubmitCommit {
        token: Token,
        room_id: RoomId,
        device_id: DeviceId,
        committer_client_id: Vec<u8>,
        expected_epoch: u64,
        bundle: rpc::mls::MlsCommitBundle,
        welcome_devices: Vec<DeviceId>,
    },
    RedeemDeviceLink {
        token: Token,
        secret_hash: Vec<u8>,
        user_id: UserId,
        username: String,
        expected: mls_identity::RosterCheckpoint,
        roster: mls_identity::SignedDeviceRoster,
        device_id: DeviceId,
        device_name: String,
        credential_hash: String,
        attempt_id: rpc::ids::PairAttemptId,
        packages: Vec<rpc::mls::PublishedKeyPackage>,
        bearer_token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    SubmitApplication {
        token: Token,
        room_id: RoomId,
        device_id: DeviceId,
        epoch: u64,
        event_id: rpc::ids::EventId,
        ciphertext: Vec<u8>,
    },
    Compact {
        scheduled: bool,
        minimum_fragmented_bytes: u64,
        minimum_fragmented_percent: u8,
        admin_reply: Option<mpsc::Sender<Result<String, String>>>,
        started: Instant,
    },
    Cleanup {
        batch_events: usize,
        interval: Duration,
    },
    StorageStatus {
        reply: mpsc::Sender<Result<String, String>>,
    },
    PublishKeyPackages {
        token: Token,
        user_id: UserId,
        device_id: DeviceId,
        packages: Vec<rpc::mls::PublishedKeyPackage>,
    },
    TakeKeyPackage {
        token: Token,
        device_id: DeviceId,
    },
    PersistAcknowledgement {
        room_id: RoomId,
        device_id: DeviceId,
    },
    PersistWelcomeAcknowledgement {
        device_id: DeviceId,
    },
}

impl MlsWorker {
    fn spawn(mut service: MlsService, events: Arc<MlsEventQueue>) -> Self {
        let (requests, receiver) = mpsc::channel();
        let pending_acknowledgements = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let pending_welcome_acknowledgements = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let worker_pending_acknowledgements = Arc::clone(&pending_acknowledgements);
        let worker_pending_welcome_acknowledgements =
            Arc::clone(&pending_welcome_acknowledgements);
        let thread = thread::Builder::new()
            .name("chatt-mls-durability".to_string())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    Self::process(
                        &mut service,
                        &events,
                        &worker_pending_acknowledgements,
                        &worker_pending_welcome_acknowledgements,
                        request,
                    );
                }
            })
            .expect("failed to spawn MLS durability worker");
        Self {
            requests: Some(requests),
            thread: Some(thread),
            pending_acknowledgements,
            pending_welcome_acknowledgements,
        }
    }

    fn enqueue_typed(&self, request: MlsWriteRequest) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|requests| requests.send(request).is_ok())
    }

    fn persist_acknowledgement(&self, room_id: RoomId, device_id: DeviceId) -> bool {
        let key = (room_id, device_id);
        if !self.pending_acknowledgements.lock().unwrap().insert(key) {
            return true;
        }
        if self.enqueue_typed(MlsWriteRequest::PersistAcknowledgement { room_id, device_id }) {
            true
        } else {
            self.pending_acknowledgements.lock().unwrap().remove(&key);
            false
        }
    }

    fn persist_welcome_acknowledgement(&self, device_id: DeviceId) -> bool {
        if !self
            .pending_welcome_acknowledgements
            .lock()
            .unwrap()
            .insert(device_id)
        {
            return true;
        }
        if self.enqueue_typed(MlsWriteRequest::PersistWelcomeAcknowledgement { device_id }) {
            true
        } else {
            self.pending_welcome_acknowledgements
                .lock()
                .unwrap()
                .remove(&device_id);
            false
        }
    }

    fn process(
        service: &mut MlsService,
        events: &MlsEventQueue,
        pending_acknowledgements: &std::sync::Mutex<HashSet<(RoomId, DeviceId)>>,
        pending_welcome_acknowledgements: &std::sync::Mutex<HashSet<DeviceId>>,
        request: MlsWriteRequest,
    ) {
        match request {
                        MlsWriteRequest::PutRoster {
                            token,
                            session_id,
                            user_id,
                            expected,
                            roster,
                            bootstrap_credential_hash,
                        } => {
                            let initial = expected.is_none();
                            let stored_roster = roster.clone();
                            let result = service
                                .put_roster(
                                    user_id,
                                    expected,
                                    roster,
                                    bootstrap_credential_hash,
                                )
                                .map(|checkpoint| (checkpoint, service.cache_state()));
                            events.push(MlsReply::RosterStored {
                                token,
                                session_id,
                                user_id,
                                initial,
                                roster: stored_roster,
                                result,
                            });
                        }
                        MlsWriteRequest::CreateRoom {
                            token,
                            creator,
                            creator_client_id,
                            descriptor,
                            checkpoints,
                            bundle,
                            welcome_devices,
                        } => {
                            let room_id = descriptor.room_id;
                            let result = (|| {
                                if let Some(room) = service.existing_room_for_creation(
                                    creator,
                                    &descriptor,
                                    &bundle,
                                )? {
                                    return Ok(RoomCreationReply::Existing { room });
                                }
                                let epoch = service.create_room(
                                    creator,
                                    &creator_client_id,
                                    descriptor,
                                    &checkpoints,
                                    bundle,
                                )?;
                                Ok(RoomCreationReply::Created {
                                    epoch,
                                    room: service
                                        .cached_room(room_id)
                                        .expect("created room cache"),
                                    deliveries: collect_latest_welcomes(
                                        &service,
                                        &welcome_devices,
                                    )?,
                                })
                            })();
                            events.push(MlsReply::RoomCreated { token, room_id, result });
                        }
                        MlsWriteRequest::SubmitCommit {
                            token,
                            room_id,
                            device_id,
                            committer_client_id,
                            expected_epoch,
                            bundle,
                            welcome_devices,
                        } => {
                            let result = service
                                .submit_commit_for_device(
                                    room_id,
                                    device_id,
                                    &committer_client_id,
                                    expected_epoch,
                                    bundle,
                                )
                                .and_then(|outcome| {
                                    let accepted = match &outcome {
                                        rpc::mls::MlsCommitOutcome::Accepted { sequence, .. } => {
                                            Some(*sequence)
                                        }
                                        _ => None,
                                    };
                                    let room = accepted.and_then(|_| service.cached_room(room_id));
                                    let pushed = if let Some(sequence) = accepted {
                                        Some((
                                            service.event_batch(
                                                room_id,
                                                sequence.saturating_sub(1),
                                                1,
                                            )?,
                                            collect_latest_welcomes(
                                                &service,
                                                &welcome_devices,
                                            )?,
                                        ))
                                    } else {
                                        None
                                    };
                                    Ok((outcome, room, pushed))
                                });
                            events.push(MlsReply::CommitSubmitted { token, room_id, result });
                        }
                        MlsWriteRequest::RedeemDeviceLink {
                            token,
                            secret_hash,
                            user_id,
                            username,
                            expected,
                            roster,
                            device_id,
                            device_name,
                            credential_hash,
                            attempt_id,
                            packages,
                            bearer_token,
                            receive_files,
                            file_receive_limit_bytes,
                        } => {
                            let result = service
                                .redeem_pair(
                                    user_id,
                                    expected,
                                    roster,
                                    device_id,
                                    credential_hash.clone(),
                                    packages,
                                )
                                .map(|_| service.cache_state());
                            events.push(MlsReply::DeviceLinkRedeemed {
                                token,
                                secret_hash,
                                user_id,
                                username,
                                device_id,
                                device_name,
                                credential_hash,
                                attempt_id,
                                bearer_token,
                                receive_files,
                                file_receive_limit_bytes,
                                result,
                            });
                        }
                        MlsWriteRequest::SubmitApplication {
                            token,
                            room_id,
                            device_id,
                            epoch,
                            event_id,
                            ciphertext,
                        } => events.push(MlsReply::ApplicationSubmitted {
                            token,
                            room_id,
                            event_id,
                            result: service.submit_application_for_device_with_event(
                                room_id,
                                device_id,
                                epoch,
                                event_id,
                                ciphertext,
                            ),
                        }),
                        MlsWriteRequest::Compact {
                            scheduled,
                            minimum_fragmented_bytes,
                            minimum_fragmented_percent,
                            admin_reply,
                            started,
                        } => {
                            let result = (|| {
                                let status = service.storage_status()?;
                                let percent = if status.allocated_bytes == 0 {
                                    0
                                } else {
                                    status.fragmented_bytes.saturating_mul(100)
                                        / status.allocated_bytes
                                };
                                if scheduled
                                    && (status.fragmented_bytes < minimum_fragmented_bytes
                                        || percent
                                            < u64::from(minimum_fragmented_percent))
                                {
                                    return Ok(None);
                                }
                                let before = status.file_bytes;
                                let compacted = service.compact()?;
                                let after = service
                                    .storage_status()
                                    .ok()
                                    .and_then(|status| status.file_bytes);
                                Ok(Some((compacted, before, after)))
                            })();
                            events.push(MlsReply::Compacted {
                                admin_reply,
                                result,
                                started,
                            });
                        }
                        MlsWriteRequest::Cleanup {
                            batch_events,
                            interval,
                        } => events.push(MlsReply::Cleanup {
                            interval,
                            result: service.cleanup(batch_events),
                        }),
                        MlsWriteRequest::StorageStatus { reply } => {
                            let result = service.storage_status().map(|status| {
                                format!(
                                    "allocated={} stored={} fragmented={} file={}",
                                    status.allocated_bytes,
                                    status.stored_bytes,
                                    status.fragmented_bytes,
                                    status.file_bytes.map_or_else(
                                        || "memory".to_string(),
                                        |bytes| bytes.to_string()
                                    )
                                )
                            });
                            events.push(MlsReply::StorageStatus { reply, result });
                        }
                        MlsWriteRequest::PublishKeyPackages {
                            token,
                            user_id,
                            device_id,
                            packages,
                        } => {
                            let result = service
                                .publish_key_packages(user_id, device_id, packages)
                                .map(|available| {
                                    (available, service.cached_key_packages(device_id))
                                });
                            events.push(MlsReply::KeyPackagesPublished {
                                token,
                                device_id,
                                result,
                            });
                        }
                        MlsWriteRequest::TakeKeyPackage { token, device_id } => {
                            let result = service.take_key_package(device_id).map(|package| {
                                (
                                    package,
                                    service.key_package_count(device_id),
                                    service.cached_key_packages(device_id),
                                )
                            });
                            events.push(MlsReply::KeyPackageTaken {
                                token,
                                device_id,
                                result,
                            });
                        }
                        MlsWriteRequest::PersistAcknowledgement { room_id, device_id } => {
                            pending_acknowledgements
                                .lock()
                                .unwrap()
                                .remove(&(room_id, device_id));
                            if let Err(error) = service.persist_acknowledgement(room_id, device_id) {
                                kvlog::warn!("MLS acknowledgement persistence failed", error = error.as_str());
                            }
                        }
                        MlsWriteRequest::PersistWelcomeAcknowledgement { device_id } => {
                            pending_welcome_acknowledgements
                                .lock()
                                .unwrap()
                                .remove(&device_id);
                            if let Err(error) = service.persist_welcome_acknowledgement(device_id) {
                                kvlog::warn!("MLS Welcome acknowledgement persistence failed", error = error.as_str());
                            }
                        }
        }
        if let Err(error) = service.checkpoint_if_needed() {
            kvlog::error!("MLS WAL rotation failed", error = error.as_str());
        }
    }
}

impl Drop for MlsWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Rolling per-second counters for video ingest copy cost and ring retention,
/// logged at most once per [`VIDEO_COPY_STATS_INTERVAL`] while frames flow so
/// copied bytes/sec and retained ring bytes are measurable without per-frame
/// log noise.
struct VideoCopyStats {
    ingest_copy_bytes: u64,
    frames: u64,
    last_log: Instant,
}

const VIDEO_COPY_STATS_INTERVAL: Duration = Duration::from_secs(1);

impl VideoCopyStats {
    fn new() -> Self {
        Self {
            ingest_copy_bytes: 0,
            frames: 0,
            last_log: Instant::now(),
        }
    }

    fn maybe_log(&mut self, _ring_visible_bytes: usize, _ring_retained_bytes: usize) {
        if self.last_log.elapsed() < VIDEO_COPY_STATS_INTERVAL {
            return;
        }
        kvlog::debug!(
            "video ingest copy stats",
            ingest_copy_bytes = self.ingest_copy_bytes,
            frames = self.frames,
            ring_visible_bytes = _ring_visible_bytes,
            ring_retained_bytes = _ring_retained_bytes
        );
        *self = Self::new();
    }
}

impl Server {
    /// Replaces the configured users before the event loop starts and rebuilds
    /// every index derived from them. Embedded harnesses should use this
    /// instead of mutating [`Self::users`] directly.
    pub fn seed_users(&mut self, users: Vec<UserConfig>) -> Result<(), String> {
        if !self.sessions.is_empty() || !self.clients.is_empty() {
            return Err("cannot replace server users after clients connect".to_string());
        }
        self.usernames = UsernameRegistry::open(self.config.data_dir(), &users)?;
        self.users.users = users;
        Ok(())
    }

    /// Local TCP address the listener is bound to, resolving an ephemeral `:0`
    /// port to the concrete port the OS assigned.
    pub fn tcp_local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Local UDP address the media socket is bound to.
    pub fn udp_local_addr(&self) -> io::Result<SocketAddr> {
        Ok(self.voice_relay.local_addr())
    }

    pub fn bind(mut config: ServerConfig) -> io::Result<Self> {
        let infer_public_tcp = config.network.public_tcp_addr.trim().is_empty()
            || (config.network.tcp_addr.port() == 0
                && config
                    .network
                    .public_tcp_addr
                    .parse::<SocketAddr>()
                    .is_ok_and(|endpoint| endpoint.port() == 0));
        let infer_public_udp = config.network.public_udp_addr.trim().is_empty()
            || (config.network.udp_addr().port() == 0
                && config
                    .network
                    .public_udp_addr
                    .parse::<SocketAddr>()
                    .is_ok_and(|endpoint| endpoint.port() == 0));
        let infer_public_udp_probe = config.network.public_udp_probe_addr.is_none();
        config.normalize();
        let tcp_addr = config.network.tcp_addr;
        let udp_addr = config.network.udp_addr();
        let udp_probe_addr = config.network.udp_probe_addr;
        let p2p_enabled = config.network.p2p_enabled;
        let poll = Poll::new()?;
        let mut listener = TcpListener::bind(tcp_addr)?;
        let udp = UdpSocket::bind(udp_addr)?;
        let udp_probe = if p2p_enabled {
            udp_probe_addr.map(UdpSocket::bind).transpose()?
        } else {
            None
        };
        if infer_public_tcp {
            let mut public = config
                .network
                .public_tcp_addr
                .parse::<SocketAddr>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            public.set_port(listener.local_addr()?.port());
            config.network.public_tcp_addr = public.to_string();
        }
        if infer_public_udp {
            let mut public = config
                .network
                .public_udp_addr
                .parse::<SocketAddr>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            public.set_port(udp.local_addr()?.port());
            config.network.public_udp_addr = public.to_string();
        }
        if infer_public_udp_probe
            && let (Some(public), Some(socket)) = (
                config.network.public_udp_probe_addr.as_mut(),
                udp_probe.as_ref(),
            )
        {
            let mut endpoint = public
                .parse::<SocketAddr>()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            endpoint.set_port(socket.local_addr()?.port());
            *public = endpoint.to_string();
        }
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;

        let users = UserStore::open(config.data_dir())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let usernames = UsernameRegistry::open(config.data_dir(), &users.users)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let server_key_pair = config
            .server_key_pair()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let room_retention_days = config
            .rooms
            .iter()
            .filter_map(|room| room.mls_retention_days.map(|days| (room.room_id(), days)))
            .collect();
        let durable_mls = MlsService::open_with_retention(
            config.data_dir(),
            server_key_pair.public_key().as_ref().to_vec(),
            config.storage.mls_retention_days,
            room_retention_days,
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mls = durable_mls
            .in_memory_view()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut rooms = HashMap::new();
        for room in &config.rooms {
            let access = match &room.members {
                None => RoomAccess::Public,
                Some(members) => RoomAccess::Private(
                    members
                        .iter()
                        .filter_map(|member| {
                            // A member is either a numeric user id or an
                            // internal_reference handle; both resolve to a UserId,
                            // which is the only identity the room ACL keeps.
                            if let Ok(id) = member.parse::<u64>() {
                                return Some(UserId(id));
                            }
                            let user = users
                                .users
                                .iter()
                                .find(|user| user.internal_reference == *member);
                            if user.is_none() {
                                kvlog::warn!(
                                    "private room member does not match any registered user; ignored until that user pairs",
                                    room = room.name.as_str(),
                                    member = member.as_str()
                                );
                            }
                            user.map(|user| user.id)
                        })
                        .collect(),
                ),
            };
            rooms.insert(
                room.room_id(),
                RoomState {
                    id: room.room_id(),
                    name: room.name.clone(),
                    access,
                    active_streams: HashMap::new(),
                },
            );
        }
        let default_room = config.default_room_id();
        let mut store = RoomStore::try_open(config.data_dir(), &config.rooms)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        for dm in store.dm_rooms() {
            rooms.insert(
                dm.room_id,
                RoomState {
                    id: dm.room_id,
                    name: dm_room_name(dm.user_a, dm.user_b),
                    access: RoomAccess::Dm(dm.user_a, dm.user_b),
                    active_streams: HashMap::new(),
                },
            );
        }

        let waker = Arc::new(Waker::new(poll.registry(), WAKER)?);
        let event_notifier = Arc::new(EventNotifier::new(Arc::clone(&waker)));
        let admin_events = Arc::new(EventQueue::new(
            Arc::clone(&event_notifier),
            ADMIN_EVENTS,
            "admin",
        ));
        store.enable_async_log_writes(Arc::clone(&event_notifier));
        store.enable_async_state_writes(Arc::clone(&event_notifier));
        let voice_relay = VoiceRelayHandle::spawn(
            udp,
            udp_probe,
            Arc::clone(&event_notifier),
            p2p_enabled,
        )?;
        let history_events = Arc::new(EventQueue::new(
            Arc::clone(&event_notifier),
            HISTORY_EVENTS,
            "history",
        ));
        let history_reader = HistoryReader::spawn(Arc::clone(&history_events));
        let mls_events = Arc::new(MlsEventQueue::new(Arc::clone(&event_notifier)));
        let mls_worker = MlsWorker::spawn(durable_mls, Arc::clone(&mls_events));
        let bug_report_events = Arc::new(EventQueue::new(
            Arc::clone(&event_notifier),
            BUG_REPORT_EVENTS,
            "bug-report",
        ));
        let bug_report_writer = BugReportWriter::spawn(Arc::clone(&bug_report_events));
        let identity_events = Arc::new(EventQueue::new(
            Arc::clone(&event_notifier),
            IDENTITY_EVENTS,
            "identity",
        ));
        let identity_writer = IdentityWriter::spawn(Arc::clone(&identity_events));

        kvlog::info!(
            "server identity loaded",
            public_key = encode_hex(server_key_pair.public_key().as_ref()).as_str()
        );
        if config.transport_mode() == rpc::crypto::TransportMode::ExternalSecureLink {
            kvlog::warn!(
                "external-secure-link transport: chatt relies on the outer link for wire security; control, media, video, and file payloads travel clear and P2P is disabled"
            );
        }
        let file_size_limit_bytes = config.security.max_file_size_bytes();
        let next_mls_cleanup_at = Instant::now()
            + Duration::from_secs(config.storage.mls_cleanup_interval_minutes * 60);
        let next_mls_compaction_at = Instant::now()
            + Duration::from_secs(config.storage.mls_compaction_min_interval_hours * 60 * 60);

        Ok(Self {
            config,
            users,
            usernames,
            mls,
            mls_worker,
            mls_events,
            pending_mls: HashSet::new(),
            pending_identity: None,
            deferred_identity_controls: VecDeque::new(),
            deferred_identity_tokens: HashSet::new(),
            identity_writer,
            identity_events,
            event_notifier,
            admin_events,
            poll,
            listener,
            voice_relay,
            voice_events: VoiceEventBatch::with_capacity(),
            clients: HashMap::new(),
            pending_controls: HashMap::new(),
            pending_control_bytes: HashMap::new(),
            pending_control_total_bytes: 0,
            write_queue_total_bytes: 0,
            control_send_tokens: VecDeque::new(),
            control_send_set: HashSet::new(),
            sessions: HashMap::new(),
            reserved_media_routes: HashMap::new(),
            peer_links: HashMap::new(),
            rooms,
            streams: HashMap::new(),
            video_fanouts: VecDeque::new(),
            video_fanout_bytes: 0,
            next_token: FIRST_CLIENT,
            next_session: 1,
            next_stream: 1,
            next_connection_id: 1,
            accept_retry_at: None,
            next_file_transfer: 1,
            active_uploads: HashMap::new(),
            active_bug_reports: HashMap::new(),
            pending_bug_reports: HashSet::new(),
            bug_report_writer,
            bug_report_events,
            reserved_file_names: HashSet::new(),
            default_room,
            store,
            file_size_limit_bytes,
            invites: HashMap::new(),
            pending_dm_waiters: HashMap::new(),
            device_links: HashMap::new(),
            open_pair_global_allocations: VecDeque::new(),
            open_pair_ip_allocations: HashMap::new(),
            rng: aws_lc_rs::rand::SystemRandom::new(),
            server_key_pair,
            next_media_sweep_at: None,
            next_rtt_snapshot_at: Instant::now() + RTT_SNAPSHOT_INTERVAL,
            next_idle_sweep_at: Instant::now() + IDLE_SWEEP_INTERVAL,
            next_mls_cleanup_at,
            next_mls_compaction_at,
            mls_cleanup_pending: false,
            mls_compaction_pending: false,
            loop_work: LoopWork::default(),
            loop_stats: EventLoopStats::new(),
            history_reader,
            history_events,
            history_deliveries: VecDeque::new(),
            pending_history_fetches: 0,
            video_copy_stats: VideoCopyStats::new(),
        })
    }

    pub fn admin_sender(&self) -> AdminSender {
        AdminSender::new(Arc::clone(&self.admin_events))
    }

    pub fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut events = Events::with_capacity(256);
        loop {
            let mut ready_workers = 0u64;
            let now = Instant::now();
            if self.accept_retry_at.is_some_and(|retry| retry <= now) {
                self.accept_retry_at = None;
                self.loop_work.queue_accept_clients();
            }
            let idle_timeout = self
                .accept_retry_at
                .map(|retry| retry.saturating_duration_since(now).min(POLL_TIMEOUT))
                .unwrap_or(POLL_TIMEOUT);
            let timeout = self.loop_work.poll_timeout(idle_timeout);
            #[cfg(debug_assertions)]
            if !self.loop_work.has_immediate_work() {
                self.debug_assert_no_immediate_read_work();
            }
            if let Err(error) = self.poll.poll(&mut events, Some(timeout)) {
                if is_interrupted_io_error(&error) {
                    kvlog::warn!("server poll interrupted", error = %error);
                    continue;
                }
                return Err(error.into());
            }
            let work_started = Instant::now();
            let event_count = events.iter().count();
            for event in events.iter() {
                let ready = MioReady::from_event(event);
                match event.token() {
                    LISTENER => {
                        if ready.readable_like() && self.accept_retry_at.is_none() {
                            self.loop_work.queue_accept_clients();
                        }
                    }
                    WAKER => ready_workers |= self.event_notifier.take_ready(),
                    token => {
                        if ready.readable_like() {
                            if let Some(client) = self.clients.get_mut(&token) {
                                client.readiness.mark_ready();
                            }
                            self.loop_work.queue_client_read(token);
                        }
                        if ready.writable_like()
                            && self
                                .clients
                                .get(&token)
                                .is_some_and(|client| !client.write_buf.is_empty())
                        {
                            self.loop_work.queue_client_write(token);
                        }
                    }
                }
            }
            if self.loop_work.take_accept_clients() {
                self.accept_clients()?;
            }
            let client_read_count = self.loop_work.client_read_batch_len();
            for _ in 0..client_read_count {
                if let Some(token) = self.loop_work.pop_client_read() {
                    self.read_client(token);
                }
            }
            self.process_control_sends();
            self.process_video_fanouts();
            let client_write_count = self.loop_work.client_write_batch_len();
            for _ in 0..client_write_count {
                if let Some(token) = self.loop_work.pop_client_write() {
                    self.write_client(token);
                }
            }
            self.requeue_unclogged_uploaders();
            if ready_workers & VOICE_EVENTS != 0 {
                self.drain_voice_events()?;
            }
            if ready_workers & ROOM_LOG_EVENTS != 0 {
                self.store.drain_log_events();
            }
            if ready_workers & ROOM_STATE_EVENTS != 0 {
                self.drain_dm_completions().map_err(io::Error::other)?;
            }
            if ready_workers & HISTORY_EVENTS != 0 {
                self.drain_history_replies();
            } else if !self.history_deliveries.is_empty() {
                self.process_history_deliveries();
            }
            if ready_workers & MLS_EVENTS != 0 {
                self.drain_mls_replies();
            }
            if ready_workers & IDENTITY_EVENTS != 0 {
                self.drain_identity_replies();
            }
            if ready_workers & BUG_REPORT_EVENTS != 0 {
                self.drain_bug_report_replies();
            }
            if ready_workers & ADMIN_EVENTS != 0 && self.handle_admin_commands() {
                return Ok(());
            }
            self.flush_disconnects();
            let now = Instant::now();
            self.sweep_idle_connections(now);
            self.sweep_stale_media_routes(now);
            self.poll_room_rtt_snapshots(now);
            self.run_mls_cleanup(now);
            self.run_mls_compaction(now);
            self.loop_stats.observe_pass(
                work_started.elapsed(),
                event_count,
                client_read_count,
                client_write_count,
                self.loop_work.has_immediate_work(),
            );
        }
    }

    fn run_mls_cleanup(&mut self, now: Instant) {
        if self.mls_cleanup_pending || now < self.next_mls_cleanup_at {
            return;
        }
        let interval = Duration::from_secs(
            self.config.storage.mls_cleanup_interval_minutes.saturating_mul(60),
        );
        let batch = self.config.storage.mls_cleanup_batch_events;
        let queued = self.mls_worker.enqueue_typed(MlsWriteRequest::Cleanup {
            batch_events: batch,
            interval,
        });
        if queued {
            self.mls_cleanup_pending = true;
        } else {
            self.next_mls_cleanup_at = now + interval;
        }
    }

    fn run_mls_compaction(&mut self, now: Instant) {
        if self.mls_compaction_pending || now < self.next_mls_compaction_at {
            return;
        }
        let retry = Duration::from_secs(
            self.config.storage.mls_cleanup_interval_minutes.saturating_mul(60),
        );
        // Compaction is exclusive. Defer while any client could be affected;
        // deleted pages remain reusable in the meantime.
        if !self.clients.is_empty() {
            self.next_mls_compaction_at = now + retry;
            return;
        }
        let minimum = self
            .config
            .storage
            .mls_compaction_min_fragmented_mib
            .saturating_mul(1024 * 1024);
        let minimum_percent = self.config.storage.mls_compaction_min_fragmented_percent;
        let queued = self.mls_worker.enqueue_typed(MlsWriteRequest::Compact {
            scheduled: true,
            minimum_fragmented_bytes: minimum,
            minimum_fragmented_percent: minimum_percent,
            admin_reply: None,
            started: Instant::now(),
        });
        if queued {
            self.mls_compaction_pending = true;
        } else {
            self.next_mls_compaction_at = now + retry;
        }
    }

    /// Returns true when an embedded owner requested a clean shutdown.
    fn handle_admin_commands(&mut self) -> bool {
        let mut commands = self.admin_events.drain_up_to(16);
        while let Some(command) = commands.pop_front() {
            match command {
                AdminCommand::Invite { user, reply } => {
                    let result = self.create_invite(&user);
                    let _ = reply.send(result);
                }
                AdminCommand::MlsStorageStatus { reply } => {
                    if !self.mls_worker.enqueue_typed(MlsWriteRequest::StorageStatus {
                        reply: reply.clone(),
                    }) {
                        kvlog::warn!("MLS storage status worker unavailable");
                        let _ = reply.send(Err("MLS delivery worker is unavailable".into()));
                    }
                }
                AdminCommand::MlsCompact { reply } => {
                    if self.mls_compaction_pending || !self.clients.is_empty() {
                        let _ = reply.send(Err(
                            "MLS compaction requires an idle server; try again after clients disconnect"
                                .to_string(),
                        ));
                        continue;
                    }
                    if self.mls_worker.enqueue_typed(MlsWriteRequest::Compact {
                        scheduled: false,
                        minimum_fragmented_bytes: 0,
                        minimum_fragmented_percent: 0,
                        admin_reply: Some(reply.clone()),
                        started: Instant::now(),
                    }) {
                        self.mls_compaction_pending = true;
                    } else {
                        kvlog::warn!("MLS compaction worker unavailable");
                        let _ = reply.send(Err("MLS delivery worker is unavailable".into()));
                    }
                }
                AdminCommand::Shutdown => return true,
            }
        }
        false
    }

    fn create_invite(&mut self, user_name: &str) -> Result<String, String> {
        self.expire_invites();
        let user_name = user_name.trim();
        if user_name.is_empty() {
            return Err("invite user is empty".to_string());
        }
        if user_name.len() > 512 {
            return Err("invite user exceeds maximum length".to_string());
        }
        if user_name.chars().any(char::is_control) {
            return Err("invite user contains control characters".to_string());
        }

        let pairing_code = random_secret_hex(&self.rng)?;
        let ticket = InviteTicket {
            version: rpc::PROTOCOL_VERSION,
            pairing_code: pairing_code.clone(),
            tcp_addr: self.config.network.public_tcp_addr.clone(),
            udp_addr: self.config.network.public_udp_addr.clone(),
            udp_probe_addr: self.config.network.public_udp_probe_addr.clone(),
            server_public_key: encode_hex(self.server_key_pair.public_key().as_ref()),
        };
        let join_string = encode_invite_ticket(&ticket)?;
        self.invites.insert(
            user_name.to_string(),
            InviteState {
                pairing_code_hash: hash_secret(&pairing_code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );
        kvlog::info!("invite created", user = user_name);
        Ok(join_string)
    }

    fn expire_invites(&mut self) {
        let now = std::time::Instant::now();
        self.invites.retain(|_, invite| invite.expires_at > now);
    }

    fn accept_clients(&mut self) -> io::Result<()> {
        let mut accepted = 0usize;
        loop {
            if accepted >= ACCEPT_BUDGET {
                self.loop_work.queue_accept_clients();
                return Ok(());
            }
            let (mut socket, addr) = match self.listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if is_interrupted_io_error(&error) => {
                    self.loop_work.queue_accept_clients();
                    return Ok(());
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionAborted => {
                    continue;
                }
                Err(error) if is_fd_pressure_accept_error(&error) => {
                    // fd pressure (EMFILE/ENFILE) cannot drain by accepting.
                    // Keep serving established clients until this deadline.
                    kvlog::warn!("transient tcp accept failure", error = %error);
                    self.accept_retry_at = Some(Instant::now() + ACCEPT_ERROR_BACKOFF);
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            accepted += 1;
            // Accept then close over-cap connections so the backlog keeps
            // draining. Returning early would leave the listener readable with
            // an edge-triggered poll, so it would not wake again until the next
            // new connection.
            if self.clients.len() >= MAX_CLIENTS {
                kvlog::warn!(
                    "tcp client cap reached, rejecting connection",
                    client_count = self.clients.len(),
                    max_clients = MAX_CLIENTS,
                    addr = %addr
                );
                drop(socket);
                continue;
            }
            // Disable Nagle so relayed frames (including bulk file chunks) are
            // not held waiting for an ACK, matching the client's `set_nodelay`.
            if let Err(error) = socket.set_nodelay(true) {
                kvlog::warn!("failed to set tcp_nodelay on client socket", error = %error);
            }
            let token = Token(self.next_token);
            self.next_token += 1;
            kvlog::info!(
                "tcp client accepted",
                token = token.0,
                addr = %addr,
                client_count = self.clients.len() + 1
            );
            self.poll.registry().register(
                &mut socket,
                token,
                Interest::READABLE | Interest::WRITABLE,
            )?;
            let now = Instant::now();
            self.clients.insert(
                token,
                ClientConn {
                    socket,
                    addr,
                    read_buf: RecvBuffer::new(),
                    readiness: Readiness::new(),
                    write_buf: WriteQueue::new(),
                    kind: ConnKind::Unidentified,
                    state: ConnState::AwaitClientHello,
                    control: None,
                    transport: None,
                    session_id: None,
                    user_id: None,
                    close: Close::Open,
                    created_at: now,
                    last_activity: now,
                },
            );
        }
    }

    fn read_client(&mut self, token: Token) {
        // A parked uploader neither fills nor processes: leaving the bytes in
        // the socket closes the uploading client's send window, which is the
        // whole flow-control chain. `readiness` keeps the lost edge; the
        // `requeue_unclogged_uploaders` sweep resumes the connection.
        let clogged = self.relay_clogged_for_reader(token);
        let mut disconnected = false;
        let mut hit_limit = false;
        if let Some(client) = self.clients.get_mut(&token) {
            // A connection with an exact video record reader pumps its own
            // socket inside `read_video_conn`; a generic fill here would put
            // frame bytes back in the shared receive buffer and force a copy.
            if !clogged && !client.video_reader_attached() {
                match read_into_buffer(
                    &client.socket,
                    &mut client.read_buf,
                    &mut client.readiness,
                    READ_BUDGET_BYTES,
                    ReadLimit::ByteBudget(READ_BUDGET_BYTES),
                ) {
                    Ok(outcome) => {
                        if outcome.bytes_read > 0 {
                            client.last_activity = Instant::now();
                        }
                        if outcome.disconnected {
                            kvlog::info!("tcp client closed", token = token.0);
                        }
                        hit_limit = outcome.hit_limit;
                        disconnected = outcome.disconnected;
                    }
                    Err(error) => {
                        kvlog::warn!("tcp client read failed", token = token.0, error = %error);
                        disconnected = true;
                    }
                }
            }
        }

        if disconnected {
            self.disconnect(token);
            return;
        }
        if hit_limit {
            self.loop_work.queue_client_read(token);
        }

        // Classify a freshly accepted connection on its first bytes. A video
        // connection opens with VIDEO_MAGIC, which reads as a control frame
        // length far above MAX_FRAME_LEN, so it can never collide with a hello.
        if matches!(
            self.clients.get(&token).map(|client| &client.kind),
            Some(ConnKind::Unidentified)
        ) {
            let client = self.clients.get_mut(&token).expect("classified token");
            if client.read_buf.len() < video::VIDEO_MAGIC.len() {
                return;
            }
            if client.read_buf.pending().starts_with(&video::VIDEO_MAGIC) {
                client.read_buf.consume(video::VIDEO_MAGIC.len());
                client.kind = ConnKind::Video(VideoConn::new());
                kvlog::info!("video connection classified", token = token.0);
            } else {
                client.kind = ConnKind::Control;
            }
        }

        if matches!(
            self.clients.get(&token).map(|client| &client.kind),
            Some(ConnKind::Video(_))
        ) {
            self.read_video_conn(token);
            return;
        }

        let mut steps = 0usize;
        loop {
            if steps >= CONTROL_STEPS_PER_READ {
                self.queue_client_read_if_work_ready(token);
                return;
            }
            // Relayed chunks queue to recipients inside `handle_control`, so
            // re-check between frames and park with the remaining frames
            // still buffered once a recipient's queue is over the cap.
            if self.relay_clogged_for_reader(token) {
                return;
            }
            if self.pending_mls.contains(&token) || self.identity_pending(token) {
                return;
            }
            let Some(client) = self.clients.get_mut(&token) else {
                return;
            };
            // A connection already being torn down (a rejected auth) gets no
            // further frame processing; pipelined frames must not spend crypto
            // or rate-limit work on its behalf.
            if client.is_closing() {
                return;
            }
            match control_conn_step(token, client) {
                Ok(None) => return,
                Ok(Some(ControlStep::Hello(hello))) => {
                    steps += 1;
                    if let Err(error) = self.handle_client_hello(token, hello) {
                        kvlog::warn!(
                            "tcp client protocol error",
                            token = token.0,
                            error = %error
                        );
                        self.disconnect(token);
                        return;
                    }
                }
                Ok(Some(ControlStep::Control(control))) => {
                    steps += 1;
                    if self.pending_identity.is_some()
                        && self.clients.get(&token).map(|client| client.state)
                            == Some(ConnState::AwaitAuth)
                        && identity_control_needs_ordering(&control)
                    {
                        self.deferred_identity_controls.push_back((token, control));
                        self.deferred_identity_tokens.insert(token);
                        return;
                    }
                    let kind = client_control_kind(&control);
                    let started = Instant::now();
                    let result = self.handle_control(token, control);
                    self.loop_stats.observe_control(kind, started.elapsed());
                    if let Err(error) = result {
                        kvlog::warn!(
                            "tcp client protocol error",
                            token = token.0,
                            error = %error
                        );
                        self.disconnect(token);
                        return;
                    }
                }
                Err(error) => {
                    kvlog::warn!("tcp client protocol error", token = token.0, error = %error);
                    self.disconnect(token);
                    return;
                }
            }
        }
    }

    fn read_video_conn(&mut self, token: Token) {
        let mut records = 0usize;
        let mut byte_budget = READ_BUDGET_BYTES;
        loop {
            if records >= VIDEO_RECORDS_PER_READ {
                self.queue_client_read_if_work_ready(token);
                return;
            }
            let (pump, spent) = self.pump_video_reader(token, byte_budget);
            byte_budget = byte_budget.saturating_sub(spent);
            match pump {
                VideoPump::Step => {}
                VideoPump::Idle => return,
                VideoPump::Budget => {
                    self.loop_work.queue_client_read(token);
                    return;
                }
                VideoPump::Closed => {
                    kvlog::info!("tcp client closed", token = token.0);
                    self.disconnect(token);
                    return;
                }
                VideoPump::Failed(error) => {
                    kvlog::warn!(
                        "video connection protocol error",
                        token = token.0,
                        error = error.as_str()
                    );
                    self.disconnect(token);
                    return;
                }
            }
            let Some(client) = self.clients.get_mut(&token) else {
                return;
            };
            if client.is_closing() {
                return;
            }
            match video_conn_step(client) {
                Ok(None) => return,
                Ok(Some(VideoStep::Handshake(record))) => {
                    records += 1;
                    if let Err(error) = self.process_video_record(token, record) {
                        kvlog::warn!(
                            "video connection protocol error",
                            token = token.0,
                            error = %error
                        );
                        self.disconnect(token);
                        return;
                    }
                }
                Ok(Some(VideoStep::Publish {
                    stream_id,
                    frame,
                    is_key,
                })) => {
                    records += 1;
                    self.publish_video_frame(stream_id, frame, is_key);
                }
                Ok(Some(VideoStep::SubscriberChatter)) => {
                    records += 1;
                }
                Err(error) => {
                    kvlog::warn!(
                        "video connection protocol error",
                        token = token.0,
                        error = %error
                    );
                    self.disconnect(token);
                    return;
                }
            }
        }
    }

    /// Feeds a reader-driven video connection: drains any residual bytes the
    /// generic receive buffer read before the reader attached, then pumps the
    /// socket until a whole record is buffered, the socket drains, or
    /// `byte_budget` is spent. Returns the outcome and the bytes read.
    /// Connections without a reader step straight from the receive buffer.
    fn pump_video_reader(&mut self, token: Token, byte_budget: usize) -> (VideoPump, usize) {
        let Some(client) = self.clients.get_mut(&token) else {
            return (VideoPump::Idle, 0);
        };
        if client.is_closing() {
            return (VideoPump::Idle, 0);
        }
        let ConnKind::Video(video) = &mut client.kind else {
            return (VideoPump::Step, 0);
        };
        let Some(reader) = video.reader.as_mut() else {
            return (VideoPump::Step, 0);
        };
        while !client.read_buf.is_empty() && !reader.record_ready() {
            match reader.accept(client.read_buf.pending()) {
                Ok(taken) => {
                    self.video_copy_stats.ingest_copy_bytes += taken as u64;
                    client.read_buf.consume(taken);
                }
                Err(error) => return (VideoPump::Failed(format!("invalid record: {error}")), 0),
            }
        }
        if reader.record_ready() {
            return (VideoPump::Step, 0);
        }
        match video::read_video_record(&client.socket, reader, &mut client.readiness, byte_budget) {
            Ok(outcome) => {
                if outcome.bytes_read > 0 {
                    client.last_activity = Instant::now();
                }
                let pump = if outcome.disconnected {
                    VideoPump::Closed
                } else if reader.record_ready() {
                    VideoPump::Step
                } else if outcome.hit_limit {
                    VideoPump::Budget
                } else {
                    VideoPump::Idle
                };
                (pump, outcome.bytes_read)
            }
            Err(error) => (VideoPump::Failed(format!("video read failed: {error}")), 0),
        }
    }

    fn process_video_record(&mut self, token: Token, record: Vec<u8>) -> Result<(), String> {
        let phase = match self.clients.get(&token).map(|client| &client.kind) {
            Some(ConnKind::Video(video)) => video.phase,
            _ => return Err("record on a non-video connection".to_string()),
        };
        match phase {
            VideoPhase::AwaitHello => self.handle_video_hello(token, &record),
            VideoPhase::AwaitAuth => self.handle_video_auth(token, &record),
            VideoPhase::Streaming => {
                Err("streaming record on a handshaking connection".to_string())
            }
        }
    }

    fn handle_video_hello(&mut self, token: Token, record: &[u8]) -> Result<(), String> {
        let hello: VideoHello = video::decode_video_hello(record)?;
        if hello.version != rpc::PROTOCOL_VERSION {
            return Err(format!("unsupported video version {}", hello.version));
        }
        let stream = self
            .streams
            .get(&hello.stream_id)
            .ok_or_else(|| "unknown video stream".to_string())?;
        if self.live_token_for_session(hello.session_id).is_none() {
            return Err("video session is not active".to_string());
        }
        let session_mode = self
            .sessions
            .get(&hello.session_id)
            .map(|session| session.transport.mode)
            .ok_or_else(|| "video session is not active".to_string())?;
        let in_voice_room = self
            .sessions
            .get(&hello.session_id)
            .is_some_and(|session| session.voice_room == Some(stream.room_id));
        let secret = match hello.role {
            VideoRole::Publisher if stream.owner_session == hello.session_id && in_voice_room => {
                stream.publish_secret
            }
            VideoRole::Subscriber if in_voice_room => stream
                .view_secrets
                .get(&hello.session_id)
                .copied()
                .ok_or_else(|| "video viewer access was not granted".to_string())?,
            _ => return Err("video session is not authorized for this stream".to_string()),
        };
        let (send, recv) = derive_video_keys(&secret, VideoKeyRole::Server);
        // The connection inherits its owning session's negotiated mode: native
        // seals video records with the per-stream keys; external-link leaves them
        // clear (the outer link secures the wire) and authenticates setup with a
        // proof instead.
        let record = match session_mode {
            TransportMode::NativeEncrypted => RecordProtection::aead(send, recv),
            TransportMode::ExternalSecureLink => RecordProtection::clear(),
        };
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let ConnKind::Video(video) = &mut client.kind else {
            return Err("hello on a non-video connection".to_string());
        };
        video.session_id = Some(hello.session_id);
        video.stream_id = Some(hello.stream_id);
        video.role = Some(hello.role);
        video.record = Some(record);
        video.phase = VideoPhase::AwaitAuth;
        kvlog::info!(
            "video hello accepted",
            token = token.0,
            stream_id = hello.stream_id.0,
            role = video_role_name(hello.role)
        );
        Ok(())
    }

    fn handle_video_auth(&mut self, token: Token, record: &[u8]) -> Result<(), String> {
        // Phase 1: read the connection identity and prove possession. Native
        // proves it by opening the AEAD auth record; external-link verifies a
        // compact HMAC proof under the session's video auth key.
        let (session_id, stream_id, role) = {
            let client = self
                .clients
                .get_mut(&token)
                .ok_or_else(|| "unknown client token".to_string())?;
            let ConnKind::Video(video) = &mut client.kind else {
                return Err("auth on a non-video connection".to_string());
            };
            let session_id = video
                .session_id
                .ok_or_else(|| "video auth before hello".to_string())?;
            let stream_id = video
                .stream_id
                .ok_or_else(|| "video auth before hello".to_string())?;
            let role = video
                .role
                .ok_or_else(|| "video auth before hello".to_string())?;
            let record_protection = video
                .record
                .as_mut()
                .ok_or_else(|| "video auth before hello".to_string())?;
            if let RecordProtection::Aead(cipher) = record_protection {
                cipher
                    .open_next(CHANNEL_VIDEO, record)
                    .map_err(|error| format!("video auth failed: {error}"))?;
            }
            (session_id, stream_id, role)
        };
        let (mode, video_auth_key) = self
            .sessions
            .get(&session_id)
            .map(|session| (session.transport.mode, session.transport.video_auth_key))
            .ok_or_else(|| "video auth session missing".to_string())?;
        if mode == TransportMode::ExternalSecureLink
            && !video::video_auth_proof_verify(
                &video_auth_key,
                session_id,
                stream_id,
                role,
                mode,
                record,
            )
        {
            return Err("video auth proof failed".to_string());
        }
        // Phase 2: ack and attach.
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let ConnKind::Video(video) = &mut client.kind else {
            return Err("auth on a non-video connection".to_string());
        };
        let record_protection = video
            .record
            .as_mut()
            .ok_or_else(|| "video auth before hello".to_string())?;
        let ack = video::encode_video_ack(&VideoAck::Ok);
        let sealed = record_protection
            .seal_next(CHANNEL_VIDEO, &ack)
            .map_err(|error| error.to_string())?;
        video.phase = VideoPhase::Streaming;
        let queued_before = client.write_buf.len();
        video::write_record(client.write_buf.tail_mut(), &sealed)
            .map_err(|error| error.to_string())?;
        self.write_queue_total_bytes += client.write_buf.len() - queued_before;
        self.write_client(token);
        match role {
            VideoRole::Publisher => {
                self.attach_publisher(token, stream_id)?;
                // From here on, published records land in exact per-record
                // allocations so their plaintext can be retained zero-copy.
                if let Some(client) = self.clients.get_mut(&token)
                    && let ConnKind::Video(video) = &mut client.kind
                {
                    video.reader = Some(VideoRecordReader::new());
                }
                Ok(())
            }
            VideoRole::Subscriber => self.attach_subscriber(token, stream_id),
        }
    }

    fn attach_publisher(&mut self, token: Token, stream_id: StreamId) -> Result<(), String> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or_else(|| "unknown video stream".to_string())?;
        if let Some(existing) = stream.publisher_conn
            && existing != token
        {
            return Err("video stream already has a publisher".to_string());
        }
        stream.publisher_conn = Some(token);
        kvlog::info!(
            "video publisher attached",
            token = token.0,
            stream_id = stream_id.0
        );
        Ok(())
    }

    /// Adds a subscriber and replays the fast-start burst: the most recent
    /// keyframe and every frame after it, so the viewer's decoder bootstraps
    /// without waiting for the next GOP.
    fn attach_subscriber(&mut self, token: Token, stream_id: StreamId) -> Result<(), String> {
        let burst: Vec<SharedVideoFrame> = {
            let stream = self
                .streams
                .get_mut(&stream_id)
                .ok_or_else(|| "unknown video stream".to_string())?;
            if !stream.subscribers.contains(&token) {
                stream.subscribers.push(token);
            }
            match fast_start_index(&stream.ring) {
                Some(index) => stream
                    .ring
                    .iter()
                    .skip(index)
                    .map(|frame| frame.data.clone())
                    .collect(),
                None => Vec::new(),
            }
        };
        let burst_len = burst.len();
        let burst_bytes = burst.iter().fold(0usize, |total, data| {
            total.saturating_add(video_fanout_charge(data, 1))
        });
        if self.video_fanout_bytes.saturating_add(burst_bytes)
            > VIDEO_FANOUT_QUEUE_MAX_BYTES
        {
            return Err("video fast-start queue is full; reconnect shortly".to_string());
        }
        for data in burst {
            self.enqueue_video_fanout(stream_id, data, vec![token])?;
        }
        kvlog::info!(
            "video subscriber attached",
            token = token.0,
            stream_id = stream_id.0,
            fast_start_frames = burst_len
        );
        Ok(())
    }

    /// Seals one inner frame for a single subscriber directly into its write
    /// queue (no intermediate frame allocation). Returns whether
    /// the subscriber's queue stayed under the high-water mark.
    fn seal_video_to_subscriber(&mut self, token: Token, data: &[u8]) -> Result<bool, String> {
        {
            let client = self
                .clients
                .get_mut(&token)
                .ok_or_else(|| "unknown subscriber token".to_string())?;
            let ConnKind::Video(video) = &mut client.kind else {
                return Err("subscriber is not a video connection".to_string());
            };
            let record = video
                .record
                .as_mut()
                .ok_or_else(|| "subscriber missing record protection".to_string())?;
            let sealed_len = record.sealed_len(data.len());
            if sealed_len > video::MAX_VIDEO_FRAME_LEN {
                return Err("sealed video record exceeds maximum length".to_string());
            }
            let queued_len = video::VIDEO_LENGTH_PREFIX_LEN + sealed_len;
            if self
                .write_queue_total_bytes
                .saturating_add(self.pending_control_total_bytes)
                .saturating_add(queued_len)
                > OUTBOUND_GLOBAL_HIGH_WATER
            {
                return Ok(false);
            }
            let queued_before = client.write_buf.len();
            let out = client.write_buf.tail_mut();
            let tail_before = out.len();
            out.extend_from_slice(&(sealed_len as u32).to_le_bytes());
            if let Err(error) = record.seal_next_into(CHANNEL_VIDEO, data, out) {
                out.truncate(tail_before);
                return Err(error.to_string());
            }
            self.write_queue_total_bytes += client.write_buf.len() - queued_before;
        }
        self.loop_work.queue_client_write(token);
        let within = self
            .clients
            .get(&token)
            .map(|client| subscriber_within_limit(client.write_buf.len()))
            .unwrap_or(false);
        Ok(within)
    }

    /// Caches one published frame and fans it out to every subscriber. On a
    /// keyframe the ring resets so a new viewer starts from a self-contained
    /// point. A subscriber whose queue overflows is dropped, it reconnects and
    /// fast-starts cheaply.
    fn publish_video_frame(&mut self, stream_id: StreamId, data: SharedVideoFrame, is_key: bool) {
        self.video_copy_stats.frames += 1;
        let subscribers = {
            let Some(stream) = self.streams.get_mut(&stream_id) else {
                return;
            };
            if is_key {
                stream.ring.clear();
                stream.ring_bytes = 0;
            }
            stream.ring_bytes += data.retained_bytes();
            stream.ring.push_back(VideoRingFrame {
                data: data.clone(),
                is_key,
            });
            while stream.ring_bytes > VIDEO_RING_MAX_BYTES && stream.ring.len() > 1 {
                if let Some(front) = stream.ring.pop_front() {
                    stream.ring_bytes -= front.data.retained_bytes();
                }
            }
            let visible_bytes = stream.ring.iter().map(|frame| frame.data.len()).sum();
            self.video_copy_stats
                .maybe_log(visible_bytes, stream.ring_bytes);
            stream.subscribers.clone()
        };
        let _ = self.enqueue_video_fanout(stream_id, data, subscribers);
    }

    fn enqueue_video_fanout(
        &mut self,
        stream_id: StreamId,
        data: SharedVideoFrame,
        subscribers: Vec<Token>,
    ) -> Result<(), String> {
        if subscribers.is_empty() {
            return Ok(());
        }
        let charged_bytes = video_fanout_charge(&data, subscribers.capacity());
        if self.video_fanout_bytes.saturating_add(charged_bytes)
            > VIDEO_FANOUT_QUEUE_MAX_BYTES
        {
            kvlog::warn!(
                "video subscribers dropped because scheduler queue is full",
                stream_id = stream_id.0,
                frame_bytes = data.len(),
                queued_bytes = self.video_fanout_bytes
            );
            // Continuing with a later delta after dropping an arbitrary frame
            // corrupts every decoder until the next keyframe. Reconnect makes
            // each viewer fast-start from a coherent cached GOP instead.
            for token in subscribers {
                self.disconnect(token);
            }
            return Err("video fanout queue is full".to_string());
        }
        self.video_fanout_bytes += charged_bytes;
        self.video_fanouts.push_back(VideoFanout {
            stream_id,
            data,
            subscribers,
            next_subscriber: 0,
            charged_bytes,
        });
        self.refresh_background_work();
        Ok(())
    }

    fn process_video_fanouts(&mut self) {
        let mut bytes = 0usize;
        let mut recipients = 0usize;
        while recipients < VIDEO_FANOUT_BUDGET_RECIPIENTS {
            let Some(mut fanout) = self.video_fanouts.pop_front() else {
                break;
            };
            if fanout.next_subscriber >= fanout.subscribers.len() {
                self.video_fanout_bytes = self
                    .video_fanout_bytes
                    .saturating_sub(fanout.charged_bytes);
                continue;
            }
            let frame_bytes = fanout.data.len();
            if recipients > 0 && bytes.saturating_add(frame_bytes) > VIDEO_FANOUT_BUDGET_BYTES {
                self.video_fanouts.push_front(fanout);
                break;
            }
            let token = fanout.subscribers[fanout.next_subscriber];
            fanout.next_subscriber += 1;
            bytes = bytes.saturating_add(frame_bytes);
            recipients += 1;
            let within = self
                .seal_video_to_subscriber(token, fanout.data.as_slice())
                .unwrap_or(false);
            if !within {
                if let Some(stream) = self.streams.get_mut(&fanout.stream_id) {
                    stream.subscribers.retain(|other| *other != token);
                }
                kvlog::warn!(
                    "video subscriber dropped for backpressure",
                    token = token.0,
                    stream_id = fanout.stream_id.0
                );
                self.disconnect(token);
            }
            if fanout.next_subscriber < fanout.subscribers.len() {
                self.video_fanouts.push_front(fanout);
            } else {
                self.video_fanout_bytes = self
                    .video_fanout_bytes
                    .saturating_sub(fanout.charged_bytes);
            }
            if bytes >= VIDEO_FANOUT_BUDGET_BYTES {
                break;
            }
        }
        self.refresh_background_work();
    }

    fn start_share(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        annexb: bool,
        extradata: Vec<u8>,
    ) -> Result<(), String> {
        let (user_id, sender_name, in_voice) = match self.sessions.get(&session_id) {
            Some(session) => (
                session.user_id,
                session.username.clone(),
                session.voice_room == Some(room_id),
            ),
            None => return Err("unknown session".to_string()),
        };
        if !in_voice {
            return Err("join the room's voice call before sharing".to_string());
        }
        if self
            .streams
            .values()
            .any(|stream| stream.owner_session == session_id)
        {
            return Err("a screen share is already active for this session".to_string());
        }
        let publish_secret = random_secret(&self.rng)?;
        let stream_id = StreamId(self.next_stream);
        self.next_stream = self.next_stream.wrapping_add(1).max(1);
        self.streams.insert(
            stream_id,
            VideoStream {
                room_id,
                owner_session: session_id,
                user_id,
                sender_name: sender_name.clone(),
                publish_secret,
                view_secrets: HashMap::new(),
                codec: codec.clone(),
                coded_width,
                coded_height,
                annexb,
                extradata: extradata.clone(),
                publisher_conn: None,
                subscribers: Vec::new(),
                ring: VecDeque::new(),
                ring_bytes: 0,
            },
        );
        kvlog::info!(
            "screen share started",
            session_id = session_id.0,
            room_id = room_id.0,
            stream_id = stream_id.0,
            codec = codec.as_str()
        );
        if let Some(token) = self.live_token_for_session(session_id) {
            self.send_control_to_token(
                token,
                &ServerControl::ShareStarted {
                    room_id,
                    stream_id,
                    publish_secret: publish_secret.to_vec(),
                    codec: codec.clone(),
                    coded_width,
                    coded_height,
                    extradata: extradata.clone(),
                },
            )?;
        }
        self.announce_share_to_voice_members(stream_id, Some(session_id));
        Ok(())
    }

    fn stop_share(&mut self, session_id: SessionId, stream_id: StreamId) -> Result<(), String> {
        match self.streams.get(&stream_id) {
            Some(stream) if stream.owner_session == session_id => {}
            Some(_) => return Err("not the owner of this share".to_string()),
            None => return Ok(()),
        }
        self.end_share(stream_id);
        Ok(())
    }

    fn end_share(&mut self, stream_id: StreamId) {
        let Some(stream) = self.streams.remove(&stream_id) else {
            return;
        };
        let room_id = stream.room_id;
        let mut tokens = stream.subscribers.clone();
        if let Some(publisher) = stream.publisher_conn {
            tokens.push(publisher);
        }
        let control = ServerControl::ShareEnded { room_id, stream_id };
        let recipients: Vec<Token> = self
            .voice_member_sessions(room_id)
            .into_iter()
            .filter_map(|session_id| self.live_token_for_session(session_id))
            .collect();
        self.send_control_to_tokens(&recipients, &control);
        for token in tokens {
            self.disconnect(token);
        }
        kvlog::info!(
            "screen share ended",
            stream_id = stream_id.0,
            room_id = room_id.0
        );
    }

    fn end_shares_for_session(&mut self, session_id: SessionId) {
        let stream_ids = self
            .streams
            .iter()
            .filter(|(_, stream)| stream.owner_session == session_id)
            .map(|(stream_id, _)| *stream_id)
            .collect::<Vec<_>>();
        for stream_id in stream_ids {
            self.end_share(stream_id);
        }
    }

    fn share_available_for_session(
        &mut self,
        stream_id: StreamId,
        session_id: SessionId,
    ) -> Result<ServerControl, String> {
        let (room_id, owner_session) = self
            .streams
            .get(&stream_id)
            .map(|stream| (stream.room_id, stream.owner_session))
            .ok_or_else(|| "unknown screen share".to_string())?;
        if owner_session == session_id {
            return Err("share owner does not need viewer access".to_string());
        }
        if !self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.voice_room == Some(room_id))
        {
            return Err("viewer is not in the share's voice room".to_string());
        }
        let existing = self
            .streams
            .get(&stream_id)
            .and_then(|stream| stream.view_secrets.get(&session_id).copied());
        let view_secret = match existing {
            Some(secret) => secret,
            None => {
                let secret = random_secret(&self.rng)?;
                self.streams
                    .get_mut(&stream_id)
                    .ok_or_else(|| "unknown screen share".to_string())?
                    .view_secrets
                    .insert(session_id, secret);
                secret
            }
        };
        let stream = self
            .streams
            .get(&stream_id)
            .ok_or_else(|| "unknown screen share".to_string())?;
        Ok(ServerControl::ShareAvailable {
            room_id: stream.room_id,
            stream_id,
            user_id: stream.user_id,
            sender_name: stream.sender_name.clone(),
            codec: stream.codec.clone(),
            coded_width: stream.coded_width,
            coded_height: stream.coded_height,
            annexb: stream.annexb,
            extradata: stream.extradata.clone(),
            view_secret: view_secret.to_vec(),
        })
    }

    fn send_share_available_to_session(&mut self, stream_id: StreamId, session_id: SessionId) {
        let Some(token) = self.live_token_for_session(session_id) else {
            return;
        };
        let Ok(control) = self.share_available_for_session(stream_id, session_id) else {
            return;
        };
        let _ = self.send_control_to_token(token, &control);
    }

    fn announce_share_to_voice_members(
        &mut self,
        stream_id: StreamId,
        excluded_session: Option<SessionId>,
    ) {
        let Some(room_id) = self.streams.get(&stream_id).map(|stream| stream.room_id) else {
            return;
        };
        let recipients = self
            .voice_member_sessions(room_id)
            .into_iter()
            .filter(|session_id| Some(*session_id) != excluded_session)
            .collect::<Vec<_>>();
        for session_id in recipients {
            self.send_share_available_to_session(stream_id, session_id);
        }
    }

    /// Announces every active share in the room to a member that just joined, so
    /// a viewer who arrives mid-share still sees a play button with the codec.
    fn replay_voice_room_shares(&mut self, room_id: RoomId, session_id: SessionId) {
        let streams = self
            .streams
            .iter()
            .filter_map(|(stream_id, stream)| {
                (stream.room_id == room_id && stream.owner_session != session_id)
                    .then_some(*stream_id)
            })
            .collect::<Vec<_>>();
        for stream_id in streams {
            self.send_share_available_to_session(stream_id, session_id);
        }
    }

    fn revoke_share_access_for_session(&mut self, session_id: SessionId) {
        for stream in self.streams.values_mut() {
            stream.view_secrets.remove(&session_id);
        }
        let tokens = self
            .clients
            .iter()
            .filter_map(|(token, client)| match &client.kind {
                ConnKind::Video(video)
                    if video.session_id == Some(session_id)
                        && video.role == Some(VideoRole::Subscriber) =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for token in tokens {
            self.disconnect(token);
        }
    }

    /// Prunes a dropped video connection's token from its stream. Called from
    /// `disconnect` before the connection's `ClientConn` is gone.
    fn detach_video_conn(&mut self, stream_id: StreamId, role: VideoRole, token: Token) {
        let Some(stream) = self.streams.get_mut(&stream_id) else {
            return;
        };
        match role {
            VideoRole::Publisher => {
                if stream.publisher_conn == Some(token) {
                    stream.publisher_conn = None;
                }
            }
            VideoRole::Subscriber => stream.subscribers.retain(|other| *other != token),
        }
    }

    fn handle_client_hello(
        &mut self,
        token: Token,
        hello: control::ClientHello,
    ) -> Result<(), String> {
        kvlog::info!(
            "client hello decoded",
            token = token.0,
            client_nonce_size = hello.client_nonce.len(),
            client_ephemeral_size = hello.client_ephemeral.len()
        );
        let mode = self.config.transport_mode();
        // Rejects the client (connection dropped) if it did not advertise the
        // server's configured mode; there is one server mode, no per-client mix.
        let response = respond_to_client_hello(&self.rng, &self.server_key_pair, &hello, mode)
            .map_err(|error| error.to_string())?;
        let control = response.transport.control_record();
        let encoded = encode_server_hello(&response.hello);
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let queued_before = client.write_buf.len();
        frame::encode_frame(&encoded, client.write_buf.tail_mut())
            .map_err(|error| error.to_string())?;
        self.write_queue_total_bytes += client.write_buf.len() - queued_before;
        client.control = Some(control);
        client.transport = Some(response.transport);
        client.state = ConnState::AwaitAuth;
        kvlog::info!(
            "client handshake completed",
            token = token.0,
            queued_bytes = client.write_buf.len(),
            transport_mode = mode.as_str()
        );
        self.write_client(token);
        Ok(())
    }

    fn handle_control(&mut self, token: Token, control: ClientControl) -> Result<(), String> {
        let state = self
            .clients
            .get(&token)
            .map(|client| client.state)
            .ok_or_else(|| "unknown client token".to_string())?;

        kvlog::info!(
            "client control handling",
            token = token.0,
            state = conn_state_name(state),
            kind = client_control_kind(&control)
        );
        match (state, control) {
            (
                ConnState::AwaitAuth,
                ClientControl::Authenticate {
                    username,
                    token: auth_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.authenticate_client(
                token,
                &username,
                &auth_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (
                ConnState::AwaitAuth,
                ClientControl::Pair {
                    username,
                    pairing_code,
                    token: new_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.pair_client(
                token,
                &username,
                &pairing_code,
                &new_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (
                ConnState::AwaitAuth,
                ClientControl::OpenPair {
                    username,
                    password,
                    existing_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.open_pair_client(
                token,
                &username,
                &password,
                &existing_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (ConnState::AwaitAuth, ClientControl::FetchDeviceLink { redemption_secret }) => {
                self.fetch_device_link(token, &redemption_secret)
            }
            (
                ConnState::AwaitAuth,
                ClientControl::RedeemDeviceLink {
                    redemption_secret,
                    attempt_id,
                    expected_roster,
                    roster,
                    key_packages,
                    bearer_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.redeem_device_link(
                token,
                &redemption_secret,
                attempt_id,
                expected_roster,
                roster,
                key_packages,
                bearer_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (ConnState::AwaitAuth, _) => Err("authenticate before sending control messages".into()),
            (
                ConnState::Ready,
                ClientControl::Authenticate { .. }
                | ClientControl::Pair { .. }
                | ClientControl::OpenPair { .. }
                | ClientControl::FetchDeviceLink { .. }
                | ClientControl::RedeemDeviceLink { .. },
            ) => Err("session is already authenticated".into()),
            (
                ConnState::Ready,
                ClientControl::CreateDeviceLink {
                    redemption_secret_hash,
                    enrollment_bundle,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                let result =
                    self.create_device_link(session_id, redemption_secret_hash, enrollment_bundle);
                self.report_request_outcome(token, result)
            }
            (
                ConnState::Ready,
                ClientControl::CancelDeviceLink {
                    redemption_secret_hash,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                let result = self.cancel_device_link(session_id, redemption_secret_hash);
                self.report_request_outcome(token, result)
            }
            (
                ConnState::Ready,
                ClientControl::SendChat {
                    room_id,
                    body,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.send_chat(session_id, room_id, body)
            }
            (
                ConnState::Ready,
                ClientControl::EditChat {
                    room_id,
                    target,
                    body,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.mutate_chat(
                    session_id,
                    room_id,
                    target,
                    MutationKind::Edit,
                    body,
                )
            }
            (
                ConnState::Ready,
                ClientControl::DeleteChat {
                    room_id,
                    target,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.mutate_chat(
                    session_id,
                    room_id,
                    target,
                    MutationKind::Delete,
                    String::new(),
                )
            }
            (
                ConnState::Ready,
                ClientControl::FetchHistory {
                    room_id,
                    before,
                    limit,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.fetch_history(session_id, room_id, before, limit);
                Ok(())
            }
            (ConnState::Ready, ClientControl::JoinVoice { room_id }) => {
                let session_id = self.session_for_token(token)?;
                self.join_voice(session_id, room_id);
                Ok(())
            }
            (ConnState::Ready, ClientControl::LeaveVoice) => {
                let session_id = self.session_for_token(token)?;
                self.leave_voice(session_id, None);
                Ok(())
            }
            (ConnState::Ready, ClientControl::OpenDm { user_id }) => {
                let session_id = self.session_for_token(token)?;
                self.open_dm(session_id, user_id);
                Ok(())
            }
            (ConnState::Ready, ClientControl::SetVoiceStatus { status }) => {
                let session_id = self.session_for_token(token)?;
                self.set_voice_status(session_id, status);
                Ok(())
            }
            (
                ConnState::Ready,
                ClientControl::PublishP2p {
                    room_id,
                    generation,
                    nat,
                    tie_breaker,
                    candidates,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                let result = self.publish_p2p(
                    session_id,
                    room_id,
                    generation,
                    nat,
                    tie_breaker,
                    candidates,
                );
                self.report_request_outcome(token, result)
            }
            (
                ConnState::Ready,
                ClientControl::UploadFileStart {
                    room_id,
                    transfer_id,
                    name,
                    size,
                    encoding,
                    mls_event_id,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.start_file_upload(
                    session_id,
                    room_id,
                    transfer_id,
                    name,
                    size,
                    encoding,
                    mls_event_id,
                )
            }
            (
                ConnState::Ready,
                ClientControl::UploadFileChunk {
                    transfer_id,
                    offset,
                    data,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.receive_file_chunk(session_id, transfer_id, offset, data)
            }
            (ConnState::Ready, ClientControl::UploadFileComplete { transfer_id }) => {
                let session_id = self.session_for_token(token)?;
                self.complete_file_upload(session_id, transfer_id)
            }
            (
                ConnState::Ready,
                ClientControl::UploadFileCancel {
                    transfer_id,
                    reason,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.cancel_file_upload(session_id, transfer_id, reason);
                Ok(())
            }
            (ConnState::Ready, ClientControl::SkipFile { transfer_id }) => {
                let session_id = self.session_for_token(token)?;
                self.skip_file_download(session_id, transfer_id);
                Ok(())
            }
            (
                ConnState::Ready,
                ClientControl::StartShare {
                    room_id,
                    codec,
                    coded_width,
                    coded_height,
                    annexb,
                    extradata,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                let result = self.start_share(
                    session_id,
                    room_id,
                    codec,
                    coded_width,
                    coded_height,
                    annexb,
                    extradata,
                );
                self.report_request_outcome(token, result)
            }
            (ConnState::Ready, ClientControl::StopShare { stream_id }) => {
                let session_id = self.session_for_token(token)?;
                let result = self.stop_share(session_id, stream_id);
                self.report_request_outcome(token, result)
            }
            (
                ConnState::Ready,
                control @ (ClientControl::FetchDeviceRoster { .. }
                | ClientControl::PutDeviceRoster { .. }
                | ClientControl::BindMlsDevice { .. }
                | ClientControl::PublishKeyPackages { .. }
                | ClientControl::TakeKeyPackage { .. }
                | ClientControl::CreateEncryptedRoom { .. }
                | ClientControl::FetchGroupInfo { .. }
                | ClientControl::SubmitCommitBundle { .. }
                | ClientControl::SubmitMlsApplication { .. }
                | ClientControl::FetchMlsEvents { .. }
                | ClientControl::FetchMlsWelcome { .. }
                | ClientControl::AckMlsEvent { .. }
                | ClientControl::AckMlsWelcome { .. }),
            ) => {
                let session_id = self.session_for_token(token)?;
                self.handle_mls_control(session_id, control)
            }
            (ConnState::Ready, ClientControl::Ping { nonce }) => {
                self.send_control_to_token(token, &ServerControl::Pong { nonce })
            }
            (
                ConnState::Ready,
                ClientControl::BugReportStart {
                    report_id,
                    description,
                    metadata,
                    logs_size,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                let result =
                    self.start_bug_report(session_id, report_id, description, metadata, logs_size);
                self.report_bug_outcome(token, result)
            }
            (
                ConnState::Ready,
                ClientControl::BugReportChunk {
                    report_id,
                    offset,
                    data,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                let result = self.receive_bug_report_chunk(session_id, report_id, offset, data);
                self.report_bug_outcome(token, result)
            }
            (ConnState::Ready, ClientControl::BugReportComplete { report_id }) => {
                let session_id = self.session_for_token(token)?;
                let result = self.complete_bug_report(session_id, report_id);
                self.report_bug_outcome(token, result)
            }
            (ConnState::AwaitClientHello, _) => Err("handshake is not complete".into()),
        }
    }

    fn handle_mls_control(
        &mut self,
        session_id: SessionId,
        control: ClientControl,
    ) -> Result<(), String> {
        let (user_id, token, bound, bootstrap_credential_hash) = self
            .sessions
            .get(&session_id)
            .map(|session| {
                (
                    session.user_id,
                    session.tcp_token,
                    session.mls_device.clone(),
                    session.bootstrap_credential_hash.clone(),
                )
            })
            .ok_or_else(|| "unknown session".to_string())?;
        match control {
            ClientControl::FetchDeviceRoster { user_id } => {
                let roster = self.mls.roster(user_id).cloned();
                let initialized = self.mls.initialized(user_id);
                self.send_control_to_token(
                    token,
                    &ServerControl::DeviceRoster {
                        user_id,
                        initialized,
                        roster,
                    },
                )
            }
            ClientControl::PutDeviceRoster { expected, roster } => {
                let bootstrap_credential_hash =
                    expected.is_none().then_some(bootstrap_credential_hash).flatten();
                self.enqueue_mls_write(
                    token,
                    MlsWriteRequest::PutRoster {
                        token,
                        session_id,
                        user_id,
                        expected,
                        roster,
                        bootstrap_credential_hash,
                    },
                )
            }
            ClientControl::BindMlsDevice {
                device_id,
                roster,
                proof,
            } => {
                let current = self
                    .mls
                    .roster(user_id)
                    .ok_or_else(|| "account has no MLS device roster".to_string())?;
                if mls_identity::roster_checkpoint(current) != roster {
                    return Err("MLS device binding names a stale roster".to_string());
                }
                let certificate = current
                    .body
                    .active_devices
                    .iter()
                    .find(|certificate| certificate.body.device_id == device_id)
                    .ok_or_else(|| "MLS device is not active".to_string())?;
                aws_lc_rs::signature::UnparsedPublicKey::new(
                    &aws_lc_rs::signature::ED25519,
                    &certificate.body.mls_signature_public_key,
                )
                .verify(
                    &mls_identity::mls_device_binding_message(session_id, device_id, roster),
                    &proof,
                )
                .map_err(|_| "MLS device binding proof is invalid".to_string())?;
                let client_id = certificate.body.mls_client_id.clone();
                self.sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| "session disappeared during MLS binding".to_string())?
                    .mls_device = Some(BoundMlsDevice {
                    device_id,
                    roster,
                    client_id,
                });
                self.send_control_to_token(
                    token,
                    &ServerControl::MlsDeviceBound {
                        device_id,
                        available_key_packages: self.mls.key_package_count(device_id),
                    },
                )
            }
            ClientControl::PublishKeyPackages {
                device_id,
                packages,
            } => {
                let bound = require_mls_device(bound.as_ref(), device_id)?;
                if !self.pending_mls.insert(token) {
                    return Err("connection already has an MLS delivery operation pending".into());
                }
                if self.mls_worker.enqueue_typed(MlsWriteRequest::PublishKeyPackages {
                    token,
                    user_id,
                    device_id: bound.device_id,
                    packages,
                }) {
                    Ok(())
                } else {
                    self.pending_mls.remove(&token);
                    Err("MLS delivery worker is unavailable".into())
                }
            }
            ClientControl::TakeKeyPackage { device_id } => {
                if !self.pending_mls.insert(token) {
                    return Err("connection already has an MLS delivery operation pending".into());
                }
                if self
                    .mls_worker
                    .enqueue_typed(MlsWriteRequest::TakeKeyPackage { token, device_id })
                {
                    Ok(())
                } else {
                    self.pending_mls.remove(&token);
                    Err("MLS delivery worker is unavailable".into())
                }
            }
            ClientControl::CreateEncryptedRoom {
                descriptor,
                roster_checkpoints,
                initial_commit,
            } => {
                let bound = bound
                    .as_ref()
                    .ok_or_else(|| "bind an active MLS device first".to_string())?;
                let account_id = self.mls_account_id(user_id)?;
                let room_id = descriptor.room_id;
                let existing_group = self
                    .mls
                    .group_info(room_id)
                    .map(|(descriptor, epoch, group_info)| {
                        (descriptor.clone(), epoch, group_info.to_vec())
                    });
                if let Some((existing, epoch, group_info)) = existing_group {
                    if !self.check_room_access(session_id, room_id) {
                        return Err("encrypted room is not accessible".to_string());
                    }
                    return self.send_control_to_token(
                        token,
                        &ServerControl::GroupInfo {
                            room_id,
                            descriptor: Some(existing),
                            epoch,
                            group_info,
                        },
                    );
                }
                self.validate_encrypted_room_descriptor(&descriptor)?;
                let welcome_devices = initial_commit
                    .welcome
                    .as_ref()
                    .map(|welcome| welcome.device_ids.clone())
                    .unwrap_or_default();
                let client_id = bound.client_id.clone();
                self.enqueue_mls_write(
                    token,
                    MlsWriteRequest::CreateRoom {
                        token,
                        creator: account_id,
                        creator_client_id: client_id,
                        descriptor,
                        checkpoints: roster_checkpoints,
                        bundle: initial_commit,
                        welcome_devices,
                    },
                )
            }
            ClientControl::FetchGroupInfo { room_id } => {
                if !self.check_room_access(session_id, room_id) {
                    return Err("encrypted room is not accessible".to_string());
                }
                let (descriptor, epoch, group_info) = self
                    .mls
                    .group_info(room_id)
                    .map(|(descriptor, epoch, info)| {
                        (Some(descriptor.clone()), epoch, info.to_vec())
                    })
                    .unwrap_or((None, 0, Vec::new()));
                self.send_control_to_token(
                    token,
                    &ServerControl::GroupInfo {
                        room_id,
                        descriptor,
                        epoch,
                        group_info,
                    },
                )
            }
            ClientControl::SubmitCommitBundle {
                room_id,
                expected_epoch,
                bundle,
            } => {
                let bound = bound
                    .as_ref()
                    .ok_or_else(|| "bind an active MLS device first".to_string())?;
                self.require_mls_room_member(user_id, room_id)?;
                let welcome_devices = bundle
                    .welcome
                    .as_ref()
                    .map(|welcome| welcome.device_ids.clone())
                    .unwrap_or_default();
                let device_id = bound.device_id;
                let client_id = bound.client_id.clone();
                self.enqueue_mls_write(
                    token,
                    MlsWriteRequest::SubmitCommit {
                        token,
                        room_id,
                        device_id,
                        committer_client_id: client_id,
                        expected_epoch,
                        bundle,
                        welcome_devices,
                    },
                )
            }
            ClientControl::SubmitMlsApplication {
                room_id,
                epoch,
                event_id,
                ciphertext,
            } => {
                let bound = bound
                    .as_ref()
                    .ok_or_else(|| "bind an active MLS device first".to_string())?;
                self.require_mls_room_member(user_id, room_id)?;
                let device_id = bound.device_id;
                if !self.pending_mls.insert(token) {
                    return Err("connection already has an MLS delivery operation pending".into());
                }
                if self.mls_worker.enqueue_typed(MlsWriteRequest::SubmitApplication {
                    token,
                    room_id,
                    device_id,
                    epoch,
                    event_id,
                    ciphertext,
                }) {
                    Ok(())
                } else {
                    self.pending_mls.remove(&token);
                    Err("MLS delivery worker is unavailable".into())
                }
            }
            ClientControl::FetchMlsEvents {
                room_id,
                after_sequence,
                limit,
            } => {
                let bound = bound
                    .as_ref()
                    .ok_or_else(|| "bind an active MLS device first".to_string())?;
                self.require_mls_room_member(user_id, room_id)?;
                let device_id = bound.device_id;
                if after_sequence > 0 {
                    if self.mls.acknowledge(room_id, device_id, after_sequence)?
                        && !self.mls_worker.persist_acknowledgement(room_id, device_id)
                    {
                        return Err("MLS durability worker is unavailable".to_string());
                    }
                }
                let batch = self
                    .mls
                    .event_batch(room_id, after_sequence, usize::from(limit))?;
                self.send_control_to_token(
                    token,
                    &ServerControl::MlsEvents {
                        room_id,
                        events: batch.events,
                        oldest_available_sequence: batch.oldest_available_sequence,
                        head_sequence: batch.head_sequence,
                    },
                )
            }
            ClientControl::FetchMlsWelcome { after_sequence } => {
                let bound = bound
                    .as_ref()
                    .ok_or_else(|| "bind an active MLS device first".to_string())?;
                let device_id = bound.device_id;
                if after_sequence > 0 {
                    if self.mls.acknowledge_welcome(device_id, after_sequence)?
                        && !self
                            .mls_worker
                            .persist_welcome_acknowledgement(device_id)
                    {
                        return Err("MLS durability worker is unavailable".to_string());
                    }
                }
                let welcomes = self.mls.welcomes(device_id, after_sequence)?;
                let head_sequence = self.mls.welcome_head(device_id)?;
                self.send_control_to_token(
                    token,
                    &ServerControl::MlsWelcomes {
                        welcomes,
                        head_sequence,
                    },
                )
            }
            ClientControl::AckMlsEvent { room_id, sequence } => {
                let bound = bound
                    .as_ref()
                    .ok_or_else(|| "bind an active MLS device first".to_string())?;
                self.require_mls_room_member(user_id, room_id)?;
                let device_id = bound.device_id;
                if self.mls.acknowledge(room_id, device_id, sequence)?
                    && !self.mls_worker.persist_acknowledgement(room_id, device_id)
                {
                    return Err("MLS durability worker is unavailable".to_string());
                }
                Ok(())
            }
            ClientControl::AckMlsWelcome { delivery_id } => {
                let bound = bound
                    .as_ref()
                    .ok_or_else(|| "bind an active MLS device first".to_string())?;
                let device_id = bound.device_id;
                if self.mls.acknowledge_welcome(device_id, delivery_id)?
                    && !self
                        .mls_worker
                        .persist_welcome_acknowledgement(device_id)
                {
                    return Err("MLS durability worker is unavailable".to_string());
                }
                Ok(())
            }
            _ => unreachable!("non-MLS control routed to MLS handler"),
        }
    }

    fn mls_account_id(&self, user_id: UserId) -> Result<rpc::ids::AccountId, String> {
        self.mls
            .roster(user_id)
            .map(|roster| roster.body.account_id)
            .ok_or_else(|| "account has no MLS device roster".to_string())
    }

    fn require_mls_room_member(&self, user_id: UserId, room_id: RoomId) -> Result<(), String> {
        let account_id = self.mls_account_id(user_id)?;
        if self.mls.room_account_member(room_id, account_id) {
            Ok(())
        } else {
            Err("account is not a member of the encrypted room".to_string())
        }
    }

    fn mls_room_tokens(&self, room_id: RoomId) -> Vec<Token> {
        self.sessions
            .values()
            .filter_map(|session| {
                let account_id = self.mls.roster(session.user_id)?.body.account_id;
                (self.mls.room_account_member(room_id, account_id)
                    && self.clients.contains_key(&session.tcp_token))
                .then_some(session.tcp_token)
            })
            .collect()
    }

    fn validate_encrypted_room_descriptor(
        &self,
        descriptor: &rpc::mls::EncryptedRoomDescriptor,
    ) -> Result<(), String> {
        let mut members = descriptor
            .member_accounts
            .iter()
            .map(|account| {
                self.mls
                    .user_for_account(*account)
                    .ok_or_else(|| "encrypted room account has no current roster".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        members.sort_unstable();
        let room = self
            .rooms
            .get(&descriptor.room_id)
            .ok_or_else(|| "create the Chatt room before its MLS group".to_string())?;
        let mut expected = match &room.access {
            RoomAccess::Dm(user_a, user_b) => vec![*user_a, *user_b],
            RoomAccess::Private(users) => users.iter().copied().collect(),
            RoomAccess::Public => {
                return Err(
                    "public rooms cannot be replaced by an encrypted descriptor".to_string()
                );
            }
        };
        expected.sort_unstable();
        if members != expected {
            return Err(
                "encrypted descriptor does not match the Chatt room membership".to_string(),
            );
        }
        Ok(())
    }

    fn push_mls_welcome_deliveries(
        &mut self,
        deliveries: Vec<(DeviceId, Vec<rpc::mls::MlsWelcome>, u64)>,
    ) {
        for (device_id, welcomes, head_sequence) in deliveries {
            let tokens = self
                .sessions
                .values()
                .filter_map(|session| {
                    (session
                        .mls_device
                        .as_ref()
                        .is_some_and(|bound| bound.device_id == device_id))
                    .then_some(session.tcp_token)
                })
                .collect::<Vec<_>>();
            for token in tokens {
                let _ = self.send_control_to_token(
                    token,
                    &ServerControl::MlsWelcomes {
                        welcomes: welcomes.clone(),
                        head_sequence,
                    },
                );
            }
        }
    }

    fn create_device_link(
        &mut self,
        session_id: SessionId,
        redemption_secret_hash: Vec<u8>,
        enrollment_bundle: Vec<u8>,
    ) -> Result<(), String> {
        let (user_id, tcp_token, is_bound) = self
            .sessions
            .get(&session_id)
            .map(|session| {
                (
                    session.user_id,
                    session.tcp_token,
                    session.mls_device.is_some(),
                )
            })
            .ok_or_else(|| "unknown session".to_string())?;
        if !is_bound {
            return Err("bind an active MLS device before creating a device link".to_string());
        }
        self.expire_device_links();
        if self.device_links.contains_key(&redemption_secret_hash) {
            return Err("device-link redemption secret is already active".to_string());
        }
        // Creating a link is replacement, not reopening: a newly generated
        // secret invalidates every older outstanding link for this account.
        self.device_links.retain(|_, link| link.user_id != user_id);
        let expires_at_ms = now_ms().saturating_add(DEVICE_LINK_TTL.as_millis() as u64);
        self.device_links.insert(
            redemption_secret_hash.clone(),
            DeviceLinkState {
                user_id,
                enrollment_bundle,
                current_roster: self
                    .mls
                    .roster(user_id)
                    .cloned()
                    .ok_or_else(|| "device-link account has no MLS roster".to_string())?,
                expires_at: Instant::now() + DEVICE_LINK_TTL,
                redemption: DeviceLinkRedemption::Active,
            },
        );
        self.send_control_to_token(
            tcp_token,
            &ServerControl::DeviceLinkCreated {
                redemption_secret_hash,
                expires_at_ms,
            },
        )
    }

    fn cancel_device_link(
        &mut self,
        session_id: SessionId,
        redemption_secret_hash: Vec<u8>,
    ) -> Result<(), String> {
        let (user_id, token, bound) = self
            .sessions
            .get(&session_id)
            .map(|session| {
                (
                    session.user_id,
                    session.tcp_token,
                    session.mls_device.is_some(),
                )
            })
            .ok_or_else(|| "unknown session".to_string())?;
        if !bound {
            return Err("bind an active MLS device before canceling a device link".to_string());
        }
        self.expire_device_links();
        let link = self
            .device_links
            .get(&redemption_secret_hash)
            .ok_or_else(|| "device link is invalid, expired, canceled, or replaced".to_string())?;
        if link.user_id != user_id {
            return Err("device link belongs to another account".to_string());
        }
        if !matches!(link.redemption, DeviceLinkRedemption::Active) {
            return Err("device link was already redeemed and cannot be canceled".to_string());
        }
        self.device_links.remove(&redemption_secret_hash);
        self.send_control_to_token(
            token,
            &ServerControl::DeviceLinkCanceled {
                redemption_secret_hash,
            },
        )
    }

    fn fetch_device_link(&mut self, token: Token, redemption_secret: &str) -> Result<(), String> {
        self.expire_device_links();
        let secret_hash = device_link_secret_hash(redemption_secret);
        let Some(link) = self.device_links.get(&secret_hash) else {
            return self.reject_auth(
                token,
                ERROR_DEVICE_LINK_UNAVAILABLE,
                "device link is invalid, expired, canceled, or already used".to_string(),
            );
        };
        let username = self
            .usernames
            .username_for(link.user_id)
            .ok_or_else(|| "device-link account has no registered username".to_string())?
            .to_string();
        let current_roster = link.current_roster.clone();
        self.send_control_to_token(
            token,
            &ServerControl::DeviceLinkBundle {
                enrollment_bundle: link.enrollment_bundle.clone(),
                current_roster,
                user_id: link.user_id,
                username,
            },
        )
    }

    fn redeem_device_link(
        &mut self,
        token: Token,
        redemption_secret: &str,
        attempt_id: rpc::ids::PairAttemptId,
        expected_roster: mls_identity::RosterCheckpoint,
        roster: mls_identity::SignedDeviceRoster,
        key_packages: Vec<rpc::mls::PublishedKeyPackage>,
        bearer_token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        self.expire_device_links();
        let secret_hash = device_link_secret_hash(redemption_secret);
        let Some(link) = self.device_links.get(&secret_hash) else {
            return self.reject_auth(
                token,
                ERROR_DEVICE_LINK_UNAVAILABLE,
                "device link is invalid, expired, canceled, or already used".to_string(),
            );
        };
        let (user_id, username, redemption) = (
            link.user_id,
            self.usernames
                .username_for(link.user_id)
                .ok_or_else(|| "device-link account has no registered username".to_string())?
                .to_string(),
            link.redemption.clone(),
        );
        let added = match &redemption {
            DeviceLinkRedemption::Active => roster.body.active_devices.iter().find(|certificate| {
                self.mls.roster(user_id).is_some_and(|current| {
                    !current
                        .body
                        .active_devices
                        .iter()
                        .any(|old| old.body.device_id == certificate.body.device_id)
                })
            }),
            DeviceLinkRedemption::Redeemed { device_id, .. } => roster
                .body
                .active_devices
                .iter()
                .find(|certificate| certificate.body.device_id == *device_id),
        }
        .ok_or_else(|| "device-link roster does not add the expected device".to_string())?;
        let (device_id, device_name) = (added.body.device_id, added.body.device_name.clone());
        let (credential_hash, first_redemption) = match redemption {
            DeviceLinkRedemption::Active => (hash_secret(&bearer_token), true),
            DeviceLinkRedemption::Redeemed {
                device_id: redeemed_device,
                credential_hash,
                attempt_id: redeemed_attempt,
            } if redeemed_device == device_id
                && redeemed_attempt == attempt_id
                && crate::config::verify_secret_hash(&credential_hash, &bearer_token) =>
            {
                (credential_hash, false)
            }
            DeviceLinkRedemption::Redeemed { .. } => {
                return self.reject_auth(
                    token,
                    ERROR_DEVICE_LINK_UNAVAILABLE,
                    "device link was already used by another redemption".to_string(),
                );
            }
        };
        if first_redemption {
            return self.enqueue_mls_write(
                token,
                MlsWriteRequest::RedeemDeviceLink {
                    token,
                    secret_hash,
                    user_id,
                    username,
                    expected: expected_roster,
                    roster,
                    device_id,
                    device_name,
                    credential_hash,
                    attempt_id,
                    packages: key_packages,
                    bearer_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            );
        }

        self.finish_device_link_redemption(
            token,
            secret_hash,
            user_id,
            username,
            device_id,
            device_name,
            bearer_token,
            receive_files,
            file_receive_limit_bytes,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_device_link_redemption(
        &mut self,
        token: Token,
        secret_hash: Vec<u8>,
        user_id: UserId,
        username: String,
        device_id: DeviceId,
        device_name: String,
        bearer_token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
        first_redemption: bool,
    ) -> Result<(), String> {
        let account_tokens = self
            .sessions
            .values()
            .filter(|session| session.user_id == user_id)
            .map(|session| session.tcp_token)
            .collect::<Vec<_>>();
        if first_redemption {
            self.send_control_to_tokens(
                &account_tokens,
                &ServerControl::DeviceLinkRedeemed {
                    redemption_secret_hash: secret_hash.clone(),
                    device_id,
                    device_name,
                },
            );
            if let Some(roster) = self.mls.roster(user_id).cloned() {
                let roster_tokens = self
                    .sessions
                    .values()
                    .map(|session| session.tcp_token)
                    .filter(|token| self.clients.contains_key(token))
                    .collect::<Vec<_>>();
                self.send_control_to_tokens(
                    &roster_tokens,
                    &ServerControl::DeviceRoster {
                        user_id,
                        initialized: true,
                        roster: Some(roster),
                    },
                );
            }
        }

        let user = self
            .users
            .users
            .iter()
            .find(|user| user.id == user_id)
            .cloned()
            .unwrap_or_else(|| Self::dynamic_user(user_id, &username));
        self.establish_session_with_credential(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            true,
            Some(IssuedSessionToken::DeviceLink(bearer_token)),
            None,
        )
    }

    fn expire_device_links(&mut self) {
        let now = Instant::now();
        self.device_links.retain(|_, link| link.expires_at > now);
    }

    fn authenticate_client(
        &mut self,
        token: Token,
        username: &str,
        auth_token: &str,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        kvlog::info!("authenticate attempt", token = token.0);
        if let Some((user_id, _device_id, _credential_hash)) =
            self.mls.authenticate_credential(auth_token)
        {
            let username = username.trim();
            if !valid_username(username) || !self.usernames.is_available(username, Some(user_id)) {
                return self.reject_auth(
                    token,
                    ERROR_AUTH_REJECTED,
                    "authentication failed: invalid username for this MLS device".to_string(),
                );
            }
            let user = self
                .users
                .users
                .iter()
                .find(|user| user.id == user_id)
                .cloned()
                .unwrap_or_else(|| Self::dynamic_user(user_id, username));
            return self.establish_session_with_credential(
                token,
                &user,
                receive_files,
                file_receive_limit_bytes,
                true,
                None,
                None,
            );
        }
        if auth_token.starts_with(DYNAMIC_TOKEN_PREFIX) {
            let claims = verify_dynamic_token(
                &self.config.security.server_identity_seed,
                auth_token,
            )
            .ok()
            .filter(|claims| is_dynamic_user_id(claims.user_id));
            let Some(claims) = claims else {
                return self.reject_auth(
                    token,
                    ERROR_AUTH_REJECTED,
                    "authentication failed: invalid public bearer token".to_string(),
                );
            };
            if !self.config.is_public() || claims.password_epoch != self.config.password_epoch() {
                return self.reject_auth(
                    token,
                    ERROR_TOKEN_STALE_EPOCH,
                    "authentication failed: the public-server credential is no longer current"
                        .to_string(),
                );
            }
            if self.mls.initialized(claims.user_id) {
                return self.reject_auth(
                    token,
                    ERROR_AUTH_REJECTED,
                    "authentication failed: use an active MLS device credential".to_string(),
                );
            }
            let username = username.trim();
            if !valid_username(username)
                || !self.usernames.is_available(username, Some(claims.user_id))
            {
                return self.reject_auth(
                    token,
                    ERROR_AUTH_REJECTED,
                    "authentication failed: invalid username for this account".to_string(),
                );
            }
            let user = Self::dynamic_user(claims.user_id, username);
            if self.usernames.needs_dynamic_claim(claims.user_id, username) {
                if let Some(path) = self.usernames.persistence_path() {
                    return self.begin_identity_write(
                        token,
                        IdentityWrite::DynamicUsername {
                            path,
                            user_id: claims.user_id,
                            username: username.to_string(),
                        },
                        PendingIdentity::DynamicAuthentication {
                            username: username.to_string(),
                            establish: PendingEstablish {
                                user,
                                receive_files,
                                file_receive_limit_bytes,
                                announce: true,
                                issued_token: None,
                                bootstrap_credential_hash: Some(hash_secret(auth_token)),
                            },
                        },
                        "authentication failed: the server could not persist the username; retry later",
                    );
                }
                self.usernames.claim_dynamic(claims.user_id, username)?;
            }
            return self.establish_session_with_credential(
                token,
                &user,
                receive_files,
                file_receive_limit_bytes,
                true,
                None,
                Some(hash_secret(auth_token)),
            );
        }
        let Some(user) = self
            .users
            .users
            .iter()
            .find(|candidate| {
                !candidate.token_hash.trim().is_empty()
                    && verify_secret_hash(&candidate.token_hash, auth_token)
            })
            .cloned()
        else {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                reason = "invalid_token"
            );
            return self.reject_auth(
                token,
                ERROR_AUTH_REJECTED,
                "authentication failed: the token is not valid for this server".to_string(),
            );
        };
        let username = username.trim();
        if !valid_username(username) {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                user_id = user.id.0,
                reason = "invalid_username"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "authentication failed: username must be 1-64 bytes with no control characters"
                    .to_string(),
            );
        }
        if !self.usernames.is_available(username, Some(user.id)) {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                user_id = user.id.0,
                reason = "username_taken"
            );
            return self.reject_username_taken(token);
        }
        if self.mls.initialized(user.id) {
            return self.reject_auth(
                token,
                ERROR_AUTH_REJECTED,
                "authentication failed: use an active MLS device credential".to_string(),
            );
        }
        let user = if username == user.username {
            user
        } else {
            match self
                .users
                .prepare_set_user_username(user.id, username.to_string())
            {
                Ok((updated, users)) => {
                    if let Some(path) = self.users.persistence_path() {
                        let credential_hash = user.token_hash.clone();
                        return self.begin_identity_write(
                            token,
                            IdentityWrite::UsersToml {
                                path,
                                snapshot: UserStore::snapshot(&users),
                            },
                            PendingIdentity::ExplicitRename {
                                users,
                                updated: PendingEstablish {
                                    user: updated,
                                    receive_files,
                                    file_receive_limit_bytes,
                                    announce: true,
                                    issued_token: None,
                                    bootstrap_credential_hash: Some(credential_hash.clone()),
                                },
                                fallback: PendingEstablish {
                                    user,
                                    receive_files,
                                    file_receive_limit_bytes,
                                    announce: true,
                                    issued_token: None,
                                    bootstrap_credential_hash: Some(credential_hash),
                                },
                            },
                            "authentication failed: the server could not persist the username; retry later",
                        );
                    }
                    self.users.install_users(users);
                    self.usernames.set_explicit(updated.id, &updated.username);
                    updated
                }
                Err(error) => {
                    kvlog::warn!(
                        "username update failed",
                        user_id = user.id.0,
                        error = error.as_str()
                    );
                    user
                }
            }
        };
        self.establish_session_with_credential(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            true,
            None,
            Some(user.token_hash.clone()),
        )
    }

    /// Builds a transient [`UserConfig`] for a dynamic (open-paired) user, whose
    /// details are never stored server-side. The user id doubles as the internal
    /// identifier since there is no username.
    fn dynamic_user(user_id: UserId, username: &str) -> UserConfig {
        let username = username.trim();
        let username = if valid_username(username) {
            username.to_string()
        } else {
            user_id.to_string()
        };
        UserConfig {
            id: user_id,
            internal_reference: user_id.to_string(),
            username,
            token_hash: String::new(),
        }
    }

    fn pair_client(
        &mut self,
        token: Token,
        username: &str,
        pairing_code: &str,
        new_token: &str,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        kvlog::info!("pairing attempt", token = token.0);
        self.expire_invites();
        let Some(user_name) = self
            .invites
            .iter()
            .find(|(_, invite)| verify_secret_hash(&invite.pairing_code_hash, pairing_code))
            .map(|(name, _)| name.clone())
        else {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                reason = "no_active_invite"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_NOT_ACTIVE,
                "pairing failed: no active invite matches this join string; the invite may have expired, been replaced, or already been used. Ask the admin to issue a new one".to_string(),
            );
        };
        let username = username.trim();
        if !valid_username(username) {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                user = user_name.as_str(),
                reason = "invalid_username"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "pairing failed: username must be 1-64 bytes with no control characters"
                    .to_string(),
            );
        }

        let token_hash = hash_secret(new_token);
        if self
            .users
            .users
            .iter()
            .any(|user| user.internal_reference != user_name && user.token_hash == token_hash)
        {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                user = user_name.as_str(),
                reason = "token_collision"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "pairing failed: the generated token is already in use; retry pairing".to_string(),
            );
        }
        // Claimant is this invite's existing record, if it already paired once,
        // so re-pairing with the same username is allowed.
        let claimant = self
            .users
            .users
            .iter()
            .find(|user| user.internal_reference == user_name)
            .map(|user| user.id);
        if !self.usernames.is_available(username, claimant) {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                user = user_name.as_str(),
                reason = "username_taken"
            );
            return self.reject_username_taken(token);
        }
        let (user, users) =
            match self.users.prepare_mark_user_paired(
                &user_name,
                username.to_string(),
                token_hash.clone(),
            ) {
                Ok(prepared) => prepared,
                Err(error) => {
                    kvlog::error!(
                        "pairing rejected",
                        token = token.0,
                        user = user_name.as_str(),
                        reason = "state_write_failed",
                        error = error.as_str()
                    );
                    return self.reject_auth(
                        token,
                        control::ERROR_INTERNAL,
                        "pairing failed: the server could not persist the pairing; retry pairing"
                            .to_string(),
                    );
                }
            };
        if let Some(path) = self.users.persistence_path() {
            return self.begin_identity_write(
                token,
                IdentityWrite::UsersToml {
                    path,
                    snapshot: UserStore::snapshot(&users),
                },
                PendingIdentity::InvitePairing {
                    invite_name: user_name,
                    users,
                    establish: PendingEstablish {
                        user,
                        receive_files,
                        file_receive_limit_bytes,
                        announce: false,
                        issued_token: None,
                        bootstrap_credential_hash: Some(token_hash),
                    },
                },
                "pairing failed: the server could not persist the pairing; retry pairing",
            );
        }
        self.users.install_users(users);
        self.invites.remove(&user_name);
        self.usernames.set_explicit(user.id, &user.username);
        kvlog::info!(
            "pairing accepted",
            token = token.0,
            user_id = user.id.0,
            user = user.internal_reference.as_str()
        );
        self.establish_session_with_credential(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            false,
            None,
            Some(token_hash),
        )
    }

    /// Self-service join on a public server. Verifies the password, allocates (or
    /// recovers from `existing_token`) a dynamic user id, and issues a fresh
    /// bearer token bound to the current password epoch.
    fn open_pair_client(
        &mut self,
        token: Token,
        username: &str,
        password: &str,
        existing_token: &str,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        kvlog::info!("open pair attempt", token = token.0);
        if !self.config.is_public() {
            kvlog::warn!(
                "open pair rejected",
                token = token.0,
                reason = "public_disabled"
            );
            return self.reject_auth(
                token,
                ERROR_PUBLIC_DISABLED,
                "open pairing is disabled on this server".to_string(),
            );
        }
        if !self.check_open_pair_attempt_rate(token)? {
            return Ok(());
        }
        let required_hash = self.config.password_hash().map(str::to_string);
        let mut current_password_verified = false;
        if let Some(required_hash) = required_hash {
            if password.is_empty() {
                return self.reject_auth(
                    token,
                    ERROR_PASSWORD_REQUIRED,
                    "open pairing requires a password".to_string(),
                );
            }
            if !verify_secret_hash(&required_hash, password) {
                kvlog::warn!(
                    "open pair rejected",
                    token = token.0,
                    reason = "password_mismatch"
                );
                return self.reject_auth(
                    token,
                    ERROR_PASSWORD_MISMATCH,
                    "open pairing password is incorrect".to_string(),
                );
            }
            current_password_verified = true;
        }
        let username = username.trim();
        if !valid_username(username) {
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "open pairing failed: username must be 1-64 bytes with no control characters"
                    .to_string(),
            );
        }
        let seed = self.config.security.server_identity_seed.clone();
        let current_epoch = self.config.password_epoch();
        let verified_claims = (!existing_token.is_empty())
            .then(|| verify_dynamic_token(&seed, existing_token).ok())
            .flatten()
            .filter(|claims| is_dynamic_user_id(claims.user_id));
        let token_user_id = verified_claims
            .filter(|claims| {
                claims.password_epoch == current_epoch || current_password_verified
            })
            .map(|claims| claims.user_id);
        let recovery_user_id = existing_token
            .starts_with(OPEN_PAIR_RECOVERY_PREFIX)
            .then(|| dynamic_user_id_from_recovery_token(&seed, existing_token).ok())
            .flatten();
        if existing_token.starts_with(OPEN_PAIR_RECOVERY_PREFIX) && recovery_user_id.is_none() {
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "open pairing failed: recovery token is invalid".to_string(),
            );
        }
        let existing_user_id = token_user_id.or(recovery_user_id);
        let Some(user_id) = existing_user_id else {
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "open pairing failed: a recovery token is required".to_string(),
            );
        };
        // Reject a taken username before charging the allocation rate limit. A
        // recovered or returning user id may keep its own name.
        if !self.usernames.is_available(username, Some(user_id)) {
            kvlog::warn!(
                "open pair rejected",
                token = token.0,
                reason = "username_taken"
            );
            return self.reject_username_taken(token);
        }
        if token_user_id.is_none() && !self.usernames.contains_user(user_id) {
            if !self.check_open_pair_allocation_rate(token)? {
                return Ok(());
            }
        }
        let issued = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id,
                password_epoch: current_epoch,
            },
        )
        .map_err(|error| error.to_string())?;
        let user = Self::dynamic_user(user_id, username);
        let bootstrap_credential_hash = hash_secret(&issued);
        if self.usernames.needs_dynamic_claim(user_id, username) {
            if let Some(path) = self.usernames.persistence_path() {
                return self.begin_identity_write(
                    token,
                    IdentityWrite::DynamicUsername {
                        path,
                        user_id,
                        username: username.to_string(),
                    },
                    PendingIdentity::OpenPairing {
                        username: username.to_string(),
                        establish: PendingEstablish {
                            user,
                            receive_files,
                            file_receive_limit_bytes,
                            announce: false,
                            issued_token: Some(IssuedSessionToken::OpenPair(issued)),
                            bootstrap_credential_hash: Some(bootstrap_credential_hash),
                        },
                    },
                    "open pairing failed: the server could not persist state; retry later",
                );
            }
            if let Err(error) = self.usernames.claim_dynamic(user_id, username) {
                kvlog::error!(
                    "open pair rejected",
                    token = token.0,
                    user_id = user_id.0,
                    reason = "state_write_failed",
                    error = error.as_str()
                );
                return self.reject_auth(
                    token,
                    control::ERROR_INTERNAL,
                    "open pairing failed: the server could not persist state; retry later"
                        .to_string(),
                );
            }
        }
        kvlog::info!("open pair accepted", token = token.0, user_id = user_id.0);
        self.establish_session_with_credential(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            false,
            Some(IssuedSessionToken::OpenPair(issued)),
            Some(bootstrap_credential_hash),
        )
    }

    fn check_open_pair_attempt_rate(&mut self, token: Token) -> Result<bool, String> {
        let now = Instant::now();
        self.prune_open_pair_rate_windows(now);
        let global_count = self.open_pair_global_allocations.len();
        if global_count >= OPEN_PAIR_GLOBAL_LIMIT {
            kvlog::warn!(
                "open pair rejected",
                token = token.0,
                reason = "rate_limited",
                global_count
            );
            return self
                .reject_auth(
                    token,
                    ERROR_OPEN_PAIR_RATE_LIMITED,
                    "open pairing is rate limited; retry later".to_string(),
                )
                .map(|()| false);
        }
        self.open_pair_global_allocations.push_back(now);
        Ok(true)
    }

    fn check_open_pair_allocation_rate(&mut self, token: Token) -> Result<bool, String> {
        let Some(source_ip) = self.clients.get(&token).map(|client| client.addr.ip()) else {
            return Ok(true);
        };
        let now = Instant::now();
        self.prune_open_pair_rate_windows(now);
        let ip_count = self
            .open_pair_ip_allocations
            .get(&source_ip)
            .map(VecDeque::len)
            .unwrap_or(0);
        if ip_count >= OPEN_PAIR_PER_IP_LIMIT {
            kvlog::warn!(
                "open pair rejected",
                token = token.0,
                reason = "rate_limited",
                source_ip = %source_ip,
                ip_count
            );
            return self
                .reject_auth(
                    token,
                    ERROR_OPEN_PAIR_RATE_LIMITED,
                    "open pairing is rate limited; retry later".to_string(),
                )
                .map(|()| false);
        }
        self.open_pair_ip_allocations
            .entry(source_ip)
            .or_default()
            .push_back(now);
        Ok(true)
    }

    fn prune_open_pair_rate_windows(&mut self, now: Instant) {
        prune_instants(
            &mut self.open_pair_global_allocations,
            now,
            OPEN_PAIR_RATE_WINDOW,
        );
        self.open_pair_ip_allocations.retain(|_, allocations| {
            prune_instants(allocations, now, OPEN_PAIR_RATE_WINDOW);
            !allocations.is_empty()
        });
    }

    fn reject_username_taken(&mut self, token: Token) -> Result<(), String> {
        self.reject_auth(
            token,
            ERROR_USERNAME_TAKEN,
            "username already in use; choose another".to_string(),
        )
    }

    fn identity_pending(&self, token: Token) -> bool {
        self.pending_identity
            .as_ref()
            .is_some_and(|(pending_token, _)| *pending_token == token)
            || self.deferred_identity_tokens.contains(&token)
    }

    fn resume_deferred_identity_controls(&mut self) {
        while self.pending_identity.is_none() {
            let Some((token, control)) = self.deferred_identity_controls.pop_front() else {
                return;
            };
            self.deferred_identity_tokens.remove(&token);
            if self.clients.get(&token).map(|client| client.state) != Some(ConnState::AwaitAuth)
                || self.clients.get(&token).is_some_and(ClientConn::is_closing)
            {
                continue;
            }

            let kind = client_control_kind(&control);
            let started = Instant::now();
            let result = self.handle_control(token, control);
            self.loop_stats.observe_control(kind, started.elapsed());
            if let Err(error) = result {
                kvlog::warn!(
                    "deferred tcp client protocol error",
                    token = token.0,
                    error = %error
                );
                self.disconnect(token);
                continue;
            }
            self.queue_client_read_if_work_ready(token);
        }
    }

    fn begin_identity_write(
        &mut self,
        token: Token,
        write: IdentityWrite,
        pending: PendingIdentity,
        failure_message: &str,
    ) -> Result<(), String> {
        if self.pending_identity.is_some() {
            return self.reject_auth(
                token,
                control::ERROR_INTERNAL,
                "identity persistence is busy; retry shortly".to_string(),
            );
        }
        match self
            .identity_writer
            .enqueue(IdentityWriteRequest { token, write })
        {
            Ok(()) => {
                self.pending_identity = Some((token, pending));
                Ok(())
            }
            Err(IdentityEnqueueError::Full | IdentityEnqueueError::Gone) => {
                kvlog::error!("identity persistence unavailable", token = token.0);
                self.reject_auth(
                    token,
                    control::ERROR_INTERNAL,
                    failure_message.to_string(),
                )
            }
        }
    }

    fn finish_pending_establish(&mut self, token: Token, establish: PendingEstablish) {
        if let Err(error) = self.establish_session_with_credential(
            token,
            &establish.user,
            establish.receive_files,
            establish.file_receive_limit_bytes,
            establish.announce,
            establish.issued_token,
            establish.bootstrap_credential_hash,
        ) {
            kvlog::warn!(
                "identity completion could not establish session",
                token = token.0,
                error = error.as_str()
            );
        }
    }

    fn drain_identity_replies(&mut self) {
        let mut replies = self.identity_events.drain_up_to(8);
        while let Some(reply) = replies.pop_front() {
            let Some((token, pending)) = self.pending_identity.take() else {
                kvlog::warn!("unexpected identity persistence reply", token = reply.token.0);
                continue;
            };
            if token != reply.token {
                kvlog::error!(
                    "identity persistence reply token mismatch",
                    expected = token.0,
                    actual = reply.token.0
                );
            }
            match pending {
                PendingIdentity::DynamicAuthentication {
                    username,
                    establish,
                } => match reply.result {
                    Ok(()) => {
                        let user_id = establish.user.id;
                        match self.usernames.apply_dynamic_claim(user_id, &username) {
                            Ok(()) => self.finish_pending_establish(token, establish),
                            Err(error) => {
                                kvlog::error!(
                                    "persisted dynamic username could not be applied",
                                    user_id = user_id.0,
                                    error = error.as_str()
                                );
                                let _ = self.reject_auth(
                                    token,
                                    control::ERROR_INTERNAL,
                                    "authentication failed: identity state conflicted; retry"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    Err(error) => {
                        kvlog::error!(
                            "dynamic authentication persistence failed",
                            token = token.0,
                            error = error.as_str()
                        );
                        let _ = self.reject_auth(
                            token,
                            control::ERROR_INTERNAL,
                            "authentication failed: the server could not persist the username; retry later"
                                .to_string(),
                        );
                    }
                },
                PendingIdentity::ExplicitRename {
                    users,
                    updated,
                    fallback,
                } => match reply.result {
                    Ok(()) => {
                        self.users.install_users(users);
                        self.usernames
                            .set_explicit(updated.user.id, &updated.user.username);
                        self.finish_pending_establish(token, updated);
                    }
                    Err(error) => {
                        kvlog::warn!(
                            "username update failed",
                            user_id = fallback.user.id.0,
                            error = error.as_str()
                        );
                        self.finish_pending_establish(token, fallback);
                    }
                },
                PendingIdentity::InvitePairing {
                    invite_name,
                    users,
                    establish,
                } => match reply.result {
                    Ok(()) => {
                        self.users.install_users(users);
                        self.usernames
                            .set_explicit(establish.user.id, &establish.user.username);
                        self.invites.remove(&invite_name);
                        kvlog::info!(
                            "pairing accepted",
                            token = token.0,
                            user_id = establish.user.id.0,
                            user = invite_name.as_str()
                        );
                        self.finish_pending_establish(token, establish);
                    }
                    Err(error) => {
                        kvlog::error!(
                            "pairing rejected",
                            token = token.0,
                            user = invite_name.as_str(),
                            reason = "state_write_failed",
                            error = error.as_str()
                        );
                        let _ = self.reject_auth(
                            token,
                            control::ERROR_INTERNAL,
                            "pairing failed: the server could not persist the pairing; retry pairing"
                                .to_string(),
                        );
                    }
                },
                PendingIdentity::OpenPairing {
                    username,
                    establish,
                } => match reply.result {
                    Ok(()) => {
                        let user_id = establish.user.id;
                        match self.usernames.apply_dynamic_claim(user_id, &username) {
                            Ok(()) => {
                                kvlog::info!(
                                    "open pair accepted",
                                    token = token.0,
                                    user_id = user_id.0
                                );
                                self.finish_pending_establish(token, establish);
                            }
                            Err(error) => {
                                kvlog::error!(
                                    "persisted open-pair username could not be applied",
                                    user_id = user_id.0,
                                    error = error.as_str()
                                );
                                let _ = self.reject_auth(
                                    token,
                                    control::ERROR_INTERNAL,
                                    "open pairing failed: identity state conflicted; retry"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    Err(error) => {
                        kvlog::error!(
                            "open pair rejected",
                            token = token.0,
                            reason = "state_write_failed",
                            error = error.as_str()
                        );
                        let _ = self.reject_auth(
                            token,
                            control::ERROR_INTERNAL,
                            "open pairing failed: the server could not persist state; retry later"
                                .to_string(),
                        );
                    }
                },
            }
            self.queue_client_read_if_work_ready(token);
        }
        self.resume_deferred_identity_controls();
    }

    fn reject_auth(&mut self, token: Token, code: u16, message: String) -> Result<(), String> {
        self.send_control_to_token(token, &ServerControl::Error { code, message })?;
        if let Some(client) = self.clients.get_mut(&token) {
            client.begin_graceful_close(Instant::now());
        }
        self.write_client(token);
        Ok(())
    }

    #[cfg(test)]
    fn establish_session(
        &mut self,
        token: Token,
        user: &UserConfig,
        receive_files: bool,
        file_receive_limit_bytes: u64,
        announce: bool,
        issued_token: Option<String>,
    ) -> Result<(), String> {
        self.establish_session_with_credential(
            token,
            user,
            receive_files,
            file_receive_limit_bytes,
            announce,
            issued_token.map(IssuedSessionToken::OpenPair),
            None,
        )
    }

    fn establish_session_with_credential(
        &mut self,
        token: Token,
        user: &UserConfig,
        receive_files: bool,
        file_receive_limit_bytes: u64,
        announce: bool,
        issued_token: Option<IssuedSessionToken>,
        bootstrap_credential_hash: Option<String>,
    ) -> Result<(), String> {
        let user_id = user.user_id();

        let session_id = SessionId(self.next_session);
        self.next_session += 1;
        let username = user.username.clone();

        let transport = self
            .clients
            .get_mut(&token)
            .and_then(|client| client.transport.take())
            .ok_or_else(|| "session transport missing after handshake".to_string())?;
        self.register_media_route(transport.route_id, session_id)?;
        self.voice_relay.submit(VoiceCommand::RegisterSession {
            session_id,
            user_id,
            protection: media::MediaProtection::from_transport(&transport),
        });
        self.sessions.insert(
            session_id,
            Session {
                user_id,
                username: username.clone(),
                tcp_token: token,
                voice_room: None,
                transport,
                active_stream: None,
                voice_status: control::ParticipantVoiceStatus::default(),
                reported_server_rtt_ms: None,
                server_rtt_reported_at: None,
                p2p: None,
                receive_files,
                file_receive_limit_bytes,
                announced: announce,
                connected_at_ms: now_ms(),
                pending_disk_history_fetches: 0,
                mls_device: None,
                bootstrap_credential_hash,
            },
        );

        if let Some(client) = self.clients.get_mut(&token) {
            client.state = ConnState::Ready;
            client.session_id = Some(session_id);
            client.user_id = Some(user_id);
        }

        kvlog::info!(
            "authenticate accepted",
            token = token.0,
            session_id = session_id.0,
            user_id = user_id.0,
            receive_files,
            file_receive_limit_bytes
        );
        let rooms = self.accessible_room_infos(user_id);
        let users = self.user_summaries();
        let response = match issued_token {
            Some(IssuedSessionToken::OpenPair(token)) => ServerControl::OpenPaired {
                token,
                udp_addr: self.config.network.public_udp_addr.clone(),
                udp_probe_addr: self.config.network.public_udp_probe_addr.clone(),
                session_id,
                user_id,
                rooms,
                users,
                default_room: self.default_room,
            },
            Some(IssuedSessionToken::DeviceLink(token)) => ServerControl::DeviceLinked {
                token,
                username: username.clone(),
                udp_addr: self.config.network.public_udp_addr.clone(),
                udp_probe_addr: self.config.network.public_udp_probe_addr.clone(),
                session_id,
                user_id,
                rooms,
                users,
                default_room: self.default_room,
            },
            None => ServerControl::Authenticated {
                session_id,
                user_id,
                rooms,
                users,
                default_room: self.default_room,
            },
        };
        self.send_control_to_token(token, &response)?;
        let first_announced_session = self
            .sessions
            .values()
            .filter(|session| session.user_id == user_id && session.announced)
            .count()
            == 1;
        if announce && first_announced_session && self.live_token_for_session(session_id).is_some()
        {
            self.broadcast_presence(session_id, true);
        }
        Ok(())
    }

    /// Registers a session's derived media route id for UDP demultiplexing.
    ///
    /// A 32-bit route id colliding with another live session's is astronomically
    /// unlikely, but silently overwriting would strand both sessions' UDP once
    /// either tears down (`teardown_session` removes the shared entry). The new
    /// session is rejected instead, forcing a reconnect whose fresh handshake
    /// draws a new route id. Stale entries whose session is already gone are
    /// reclaimed.
    fn register_media_route(&mut self, route_id: u32, session_id: SessionId) -> Result<(), String> {
        if let Some(existing) = self.reserved_media_routes.get(&route_id).copied()
            && existing != session_id
            && self.sessions.contains_key(&existing)
        {
            kvlog::error!(
                "media route id collision with a live session",
                route_id,
                existing_session_id = existing.0,
                rejected_session_id = session_id.0
            );
            return Err("media route id collision".to_string());
        }
        self.reserved_media_routes.insert(route_id, session_id);
        Ok(())
    }

    /// Every room `user_id` can access, in stable id order.
    fn accessible_room_infos(&self, user_id: UserId) -> Vec<RoomInfo> {
        let mut rooms: Vec<&RoomState> = self
            .rooms
            .values()
            .filter(|room| room.access.allows(user_id))
            .collect();
        rooms.sort_by_key(|room| room.id.0);
        rooms.iter().map(|room| self.room_info(room)).collect()
    }

    fn room_info(&self, room: &RoomState) -> RoomInfo {
        RoomInfo {
            room_id: room.id,
            name: room.name.clone(),
            kind: room.access.kind(),
            head: self.store.head(room.id),
            voice_users: room
                .active_streams
                .values()
                .filter_map(|session_id| self.sessions.get(session_id))
                .map(|session| session.user_id)
                .collect(),
        }
    }

    /// The server-wide user directory: every configured user (online or not)
    /// plus currently-online dynamic users.
    fn user_summaries(&self) -> Vec<UserSummary> {
        let mut users: Vec<UserSummary> = self
            .users
            .users
            .iter()
            .map(|user| {
                let session = self
                    .sessions
                    .values()
                    .find(|session| session.user_id == user.id && session.announced);
                UserSummary {
                    user_id: user.id,
                    username: user.username.clone(),
                    online: session.is_some(),
                    connected_at_ms: session.map(|s| s.connected_at_ms).unwrap_or(0),
                    voice_status: session.map(|s| s.voice_status).unwrap_or_default(),
                }
            })
            .collect();
        for session in self.sessions.values() {
            if session.announced && is_dynamic_user_id(session.user_id) {
                users.push(Self::session_user_summary(session, true));
            }
        }
        users.sort_by_key(|user| user.user_id.0);
        users.dedup_by_key(|user| user.user_id.0);
        users
    }

    fn session_user_summary(session: &Session, online: bool) -> UserSummary {
        UserSummary {
            user_id: session.user_id,
            username: session.username.clone(),
            online,
            connected_at_ms: if online { session.connected_at_ms } else { 0 },
            voice_status: if online {
                session.voice_status
            } else {
                control::ParticipantVoiceStatus::default()
            },
        }
    }

    /// Announces a session's user coming online or going offline to every
    /// other connected client.
    fn broadcast_presence(&mut self, session_id: SessionId, online: bool) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let own_token = session.tcp_token;
        let user = Self::session_user_summary(session, online);
        let control = ServerControl::Presence { user, online };
        let tokens: Vec<Token> = self
            .sessions
            .values()
            .map(|candidate| candidate.tcp_token)
            .filter(|token| *token != own_token && self.clients.contains_key(token))
            .collect();
        self.send_control_to_tokens(&tokens, &control);
    }

    /// Whether the session's user may access the room. Denials never reveal
    /// whether the room exists: a private room a user cannot see and a missing
    /// room answer identically, so callers must pick failure replies that
    /// preserve that indistinguishability.
    fn room_allows(&self, session_id: SessionId, room_id: RoomId) -> bool {
        match (self.sessions.get(&session_id), self.rooms.get(&room_id)) {
            (Some(session), Some(room)) => room.access.allows(session.user_id),
            _ => false,
        }
    }

    /// [`Self::room_allows`], sending the deliberate `404 room not found`
    /// control error on denial.
    fn check_room_access(&mut self, session_id: SessionId, room_id: RoomId) -> bool {
        let allowed = self.room_allows(session_id, room_id);
        if !allowed && let Some(token) = self.live_token_for_session(session_id) {
            let _ = self.send_control_to_token(
                token,
                &ServerControl::Error {
                    code: control::ERROR_ROOM_NOT_FOUND,
                    message: "room not found".to_string(),
                },
            );
        }
        allowed
    }

    fn join_voice(&mut self, session_id: SessionId, room_id: RoomId) {
        kvlog::info!(
            "join voice requested",
            session_id = session_id.0,
            room_id = room_id.0
        );
        if !self.room_allows(session_id, room_id) {
            self.send_voice_join_failed(session_id, room_id, "room not found");
            return;
        }
        let previous = self.sessions.get(&session_id).and_then(|s| s.voice_room);
        if previous.is_some_and(|previous| previous != room_id) {
            self.leave_voice(session_id, None);
            if self.live_token_for_session(session_id).is_none() {
                return;
            }
        }
        let already_active = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.voice_room == Some(room_id));
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.voice_room = Some(room_id);
        }
        let (user_id, stream_id) = match self.ensure_voice_stream(session_id, room_id) {
            Ok(voice) => voice,
            Err(error) => {
                kvlog::warn!(
                    "voice stream failed",
                    session_id = session_id.0,
                    room_id = room_id.0,
                    error = error.as_str()
                );
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    session.voice_room = None;
                }
                self.send_voice_join_failed(session_id, room_id, &error);
                return;
            }
        };
        self.voice_relay.submit(VoiceCommand::SetRoute {
            session_id,
            route: Some(VoiceRoute {
                room_id,
                stream_id,
            }),
        });
        if !already_active {
            self.broadcast_voice_started(room_id, session_id, user_id, stream_id);
            if let Some(token) = self.live_token_for_session(session_id) {
                self.send_existing_voice_streams_to_token(room_id, session_id, token);
            }
            self.replay_voice_room_shares(room_id, session_id);
            // Peers cache the last VoiceStatus broadcast per user, and status
            // changes made while out of a call are stored without broadcast,
            // so an effective join must republish the joiner's current status
            // unconditionally to overwrite any stale belief.
            let status = self
                .sessions
                .get(&session_id)
                .map(|session| session.voice_status)
                .unwrap_or_default();
            self.broadcast_control(
                room_id,
                &ServerControl::VoiceStatus {
                    room_id,
                    user_id,
                    status,
                },
            );
        }
    }

    fn send_voice_join_failed(&mut self, session_id: SessionId, room_id: RoomId, message: &str) {
        let Some(token) = self.live_token_for_session(session_id) else {
            return;
        };
        let _ = self.send_control_to_token(
            token,
            &ServerControl::VoiceJoinFailed {
                room_id,
                message: message.to_string(),
            },
        );
    }

    /// Leaves the session's current voice call: stops its stream, tears down
    /// P2P pairings, and clears the voice room.
    fn leave_voice(
        &mut self,
        session_id: SessionId,
        excluded_broadcast_session: Option<SessionId>,
    ) {
        let Some(room_id) = self.sessions.get(&session_id).and_then(|s| s.voice_room) else {
            return;
        };
        kvlog::info!(
            "leave voice",
            session_id = session_id.0,
            room_id = room_id.0
        );
        self.end_shares_for_session(session_id);
        self.revoke_share_access_for_session(session_id);
        self.stop_voice(session_id, None, excluded_broadcast_session);
        self.broadcast_p2p_gone(session_id, room_id);
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.voice_room = None;
            session.p2p = None;
        }
        self.remove_peer_links(session_id);
    }

    /// Plans a history page from the resident window or queues a disk read;
    /// encoding and delivery then advance under the loop-wide history budget.
    fn fetch_history(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        before: Option<MessageId>,
        limit: u16,
    ) {
        if !self.check_room_access(session_id, room_id) {
            return;
        }
        let Some(token) = self.live_token_for_session(session_id) else {
            return;
        };
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| {
                session.pending_disk_history_fetches >= MAX_PENDING_DISK_HISTORY_FETCHES
            })
        {
            kvlog::warn!(
                "history fetch limit exceeded",
                session_id = session_id.0,
                room_id = room_id.0
            );
            self.send_empty_history_chunk(token, room_id, before);
            return;
        }
        if self.pending_history_fetches >= MAX_PENDING_HISTORY_FETCHES {
            kvlog::warn!(
                "server history fetch limit exceeded",
                session_id = session_id.0,
                room_id = room_id.0,
                pending = self.pending_history_fetches
            );
            self.send_empty_history_chunk(token, room_id, before);
            return;
        }
        let plan = self.store.history_fetch_plan_before(
            room_id,
            before,
            usize::from(limit),
            HISTORY_CHUNK_TARGET_BYTES,
        );
        if plan.message_count == 0
            && let Some(request) = self.store.disk_fetch_request(
                session_id,
                room_id,
                before,
                usize::from(limit),
                HISTORY_CHUNK_TARGET_BYTES,
            )
        {
            self.enqueue_disk_history_fetch(session_id, token, room_id, request);
            return;
        }
        let chunk_count = plan.chunk_count();
        kvlog::info!(
            "history chunks queued",
            session_id = session_id.0,
            room_id = room_id.0,
            message_count = plan.message_count,
            chunk_count,
            at_start = plan.at_start
        );
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.pending_disk_history_fetches += 1;
        }
        self.pending_history_fetches += 1;
        self.history_deliveries.push_back(HistoryDelivery {
            session_id,
            token,
            room_id,
            source: HistoryDeliverySource::Resident {
                plan,
                next_chunk: 0,
            },
        });
        self.loop_work.set_background_work(true);
    }

    fn enqueue_disk_history_fetch(
        &mut self,
        session_id: SessionId,
        token: Token,
        room_id: RoomId,
        request: history_reader::HistoryReadRequest,
    ) {
        let Some(session) = self.sessions.get(&session_id) else {
            return;
        };
        let before = request.before;
        if session.pending_disk_history_fetches >= MAX_PENDING_DISK_HISTORY_FETCHES {
            kvlog::warn!(
                "disk history fetch limit exceeded",
                session_id = session_id.0,
                room_id = room_id.0
            );
            self.send_empty_history_chunk(token, room_id, before);
            return;
        }
        if !self.history_reader.enqueue(request) {
            kvlog::error!(
                "history reader unavailable",
                session_id = session_id.0,
                room_id = room_id.0
            );
            self.send_empty_history_chunk(token, room_id, before);
            return;
        }
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.pending_disk_history_fetches += 1;
        }
        self.pending_history_fetches += 1;
        kvlog::info!(
            "disk history fetch queued",
            session_id = session_id.0,
            room_id = room_id.0
        );
    }

    /// Terminates a history fetch that cannot be served with an empty
    /// `complete` chunk; the client has no fetch timeout, so a request
    /// without a terminal chunk would wedge its paging for the room.
    fn send_empty_history_chunk(
        &mut self,
        token: Token,
        room_id: RoomId,
        before: Option<MessageId>,
    ) {
        let payload = history_reader::empty_chunk(room_id, before);
        let _ = self.queue_control_payload_to_token(token, "history_chunk", &payload);
    }

    /// Queues finished disk history pages to their sessions. Called every
    /// event-loop pass; the reader's waker only shortens the wait.
    fn drain_history_replies(&mut self) {
        let mut replies = self.history_events.drain_up_to(16);
        while let Some(reply) = replies.pop_front() {
            let Some(token) = self.live_token_for_session(reply.session_id) else {
                if let Some(session) = self.sessions.get_mut(&reply.session_id) {
                    session.pending_disk_history_fetches =
                        session.pending_disk_history_fetches.saturating_sub(1);
                }
                self.pending_history_fetches = self.pending_history_fetches.saturating_sub(1);
                continue;
            };
            self.history_deliveries.push_back(HistoryDelivery {
                session_id: reply.session_id,
                token,
                room_id: reply.room_id,
                source: HistoryDeliverySource::Encoded(reply.payloads.into()),
            });
        }
        self.process_history_deliveries();
    }

    fn process_history_deliveries(&mut self) {
        let mut bytes = 0usize;
        let mut chunks = 0usize;
        while chunks < HISTORY_DELIVERY_BUDGET_CHUNKS {
            let Some(mut delivery) = self.history_deliveries.pop_front() else {
                break;
            };
            if !self.clients.contains_key(&delivery.token) {
                self.finish_history_delivery(delivery.session_id);
                continue;
            }
            let payload = match &mut delivery.source {
                HistoryDeliverySource::Resident { plan, next_chunk } => {
                    if *next_chunk >= plan.chunk_count() {
                        self.finish_history_delivery(delivery.session_id);
                        continue;
                    }
                    match self.store.encode_history_chunk(plan, *next_chunk) {
                        Ok(payload) => {
                            *next_chunk += 1;
                            payload
                        }
                        Err(error) => {
                            kvlog::warn!(
                                "history chunk encode failed",
                                session_id = delivery.session_id.0,
                                room_id = delivery.room_id.0,
                                error = error.as_str()
                            );
                            *next_chunk = plan.chunk_count();
                            history_reader::empty_chunk(delivery.room_id, plan.before())
                        }
                    }
                }
                HistoryDeliverySource::Encoded(payloads) => {
                    let Some(payload) = payloads.pop_front() else {
                        self.finish_history_delivery(delivery.session_id);
                        continue;
                    };
                    payload
                }
            };
            if chunks > 0 && bytes.saturating_add(payload.len()) > HISTORY_DELIVERY_BUDGET_BYTES {
                match &mut delivery.source {
                    HistoryDeliverySource::Resident { next_chunk, .. } => *next_chunk -= 1,
                    HistoryDeliverySource::Encoded(payloads) => payloads.push_front(payload),
                }
                self.history_deliveries.push_front(delivery);
                break;
            }
            bytes = bytes.saturating_add(payload.len());
            chunks += 1;
            let send_failed = self
                .queue_control_payload_to_token(delivery.token, "history_chunk", &payload)
                .is_err();
            let complete = match &delivery.source {
                HistoryDeliverySource::Resident { plan, next_chunk } => {
                    send_failed || *next_chunk >= plan.chunk_count()
                }
                HistoryDeliverySource::Encoded(payloads) => send_failed || payloads.is_empty(),
            };
            if complete {
                self.finish_history_delivery(delivery.session_id);
            } else {
                self.history_deliveries.push_back(delivery);
            }
            if bytes >= HISTORY_DELIVERY_BUDGET_BYTES {
                break;
            }
        }
        self.refresh_background_work();
    }

    fn refresh_background_work(&mut self) {
        self.loop_work.set_background_work(
            !self.history_deliveries.is_empty()
                || !self.video_fanouts.is_empty()
                || !self.control_send_tokens.is_empty(),
        );
    }

    fn finish_history_delivery(&mut self, session_id: SessionId) {
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.pending_disk_history_fetches =
                session.pending_disk_history_fetches.saturating_sub(1);
        }
        self.pending_history_fetches = self.pending_history_fetches.saturating_sub(1);
    }

    fn enqueue_mls_write(
        &mut self,
        token: Token,
        request: MlsWriteRequest,
    ) -> Result<(), String> {
        if !self.pending_mls.insert(token) {
            return Err("connection already has an MLS delivery operation pending".to_string());
        }
        if self.mls_worker.enqueue_typed(request) {
            Ok(())
        } else {
            self.pending_mls.remove(&token);
            Err("MLS delivery worker is unavailable".to_string())
        }
    }

    /// Applies finished delivery work and resumes only the connection that
    /// initiated it. A failed request follows the normal protocol-error path
    /// and disconnects that client; unrelated sockets continue to run while
    /// the worker is blocked in storage I/O.
    fn drain_mls_replies(&mut self) {
        let mut events = self.mls_events.drain_up_to(32);
        while let Some(reply) = events.pop_front() {
            let (token, result) = match reply {
                MlsReply::RosterStored {
                    token,
                    session_id,
                    user_id,
                    initial,
                    roster,
                    result,
                } => {
                    let result = match result {
                        Ok((checkpoint, state)) => (|| {
                            self.install_mls_cache(state)?;
                            if initial
                                && let Some(session) = self.sessions.get_mut(&session_id)
                            {
                                session.bootstrap_credential_hash = None;
                            }
                            let active_devices = roster
                                .body
                                .active_devices
                                .iter()
                                .map(|certificate| certificate.body.device_id)
                                .collect::<Vec<_>>();
                            let revoked_tokens = self
                                .sessions
                                .values()
                                .filter(|session| session.user_id == user_id)
                                .filter_map(|session| {
                                    session
                                        .mls_device
                                        .as_ref()
                                        .filter(|bound| {
                                            !active_devices.contains(&bound.device_id)
                                        })
                                        .map(|_| session.tcp_token)
                                })
                                .collect::<Vec<_>>();
                            for session in self
                                .sessions
                                .values_mut()
                                .filter(|session| session.user_id == user_id)
                            {
                                if let Some(bound) = session.mls_device.as_mut() {
                                    if active_devices.contains(&bound.device_id) {
                                        bound.roster = checkpoint;
                                    } else {
                                        session.mls_device = None;
                                    }
                                }
                            }
                            self.send_control_to_token(
                                token,
                                &ServerControl::DeviceRosterStored { checkpoint },
                            )?;
                            let tokens = self
                                .sessions
                                .values()
                                .map(|session| session.tcp_token)
                                .filter(|token| self.clients.contains_key(token))
                                .collect::<Vec<_>>();
                            self.send_control_to_tokens(
                                &tokens,
                                &ServerControl::DeviceRoster {
                                    user_id,
                                    initialized: true,
                                    roster: Some(roster),
                                },
                            );
                            for revoked in revoked_tokens {
                                self.disconnect(revoked);
                            }
                            Ok(())
                        })(),
                        Err(PutRosterError::Conflict(current)) => self.send_control_to_token(
                            token,
                            &ServerControl::DeviceRosterConflict { current },
                        ),
                        Err(PutRosterError::Invalid(error)) => Err(error),
                    };
                    (Some(token), result)
                }
                MlsReply::RoomCreated {
                    token,
                    room_id,
                    result,
                } => {
                    let result = result.and_then(|reply| match reply {
                        RoomCreationReply::Created {
                            epoch,
                            room,
                            deliveries,
                        } => {
                            self.mls.install_cached_room(room);
                            self.push_mls_welcome_deliveries(deliveries);
                            self.send_control_to_token(
                                token,
                                &ServerControl::EncryptedRoomCreated { room_id, epoch },
                            )
                        }
                        RoomCreationReply::Existing { room } => {
                            let descriptor = room.descriptor.clone();
                            let epoch = room.public_state.epoch;
                            let group_info = room.group_info.clone();
                            self.mls.install_cached_room(room);
                            self.send_control_to_token(
                                token,
                                &ServerControl::GroupInfo {
                                    room_id,
                                    descriptor: Some(descriptor),
                                    epoch,
                                    group_info,
                                },
                            )
                        }
                    });
                    (Some(token), result)
                }
                MlsReply::CommitSubmitted {
                    token,
                    room_id,
                    result,
                } => {
                    let result = result.and_then(|(outcome, room, pushed)| {
                        if let Some(room) = room {
                            self.mls.install_cached_room(room);
                        }
                        self.send_control_to_token(
                            token,
                            &ServerControl::MlsCommitSubmitted {
                                room_id,
                                outcome: outcome.clone(),
                            },
                        )?;
                        if let Some((batch, deliveries)) = pushed {
                            self.push_mls_welcome_deliveries(deliveries);
                            let tokens = self.mls_room_tokens(room_id);
                            self.send_control_to_tokens(
                                &tokens,
                                &ServerControl::MlsEvents {
                                    room_id,
                                    events: batch.events,
                                    oldest_available_sequence: batch.oldest_available_sequence,
                                    head_sequence: batch.head_sequence,
                                },
                            );
                        }
                        Ok(())
                    });
                    (Some(token), result)
                }
                MlsReply::DeviceLinkRedeemed {
                    token,
                    secret_hash,
                    user_id,
                    username,
                    device_id,
                    device_name,
                    credential_hash,
                    attempt_id,
                    bearer_token,
                    receive_files,
                    file_receive_limit_bytes,
                    result,
                } => {
                    let result = result.and_then(|state| {
                        self.install_mls_cache(state)?;
                        let link = self.device_links.get_mut(&secret_hash).ok_or_else(|| {
                            "device link disappeared during redemption".to_string()
                        })?;
                        link.redemption = DeviceLinkRedemption::Redeemed {
                            device_id,
                            credential_hash,
                            attempt_id,
                        };
                        self.finish_device_link_redemption(
                            token,
                            secret_hash,
                            user_id,
                            username,
                            device_id,
                            device_name,
                            bearer_token,
                            receive_files,
                            file_receive_limit_bytes,
                            true,
                        )
                    });
                    (Some(token), result)
                }
                MlsReply::ApplicationSubmitted {
                    token,
                    room_id,
                    event_id,
                    result,
                } => {
                    let result = result.and_then(|(outcome, batch)| {
                        kvlog::info!(
                            "MLS application submission decided",
                            room_id = room_id.0,
                            outcome = mls_submit_outcome_name(&outcome),
                        );
                        self.send_control_to_token(
                            token,
                            &ServerControl::MlsApplicationSubmitted {
                                room_id,
                                event_id,
                                outcome,
                            },
                        )?;
                        if let Some(batch) = batch {
                            let tokens = self.mls_room_tokens(room_id);
                            self.send_control_to_tokens(
                                &tokens,
                                &ServerControl::MlsEvents {
                                    room_id,
                                    events: batch.events,
                                    oldest_available_sequence: batch.oldest_available_sequence,
                                    head_sequence: batch.head_sequence,
                                },
                            );
                        }
                        Ok(())
                    });
                    (Some(token), result)
                }
                MlsReply::Compacted {
                    admin_reply,
                    result,
                    started,
                } => {
                    self.mls_compaction_pending = false;
                    self.next_mls_compaction_at = Instant::now()
                        + Duration::from_secs(
                            self.config
                                .storage
                                .mls_compaction_min_interval_hours
                                .saturating_mul(60 * 60),
                        );
                    match &result {
                        Ok(Some((compacted, before, after))) => kvlog::info!(
                            "MLS database compaction completed",
                            compacted = *compacted,
                            before_bytes = *before,
                            after_bytes = *after,
                            duration_ms = started.elapsed().as_millis() as u64,
                        ),
                        Ok(None) => {}
                        Err(error) => {
                            kvlog::warn!("MLS database compaction failed", error = error.as_str())
                        }
                    }
                    if let Some(reply) = admin_reply {
                        let response = result.map(|compacted| match compacted {
                            Some((compacted, before, after)) => format!(
                                "compacted={compacted} before={} after={}",
                                before.map_or_else(|| "memory".to_string(), |v| v.to_string()),
                                after.map_or_else(|| "memory".to_string(), |v| v.to_string())
                            ),
                            None => "compacted=false before=memory after=memory".to_string(),
                        });
                        let _ = reply.send(response);
                    }
                    (None, Ok(()))
                }
                MlsReply::Cleanup { interval, result } => {
                    self.mls_cleanup_pending = false;
                    match result {
                        Ok(report) => {
                            if report.deleted_events > 0 || report.lagging_devices > 0 {
                                kvlog::info!(
                                    "MLS retention cleanup",
                                    deleted_events = report.deleted_events,
                                    lagging_devices = report.lagging_devices,
                                    more_work = report.more_work,
                                );
                            }
                            self.next_mls_cleanup_at = if report.more_work {
                                Instant::now()
                            } else {
                                Instant::now() + interval
                            };
                            (None, Ok(()))
                        }
                        Err(error) => {
                            self.next_mls_cleanup_at = Instant::now() + interval;
                            (None, Err(error))
                        }
                    }
                }
                MlsReply::StorageStatus { reply, result } => {
                    let _ = reply.send(result);
                    (None, Ok(()))
                }
                MlsReply::KeyPackagesPublished {
                    token,
                    device_id,
                    result,
                } => {
                    let result = result.map(|(available, cached)| {
                        self.mls.install_cached_key_packages(device_id, cached);
                        let tokens = self
                            .sessions
                            .values()
                            .map(|session| session.tcp_token)
                            .filter(|token| self.clients.contains_key(token))
                            .collect::<Vec<_>>();
                        self.send_control_to_tokens(
                            &tokens,
                            &ServerControl::KeyPackagesPublished {
                                device_id,
                                available,
                            },
                        );
                    });
                    (Some(token), result)
                }
                MlsReply::KeyPackageTaken {
                    token,
                    device_id,
                    result,
                } => {
                    let result = result.and_then(|(package, available, cached)| {
                        self.mls.install_cached_key_packages(device_id, cached);
                        self.send_control_to_token(
                            token,
                            &ServerControl::KeyPackage { device_id, package },
                        )?;
                        let owner_token = self
                            .sessions
                            .values()
                            .filter(|session| {
                                session
                                    .mls_device
                                    .as_ref()
                                    .is_some_and(|bound| bound.device_id == device_id)
                            })
                            .map(|session| session.tcp_token)
                            .min_by_key(|token| token.0);
                        if let Some(owner_token) = owner_token {
                            self.send_control_to_token(
                                owner_token,
                                &ServerControl::KeyPackagesLow {
                                    device_id,
                                    available,
                                },
                            )?;
                        }
                        Ok(())
                    });
                    (Some(token), result)
                }
            };
            if let Err(error) = result {
                if let Some(token) = token {
                    kvlog::warn!(
                        "MLS delivery operation failed",
                        token = token.0,
                        error = error.as_str()
                    );
                    self.disconnect(token);
                } else {
                    kvlog::warn!("MLS background operation failed", error = error.as_str());
                }
            }
            if let Some(token) = token {
                self.pending_mls.remove(&token);
                if self.clients.contains_key(&token) {
                    self.loop_work.queue_client_read(token);
                }
            }
        }
    }

    fn install_mls_cache(&mut self, state: MlsCacheState) -> Result<(), String> {
        self.mls.install_cache_state(state)
    }

    /// Opens (or returns) the DM room between the requesting session's user
    /// and `peer`, creating and persisting it on first use.
    fn open_dm(&mut self, session_id: SessionId, peer: UserId) {
        let Some(requester) = self
            .sessions
            .get(&session_id)
            .map(|session| session.user_id)
        else {
            return;
        };
        let Some(token) = self.live_token_for_session(session_id) else {
            return;
        };
        if requester == peer {
            let _ = self.send_control_to_token(
                token,
                &ServerControl::Error {
                    code: control::ERROR_REQUEST_REJECTED,
                    message: "cannot open a direct message with yourself".to_string(),
                },
            );
            return;
        }
        let peer_known = self.users.users.iter().any(|user| user.id == peer)
            || self
                .sessions
                .values()
                .any(|session| session.user_id == peer)
            || self.store.dm_room_for(requester, peer).is_some();
        if !peer_known {
            let _ = self.send_control_to_token(
                token,
                &ServerControl::Error {
                    code: control::ERROR_ROOM_NOT_FOUND,
                    message: "room not found".to_string(),
                },
            );
            return;
        }
        let existing = self.store.dm_room_for(requester, peer);
        let room_id = match self.store.begin_open_dm(requester, peer, now_ms()) {
            Ok(OpenDmResult::Existing(room_id)) => room_id,
            Ok(OpenDmResult::Pending { operation_id }) => {
                self.pending_dm_waiters
                    .entry(operation_id)
                    .or_default()
                    .push((session_id, peer));
                return;
            }
            Err(error) => {
                let _ = self.send_control_to_token(
                    token,
                    &ServerControl::Error {
                        code: control::ERROR_INTERNAL,
                        message: error,
                    },
                );
                return;
            }
        };
        if !self.rooms.contains_key(&room_id) {
            self.rooms.insert(
                room_id,
                RoomState {
                    id: room_id,
                    name: dm_room_name(requester, peer),
                    access: RoomAccess::Dm(requester, peer),
                    active_streams: HashMap::new(),
                },
            );
        }
        kvlog::info!(
            "dm room opened",
            session_id = session_id.0,
            requester = requester.0,
            peer = peer.0,
            room_id = room_id.0,
            created = existing.is_none()
        );
        if existing.is_none() {
            let room = self.room_info(self.rooms.get(&room_id).expect("dm room inserted"));
            let control = ServerControl::RoomUpserted { room };
            let endpoint_tokens: Vec<Token> = self
                .sessions
                .values()
                .filter(|session| session.user_id == requester || session.user_id == peer)
                .map(|session| session.tcp_token)
                .filter(|token| self.clients.contains_key(token))
                .collect();
            self.send_control_to_tokens(&endpoint_tokens, &control);
        }
        if self.live_token_for_session(session_id).is_some() {
            let _ = self.send_control_to_token(token, &ServerControl::DmOpened { room_id, peer });
        }
    }

    fn drain_dm_completions(&mut self) -> Result<(), String> {
        let mut completed = self.store.drain_dm_completions()?;
        while let Some(completion) = completed.pop_front() {
            let waiters = self
                .pending_dm_waiters
                .remove(&completion.operation_id)
                .unwrap_or_default();
            if let Err(error) = completion.result {
                for (session_id, _) in waiters {
                    if let Some(token) = self.live_token_for_session(session_id) {
                        let _ = self.send_control_to_token(
                            token,
                            &ServerControl::Error {
                                code: control::ERROR_INTERNAL,
                                message: error.clone(),
                            },
                        );
                    }
                }
                continue;
            }
            let room_id = completion.room.room_id;
            let requester = completion.room.user_a;
            let peer = completion.room.user_b;
            self.rooms.insert(
                room_id,
                RoomState {
                    id: room_id,
                    name: dm_room_name(requester, peer),
                    access: RoomAccess::Dm(requester, peer),
                    active_streams: HashMap::new(),
                },
            );
            kvlog::info!(
                "dm room opened",
                requester = requester.0,
                peer = peer.0,
                room_id = room_id.0,
                created = true
            );
            let room = self.room_info(self.rooms.get(&room_id).expect("dm room inserted"));
            let endpoint_tokens: Vec<Token> = self
                .sessions
                .values()
                .filter(|session| session.user_id == requester || session.user_id == peer)
                .map(|session| session.tcp_token)
                .filter(|token| self.clients.contains_key(token))
                .collect();
            self.send_control_to_tokens(
                &endpoint_tokens,
                &ServerControl::RoomUpserted { room },
            );
            for (session_id, requested_peer) in waiters {
                if let Some(token) = self.live_token_for_session(session_id) {
                    let _ = self.send_control_to_token(
                        token,
                        &ServerControl::DmOpened {
                            room_id,
                            peer: requested_peer,
                        },
                    );
                }
            }
        }
        Ok(())
    }

    fn broadcast_p2p_gone(&mut self, session_id: SessionId, room_id: RoomId) {
        if !self.config.network.p2p_enabled {
            return;
        }
        let Some(user_id) = self
            .sessions
            .get(&session_id)
            .map(|session| session.user_id)
        else {
            return;
        };
        let tokens: Vec<Token> = self
            .voice_member_sessions(room_id)
            .into_iter()
            .filter(|member| *member != session_id)
            .filter_map(|member| self.live_token_for_session(member))
            .collect();
        self.send_control_to_tokens(
            &tokens,
            &ServerControl::P2pPeerGone {
                session_id,
                user_id,
            },
        );
    }

    /// Sessions currently in the room's voice call.
    fn voice_member_sessions(&self, room_id: RoomId) -> Vec<SessionId> {
        self.rooms
            .get(&room_id)
            .map(|room| room.active_streams.values().copied().collect())
            .unwrap_or_default()
    }

    /// Private rooms and DMs use the MLS delivery service; the ordinary chat
    /// store is public-only.
    fn ordinary_chat_policy_error(&self, room_id: RoomId) -> Option<&'static str> {
        match self.rooms.get(&room_id).map(|room| &room.access) {
            Some(RoomAccess::Public) => None,
            Some(RoomAccess::Private(_) | RoomAccess::Dm(..)) => {
                Some("encrypted rooms accept messages only through MLS")
            }
            None => None,
        }
    }

    fn send_chat(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        body: String,
    ) -> Result<(), String> {
        let body_size = body.len();
        kvlog::info!(
            "chat send requested",
            session_id = session_id.0,
            room_id = room_id.0,
            body_size
        );
        let (sender, sender_name) = match self.sessions.get(&session_id) {
            Some(session) => (session.user_id, session.username.clone()),
            None => {
                kvlog::warn!(
                    "chat send rejected",
                    session_id = session_id.0,
                    error = "unknown session"
                );
                return Err("unknown session".into());
            }
        };
        if !self.check_room_access(session_id, room_id) {
            kvlog::warn!(
                "chat send rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                error = "room not accessible"
            );
            return Ok(());
        }
        if let Some(error) = self.ordinary_chat_policy_error(room_id) {
            kvlog::warn!(
                "chat send rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                error
            );
            let token = self
                .live_token_for_session(session_id)
                .ok_or_else(|| "chat sender disconnected".to_string())?;
            return self.send_control_to_token(
                token,
                &ServerControl::Error {
                    code: control::ERROR_REQUEST_REJECTED,
                    message: error.to_string(),
                },
            );
        }
        let message = ChatMessage {
            message_id: self.store.allocate_message_id(room_id),
            room_id,
            sender,
            sender_name,
            timestamp_ms: now_ms(),
            body,
            file_transfer_id: None,
            flags: MessageFlags::default(),
            target: None,
        };
        let history_len = match self.store.try_append(room_id, &message) {
            Ok(history_len) => history_len,
            Err(error) => {
                kvlog::warn!(
                    "chat send deferred by durable history backpressure",
                    session_id = session_id.0,
                    room_id = room_id.0,
                    error = error.as_str()
                );
                return self.reject_durable_history_busy(session_id);
            }
        };
        kvlog::info!(
            "chat message accepted",
            session_id = session_id.0,
            room_id = room_id.0,
            message_id = message.message_id.0,
            user_id = sender.0,
            body_size,
            history_len
        );
        self.broadcast_control(room_id, &ServerControl::Chat { message });
        Ok(())
    }

    /// Applies an edit or delete of a recent message. Retained targets also get
    /// ownership and operation checks; without a retained target the server
    /// broadcasts the mutation for clients to validate against their copies.
    /// Retained-state rejections are reported directly to the requester.
    fn mutate_chat(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        target: MessageId,
        kind: MutationKind,
        body: String,
    ) -> Result<(), String> {
        let (sender, sender_name) = match self.sessions.get(&session_id) {
            Some(session) => (session.user_id, session.username.clone()),
            None => {
                kvlog::warn!(
                    "chat mutation rejected",
                    session_id = session_id.0,
                    error = "unknown session"
                );
                return Err("unknown session".into());
            }
        };
        if !self.check_room_access(session_id, room_id) {
            kvlog::warn!(
                "chat mutation rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                error = "room not accessible"
            );
            return self.reject_chat_mutation(
                session_id,
                room_id,
                target,
                kind,
                "message is unavailable".to_string(),
            );
        }
        if let Some(error) = self.ordinary_chat_policy_error(room_id) {
            kvlog::warn!(
                "chat mutation rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                target = target.0,
                error
            );
            return self.reject_chat_mutation(session_id, room_id, target, kind, error.to_string());
        }
        if let Err(denied) = self.store.validate_mutation(room_id, target, sender, kind) {
            kvlog::warn!(
                "chat mutation rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                target = target.0,
                error = denied.as_str()
            );
            let message = match (kind, denied) {
                (_, room_store::MutationDenied::TargetMissing) => {
                    "message is too old or no longer available"
                }
                (MutationKind::Edit, room_store::MutationDenied::WrongSender) => {
                    "you can only edit your own messages"
                }
                (MutationKind::Delete, room_store::MutationDenied::WrongSender) => {
                    "you can only delete your own messages"
                }
                (MutationKind::Edit, room_store::MutationDenied::FileMessage) => {
                    "file messages cannot be edited"
                }
                (_, room_store::MutationDenied::MutationRecord) => {
                    "message mutations cannot be changed"
                }
                (_, room_store::MutationDenied::Deleted) => "message was already deleted",
                (MutationKind::Delete, room_store::MutationDenied::FileMessage) => {
                    "message cannot be deleted"
                }
            };
            return self.reject_chat_mutation(
                session_id,
                room_id,
                target,
                kind,
                message.to_string(),
            );
        }
        let mut flags = MessageFlags::default();
        match kind {
            MutationKind::Edit => flags.set_edited(),
            MutationKind::Delete => flags.set_deleted(),
        }
        let message = ChatMessage {
            message_id: self.store.allocate_message_id(room_id),
            room_id,
            sender,
            sender_name,
            timestamp_ms: now_ms(),
            body,
            file_transfer_id: None,
            flags,
            target: Some(target),
        };
        let history_len = match self.store.try_append(room_id, &message) {
            Ok(history_len) => history_len,
            Err(error) => {
                kvlog::warn!(
                    "chat mutation deferred by durable history backpressure",
                    session_id = session_id.0,
                    room_id = room_id.0,
                    error = error.as_str()
                );
                return self.reject_durable_history_busy(session_id);
            }
        };
        kvlog::info!(
            "chat mutation accepted",
            session_id = session_id.0,
            room_id = room_id.0,
            message_id = message.message_id.0,
            target = target.0,
            user_id = sender.0,
            history_len
        );
        self.broadcast_control(room_id, &ServerControl::Chat { message });
        Ok(())
    }

    fn reject_chat_mutation(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        target: MessageId,
        kind: MutationKind,
        message: String,
    ) -> Result<(), String> {
        let token = self
            .live_token_for_session(session_id)
            .ok_or_else(|| "mutation requester disconnected".to_string())?;
        let kind = match kind {
            MutationKind::Edit => ChatMutationKind::Edit,
            MutationKind::Delete => ChatMutationKind::Delete,
        };
        self.send_control_to_token(
            token,
            &ServerControl::ChatMutationRejected {
                room_id,
                target,
                kind,
                message,
            },
        )
    }

    fn reject_durable_history_busy(&mut self, session_id: SessionId) -> Result<(), String> {
        let token = self
            .live_token_for_session(session_id)
            .ok_or_else(|| "chat sender disconnected".to_string())?;
        self.send_control_to_token(
            token,
            &ServerControl::Error {
                code: control::ERROR_REQUEST_REJECTED,
                message: "durable history is busy; retry shortly".to_string(),
            },
        )
    }

    fn start_file_upload(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        client_transfer_id: FileTransferId,
        name: String,
        original_size: u64,
        encoding: FileContentEncoding,
        mls_event_id: Option<rpc::ids::EventId>,
    ) -> Result<(), String> {
        kvlog::info!(
            "file upload start requested",
            session_id = session_id.0,
            room_id = room_id.0,
            client_transfer_id = client_transfer_id.0,
            original_size,
            encoding = file_content_encoding_name(encoding)
        );
        if original_size > self.file_size_limit_bytes {
            return Err("file exceeds server maximum length".into());
        }
        let encrypted_room = self
            .rooms
            .get(&room_id)
            .is_some_and(|room| !matches!(room.access, RoomAccess::Public));
        let mls_encrypted = self.mls.group_info(room_id).is_some();
        if encrypted_room && !mls_encrypted {
            return Err("encrypted room file transfer is waiting for MLS setup".into());
        }
        if mls_event_id.is_some() != (encoding == FileContentEncoding::Sealed) {
            return Err("sealed metadata does not match the upload encoding".into());
        }
        if (encoding == FileContentEncoding::Sealed) != mls_encrypted {
            return Err(match encoding {
                FileContentEncoding::Sealed => "sealed uploads require an initialized MLS room",
                _ => "MLS room uploads require end-to-end encryption",
            }
            .into());
        }
        let key = (session_id, client_transfer_id);
        if self.active_uploads.contains_key(&key) {
            return Err("file transfer id is already active".into());
        }
        let session_uploads = self
            .active_uploads
            .keys()
            .filter(|(owner, _)| *owner == session_id)
            .count();
        if session_uploads >= MAX_ACTIVE_UPLOADS_PER_SESSION {
            return Err("too many concurrent file uploads".into());
        }
        let (sender, sender_name) = match self.sessions.get(&session_id) {
            Some(session) => (session.user_id, session.username.clone()),
            None => return Err("unknown session".into()),
        };
        if !self.check_room_access(session_id, room_id) {
            return Ok(());
        }
        let Some(uploader_token) = self.live_token_for_session(session_id) else {
            return Err("uploading session disconnected".into());
        };
        if self
            .clients
            .get(&uploader_token)
            .is_some_and(ClientConn::is_closing)
        {
            return Err("uploading session is closing".into());
        }

        let member_ids: Vec<SessionId> = self
            .rooms
            .get(&room_id)
            .map(|room| {
                self.sessions
                    .iter()
                    .filter(|(_, session)| room.access.allows(session.user_id))
                    .map(|(session_id, _)| *session_id)
                    .collect()
            })
            .unwrap_or_default();

        let original_name = sanitize_file_name(&name);
        let file_name = self.allocate_file_name(&original_name);
        let server_transfer_id = FileTransferId(self.next_file_transfer);
        self.next_file_transfer = self.next_file_transfer.wrapping_add(1).max(1);
        let timestamp_ms = now_ms();
        let transfer_members = member_ids
            .iter()
            .copied()
            .filter(|member_id| *member_id != session_id)
            .collect::<Vec<_>>();
        let mut recipients = transfer_members
            .iter()
            .copied()
            .filter(|member_id| {
                self.sessions
                    .get(member_id)
                    .is_some_and(|session| session_accepts_file(session, original_size))
            })
            .collect::<HashSet<_>>();
        let metadata = FileMetadata {
            transfer_id: server_transfer_id,
            room_id,
            sender,
            sender_name: sender_name.clone(),
            file_name: file_name.clone(),
            original_name,
            size: original_size,
            encoding,
            timestamp_ms,
            mls_event_id,
        };

        let body = format!(
            "sent file `{}` ({})",
            file_name,
            format_bytes(original_size)
        );
        let message = ChatMessage {
            message_id: self.store.allocate_message_id(room_id),
            room_id,
            sender,
            sender_name,
            timestamp_ms,
            body,
            file_transfer_id: Some(server_transfer_id),
            flags: MessageFlags::default(),
            target: None,
        };
        // The upload is registered before anything durable or visible
        // happens, so an uploader torn down by any of the sends below is
        // swept by `cancel_uploads_for_session`, which emits the terminal
        // `FileCanceled` and releases the reserved name.
        self.active_uploads.insert(
            key,
            ServerUpload {
                server_transfer_id,
                room_id,
                encoding,
                original_size,
                wire_received: 0,
                file_name,
                recipients: recipients.clone(),
                declined_notified: false,
            },
        );
        let accepted = self.send_control_to_token(
            uploader_token,
            &ServerControl::UploadFileAccepted {
                client_transfer_id,
                file: metadata.clone(),
            },
        );
        if accepted.is_err() || self.live_token_for_session(session_id).is_none() {
            self.cancel_file_upload(
                session_id,
                client_transfer_id,
                "uploading session disconnected".to_string(),
            );
            return accepted;
        }
        if !mls_encrypted {
            if let Err(error) = self.store.try_append(room_id, &message) {
                kvlog::warn!(
                    "file announcement deferred by durable history backpressure",
                    session_id = session_id.0,
                    room_id = room_id.0,
                    error = error.as_str()
                );
                self.cancel_file_upload(
                    session_id,
                    client_transfer_id,
                    "durable history is busy; retry shortly".to_string(),
                );
                return Ok(());
            }
            self.broadcast_control(room_id, &ServerControl::Chat { message });
        }
        if self.live_token_for_session(session_id).is_none() {
            // The uploader's teardown during the broadcast already canceled
            // the upload, giving the persisted message its terminal event.
            return Ok(());
        }
        recipients.retain(|recipient| self.live_token_for_session(*recipient).is_some());

        for member_id in &transfer_members {
            let Some(token) = self.live_token_for_session(*member_id) else {
                continue;
            };
            let contents = recipients.contains(member_id);
            let _ = self.send_control_to_token(
                token,
                &ServerControl::FileOffered {
                    file: metadata.clone(),
                    contents,
                },
            );
            if contents && self.live_token_for_session(*member_id).is_none() {
                recipients.remove(member_id);
            }
        }

        // A recipient teardown during the fan-out can cascade into canceling
        // this upload, so the entry may already be gone.
        if let Some(upload) = self.active_uploads.get_mut(&key) {
            upload.recipients = recipients;
        }
        // The room has other members, but none accepted the file (all have
        // downloads disabled or a smaller limit): the upload would stream to
        // nobody, so tell the uploader to cancel it. An upload into a room with
        // no other members completes normally and gets no false decline.
        if !transfer_members.is_empty() {
            self.notify_upload_declined(key, "recipient declined");
        }
        Ok(())
    }

    /// Tells the uploader, once, that transfer `key` has lost every recipient so
    /// it can cancel the now-pointless upload. The upload stays registered: the
    /// uploader replies with [`ClientControl::UploadFileCancel`], which tears it
    /// down through [`Self::cancel_file_upload`] without the `Err` that removing
    /// it here would turn into an uploader disconnect. A no-op when the transfer
    /// is gone, still has recipients, or was already notified.
    fn notify_upload_declined(&mut self, key: (SessionId, FileTransferId), reason: &str) {
        let Some(upload) = self.active_uploads.get(&key) else {
            return;
        };
        if !upload.recipients.is_empty() || upload.declined_notified {
            return;
        }
        let (uploader, client_transfer_id) = key;
        let Some(token) = self.live_token_for_session(uploader) else {
            return;
        };
        let _ = self.send_control_to_token(
            token,
            &ServerControl::UploadDeclined {
                client_transfer_id,
                reason: reason.to_string(),
            },
        );
        if let Some(upload) = self.active_uploads.get_mut(&key) {
            upload.declined_notified = true;
        }
    }

    fn receive_file_chunk(
        &mut self,
        session_id: SessionId,
        client_transfer_id: FileTransferId,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(), String> {
        if data.len() > MAX_FILE_CHUNK_BYTES {
            return Err("file chunk exceeds maximum length".into());
        }
        let key = (session_id, client_transfer_id);
        let (server_transfer_id, recipients) = {
            let upload = self
                .active_uploads
                .get_mut(&key)
                .ok_or_else(|| "unknown file transfer".to_string())?;
            if upload.wire_received != offset {
                return Err("file chunk offset mismatch".into());
            }
            let end = offset.saturating_add(data.len() as u64);
            if end > max_file_wire_bytes(upload.encoding, upload.original_size) {
                return Err("file chunk exceeds allowed relay size".into());
            }
            upload.wire_received = end;
            (upload.server_transfer_id, upload.recipients.clone())
        };
        kvlog::debug!(
            "file chunk relaying",
            session_id = session_id.0,
            client_transfer_id = client_transfer_id.0,
            server_transfer_id = server_transfer_id.0,
            offset,
            chunk_size = data.len(),
            recipient_count = recipients.len()
        );
        // The recipient set can already be empty (all skipped/left): the offset
        // check above still runs so a resuming uploader stays consistent, but
        // there is nobody to relay to. The uploader has been (or is about to be)
        // told to cancel via `notify_upload_declined`.
        if recipients.is_empty() {
            self.notify_upload_declined(key, "recipient declined");
            return Ok(());
        }
        let control = ServerControl::FileChunk {
            transfer_id: server_transfer_id,
            offset,
            data,
        };
        let kind = server_control_kind(&control);
        let payload = encode_server_control(&control);
        let mut disconnected_recipients = Vec::new();
        for recipient in recipients {
            let Some(token) = self.live_token_for_session(recipient) else {
                disconnected_recipients.push(recipient);
                continue;
            };
            let _ = self.queue_control_payload_to_token(token, kind, &payload);
            if self.live_token_for_session(recipient).is_none() {
                disconnected_recipients.push(recipient);
            }
        }
        if !disconnected_recipients.is_empty()
            && let Some(upload) = self.active_uploads.get_mut(&key)
        {
            for recipient in disconnected_recipients {
                upload.recipients.remove(&recipient);
            }
        }
        // If that prune took the last recipient, stop the uploader now rather
        // than waiting for the next chunk's empty check.
        self.notify_upload_declined(key, "recipient declined");
        Ok(())
    }

    fn complete_file_upload(
        &mut self,
        session_id: SessionId,
        client_transfer_id: FileTransferId,
    ) -> Result<(), String> {
        let key = (session_id, client_transfer_id);
        let upload = self
            .active_uploads
            .remove(&key)
            .ok_or_else(|| "unknown file transfer".to_string())?;
        self.reserved_file_names.remove(&upload.file_name);
        if !upload_completion_is_valid(upload.encoding, upload.original_size, upload.wire_received)
        {
            self.send_file_canceled(&upload, "upload ended before all bytes arrived");
            return Err("file upload ended before all bytes arrived".into());
        }
        kvlog::info!(
            "file upload completed",
            session_id = session_id.0,
            room_id = upload.room_id.0,
            client_transfer_id = client_transfer_id.0,
            server_transfer_id = upload.server_transfer_id.0,
            encoding = file_content_encoding_name(upload.encoding),
            original_bytes = upload.original_size,
            wire_bytes = upload.wire_received,
            savings_percent = wire_savings_percent(upload.original_size, upload.wire_received)
        );
        let recipients: Vec<Token> = upload
            .recipients
            .iter()
            .filter_map(|recipient| self.live_token_for_session(*recipient))
            .collect();
        self.send_control_to_tokens(
            &recipients,
            &ServerControl::FileComplete {
                transfer_id: upload.server_transfer_id,
            },
        );
        Ok(())
    }

    fn cancel_file_upload(
        &mut self,
        session_id: SessionId,
        client_transfer_id: FileTransferId,
        reason: String,
    ) {
        let key = (session_id, client_transfer_id);
        if let Some(upload) = self.active_uploads.remove(&key) {
            self.reserved_file_names.remove(&upload.file_name);
            kvlog::warn!(
                "file upload canceled",
                session_id = session_id.0,
                room_id = upload.room_id.0,
                client_transfer_id = client_transfer_id.0,
                server_transfer_id = upload.server_transfer_id.0,
                reason = reason.as_str()
            );
            self.send_file_canceled(&upload, &reason);
        }
    }

    fn cancel_uploads_for_session(&mut self, session_id: SessionId, reason: &str) {
        let keys = self
            .active_uploads
            .keys()
            .filter_map(|(owner, transfer_id)| (*owner == session_id).then_some(*transfer_id))
            .collect::<Vec<_>>();
        for transfer_id in keys {
            self.cancel_file_upload(session_id, transfer_id, reason.to_string());
        }
    }

    /// Drops `session_id` from an in-flight transfer's recipient set when that
    /// recipient skips the download. `active_uploads` is keyed by the uploader's
    /// `(SessionId, client FileTransferId)`, so the transfer is located by a
    /// value scan on `server_transfer_id` (at most
    /// `MAX_ACTIVE_UPLOADS_PER_SESSION` per uploader). Removal stops
    /// [`Self::receive_file_chunk`] and [`Self::complete_file_upload`] from
    /// relaying anything further to this recipient; the upload continues for the
    /// remaining recipients, or — when this was the last one — the uploader is
    /// told to cancel via [`Self::notify_upload_declined`].
    fn skip_file_download(&mut self, session_id: SessionId, server_transfer_id: FileTransferId) {
        let Some((key, upload)) = self
            .active_uploads
            .iter_mut()
            .find(|(_, upload)| upload.server_transfer_id == server_transfer_id)
        else {
            return;
        };
        let key = *key;
        if upload.recipients.remove(&session_id) {
            kvlog::info!(
                "file download skipped",
                session_id = session_id.0,
                room_id = upload.room_id.0,
                server_transfer_id = server_transfer_id.0
            );
            self.notify_upload_declined(key, "recipient declined");
        }
    }

    fn send_file_canceled(&mut self, upload: &ServerUpload, reason: &str) {
        for recipient in &upload.recipients {
            let Some(token) = self.live_token_for_session(*recipient) else {
                continue;
            };
            let _ = self.send_control_to_token(
                token,
                &ServerControl::FileCanceled {
                    transfer_id: upload.server_transfer_id,
                    reason: reason.to_string(),
                },
            );
        }
    }

    fn ensure_voice_stream(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
    ) -> Result<(UserId, StreamId), String> {
        let (user_id, in_voice) = match self.sessions.get(&session_id) {
            Some(session) => (session.user_id, session.voice_room == Some(room_id)),
            None => return Err("unknown session".into()),
        };
        if !in_voice {
            kvlog::warn!(
                "voice start rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                error = "not in this room's voice call"
            );
            return Err("join the room's voice call first".into());
        }
        if self
            .sessions
            .get(&session_id)
            .and_then(|session| session.active_stream)
            .is_some()
        {
            let stream_id = self
                .sessions
                .get(&session_id)
                .and_then(|session| session.active_stream)
                .expect("checked above");
            return Ok((user_id, stream_id));
        }
        let stream_id = StreamId(self.next_stream);
        self.next_stream = self.next_stream.wrapping_add(1).max(1);
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.active_stream = Some(stream_id);
        }
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.active_streams.insert(stream_id, session_id);
        }
        kvlog::info!(
            "voice started",
            session_id = session_id.0,
            room_id = room_id.0,
            user_id = user_id.0,
            stream_id = stream_id.0
        );
        Ok((user_id, stream_id))
    }

    fn broadcast_voice_started(
        &mut self,
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
    ) {
        self.broadcast_control(
            room_id,
            &ServerControl::VoiceStarted {
                room_id,
                session_id,
                user_id,
                stream_id,
            },
        );
    }

    fn send_existing_voice_streams_to_token(
        &mut self,
        room_id: RoomId,
        joining_session_id: SessionId,
        token: Token,
    ) {
        let streams = self
            .rooms
            .get(&room_id)
            .map(|room| {
                room.active_streams
                    .iter()
                    .filter_map(|(stream_id, session_id)| {
                        (*session_id != joining_session_id).then_some((*stream_id, *session_id))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (stream_id, session_id) in streams {
            let Some(user_id) = self
                .sessions
                .get(&session_id)
                .map(|session| session.user_id)
            else {
                continue;
            };
            let _ = self.send_control_to_token(
                token,
                &ServerControl::VoiceStarted {
                    room_id,
                    session_id,
                    user_id,
                    stream_id,
                },
            );
        }
    }

    /// Ends the session's active voice stream (if it matches `requested`),
    /// removes it from the room's call membership, and broadcasts
    /// `VoiceStopped` to the remaining members.
    fn stop_voice(
        &mut self,
        session_id: SessionId,
        requested: Option<StreamId>,
        excluded_broadcast_session: Option<SessionId>,
    ) {
        let (room_id, user_id, stream_id) = match self.sessions.get_mut(&session_id) {
            Some(session) => {
                let Some(stream_id) = session.active_stream else {
                    return;
                };
                if requested.is_some_and(|requested| requested != stream_id) {
                    return;
                }
                session.active_stream = None;
                (session.voice_room, session.user_id, stream_id)
            }
            None => return,
        };
        let Some(room_id) = room_id else {
            return;
        };
        self.voice_relay.submit(VoiceCommand::SetRoute {
            session_id,
            route: None,
        });
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.active_streams.remove(&stream_id);
        }
        kvlog::info!(
            "voice stopped",
            session_id = session_id.0,
            room_id = room_id.0,
            user_id = user_id.0,
            stream_id = stream_id.0
        );
        self.broadcast_control_except(
            room_id,
            &ServerControl::VoiceStopped {
                room_id,
                session_id,
                user_id,
                stream_id,
            },
            excluded_broadcast_session,
        );
    }

    fn set_voice_status(&mut self, session_id: SessionId, status: control::ParticipantVoiceStatus) {
        let status = status.normalized();
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if session.voice_status == status {
            return;
        }
        session.voice_status = status;
        let Some(room_id) = session.voice_room else {
            return;
        };
        let user_id = session.user_id;
        kvlog::info!(
            "voice status changed",
            session_id = session_id.0,
            room_id = room_id.0,
            user_id = user_id.0,
            muted = status.muted,
            deafened = status.deafened
        );
        if audio_pop_logging_enabled() {
            kvlog::info!(
                "audio pop control voice status relay",
                session_id = session_id.0,
                room_id = room_id.0,
                user_id = user_id.0,
                muted = status.muted,
                deafened = status.deafened
            );
        }
        self.broadcast_control(
            room_id,
            &ServerControl::VoiceStatus {
                room_id,
                user_id,
                status,
            },
        );
    }

    fn publish_p2p(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        generation: u64,
        nat: P2pNatKind,
        tie_breaker: u64,
        candidates: Vec<P2pCandidate>,
    ) -> Result<(), String> {
        if !self.config.network.p2p_enabled {
            kvlog::info!(
                "p2p candidates ignored; p2p disabled",
                session_id = session_id.0,
                room_id = room_id.0,
                generation,
                candidate_count = candidates.len()
            );
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.p2p = None;
            }
            self.remove_peer_links(session_id);
            return Ok(());
        }
        if candidates.is_empty() {
            kvlog::info!(
                "p2p candidates cleared",
                session_id = session_id.0,
                room_id = room_id.0,
                generation
            );
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.p2p = None;
            }
            self.broadcast_p2p_gone(session_id, room_id);
            self.remove_peer_links(session_id);
            return Ok(());
        }
        let in_voice = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.voice_room == Some(room_id));
        if !in_voice {
            return Err("join the room's voice call before publishing P2P candidates".into());
        }
        kvlog::info!(
            "p2p candidates published",
            session_id = session_id.0,
            room_id = room_id.0,
            generation,
            candidate_count = candidates.len()
        );
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.p2p = Some(P2pSessionState {
                generation,
                nat,
                tie_breaker,
                candidates,
            });
        }

        let peers: Vec<SessionId> = self
            .voice_member_sessions(room_id)
            .into_iter()
            .filter(|peer| *peer != session_id)
            .filter(|peer| self.live_token_for_session(*peer).is_some())
            .filter(|peer| {
                self.sessions
                    .get(peer)
                    .and_then(|s| s.p2p.as_ref())
                    .is_some()
            })
            .collect();

        for peer_session_id in peers {
            if self.live_token_for_session(session_id).is_none() {
                break;
            }
            self.send_p2p_pair(session_id, peer_session_id)?;
        }
        Ok(())
    }

    fn send_p2p_pair(&mut self, a: SessionId, b: SessionId) -> Result<(), String> {
        let pair = ordered_pair(a, b);
        if !self.peer_links.contains_key(&pair) {
            let link = self.new_peer_link()?;
            self.peer_links.insert(pair, link);
        }
        let link = self
            .peer_links
            .get(&pair)
            .expect("peer link inserted")
            .clone();
        let a_info = self.p2p_peer_info(a, b, &link)?;
        let b_info = self.p2p_peer_info(b, a, &link)?;
        let a_token = self
            .live_token_for_session(a)
            .ok_or_else(|| "unknown P2P session".to_string())?;
        let b_token = self
            .live_token_for_session(b)
            .ok_or_else(|| "unknown P2P peer".to_string())?;
        self.send_control_to_token(a_token, &ServerControl::P2pPeer { peer: a_info })?;
        if self.live_token_for_session(a).is_none() || self.live_token_for_session(b).is_none() {
            return Ok(());
        }
        self.send_control_to_token(b_token, &ServerControl::P2pPeer { peer: b_info })?;
        Ok(())
    }

    fn p2p_peer_info(
        &self,
        recipient: SessionId,
        peer: SessionId,
        link: &PeerLink,
    ) -> Result<P2pPeerInfo, String> {
        let recipient_session = self
            .sessions
            .get(&recipient)
            .ok_or_else(|| "unknown recipient session".to_string())?;
        let peer_session = self
            .sessions
            .get(&peer)
            .ok_or_else(|| "unknown peer session".to_string())?;
        let peer_p2p = peer_session
            .p2p
            .as_ref()
            .ok_or_else(|| "peer has not published P2P candidates".to_string())?;
        let (send_key, recv_key) = if recipient < peer {
            (&link.low_to_high, &link.high_to_low)
        } else {
            (&link.high_to_low, &link.low_to_high)
        };
        Ok(P2pPeerInfo {
            room_id: recipient_session.voice_room.unwrap_or(self.default_room),
            session_id: peer,
            user_id: peer_session.user_id,
            generation: peer_p2p.generation,
            role: if recipient < peer {
                P2pRole::Controlling
            } else {
                P2pRole::Controlled
            },
            nat: peer_p2p.nat,
            tie_breaker: peer_p2p.tie_breaker,
            candidates: peer_p2p.candidates.clone(),
            send_key: p2p_key(send_key),
            recv_key: p2p_key(recv_key),
            stun_key: p2p_key(&link.stun_key),
            connection_id: link.connection_id,
        })
    }

    fn new_peer_link(&mut self) -> Result<PeerLink, String> {
        let connection_id = self.next_connection_id;
        self.next_connection_id = self.next_connection_id.wrapping_add(1).max(1);
        Ok(PeerLink {
            connection_id,
            low_to_high: random_key(&self.rng)?,
            high_to_low: random_key(&self.rng)?,
            stun_key: random_key(&self.rng)?,
        })
    }

    fn drain_voice_events(&mut self) -> io::Result<()> {
        let mut events = std::mem::take(&mut self.voice_events);
        debug_assert!(events.is_empty());
        self.voice_relay.drain_events(&mut events);
        if let Some(error) = events.failure.take() {
            return Err(io::Error::other(format!(
                "voice relay event loop stopped: {error}"
            )));
        }
        for (session_id, addr) in events.udp_bound.drain() {
            kvlog::info!("udp session bound", session_id = session_id.0, addr = %addr);
            let Some(token) = self.live_token_for_session(session_id) else {
                continue;
            };
            let _ = self.send_control_to_token(token, &ServerControl::UdpBound);
            if self.config.network.p2p_enabled
                && self.live_token_for_session(session_id).is_some()
            {
                let _ = self.send_control_to_token(
                    token,
                    &ServerControl::UdpReflexive {
                        addr: addr.to_string(),
                    },
                );
            }
        }
        if self.config.network.p2p_enabled {
            for ((session_id, probe_id), addr) in events.nat_probe.drain() {
                let Some(token) = self.live_token_for_session(session_id) else {
                    continue;
                };
                let _ = self.send_control_to_token(
                    token,
                    &ServerControl::P2pNatProbe {
                        probe_id,
                        addr: addr.to_string(),
                    },
                );
            }
        } else {
            debug_assert!(events.nat_probe.is_empty());
            events.nat_probe.clear();
        }
        for (session_id, activity) in events.activity.drain() {
            let Some(token) = self
                .sessions
                .get(&session_id)
                .map(|session| session.tcp_token)
            else {
                continue;
            };
            if let Some(client) = self.clients.get_mut(&token) {
                client.last_activity = client.last_activity.max(activity.last_activity);
            }
            if let Some(session) = self.sessions.get_mut(&session_id) {
                session.reported_server_rtt_ms = activity.reported_rtt_ms;
                session.server_rtt_reported_at = activity.rtt_reported_at;
            }
        }
        self.voice_events = events;
        Ok(())
    }

    fn send_control_to_token(
        &mut self,
        token: Token,
        control: &ServerControl,
    ) -> Result<(), String> {
        let payload: Arc<[u8]> = encode_server_control(control).into();
        self.queue_shared_control_payload(token, server_control_kind(control), payload)
    }

    /// Encodes the control once and queues it to every token; per-recipient
    /// failures are logged and skipped.
    fn send_control_to_tokens(&mut self, tokens: &[Token], control: &ServerControl) {
        let kind = server_control_kind(control);
        let payload: Arc<[u8]> = encode_server_control(control).into();
        for token in tokens {
            if let Err(error) =
                self.queue_shared_control_payload(*token, kind, Arc::clone(&payload))
            {
                kvlog::warn!(
                    "server control send failed",
                    token = token.0,
                    kind,
                    error = %error
                );
            }
        }
    }

    fn queue_control_payload_to_token(
        &mut self,
        token: Token,
        kind: &'static str,
        payload: &[u8],
    ) -> Result<(), String> {
        self.queue_shared_control_payload(token, kind, Arc::from(payload))
    }

    fn queue_shared_control_payload(
        &mut self,
        token: Token,
        kind: &'static str,
        payload: Arc<[u8]>,
    ) -> Result<(), String> {
        if payload.len() > MAX_CONTROL_PAYLOAD_BYTES {
            return Err(format!("{kind} exceeds control payload limit"));
        }
        let client = self
            .clients
            .get(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let transport = client
            .control
            .as_ref()
            .ok_or_else(|| "missing control cipher".to_string())?;
        let sealed_len = transport.sealed_len(payload.len());
        if sealed_len > frame::MAX_FRAME_LEN {
            return Err(format!("{kind} exceeds frame limit"));
        }
        let pending_bytes = self.pending_control_bytes.get(&token).copied().unwrap_or(0);
        let projected = client
            .write_buf
            .len()
            .saturating_add(pending_bytes)
            .saturating_add(frame::LENGTH_PREFIX_LEN + sealed_len);
        if projected > CONTROL_WRITE_HIGH_WATER {
            kvlog::warn!("control client dropped for backpressure", token = token.0, kind);
            self.disconnect(token);
            return Err("control write buffer overflow".to_string());
        }
        let scheduled_bytes = frame::LENGTH_PREFIX_LEN + sealed_len;
        if self
            .write_queue_total_bytes
            .saturating_add(self.pending_control_total_bytes)
            .saturating_add(scheduled_bytes)
            > OUTBOUND_GLOBAL_HIGH_WATER
        {
            kvlog::warn!(
                "control client dropped because global outbound queue is full",
                token = token.0,
                kind,
                queued_bytes = self
                    .write_queue_total_bytes
                    .saturating_add(self.pending_control_total_bytes)
            );
            self.disconnect(token);
            return Err("server control queue is full".to_string());
        }
        *self.pending_control_bytes.entry(token).or_default() +=
            scheduled_bytes;
        self.pending_control_total_bytes += scheduled_bytes;
        self.pending_controls
            .entry(token)
            .or_default()
            .push_back(PendingControl { kind, payload });
        if self.control_send_set.insert(token) {
            self.control_send_tokens.push_back(token);
        }
        kvlog::debug!(
            "server control scheduled",
            token = token.0,
            kind = kind,
            encrypted_size = sealed_len,
            queued_bytes = projected
        );
        self.refresh_background_work();
        Ok(())
    }

    fn process_control_sends(&mut self) {
        let mut bytes = 0usize;
        let mut records = 0usize;
        while records < CONTROL_SEAL_BUDGET_RECORDS {
            let Some(token) = self.control_send_tokens.pop_front() else {
                break;
            };
            self.control_send_set.remove(&token);
            let Some(control) = self
                .pending_controls
                .get_mut(&token)
                .and_then(VecDeque::pop_front)
            else {
                self.pending_controls.remove(&token);
                self.pending_control_bytes.remove(&token);
                continue;
            };
            if records > 0
                && bytes.saturating_add(control.payload.len()) > CONTROL_SEAL_BUDGET_BYTES
            {
                self.pending_controls
                    .entry(token)
                    .or_default()
                    .push_front(control);
                if self.control_send_set.insert(token) {
                    self.control_send_tokens.push_front(token);
                }
                break;
            }
            let sealed_size = self
                .clients
                .get(&token)
                .and_then(|client| client.control.as_ref())
                .map(|control_cipher| {
                    frame::LENGTH_PREFIX_LEN + control_cipher.sealed_len(control.payload.len())
                })
                .unwrap_or(0);
            if let Some(pending) = self.pending_control_bytes.get_mut(&token) {
                *pending = pending.saturating_sub(sealed_size);
            }
            self.pending_control_total_bytes =
                self.pending_control_total_bytes.saturating_sub(sealed_size);
            bytes = bytes.saturating_add(control.payload.len());
            records += 1;
            if let Err(error) = self.seal_control_payload_now(token, &control.payload)
            {
                kvlog::warn!(
                    "scheduled server control failed",
                    token = token.0,
                    kind = control.kind,
                    error = error.as_str()
                );
                self.disconnect(token);
                continue;
            }
            let has_more = self
                .pending_controls
                .get(&token)
                .is_some_and(|pending| !pending.is_empty());
            if has_more && self.control_send_set.insert(token) {
                self.control_send_tokens.push_back(token);
            } else if !has_more {
                self.pending_controls.remove(&token);
                self.pending_control_bytes.remove(&token);
            }
            if bytes >= CONTROL_SEAL_BUDGET_BYTES {
                break;
            }
        }
        self.refresh_background_work();
    }

    fn seal_control_payload_now(
        &mut self,
        token: Token,
        payload: &[u8],
    ) -> Result<(), String> {
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let transport = client
            .control
            .as_mut()
            .ok_or_else(|| "missing control cipher".to_string())?;
        let sealed_len = transport.sealed_len(payload.len());
        let queued_before = client.write_buf.len();
        let out = client.write_buf.tail_mut();
        let tail_before = out.len();
        out.extend_from_slice(&(sealed_len as u32).to_le_bytes());
        if let Err(error) = transport.seal_next_into(CHANNEL_CONTROL, payload, out) {
            out.truncate(tail_before);
            return Err(error.to_string());
        }
        self.write_queue_total_bytes += client.write_buf.len() - queued_before;
        kvlog::debug!(
            "server control queued",
            token = token.0,
            payload_size = payload.len(),
            encrypted_size = sealed_len,
            queued_bytes = client.write_buf.len()
        );
        self.loop_work.queue_client_write(token);
        Ok(())
    }

    fn broadcast_control(&mut self, room_id: RoomId, control: &ServerControl) {
        self.broadcast_control_except(room_id, control, None);
    }

    fn broadcast_control_except(
        &mut self,
        room_id: RoomId,
        control: &ServerControl,
        excluded_session: Option<SessionId>,
    ) {
        let kind = server_control_kind(control);
        let tokens = accessible_recipient_tokens(
            &self.rooms,
            &self.sessions,
            room_id,
            excluded_session,
            |token| self.clients.contains_key(&token),
        );
        kvlog::info!(
            "server control broadcasting",
            room_id = room_id.0,
            kind,
            recipient_count = tokens.len()
        );
        let payload: Arc<[u8]> = encode_server_control(control).into();
        for token in tokens {
            if let Err(error) =
                self.queue_shared_control_payload(token, kind, Arc::clone(&payload))
            {
                kvlog::warn!(
                    "server control broadcast failed",
                    token = token.0,
                    room_id = room_id.0,
                    kind,
                    error = %error
                );
            }
        }
    }

    fn write_client(&mut self, token: Token) {
        let mut disconnected = false;
        let mut requeue = false;
        let pending_controls = self
            .pending_controls
            .get(&token)
            .is_some_and(|pending| !pending.is_empty());
        if let Some(client) = self.clients.get_mut(&token) {
            let queued_before = client.write_buf.len();
            let write_result = write_queue_to(
                &mut client.socket,
                &mut client.write_buf,
                TCP_WRITE_ATTEMPTS,
            );
            self.write_queue_total_bytes = self
                .write_queue_total_bytes
                .saturating_sub(queued_before - client.write_buf.len());
            match write_result {
                Ok(outcome) => {
                    if outcome.bytes_written > 0 {
                        client.last_activity = Instant::now();
                        kvlog::debug!(
                            "tcp client bytes written",
                            token = token.0,
                            size = outcome.bytes_written,
                            remaining = client.write_buf.len(),
                            attempts = outcome.attempts
                        );
                    }
                    if outcome.wrote_zero {
                        kvlog::warn!("tcp client write returned zero bytes", token = token.0);
                        disconnected = true;
                    }
                    requeue = outcome.hit_limit;
                }
                Err(error) => {
                    kvlog::warn!("tcp client write failed", token = token.0, error = %error);
                    disconnected = true;
                }
            }
            if client.is_closing() && client.write_buf.is_empty() && !pending_controls {
                disconnected = true;
            }
        }
        if disconnected {
            self.disconnect(token);
        } else if requeue {
            self.loop_work.queue_client_write(token);
        }
    }

    /// Closes connections marked for a graceful teardown once their queued
    /// frames drained, or force-closes them at their drain deadline so a peer
    /// that stops reading cannot pin the slot.
    fn flush_disconnects(&mut self) {
        let now = Instant::now();
        let mut expired = Vec::new();
        for (token, client) in &self.clients {
            let Close::Draining { deadline } = client.close else {
                continue;
            };
            let controls_pending = self
                .pending_controls
                .get(token)
                .is_some_and(|pending| !pending.is_empty());
            if (client.write_buf.is_empty() && !controls_pending) || now >= deadline {
                expired.push(*token);
            }
        }
        for token in expired {
            self.disconnect(token);
        }
    }

    /// Drops connections past their applicable deadline: [`HANDSHAKE_TIMEOUT`]
    /// from creation while classification/handshake/auth is incomplete, and
    /// [`IDLE_TIMEOUT`] without socket or session-UDP traffic once established.
    /// Runs off the poll timeout, so idle sockets are reaped without traffic.
    fn sweep_idle_connections(&mut self, now: Instant) {
        if now < self.next_idle_sweep_at {
            return;
        }
        self.next_idle_sweep_at = now + IDLE_SWEEP_INTERVAL;
        let mut expired = Vec::new();
        for (token, client) in &self.clients {
            // Draining connections are already being torn down on their own
            // deadline in `flush_disconnects`.
            if client.is_closing() {
                continue;
            }
            if client.setup_complete() {
                if now.saturating_duration_since(client.last_activity) > IDLE_TIMEOUT {
                    expired.push((*token, "idle"));
                }
            } else if now.saturating_duration_since(client.created_at) > HANDSHAKE_TIMEOUT {
                expired.push((*token, "handshake"));
            }
        }
        for (token, reason) in expired {
            kvlog::warn!("tcp connection timed out", token = token.0, reason);
            self.disconnect(token);
        }
    }

    fn disconnect(&mut self, token: Token) {
        if self.deferred_identity_tokens.remove(&token) {
            self.deferred_identity_controls
                .retain(|(pending_token, _)| *pending_token != token);
        }
        self.pending_controls.remove(&token);
        if let Some(pending) = self.pending_control_bytes.remove(&token) {
            self.pending_control_total_bytes =
                self.pending_control_total_bytes.saturating_sub(pending);
        }
        self.control_send_set.remove(&token);
        let Some(mut client) = self.clients.remove(&token) else {
            return;
        };
        self.write_queue_total_bytes = self
            .write_queue_total_bytes
            .saturating_sub(client.write_buf.len());
        if let Err(error) = self.poll.registry().deregister(&mut client.socket) {
            kvlog::warn!(
                "tcp client deregister failed",
                token = token.0,
                error = %error
            );
        }
        kvlog::info!(
            "tcp client disconnecting",
            token = token.0,
            session_id = client.session_id.map(|id| id.0),
            user_id = client.user_id.map(|id| id.0)
        );
        if let ConnKind::Video(video) = &client.kind
            && let (Some(stream_id), Some(role)) = (video.stream_id, video.role)
        {
            self.detach_video_conn(stream_id, role, token);
        }
        if let Some(session_id) = client.session_id {
            self.teardown_session(session_id, "sender disconnected");
        }
    }

    /// Removes a session and everything keyed to it: its voice call (with a
    /// `VoiceStopped` broadcast), a `Presence` offline broadcast, media keys,
    /// active shares, pending uploads, and peer links. Shared by the disconnect
    /// path and by the reconnect path that supersedes a user's earlier session.
    fn teardown_session(&mut self, session_id: SessionId, reason: &str) {
        self.cancel_uploads_for_session(session_id, reason);
        self.active_bug_reports
            .retain(|(owner, _), _| *owner != session_id);
        self.end_shares_for_session(session_id);
        self.leave_voice(session_id, Some(session_id));
        let announce_offline = self.sessions.get(&session_id).is_some_and(|leaving| {
            leaving.announced
                && !self.sessions.iter().any(|(other_id, other)| {
                    *other_id != session_id && other.user_id == leaving.user_id && other.announced
                })
        });
        if announce_offline {
            self.broadcast_presence(session_id, false);
        }
        if let Some(session) = self.sessions.remove(&session_id) {
            self.reserved_media_routes
                .remove(&session.transport.route_id);
        }
        self.voice_relay
            .submit(VoiceCommand::RemoveSession { session_id });
        self.remove_peer_links(session_id);
    }

    /// Backstop that drops media-route mappings whose session has gone away.
    /// `disconnect` already removes a session's route on the normal path, so this
    /// only catches leaks and runs at most once per [`MEDIA_SWEEP_INTERVAL`].
    fn sweep_stale_media_routes(&mut self, now: Instant) {
        if self
            .next_media_sweep_at
            .is_some_and(|deadline| now < deadline)
        {
            return;
        }
        self.next_media_sweep_at = Some(now + MEDIA_SWEEP_INTERVAL);
        let sessions = &self.sessions;
        let before = self.reserved_media_routes.len();
        self.reserved_media_routes
            .retain(|_, session_id| sessions.contains_key(session_id));
        let removed = before.saturating_sub(self.reserved_media_routes.len());
        if removed > 0 {
            kvlog::warn!("stale media route mappings removed", removed);
        }
    }

    fn poll_room_rtt_snapshots(&mut self, now: Instant) {
        if now < self.next_rtt_snapshot_at {
            return;
        }
        self.next_rtt_snapshot_at = now + RTT_SNAPSHOT_INTERVAL;
        let room_ids = self
            .rooms
            .values()
            .filter(|room| !room.active_streams.is_empty())
            .map(|room| room.id)
            .collect::<Vec<_>>();
        for room_id in room_ids {
            let members = self.room_rtt_snapshot(room_id, now);
            let control = ServerControl::RoomRttSnapshot { room_id, members };
            let tokens: Vec<Token> = self
                .voice_member_sessions(room_id)
                .into_iter()
                .filter_map(|session_id| self.live_token_for_session(session_id))
                .collect();
            self.send_control_to_tokens(&tokens, &control);
        }
    }

    /// RTT snapshot of the room's voice call members.
    fn room_rtt_snapshot(
        &self,
        room_id: RoomId,
        now: Instant,
    ) -> Vec<control::ParticipantServerRtt> {
        let mut members = self
            .voice_member_sessions(room_id)
            .into_iter()
            .filter_map(|session_id| self.sessions.get(&session_id))
            .map(|session| control::ParticipantServerRtt {
                user_id: session.user_id,
                server_rtt_ms: session.fresh_server_rtt(now),
            })
            .collect::<Vec<_>>();
        members.sort_by_key(|member| member.user_id.0);
        members
    }

    fn session_for_token(&self, token: Token) -> Result<SessionId, String> {
        self.clients
            .get(&token)
            .and_then(|client| client.session_id)
            .ok_or_else(|| "client is not authenticated".to_string())
    }

    fn live_token_for_session(&self, session_id: SessionId) -> Option<Token> {
        let token = self.sessions.get(&session_id)?.tcp_token;
        self.clients.contains_key(&token).then_some(token)
    }

    /// Whether reads from `token` are parked by relay flow control: the
    /// connection is an authenticated control connection whose session has an
    /// active upload with a recipient queued past
    /// [`FILE_RELAY_WRITE_SOFT_CAP`].
    fn relay_clogged_for_reader(&self, token: Token) -> bool {
        let Some(client) = self.clients.get(&token) else {
            return false;
        };
        if !matches!(client.kind, ConnKind::Control) {
            return false;
        }
        let Some(session_id) = client.session_id else {
            return false;
        };
        self.upload_recipients_clogged(session_id)
    }

    /// Whether any live recipient of `uploader`'s active uploads has more
    /// than [`FILE_RELAY_WRITE_SOFT_CAP`] bytes queued.
    fn upload_recipients_clogged(&self, uploader: SessionId) -> bool {
        for ((session_id, _), upload) in &self.active_uploads {
            if *session_id != uploader {
                continue;
            }
            for recipient in &upload.recipients {
                let Some(token) = self.live_token_for_session(*recipient) else {
                    continue;
                };
                let Some(conn) = self.clients.get(&token) else {
                    continue;
                };
                if conn.write_buf.len() > FILE_RELAY_WRITE_SOFT_CAP {
                    return true;
                }
            }
        }
        false
    }

    /// Resumes uploader connections parked by relay flow control. An uploader
    /// with pending work (unread socket bytes or a whole buffered frame)
    /// whose recipients have all drained below the soft cap is queued in
    /// [`LoopWork`], so the next loop iteration reads it at zero poll timeout.
    /// Runs every loop iteration; recipient socket writability keeps the loop
    /// awake while a parked transfer drains.
    fn requeue_unclogged_uploaders(&mut self) {
        if self.active_uploads.is_empty() {
            return;
        }
        let mut resumed = Vec::new();
        for (session_id, _) in self.active_uploads.keys() {
            let Some(token) = self.live_token_for_session(*session_id) else {
                continue;
            };
            if resumed.contains(&token) || self.loop_work.has_client_read(token) {
                continue;
            }
            let Some(client) = self.clients.get(&token) else {
                continue;
            };
            let whole_frame_buffered =
                matches!(frame::parse_frame(client.read_buf.pending()), Ok(Some(_)));
            if !client.readiness.is_ready() && !whole_frame_buffered {
                continue;
            }
            if self.upload_recipients_clogged(*session_id) {
                continue;
            }
            resumed.push(token);
        }
        for token in resumed {
            self.loop_work.queue_client_read(token);
        }
    }

    fn queue_client_read_if_work_ready(&mut self, token: Token) {
        if self.client_read_work_ready(token) {
            self.loop_work.queue_client_read(token);
        }
    }

    fn client_read_work_ready(&self, token: Token) -> bool {
        let Some(client) = self.clients.get(&token) else {
            return false;
        };
        if client.is_closing()
            || self.pending_mls.contains(&token)
            || self.identity_pending(token)
            || self.relay_clogged_for_reader(token)
        {
            return false;
        }
        if client.readiness.is_ready() {
            return true;
        }
        match &client.kind {
            ConnKind::Video(video) => match &video.reader {
                // Residual receive-buffer bytes drain into the reader without
                // a new socket edge, so they count as work too.
                Some(reader) => reader.record_ready() || !client.read_buf.is_empty(),
                None => !matches!(video::parse_record(client.read_buf.pending()), Ok(None)),
            },
            ConnKind::Unidentified => client.read_buf.len() >= video::VIDEO_MAGIC.len(),
            ConnKind::Control => !matches!(frame::parse_frame(client.read_buf.pending()), Ok(None)),
        }
    }

    #[cfg(debug_assertions)]
    fn debug_assert_no_immediate_read_work(&self) {
        for token in self.clients.keys() {
            let can_process_now = self.client_read_work_ready(*token);
            debug_assert!(
                !can_process_now,
                "server poll would sleep with immediate client read work for token {}",
                token.0
            );
        }
    }

    fn remove_peer_links(&mut self, session_id: SessionId) {
        self.peer_links
            .retain(|(a, b), _| *a != session_id && *b != session_id);
    }

    fn allocate_file_name(&mut self, requested: &str) -> String {
        reserve_unique_file_name(&mut self.reserved_file_names, requested)
    }

    /// Converts a room-verb rejection into a recoverable control `Error`.
    /// These verbs fail on state races (voice eviction, a share ending) that a
    /// connected client cannot avoid, so they must never read as protocol
    /// errors that tear down the connection.
    fn report_request_outcome(
        &mut self,
        token: Token,
        result: Result<(), String>,
    ) -> Result<(), String> {
        match result {
            Ok(()) => Ok(()),
            Err(message) => {
                kvlog::warn!(
                    "client request rejected",
                    token = token.0,
                    error = message.as_str()
                );
                self.send_control_to_token(
                    token,
                    &ServerControl::Error {
                        code: control::ERROR_REQUEST_REJECTED,
                        message,
                    },
                )
            }
        }
    }

    /// Reports a bug-report handler failure to the client as a non-fatal
    /// `Error` instead of propagating it, so a rejected report (for example
    /// because no `bug-report-dir` is configured) never tears down the session.
    fn report_bug_outcome(
        &mut self,
        token: Token,
        result: Result<(), String>,
    ) -> Result<(), String> {
        match result {
            Ok(()) => Ok(()),
            Err(message) => {
                kvlog::warn!(
                    "bug report rejected",
                    token = token.0,
                    error = message.as_str()
                );
                self.send_control_to_token(
                    token,
                    &ServerControl::Error {
                        code: ERROR_BUG_REPORT_REJECTED,
                        message,
                    },
                )
            }
        }
    }

    fn drain_bug_report_replies(&mut self) {
        let mut replies = self.bug_report_events.drain_up_to(16);
        while let Some(reply) = replies.pop_front() {
            self.pending_bug_reports
                .remove(&(reply.session_id, reply.report_id));
            let Some(token) = self.live_token_for_session(reply.session_id) else {
                continue;
            };
            match reply.result {
                Ok(saved) => {
                    kvlog::info!(
                        "bug report saved",
                        session_id = reply.session_id.0,
                        report_id = reply.report_id.0,
                        description = reply.description.as_str(),
                        logs = saved.as_str()
                    );
                    if let Err(error) = self.send_control_to_token(
                        token,
                        &ServerControl::BugReportSaved {
                            report_id: reply.report_id,
                        },
                    ) {
                        kvlog::warn!(
                            "bug report completion send failed",
                            session_id = reply.session_id.0,
                            report_id = reply.report_id.0,
                            error = error.as_str()
                        );
                    }
                }
                Err(error) => {
                    let _ = self.report_bug_outcome(token, Err(error));
                }
            }
        }
    }

    fn start_bug_report(
        &mut self,
        session_id: SessionId,
        report_id: BugReportId,
        description: String,
        metadata: String,
        logs_size: u64,
    ) -> Result<(), String> {
        if self.config.security.bug_report_dir.is_none() {
            return Err("bug reports are not enabled on this server".into());
        }
        if logs_size > MAX_BUG_REPORT_BYTES {
            return Err("bug report logs exceed maximum length".into());
        }
        let key = (session_id, report_id);
        if self.active_bug_reports.contains_key(&key) || self.pending_bug_reports.contains(&key) {
            return Err("bug report is already active".into());
        }
        let session_reports = self
            .active_bug_reports
            .keys()
            .chain(self.pending_bug_reports.iter())
            .filter(|(owner, _)| *owner == session_id)
            .count();
        if session_reports >= MAX_ACTIVE_BUG_REPORTS_PER_SESSION {
            return Err("another bug report is already active for this session".into());
        }
        if self.active_bug_reports.len() + self.pending_bug_reports.len() >= MAX_ACTIVE_BUG_REPORTS {
            return Err("the server bug report queue is full".into());
        }
        kvlog::info!(
            "bug report starting",
            session_id = session_id.0,
            report_id = report_id.0,
            logs_size
        );
        self.active_bug_reports.insert(
            key,
            ServerBugReport {
                description,
                metadata,
                logs: Vec::new(),
                expected: logs_size,
            },
        );
        Ok(())
    }

    fn receive_bug_report_chunk(
        &mut self,
        session_id: SessionId,
        report_id: BugReportId,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(), String> {
        let report = self
            .active_bug_reports
            .get_mut(&(session_id, report_id))
            .ok_or_else(|| "unknown bug report".to_string())?;
        if report.logs.len() as u64 != offset {
            return Err("bug report chunk offset mismatch".into());
        }
        let end = offset.saturating_add(data.len() as u64);
        if end > report.expected {
            return Err("bug report chunk exceeds declared size".into());
        }
        report.logs.extend_from_slice(&data);
        Ok(())
    }

    fn complete_bug_report(
        &mut self,
        session_id: SessionId,
        report_id: BugReportId,
    ) -> Result<(), String> {
        let report = self
            .active_bug_reports
            .remove(&(session_id, report_id))
            .ok_or_else(|| "unknown bug report".to_string())?;
        if report.logs.len() as u64 != report.expected {
            return Err("bug report ended before all bytes arrived".into());
        }
        let Some(dir) = self.config.security.bug_report_dir.clone() else {
            return Err("bug reports are not enabled on this server".into());
        };
        let username = self
            .sessions
            .get(&session_id)
            .map(|session| session.username.clone())
            .unwrap_or_default();
        let key = (session_id, report_id);
        self.pending_bug_reports.insert(key);
        let request = BugReportWriteRequest {
            session_id,
            report_id,
            dir,
            username,
            description: report.description,
            metadata: report.metadata,
            logs: report.logs,
        };
        if let Err(error) = self.bug_report_writer.enqueue(request) {
            self.pending_bug_reports.remove(&key);
            return Err(match error {
                BugReportEnqueueError::Full => "the server bug report writer is busy".to_string(),
                BugReportEnqueueError::Gone => {
                    "the server bug report writer is unavailable".to_string()
                }
            });
        }
        kvlog::info!(
            "bug report queued for storage",
            session_id = session_id.0,
            report_id = report_id.0
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnState {
    AwaitClientHello,
    AwaitAuth,
    Ready,
}

struct ClientConn {
    socket: TcpStream,
    addr: SocketAddr,
    read_buf: RecvBuffer,
    /// Whether the socket may hold unread bytes. Set on every readable poll
    /// event and cleared only when a fill drains to `WouldBlock`. The poll is
    /// edge-triggered, so a read skipped or capped by budget or relay flow
    /// control must remember the readiness here; the edge itself never
    /// re-fires while the peer is blocked on our zero receive window.
    readiness: Readiness,
    write_buf: WriteQueue,
    kind: ConnKind,
    state: ConnState,
    control: Option<RecordProtection>,
    /// The full session material derived at handshake; moved into the `Session`
    /// at authentication. Present for both transport modes.
    transport: Option<SessionTransport>,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    close: Close,
    created_at: Instant,
    last_activity: Instant,
}

impl ClientConn {
    /// Marks the connection for a graceful close: queued frames flush before
    /// the socket drops, force-closed at the deadline if the peer stops
    /// reading. A connection already closing keeps its original deadline.
    fn begin_graceful_close(&mut self, now: Instant) {
        if matches!(self.close, Close::Open) {
            self.close = Close::Draining {
                deadline: now + CLOSE_FLUSH_GRACE,
            };
        }
    }

    fn is_closing(&self) -> bool {
        !matches!(self.close, Close::Open)
    }

    /// Whether socket reads route through an attached exact video record
    /// reader instead of the generic receive buffer.
    fn video_reader_attached(&self) -> bool {
        matches!(&self.kind, ConnKind::Video(video) if video.reader.is_some())
    }

    /// Whether classification, handshake, and auth have all completed.
    /// Connections still setting up are bounded by [`HANDSHAKE_TIMEOUT`],
    /// established ones by [`IDLE_TIMEOUT`].
    fn setup_complete(&self) -> bool {
        match &self.kind {
            ConnKind::Unidentified => false,
            ConnKind::Control => self.state == ConnState::Ready,
            ConnKind::Video(video) => video.phase == VideoPhase::Streaming,
        }
    }
}

/// Teardown intent for a connection. `Draining` flushes the queued frames (an
/// auth-rejection error) before the socket drops, but only until `deadline`,
/// so a peer that stops reading cannot pin its slot. Timeouts and backpressure
/// breaches skip this and disconnect immediately.
#[derive(Clone, Copy)]
enum Close {
    Open,
    Draining { deadline: Instant },
}

/// One inbound control-channel frame, parsed and decrypted in place in the
/// connection's receive buffer by [`control_conn_step`].
enum ControlStep {
    Hello(control::ClientHello),
    Control(ClientControl),
}

/// Parses, decrypts (in place), and decodes the next control frame buffered on
/// `client`, consuming it from the receive buffer. Returns `None` when no
/// whole frame is buffered yet.
// `_token` is referenced only by `kvlog::debug!`, which compiles to nothing
// without the `debug` feature.
fn control_conn_step(
    _token: Token,
    client: &mut ClientConn,
) -> Result<Option<ControlStep>, String> {
    let total = match frame::parse_frame(client.read_buf.pending()) {
        Ok(Some((_, total))) => total,
        Ok(None) => return Ok(None),
        Err(error) => return Err(format!("invalid frame: {error}")),
    };
    kvlog::debug!(
        "tcp frame processing",
        token = _token.0,
        state = conn_state_name(client.state),
        size = total
    );
    match client.state {
        ConnState::AwaitClientHello => {
            let frame = &client.read_buf.pending()[frame::LENGTH_PREFIX_LEN..total];
            let hello = decode_client_hello(frame)?;
            client.read_buf.consume(total);
            Ok(Some(ControlStep::Hello(hello)))
        }
        ConnState::AwaitAuth | ConnState::Ready => {
            let transport = client
                .control
                .as_mut()
                .ok_or_else(|| "missing control cipher".to_string())?;
            let frame = &mut client.read_buf.pending_mut()[frame::LENGTH_PREFIX_LEN..total];
            let plaintext = transport
                .open_next_in_place(CHANNEL_CONTROL, frame)
                .map_err(|error| error.to_string())?;
            kvlog::debug!(
                "client control decrypted",
                token = _token.0,
                payload_size = plaintext.len()
            );
            let control = decode_client_control(plaintext)?;
            kvlog::debug!(
                "client control decoded",
                token = _token.0,
                kind = client_control_kind(&control)
            );
            client.read_buf.consume(total);
            Ok(Some(ControlStep::Control(control)))
        }
    }
}

/// Outcome of feeding a video connection's reader in
/// [`Server::pump_video_reader`], deciding how [`Server::read_video_conn`]
/// proceeds.
enum VideoPump {
    /// A record may be steppable: either the reader holds a whole record or
    /// the connection reads through the generic receive buffer.
    Step,
    /// No whole record and the socket is drained; wait for the next edge.
    Idle,
    /// The byte budget ran out mid-record; requeue the read.
    Budget,
    /// The peer closed the socket.
    Closed,
    /// A read or protocol failure that tears the connection down.
    Failed(String),
}

/// One inbound video-connection record, extracted from the receive buffer by
/// [`video_conn_step`].
enum VideoStep {
    Handshake(Vec<u8>),
    Publish {
        stream_id: StreamId,
        frame: SharedVideoFrame,
        is_key: bool,
    },
    SubscriberChatter,
}

/// Extracts the next video record buffered on `client`. Handshake records are
/// small and copied out. A streaming publisher's records arrive through the
/// exact per-record reader: the sealed body decrypts in place in its own
/// allocation, which then backs the shared [`SharedVideoFrame`] without a
/// copy. Returns `None` when no whole record is buffered yet.
fn video_conn_step(client: &mut ClientConn) -> Result<Option<VideoStep>, String> {
    if let ConnKind::Video(video) = &mut client.kind
        && video.phase == VideoPhase::Streaming
        && let Some(reader) = video.reader.as_mut()
    {
        let Some(mut record) = reader.take_record() else {
            return Ok(None);
        };
        let stream_id = video
            .stream_id
            .ok_or_else(|| "stream before hello".to_string())?;
        let record_protection = video
            .record
            .as_mut()
            .ok_or_else(|| "stream before auth".to_string())?;
        let base = record.as_ptr() as usize;
        let (plaintext_offset, plaintext_len) = {
            let plaintext = record_protection
                .open_next_in_place(CHANNEL_VIDEO, &mut record)
                .map_err(|error| error.to_string())?;
            (plaintext.as_ptr() as usize - base, plaintext.len())
        };
        if plaintext_len < video::VIDEO_FRAME_HEADER_LEN {
            return Err("video frame is shorter than its header".to_string());
        }
        let frame = SharedVideoFrame::from_record(record, plaintext_offset, plaintext_len);
        let is_key = frame.as_slice()[12] == 1;
        return Ok(Some(VideoStep::Publish {
            stream_id,
            frame,
            is_key,
        }));
    }
    let total = match video::parse_record(client.read_buf.pending()) {
        Ok(Some((_, total))) => total,
        Ok(None) => return Ok(None),
        Err(error) => return Err(format!("invalid record: {error}")),
    };
    let ConnKind::Video(video) = &mut client.kind else {
        return Err("record on a non-video connection".to_string());
    };
    match video.phase {
        VideoPhase::AwaitHello | VideoPhase::AwaitAuth => {
            let record = client.read_buf.pending()[video::VIDEO_LENGTH_PREFIX_LEN..total].to_vec();
            client.read_buf.consume(total);
            Ok(Some(VideoStep::Handshake(record)))
        }
        VideoPhase::Streaming => {
            let role = video
                .role
                .ok_or_else(|| "stream before hello".to_string())?;
            let stream_id = video
                .stream_id
                .ok_or_else(|| "stream before hello".to_string())?;
            let record_protection = video
                .record
                .as_mut()
                .ok_or_else(|| "stream before auth".to_string())?;
            let record = &mut client.read_buf.pending_mut()[video::VIDEO_LENGTH_PREFIX_LEN..total];
            let plaintext = record_protection
                .open_next_in_place(CHANNEL_VIDEO, record)
                .map_err(|error| error.to_string())?;
            let step = match role {
                VideoRole::Publisher => {
                    if plaintext.len() < video::VIDEO_FRAME_HEADER_LEN {
                        return Err("video frame is shorter than its header".to_string());
                    }
                    VideoStep::Publish {
                        stream_id,
                        frame: SharedVideoFrame::copy_from_slice(plaintext),
                        is_key: plaintext[12] == 1,
                    }
                }
                // A subscriber sends nothing after authenticating.
                VideoRole::Subscriber => VideoStep::SubscriberChatter,
            };
            client.read_buf.consume(total);
            Ok(Some(step))
        }
    }
}

/// How a TCP connection has been classified on its first read.
///
/// Every connection starts [`ConnKind::Unidentified`]. The first bytes either
/// match [`video::VIDEO_MAGIC`], promoting it to [`ConnKind::Video`], or fall
/// through to the control handshake as [`ConnKind::Control`]. Control, voice,
/// and file traffic only ever run under [`ConnKind::Control`].
enum ConnKind {
    Unidentified,
    Control,
    Video(VideoConn),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoPhase {
    AwaitHello,
    AwaitAuth,
    Streaming,
}

/// Per-connection state for a dedicated video connection. `stream_id`, `role`,
/// and `record` are filled once the clear [`VideoHello`] arrives, before the
/// auth record is opened. `record` is AEAD in native mode and clear in
/// external-link mode.
struct VideoConn {
    phase: VideoPhase,
    session_id: Option<SessionId>,
    stream_id: Option<StreamId>,
    role: Option<VideoRole>,
    record: Option<RecordProtection>,
    /// Exact per-record reader, attached when a publisher enters
    /// [`VideoPhase::Streaming`] so retained frames never share an allocation
    /// with the generic receive buffer. Subscriber connections send only
    /// small chatter and stay on the receive buffer.
    reader: Option<VideoRecordReader>,
}

impl VideoConn {
    fn new() -> Self {
        Self {
            phase: VideoPhase::AwaitHello,
            session_id: None,
            stream_id: None,
            role: None,
            record: None,
            reader: None,
        }
    }
}

struct Session {
    user_id: UserId,
    username: String,
    tcp_token: Token,
    /// The room whose voice call this session is in, independent of which
    /// room's chat the client is reading.
    voice_room: Option<RoomId>,
    /// Full session material derived at handshake. The media loop receives its
    /// own codec and exclusively owns the mutable media counters/replay state.
    transport: SessionTransport,
    active_stream: Option<StreamId>,
    voice_status: control::ParticipantVoiceStatus,
    reported_server_rtt_ms: Option<u16>,
    server_rtt_reported_at: Option<Instant>,
    p2p: Option<P2pSessionState>,
    receive_files: bool,
    file_receive_limit_bytes: u64,
    /// Whether this session was announced with a `Presence` online
    /// broadcast; pairing throwaway connections are not, so their teardown
    /// must not broadcast a matching offline.
    announced: bool,
    /// Server wall-clock (UNIX ms) the session connected.
    connected_at_ms: u64,
    /// History fetches queued on the disk reader thread, bounded by
    /// [`MAX_PENDING_DISK_HISTORY_FETCHES`].
    pending_disk_history_fetches: u8,
    /// Active MLS credential proven on this exact authenticated session.
    mls_device: Option<BoundMlsDevice>,
    /// Credential authenticated before this account created its first MLS
    /// roster. The initial roster binds it to that roster's sole device.
    bootstrap_credential_hash: Option<String>,
}

enum IssuedSessionToken {
    OpenPair(String),
    DeviceLink(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BoundMlsDevice {
    device_id: DeviceId,
    roster: mls_identity::RosterCheckpoint,
    client_id: Vec<u8>,
}

fn require_mls_device(
    bound: Option<&BoundMlsDevice>,
    device_id: DeviceId,
) -> Result<&BoundMlsDevice, String> {
    match bound {
        Some(bound) if bound.device_id == device_id => Ok(bound),
        _ => Err("bind the named active MLS device first".to_string()),
    }
}

impl Session {
    #[cfg(test)]
    fn report_server_rtt(&mut self, rtt_ms: Option<u16>, now: Instant) {
        self.reported_server_rtt_ms = rtt_ms;
        self.server_rtt_reported_at = Some(now);
    }

    fn fresh_server_rtt(&self, now: Instant) -> Option<u16> {
        self.server_rtt_reported_at
            .filter(|reported_at| now.saturating_duration_since(*reported_at) < RTT_STALE_AFTER)
            .and(self.reported_server_rtt_ms)
    }
}

struct ServerUpload {
    server_transfer_id: FileTransferId,
    room_id: RoomId,
    encoding: FileContentEncoding,
    original_size: u64,
    wire_received: u64,
    /// Name held in `reserved_file_names` for the transfer's lifetime,
    /// released when the upload completes or cancels.
    file_name: String,
    recipients: HashSet<SessionId>,
    /// Set once the uploader has been told (via [`ServerControl::UploadDeclined`])
    /// that its recipient set emptied, so later chunks arriving before the
    /// uploader's cancel don't re-notify.
    declined_notified: bool,
}

fn session_accepts_file(session: &Session, original_size: u64) -> bool {
    session.receive_files && original_size <= session.file_receive_limit_bytes
}

fn upload_completion_is_valid(
    encoding: FileContentEncoding,
    original_size: u64,
    wire_received: u64,
) -> bool {
    match encoding {
        FileContentEncoding::Identity => wire_received == original_size,
        FileContentEncoding::Zstd | FileContentEncoding::Sealed => {
            original_size == 0 || wire_received != 0
        }
    }
}

fn wire_savings_percent(original_size: u64, wire_size: u64) -> i64 {
    if original_size == 0 {
        return 0;
    }
    let percent =
        (i128::from(original_size) - i128::from(wire_size)) * 100 / i128::from(original_size);
    percent.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn file_content_encoding_name(encoding: FileContentEncoding) -> &'static str {
    match encoding {
        FileContentEncoding::Identity => "identity",
        FileContentEncoding::Zstd => "zstd",
        FileContentEncoding::Sealed => "sealed",
    }
}

/// An in-progress bug report upload, accumulated until completion then written
/// to the configured `bug-report-dir`.
struct ServerBugReport {
    description: String,
    metadata: String,
    logs: Vec<u8>,
    expected: u64,
}

#[derive(Clone)]
struct P2pSessionState {
    generation: u64,
    nat: P2pNatKind,
    tie_breaker: u64,
    candidates: Vec<P2pCandidate>,
}

#[derive(Clone)]
struct PeerLink {
    connection_id: u64,
    low_to_high: KeyMaterial,
    high_to_low: KeyMaterial,
    stun_key: KeyMaterial,
}

struct InviteState {
    pairing_code_hash: String,
    expires_at: std::time::Instant,
}

struct DeviceLinkState {
    user_id: UserId,
    enrollment_bundle: Vec<u8>,
    current_roster: mls_identity::SignedDeviceRoster,
    expires_at: Instant,
    redemption: DeviceLinkRedemption,
}

#[derive(Clone)]
enum DeviceLinkRedemption {
    Active,
    Redeemed {
        device_id: DeviceId,
        credential_hash: String,
        attempt_id: rpc::ids::PairAttemptId,
    },
}

struct RoomState {
    id: RoomId,
    name: String,
    access: RoomAccess,
    active_streams: HashMap<StreamId, SessionId>,
}

/// Who may read, post to, and join voice in a room. Doubles as the source of
/// the wire-facing [`RoomKind`].
#[derive(Clone, Debug)]
enum RoomAccess {
    Public,
    Private(HashSet<UserId>),
    Dm(UserId, UserId),
}

impl RoomAccess {
    fn allows(&self, user_id: UserId) -> bool {
        match self {
            RoomAccess::Public => true,
            RoomAccess::Private(members) => members.contains(&user_id),
            RoomAccess::Dm(a, b) => *a == user_id || *b == user_id,
        }
    }

    fn kind(&self) -> RoomKind {
        match self {
            RoomAccess::Public => RoomKind::Public,
            RoomAccess::Private(members) => {
                let mut members: Vec<UserId> = members.iter().copied().collect();
                members.sort_unstable();
                RoomKind::Private { members }
            }
            RoomAccess::Dm(a, b) => RoomKind::Dm {
                user_a: *a,
                user_b: *b,
            },
        }
    }
}

/// Server-side state for one screen-share stream: the room it belongs to, the
/// per-stream secrets distributed over the control channel, the codec metadata
/// echoed to viewers, the live publisher and subscriber connections, and the
/// fast-start ring of recent frames.
struct VideoStream {
    room_id: RoomId,
    owner_session: SessionId,
    user_id: UserId,
    sender_name: String,
    publish_secret: [u8; KEY_LEN],
    view_secrets: HashMap<SessionId, [u8; KEY_LEN]>,
    codec: String,
    coded_width: u32,
    coded_height: u32,
    annexb: bool,
    extradata: Vec<u8>,
    publisher_conn: Option<Token>,
    subscribers: Vec<Token>,
    ring: VecDeque<VideoRingFrame>,
    ring_bytes: usize,
}

/// One cached frame in a stream's fast-start ring. `data` is the inner
/// [`video::write_video_frame`] plaintext (17-byte header plus bitstream),
/// shared so fan-out to many subscribers does not copy it. Ring budgets charge
/// [`SharedVideoFrame::retained_bytes`], the allocation actually held.
struct VideoRingFrame {
    data: SharedVideoFrame,
    is_key: bool,
}

/// Live tokens of every connected session whose user can access the room —
/// the fan-out set for chat, files, presence-in-room events, and catalog
/// updates. Text delivery is not gated on any join.
fn accessible_recipient_tokens(
    rooms: &HashMap<RoomId, RoomState>,
    sessions: &HashMap<SessionId, Session>,
    room_id: RoomId,
    excluded_session: Option<SessionId>,
    mut is_token_active: impl FnMut(Token) -> bool,
) -> Vec<Token> {
    let Some(room) = rooms.get(&room_id) else {
        return Vec::new();
    };
    sessions
        .iter()
        .filter(|(session_id, _)| Some(**session_id) != excluded_session)
        .filter(|(_, session)| room.access.allows(session.user_id))
        .map(|(_, session)| session.tcp_token)
        .filter(|token| is_token_active(*token))
        .collect()
}

fn dm_room_name(a: UserId, b: UserId) -> String {
    format!("dm:{}:{}", a.0, b.0)
}

/// Index of the most recent keyframe in a fast-start ring, the point a new
/// subscriber must begin from so its decoder bootstraps.
fn fast_start_index(ring: &VecDeque<VideoRingFrame>) -> Option<usize> {
    ring.iter().rposition(|frame| frame.is_key)
}

/// Whether a subscriber's queued bytes are still under the backpressure cap.
fn subscriber_within_limit(write_buf_len: usize) -> bool {
    write_buf_len <= VIDEO_SUBSCRIBER_HIGH_WATER
}

fn identity_control_needs_ordering(control: &ClientControl) -> bool {
    matches!(
        control,
        ClientControl::Authenticate { .. }
            | ClientControl::Pair { .. }
            | ClientControl::OpenPair { .. }
            | ClientControl::RedeemDeviceLink { .. }
    )
}

fn video_fanout_charge(data: &SharedVideoFrame, subscriber_capacity: usize) -> usize {
    data.retained_bytes()
        .saturating_add(subscriber_capacity.saturating_mul(std::mem::size_of::<Token>()))
        .saturating_add(std::mem::size_of::<VideoFanout>())
}

fn is_dynamic_user_id(user_id: UserId) -> bool {
    user_id.0 >= config::FIRST_DYNAMIC_USER_ID
}

fn prune_instants(queue: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while queue
        .front()
        .is_some_and(|then| now.saturating_duration_since(*then) >= window)
    {
        queue.pop_front();
    }
}

fn now_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as u64,
        Err(error) => {
            kvlog::warn!("system clock is before unix epoch", error = %error);
            0
        }
    }
}

fn invalid_config(error: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn server_usage() -> String {
    "usage: chatt-server serve CONFIG_PATH | chatt-server init-config CONFIG_PATH | chatt-server invite USER | chatt-server mls storage-status | chatt-server mls compact".to_string()
}

fn positional_args(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--logfile" {
            index += 2;
        } else {
            out.push(args[index].as_str());
            index += 1;
        }
    }
    out
}

fn is_fd_pressure_accept_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

fn random_secret_hex(rng: &aws_lc_rs::rand::SystemRandom) -> Result<String, String> {
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes)
        .map_err(|_| "failed to generate invite secret".to_string())?;
    Ok(encode_hex(&bytes))
}

fn device_link_secret_hash(secret: &str) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, secret.as_bytes())
        .as_ref()
        .to_vec()
}

fn ordered_pair(a: SessionId, b: SessionId) -> (SessionId, SessionId) {
    if a <= b { (a, b) } else { (b, a) }
}

fn random_secret(rng: &dyn SecureRandom) -> Result<[u8; KEY_LEN], String> {
    let mut bytes = [0u8; KEY_LEN];
    rng.fill(&mut bytes)
        .map_err(|_| "failed to generate video secret".to_string())?;
    Ok(bytes)
}

fn random_key(rng: &dyn SecureRandom) -> Result<KeyMaterial, String> {
    let mut bytes = [0u8; KEY_LEN];
    rng.fill(&mut bytes)
        .map_err(|_| "failed to generate P2P key".to_string())?;
    let mut id_bytes = [0u8; 4];
    rng.fill(&mut id_bytes)
        .map_err(|_| "failed to generate P2P key id".to_string())?;
    Ok(KeyMaterial {
        id: u32::from_le_bytes(id_bytes).max(1),
        bytes,
    })
}

fn p2p_key(key: &KeyMaterial) -> P2pKey {
    P2pKey {
        id: key.id,
        bytes: key.bytes.to_vec(),
    }
}

fn sanitize_file_name(name: &str) -> String {
    let trimmed = name.rsplit(['/', '\\']).next().unwrap_or("file").trim();
    let mut out = String::with_capacity(trimmed.len().max(4));
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches([' ', '.']);
    if out.is_empty() {
        "file".to_string()
    } else {
        out.to_string()
    }
}

fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => (&name[..index], &name[index..]),
        _ => (name, ""),
    }
}

fn reserve_unique_file_name(reserved: &mut HashSet<String>, requested: &str) -> String {
    let requested = sanitize_file_name(requested);
    if reserved.insert(requested.clone()) {
        return requested;
    }
    let (stem, extension) = split_extension(&requested);
    for index in 1u64.. {
        let candidate = format!("{stem}-{index}{extension}");
        if reserved.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("u64 filename suffix space exhausted")
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn audio_pop_logging_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_flag_enabled(AUDIO_POP_LOG_ENV))
}

fn env_flag_enabled(name: &str) -> bool {
    let Ok(value) = std::env::var(name) else {
        return false;
    };
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let normalized = value.to_ascii_lowercase();
    !matches!(normalized.as_str(), "0" | "false" | "no" | "off")
}

fn log_audio_pop_server_media_packet(
    direction: &'static str,
    session_id: SessionId,
    room_id: Option<RoomId>,
    stream_id: StreamId,
    sequence: u32,
    flags: u8,
    payload: &media::VoicePayloadRef<'_>,
    recipient_count: Option<usize>,
) {
    if !audio_pop_logging_enabled() || !audio_pop_should_log_packet(flags, payload) {
        return;
    }
    kvlog::info!(
        "audio pop server media packet",
        direction,
        session_id = session_id.0,
        room_id = room_id.map(|room_id| room_id.0),
        stream_id = stream_id.0,
        sequence,
        flags,
        flag_opus_reset = flags & AUDIO_POP_PACKET_FLAG_OPUS_RESET != 0,
        flag_silence_hint = flags & AUDIO_POP_PACKET_FLAG_SILENCE_HINT != 0,
        flag_silence_resume = flags & AUDIO_POP_PACKET_FLAG_SILENCE_RESUME != 0,
        flag_mute = flags & AUDIO_POP_PACKET_FLAG_MUTE != 0,
        payload_size = payload.len(),
        payload_kind = server_voice_payload_kind(payload),
        recipient_count
    );
}

fn audio_pop_should_log_packet(flags: u8, payload: &media::VoicePayloadRef<'_>) -> bool {
    flags != 0 || matches!(payload, media::VoicePayloadRef::Silence)
}

fn server_voice_payload_kind(payload: &media::VoicePayloadRef<'_>) -> &'static str {
    match payload {
        media::VoicePayloadRef::Opus(_) => "opus",
        media::VoicePayloadRef::Silence => "silence",
    }
}

fn conn_state_name(state: ConnState) -> &'static str {
    match state {
        ConnState::AwaitClientHello => "await_client_hello",
        ConnState::AwaitAuth => "await_auth",
        ConnState::Ready => "ready",
    }
}

fn client_control_kind(control: &ClientControl) -> &'static str {
    match control {
        ClientControl::Authenticate { .. } => "authenticate",
        ClientControl::Pair { .. } => "pair",
        ClientControl::OpenPair { .. } => "open_pair",
        ClientControl::SendChat { .. } => "send_chat",
        ClientControl::SetVoiceStatus { .. } => "set_voice_status",
        ClientControl::PublishP2p { .. } => "publish_p2p",
        ClientControl::UploadFileStart { .. } => "upload_file_start",
        ClientControl::UploadFileChunk { .. } => "upload_file_chunk",
        ClientControl::UploadFileComplete { .. } => "upload_file_complete",
        ClientControl::UploadFileCancel { .. } => "upload_file_cancel",
        ClientControl::StartShare { .. } => "start_share",
        ClientControl::StopShare { .. } => "stop_share",
        ClientControl::Ping { .. } => "ping",
        ClientControl::BugReportStart { .. } => "bug_report_start",
        ClientControl::BugReportChunk { .. } => "bug_report_chunk",
        ClientControl::BugReportComplete { .. } => "bug_report_complete",
        ClientControl::FetchHistory { .. } => "fetch_history",
        ClientControl::JoinVoice { .. } => "join_voice",
        ClientControl::LeaveVoice => "leave_voice",
        ClientControl::OpenDm { .. } => "open_dm",
        ClientControl::SkipFile { .. } => "skip_file",
        ClientControl::EditChat { .. } => "edit_chat",
        ClientControl::DeleteChat { .. } => "delete_chat",
        ClientControl::CreateDeviceLink { .. } => "create_device_link",
        ClientControl::CancelDeviceLink { .. } => "cancel_device_link",
        ClientControl::FetchDeviceLink { .. } => "fetch_device_link",
        ClientControl::RedeemDeviceLink { .. } => "redeem_device_link",
        ClientControl::FetchDeviceRoster { .. } => "fetch_device_roster",
        ClientControl::PutDeviceRoster { .. } => "put_device_roster",
        ClientControl::BindMlsDevice { .. } => "bind_mls_device",
        ClientControl::PublishKeyPackages { .. } => "publish_key_packages",
        ClientControl::TakeKeyPackage { .. } => "take_key_package",
        ClientControl::CreateEncryptedRoom { .. } => "create_encrypted_room",
        ClientControl::FetchGroupInfo { .. } => "fetch_group_info",
        ClientControl::SubmitCommitBundle { .. } => "submit_commit_bundle",
        ClientControl::SubmitMlsApplication { .. } => "submit_mls_application",
        ClientControl::FetchMlsEvents { .. } => "fetch_mls_events",
        ClientControl::FetchMlsWelcome { .. } => "fetch_mls_welcome",
        ClientControl::AckMlsEvent { .. } => "ack_mls_event",
        ClientControl::AckMlsWelcome { .. } => "ack_mls_welcome",
    }
}

fn video_role_name(role: VideoRole) -> &'static str {
    match role {
        VideoRole::Publisher => "publisher",
        VideoRole::Subscriber => "subscriber",
    }
}

fn server_control_kind(control: &ServerControl) -> &'static str {
    match control {
        ServerControl::Authenticated { .. } => "authenticated",
        ServerControl::OpenPaired { .. } => "open_paired",
        ServerControl::Chat { .. } => "chat",
        ServerControl::ChatMutationRejected { .. } => "chat_mutation_rejected",
        ServerControl::Presence { .. } => "presence",
        ServerControl::VoiceStarted { .. } => "voice_started",
        ServerControl::VoiceStopped { .. } => "voice_stopped",
        ServerControl::VoiceStatus { .. } => "voice_status",
        ServerControl::VoiceJoinFailed { .. } => "voice_join_failed",
        ServerControl::RoomRttSnapshot { .. } => "room_rtt_snapshot",
        ServerControl::UdpBound => "udp_bound",
        ServerControl::UdpReflexive { .. } => "udp_reflexive",
        ServerControl::P2pNatProbe { .. } => "p2p_nat_probe",
        ServerControl::P2pPeer { .. } => "p2p_peer",
        ServerControl::P2pPeerGone { .. } => "p2p_peer_gone",
        ServerControl::FileOffered { .. } => "file_offered",
        ServerControl::UploadFileAccepted { .. } => "upload_file_accepted",
        ServerControl::FileChunk { .. } => "file_chunk",
        ServerControl::FileComplete { .. } => "file_complete",
        ServerControl::FileCanceled { .. } => "file_canceled",
        ServerControl::ShareStarted { .. } => "share_started",
        ServerControl::ShareAvailable { .. } => "share_available",
        ServerControl::ShareEnded { .. } => "share_ended",
        ServerControl::Pong { .. } => "pong",
        ServerControl::Error { .. } => "error",
        ServerControl::BugReportSaved { .. } => "bug_report_saved",
        ServerControl::RoomUpserted { .. } => "room_upserted",
        ServerControl::DmOpened { .. } => "dm_opened",
        ServerControl::UploadDeclined { .. } => "upload_declined",
        ServerControl::DeviceLinkCreated { .. } => "device_link_created",
        ServerControl::DeviceLinkCanceled { .. } => "device_link_canceled",
        ServerControl::DeviceLinkBundle { .. } => "device_link_bundle",
        ServerControl::DeviceLinked { .. } => "device_linked",
        ServerControl::DeviceLinkRedeemed { .. } => "device_link_redeemed",
        ServerControl::DeviceRoster { .. } => "device_roster",
        ServerControl::DeviceRosterStored { .. } => "device_roster_stored",
        ServerControl::DeviceRosterConflict { .. } => "device_roster_conflict",
        ServerControl::MlsDeviceBound { .. } => "mls_device_bound",
        ServerControl::KeyPackagesPublished { .. } => "key_packages_published",
        ServerControl::KeyPackagesLow { .. } => "key_packages_low",
        ServerControl::KeyPackage { .. } => "key_package",
        ServerControl::EncryptedRoomCreated { .. } => "encrypted_room_created",
        ServerControl::GroupInfo { .. } => "group_info",
        ServerControl::MlsCommitSubmitted { .. } => "mls_commit_submitted",
        ServerControl::MlsApplicationSubmitted { .. } => "mls_application_submitted",
        ServerControl::MlsEvents { .. } => "mls_events",
        ServerControl::MlsWelcomes { .. } => "mls_welcomes",
    }
}

fn collect_latest_welcomes(
    mls: &MlsService,
    devices: &[DeviceId],
) -> Result<Vec<(DeviceId, Vec<rpc::mls::MlsWelcome>, u64)>, String> {
    devices
        .iter()
        .map(|device_id| {
            let head = mls.welcome_head(*device_id)?;
            let welcomes = mls.welcomes(*device_id, head.saturating_sub(1))?;
            Ok((*device_id, welcomes, head))
        })
        .collect()
}

fn mls_submit_outcome_name(outcome: &rpc::mls::MlsSubmitOutcome) -> &'static str {
    match outcome {
        rpc::mls::MlsSubmitOutcome::Stored { .. } => "stored",
        rpc::mls::MlsSubmitOutcome::AlreadyStored { .. } => "already_stored",
        rpc::mls::MlsSubmitOutcome::StaleEpochNotStored { .. } => "stale_epoch",
        rpc::mls::MlsSubmitOutcome::RevocationPending => "revocation_pending",
        rpc::mls::MlsSubmitOutcome::RejoinRequired => "rejoin_required",
        rpc::mls::MlsSubmitOutcome::TemporarilyBlocked => "temporarily_blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::crypto::{SessionSecrets, TAG_LEN, TRANSPORT_HEADER_LEN, TransportCipher};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    fn finish_test_roster(server: &mut Server) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut events = server.mls_events.drain();
            while let Some(event) = events.pop_front() {
                if let MlsReply::RosterStored { result, .. } = event {
                    let (_, state) = result.expect("test roster stored");
                    server.install_mls_cache(state).expect("test roster cache");
                    return;
                }
            }
            assert!(Instant::now() < deadline, "MLS worker reply");
            thread::yield_now();
        }
    }

    fn finish_mls_for_token(server: &mut Server, token: Token) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.pending_mls.contains(&token) {
            server.drain_mls_replies();
            assert!(Instant::now() < deadline, "MLS worker reply");
            thread::yield_now();
        }
    }

    fn put_test_roster(
        server: &mut Server,
        user_id: UserId,
        roster: mls_identity::SignedDeviceRoster,
    ) {
        assert!(server.mls_worker.enqueue_typed(MlsWriteRequest::PutRoster {
            token: Token(usize::MAX),
            session_id: SessionId(u64::MAX),
            user_id,
            expected: None,
            roster,
            bootstrap_credential_hash: None,
        }));
        finish_test_roster(server);
    }

    fn test_room(room_id: RoomId, voice_members: &[SessionId]) -> RoomState {
        RoomState {
            id: room_id,
            name: "test".to_string(),
            access: RoomAccess::Public,
            active_streams: voice_members
                .iter()
                .enumerate()
                .map(|(index, session_id)| (StreamId(index as u32 + 100), *session_id))
                .collect(),
        }
    }

    fn private_room(room_id: RoomId, members: &[UserId]) -> RoomState {
        RoomState {
            id: room_id,
            name: "secret".to_string(),
            access: RoomAccess::Private(members.iter().copied().collect()),
            active_streams: HashMap::new(),
        }
    }

    fn test_session(user_id: UserId, token: Token, voice_room: Option<RoomId>) -> Session {
        Session {
            user_id,
            username: format!("user-{}", user_id.0),
            tcp_token: token,
            voice_room,
            transport: test_transport(1),
            active_stream: None,
            voice_status: control::ParticipantVoiceStatus::default(),
            reported_server_rtt_ms: None,
            server_rtt_reported_at: None,
            p2p: None,
            receive_files: false,
            file_receive_limit_bytes: 0,
            announced: true,
            connected_at_ms: 0,
            pending_disk_history_fetches: 0,
            mls_device: None,
            bootstrap_credential_hash: None,
        }
    }

    fn decode_history_chunk(payload: &[u8]) -> history::HistoryChunk {
        history::decode_chunk(payload)
            .expect("decode history chunk")
            .expect("history chunk payload")
    }

    #[test]
    fn positional_args_skip_global_logfile_pair() {
        let args = vec![
            "chatt-server".to_string(),
            "--logfile".to_string(),
            "/tmp/chatt-server.log".to_string(),
            "serve".to_string(),
            "server.toml".to_string(),
        ];

        assert_eq!(positional_args(&args), vec!["serve", "server.toml"]);
    }

    fn test_server_config() -> ServerConfig {
        let mut config = ServerConfig::default();
        config.network.tcp_addr = "127.0.0.1:0".parse().expect("valid tcp addr");
        config.network.udp_addr = Some("127.0.0.1:0".parse().expect("valid udp addr"));
        config.network.udp_probe_addr = None;
        config.network.p2p_enabled = false;
        config
    }

    fn test_server() -> Server {
        Server::bind(test_server_config()).expect("test server")
    }

    #[test]
    fn bug_report_completion_arrives_from_background_writer() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-bug-server-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = test_server_config();
        config.security.bug_report_dir = Some(dir.display().to_string());
        let mut server = Server::bind(config).expect("test server");
        let session_id = SessionId(3);
        let report_id = BugReportId(7);
        let token = Token(11);
        let mut peer = live_user(&mut server, token, session_id, UserId(5));

        server
            .start_bug_report(
                session_id,
                report_id,
                "stuck mute".to_string(),
                "{\"version\":\"0.1.0\"}".to_string(),
                4,
            )
            .unwrap();
        server
            .receive_bug_report_chunk(session_id, report_id, 0, vec![1, 2, 3, 4])
            .unwrap();
        server.complete_bug_report(session_id, report_id).unwrap();
        assert!(server.pending_bug_reports.contains(&(session_id, report_id)));

        let deadline = Instant::now() + Duration::from_secs(5);
        while server.pending_bug_reports.contains(&(session_id, report_id)) {
            server.drain_bug_report_replies();
            assert!(Instant::now() < deadline, "bug report completion");
            thread::yield_now();
        }

        assert_eq!(
            read_plaintext_server_control(&mut server, &mut peer),
            ServerControl::BugReportSaved { report_id }
        );
        let logs_path = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "zst"))
            .expect("bug report logs");
        assert_eq!(std::fs::read(logs_path).unwrap(), vec![1, 2, 3, 4]);

        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bug_report_upload_count_is_bounded_per_session() {
        let mut server = test_server();
        server.config.security.bug_report_dir = Some("/tmp/chatt-unused-bug-dir".to_string());
        let session_id = SessionId(3);
        server
            .start_bug_report(
                session_id,
                BugReportId(1),
                String::new(),
                String::new(),
                0,
            )
            .unwrap();

        let error = server
            .start_bug_report(
                session_id,
                BugReportId(2),
                String::new(),
                String::new(),
                0,
            )
            .unwrap_err();

        assert!(error.contains("already active"));
    }

    #[test]
    fn invite_ticket_uses_advertised_public_udp_endpoint() {
        let mut server = test_server();
        server.config.network.public_tcp_addr = "104.247.224.7:41000".to_string();
        server.config.network.public_udp_addr = "104.247.224.7:41000".to_string();

        let join_string = server.create_invite("alice").unwrap();
        let ticket = control::decode_invite_ticket(&join_string).unwrap();

        assert_eq!(ticket.tcp_addr, "104.247.224.7:41000");
        assert_eq!(ticket.udp_addr, "104.247.224.7:41000");
    }

    #[test]
    fn room_rtt_snapshot_is_sorted_and_expires_stale_reports() {
        let mut server = test_server();
        let now = Instant::now();
        let room_id = RoomId(1);
        let first_id = SessionId(1);
        let second_id = SessionId(2);
        server
            .rooms
            .insert(room_id, test_room(room_id, &[first_id, second_id]));
        let mut first = test_session(UserId(9), Token(11), Some(room_id));
        first.report_server_rtt(Some(45), now);
        let mut second = test_session(UserId(5), Token(22), Some(room_id));
        second.report_server_rtt(Some(30), now - RTT_STALE_AFTER);
        server.sessions.insert(first_id, first);
        server.sessions.insert(second_id, second);

        assert_eq!(
            server.room_rtt_snapshot(room_id, now),
            vec![
                control::ParticipantServerRtt {
                    user_id: UserId(5),
                    server_rtt_ms: None,
                },
                control::ParticipantServerRtt {
                    user_id: UserId(9),
                    server_rtt_ms: Some(45),
                },
            ]
        );
    }

    fn test_key(id: u32, byte: u8) -> KeyMaterial {
        KeyMaterial {
            id,
            bytes: [byte; KEY_LEN],
        }
    }

    /// A native-mode session transport with the given media route id. The media
    /// send/recv keys are identical so one [`media::MediaProtection`] both seals
    /// (as the peer) and opens (as this session) a datagram in tests.
    fn test_transport(route_id: u32) -> SessionTransport {
        SessionTransport {
            mode: TransportMode::NativeEncrypted,
            secrets: SessionSecrets {
                control_send: test_key(1, 1),
                control_recv: test_key(2, 2),
                media_send: test_key(4, 4),
                media_recv: test_key(4, 4),
            },
            route_id,
            bind_key: [5; KEY_LEN],
            video_auth_key: [6; KEY_LEN],
        }
    }

    #[test]
    fn media_route_collision_rejects_new_session_and_keeps_survivor() {
        // A 32-bit media route id collision must not silently overwrite the live
        // session's mapping: `teardown_session` removes the shared route when
        // either session ends, permanently stranding the survivor's UDP.
        let mut server = test_server();
        let alice = UserConfig {
            id: UserId(1),
            internal_reference: "alice".to_string(),
            username: "Alice".to_string(),
            token_hash: String::new(),
        };
        let bob = UserConfig {
            id: UserId(2),
            internal_reference: "bob".to_string(),
            username: "Bob".to_string(),
            token_hash: String::new(),
        };
        let (conn, _alice_peer) = test_live_client();
        server.clients.insert(Token(11), conn);
        server.clients.get_mut(&Token(11)).unwrap().transport = Some(test_transport(77));
        let (conn, _bob_peer) = test_live_client();
        server.clients.insert(Token(22), conn);
        server.clients.get_mut(&Token(22)).unwrap().transport = Some(test_transport(77));

        server
            .establish_session(Token(11), &alice, false, 0, false, None)
            .unwrap();
        let alice_session = server.clients.get(&Token(11)).unwrap().session_id.unwrap();
        assert_eq!(server.reserved_media_routes.get(&77), Some(&alice_session));

        let result = server.establish_session(Token(22), &bob, false, 0, false, None);
        assert!(
            result.is_err(),
            "a colliding media route must reject the new session"
        );
        assert_eq!(
            server.reserved_media_routes.get(&77),
            Some(&alice_session),
            "the live session's mapping must survive the collision"
        );
    }

    #[test]
    fn media_route_of_dead_session_is_reclaimed() {
        // A mapping left behind by a torn-down session (the sweep backstop has
        // not run yet) must not block a new session from taking the route id.
        let mut server = test_server();
        server.reserved_media_routes.insert(77, SessionId(999));
        let carol = UserConfig {
            id: UserId(3),
            internal_reference: "carol".to_string(),
            username: "Carol".to_string(),
            token_hash: String::new(),
        };
        let (conn, _carol_peer) = test_live_client();
        server.clients.insert(Token(33), conn);
        server.clients.get_mut(&Token(33)).unwrap().transport = Some(test_transport(77));

        server
            .establish_session(Token(33), &carol, false, 0, false, None)
            .unwrap();
        let carol_session = server.clients.get(&Token(33)).unwrap().session_id.unwrap();
        assert_eq!(server.reserved_media_routes.get(&77), Some(&carol_session));
    }

    #[test]
    fn room_allows_denies_non_member_and_missing_room() {
        let mut server = test_server();
        let secret = RoomId(3);
        server
            .rooms
            .insert(secret, private_room(secret, &[UserId(1)]));
        let member = SessionId(1);
        let outsider = SessionId(2);
        server
            .sessions
            .insert(member, test_session(UserId(1), Token(11), None));
        server
            .sessions
            .insert(outsider, test_session(UserId(2), Token(22), None));

        assert!(server.room_allows(member, secret));
        assert!(!server.room_allows(outsider, secret));
        assert!(!server.room_allows(member, RoomId(99)));
        assert!(!server.room_allows(SessionId(99), secret));
    }

    #[test]
    fn join_voice_answers_voice_join_failed_for_denied_and_missing_rooms() {
        // The private-room invisibility policy: a denied voice join and a voice
        // join to a nonexistent room must answer identically.
        let mut server = test_server();
        let secret = RoomId(3);
        server
            .rooms
            .insert(secret, private_room(secret, &[UserId(1)]));
        let outsider = SessionId(2);
        let mut peer = live_user(&mut server, Token(22), outsider, UserId(2));

        server.join_voice(outsider, secret);
        let denied = read_until(&mut server, &mut peer, |control| {
            matches!(control, ServerControl::VoiceJoinFailed { .. })
        });
        server.join_voice(outsider, RoomId(99));
        let missing = read_until(&mut server, &mut peer, |control| {
            matches!(control, ServerControl::VoiceJoinFailed { .. })
        });

        let ServerControl::VoiceJoinFailed { room_id, message } = denied else {
            unreachable!();
        };
        assert_eq!(room_id, secret);
        assert_eq!(message, "room not found");
        let ServerControl::VoiceJoinFailed { room_id, message } = missing else {
            unreachable!();
        };
        assert_eq!(room_id, RoomId(99));
        assert_eq!(message, "room not found");
    }

    #[test]
    fn voice_status_normalizes_deafened_to_muted() {
        let mut server = test_server();
        let room_id = RoomId(1);
        let session_id = SessionId(1);
        server
            .rooms
            .insert(room_id, test_room(room_id, &[session_id]));
        server.sessions.insert(
            session_id,
            test_session(UserId(9), Token(11), Some(room_id)),
        );

        server.set_voice_status(
            session_id,
            control::ParticipantVoiceStatus {
                muted: false,
                deafened: true,
            },
        );

        let session = server.sessions.get(&session_id).expect("session exists");
        assert_eq!(
            session.voice_status,
            control::ParticipantVoiceStatus {
                muted: true,
                deafened: true,
            }
        );
    }

    #[test]
    fn user_summaries_hides_unannounced_sessions() {
        let mut server = test_server();
        server.users.users.push(UserConfig {
            id: UserId(9),
            internal_reference: "bob".to_string(),
            username: "Bob".to_string(),
            token_hash: String::new(),
        });
        let mut configured = test_session(UserId(9), Token(11), None);
        configured.announced = false;
        server.sessions.insert(SessionId(1), configured);
        let dynamic_id = UserId(config::FIRST_DYNAMIC_USER_ID);
        let mut dynamic = test_session(dynamic_id, Token(22), None);
        dynamic.announced = false;
        server.sessions.insert(SessionId(2), dynamic);

        let users = server.user_summaries();

        let configured = users
            .iter()
            .find(|user| user.user_id == UserId(9))
            .expect("configured users stay listed");
        assert!(
            !configured.online,
            "an unannounced session must not count as online"
        );
        assert_eq!(configured.connected_at_ms, 0);
        assert!(
            users.iter().all(|user| user.user_id != dynamic_id),
            "an unannounced dynamic session must stay out of the directory"
        );
    }

    fn live_user(
        server: &mut Server,
        token: Token,
        session_id: SessionId,
        user_id: UserId,
    ) -> std::net::TcpStream {
        let (conn, peer) = test_live_client();
        server.clients.insert(token, conn);
        server
            .sessions
            .insert(session_id, test_session(user_id, token, None));
        peer
    }

    /// Like [`live_user`], but the session accepts relayed files, so it lands in
    /// a transfer's recipient set.
    fn live_receiver(
        server: &mut Server,
        token: Token,
        session_id: SessionId,
        user_id: UserId,
    ) -> std::net::TcpStream {
        let (conn, peer) = test_live_client();
        server.clients.insert(token, conn);
        let mut session = test_session(user_id, token, None);
        session.receive_files = true;
        session.file_receive_limit_bytes = u64::MAX;
        server.sessions.insert(session_id, session);
        peer
    }

    fn read_until(
        server: &mut Server,
        peer: &mut std::net::TcpStream,
        accept: impl Fn(&ServerControl) -> bool,
    ) -> ServerControl {
        for _ in 0..64 {
            let control = read_plaintext_server_control(server, peer);
            if accept(&control) {
                return control;
            }
        }
        panic!("expected control was not received");
    }

    fn read_until_history_chunk(
        server: &mut Server,
        peer: &mut std::net::TcpStream,
    ) -> history::HistoryChunk {
        for _ in 0..64 {
            let payload = read_plaintext_server_payload(server, peer);
            if let Some(chunk) = history::decode_chunk(&payload).expect("decode history chunk") {
                return chunk;
            }
            let _ = rpc::control::decode_server_control(&payload).expect("decode server control");
        }
        panic!("expected history chunk was not received");
    }

    fn assert_no_control(peer: &mut std::net::TcpStream) {
        peer.set_read_timeout(Some(Duration::from_millis(200)))
            .expect("set read timeout");
        let mut byte = [0u8; 1];
        match peer.read(&mut byte) {
            Ok(0) => {}
            Ok(_) => panic!("unexpected control bytes received"),
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ),
                "unexpected read error: {error}"
            ),
        }
    }

    #[test]
    fn chat_fans_out_to_all_accessible_sessions() {
        let mut server = test_server();
        let sender = SessionId(1);
        let reader = SessionId(2);
        let mut sender_peer = live_user(&mut server, Token(11), sender, UserId(1));
        let mut reader_peer = live_user(&mut server, Token(22), reader, UserId(2));

        server
            .send_chat(sender, RoomId(1), "hello everyone".to_string())
            .unwrap();

        for peer in [&mut sender_peer, &mut reader_peer] {
            let control = read_until(&mut server, peer, |control| {
                matches!(control, ServerControl::Chat { .. })
            });
            let ServerControl::Chat { message } = control else {
                unreachable!();
            };
            assert_eq!(message.body, "hello everyone");
            assert_eq!(message.room_id, RoomId(1));
            assert_eq!(message.message_id, MessageId(1));
        }
    }

    #[test]
    fn chat_mutations_fan_out_and_foreign_mutations_are_dropped() {
        let mut config = test_server_config();
        config.rooms[0].persistence = config::RoomPersistenceConfig::Memory;
        config.rooms[0].memory_limit = Some(16);
        let mut server = Server::bind(config).expect("test server");
        let sender = SessionId(1);
        let reader = SessionId(2);
        let mut sender_peer = live_user(&mut server, Token(11), sender, UserId(1));
        let mut reader_peer = live_user(&mut server, Token(22), reader, UserId(2));

        server
            .send_chat(sender, RoomId(1), "original".to_string())
            .unwrap();
        let target = server.store.head(RoomId(1)).expect("stored chat message");
        server
            .mutate_chat(
                sender,
                RoomId(1),
                target,
                MutationKind::Edit,
                "revised".to_string(),
            )
            .unwrap();
        for peer in [&mut sender_peer, &mut reader_peer] {
            let control = read_until(
                &mut server,
                peer,
                |control| matches!(control, ServerControl::Chat { message } if message.target.is_some()),
            );
            let ServerControl::Chat { message } = control else {
                unreachable!();
            };
            assert!(message.flags.edited());
            assert_eq!(message.target, Some(target));
            assert_eq!(message.body, "revised");
            assert!(message.message_id > target);
        }

        server
            .mutate_chat(
                reader,
                RoomId(1),
                target,
                MutationKind::Edit,
                "hijacked".to_string(),
            )
            .unwrap();
        assert_no_control(&mut sender_peer);
        let rejected = read_until(&mut server, &mut reader_peer, |control| {
            matches!(control, ServerControl::ChatMutationRejected { .. })
        });
        assert!(matches!(
            rejected,
            ServerControl::ChatMutationRejected {
                room_id: RoomId(1),
                target: rejected_target,
                kind: ChatMutationKind::Edit,
                ..
            } if rejected_target == target
        ));

        server
            .mutate_chat(
                reader,
                RoomId(1),
                target,
                MutationKind::Delete,
                String::new(),
            )
            .unwrap();
        let rejected = read_until(&mut server, &mut reader_peer, |control| {
            matches!(control, ServerControl::ChatMutationRejected { .. })
        });
        assert!(matches!(
            rejected,
            ServerControl::ChatMutationRejected {
                room_id: RoomId(1),
                target: rejected_target,
                kind: ChatMutationKind::Delete,
                ..
            } if rejected_target == target
        ));

        server
            .mutate_chat(
                sender,
                RoomId(1),
                target,
                MutationKind::Delete,
                String::new(),
            )
            .unwrap();
        let control = read_until(
            &mut server,
            &mut reader_peer,
            |control| matches!(control, ServerControl::Chat { message } if message.flags.deleted()),
        );
        let ServerControl::Chat { message } = control else {
            unreachable!();
        };
        assert_eq!(message.target, Some(target));
        assert!(message.body.is_empty());
    }

    #[test]
    fn stateless_chat_mutations_fan_out_for_client_validation() {
        let mut server = test_server();
        let sender = SessionId(1);
        let reader = SessionId(2);
        let mut sender_peer = live_user(&mut server, Token(11), sender, UserId(1));
        let mut reader_peer = live_user(&mut server, Token(22), reader, UserId(2));

        server
            .send_chat(sender, RoomId(1), "original".to_string())
            .unwrap();
        let target = server.store.head(RoomId(1)).expect("recent chat message");
        server
            .mutate_chat(
                reader,
                RoomId(1),
                target,
                MutationKind::Edit,
                "foreign edit".to_string(),
            )
            .unwrap();

        for peer in [&mut sender_peer, &mut reader_peer] {
            let control = read_until(
                &mut server,
                peer,
                |control| matches!(control, ServerControl::Chat { message } if message.target.is_some()),
            );
            let ServerControl::Chat { message } = control else {
                unreachable!();
            };
            assert_eq!(message.target, Some(target));
            assert_eq!(message.sender, UserId(2));
            assert_eq!(message.body, "foreign edit");
        }
    }

    #[test]
    fn private_room_is_indistinguishable_from_missing() {
        let mut server = test_server();
        let secret = RoomId(3);
        server
            .rooms
            .insert(secret, private_room(secret, &[UserId(1)]));
        let outsider = SessionId(2);
        let mut outsider_peer = live_user(&mut server, Token(22), outsider, UserId(2));

        server
            .send_chat(outsider, secret, "let me in".to_string())
            .unwrap();
        let denied = read_until(&mut server, &mut outsider_peer, |control| {
            matches!(control, ServerControl::Error { .. })
        });

        server.fetch_history(outsider, RoomId(99), None, 10);
        let missing = read_until(&mut server, &mut outsider_peer, |control| {
            matches!(control, ServerControl::Error { .. })
        });

        assert_eq!(
            denied, missing,
            "private and missing rooms must answer identically"
        );
        let ServerControl::Error { code, .. } = denied else {
            unreachable!();
        };
        assert_eq!(code, control::ERROR_ROOM_NOT_FOUND);
    }

    #[test]
    fn dm_open_is_idempotent_per_user_pair() {
        let dir = std::env::temp_dir().join(format!("chatt-dm-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut server = test_server();
        server.config.storage.data_dir = Some(dir.display().to_string());
        let requester = SessionId(1);
        let peer_session = SessionId(2);
        let mut requester_peer = live_user(&mut server, Token(11), requester, UserId(1));
        let mut endpoint_peer = live_user(&mut server, Token(22), peer_session, UserId(2));

        server.open_dm(requester, UserId(2));
        let upserted = read_until(&mut server, &mut requester_peer, |control| {
            matches!(control, ServerControl::RoomUpserted { .. })
        });
        let ServerControl::RoomUpserted { room } = upserted else {
            unreachable!();
        };
        assert_eq!(
            room.kind,
            RoomKind::Dm {
                user_a: UserId(1),
                user_b: UserId(2),
            }
        );
        let opened = read_until(&mut server, &mut requester_peer, |control| {
            matches!(control, ServerControl::DmOpened { .. })
        });
        let ServerControl::DmOpened { room_id, peer } = opened else {
            unreachable!();
        };
        assert_eq!(room_id, room.room_id);
        assert_eq!(peer, UserId(2));
        read_until(&mut server, &mut endpoint_peer, |control| {
            matches!(control, ServerControl::RoomUpserted { .. })
        });

        server.open_dm(peer_session, UserId(1));
        let reopened = read_until(&mut server, &mut endpoint_peer, |control| {
            matches!(control, ServerControl::DmOpened { .. })
        });
        let ServerControl::DmOpened {
            room_id: second_id, ..
        } = reopened
        else {
            unreachable!();
        };
        assert_eq!(second_id, room_id, "reopening must reuse the same DM room");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_dm_is_published_only_after_async_state_completion() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-server-async-dm-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = test_server_config();
        config.storage.data_dir = Some(dir.display().to_string());
        let mut server = Server::bind(config).expect("test server");
        let requester = SessionId(1);
        let peer_session = SessionId(2);
        let mut requester_peer = live_user(&mut server, Token(11), requester, UserId(1));
        let _peer = live_user(&mut server, Token(22), peer_session, UserId(2));

        server.open_dm(requester, UserId(2));

        assert_eq!(server.pending_dm_waiters.len(), 1);
        assert!(server
            .rooms
            .keys()
            .all(|room_id| room_id.0 < config::FIRST_DYNAMIC_ROOM_ID));
        drive_scheduled_work(&mut server);
        assert!(server.pending_dm_waiters.is_empty());
        assert!(matches!(
            read_until(&mut server, &mut requester_peer, |control| matches!(
                control,
                ServerControl::DmOpened { .. }
            )),
            ServerControl::DmOpened { .. }
        ));

        drop(server);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn dm_open_reply_reaches_requester_not_peers_other_devices() {
        let mut server = test_server();
        let bob = SessionId(1);
        let mut bob_peer = live_user(&mut server, Token(11), bob, UserId(2));
        let mut alice_first = live_user(&mut server, Token(22), SessionId(2), UserId(1));
        let mut alice_second = live_user(&mut server, Token(33), SessionId(3), UserId(1));

        server.open_dm(bob, UserId(1));

        let opened = read_until(&mut server, &mut bob_peer, |control| {
            matches!(control, ServerControl::DmOpened { .. })
        });
        assert!(matches!(
            opened,
            ServerControl::DmOpened {
                peer: UserId(1),
                ..
            }
        ));
        for peer in [&mut alice_first, &mut alice_second] {
            assert!(matches!(
                read_until(&mut server, peer, |control| matches!(
                    control,
                    ServerControl::RoomUpserted { .. }
                )),
                ServerControl::RoomUpserted { .. }
            ));
            assert_no_control(peer);
        }
    }

    #[test]
    fn dm_open_with_self_is_rejected() {
        let mut server = test_server();
        let requester = SessionId(1);
        let mut requester_peer = live_user(&mut server, Token(11), requester, UserId(1));

        server.open_dm(requester, UserId(1));

        let control = read_until(&mut server, &mut requester_peer, |control| {
            matches!(control, ServerControl::Error { .. })
        });
        let ServerControl::Error { code, .. } = control else {
            unreachable!();
        };
        assert_eq!(code, control::ERROR_REQUEST_REJECTED);
        assert!(server.store.dm_rooms().is_empty());
        assert!(
            server
                .rooms
                .values()
                .all(|room| !matches!(room.access, RoomAccess::Dm(..))),
            "a self dm room must not be created"
        );
    }

    #[test]
    fn dm_room_survives_restart() {
        let dir = std::env::temp_dir().join(format!("chatt-dm-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut server = test_server();
        server.config.storage.data_dir = Some(dir.display().to_string());
        server.store = RoomStore::open(server.config.data_dir(), &server.config.rooms);
        let requester = SessionId(1);
        let _requester_peer = live_user(&mut server, Token(11), requester, UserId(1));
        let _peer_peer = live_user(&mut server, Token(22), SessionId(2), UserId(2));
        server.open_dm(requester, UserId(2));
        let dm_room = server
            .store
            .dm_room_for(UserId(1), UserId(2))
            .expect("dm registered");
        let mut config = server.config.clone();
        config.network.tcp_addr = "127.0.0.1:0".parse().unwrap();
        config.network.udp_addr = Some("127.0.0.1:0".parse().unwrap());
        drop(server);

        let restarted = Server::bind(config).expect("restarted server");

        let room = restarted.rooms.get(&dm_room).expect("dm room restored");
        assert!(matches!(
            room.access,
            RoomAccess::Dm(UserId(1), UserId(2)) | RoomAccess::Dm(UserId(2), UserId(1))
        ));
        assert_eq!(
            restarted.store.dm_room_for(UserId(1), UserId(2)),
            Some(dm_room)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_mls_roster(
        server: &Server,
        user_id: UserId,
        authority_seed: [u8; 32],
        devices: &[(u8, &str)],
        revision: u64,
    ) -> mls_identity::SignedDeviceRoster {
        let server_id: [u8; 32] = server
            .server_key_pair
            .public_key()
            .as_ref()
            .try_into()
            .unwrap();
        let authority_public_key = mls_identity::authority_public_key(&authority_seed).unwrap();
        let account_id = mls_identity::account_id(&server_id, user_id, &authority_public_key);
        let mut active_devices = devices
            .iter()
            .map(|(tag, name)| {
                let device_id = DeviceId([*tag; 16]);
                mls_identity::sign_device_certificate(
                    mls_identity::DeviceCertificateBody {
                        user_id,
                        account_id,
                        authority_public_key,
                        device_id,
                        device_name: (*name).to_string(),
                        mls_client_id: mls_identity::mls_client_id(
                            &server_id, account_id, device_id,
                        )
                        .unwrap(),
                        mls_signature_public_key: vec![*tag; 32],
                    },
                    &authority_seed,
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        active_devices.sort_by_key(|certificate| certificate.body.device_id);
        mls_identity::sign_device_roster(
            mls_identity::DeviceRosterBody {
                user_id,
                account_id,
                authority_public_key,
                revision,
                active_devices,
            },
            &authority_seed,
        )
        .unwrap()
    }

    fn authorize_test_mls_device(
        server: &mut Server,
        session_id: SessionId,
        user_id: UserId,
        tag: u8,
    ) {
        let roster = test_mls_roster(server, user_id, [tag; 32], &[(tag, "test")], 1);
        put_test_roster(server, user_id, roster.clone());
        let certificate = &roster.body.active_devices[0].body;
        server.sessions.get_mut(&session_id).unwrap().mls_device = Some(BoundMlsDevice {
            device_id: certificate.device_id,
            roster: mls_identity::roster_checkpoint(&roster),
            client_id: certificate.mls_client_id.clone(),
        });
    }

    #[test]
    fn queued_duplicate_room_creation_reconciles_loser_without_disconnect() {
        let mut server = test_server();
        let server_id: [u8; 32] = server
            .server_key_pair
            .public_key()
            .as_ref()
            .try_into()
            .unwrap();
        let client_dir = tempfile::tempdir().unwrap();
        let (alice, _) = chatt_mls::LocalInstallation::open_or_create(
            &client_dir.path().join("alice"),
            server_id,
            UserId(1),
            "alice",
        )
        .unwrap();
        let (bob, _) = chatt_mls::LocalInstallation::open_or_create(
            &client_dir.path().join("bob"),
            server_id,
            UserId(2),
            "bob",
        )
        .unwrap();
        alice.install_roster(&bob.bootstrap.own_roster).unwrap();
        bob.install_roster(&alice.bootstrap.own_roster).unwrap();

        let mut service = MlsService::new(server_id.to_vec());
        service
            .put_roster(
                UserId(1),
                None,
                alice.bootstrap.own_roster.clone(),
                None,
            )
            .unwrap();
        service
            .put_roster(
                UserId(2),
                None,
                bob.bootstrap.own_roster.clone(),
                None,
            )
            .unwrap();
        let published = bob
            .client
            .generate_key_packages(bob.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        service
            .publish_key_packages(UserId(2), bob.bootstrap.device_id, vec![published])
            .unwrap();
        let package = service
            .take_key_package(bob.bootstrap.device_id)
            .unwrap()
            .unwrap();
        server.mls = service.in_memory_view().unwrap();
        server.mls_worker = MlsWorker::spawn(service, Arc::clone(&server.mls_events));

        let room_id = RoomId(50);
        let mut accounts = vec![alice.bootstrap.account_id, bob.bootstrap.account_id];
        accounts.sort_unstable();
        let descriptor = rpc::mls::EncryptedRoomDescriptor::new(
            room_id,
            alice.bootstrap.account_id,
            accounts,
            10,
        )
        .unwrap();
        let bundle = alice
            .client
            .create_room(&descriptor, &[(bob.bootstrap.device_id, package)])
            .unwrap();
        let mut checkpoints = vec![
            mls_identity::roster_checkpoint(&alice.bootstrap.own_roster),
            mls_identity::roster_checkpoint(&bob.bootstrap.own_roster),
        ];
        checkpoints.sort_by_key(|checkpoint| checkpoint.account_id);

        let winner = Token(11);
        let loser = Token(22);
        let mut winner_peer = live_user(&mut server, winner, SessionId(1), UserId(1));
        let mut loser_peer = live_user(&mut server, loser, SessionId(2), UserId(1));
        server.pending_mls.insert(winner);
        server.pending_mls.insert(loser);
        for token in [winner, loser] {
            assert!(server.mls_worker.enqueue_typed(MlsWriteRequest::CreateRoom {
                token,
                creator: descriptor.creator,
                creator_client_id: alice
                    .bootstrap
                    .device_certificate
                    .body
                    .mls_client_id
                    .clone(),
                descriptor: descriptor.clone(),
                checkpoints: checkpoints.clone(),
                bundle: bundle.clone(),
                welcome_devices: Vec::new(),
            }));
        }
        finish_mls_for_token(&mut server, winner);
        finish_mls_for_token(&mut server, loser);

        assert!(server.clients.contains_key(&winner));
        assert!(server.clients.contains_key(&loser));
        assert!(matches!(
            read_until(&mut server, &mut winner_peer, |control| matches!(
                control,
                ServerControl::EncryptedRoomCreated { room_id: observed, .. }
                    if *observed == room_id
            )),
            ServerControl::EncryptedRoomCreated { .. }
        ));
        let reconciled = read_until(&mut server, &mut loser_peer, |control| matches!(
            control,
            ServerControl::GroupInfo {
                room_id: observed,
                descriptor: Some(_),
                ..
            } if *observed == room_id
        ));
        let ServerControl::GroupInfo {
            descriptor: Some(canonical),
            epoch,
            group_info,
            ..
        } = reconciled
        else {
            unreachable!()
        };
        assert_eq!(canonical, descriptor);
        assert_eq!(epoch, 1);
        assert!(!group_info.is_empty());
    }

    #[test]
    fn device_link_redemption_atomically_installs_mls_roster_packages_and_bearer() {
        let mut server = test_server();
        let user_id = UserId(config::FIRST_DYNAMIC_USER_ID);
        server.usernames.claim_dynamic(user_id, "Alice").unwrap();
        let authority_seed = [7; 32];
        let current = test_mls_roster(&server, user_id, authority_seed, &[(7, "first")], 1);
        put_test_roster(&mut server, user_id, current.clone());
        let attempt_id = rpc::ids::PairAttemptId([5; 16]);
        let redemption_secret = "single-use-device-link-secret";
        let bearer = "tct2_test-device-bearer-token-value".to_string();
        let package_dir = tempfile::tempdir().unwrap();
        let server_id: [u8; 32] = server
            .server_key_pair
            .public_key()
            .as_ref()
            .try_into()
            .unwrap();
        let package_source = chatt_mls::LocalInstallation::create_pending_pair(
            package_dir.path(),
            server_id,
            user_id,
            "second",
            authority_seed,
            &current,
            attempt_id,
            redemption_secret,
            &bearer,
        )
        .unwrap();
        let next = package_source.bootstrap.own_roster.clone();
        let device_id = package_source.bootstrap.device_id;
        let package = package_source
            .client
            .generate_key_packages(package_source.bootstrap.device_id, 1)
            .unwrap()
            .remove(0);
        let packages = vec![package];
        server.device_links.insert(
            device_link_secret_hash(redemption_secret),
            DeviceLinkState {
                user_id,
                enrollment_bundle: vec![1],
                current_roster: current.clone(),
                expires_at: Instant::now() + DEVICE_LINK_TTL,
                redemption: DeviceLinkRedemption::Active,
            },
        );
        let mut linking_peer = seed_session_client(&mut server, Token(33));
        server
            .redeem_device_link(
                Token(33),
                redemption_secret,
                attempt_id,
                mls_identity::roster_checkpoint(&current),
                next.clone(),
                packages.clone(),
                bearer.clone(),
                false,
                0,
            )
            .unwrap();
        finish_mls_for_token(&mut server, Token(33));
        assert_eq!(server.mls.roster(user_id), Some(&next));
        assert!(server.mls.authenticate_credential(&bearer).is_some());
        assert_eq!(server.mls.key_package_count(device_id), 1);
        assert!(matches!(
            read_until(&mut server, &mut linking_peer, |control| matches!(
                control,
                ServerControl::DeviceLinked { .. }
            )),
            ServerControl::DeviceLinked { .. }
        ));

        let mut retry_peer = seed_session_client(&mut server, Token(44));
        server
            .redeem_device_link(
                Token(44),
                redemption_secret,
                attempt_id,
                mls_identity::roster_checkpoint(&current),
                next,
                packages,
                bearer,
                false,
                0,
            )
            .unwrap();
        // Retry redemption is cache-only and completes inline.
        assert!(matches!(
            read_until(&mut server, &mut retry_peer, |control| matches!(
                control,
                ServerControl::DeviceLinked { .. }
            )),
            ServerControl::DeviceLinked { .. }
        ));
    }

    #[test]
    fn creating_device_link_replaces_previous_link_for_account() {
        let mut server = test_server();
        let user_id = UserId(config::FIRST_DYNAMIC_USER_ID);
        let session_id = SessionId(1);
        let mut peer = live_user(&mut server, Token(11), session_id, user_id);
        authorize_test_mls_device(&mut server, session_id, user_id, 7);

        server
            .create_device_link(session_id, vec![1; 32], vec![1])
            .unwrap();
        assert!(matches!(
            read_plaintext_server_control(&mut server, &mut peer),
            ServerControl::DeviceLinkCreated { .. }
        ));
        server
            .create_device_link(session_id, vec![2; 32], vec![2])
            .unwrap();
        assert!(matches!(
            read_plaintext_server_control(&mut server, &mut peer),
            ServerControl::DeviceLinkCreated { .. }
        ));

        assert_eq!(server.device_links.len(), 1);
        assert!(!server.device_links.contains_key(&vec![1; 32]));
        assert_eq!(
            server
                .device_links
                .get(&vec![2; 32])
                .map(|link| link.enrollment_bundle.as_slice()),
            Some(&[2][..])
        );
    }

    #[test]
    fn explicit_device_link_cancel_invalidates_only_an_active_link() {
        let mut server = test_server();
        let user_id = UserId(config::FIRST_DYNAMIC_USER_ID);
        let session_id = SessionId(1);
        let mut peer = live_user(&mut server, Token(11), session_id, user_id);
        authorize_test_mls_device(&mut server, session_id, user_id, 7);
        let secret_hash = vec![9; 32];

        server
            .create_device_link(session_id, secret_hash.clone(), vec![1])
            .unwrap();
        assert!(matches!(
            read_plaintext_server_control(&mut server, &mut peer),
            ServerControl::DeviceLinkCreated { .. }
        ));
        server
            .cancel_device_link(session_id, secret_hash.clone())
            .unwrap();

        assert!(!server.device_links.contains_key(&secret_hash));
        assert_eq!(
            read_plaintext_server_control(&mut server, &mut peer),
            ServerControl::DeviceLinkCanceled {
                redemption_secret_hash: secret_hash,
            }
        );
    }

    #[test]
    fn history_fetch_paginates_backwards() {
        let mut config = ServerConfig::default();
        config.network.tcp_addr = "127.0.0.1:0".parse().unwrap();
        config.network.udp_addr = Some("127.0.0.1:0".parse().unwrap());
        config.network.p2p_enabled = false;
        config.rooms[0].persistence = config::RoomPersistenceConfig::Memory;
        config.rooms[0].memory_limit = Some(100);
        let mut server = Server::bind(config).expect("test server");
        let session = SessionId(1);
        let mut peer = live_user(&mut server, Token(11), session, UserId(1));
        for index in 0..9 {
            server
                .send_chat(session, RoomId(1), format!("message {index}"))
                .unwrap();
        }

        server.fetch_history(session, RoomId(1), Some(MessageId(6)), 4);

        let chunk = read_until_history_chunk(&mut server, &mut peer);
        assert_eq!(chunk.room_id, RoomId(1));
        assert_eq!(chunk.before, Some(MessageId(6)));
        assert!(chunk.complete);
        let ids: Vec<u64> = chunk
            .messages
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        assert_eq!(ids, vec![2, 3, 4, 5]);
        assert!(!chunk.at_start);

        server.fetch_history(session, RoomId(1), Some(MessageId(2)), 4);
        let chunk = read_until_history_chunk(&mut server, &mut peer);
        assert_eq!(chunk.before, Some(MessageId(2)));
        assert!(chunk.complete);
        assert_eq!(chunk.messages.len(), 1);
        assert!(chunk.at_start);
    }

    #[test]
    fn history_fetch_streams_page_over_bounded_chunks() {
        let mut config = ServerConfig::default();
        config.network.tcp_addr = "127.0.0.1:0".parse().unwrap();
        config.network.udp_addr = Some("127.0.0.1:0".parse().unwrap());
        config.network.p2p_enabled = false;
        config.rooms[0].persistence = config::RoomPersistenceConfig::Memory;
        config.rooms[0].memory_limit = Some(256);
        let mut server = Server::bind(config).expect("test server");
        let session = SessionId(1);
        server
            .sessions
            .insert(session, test_session(UserId(1), Token(11), None));
        let body = "x".repeat(rpc::control::MAX_CHAT_BODY_BYTES);
        for _ in 0..80 {
            server
                .send_chat(session, RoomId(1), body.clone())
                .unwrap();
        }
        let mut peer = live_user(&mut server, Token(11), session, UserId(1));

        server.fetch_history(
            session,
            RoomId(1),
            None,
            rpc::control::MAX_HISTORY_FETCH_MESSAGES,
        );

        let mut chunk_count = 0usize;
        let mut message_count = 0usize;
        let mut completed = false;
        let mut final_at_start = false;
        while !completed {
            let payload =
                read_plaintext_server_payload_while_flushing(&mut server, Token(11), &mut peer);
            assert!(payload.len() <= rpc::control::MAX_CONTROL_PAYLOAD_BYTES);
            let chunk = decode_history_chunk(&payload);
            assert_eq!(chunk.room_id, RoomId(1));
            if !chunk.complete {
                assert!(!chunk.at_start, "only the final chunk carries at_start");
            }
            message_count += chunk.messages.len();
            chunk_count += 1;
            completed = chunk.complete;
            final_at_start = chunk.at_start;
        }
        assert!(chunk_count > 1, "large history response should be chunked");
        assert_eq!(message_count, 80);
        assert!(final_at_start);
    }

    /// Reads frames until a history chunk arrives, draining the disk reader's
    /// replies between attempts since no event loop is running.
    fn pump_history_chunk(
        server: &mut Server,
        peer: &mut std::net::TcpStream,
    ) -> history::HistoryChunk {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            drive_scheduled_pass(server);
            peer.set_read_timeout(Some(Duration::from_millis(20)))
                .expect("set read timeout");
            let mut len = [0u8; 4];
            match peer.peek(&mut len) {
                Ok(4) => {
                    let payload = read_plaintext_server_payload(server, peer);
                    if let Some(chunk) =
                        history::decode_chunk(&payload).expect("decode history chunk")
                    {
                        return chunk;
                    }
                    let _ = rpc::control::decode_server_control(&payload)
                        .expect("decode server control");
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("peek frame length: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "history chunk was not received in time"
            );
        }
    }

    #[test]
    fn history_fetch_pages_from_disk_below_resident_window() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-server-disk-paging-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut config = ServerConfig::default();
        config.network.tcp_addr = "127.0.0.1:0".parse().unwrap();
        config.network.udp_addr = Some("127.0.0.1:0".parse().unwrap());
        config.network.p2p_enabled = false;
        config.rooms[0].persistence = config::RoomPersistenceConfig::Durable;
        config.storage.data_dir = Some(dir.display().to_string());
        let mut server = Server::bind(config).expect("test server");
        server.store.set_tuning(room_store::StoreTuning {
            max_active_log_bytes: 512,
            max_resident_messages: 6,
        });
        let session = SessionId(1);
        let mut peer = live_user(&mut server, Token(11), session, UserId(1));
        for index in 0..30 {
            server
                .send_chat(session, RoomId(1), format!("message {index}"))
                .unwrap();
        }

        let mut collected: Vec<u64> = Vec::new();
        let mut before: Option<MessageId> = None;
        let mut at_start = false;
        for _ in 0..30 {
            server.fetch_history(session, RoomId(1), before, 4);
            let mut page: Vec<u64> = Vec::new();
            loop {
                let chunk = pump_history_chunk(&mut server, &mut peer);
                assert_eq!(chunk.room_id, RoomId(1));
                let ids: Vec<u64> = chunk
                    .messages
                    .iter()
                    .map(|message| message.message_id.0)
                    .collect();
                page.splice(0..0, ids);
                if chunk.complete {
                    at_start = chunk.at_start;
                    break;
                }
            }
            assert!(
                !page.is_empty() || at_start,
                "an empty page must only terminate paging at the start"
            );
            before = page.first().copied().map(MessageId).or(before);
            page.append(&mut collected);
            collected = page;
            if at_start {
                break;
            }
        }
        assert!(at_start, "paging must reach the durable start");
        let expected: Vec<u64> = (1..=30).collect();
        assert_eq!(
            collected, expected,
            "pages must cover every durable message exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn voice_in_room_a_persists_while_chatting_in_b() {
        let mut server = test_server();
        let room_a = RoomId(1);
        let room_b = RoomId(2);
        server.rooms.insert(room_b, test_room(room_b, &[]));
        let talker = SessionId(1);
        let listener = SessionId(2);
        let mut talker_peer = live_user(&mut server, Token(11), talker, UserId(1));
        let _listener_peer = live_user(&mut server, Token(22), listener, UserId(2));

        server.join_voice(talker, room_a);
        server
            .send_chat(talker, room_b, "reading elsewhere".to_string())
            .unwrap();

        let session = server.sessions.get(&talker).expect("session exists");
        assert_eq!(session.voice_room, Some(room_a));
        assert!(session.active_stream.is_some());
        assert_eq!(server.voice_member_sessions(room_a), vec![talker]);
        let control = read_until(&mut server, &mut talker_peer, |control| {
            matches!(control, ServerControl::Chat { .. })
        });
        let ServerControl::Chat { message } = control else {
            unreachable!();
        };
        assert_eq!(message.room_id, room_b);
    }

    #[test]
    fn join_voice_moves_the_call_between_rooms() {
        let mut server = test_server();
        let room_a = RoomId(1);
        let room_b = RoomId(2);
        server.rooms.insert(room_b, test_room(room_b, &[]));
        let talker = SessionId(1);
        let _talker_peer = live_user(&mut server, Token(11), talker, UserId(1));

        server.join_voice(talker, room_a);
        server.join_voice(talker, room_b);

        let session = server.sessions.get(&talker).expect("session exists");
        assert_eq!(session.voice_room, Some(room_b));
        assert!(server.voice_member_sessions(room_a).is_empty());
        assert_eq!(server.voice_member_sessions(room_b), vec![talker]);
    }

    #[test]
    fn join_voice_confirms_self_before_replaying_existing_members() {
        let mut server = test_server();
        let room_id = RoomId(1);
        let existing = SessionId(1);
        let joining = SessionId(2);
        let _existing_peer = live_user(&mut server, Token(11), existing, UserId(1));
        let mut joining_peer = live_user(&mut server, Token(22), joining, UserId(2));
        server.sessions.get_mut(&existing).unwrap().voice_room = Some(room_id);
        server.ensure_voice_stream(existing, room_id).unwrap();

        server.join_voice(joining, room_id);

        let ServerControl::VoiceStarted { user_id, .. } =
            read_plaintext_server_control(&mut server, &mut joining_peer)
        else {
            panic!("voice confirmation must be the first control");
        };
        assert_eq!(user_id, UserId(2));
        let ServerControl::VoiceStarted { user_id, .. } =
            read_plaintext_server_control(&mut server, &mut joining_peer)
        else {
            panic!("existing voice member must follow local confirmation");
        };
        assert_eq!(user_id, UserId(1));
    }

    #[test]
    fn join_voice_broadcasts_joiner_stored_status() {
        fn read_voice_status(
            server: &mut Server,
            peer: &mut std::net::TcpStream,
        ) -> (UserId, control::ParticipantVoiceStatus) {
            let control = read_until(server, peer, |control| {
                matches!(control, ServerControl::VoiceStatus { .. })
            });
            let ServerControl::VoiceStatus {
                user_id, status, ..
            } = control
            else {
                unreachable!();
            };
            (user_id, status)
        }

        let mut server = test_server();
        let room_id = RoomId(1);
        let joiner = SessionId(1);
        let observer = SessionId(2);
        let _joiner_peer = live_user(&mut server, Token(11), joiner, UserId(1));
        let mut observer_peer = live_user(&mut server, Token(22), observer, UserId(2));
        let muted = control::ParticipantVoiceStatus {
            muted: true,
            deafened: false,
        };

        server.join_voice(joiner, room_id);
        server.leave_voice(joiner, None);
        server.set_voice_status(joiner, muted);
        server.join_voice(joiner, room_id);
        server.leave_voice(joiner, None);
        server.set_voice_status(joiner, control::ParticipantVoiceStatus::default());
        server.join_voice(joiner, room_id);

        let mut statuses = Vec::new();
        for _ in 0..3 {
            statuses.push(read_voice_status(&mut server, &mut observer_peer));
        }
        assert_eq!(
            statuses,
            vec![
                (UserId(1), control::ParticipantVoiceStatus::default()),
                (UserId(1), muted),
                (UserId(1), control::ParticipantVoiceStatus::default()),
            ]
        );
    }

    #[test]
    fn screen_share_is_announced_only_to_voice_members() {
        let mut server = test_server();
        let room_id = RoomId(1);
        let owner = SessionId(1);
        let viewer = SessionId(2);
        let idler = SessionId(3);
        let _owner_peer = live_user(&mut server, Token(11), owner, UserId(1));
        let mut viewer_peer = live_user(&mut server, Token(22), viewer, UserId(2));
        let mut idler_peer = live_user(&mut server, Token(33), idler, UserId(3));
        for session_id in [owner, viewer] {
            server.sessions.get_mut(&session_id).unwrap().voice_room = Some(room_id);
            server.ensure_voice_stream(session_id, room_id).unwrap();
        }

        server
            .start_share(
                owner,
                room_id,
                "avc1.42c01f".to_string(),
                1280,
                720,
                true,
                Vec::new(),
            )
            .unwrap();

        let available = read_until(&mut server, &mut viewer_peer, |control| {
            matches!(control, ServerControl::ShareAvailable { .. })
        });
        assert!(matches!(
            available,
            ServerControl::ShareAvailable {
                room_id: RoomId(1),
                ..
            }
        ));
        assert_no_control(&mut idler_peer);
    }

    #[test]
    fn share_rejection_answers_error_without_disconnect() {
        let mut server = test_server();
        let session = SessionId(1);
        let token = Token(11);
        let mut peer = live_user(&mut server, token, session, UserId(1));
        server.clients.get_mut(&token).unwrap().session_id = Some(session);

        let result = server.handle_control(
            token,
            ClientControl::StartShare {
                room_id: RoomId(1),
                codec: "avc1.42c01f".to_string(),
                coded_width: 1280,
                coded_height: 720,
                annexb: true,
                extradata: Vec::new(),
            },
        );

        assert_eq!(
            result,
            Ok(()),
            "rejection must not read as a protocol error"
        );
        assert!(
            server.clients.contains_key(&token),
            "the connection must survive the rejection"
        );
        let control = read_until(&mut server, &mut peer, |control| {
            matches!(control, ServerControl::Error { .. })
        });
        let ServerControl::Error { code, .. } = control else {
            unreachable!();
        };
        assert_eq!(code, control::ERROR_REQUEST_REJECTED);
        assert!(server.streams.is_empty());
    }

    #[test]
    fn rejoining_the_active_voice_room_replays_nothing() {
        let mut server = test_server();
        let room_id = RoomId(1);
        let joiner = SessionId(1);
        let owner = SessionId(2);
        let mut joiner_peer = live_user(&mut server, Token(11), joiner, UserId(1));
        let _owner_peer = live_user(&mut server, Token(22), owner, UserId(2));
        server.sessions.get_mut(&owner).unwrap().voice_room = Some(room_id);
        server.ensure_voice_stream(owner, room_id).unwrap();
        server
            .start_share(
                owner,
                room_id,
                "avc1.42c01f".to_string(),
                1280,
                720,
                true,
                Vec::new(),
            )
            .unwrap();

        server.join_voice(joiner, room_id);
        read_until(&mut server, &mut joiner_peer, |control| {
            matches!(control, ServerControl::VoiceStatus { .. })
        });

        server.join_voice(joiner, room_id);

        assert_no_control(&mut joiner_peer);
    }

    #[test]
    fn leaving_voice_ends_the_owners_screen_share() {
        let mut server = test_server();
        let owner = SessionId(1);
        let room_id = RoomId(1);
        let _owner_peer = live_user(&mut server, Token(11), owner, UserId(1));
        server.sessions.get_mut(&owner).unwrap().voice_room = Some(room_id);
        server.ensure_voice_stream(owner, room_id).unwrap();
        server
            .start_share(
                owner,
                room_id,
                "avc1.42c01f".to_string(),
                1280,
                720,
                true,
                Vec::new(),
            )
            .unwrap();
        assert_eq!(server.streams.len(), 1);

        server.leave_voice(owner, None);

        assert!(server.streams.is_empty());
    }

    #[test]
    fn leaving_voice_revokes_session_specific_video_access() {
        let mut server = test_server();
        let room_id = RoomId(1);
        let owner = SessionId(1);
        let viewer = SessionId(2);
        let idler = SessionId(3);
        let _owner_peer = live_user(&mut server, Token(11), owner, UserId(1));
        let _viewer_peer = live_user(&mut server, Token(22), viewer, UserId(2));
        let _idler_peer = live_user(&mut server, Token(33), idler, UserId(3));
        for session_id in [owner, viewer] {
            server.sessions.get_mut(&session_id).unwrap().voice_room = Some(room_id);
            server.ensure_voice_stream(session_id, room_id).unwrap();
        }
        server
            .start_share(
                owner,
                room_id,
                "avc1.42c01f".to_string(),
                1280,
                720,
                true,
                Vec::new(),
            )
            .unwrap();
        let stream_id = *server.streams.keys().next().unwrap();
        assert!(
            server.streams[&stream_id]
                .view_secrets
                .contains_key(&viewer)
        );
        assert!(!server.streams[&stream_id].view_secrets.contains_key(&idler));

        let (mut conn, _video_peer) = test_live_client();
        conn.kind = ConnKind::Video(VideoConn::new());
        server.clients.insert(Token(44), conn);
        let hello = VideoHello {
            version: rpc::PROTOCOL_VERSION,
            session_id: viewer,
            stream_id,
            role: VideoRole::Subscriber,
        };
        server
            .handle_video_hello(Token(44), &video::encode_video_hello(&hello))
            .unwrap();

        let (mut unauthorized, _unauthorized_peer) = test_live_client();
        unauthorized.kind = ConnKind::Video(VideoConn::new());
        server.clients.insert(Token(55), unauthorized);
        let hello = VideoHello {
            session_id: idler,
            ..hello
        };
        assert!(
            server
                .handle_video_hello(Token(55), &video::encode_video_hello(&hello))
                .is_err()
        );

        server.leave_voice(viewer, None);

        assert!(!server.clients.contains_key(&Token(44)));
        assert!(
            !server.streams[&stream_id]
                .view_secrets
                .contains_key(&viewer)
        );
    }

    #[test]
    fn joining_voice_replays_active_shares_with_viewer_secret() {
        let mut server = test_server();
        let room_id = RoomId(1);
        let owner = SessionId(1);
        let viewer = SessionId(2);
        let _owner_peer = live_user(&mut server, Token(11), owner, UserId(1));
        let mut viewer_peer = live_user(&mut server, Token(22), viewer, UserId(2));
        server.sessions.get_mut(&owner).unwrap().voice_room = Some(room_id);
        server.ensure_voice_stream(owner, room_id).unwrap();
        server
            .start_share(
                owner,
                room_id,
                "avc1.42c01f".to_string(),
                1280,
                720,
                true,
                Vec::new(),
            )
            .unwrap();

        server.join_voice(viewer, room_id);

        let available = read_until(&mut server, &mut viewer_peer, |control| {
            matches!(control, ServerControl::ShareAvailable { .. })
        });
        let ServerControl::ShareAvailable {
            view_secret,
            stream_id,
            ..
        } = available
        else {
            unreachable!();
        };
        assert_eq!(view_secret.len(), KEY_LEN);
        assert!(
            server.streams[&stream_id]
                .view_secrets
                .contains_key(&viewer)
        );
    }

    #[test]
    fn leave_voice_stops_stream_and_broadcasts() {
        let mut server = test_server();
        let room_a = RoomId(1);
        let talker = SessionId(1);
        let watcher = SessionId(2);
        let _talker_peer = live_user(&mut server, Token(11), talker, UserId(1));
        let mut watcher_peer = live_user(&mut server, Token(22), watcher, UserId(2));

        server.join_voice(talker, room_a);
        read_until(&mut server, &mut watcher_peer, |control| {
            matches!(control, ServerControl::VoiceStarted { .. })
        });
        server.leave_voice(talker, None);

        let stopped = read_until(&mut server, &mut watcher_peer, |control| {
            matches!(control, ServerControl::VoiceStopped { .. })
        });
        let ServerControl::VoiceStopped {
            room_id, user_id, ..
        } = stopped
        else {
            unreachable!();
        };
        assert_eq!(room_id, room_a);
        assert_eq!(user_id, UserId(1));
        let session = server.sessions.get(&talker).expect("session exists");
        assert_eq!(session.voice_room, None);
        assert!(session.active_stream.is_none());
        assert!(server.voice_member_sessions(room_a).is_empty());
    }

    #[test]
    fn join_voice_answers_failure_for_inaccessible_room() {
        let mut server = test_server();
        let secret = RoomId(3);
        server
            .rooms
            .insert(secret, private_room(secret, &[UserId(1)]));
        let outsider = SessionId(2);
        let mut outsider_peer = live_user(&mut server, Token(22), outsider, UserId(2));

        server.join_voice(outsider, secret);

        let control = read_until(&mut server, &mut outsider_peer, |control| {
            matches!(control, ServerControl::VoiceJoinFailed { .. })
        });
        let ServerControl::VoiceJoinFailed { room_id, .. } = control else {
            unreachable!();
        };
        assert_eq!(room_id, secret);
        let session = server.sessions.get(&outsider).expect("session exists");
        assert_eq!(session.voice_room, None);
    }

    #[test]
    fn rtt_snapshot_covers_voice_room_only() {
        let mut server = test_server();
        let room_a = RoomId(1);
        let talker = SessionId(1);
        let idler = SessionId(2);
        let _talker_peer = live_user(&mut server, Token(11), talker, UserId(1));
        let _idler_peer = live_user(&mut server, Token(22), idler, UserId(2));
        server.join_voice(talker, room_a);
        let now = Instant::now();
        if let Some(session) = server.sessions.get_mut(&talker) {
            session.report_server_rtt(Some(45), now);
        }

        let members = server.room_rtt_snapshot(room_a, now);

        assert_eq!(
            members,
            vec![control::ParticipantServerRtt {
                user_id: UserId(1),
                server_rtt_ms: Some(45),
            }]
        );
    }

    fn ring_frame(is_key: bool) -> VideoRingFrame {
        let mut bytes = Vec::new();
        video::write_video_frame(&mut bytes, 0, is_key, 0, &[0u8; 8]);
        VideoRingFrame {
            data: SharedVideoFrame::copy_from_slice(&bytes),
            is_key,
        }
    }

    fn test_stream(room_id: RoomId, owner: SessionId) -> VideoStream {
        VideoStream {
            room_id,
            owner_session: owner,
            user_id: UserId(1),
            sender_name: "sharer".to_string(),
            publish_secret: [1u8; KEY_LEN],
            view_secrets: HashMap::new(),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            annexb: true,
            extradata: Vec::new(),
            publisher_conn: None,
            subscribers: Vec::new(),
            ring: VecDeque::new(),
            ring_bytes: 0,
        }
    }

    #[test]
    fn fast_start_index_points_at_last_keyframe() {
        let mut ring = VecDeque::new();
        ring.push_back(ring_frame(true));
        ring.push_back(ring_frame(false));
        ring.push_back(ring_frame(true));
        ring.push_back(ring_frame(false));
        let index = fast_start_index(&ring).unwrap();
        assert_eq!(index, 2);
        assert!(
            ring[index].is_key,
            "a late joiner's first frame is a keyframe"
        );
    }

    #[test]
    fn fast_start_index_is_none_without_a_keyframe() {
        let mut ring = VecDeque::new();
        ring.push_back(ring_frame(false));
        ring.push_back(ring_frame(false));
        assert_eq!(fast_start_index(&ring), None);
    }

    #[test]
    fn subscriber_within_limit_trips_above_high_water() {
        assert!(subscriber_within_limit(VIDEO_SUBSCRIBER_HIGH_WATER));
        assert!(!subscriber_within_limit(VIDEO_SUBSCRIBER_HIGH_WATER + 1));
    }

    #[test]
    fn sweep_drops_stale_handshake_but_keeps_active_connection() {
        let mut server = test_server();
        let now = Instant::now();
        server.next_idle_sweep_at = now;

        let (mut stale, _stale_peer) = test_live_client();
        stale.kind = ConnKind::Unidentified;
        stale.state = ConnState::AwaitClientHello;
        stale.created_at = now - HANDSHAKE_TIMEOUT - Duration::from_secs(1);
        server.clients.insert(Token(5), stale);

        let (active, _active_peer) = test_live_client();
        server.clients.insert(Token(6), active);

        server.sweep_idle_connections(now);

        assert!(!server.clients.contains_key(&Token(5)));
        assert!(server.clients.contains_key(&Token(6)));
    }

    #[test]
    fn sweep_drops_ready_connection_idle_past_timeout() {
        let mut server = test_server();
        let now = Instant::now();
        server.next_idle_sweep_at = now;

        let (mut idle, _peer) = test_live_client();
        idle.last_activity = now - IDLE_TIMEOUT - Duration::from_secs(1);
        server.clients.insert(Token(5), idle);

        server.sweep_idle_connections(now);

        assert!(server.clients.is_empty());
    }

    #[test]
    fn overflowed_control_write_buffer_disconnects_client() {
        let mut server = test_server();
        let token = Token(5);
        let (mut conn, _peer) = test_live_client();
        // Twice the high-water mark: the flush inside the queue call moves at
        // most the kernel socket buffer's worth, so the remainder stays over
        // the limit with the peer not reading.
        conn.write_buf
            .tail_mut()
            .extend_from_slice(&vec![0u8; 2 * CONTROL_WRITE_HIGH_WATER]);
        server.clients.insert(token, conn);

        let result = server.queue_control_payload_to_token(token, "test", b"x");

        assert!(result.is_err());
        assert!(server.clients.is_empty());
    }

    #[test]
    fn graceful_close_force_drops_after_flush_deadline() {
        let mut server = test_server();
        let now = Instant::now();

        let (mut expired, _expired_peer) = test_live_client();
        expired
            .write_buf
            .tail_mut()
            .extend_from_slice(b"undelivered");
        expired.close = Close::Draining {
            deadline: now - Duration::from_secs(1),
        };
        server.clients.insert(Token(5), expired);

        let (mut draining, _draining_peer) = test_live_client();
        draining
            .write_buf
            .tail_mut()
            .extend_from_slice(b"undelivered");
        draining.close = Close::Draining {
            deadline: now + Duration::from_secs(60),
        };
        server.clients.insert(Token(6), draining);

        server.flush_disconnects();

        assert!(!server.clients.contains_key(&Token(5)));
        assert!(server.clients.contains_key(&Token(6)));
    }

    #[test]
    fn frames_behind_a_closing_connection_are_not_processed() {
        let mut server = test_server();
        let token = Token(5);
        let (mut conn, mut peer) = test_live_client();
        conn.state = ConnState::AwaitAuth;
        conn.close = Close::Draining {
            deadline: Instant::now() + Duration::from_secs(60),
        };
        server.clients.insert(token, conn);

        let mut pipelined = Vec::new();
        frame::encode_frame(b"never-processed", &mut pipelined).expect("encode frame");
        peer.write_all(&pipelined).expect("write pipelined frame");

        // Nonblocking loopback: retry until the written bytes are readable.
        // Each retry models a readable poll event, which sets `readiness`.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            server
                .clients
                .get_mut(&token)
                .expect("still draining")
                .readiness
                .mark_ready();
            server.read_client(token);
            let buffered = server.clients.get(&token).expect("still draining");
            if buffered.read_buf.len() >= pipelined.len() || Instant::now() > deadline {
                break;
            }
        }

        let client = server.clients.get(&token).expect("still draining");
        assert_eq!(client.read_buf.len(), pipelined.len());
        assert!(client.write_buf.is_empty());
    }

    #[test]
    fn read_budget_stop_keeps_readiness_for_retry() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"x").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::ByteBudget(1),
        )
        .unwrap();

        assert!(outcome.hit_limit);
        assert!(!outcome.disconnected);
        assert!(outcome.bytes_read >= 1);
        assert!(readiness.is_ready());
        assert!(!read_buf.is_empty());
    }

    #[test]
    fn would_block_clears_retained_readiness_after_buffer_consumed() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"x").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::ByteBudget(1),
        )
        .unwrap();
        assert!(outcome.hit_limit);
        assert!(readiness.is_ready());

        read_buf.consume(read_buf.len());
        let outcome = read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::ByteBudget(1),
        )
        .unwrap();

        assert_eq!(outcome, ReadPumpOutcome::default());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn loop_work_queues_reads_and_forces_zero_timeout() {
        let mut work = LoopWork::default();
        assert_eq!(work.poll_timeout(POLL_TIMEOUT), POLL_TIMEOUT);

        work.queue_accept_clients();
        work.queue_accept_clients();
        work.queue_client_read(Token(7));
        work.queue_client_read(Token(7));
        work.queue_client_write(Token(9));
        work.queue_client_write(Token(9));

        assert_eq!(work.poll_timeout(POLL_TIMEOUT), Duration::ZERO);
        assert!(work.take_accept_clients());
        assert!(!work.take_accept_clients());
        assert_eq!(work.queued_client_reads(), vec![Token(7)]);
        assert_eq!(work.client_read_count(), 1);
        assert_eq!(work.pop_client_read(), Some(Token(7)));
        assert_eq!(work.pop_client_read(), None);
        assert_eq!(work.queued_client_writes(), vec![Token(9)]);
        assert_eq!(work.client_write_count(), 1);
        assert_eq!(work.pop_client_write(), Some(Token(9)));
        assert_eq!(work.pop_client_write(), None);
        assert_eq!(work.poll_timeout(POLL_TIMEOUT), POLL_TIMEOUT);
    }

    #[test]
    fn loop_work_caps_socket_batches_and_preserves_fifo_remainder() {
        let mut work = LoopWork::default();
        for index in 0..=CLIENT_READS_PER_PASS {
            work.queue_client_read(Token(FIRST_CLIENT + index));
        }
        for index in 0..=CLIENT_WRITES_PER_PASS {
            work.queue_client_write(Token(FIRST_CLIENT + index));
        }

        assert_eq!(work.client_read_batch_len(), CLIENT_READS_PER_PASS);
        for _ in 0..work.client_read_batch_len() {
            work.pop_client_read().unwrap();
        }
        assert_eq!(
            work.queued_client_reads(),
            vec![Token(FIRST_CLIENT + CLIENT_READS_PER_PASS)]
        );

        assert_eq!(work.client_write_batch_len(), CLIENT_WRITES_PER_PASS);
        for _ in 0..work.client_write_batch_len() {
            work.pop_client_write().unwrap();
        }
        assert_eq!(
            work.queued_client_writes(),
            vec![Token(FIRST_CLIENT + CLIENT_WRITES_PER_PASS)]
        );
        assert_eq!(work.poll_timeout(POLL_TIMEOUT), Duration::ZERO);
    }

    #[test]
    fn control_scheduler_limits_records_per_pass_and_preserves_remainder() {
        let mut server = test_server();
        let token = Token(17);
        let (conn, _peer) = test_live_client();
        server.clients.insert(token, conn);
        for _ in 0..40 {
            server
                .queue_control_payload_to_token(token, "test", b"payload")
                .unwrap();
        }

        server.process_control_sends();

        assert_eq!(server.pending_controls[&token].len(), 8);
        assert!(server.control_send_set.contains(&token));
        assert!(server.loop_work.client_write_count() > 0);
        server.process_control_sends();
        assert!(!server.pending_controls.contains_key(&token));
    }

    #[test]
    fn udp_recv_retries_interrupted_before_datagram_or_drain() {
        let src = "127.0.0.1:12345".parse::<SocketAddr>().unwrap();
        let mut calls = 0;
        let mut buf = [0u8; 8];

        let received = recv_datagram_with(&mut buf, |_| {
            calls += 1;
            match calls {
                1 => Err(io::Error::from_raw_os_error(libc::EINTR)),
                2 => Ok((3, src)),
                _ => unreachable!("receive loop should return after one datagram"),
            }
        })
        .unwrap();

        assert_eq!(received, Some((3, src)));
        assert_eq!(calls, 2);

        let mut calls = 0;
        let drained: Option<(usize, SocketAddr)> = recv_datagram_with(&mut buf, |_| {
            calls += 1;
            match calls {
                1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                2 => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                _ => unreachable!("receive loop should stop at WouldBlock"),
            }
        })
        .unwrap();

        assert_eq!(drained, None);
        assert_eq!(calls, 2);
    }

    #[test]
    fn pipelined_control_frames_process_from_one_read() {
        let mut server = test_server();
        let token = Token(11);
        let session_id = SessionId(1);
        let (mut conn, mut peer) = test_live_client();
        conn.session_id = Some(session_id);
        server.clients.insert(token, conn);
        server
            .sessions
            .insert(session_id, test_session(UserId(9), token, None));

        let mut pipelined = Vec::new();
        for muted in [true, false, true] {
            let control = ClientControl::SetVoiceStatus {
                status: control::ParticipantVoiceStatus {
                    muted,
                    deafened: false,
                },
            };
            let payload =
                rpc::control::encode_client_control(&control).expect("encode client control");
            frame::encode_frame(&payload, &mut pipelined).expect("encode frame");
        }
        peer.write_all(&pipelined).expect("write pipelined frames");

        // Nonblocking loopback: retry until the written bytes are readable.
        // Each retry models a readable poll event, which sets `readiness`.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            server
                .clients
                .get_mut(&token)
                .expect("client still connected")
                .readiness
                .mark_ready();
            server.read_client(token);
            let muted = server
                .sessions
                .get(&session_id)
                .expect("session exists")
                .voice_status
                .muted;
            if muted || Instant::now() > deadline {
                break;
            }
        }

        let session = server.sessions.get(&session_id).expect("session exists");
        assert!(session.voice_status.muted);
        let client = server.clients.get(&token).expect("client still connected");
        assert!(client.read_buf.is_empty());
    }

    #[test]
    fn relay_backpressure_parks_uploader_and_resumes_after_drain() {
        let mut server = test_server();
        let uploader_token = Token(21);
        let uploader_session = SessionId(1);
        let (mut uploader_conn, _uploader_peer) = test_live_client();
        uploader_conn.session_id = Some(uploader_session);
        server.clients.insert(uploader_token, uploader_conn);
        server.sessions.insert(
            uploader_session,
            test_session(UserId(1), uploader_token, None),
        );

        let recipient_token = Token(22);
        let recipient_session = SessionId(2);
        let (mut recipient_conn, _recipient_peer) = test_live_client();
        recipient_conn.session_id = Some(recipient_session);
        server.clients.insert(recipient_token, recipient_conn);
        server.sessions.insert(
            recipient_session,
            test_session(UserId(2), recipient_token, None),
        );

        server.active_uploads.insert(
            (uploader_session, FileTransferId(1)),
            ServerUpload {
                server_transfer_id: FileTransferId(7),
                room_id: RoomId(1),
                encoding: FileContentEncoding::Zstd,
                original_size: 1024,
                wire_received: 0,
                file_name: "clogged.bin".to_string(),
                recipients: HashSet::from([recipient_session]),
                declined_notified: false,
            },
        );

        // Below the soft cap the uploader is not parked.
        assert!(!server.relay_clogged_for_reader(uploader_token));

        // Over the cap: a readable uploader stays parked, its socket unread.
        server
            .clients
            .get_mut(&recipient_token)
            .expect("recipient connected")
            .write_buf
            .tail_mut()
            .resize(FILE_RELAY_WRITE_SOFT_CAP + 1, 0);
        assert!(server.relay_clogged_for_reader(uploader_token));
        server
            .clients
            .get_mut(&uploader_token)
            .expect("uploader connected")
            .readiness
            .mark_ready();
        server.read_client(uploader_token);
        let uploader = server.clients.get(&uploader_token).expect("still parked");
        assert!(uploader.read_buf.is_empty());
        assert!(uploader.readiness.is_ready());
        server.requeue_unclogged_uploaders();
        assert!(server.loop_work.queued_client_reads().is_empty());

        // Draining the recipient below the cap resumes the uploader.
        let queued = server
            .clients
            .get(&recipient_token)
            .expect("recipient connected")
            .write_buf
            .len();
        server
            .clients
            .get_mut(&recipient_token)
            .expect("recipient connected")
            .write_buf
            .consume(queued);
        server.requeue_unclogged_uploaders();
        assert_eq!(server.loop_work.queued_client_reads(), vec![uploader_token]);
    }

    #[test]
    fn partial_writes_reassemble_byte_for_byte() {
        let mut server = test_server();
        let token = Token(5);
        let (mut conn, mut peer) = test_live_client();
        let payload: Vec<u8> = (0..1024 * 1024u32).map(|index| index as u8).collect();
        conn.write_buf.tail_mut().extend_from_slice(&payload);
        server.clients.insert(token, conn);
        peer.set_read_timeout(Some(Duration::from_millis(200)))
            .expect("set read timeout");

        let mut received = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        while received.len() < payload.len() {
            server.write_client(token);
            match peer.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => received.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("peer read failed: {error}"),
            }
        }

        assert_eq!(received, payload);
        assert!(
            server
                .clients
                .get(&token)
                .expect("client still connected")
                .write_buf
                .is_empty()
        );
    }

    #[test]
    fn publish_video_frame_resets_ring_on_keyframe() {
        let mut server = test_server();
        let stream_id = StreamId(1);
        server
            .streams
            .insert(stream_id, test_stream(RoomId(1), SessionId(1)));

        let delta = ring_frame(false).data;
        let key = ring_frame(true).data;
        server.publish_video_frame(stream_id, key.clone(), true);
        server.publish_video_frame(stream_id, delta.clone(), false);
        server.publish_video_frame(stream_id, delta.clone(), false);
        // A new keyframe drops the earlier GOP so a fresh viewer starts clean.
        server.publish_video_frame(stream_id, key.clone(), true);

        let stream = server.streams.get(&stream_id).unwrap();
        assert_eq!(stream.ring.len(), 1);
        assert!(stream.ring[0].is_key);
        assert_eq!(stream.ring_bytes, key.retained_bytes());
    }

    #[test]
    fn streaming_publisher_reader_retains_frames_zero_copy() {
        let mut server = test_server();
        let stream_id = StreamId(1);
        server
            .streams
            .insert(stream_id, test_stream(RoomId(1), SessionId(1)));

        let secret = [9u8; KEY_LEN];
        let (client_send, client_recv) = derive_video_keys(&secret, VideoKeyRole::Client);
        let mut client_cipher = TransportCipher::new(client_send, client_recv);
        let (server_send, server_recv) = derive_video_keys(&secret, VideoKeyRole::Server);
        let server_cipher = TransportCipher::new(server_send, server_recv);

        let token = Token(5);
        let (mut conn, mut peer) = test_live_client();
        let mut video = VideoConn::new();
        video.phase = VideoPhase::Streaming;
        video.session_id = Some(SessionId(1));
        video.stream_id = Some(stream_id);
        video.role = Some(VideoRole::Publisher);
        video.record = Some(RecordProtection::Aead(server_cipher));
        conn.kind = ConnKind::Video(video);

        let seal_record = |cipher: &mut TransportCipher, is_key: bool, body: &[u8]| {
            let mut inner = Vec::new();
            video::write_video_frame(&mut inner, 0, is_key, stream_id.0, body);
            let sealed = cipher.seal_next(CHANNEL_VIDEO, &inner).expect("seal");
            let mut record = Vec::new();
            video::write_record(&mut record, &sealed).expect("record");
            (inner, record)
        };

        // The first record lands in the generic receive buffer before the
        // reader attaches, becoming the residual the reader must drain.
        let (key_inner, key_record) = seal_record(&mut client_cipher, true, &[1u8; 8]);
        peer.write_all(&key_record).expect("write key record");
        let deadline = Instant::now() + Duration::from_secs(1);
        while conn.read_buf.len() < key_record.len() {
            assert!(Instant::now() < deadline, "residual never arrived");
            conn.readiness.mark_ready();
            read_into_buffer(
                &conn.socket,
                &mut conn.read_buf,
                &mut conn.readiness,
                READ_BUDGET_BYTES,
                ReadLimit::ByteBudget(READ_BUDGET_BYTES),
            )
            .expect("fill residual");
        }
        let ConnKind::Video(video) = &mut conn.kind else {
            unreachable!()
        };
        video.reader = Some(VideoRecordReader::new());
        server.clients.insert(token, conn);

        // The second record flows through the exact per-record reader.
        let (delta_inner, delta_record) = seal_record(&mut client_cipher, false, &[2u8; 16]);
        peer.write_all(&delta_record).expect("write delta record");
        let deadline = Instant::now() + Duration::from_secs(1);
        while server.streams.get(&stream_id).expect("stream").ring.len() < 2 {
            assert!(Instant::now() < deadline, "frames never published");
            server
                .clients
                .get_mut(&token)
                .expect("publisher connected")
                .readiness
                .mark_ready();
            server.read_video_conn(token);
        }

        let stream = server.streams.get(&stream_id).expect("stream");
        assert!(stream.ring[0].is_key);
        assert!(!stream.ring[1].is_key);
        assert_eq!(stream.ring[0].data.as_slice(), key_inner);
        assert_eq!(stream.ring[1].data.as_slice(), delta_inner);
        // Both frames retain exactly their sealed record allocation: the
        // plaintext plus transport header and tag, nothing more.
        for (frame, inner) in [
            (&stream.ring[0], &key_inner),
            (&stream.ring[1], &delta_inner),
        ] {
            assert_eq!(
                frame.data.retained_bytes(),
                inner.len() + TRANSPORT_HEADER_LEN + TAG_LEN
            );
        }
        assert_eq!(
            stream.ring_bytes,
            stream.ring[0].data.retained_bytes() + stream.ring[1].data.retained_bytes()
        );
    }

    #[test]
    fn external_streaming_publisher_reader_uses_clear_record_offset() {
        let stream_id = StreamId(1);
        let (mut conn, _peer) = test_live_client();
        let mut video = VideoConn::new();
        video.phase = VideoPhase::Streaming;
        video.session_id = Some(SessionId(1));
        video.stream_id = Some(stream_id);
        video.role = Some(VideoRole::Publisher);
        video.record = Some(RecordProtection::clear());
        video.reader = Some(VideoRecordReader::new());
        conn.kind = ConnKind::Video(video);

        let mut inner = Vec::new();
        video::write_video_frame(&mut inner, 0, true, stream_id.0, &[1u8; 8]);
        let mut wire = Vec::new();
        video::write_record(&mut wire, &inner).expect("record");
        let ConnKind::Video(video) = &mut conn.kind else {
            unreachable!()
        };
        let reader = video.reader.as_mut().expect("reader");
        let mut offset = 0;
        while offset < wire.len() {
            let taken = reader.accept(&wire[offset..]).expect("accept");
            assert!(taken > 0 || reader.record_ready());
            offset += taken;
        }

        let step = video_conn_step(&mut conn)
            .expect("step")
            .expect("published frame");
        let VideoStep::Publish { frame, is_key, .. } = step else {
            panic!("expected published frame");
        };
        assert!(is_key);
        assert_eq!(frame.as_slice(), inner);
        assert_eq!(frame.retained_bytes(), inner.len());
    }

    #[test]
    fn server_bind_normalizes_external_link_p2p_off() {
        let mut config = test_server_config();
        config.security.transport_mode = crate::config::TransportModeConfig::ExternalSecureLink;
        config.network.p2p_enabled = true;
        config.network.udp_probe_addr = Some("127.0.0.1:0".parse().unwrap());

        let server = Server::bind(config).expect("test server");

        assert!(!server.config.network.p2p_enabled);
    }

    #[test]
    fn ring_accounting_charges_retained_bytes_not_visible_bytes() {
        let mut server = test_server();
        let stream_id = StreamId(1);
        server
            .streams
            .insert(stream_id, test_stream(RoomId(1), SessionId(1)));

        let mut record = Vec::with_capacity(TRANSPORT_HEADER_LEN + 24 + TAG_LEN);
        record.extend_from_slice(&[0u8; TRANSPORT_HEADER_LEN]);
        video::write_video_frame(&mut record, 0, true, 0, &[0u8; 7]);
        record.extend_from_slice(&[0u8; TAG_LEN]);
        let key = SharedVideoFrame::from_record(record, TRANSPORT_HEADER_LEN, 24);
        assert_eq!(key.len(), 24);
        assert!(key.retained_bytes() > key.len());

        server.publish_video_frame(stream_id, key.clone(), true);
        let delta = ring_frame(false).data;
        server.publish_video_frame(stream_id, delta.clone(), false);

        let stream = server.streams.get(&stream_id).unwrap();
        assert_eq!(
            stream.ring_bytes,
            key.retained_bytes() + delta.retained_bytes()
        );
    }

    #[test]
    fn split_extension_preserves_regular_extension() {
        assert_eq!(split_extension("report.pdf"), ("report", ".pdf"));
        assert_eq!(split_extension("archive.tar.zst"), ("archive.tar", ".zst"));
        assert_eq!(split_extension(".env"), (".env", ""));
    }

    #[test]
    fn sanitize_file_name_removes_paths_and_controls() {
        assert_eq!(sanitize_file_name("../unsafe/report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("bad/name?.txt"), "name_.txt");
        assert_eq!(sanitize_file_name("..."), "file");
    }

    #[test]
    fn reserve_unique_file_name_preserves_extension() {
        let mut reserved = HashSet::new();

        assert_eq!(
            reserve_unique_file_name(&mut reserved, "report.pdf"),
            "report.pdf"
        );
        assert_eq!(
            reserve_unique_file_name(&mut reserved, "report.pdf"),
            "report-1.pdf"
        );
        assert_eq!(
            reserve_unique_file_name(&mut reserved, "report.pdf"),
            "report-2.pdf"
        );
    }

    #[test]
    fn file_upload_wire_bounds_depend_on_encoding() {
        let original = 50 * 1024 * 1024;
        assert_eq!(
            max_file_wire_bytes(FileContentEncoding::Identity, original),
            original
        );
        assert_eq!(
            max_file_wire_bytes(FileContentEncoding::Zstd, original),
            original + original / 128 + 64 * 1024
        );
    }

    #[test]
    fn file_upload_completion_validates_identity_but_not_zstd_ratio() {
        assert!(upload_completion_is_valid(
            FileContentEncoding::Identity,
            100,
            100
        ));
        assert!(!upload_completion_is_valid(
            FileContentEncoding::Identity,
            100,
            99
        ));
        assert!(upload_completion_is_valid(
            FileContentEncoding::Zstd,
            100,
            12
        ));
        assert!(!upload_completion_is_valid(
            FileContentEncoding::Zstd,
            100,
            0
        ));
        assert!(upload_completion_is_valid(FileContentEncoding::Zstd, 0, 0));
    }

    #[test]
    fn file_upload_savings_support_expansion_and_empty_files() {
        assert_eq!(wire_savings_percent(100, 25), 75);
        assert_eq!(wire_savings_percent(100, 101), -1);
        assert_eq!(wire_savings_percent(0, 0), 0);
    }

    #[test]
    fn file_recipient_limit_uses_original_size() {
        let mut session = test_session(UserId(1), Token(1), Some(RoomId(1)));
        session.receive_files = true;
        session.file_receive_limit_bytes = 1024;

        assert!(session_accepts_file(&session, 1024));
        assert!(!session_accepts_file(&session, 1025));
        session.receive_files = false;
        assert!(!session_accepts_file(&session, 1));
    }

    #[test]
    fn upload_start_of_uploader_torn_down_mid_accept_persists_no_file_message() {
        // The `UploadFileAccepted` send can tear the uploader down
        // synchronously (a draining connection whose queue flushed, or
        // backpressure overflow). The durable chat message carrying the
        // transfer id must not be appended after that teardown: with the
        // upload never registered, no `FileCanceled` ever follows and every
        // client renders a broken file message forever.
        let mut config = test_server_config();
        config.rooms[0].persistence = config::RoomPersistenceConfig::Memory;
        let mut server = Server::bind(config).expect("test server");
        let room_id = server.default_room;
        let session_id = SessionId(1);
        let token = Token(11);
        let (mut conn, _peer) = test_live_client();
        conn.close = Close::Draining {
            deadline: Instant::now() + Duration::from_secs(60),
        };
        server.clients.insert(token, conn);
        server
            .sessions
            .insert(session_id, test_session(UserId(9), token, None));

        let result = server.start_file_upload(
            session_id,
            room_id,
            FileTransferId(1),
            "report.pdf".to_string(),
            128,
            FileContentEncoding::Identity,
            None,
        );

        assert_eq!(result.unwrap_err(), "uploading session is closing");
        assert!(server.active_uploads.is_empty());
        assert!(server.reserved_file_names.is_empty());
        let dangling = server
            .store
            .recent(room_id, 16)
            .into_iter()
            .filter(|message| message.file_transfer_id.is_some())
            .count();
        assert_eq!(dangling, 0, "no chat message may reference the transfer");
    }

    #[test]
    fn completed_upload_releases_its_reserved_file_name() {
        let mut server = test_server();
        let room_id = server.default_room;
        let session_id = SessionId(1);
        let _peer = live_user(&mut server, Token(11), session_id, UserId(9));

        server
            .start_file_upload(
                session_id,
                room_id,
                FileTransferId(1),
                "report.pdf".to_string(),
                4,
                FileContentEncoding::Identity,
                None,
            )
            .unwrap();
        assert!(server.reserved_file_names.contains("report.pdf"));
        server
            .receive_file_chunk(session_id, FileTransferId(1), 0, vec![0u8; 4])
            .unwrap();
        assert!(
            server.reserved_file_names.contains("report.pdf"),
            "the name stays reserved while chunks stream"
        );
        server
            .complete_file_upload(session_id, FileTransferId(1))
            .unwrap();
        assert!(server.reserved_file_names.is_empty());

        server
            .start_file_upload(
                session_id,
                room_id,
                FileTransferId(2),
                "report.pdf".to_string(),
                4,
                FileContentEncoding::Identity,
                None,
            )
            .unwrap();
        let upload = server
            .active_uploads
            .get(&(session_id, FileTransferId(2)))
            .unwrap();
        assert_eq!(
            upload.file_name, "report.pdf",
            "a released name is reused without a numeric suffix"
        );
    }

    #[test]
    fn canceled_upload_releases_its_reserved_file_name() {
        let mut server = test_server();
        let room_id = server.default_room;
        let session_id = SessionId(1);
        let _peer = live_user(&mut server, Token(11), session_id, UserId(9));

        server
            .start_file_upload(
                session_id,
                room_id,
                FileTransferId(1),
                "report.pdf".to_string(),
                4,
                FileContentEncoding::Identity,
                None,
            )
            .unwrap();
        assert!(server.reserved_file_names.contains("report.pdf"));

        server.cancel_file_upload(session_id, FileTransferId(1), "client canceled".to_string());

        assert!(server.active_uploads.is_empty());
        assert!(server.reserved_file_names.is_empty());
    }

    #[test]
    fn skip_file_download_drops_only_the_skipping_recipient() {
        let mut server = test_server();
        let room_id = server.default_room;
        let uploader = SessionId(1);
        let recipient_a = SessionId(2);
        let recipient_b = SessionId(3);
        let _uploader_peer = live_user(&mut server, Token(11), uploader, UserId(9));
        let _peer_a = live_receiver(&mut server, Token(12), recipient_a, UserId(10));
        let _peer_b = live_receiver(&mut server, Token(13), recipient_b, UserId(11));

        server
            .start_file_upload(
                uploader,
                room_id,
                FileTransferId(1),
                "report.pdf".to_string(),
                8,
                FileContentEncoding::Identity,
                None,
            )
            .unwrap();
        let key = (uploader, FileTransferId(1));
        let server_transfer_id = server.active_uploads[&key].server_transfer_id;
        assert!(
            server.active_uploads[&key]
                .recipients
                .contains(&recipient_a)
        );
        assert!(
            server.active_uploads[&key]
                .recipients
                .contains(&recipient_b)
        );

        server.skip_file_download(recipient_a, server_transfer_id);

        let upload = server
            .active_uploads
            .get(&key)
            .expect("upload continues for the remaining recipients");
        assert!(
            !upload.recipients.contains(&recipient_a),
            "the skipping recipient is dropped from the relay set"
        );
        assert!(
            upload.recipients.contains(&recipient_b),
            "other recipients keep receiving"
        );
        // The upload itself is unaffected and keeps accepting chunks.
        server
            .receive_file_chunk(uploader, FileTransferId(1), 0, vec![0u8; 8])
            .unwrap();
    }

    #[test]
    fn skip_file_download_for_unknown_transfer_is_a_noop() {
        let mut server = test_server();
        let session_id = SessionId(1);
        let _peer = live_user(&mut server, Token(11), session_id, UserId(9));
        // No panic, nothing to drop.
        server.skip_file_download(session_id, FileTransferId(999));
    }

    #[test]
    fn last_recipient_skip_notifies_uploader_and_keeps_upload() {
        let mut server = test_server();
        let room_id = server.default_room;
        let uploader = SessionId(1);
        let recipient = SessionId(2);
        let mut uploader_peer = live_user(&mut server, Token(11), uploader, UserId(9));
        let _recipient_peer = live_receiver(&mut server, Token(12), recipient, UserId(10));

        server
            .start_file_upload(
                uploader,
                room_id,
                FileTransferId(1),
                "report.pdf".to_string(),
                8,
                FileContentEncoding::Identity,
                None,
            )
            .unwrap();
        let key = (uploader, FileTransferId(1));
        let server_transfer_id = server.active_uploads[&key].server_transfer_id;

        server.skip_file_download(recipient, server_transfer_id);

        // The upload stays registered with an empty recipient set: removing it
        // here would turn the uploader's next chunk into an error and disconnect
        // it. The name stays reserved until the uploader's own cancel arrives.
        let upload = server
            .active_uploads
            .get(&key)
            .expect("upload stays registered so the uploader can cancel it");
        assert!(upload.recipients.is_empty());
        assert!(upload.declined_notified);
        assert!(server.reserved_file_names.contains("report.pdf"));

        let declined = read_until(&mut server, &mut uploader_peer, |control| {
            matches!(control, ServerControl::UploadDeclined { .. })
        });
        let ServerControl::UploadDeclined {
            client_transfer_id,
            reason,
        } = declined
        else {
            unreachable!()
        };
        assert_eq!(client_transfer_id, FileTransferId(1));
        assert_eq!(reason, "recipient declined");

        // A chunk arriving before the uploader's cancel is accepted (offset
        // validation still advances) but relayed to nobody.
        server
            .receive_file_chunk(uploader, FileTransferId(1), 0, vec![0u8; 8])
            .unwrap();
        assert_eq!(server.active_uploads[&key].wire_received, 8);
    }

    #[test]
    fn upload_with_no_accepting_recipients_notifies_uploader() {
        let mut server = test_server();
        let room_id = server.default_room;
        let uploader = SessionId(1);
        let bystander = SessionId(2);
        let mut uploader_peer = live_user(&mut server, Token(11), uploader, UserId(9));
        // A room member who does not accept relayed files, so the transfer opens
        // with a non-empty membership but an empty recipient set.
        let _bystander_peer = live_user(&mut server, Token(12), bystander, UserId(10));

        server
            .start_file_upload(
                uploader,
                room_id,
                FileTransferId(1),
                "report.pdf".to_string(),
                8,
                FileContentEncoding::Identity,
                None,
            )
            .unwrap();
        let key = (uploader, FileTransferId(1));
        assert!(server.active_uploads[&key].recipients.is_empty());
        assert!(server.active_uploads[&key].declined_notified);

        let declined = read_until(&mut server, &mut uploader_peer, |control| {
            matches!(control, ServerControl::UploadDeclined { .. })
        });
        assert!(matches!(
            declined,
            ServerControl::UploadDeclined { reason, .. } if reason == "recipient declined"
        ));
    }

    #[test]
    fn upload_into_room_without_other_members_is_not_declined() {
        let mut server = test_server();
        let room_id = server.default_room;
        let uploader = SessionId(1);
        let _uploader_peer = live_user(&mut server, Token(11), uploader, UserId(9));

        server
            .start_file_upload(
                uploader,
                room_id,
                FileTransferId(1),
                "report.pdf".to_string(),
                8,
                FileContentEncoding::Identity,
                None,
            )
            .unwrap();
        let key = (uploader, FileTransferId(1));
        // No other members means nobody to decline; the upload completes as a
        // sender-only save rather than showing a false "recipient declined".
        assert!(!server.active_uploads[&key].declined_notified);
        server
            .receive_file_chunk(uploader, FileTransferId(1), 0, vec![0u8; 8])
            .unwrap();
        server
            .complete_file_upload(uploader, FileTransferId(1))
            .unwrap();
        assert!(server.active_uploads.is_empty());
    }

    #[test]
    fn upload_start_beyond_per_session_cap_is_rejected() {
        let mut server = test_server();
        let room_id = server.default_room;
        let session_id = SessionId(1);
        let _peer = live_user(&mut server, Token(11), session_id, UserId(9));

        for index in 0..MAX_ACTIVE_UPLOADS_PER_SESSION {
            server
                .start_file_upload(
                    session_id,
                    room_id,
                    FileTransferId(index as u64 + 1),
                    format!("file-{index}.bin"),
                    4,
                    FileContentEncoding::Identity,
                    None,
                )
                .unwrap();
        }

        let result = server.start_file_upload(
            session_id,
            room_id,
            FileTransferId(99),
            "file-extra.bin".to_string(),
            4,
            FileContentEncoding::Identity,
            None,
        );

        assert!(result.is_err());
        assert_eq!(server.active_uploads.len(), MAX_ACTIVE_UPLOADS_PER_SESSION);
    }

    #[test]
    fn accessible_recipient_tokens_exclude_the_acting_session() {
        let room_id = RoomId(1);
        let acting_session = SessionId(1);
        let other_session = SessionId(2);
        let mut rooms = HashMap::new();
        rooms.insert(room_id, test_room(room_id, &[]));
        let mut sessions = HashMap::new();
        sessions.insert(acting_session, test_session(UserId(1), Token(3), None));
        sessions.insert(other_session, test_session(UserId(2), Token(4), None));

        let tokens =
            accessible_recipient_tokens(&rooms, &sessions, room_id, Some(acting_session), |_| true);

        assert_eq!(tokens, vec![Token(4)]);
    }

    #[test]
    fn accessible_recipient_tokens_cover_every_session_for_public_rooms() {
        let room_id = RoomId(1);
        let mut rooms = HashMap::new();
        rooms.insert(room_id, test_room(room_id, &[]));
        let mut sessions = HashMap::new();
        sessions.insert(SessionId(1), test_session(UserId(1), Token(3), None));
        sessions.insert(SessionId(2), test_session(UserId(2), Token(4), None));

        let mut tokens = accessible_recipient_tokens(&rooms, &sessions, room_id, None, |_| true);
        tokens.sort_by_key(|token| token.0);

        assert_eq!(tokens, vec![Token(3), Token(4)]);
    }

    #[test]
    fn accessible_recipient_tokens_skip_inactive_tokens() {
        let room_id = RoomId(1);
        let mut rooms = HashMap::new();
        rooms.insert(room_id, test_room(room_id, &[]));
        let mut sessions = HashMap::new();
        sessions.insert(SessionId(1), test_session(UserId(1), Token(3), None));
        sessions.insert(SessionId(2), test_session(UserId(2), Token(4), None));

        let tokens = accessible_recipient_tokens(&rooms, &sessions, room_id, None, |token| {
            token != Token(3)
        });

        assert_eq!(tokens, vec![Token(4)]);
    }

    #[test]
    fn accessible_recipient_tokens_respect_private_membership() {
        let room_id = RoomId(2);
        let mut rooms = HashMap::new();
        rooms.insert(room_id, private_room(room_id, &[UserId(1)]));
        let mut sessions = HashMap::new();
        sessions.insert(SessionId(1), test_session(UserId(1), Token(3), None));
        sessions.insert(SessionId(2), test_session(UserId(2), Token(4), None));

        let tokens = accessible_recipient_tokens(&rooms, &sessions, room_id, None, |_| true);

        assert_eq!(tokens, vec![Token(3)]);
    }

    #[test]
    fn authenticate_resolves_user_by_token_alone() {
        let mut server = test_server();
        let token_secret = "alice-client-generated-token-with-at-least-32-bytes";
        server.users.users = vec![UserConfig {
            id: UserId(7),
            internal_reference: "alice-internal".to_string(),
            username: "Alice".to_string(),
            token_hash: hash_secret(token_secret),
        }];

        // The session is established from the client's handshake transport; the
        // display name matches the stored one, so no config save is attempted.
        let _peer = seed_session_client(&mut server, Token(1));
        let _ = server.authenticate_client(Token(1), "Alice", token_secret, true, 0);

        let session = server
            .sessions
            .values()
            .find(|session| session.tcp_token == Token(1))
            .expect("session established by token");
        assert_eq!(session.username, "Alice");
        assert_eq!(session.user_id, UserId(7));
    }

    #[test]
    fn authenticate_rejects_unknown_token() {
        let mut server = test_server();
        server.users.users = vec![UserConfig {
            id: UserId(7),
            internal_reference: "alice-internal".to_string(),
            username: "Alice".to_string(),
            token_hash: hash_secret("alice-client-generated-token-with-at-least-32-bytes"),
        }];

        let _ = server.authenticate_client(Token(1), "Mallory", "wrong-token", true, 0);

        assert!(server.sessions.is_empty());
    }

    #[test]
    fn authenticate_rejects_invalid_username_instead_of_using_the_stored_one() {
        let mut server = test_server();
        let token_secret = "alice-client-generated-token-with-at-least-32-bytes";
        server.users.users = vec![UserConfig {
            id: UserId(7),
            internal_reference: "alice-internal".to_string(),
            username: "Alice".to_string(),
            token_hash: hash_secret(token_secret),
        }];
        server.usernames = UsernameRegistry::in_memory(&server.users.users);
        let (conn, mut peer) = test_live_client();
        server.clients.insert(Token(1), conn);

        server
            .authenticate_client(Token(1), "bad\u{1}name", token_secret, true, 0)
            .unwrap();

        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut peer) else {
            panic!("expected invalid-username error");
        };
        assert_eq!(code, ERROR_PAIRING_INVALID_REQUEST);
        assert!(server.sessions.is_empty());
    }

    fn open_pair_test_server() -> Server {
        let mut server = test_server();
        server.config.security.public = true;
        server
    }

    fn open_pair_recovery(tag: u64) -> String {
        format!("{OPEN_PAIR_RECOVERY_PREFIX}{tag:064x}")
    }

    fn session_for(server: &Server, token: Token) -> Option<&Session> {
        server
            .sessions
            .values()
            .find(|session| session.tcp_token == token)
    }

    #[test]
    fn open_pair_allocates_dynamic_user_and_issues_token() {
        let mut server = open_pair_test_server();

        let _peer = seed_session_client(&mut server, Token(1));
        let recovery = open_pair_recovery(1);
        let _ = server.open_pair_client(Token(1), "Zoe", "", &recovery, true, 0);

        let session = session_for(&server, Token(1)).expect("session established");
        assert_eq!(
            session.user_id,
            dynamic_user_id_from_recovery_token(
                &server.config.security.server_identity_seed,
                &recovery,
            )
            .unwrap()
        );
        assert_eq!(session.username, "Zoe");
    }

    #[test]
    fn open_pair_recovery_token_reclaims_identity_after_a_lost_response() {
        let mut server = open_pair_test_server();
        let recovery = open_pair_recovery(2);

        let _lost_response = seed_session_client(&mut server, Token(1));
        server
            .open_pair_client(Token(1), "Zoe", "", &recovery, true, 0)
            .unwrap();
        let first_id = session_for(&server, Token(1)).unwrap().user_id;

        let _retry_response = seed_session_client(&mut server, Token(2));
        server
            .open_pair_client(Token(2), "Zoe", "", &recovery, true, 0)
            .unwrap();
        let retry = session_for(&server, Token(2)).expect("retry session established");

        assert_eq!(retry.user_id, first_id);
        assert_eq!(server.usernames.owner_of("Zoe"), Some(first_id));
    }

    #[test]
    fn open_pair_rejects_a_taken_username() {
        let mut server = open_pair_test_server();
        // First user claims "Zoe".
        let _peer = seed_session_client(&mut server, Token(1));
        let first_recovery = open_pair_recovery(3);
        server
            .open_pair_client(Token(1), "Zoe", "", &first_recovery, true, 0)
            .unwrap();
        assert!(session_for(&server, Token(1)).is_some());

        // A different fresh connection cannot take it, even by a case variant.
        let (conn, mut peer) = test_live_client();
        server.clients.insert(Token(2), conn);
        let second_recovery = open_pair_recovery(4);
        server
            .open_pair_client(Token(2), "zoe", "", &second_recovery, true, 0)
            .unwrap();
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut peer) else {
            panic!("expected username-taken error");
        };
        assert_eq!(code, ERROR_USERNAME_TAKEN);
        assert!(session_for(&server, Token(2)).is_none());
    }

    #[test]
    fn open_pair_lets_a_returning_user_keep_and_change_its_username() {
        let mut server = open_pair_test_server();
        let seed = server.config.security.server_identity_seed.clone();
        let id = UserId(config::FIRST_DYNAMIC_USER_ID + 5);
        let existing = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id: id,
                password_epoch: 0,
            },
        )
        .unwrap();
        let _peer = seed_session_client(&mut server, Token(1));
        server
            .open_pair_client(Token(1), "Zoe", "", &existing, true, 0)
            .unwrap();
        assert_eq!(session_for(&server, Token(1)).unwrap().user_id, id);

        // Reconnecting with the same name is fine: the id owns it.
        let _peer2 = seed_session_client(&mut server, Token(2));
        server
            .open_pair_client(Token(2), "Zoe", "", &existing, true, 0)
            .unwrap();
        assert!(session_for(&server, Token(2)).is_some());

        // Renaming to a free name works and frees the old one.
        let _peer3 = seed_session_client(&mut server, Token(3));
        server
            .open_pair_client(Token(3), "Zabette", "", &existing, true, 0)
            .unwrap();
        assert_eq!(session_for(&server, Token(3)).unwrap().username, "Zabette");
        assert_eq!(server.usernames.owner_of("zoe"), None);
        assert_eq!(server.usernames.owner_of("zabette"), Some(id));
    }

    #[test]
    fn open_pair_rejects_a_username_owned_by_an_explicit_user() {
        let mut server = open_pair_test_server();
        server.users.users = vec![UserConfig {
            id: UserId(1),
            internal_reference: "a".to_string(),
            username: "Alice".to_string(),
            token_hash: String::new(),
        }];
        server.usernames = UsernameRegistry::in_memory(&server.users.users);

        let (conn, mut peer) = test_live_client();
        server.clients.insert(Token(2), conn);
        let recovery = open_pair_recovery(5);
        server
            .open_pair_client(Token(2), "alice", "", &recovery, true, 0)
            .unwrap();
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut peer) else {
            panic!("expected username-taken error");
        };
        assert_eq!(code, ERROR_USERNAME_TAKEN);
        assert!(session_for(&server, Token(2)).is_none());
    }

    #[test]
    fn pairing_rejects_a_username_taken_by_another_user() {
        let mut server = test_server();
        server.users.users = vec![UserConfig {
            id: UserId(1),
            internal_reference: "zed".to_string(),
            username: "Zoe".to_string(),
            token_hash: String::new(),
        }];
        server.usernames = UsernameRegistry::in_memory(&server.users.users);

        let code = "pairing-code-secret-1234567890";
        server.invites.insert(
            "dana".to_string(),
            InviteState {
                pairing_code_hash: hash_secret(code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );

        let (conn, mut peer) = test_live_client();
        server.clients.insert(Token(2), conn);
        let new_token = "dana-client-generated-token-with-at-least-32-bytes";
        server
            .pair_client(Token(2), "zoe", code, new_token, true, 0)
            .unwrap();
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut peer) else {
            panic!("expected username-taken error");
        };
        assert_eq!(code, ERROR_USERNAME_TAKEN);
        // The invite survives so the user can retry with another username.
        assert!(!server.invites.is_empty());
    }

    #[test]
    fn open_pair_rejected_when_not_public() {
        let mut server = open_pair_test_server();
        server.config.security.public = false;

        let _ = server.open_pair_client(Token(1), "Zoe", "", "", true, 0);

        assert!(server.sessions.is_empty());
    }

    #[test]
    fn open_pair_enforces_password() {
        let mut server = open_pair_test_server();
        server.config.security.password_hash = Some(hash_secret("hunter2"));

        let _ = server.open_pair_client(Token(1), "Zoe", "", "", true, 0);
        assert!(session_for(&server, Token(1)).is_none());

        let _ = server.open_pair_client(Token(2), "Zoe", "wrong", "", true, 0);
        assert!(session_for(&server, Token(2)).is_none());

        let _peer = seed_session_client(&mut server, Token(3));
        let recovery = open_pair_recovery(6);
        let _ = server.open_pair_client(Token(3), "Zoe", "hunter2", &recovery, true, 0);
        assert!(session_for(&server, Token(3)).is_some());
    }

    #[test]
    fn open_pair_rejects_unregistered_stale_token_without_password() {
        let mut server = open_pair_test_server();
        server.config.security.password_epoch = 7;
        let seed = server.config.security.server_identity_seed.clone();
        let existing = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id: UserId(config::FIRST_DYNAMIC_USER_ID + 5),
                password_epoch: 0,
            },
        )
        .unwrap();

        let _peer = seed_session_client(&mut server, Token(1));
        let _ = server.open_pair_client(Token(1), "Zoe", "", &existing, true, 0);

        assert!(session_for(&server, Token(1)).is_none());
    }

    #[test]
    fn open_pair_reissues_current_epoch_token_on_re_pair() {
        let mut server = open_pair_test_server();
        server.config.security.password_hash = Some(hash_secret("hunter2"));
        server.config.security.password_epoch = 7;
        server.config.network.public_udp_addr = "198.51.100.20:54100".to_string();
        server.config.network.public_udp_probe_addr = Some("198.51.100.20:54101".to_string());
        let seed = server.config.security.server_identity_seed.clone();
        let user_id = UserId(config::FIRST_DYNAMIC_USER_ID + 5);
        let existing = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id,
                password_epoch: 0,
            },
        )
        .unwrap();
        let token = Token(17);
        let (conn, mut peer) = test_live_client();
        server.clients.insert(token, conn);

        server
            .open_pair_client(token, "Zoe", "hunter2", &existing, true, 0)
            .unwrap();

        let response = read_plaintext_server_control(&mut server, &mut peer);
        let ServerControl::OpenPaired {
            token,
            udp_addr,
            udp_probe_addr,
            ..
        } = response
        else {
            panic!("expected OpenPaired");
        };
        let claims = verify_dynamic_token(&seed, &token).unwrap();
        assert_eq!(claims.user_id, user_id);
        assert_eq!(claims.password_epoch, 7);
        assert_eq!(udp_addr, "198.51.100.20:54100");
        assert_eq!(udp_probe_addr.as_deref(), Some("198.51.100.20:54101"));
    }

    #[test]
    fn open_pairing_does_not_announce_or_join_voice() {
        let mut server = open_pair_test_server();
        let token = Token(11);
        let (conn, _peer) = test_live_client();
        server.clients.insert(token, conn);

        let recovery = open_pair_recovery(7);
        server
            .open_pair_client(token, "Zoe", "", &recovery, true, 0)
            .unwrap();

        let session = session_for(&server, token).expect("session established");
        assert_eq!(session.voice_room, None);
        assert!(!session.announced);
        assert!(
            server
                .rooms
                .get(&DEFAULT_ROOM)
                .expect("lobby exists")
                .active_streams
                .is_empty()
        );
    }

    #[test]
    fn authenticate_accepts_registered_device_token() {
        let mut server = open_pair_test_server();
        let seed = server.config.security.server_identity_seed.clone();
        let token = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id: UserId(config::FIRST_DYNAMIC_USER_ID),
                password_epoch: 0,
            },
        )
        .unwrap();
        let _peer = seed_session_client(&mut server, Token(1));
        let _ = server.authenticate_client(Token(1), "Zoe", &token, true, 0);

        let session = session_for(&server, Token(1)).expect("session established");
        assert_eq!(session.user_id, UserId(config::FIRST_DYNAMIC_USER_ID));
    }

    #[test]
    fn authenticate_rejects_dynamic_token_when_public_disabled() {
        let mut server = open_pair_test_server();
        server.config.security.public = false;
        let seed = server.config.security.server_identity_seed.clone();
        let token = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id: UserId(config::FIRST_DYNAMIC_USER_ID),
                password_epoch: 0,
            },
        )
        .unwrap();

        let _ = server.authenticate_client(Token(1), "Zoe", &token, true, 0);

        assert!(server.sessions.is_empty());
    }

    #[test]
    fn authenticate_rejects_stale_dynamic_token_epoch() {
        let mut server = open_pair_test_server();
        server.config.security.password_hash = Some(hash_secret("hunter2"));
        server.config.security.password_epoch = 1;
        let seed = server.config.security.server_identity_seed.clone();
        let token = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id: UserId(config::FIRST_DYNAMIC_USER_ID),
                password_epoch: 0,
            },
        )
        .unwrap();

        let _ = server.authenticate_client(Token(1), "Zoe", &token, true, 0);

        assert!(server.sessions.is_empty());
    }

    #[test]
    fn authenticate_rejects_stale_dynamic_token_epoch_without_password() {
        let mut server = open_pair_test_server();
        server.config.security.password_hash = None;
        server.config.security.password_epoch = 1;
        let seed = server.config.security.server_identity_seed.clone();
        let token = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id: UserId(config::FIRST_DYNAMIC_USER_ID),
                password_epoch: 0,
            },
        )
        .unwrap();

        let _ = server.authenticate_client(Token(1), "Zoe", &token, true, 0);

        assert!(server.sessions.is_empty());
    }

    #[test]
    fn authenticate_rejects_dynamic_token_for_explicit_id_range() {
        let mut server = open_pair_test_server();
        let seed = server.config.security.server_identity_seed.clone();
        let token = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id: UserId(7),
                password_epoch: 0,
            },
        )
        .unwrap();

        let _ = server.authenticate_client(Token(1), "Zoe", &token, true, 0);

        assert!(server.sessions.is_empty());
    }

    #[test]
    fn open_pair_rate_limits_new_allocations_per_ip() {
        let mut server = open_pair_test_server();
        let mut peers = Vec::new();
        for index in 0..OPEN_PAIR_PER_IP_LIMIT {
            let token = Token(100 + index);
            let (conn, peer) = test_live_client();
            server.clients.insert(token, conn);
            // Each allocation is a distinct user, so each needs a distinct name.
            let recovery = open_pair_recovery(100 + index as u64);
            server
                .open_pair_client(token, &format!("Zoe{index}"), "", &recovery, true, 0)
                .unwrap();
            assert!(session_for(&server, token).is_some());
            peers.push(peer);
        }
        let blocked = Token(200);
        let (conn, peer) = test_live_client();
        server.clients.insert(blocked, conn);
        peers.push(peer);

        let recovery = open_pair_recovery(999);
        server
            .open_pair_client(blocked, "Zoe", "", &recovery, true, 0)
            .unwrap();

        assert!(session_for(&server, blocked).is_none());
    }

    #[test]
    fn open_pair_rate_limits_new_allocations_globally() {
        let mut server = open_pair_test_server();
        let now = Instant::now();
        for _ in 0..OPEN_PAIR_GLOBAL_LIMIT {
            server.open_pair_global_allocations.push_back(now);
        }
        let token = Token(300);
        let (conn, _peer) = test_live_client();
        server.clients.insert(token, conn);

        server
            .open_pair_client(token, "Zoe", "", "", true, 0)
            .unwrap();

        assert!(session_for(&server, token).is_none());
    }

    #[test]
    fn open_pair_rate_limits_password_attempts_globally() {
        let mut server = open_pair_test_server();
        server.config.security.password_hash = Some(hash_secret("hunter2"));
        let now = Instant::now();
        for _ in 0..(OPEN_PAIR_GLOBAL_LIMIT - 1) {
            server.open_pair_global_allocations.push_back(now);
        }

        let wrong = Token(400);
        let (conn, mut wrong_peer) = test_live_client();
        server.clients.insert(wrong, conn);
        server
            .open_pair_client(wrong, "Zoe", "wrong", "", true, 0)
            .unwrap();
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut wrong_peer)
        else {
            panic!("expected password mismatch error");
        };
        assert_eq!(code, ERROR_PASSWORD_MISMATCH);

        let blocked = Token(401);
        let (conn, mut blocked_peer) = test_live_client();
        server.clients.insert(blocked, conn);
        server
            .open_pair_client(blocked, "Zoe", "hunter2", "", true, 0)
            .unwrap();
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut blocked_peer)
        else {
            panic!("expected rate-limit error");
        };
        assert_eq!(code, ERROR_OPEN_PAIR_RATE_LIMITED);
        assert!(session_for(&server, blocked).is_none());
    }

    #[test]
    fn pairing_resolves_user_from_code() {
        let mut server = test_server();
        server.users.users = vec![UserConfig {
            id: UserId(3),
            internal_reference: "dana".to_string(),
            username: "old".to_string(),
            token_hash: String::new(),
        }];
        let code = "pairing-code-secret-1234567890";
        server.invites.insert(
            "dana".to_string(),
            InviteState {
                pairing_code_hash: hash_secret(code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );
        let new_token = "dana-client-generated-token-with-at-least-32-bytes";

        // No `user` is supplied: the server matches the invite by pairing code.
        let _peer = seed_session_client(&mut server, Token(2));
        let _ = server.pair_client(Token(2), "Dana", code, new_token, true, 0);

        let user = server
            .users
            .users
            .iter()
            .find(|user| user.internal_reference == "dana")
            .expect("user exists");
        assert!(verify_secret_hash(&user.token_hash, new_token));
        assert_eq!(user.username, "Dana");
        assert!(server.invites.is_empty());
    }

    /// A monotonic route-id source for [`test_live_client`], so distinct test
    /// connections that each establish a session get non-colliding media routes.
    static NEXT_TEST_ROUTE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);

    /// Builds a live `ClientConn` over a connected loopback socket with a clear
    /// control record and a native session transport, so server send and
    /// session-establishment paths run without a full crypto handshake. The
    /// returned peer socket must be kept alive for the test so the connection
    /// stays open.
    fn test_live_client() -> (ClientConn, std::net::TcpStream) {
        let route_id = NEXT_TEST_ROUTE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let client = std::net::TcpStream::connect(addr).expect("connect loopback");
        let client_addr = client.local_addr().expect("client addr");
        // The production socket is nonblocking under mio; tests exercising the
        // read/write paths rely on the same would-block semantics.
        client.set_nonblocking(true).expect("set nonblocking");
        let (peer, _) = listener.accept().expect("accept loopback");
        let now = Instant::now();
        let conn = ClientConn {
            socket: TcpStream::from_std(client),
            addr: client_addr,
            read_buf: RecvBuffer::new(),
            readiness: Readiness::new(),
            write_buf: WriteQueue::new(),
            kind: ConnKind::Control,
            state: ConnState::Ready,
            control: Some(RecordProtection::clear()),
            transport: Some(test_transport(route_id)),
            session_id: None,
            user_id: None,
            close: Close::Open,
            created_at: now,
            last_activity: now,
        };
        (conn, peer)
    }

    /// Registers a live client at `token` carrying a session transport, so
    /// `establish_session` can take it exactly as it would after a real
    /// handshake. Returns the peer socket, which the caller keeps alive.
    fn seed_session_client(server: &mut Server, token: Token) -> std::net::TcpStream {
        let (conn, peer) = test_live_client();
        server.clients.insert(token, conn);
        peer
    }

    fn drive_scheduled_pass(server: &mut Server) -> u64 {
        let ready = server.event_notifier.take_ready();
        server.process_control_sends();
        server.process_video_fanouts();
        if ready & ROOM_LOG_EVENTS != 0 {
            server.store.drain_log_events();
        }
        if ready & ROOM_STATE_EVENTS != 0 {
            server.drain_dm_completions().unwrap();
        }
        if ready & HISTORY_EVENTS != 0 {
            server.drain_history_replies();
        } else if !server.history_deliveries.is_empty() {
            server.process_history_deliveries();
        }
        if ready & MLS_EVENTS != 0 {
            server.drain_mls_replies();
        }
        if ready & IDENTITY_EVENTS != 0 {
            server.drain_identity_replies();
        }
        if ready & BUG_REPORT_EVENTS != 0 {
            server.drain_bug_report_replies();
        }
        let writes = server.loop_work.client_write_count();
        for _ in 0..writes {
            if let Some(token) = server.loop_work.pop_client_write() {
                server.write_client(token);
            }
        }
        ready
    }

    fn drive_scheduled_work(server: &mut Server) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ready = drive_scheduled_pass(server);
            let waiting_for_worker = server.pending_identity.is_some()
                || !server.pending_bug_reports.is_empty()
                || !server.pending_dm_waiters.is_empty()
                || server
                    .sessions
                    .values()
                    .any(|session| session.pending_disk_history_fetches > 0);
            let scheduled_output = server.loop_work.background_work
                || server.loop_work.client_write_count() > 0;
            if ready == 0 && !waiting_for_worker && !scheduled_output {
                return;
            }
            assert!(Instant::now() < deadline, "server scheduler did not quiesce");
            thread::yield_now();
        }
    }

    fn read_plaintext_server_payload(
        server: &mut Server,
        peer: &mut std::net::TcpStream,
    ) -> Vec<u8> {
        drive_scheduled_work(server);
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let mut len = [0u8; 4];
        peer.read_exact(&mut len).expect("read frame length");
        let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
        peer.read_exact(&mut frame).expect("read frame body");
        frame
    }

    /// Reads a frame while driving the server's nonblocking writer. Production
    /// does this from writable poll events; large-frame tests have to model it
    /// explicitly because socket buffer capacity varies by operating system.
    fn read_plaintext_server_payload_while_flushing(
        server: &mut Server,
        token: Token,
        peer: &mut std::net::TcpStream,
    ) -> Vec<u8> {
        peer.set_read_timeout(Some(Duration::from_millis(20)))
            .expect("set read timeout");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut read_exact = |buf: &mut [u8], what: &str| {
            let mut offset = 0;
            while offset < buf.len() {
                drive_scheduled_pass(server);
                server.write_client(token);
                match peer.read(&mut buf[offset..]) {
                    Ok(0) => panic!("server closed while reading {what}"),
                    Ok(read) => offset += read,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => panic!("read {what}: {error}"),
                }
                assert!(Instant::now() < deadline, "timed out reading {what}");
            }
        };

        let mut len = [0u8; 4];
        read_exact(&mut len, "frame length");
        let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
        read_exact(&mut frame, "frame body");
        frame
    }

    fn read_plaintext_server_control(
        server: &mut Server,
        peer: &mut std::net::TcpStream,
    ) -> ServerControl {
        let frame = read_plaintext_server_payload(server, peer);
        rpc::control::decode_server_control(&frame).expect("decode server control")
    }

    #[test]
    fn pairing_does_not_announce_or_join_voice() {
        // A pairing connection acknowledges with `Authenticated` but must not
        // announce presence or join voice. Otherwise it broadcasts an online
        // presence, then a matching offline when the throwaway socket drops,
        // so other clients see the user flicker before the real session joins.
        let mut server = test_server();
        server.users.users = vec![UserConfig {
            id: UserId(4),
            internal_reference: "gwen".to_string(),
            username: "old".to_string(),
            token_hash: String::new(),
        }];
        let code = "pairing-code-secret-2468013579";
        server.invites.insert(
            "gwen".to_string(),
            InviteState {
                pairing_code_hash: hash_secret(code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );

        // A live client under the pairing token makes `live_token_for_session`
        // resolve, so the join path would run if it were not gated off.
        let token = Token(7);
        let (conn, _peer) = test_live_client();
        server.clients.insert(token, conn);

        let new_token = "gwen-client-generated-token-with-at-least-32-bytes";
        let _ = server.pair_client(token, "Gwen", code, new_token, true, 0);

        let session = server
            .sessions
            .values()
            .find(|session| session.username == "Gwen")
            .expect("pairing session exists");
        assert_eq!(session.voice_room, None);
        assert!(!session.announced);
        assert!(
            server
                .rooms
                .get(&DEFAULT_ROOM)
                .expect("lobby exists")
                .active_streams
                .is_empty()
        );
    }

    #[test]
    fn pairing_rejects_token_collision() {
        let new_token = "shared-client-generated-token-with-at-least-32-bytes";
        let mut server = test_server();
        server.users.users = vec![
            UserConfig {
                id: UserId(1),
                internal_reference: "erin".to_string(),
                username: "Erin".to_string(),
                token_hash: hash_secret(new_token),
            },
            UserConfig {
                id: UserId(2),
                internal_reference: "frank".to_string(),
                username: "old".to_string(),
                token_hash: String::new(),
            },
        ];
        let code = "pairing-code-secret-0987654321";
        server.invites.insert(
            "frank".to_string(),
            InviteState {
                pairing_code_hash: hash_secret(code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );

        // Frank's generated token collides with Erin's existing one, so the
        // server must refuse to pair rather than make the token ambiguous.
        let _ = server.pair_client(Token(5), "Frank", code, new_token, true, 0);

        let frank = server
            .users
            .users
            .iter()
            .find(|user| user.internal_reference == "frank")
            .expect("frank exists");
        assert!(frank.token_hash.is_empty());
        assert!(!server.invites.is_empty());
        assert!(server.sessions.is_empty());
    }

    #[test]
    fn pairing_rejects_control_character_username() {
        let mut server = test_server();
        let code = "pairing-code-secret-5544332211";
        server.invites.insert(
            "ivy".to_string(),
            InviteState {
                pairing_code_hash: hash_secret(code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );
        let token = Token(6);
        let (conn, mut peer) = test_live_client();
        server.clients.insert(token, conn);

        server
            .pair_client(
                token,
                "Ivy\u{1}",
                code,
                "ivy-client-generated-token-with-at-least-32-bytes",
                true,
                0,
            )
            .unwrap();

        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut peer) else {
            panic!("expected invalid display name error");
        };
        assert_eq!(code, ERROR_PAIRING_INVALID_REQUEST);
        assert!(server.sessions.is_empty());
        assert!(server.users.users.is_empty());
    }

    #[test]
    fn pairing_state_write_failure_replies_with_error() {
        let blocker = std::env::temp_dir().join(format!(
            "chatt-pair-blocker-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&blocker, "not a directory").unwrap();
        let mut server = test_server();
        server.users.path = Some(blocker.join("users.toml"));
        let code = "pairing-code-secret-9988776655";
        server.invites.insert(
            "jo".to_string(),
            InviteState {
                pairing_code_hash: hash_secret(code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );
        let token = Token(8);
        let (conn, mut peer) = test_live_client();
        server.clients.insert(token, conn);

        server
            .pair_client(
                token,
                "Jo",
                code,
                "jo-client-generated-token-with-at-least-32-bytes",
                true,
                0,
            )
            .unwrap();

        drive_scheduled_work(&mut server);
        let _ = std::fs::remove_file(&blocker);
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut server, &mut peer) else {
            panic!("expected persist failure error reply");
        };
        assert_eq!(code, control::ERROR_INTERNAL);
        assert!(server.sessions.is_empty());
        assert!(
            !server.invites.is_empty(),
            "invite must survive a failed pairing so the client can retry"
        );
    }

    #[test]
    fn pairing_never_rewrites_the_operator_config() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-pair-config-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("chatt-server.toml");
        let content = format!(
            "# operator comment that must survive pairing\n\
             [network]\ntcp-addr = \"127.0.0.1:0\"\np2p-enabled = false\n\n\
             [security]\nserver-identity-seed = \"{}\"\n",
            rpc::crypto::dev_server_seed_hex()
        );
        std::fs::write(&config_path, &content).unwrap();
        let mut server = Server::bind(ServerConfig::load(&config_path).unwrap()).unwrap();
        let code = "pairing-code-secret-1122334455";
        server.invites.insert(
            "hana".to_string(),
            InviteState {
                pairing_code_hash: hash_secret(code),
                expires_at: std::time::Instant::now() + INVITE_TTL,
            },
        );

        let _peer = seed_session_client(&mut server, Token(2));
        let _ = server.pair_client(
            Token(2),
            "Hana",
            code,
            "hana-client-generated-token-with-at-least-32-bytes",
            true,
            0,
        );

        drive_scheduled_work(&mut server);

        let after = std::fs::read_to_string(&config_path).unwrap();
        let users_content =
            std::fs::read_to_string(dir.join("chatt-server-data").join("users.toml")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(after, content, "operator config must stay byte-identical");
        assert!(users_content.contains("name = \"hana\""));
        assert!(
            server
                .sessions
                .values()
                .any(|session| session.username == "Hana")
        );
    }

    #[test]
    fn accept_error_classification_matches_fd_pressure_only() {
        assert!(is_fd_pressure_accept_error(&io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(is_fd_pressure_accept_error(&io::Error::from_raw_os_error(
            libc::ENFILE
        )));
        assert!(!is_fd_pressure_accept_error(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(!is_fd_pressure_accept_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

}
