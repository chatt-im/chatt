use hashbrown::{HashMap, HashSet};
use std::{
    collections::VecDeque,
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::mpsc,
    sync::{Arc, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub mod config;
mod history_reader;
pub mod local_admin;
pub mod room_store;
pub mod user_store;

use mio::{
    Events, Interest, Poll, Token, Waker,
    net::{TcpListener, TcpStream, UdpSocket},
};
use ring::rand::SecureRandom;
use ring::signature::KeyPair;
use rpc::{
    control::{
        self, ChatMessage, ClientControl, ERROR_AUTH_REJECTED, ERROR_BUG_REPORT_REJECTED,
        ERROR_OPEN_PAIR_RATE_LIMITED, ERROR_PAIRING_INVALID_REQUEST, ERROR_PAIRING_NOT_ACTIVE,
        ERROR_PASSWORD_MISMATCH, ERROR_PASSWORD_REQUIRED, ERROR_PUBLIC_DISABLED,
        ERROR_TOKEN_STALE_EPOCH, FileContentEncoding, FileMetadata, InviteTicket,
        MAX_BUG_REPORT_BYTES, MAX_CONTROL_PAYLOAD_BYTES, MAX_FILE_CHUNK_BYTES, P2pCandidate,
        P2pKey, P2pNatKind, P2pPeerInfo, P2pRole, RoomInfo, RoomKind, ServerControl, UserSummary,
        decode_client_control, decode_client_hello, encode_invite_ticket, encode_server_control,
        encode_server_hello, max_file_wire_bytes,
    },
    crypto::{
        AntiReplay, CHANNEL_CONTROL, CHANNEL_VIDEO, ControlTransport, DYNAMIC_TOKEN_PREFIX,
        DynamicTokenClaims, KEY_LEN, KeyMaterial, SessionSecrets, TAG_LEN, TRANSPORT_HEADER_LEN,
        TransportCipher, VideoKeyRole, derive_video_keys, encode_hex, issue_dynamic_token,
        respond_to_client_hello, respond_to_client_hello_plaintext, verify_dynamic_token,
    },
    frame,
    ids::{BugReportId, FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload},
    recv::RecvBuffer,
    video::{self, VideoAck, VideoHello, VideoRole},
};

use config::{Config as ServerConfig, UserConfig, hash_secret, value_arg, verify_secret_hash};
use history_reader::{HistoryReadReply, HistoryReader};
use local_admin::{AdminCommand, AdminSocket};
use room_store::RoomStore;
use user_store::UserStore;

#[cfg(test)]
use rpc::history;

const LISTENER: Token = Token(0);
const UDP: Token = Token(1);
const UDP_PROBE: Token = Token(2);
/// Wake-only token the history reader thread signals after queueing a reply.
const WAKER: Token = Token(3);
const FIRST_CLIENT: usize = 4;
/// The default config's lobby room id, used by tests asserting lobby state.
#[cfg(test)]
const DEFAULT_ROOM: RoomId = RoomId(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(20);
const MAX_CLIENTS: usize = 1024;
/// Minimum spare capacity ensured before each direct read into a connection's
/// receive buffer. The buffer grows past this on demand (a full file-chunk
/// frame arrives in a couple of reads once its capacity has ramped), while
/// connections that only exchange small control frames stay small.
const TCP_READ_CHUNK_BYTES: usize = 64 * 1024;
/// Cap on bytes read from one TCP connection per readiness pass, so one fast
/// sender cannot monopolize the single-threaded loop or grow its `read_buf`
/// without bound inside a single poll iteration. mio events are edge-triggered,
/// so a connection cut off by the budget is queued for a re-read on the next
/// loop pass rather than waiting for a new readiness event.
const READ_BUDGET_BYTES: usize = 256 * 1024;
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
/// Most concurrent file uploads one session may hold open. A well-behaved
/// client streams uploads one at a time, so this only bounds a client that
/// opens transfers and never finishes them.
const MAX_ACTIVE_UPLOADS_PER_SESSION: usize = 8;
/// Cap on a stream's fast-start ring. Keyframes reset the ring, so this only
/// bounds a pathologically long GOP rather than normal operation.
const VIDEO_RING_MAX_BYTES: usize = 8 * 1024 * 1024;
/// A subscriber whose queued bytes exceed this after a flush is too slow to keep
/// up and is dropped. It reconnects and fast-starts from the latest keyframe.
const VIDEO_SUBSCRIBER_HIGH_WATER: usize = 8 * 1024 * 1024;
/// A control connection whose queued bytes exceed this after a flush is not
/// draining its socket and is dropped, so a stalled file-relay recipient or a
/// history-pipelining client cannot pin megabytes of server memory.
const CONTROL_WRITE_HIGH_WATER: usize = 8 * 1024 * 1024;
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

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Readiness(bool);

impl Readiness {
    #[inline]
    fn new() -> Self {
        Self(false)
    }

    #[cfg(test)]
    #[inline]
    fn primed() -> Self {
        Self(true)
    }

    #[inline]
    fn mark_ready(&mut self) {
        self.0 = true;
    }

    #[inline]
    fn mark_drained(&mut self) {
        self.0 = false;
    }

    #[inline]
    fn is_ready(self) -> bool {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReadPumpOutcome {
    bytes_read: usize,
    hit_budget: bool,
    disconnected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopTask {
    ClientRead(Token),
}

#[derive(Debug, Default)]
struct LoopWork {
    tasks: Vec<LoopTask>,
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
        !self.tasks.is_empty()
    }

    #[inline]
    fn queue_client_read(&mut self, token: Token) {
        let task = LoopTask::ClientRead(token);
        if !self.tasks.contains(&task) {
            self.tasks.push(task);
        }
    }

    #[inline]
    fn has_client_read(&self, token: Token) -> bool {
        self.tasks.contains(&LoopTask::ClientRead(token))
    }

    #[inline]
    fn take_tasks(&mut self) -> Vec<LoopTask> {
        std::mem::take(&mut self.tasks)
    }

    #[cfg(test)]
    fn queued_client_reads(&self) -> Vec<Token> {
        self.tasks
            .iter()
            .map(|task| match *task {
                LoopTask::ClientRead(token) => token,
            })
            .collect()
    }
}

#[inline]
fn read_socket_into_buffer(
    socket: &impl std::os::fd::AsRawFd,
    read_buf: &mut RecvBuffer,
    readiness: &mut Readiness,
    read_chunk_bytes: usize,
    read_budget_bytes: usize,
) -> io::Result<ReadPumpOutcome> {
    debug_assert!(read_chunk_bytes > 0);
    debug_assert!(read_budget_bytes > 0);

    let mut outcome = ReadPumpOutcome::default();
    while readiness.is_ready() {
        match read_buf.fill(socket, read_chunk_bytes) {
            Ok(0) => {
                readiness.mark_drained();
                outcome.disconnected = true;
                break;
            }
            Ok(read) => {
                outcome.bytes_read += read;
                if outcome.bytes_read >= read_budget_bytes {
                    outcome.hit_budget = true;
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                readiness.mark_drained();
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(outcome)
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
        encryption = config.security.encryption,
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
    let (admin_tx, admin_rx) = mpsc::channel();
    let admin_socket = AdminSocket::spawn(admin_tx).map_err(invalid_config)?;
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
        "chatt transport encryption: {}",
        if server.config.security.encryption {
            "enabled"
        } else {
            "disabled"
        }
    );
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
        encryption = server.config.security.encryption,
        p2p_enabled = server.config.network.p2p_enabled
    );
    let _admin_socket = admin_socket;
    server.run(&admin_rx)
}

pub struct Server {
    config: ServerConfig,
    /// The server-managed user registry, persisted under the data dir. Public
    /// so harnesses (`bench_upload`) can seed users directly.
    pub users: UserStore,
    poll: Poll,
    listener: TcpListener,
    udp: UdpSocket,
    udp_probe: Option<UdpSocket>,
    clients: HashMap<Token, ClientConn>,
    sessions: HashMap<SessionId, Session>,
    media_key_to_session: HashMap<u32, SessionId>,
    plaintext_addr_to_session: HashMap<SocketAddr, SessionId>,
    peer_links: HashMap<(SessionId, SessionId), PeerLink>,
    rooms: HashMap<RoomId, RoomState>,
    streams: HashMap<StreamId, VideoStream>,
    next_token: usize,
    next_session: u64,
    next_stream: u32,
    next_connection_id: u64,
    next_file_transfer: u64,
    active_uploads: HashMap<(SessionId, FileTransferId), ServerUpload>,
    active_bug_reports: HashMap<(SessionId, BugReportId), ServerBugReport>,
    reserved_file_names: HashSet<String>,
    default_room: RoomId,
    store: RoomStore,
    file_size_limit_bytes: u64,
    invites: HashMap<String, InviteState>,
    open_pair_global_allocations: VecDeque<Instant>,
    open_pair_ip_allocations: HashMap<IpAddr, VecDeque<Instant>>,
    rng: ring::rand::SystemRandom,
    server_key_pair: ring::signature::Ed25519KeyPair,
    next_media_sweep_at: Option<Instant>,
    next_rtt_snapshot_at: Instant,
    next_idle_sweep_at: Instant,
    /// Local work that must run before the loop parks in `poll`. Producers
    /// register here instead of teaching the core loop each local no-sleep
    /// condition.
    loop_work: LoopWork,
    history_reader: HistoryReader,
    history_replies: mpsc::Receiver<HistoryReadReply>,
    /// Reusable sealing buffers for [`Self::send_udp_payload`], so per-packet
    /// voice fan-out allocates nothing in steady state.
    udp_send_packet: Vec<u8>,
    udp_send_scratch: Vec<u8>,
    /// Reusable recipient list for [`Self::relay_voice`].
    relay_recipients: Vec<SessionId>,
}

impl Server {
    /// Local TCP address the listener is bound to, resolving an ephemeral `:0`
    /// port to the concrete port the OS assigned.
    pub fn tcp_local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Local UDP address the media socket is bound to.
    pub fn udp_local_addr(&self) -> io::Result<SocketAddr> {
        self.udp.local_addr()
    }

    pub fn bind(config: ServerConfig) -> io::Result<Self> {
        let tcp_addr = config.network.tcp_addr;
        let udp_addr = config.network.udp_addr();
        let udp_probe_addr = config.network.udp_probe_addr;
        let p2p_enabled = config.network.p2p_enabled;
        let poll = Poll::new()?;
        let mut listener = TcpListener::bind(tcp_addr)?;
        let mut udp = UdpSocket::bind(udp_addr)?;
        let mut udp_probe = if p2p_enabled {
            udp_probe_addr.map(UdpSocket::bind).transpose()?
        } else {
            None
        };
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;
        poll.registry()
            .register(&mut udp, UDP, Interest::READABLE)?;
        if let Some(udp_probe) = udp_probe.as_mut() {
            poll.registry()
                .register(udp_probe, UDP_PROBE, Interest::READABLE)?;
        }

        let users = UserStore::open(config.data_dir())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut rooms = HashMap::new();
        for room in &config.rooms {
            let access = match &room.members {
                None => RoomAccess::Public,
                Some(members) => RoomAccess::Private(
                    members
                        .iter()
                        .filter_map(|member| {
                            let user = users.users.iter().find(|user| user.name == *member);
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
        let store = RoomStore::open(config.data_dir(), &config.rooms);
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
        let (history_reader, history_replies) = HistoryReader::spawn(waker);

        let server_key_pair = config
            .server_key_pair()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        kvlog::info!(
            "server identity loaded",
            public_key = encode_hex(server_key_pair.public_key().as_ref()).as_str()
        );
        if !config.security.encryption {
            kvlog::warn!(
                "encryption disabled: plaintext UDP trusts spoofable Bind packets and allows session hijack, development only, never expose publicly"
            );
        }
        let file_size_limit_bytes = config.security.max_file_size_bytes;

        Ok(Self {
            config,
            users,
            poll,
            listener,
            udp,
            udp_probe,
            clients: HashMap::new(),
            sessions: HashMap::new(),
            media_key_to_session: HashMap::new(),
            plaintext_addr_to_session: HashMap::new(),
            peer_links: HashMap::new(),
            rooms,
            streams: HashMap::new(),
            next_token: FIRST_CLIENT,
            next_session: 1,
            next_stream: 1,
            next_connection_id: 1,
            next_file_transfer: 1,
            active_uploads: HashMap::new(),
            active_bug_reports: HashMap::new(),
            reserved_file_names: HashSet::new(),
            default_room,
            store,
            file_size_limit_bytes,
            invites: HashMap::new(),
            open_pair_global_allocations: VecDeque::new(),
            open_pair_ip_allocations: HashMap::new(),
            rng: ring::rand::SystemRandom::new(),
            server_key_pair,
            next_media_sweep_at: None,
            next_rtt_snapshot_at: Instant::now() + RTT_SNAPSHOT_INTERVAL,
            next_idle_sweep_at: Instant::now() + IDLE_SWEEP_INTERVAL,
            loop_work: LoopWork::default(),
            history_reader,
            history_replies,
            udp_send_packet: Vec::new(),
            udp_send_scratch: Vec::new(),
            relay_recipients: Vec::new(),
        })
    }

    pub fn run(
        &mut self,
        admin_rx: &mpsc::Receiver<AdminCommand>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut events = Events::with_capacity(256);
        loop {
            let timeout = self.loop_work.poll_timeout(POLL_TIMEOUT);
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
            for event in events.iter() {
                match event.token() {
                    LISTENER => self.accept_clients()?,
                    UDP => self.receive_udp(0),
                    UDP_PROBE => self.receive_udp(1),
                    // Wake-only; replies drain unconditionally below.
                    WAKER => {}
                    token => {
                        if event.is_readable() {
                            if let Some(client) = self.clients.get_mut(&token) {
                                client.readiness.mark_ready();
                            }
                            self.loop_work.queue_client_read(token);
                        }
                        if event.is_writable() {
                            self.write_client(token);
                        }
                    }
                }
            }
            let loop_tasks = self.loop_work.take_tasks();
            for task in loop_tasks {
                match task {
                    LoopTask::ClientRead(token) => self.read_client(token),
                }
            }
            self.requeue_unclogged_uploaders();
            self.drain_history_replies();
            self.handle_admin_commands(admin_rx);
            self.flush_disconnects();
            let now = Instant::now();
            self.sweep_idle_connections(now);
            self.sweep_stale_media_keys(now);
            self.poll_room_rtt_snapshots(now);
        }
    }

    fn handle_admin_commands(&mut self, admin_rx: &mpsc::Receiver<AdminCommand>) {
        loop {
            match admin_rx.try_recv() {
                Ok(AdminCommand::Invite { user, reply }) => {
                    let result = self.create_invite(&user);
                    let _ = reply.send(result);
                }
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
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
        loop {
            let (mut socket, addr) = match self.listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if is_transient_accept_error(&error) => {
                    // fd pressure (EMFILE/ENFILE) cannot drain by accepting, so
                    // back off briefly before returning. This stalls the whole
                    // single-threaded loop, but a tight retry would just spin on
                    // the same error and starve existing clients harder.
                    kvlog::warn!("transient tcp accept failure", error = %error);
                    thread::sleep(ACCEPT_ERROR_BACKOFF);
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
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
                    secrets: None,
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
        let mut hit_budget = false;
        if let Some(client) = self.clients.get_mut(&token) {
            if !clogged {
                match read_socket_into_buffer(
                    &client.socket,
                    &mut client.read_buf,
                    &mut client.readiness,
                    TCP_READ_CHUNK_BYTES,
                    READ_BUDGET_BYTES,
                ) {
                    Ok(outcome) => {
                        if outcome.bytes_read > 0 {
                            client.last_activity = Instant::now();
                        }
                        if outcome.disconnected {
                            kvlog::info!("tcp client closed", token = token.0);
                        }
                        hit_budget = outcome.hit_budget;
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
        if hit_budget {
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

        loop {
            // Relayed chunks queue to recipients inside `handle_control`, so
            // re-check between frames and park with the remaining frames
            // still buffered once a recipient's queue is over the cap.
            if self.relay_clogged_for_reader(token) {
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
                    if let Err(error) = self.handle_control(token, control) {
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
        loop {
            let Some(client) = self.clients.get_mut(&token) else {
                return;
            };
            if client.is_closing() {
                return;
            }
            match video_conn_step(client) {
                Ok(None) => return,
                Ok(Some(VideoStep::Handshake(record))) => {
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
                })) => self.publish_video_frame(stream_id, frame, is_key),
                Ok(Some(VideoStep::SubscriberChatter)) => {}
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
        video.cipher = Some(TransportCipher::new(send, recv));
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
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let ConnKind::Video(video) = &mut client.kind else {
            return Err("auth on a non-video connection".to_string());
        };
        let cipher = video
            .cipher
            .as_mut()
            .ok_or_else(|| "video auth before hello".to_string())?;
        // The auth payload's contents are irrelevant: opening it proves the peer
        // holds the per-stream secret. A peer that cannot seal a record the
        // server opens is dropped, so guessing a stream id yields nothing.
        cipher
            .open_next(CHANNEL_VIDEO, record)
            .map_err(|error| format!("video auth failed: {error}"))?;
        let ack = video::encode_video_ack(&VideoAck::Ok);
        let sealed = cipher
            .seal_next(CHANNEL_VIDEO, &ack)
            .map_err(|error| error.to_string())?;
        video.phase = VideoPhase::Streaming;
        let role = video
            .role
            .ok_or_else(|| "video auth before hello".to_string())?;
        let stream_id = video
            .stream_id
            .ok_or_else(|| "video auth before hello".to_string())?;
        video::write_record(client.write_buf.tail_mut(), &sealed)
            .map_err(|error| error.to_string())?;
        self.write_client(token);
        match role {
            VideoRole::Publisher => self.attach_publisher(token, stream_id),
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
        let burst: Vec<Arc<[u8]>> = {
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
        for data in &burst {
            self.seal_video_to_subscriber(token, data)?;
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
    /// queue (no intermediate frame allocation) and flushes. Returns whether
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
            let cipher = video
                .cipher
                .as_mut()
                .ok_or_else(|| "subscriber missing cipher".to_string())?;
            let sealed_len = TRANSPORT_HEADER_LEN + data.len() + TAG_LEN;
            if sealed_len > video::MAX_VIDEO_FRAME_LEN {
                return Err("sealed video record exceeds maximum length".to_string());
            }
            let out = client.write_buf.tail_mut();
            out.extend_from_slice(&(sealed_len as u32).to_le_bytes());
            if let Err(error) = cipher.seal_next_into(CHANNEL_VIDEO, data, out) {
                out.truncate(out.len() - video::VIDEO_LENGTH_PREFIX_LEN);
                return Err(error.to_string());
            }
        }
        self.write_client(token);
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
    fn publish_video_frame(&mut self, stream_id: StreamId, data: Arc<[u8]>, is_key: bool) {
        let subscribers = {
            let Some(stream) = self.streams.get_mut(&stream_id) else {
                return;
            };
            if is_key {
                stream.ring.clear();
                stream.ring_bytes = 0;
            }
            stream.ring_bytes += data.len();
            stream.ring.push_back(VideoRingFrame {
                data: data.clone(),
                is_key,
            });
            while stream.ring_bytes > VIDEO_RING_MAX_BYTES && stream.ring.len() > 1 {
                if let Some(front) = stream.ring.pop_front() {
                    stream.ring_bytes -= front.data.len();
                }
            }
            stream.subscribers.clone()
        };
        let mut overflowed = Vec::new();
        for token in subscribers {
            match self.seal_video_to_subscriber(token, &data) {
                Ok(true) => {}
                Ok(false) | Err(_) => overflowed.push(token),
            }
        }
        for token in overflowed {
            if let Some(stream) = self.streams.get_mut(&stream_id) {
                stream.subscribers.retain(|other| *other != token);
            }
            kvlog::warn!(
                "video subscriber dropped for backpressure",
                token = token.0,
                stream_id = stream_id.0
            );
            self.disconnect(token);
        }
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
                session.display_name.clone(),
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
        let encryption = self.config.security.encryption;
        let (server_hello, control, secrets) = if encryption {
            let response = respond_to_client_hello(&self.rng, &self.server_key_pair, &hello)
                .map_err(|error| error.to_string())?;
            (
                response.hello,
                ControlTransport::encrypted(
                    response.secrets.control_send.clone(),
                    response.secrets.control_recv.clone(),
                ),
                Some(response.secrets),
            )
        } else {
            let response =
                respond_to_client_hello_plaintext(&self.rng, &self.server_key_pair, &hello)
                    .map_err(|error| error.to_string())?;
            (response.hello, ControlTransport::plaintext(), None)
        };
        let encoded = encode_server_hello(&server_hello);
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        frame::encode_frame(&encoded, client.write_buf.tail_mut())
            .map_err(|error| error.to_string())?;
        client.control = Some(control);
        client.secrets = secrets;
        client.state = ConnState::AwaitAuth;
        kvlog::info!(
            "client handshake completed",
            token = token.0,
            queued_bytes = client.write_buf.len(),
            encryption
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
                    display_name,
                    token: auth_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.authenticate_client(
                token,
                &display_name,
                &auth_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (
                ConnState::AwaitAuth,
                ClientControl::Pair {
                    display_name,
                    pairing_code,
                    token: new_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.pair_client(
                token,
                &display_name,
                &pairing_code,
                &new_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (
                ConnState::AwaitAuth,
                ClientControl::OpenPair {
                    display_name,
                    password,
                    existing_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.open_pair_client(
                token,
                &display_name,
                &password,
                &existing_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (ConnState::AwaitAuth, _) => Err("authenticate before sending control messages".into()),
            (
                ConnState::Ready,
                ClientControl::Authenticate { .. }
                | ClientControl::Pair { .. }
                | ClientControl::OpenPair { .. },
            ) => Err("session is already authenticated".into()),
            (ConnState::Ready, ClientControl::SendChat { room_id, body }) => {
                let session_id = self.session_for_token(token)?;
                self.send_chat(session_id, room_id, body)
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
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.start_file_upload(session_id, room_id, transfer_id, name, size, encoding)
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
                let result = self.complete_bug_report(token, session_id, report_id);
                self.report_bug_outcome(token, result)
            }
            (ConnState::AwaitClientHello, _) => Err("handshake is not complete".into()),
        }
    }

    fn authenticate_client(
        &mut self,
        token: Token,
        display_name: &str,
        auth_token: &str,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        kvlog::info!("authenticate attempt", token = token.0);
        if auth_token.starts_with(DYNAMIC_TOKEN_PREFIX) {
            return self.authenticate_dynamic(
                token,
                display_name,
                auth_token,
                receive_files,
                file_receive_limit_bytes,
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
        let user_name = user.name.clone();
        let display_name = display_name.trim();
        let user = if !valid_display_name(display_name) || display_name == user.display_name {
            user
        } else {
            match self
                .users
                .set_user_display_name(&user_name, display_name.to_string())
            {
                Ok(user) => user,
                Err(error) => {
                    kvlog::warn!(
                        "display name update failed",
                        user = user_name.as_str(),
                        error = error.as_str()
                    );
                    user
                }
            }
        };
        self.establish_session(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            true,
            None,
        )
    }

    /// Builds a transient [`UserConfig`] for a dynamic (open-paired) user, whose
    /// details are never stored server-side. The user id doubles as the internal
    /// identifier since there is no username.
    fn dynamic_user(user_id: UserId, display_name: &str) -> UserConfig {
        let display_name = display_name.trim();
        let display_name = if valid_display_name(display_name) {
            display_name.to_string()
        } else {
            user_id.to_string()
        };
        UserConfig {
            id: user_id,
            name: user_id.to_string(),
            display_name,
            token_hash: String::new(),
        }
    }

    fn pair_client(
        &mut self,
        token: Token,
        display_name: &str,
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
        let display_name = display_name.trim();
        if !valid_display_name(display_name) {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                user = user_name.as_str(),
                reason = "invalid_display_name"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "pairing failed: display name must be 1-64 bytes with no control characters"
                    .to_string(),
            );
        }

        let token_hash = hash_secret(new_token);
        if self
            .users
            .users
            .iter()
            .any(|user| user.name != user_name && user.token_hash == token_hash)
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
        let user =
            match self
                .users
                .mark_user_paired(&user_name, display_name.to_string(), token_hash)
            {
                Ok(user) => user,
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
        self.invites.remove(&user_name);
        kvlog::info!(
            "pairing accepted",
            token = token.0,
            user_id = user.id.0,
            user = user.name.as_str()
        );
        self.establish_session(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            false,
            None,
        )
    }

    /// Authenticates a dynamic user from a server-issued bearer token. No config
    /// lookup: the token's AEAD tag proves authenticity and the claims carry the
    /// user id.
    fn authenticate_dynamic(
        &mut self,
        token: Token,
        display_name: &str,
        auth_token: &str,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        if !self.config.is_public() {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                reason = "public_disabled"
            );
            return self.reject_auth(
                token,
                ERROR_PUBLIC_DISABLED,
                "authentication failed: public dynamic users are disabled on this server"
                    .to_string(),
            );
        }
        let claims = verify_dynamic_token(&self.config.security.server_identity_seed, auth_token);
        let Ok(claims) = claims else {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                reason = "invalid_dynamic_token"
            );
            return self.reject_auth(
                token,
                ERROR_AUTH_REJECTED,
                "authentication failed: the token is not valid for this server".to_string(),
            );
        };
        if !is_dynamic_user_id(claims.user_id) {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                reason = "invalid_dynamic_user_id",
                user_id = claims.user_id.0
            );
            return self.reject_auth(
                token,
                ERROR_AUTH_REJECTED,
                "authentication failed: the token is not valid for this server".to_string(),
            );
        }
        if claims.password_epoch != self.config.password_epoch() {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                reason = "stale_epoch"
            );
            return self.reject_auth(
                token,
                ERROR_TOKEN_STALE_EPOCH,
                "authentication failed: the server password changed; re-pair to refresh your token"
                    .to_string(),
            );
        }
        let user = Self::dynamic_user(claims.user_id, display_name);
        self.establish_session(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            true,
            None,
        )
    }

    /// Self-service join on a public server. Verifies the password, allocates (or
    /// reuses, when `existing_token` still decodes) a dynamic user id, and issues
    /// a fresh bearer token bound to the current password epoch.
    fn open_pair_client(
        &mut self,
        token: Token,
        display_name: &str,
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
        let display_name = display_name.trim();
        if !valid_display_name(display_name) {
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "open pairing failed: display name must be 1-64 bytes with no control characters"
                    .to_string(),
            );
        }
        let seed = self.config.security.server_identity_seed.clone();
        let current_epoch = self.config.password_epoch();
        let existing_user_id = (!existing_token.is_empty())
            .then(|| verify_dynamic_token(&seed, existing_token).ok())
            .flatten()
            .filter(|claims| claims.password_epoch == current_epoch || current_password_verified)
            .map(|claims| claims.user_id)
            .filter(|user_id| is_dynamic_user_id(*user_id));
        let user_id = match existing_user_id {
            Some(user_id) => user_id,
            None => {
                if !self.check_open_pair_allocation_rate(token)? {
                    return Ok(());
                }
                match self.users.allocate_dynamic_user_id() {
                    Ok(user_id) => user_id,
                    Err(error) => {
                        kvlog::error!(
                            "open pair rejected",
                            token = token.0,
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
            }
        };
        let issued = issue_dynamic_token(
            &seed,
            &DynamicTokenClaims {
                user_id,
                password_epoch: current_epoch,
            },
        )
        .map_err(|error| error.to_string())?;
        kvlog::info!("open pair accepted", token = token.0, user_id = user_id.0);
        let user = Self::dynamic_user(user_id, display_name);
        self.establish_session(
            token,
            &user,
            receive_files,
            file_receive_limit_bytes,
            false,
            Some(issued),
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

    fn reject_auth(&mut self, token: Token, code: u16, message: String) -> Result<(), String> {
        self.send_control_to_token(token, &ServerControl::Error { code, message })?;
        if let Some(client) = self.clients.get_mut(&token) {
            client.begin_graceful_close(Instant::now());
        }
        self.write_client(token);
        Ok(())
    }

    fn establish_session(
        &mut self,
        token: Token,
        user: &UserConfig,
        receive_files: bool,
        file_receive_limit_bytes: u64,
        announce: bool,
        issued_token: Option<String>,
    ) -> Result<(), String> {
        let user_id = user.user_id();
        // Supersede any earlier session for this user before standing up the new
        // one, so a reconnect never leaves a stale ghost (and its last voice
        // status) behind alongside the fresh session.
        self.supersede_existing_sessions(user_id, token);

        let session_id = SessionId(self.next_session);
        self.next_session += 1;
        let display_name = user.display_name.clone();
        let identifier = user.name.clone();

        let secrets = self
            .clients
            .get(&token)
            .and_then(|client| client.secrets.clone());
        if let Some(secrets) = &secrets {
            self.register_media_key(secrets.media_recv.id, session_id)?;
        }
        self.sessions.insert(
            session_id,
            Session {
                user_id,
                display_name: display_name.clone(),
                identifier: identifier.clone(),
                tcp_token: token,
                voice_room: None,
                udp_addr: None,
                secrets,
                media_send_counter: 0,
                media_recv_replay: AntiReplay::new(),
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
            user = identifier.as_str(),
            receive_files,
            file_receive_limit_bytes
        );
        let rooms = self.accessible_room_infos(user_id);
        let users = self.user_summaries();
        let response = match issued_token {
            Some(token) => ServerControl::OpenPaired {
                token,
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
        if announce && self.live_token_for_session(session_id).is_some() {
            self.broadcast_presence(session_id, true);
        }
        Ok(())
    }

    /// Registers a session's inbound media key id for UDP demultiplexing.
    ///
    /// A 32-bit key id colliding with another live session's is astronomically
    /// unlikely, but silently overwriting would strand both sessions' UDP once
    /// either tears down (`teardown_session` removes the shared entry). The new
    /// session is rejected instead, forcing a reconnect whose fresh handshake
    /// draws a new random key id. Stale entries whose session is already gone
    /// are reclaimed.
    fn register_media_key(&mut self, key_id: u32, session_id: SessionId) -> Result<(), String> {
        if let Some(existing) = self.media_key_to_session.get(&key_id).copied()
            && existing != session_id
            && self.sessions.contains_key(&existing)
        {
            kvlog::error!(
                "media key id collision with a live session",
                key_id,
                existing_session_id = existing.0,
                rejected_session_id = session_id.0
            );
            return Err("media key id collision".to_string());
        }
        self.media_key_to_session.insert(key_id, session_id);
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
                    display_name: user.display_name.clone(),
                    identifier: user.name.clone(),
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
            display_name: session.display_name.clone(),
            identifier: session.identifier.clone(),
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

    /// Answers a history page: synchronously from the resident window when it
    /// holds any of the requested range, otherwise via the disk reader thread
    /// paging the room's rotated segments.
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
        for chunk_index in 0..chunk_count {
            let payload = match self.store.encode_history_chunk(&plan, chunk_index) {
                Ok(payload) => payload,
                Err(error) => {
                    kvlog::warn!(
                        "history chunk encode failed",
                        session_id = session_id.0,
                        room_id = room_id.0,
                        chunk_index,
                        chunk_count,
                        error = error.as_str()
                    );
                    break;
                }
            };
            if let Err(error) =
                self.queue_control_payload_to_token(token, "history_chunk", &payload)
            {
                kvlog::warn!(
                    "history chunk send failed",
                    session_id = session_id.0,
                    room_id = room_id.0,
                    chunk_index,
                    chunk_count,
                    error = error.as_str()
                );
                break;
            }
        }
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
        while let Ok(reply) = self.history_replies.try_recv() {
            if let Some(session) = self.sessions.get_mut(&reply.session_id) {
                session.pending_disk_history_fetches =
                    session.pending_disk_history_fetches.saturating_sub(1);
            }
            let Some(token) = self.live_token_for_session(reply.session_id) else {
                continue;
            };
            let chunk_count = reply.payloads.len();
            for (chunk_index, payload) in reply.payloads.into_iter().enumerate() {
                if let Err(error) =
                    self.queue_control_payload_to_token(token, "history_chunk", &payload)
                {
                    kvlog::warn!(
                        "disk history chunk send failed",
                        session_id = reply.session_id.0,
                        room_id = reply.room_id.0,
                        chunk_index,
                        chunk_count,
                        error = error.as_str()
                    );
                    break;
                }
            }
        }
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
        let room_id = match self.store.open_dm(requester, peer, now_ms()) {
            Ok(room_id) => room_id,
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
            Some(session) => (session.user_id, session.display_name.clone()),
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
        let message = ChatMessage {
            message_id: self.store.allocate_message_id(room_id),
            room_id,
            sender,
            sender_name,
            timestamp_ms: now_ms(),
            body,
            file_transfer_id: None,
        };
        let history_len = self.store.append(room_id, &message);
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

    fn start_file_upload(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        client_transfer_id: FileTransferId,
        name: String,
        original_size: u64,
        encoding: FileContentEncoding,
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
            Some(session) => (session.user_id, session.display_name.clone()),
            None => return Err("unknown session".into()),
        };
        if !self.check_room_access(session_id, room_id) {
            return Ok(());
        }
        let Some(uploader_token) = self.live_token_for_session(session_id) else {
            return Err("uploading session disconnected".into());
        };

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
        };

        let message = ChatMessage {
            message_id: self.store.allocate_message_id(room_id),
            room_id,
            sender,
            sender_name,
            timestamp_ms,
            body: format!(
                "sent file `{}` ({})",
                file_name,
                format_bytes(original_size)
            ),
            file_transfer_id: Some(server_transfer_id),
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
        self.store.append(room_id, &message);
        self.broadcast_control(room_id, &ServerControl::Chat { message });
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
        Ok(())
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

    fn receive_udp(&mut self, probe_id: u8) {
        let mut buf = [0u8; 2048];
        loop {
            let received = if probe_id == 0 {
                self.udp.recv_from(&mut buf)
            } else {
                let Some(udp_probe) = self.udp_probe.as_mut() else {
                    return;
                };
                udp_probe.recv_from(&mut buf)
            };
            let (len, src) = match received {
                Ok(value) => value,
                Err(error) if is_interrupted_io_error(&error) => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    kvlog::warn!("udp receive failed", error = %error);
                    break;
                }
            };
            let packet = &buf[..len];
            if let Err(error) = self.handle_udp_packet(probe_id, src, packet) {
                kvlog::warn!(
                    "udp packet rejected",
                    addr = %src,
                    packet_size = len,
                    error = %error
                );
            }
        }
    }

    fn handle_udp_packet(
        &mut self,
        server_probe_id: u8,
        src: SocketAddr,
        packet: &[u8],
    ) -> Result<(), String> {
        let (header, _) = media::parse_header(packet).map_err(|error| error.to_string())?;
        let (session_id, payload, udp_addr_changed) = if header.key_id == media::PLAINTEXT_KEY_ID {
            self.open_plaintext_udp_packet(server_probe_id, src, packet)?
        } else {
            let session_id = *self
                .media_key_to_session
                .get(&header.key_id)
                .ok_or_else(|| "unknown UDP key id".to_string())?;
            let (payload, udp_addr_changed) = {
                let session = self
                    .sessions
                    .get_mut(&session_id)
                    .ok_or_else(|| "unknown UDP session".to_string())?;
                let secrets = session
                    .secrets
                    .as_ref()
                    .ok_or_else(|| "encrypted UDP for plaintext session".to_string())?;
                let (_, payload) =
                    media::open_media(&secrets.media_recv, &mut session.media_recv_replay, packet)
                        .map_err(|error| error.to_string())?;
                // Behind a symmetric NAT the mapping toward the probe port
                // differs from the mapping toward the main port, so a probe
                // packet's source is only reachable by the probe flow. Recording
                // it would point voice and pongs sent from the main socket at a
                // dead address until the next main-socket packet flips it back.
                let udp_addr_changed = if server_probe_id == 0 {
                    let old_addr = session.observe_udp_addr(src);
                    old_addr.is_some_and(|old| old != src)
                } else {
                    false
                };
                (payload, udp_addr_changed)
            };
            (session_id, payload, udp_addr_changed)
        };

        // Authenticated media traffic proves the session is alive: the client
        // has no TCP keepalive but pings over UDP every few seconds, so this
        // is what keeps a healthy-but-silent control connection from being
        // reaped by the idle sweep.
        if let Some(session) = self.sessions.get(&session_id)
            && let Some(client) = self.clients.get_mut(&session.tcp_token)
        {
            client.last_activity = Instant::now();
        }

        match payload {
            MediaPayload::Bind { session_id: bound } => {
                if bound != session_id {
                    kvlog::warn!(
                        "udp bind rejected",
                        session_id = session_id.0,
                        bound_session_id = bound.0
                    );
                    return Err("UDP bind session mismatch".into());
                }
                kvlog::info!("udp session bound", session_id = session_id.0, addr = %src);
                let token = self.live_token_for_session(session_id);
                if let Some(token) = token {
                    let _ = self.send_control_to_token(token, &ServerControl::UdpBound);
                    if self.config.network.p2p_enabled
                        && self.live_token_for_session(session_id).is_some()
                    {
                        let _ = self.send_control_to_token(
                            token,
                            &ServerControl::UdpReflexive {
                                addr: src.to_string(),
                            },
                        );
                    }
                }
                Ok(())
            }
            MediaPayload::NatProbe {
                session_id: bound,
                probe_id,
            } => {
                if bound != session_id {
                    return Err("NAT probe session mismatch".into());
                }
                if !self.config.network.p2p_enabled {
                    return Ok(());
                }
                let token = self.live_token_for_session(session_id);
                if let Some(token) = token {
                    let _ = self.send_control_to_token(
                        token,
                        &ServerControl::P2pNatProbe {
                            probe_id: probe_id.max(server_probe_id),
                            addr: src.to_string(),
                        },
                    );
                }
                Ok(())
            }
            MediaPayload::Voice {
                stream_id,
                sequence,
                timestamp,
                flags,
                payload,
            } => self.relay_voice(session_id, stream_id, sequence, timestamp, flags, payload),
            MediaPayload::VoiceFeedback {
                stream_id,
                feedback,
            } => self.relay_voice_feedback(session_id, stream_id, feedback),
            MediaPayload::PeerVoice { .. } => Ok(()),
            MediaPayload::PeerVoiceFeedback { .. } => Ok(()),
            MediaPayload::Ping {
                nonce,
                observed_rtt_ms,
            } => {
                if let Some(session) = self.sessions.get_mut(&session_id) {
                    let reported_rtt_ms = if udp_addr_changed {
                        None
                    } else {
                        observed_rtt_ms
                    };
                    session.report_server_rtt(reported_rtt_ms, Instant::now());
                }
                self.send_udp_payload(session_id, &MediaPayload::Pong { nonce });
                Ok(())
            }
            MediaPayload::Pong { .. } => Ok(()),
        }
    }

    /// Development-only UDP path for `encryption = false`. The plaintext `Bind`
    /// and `NatProbe` carry a bare session id with no proof of possession, so
    /// any host able to spoof UDP toward the server can hijack a session's
    /// media address. This mode must never be exposed publicly; [`Server::bind`]
    /// logs a startup warning whenever it is active.
    fn open_plaintext_udp_packet(
        &mut self,
        server_probe_id: u8,
        src: SocketAddr,
        packet: &[u8],
    ) -> Result<(SessionId, MediaPayload, bool), String> {
        let (header, payload) =
            media::open_plaintext_media(packet).map_err(|error| error.to_string())?;
        let session_id = match &payload {
            MediaPayload::Bind { session_id } | MediaPayload::NatProbe { session_id, .. } => {
                *session_id
            }
            MediaPayload::Voice { .. }
            | MediaPayload::PeerVoice { .. }
            | MediaPayload::VoiceFeedback { .. }
            | MediaPayload::PeerVoiceFeedback { .. }
            | MediaPayload::Ping { .. }
            | MediaPayload::Pong { .. } => *self
                .plaintext_addr_to_session
                .get(&src)
                .ok_or_else(|| "unknown plaintext UDP source".to_string())?,
        };

        let old_addr = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| "unknown UDP session".to_string())?;
            if session.secrets.is_some() {
                return Err("plaintext UDP for encrypted session".to_string());
            }
            if !session.media_recv_replay.update(header.counter) {
                return Err(media::MediaError::Replay.to_string());
            }
            // Same probe-flow hazard as the encrypted path: a probe-socket
            // packet's source must not rebind the media address.
            if server_probe_id != 0 {
                return Ok((session_id, payload, false));
            }
            session.observe_udp_addr(src)
        };
        if let Some(old_addr) = old_addr
            && old_addr != src
            && self.plaintext_addr_to_session.get(&old_addr) == Some(&session_id)
        {
            self.plaintext_addr_to_session.remove(&old_addr);
        }
        self.plaintext_addr_to_session.insert(src, session_id);
        Ok((session_id, payload, old_addr.is_some_and(|old| old != src)))
    }

    fn relay_voice(
        &mut self,
        sender_session_id: SessionId,
        stream_id: StreamId,
        sequence: u32,
        timestamp: u32,
        flags: u8,
        voice_payload: media::VoicePayload,
    ) -> Result<(), String> {
        let room_id = match self.sessions.get(&sender_session_id) {
            Some(session)
                if session.active_stream == Some(stream_id) && session.voice_room.is_some() =>
            {
                session.voice_room.unwrap()
            }
            _ => return Ok(()),
        };
        let mut recipients = std::mem::take(&mut self.relay_recipients);
        recipients.clear();
        if let Some(room) = self.rooms.get(&room_id) {
            for session_id in room.active_streams.values() {
                if *session_id != sender_session_id {
                    recipients.push(*session_id);
                }
            }
        }
        kvlog::debug!(
            "voice packet relaying",
            session_id = sender_session_id.0,
            room_id = room_id.0,
            stream_id = stream_id.0,
            sequence,
            recipient_count = recipients.len(),
            payload_size = voice_payload.len()
        );
        log_audio_pop_server_media_packet(
            "rx",
            sender_session_id,
            Some(room_id),
            stream_id,
            sequence,
            flags,
            &voice_payload,
            Some(recipients.len()),
        );
        let payload = MediaPayload::Voice {
            stream_id,
            sequence,
            timestamp,
            flags,
            payload: voice_payload,
        };
        for session_id in &recipients {
            self.send_udp_payload(*session_id, &payload);
        }
        self.relay_recipients = recipients;
        Ok(())
    }

    fn relay_voice_feedback(
        &mut self,
        receiver_session_id: SessionId,
        stream_id: StreamId,
        feedback: media::VoiceFeedback,
    ) -> Result<(), String> {
        let room_id = match self.sessions.get(&receiver_session_id) {
            Some(session) => session.voice_room,
            None => None,
        };
        let Some(room_id) = room_id else {
            return Ok(());
        };
        let Some(owner_session_id) = self
            .rooms
            .get(&room_id)
            .and_then(|room| room.active_streams.get(&stream_id).copied())
        else {
            return Ok(());
        };
        if owner_session_id == receiver_session_id {
            return Ok(());
        }
        kvlog::debug!(
            "voice feedback relaying",
            receiver_session_id = receiver_session_id.0,
            owner_session_id = owner_session_id.0,
            room_id = room_id.0,
            stream_id = stream_id.0,
            expected = feedback.expected_packets,
            lost = feedback.lost_packets,
            late = feedback.late_packets,
            output_ring_ms = feedback.max_output_ring_ms,
            neteq_target_ms = feedback.max_neteq_target_ms,
            neteq_playout_delay_ms = feedback.max_neteq_playout_delay_ms,
            neteq_packet_buffer_ms = feedback.max_neteq_packet_buffer_ms
        );
        self.send_udp_payload(
            owner_session_id,
            &MediaPayload::VoiceFeedback {
                stream_id,
                feedback,
            },
        );
        Ok(())
    }

    fn send_udp_payload(&mut self, session_id: SessionId, payload: &MediaPayload) {
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        let Some(addr) = session.udp_addr else {
            return;
        };
        if let MediaPayload::Voice {
            stream_id,
            sequence,
            timestamp: _,
            flags,
            payload,
        } = payload
        {
            log_audio_pop_server_media_packet(
                "tx",
                session_id,
                session.voice_room,
                *stream_id,
                *sequence,
                *flags,
                payload,
                None,
            );
        }
        let counter = session.media_send_counter;
        session.media_send_counter = session.media_send_counter.wrapping_add(1);
        let packet = &mut self.udp_send_packet;
        let scratch = &mut self.udp_send_scratch;
        let sealed = match &session.secrets {
            Some(secrets) => {
                media::seal_media_into(&secrets.media_send, counter, payload, packet, scratch)
            }
            None => media::seal_plaintext_media_into(counter, payload, packet, scratch),
        };
        if let Err(error) = sealed {
            kvlog::warn!("udp seal failed", session_id = session_id.0, error = %error);
            return;
        }
        if let Err(error) = self.udp.send_to(packet, addr) {
            kvlog::warn!(
                "udp send failed",
                session_id = session_id.0,
                addr = %addr,
                packet_size = packet.len(),
                error = %error
            );
        }
    }

    fn send_control_to_token(
        &mut self,
        token: Token,
        control: &ServerControl,
    ) -> Result<(), String> {
        let payload = encode_server_control(control);
        self.queue_control_payload_to_token(token, server_control_kind(control), &payload)
    }

    /// Encodes the control once and queues it to every token; per-recipient
    /// failures are logged and skipped.
    fn send_control_to_tokens(&mut self, tokens: &[Token], control: &ServerControl) {
        let kind = server_control_kind(control);
        let payload = encode_server_control(control);
        for token in tokens {
            if let Err(error) = self.queue_control_payload_to_token(*token, kind, &payload) {
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
        if payload.len() > MAX_CONTROL_PAYLOAD_BYTES {
            return Err(format!("{kind} exceeds control payload limit"));
        }
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let transport = client
            .control
            .as_mut()
            .ok_or_else(|| "missing control cipher".to_string())?;
        let sealed_len = transport.sealed_len(payload.len());
        if sealed_len > frame::MAX_FRAME_LEN {
            return Err(format!("{kind} exceeds frame limit"));
        }
        let out = client.write_buf.tail_mut();
        out.extend_from_slice(&(sealed_len as u32).to_le_bytes());
        if let Err(error) = transport.seal_next_into(CHANNEL_CONTROL, payload, out) {
            out.truncate(out.len() - frame::LENGTH_PREFIX_LEN);
            return Err(error.to_string());
        }
        kvlog::debug!(
            "server control queued",
            token = token.0,
            kind = kind,
            payload_size = payload.len(),
            encrypted_size = sealed_len,
            queued_bytes = client.write_buf.len()
        );
        self.write_client(token);
        // The flush above wrote as much as the socket accepted; what remains
        // is bounded by the peer draining its end. A control client past the
        // high-water mark is dropped rather than buffered further, the same
        // treatment `publish_video_frame` gives a stalled video subscriber.
        let over_water = self
            .clients
            .get(&token)
            .is_some_and(|client| client.write_buf.len() > CONTROL_WRITE_HIGH_WATER);
        if over_water {
            kvlog::warn!(
                "control client dropped for backpressure",
                token = token.0,
                kind
            );
            self.disconnect(token);
            return Err("control write buffer overflow".to_string());
        }
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
        let payload = encode_server_control(control);
        for token in tokens {
            if let Err(error) = self.queue_control_payload_to_token(token, kind, &payload) {
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
        if let Some(client) = self.clients.get_mut(&token) {
            let mut wrote = false;
            while !client.write_buf.is_empty() {
                match client.socket.write(client.write_buf.pending()) {
                    Ok(0) => break,
                    Ok(n) => {
                        client.write_buf.consume(n);
                        wrote = true;
                        kvlog::debug!(
                            "tcp client bytes written",
                            token = token.0,
                            size = n,
                            remaining = client.write_buf.len()
                        );
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        kvlog::warn!("tcp client write failed", token = token.0, error = %error);
                        disconnected = true;
                        break;
                    }
                }
            }
            if wrote {
                client.last_activity = Instant::now();
            }
            if client.is_closing() && client.write_buf.is_empty() {
                disconnected = true;
            }
        }
        if disconnected {
            self.disconnect(token);
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
            if client.write_buf.is_empty() || now >= deadline {
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
        let Some(mut client) = self.clients.remove(&token) else {
            return;
        };
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
        if self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.announced)
        {
            self.broadcast_presence(session_id, false);
        }
        if let Some(session) = self.sessions.remove(&session_id) {
            if let Some(secrets) = &session.secrets {
                self.media_key_to_session.remove(&secrets.media_recv.id);
            }
            if let Some(addr) = session.udp_addr
                && self.plaintext_addr_to_session.get(&addr) == Some(&session_id)
            {
                self.plaintext_addr_to_session.remove(&addr);
            }
        }
        self.remove_peer_links(session_id);
    }

    /// Drops any session still held for `user_id` under a connection other than
    /// `keep_token`, so a reconnecting user supersedes a session left behind by
    /// an ungraceful disconnect rather than coexisting with it. Without this, a
    /// stale session whose TCP close has not yet been observed lingers in the
    /// room as a ghost participant carrying its last voice status — a user who
    /// muted before dropping then reappears muted even after rejoining, because
    /// the client roster is keyed by user and the duplicate's stale status wins.
    fn supersede_existing_sessions(&mut self, user_id: UserId, keep_token: Token) {
        let stale = self
            .sessions
            .iter()
            .filter(|(_, session)| session.user_id == user_id && session.tcp_token != keep_token)
            .map(|(session_id, session)| (*session_id, session.tcp_token))
            .collect::<Vec<_>>();
        for (session_id, token) in stale {
            kvlog::info!(
                "superseding stale session on reconnect",
                user_id = user_id.0,
                stale_session_id = session_id.0,
                stale_token = token.0,
                keep_token = keep_token.0
            );
            // A live ghost connection is fully disconnected (deregistering its
            // socket); an orphaned session with no client is torn down directly.
            if self.clients.contains_key(&token) {
                self.disconnect(token);
            } else {
                self.teardown_session(session_id, "superseded by reconnecting user");
            }
        }
    }

    /// Backstop that drops media-key mappings whose session has gone away.
    /// `disconnect` already removes a session's key on the normal path, so this
    /// only catches leaks and runs at most once per [`MEDIA_SWEEP_INTERVAL`].
    fn sweep_stale_media_keys(&mut self, now: Instant) {
        if self
            .next_media_sweep_at
            .is_some_and(|deadline| now < deadline)
        {
            return;
        }
        self.next_media_sweep_at = Some(now + MEDIA_SWEEP_INTERVAL);
        let sessions = &self.sessions;
        let before = self.media_key_to_session.len();
        self.media_key_to_session
            .retain(|_, session_id| sessions.contains_key(session_id));
        let removed = before.saturating_sub(self.media_key_to_session.len());
        if removed > 0 {
            kvlog::warn!("stale media key mappings removed", removed);
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

    #[cfg(debug_assertions)]
    fn debug_assert_no_immediate_read_work(&self) {
        for (token, client) in &self.clients {
            let whole_frame_buffered =
                matches!(frame::parse_frame(client.read_buf.pending()), Ok(Some(_)));
            let can_process_now = !client.is_closing()
                && !self.relay_clogged_for_reader(*token)
                && (client.readiness.is_ready() || whole_frame_buffered);
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
        kvlog::info!(
            "bug report starting",
            session_id = session_id.0,
            report_id = report_id.0,
            logs_size
        );
        self.active_bug_reports.insert(
            (session_id, report_id),
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
        token: Token,
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
        let display_name = self
            .sessions
            .get(&session_id)
            .map(|session| session.display_name.clone())
            .unwrap_or_default();
        let saved = write_bug_report(&dir, &display_name, report_id, &report)?;
        kvlog::info!(
            "bug report saved",
            session_id = session_id.0,
            report_id = report_id.0,
            description = report.description.as_str(),
            logs = saved.as_str()
        );
        self.send_control_to_token(token, &ServerControl::BugReportSaved { report_id })
    }
}

/// Writes a bug report bundle as two files sharing a unique prefix: the
/// zstd-compressed logs (`.log.zst`, stored verbatim) and the metadata
/// (`.json`). Returns the logs path.
fn write_bug_report(
    dir: &str,
    display_name: &str,
    report_id: BugReportId,
    report: &ServerBugReport,
) -> Result<String, String> {
    let dir = std::path::Path::new(dir);
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("failed to create bug-report dir: {error}"))?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    let user = sanitize_file_name(if display_name.trim().is_empty() {
        "anon"
    } else {
        display_name
    });
    let base = format!("{timestamp}-{user}-{}", report_id.0);

    // Probe for a collision-free prefix using create_new on the logs file.
    for index in 0u64.. {
        let prefix = if index == 0 {
            base.clone()
        } else {
            format!("{base}-{index}")
        };
        let logs_path = dir.join(format!("{prefix}.log.zst"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&logs_path)
        {
            Ok(mut file) => {
                file.write_all(&report.logs)
                    .map_err(|error| format!("failed to write bug report logs: {error}"))?;
                let metadata_path = dir.join(format!("{prefix}.json"));
                std::fs::write(&metadata_path, report.metadata.as_bytes())
                    .map_err(|error| format!("failed to write bug report metadata: {error}"))?;
                return Ok(logs_path.display().to_string());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("failed to create bug report file: {error}")),
        }
    }
    unreachable!("u64 bug-report suffix space exhausted")
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
    control: Option<ControlTransport>,
    secrets: Option<SessionSecrets>,
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

/// Threshold above which [`WriteQueue::consume`] compacts the consumed prefix
/// away. Compaction additionally requires the prefix to be at least half the
/// buffer, keeping the memmove amortized against the bytes already written.
const WRITE_QUEUE_COMPACT_BYTES: usize = 64 * 1024;

/// Outbound byte queue drained from the front through a cursor rather than
/// `Vec::drain`, so a partial socket write against a deep backlog (an 8 MiB
/// video queue) does not memmove the remaining megabytes on every write.
struct WriteQueue {
    buf: Vec<u8>,
    start: usize,
}

impl WriteQueue {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
        }
    }

    /// Bytes queued but not yet written.
    fn pending(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn len(&self) -> usize {
        self.buf.len() - self.start
    }

    fn is_empty(&self) -> bool {
        self.start == self.buf.len()
    }

    /// The backing vector, for encoders that append to a `Vec`. Only appends
    /// are valid; bytes before the cursor are dead.
    fn tail_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Marks the next `n` pending bytes written, compacting the dead prefix
    /// once it is both large and the majority of the buffer.
    fn consume(&mut self, n: usize) {
        self.start += n;
        debug_assert!(self.start <= self.buf.len());
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start >= WRITE_QUEUE_COMPACT_BYTES && self.start >= self.len() {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }
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

/// One inbound video-connection record, extracted from the receive buffer by
/// [`video_conn_step`].
enum VideoStep {
    Handshake(Vec<u8>),
    Publish {
        stream_id: StreamId,
        frame: Arc<[u8]>,
        is_key: bool,
    },
    SubscriberChatter,
}

/// Extracts the next video record buffered on `client`. Handshake records are
/// small and copied out; streaming records decrypt in place in the receive
/// buffer, so a published frame's only copy is into the shared ring [`Arc`].
/// Returns `None` when no whole record is buffered yet.
fn video_conn_step(client: &mut ClientConn) -> Result<Option<VideoStep>, String> {
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
            let cipher = video
                .cipher
                .as_mut()
                .ok_or_else(|| "stream before auth".to_string())?;
            let record = &mut client.read_buf.pending_mut()[video::VIDEO_LENGTH_PREFIX_LEN..total];
            let plaintext = cipher
                .open_next_in_place(CHANNEL_VIDEO, record)
                .map_err(|error| error.to_string())?;
            let step = match role {
                VideoRole::Publisher => {
                    if plaintext.len() < video::VIDEO_FRAME_HEADER_LEN {
                        return Err("video frame is shorter than its header".to_string());
                    }
                    VideoStep::Publish {
                        stream_id,
                        frame: Arc::from(plaintext),
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
/// and `cipher` are filled once the clear [`VideoHello`] arrives, before the
/// sealed auth record is opened.
struct VideoConn {
    phase: VideoPhase,
    session_id: Option<SessionId>,
    stream_id: Option<StreamId>,
    role: Option<VideoRole>,
    cipher: Option<TransportCipher>,
}

impl VideoConn {
    fn new() -> Self {
        Self {
            phase: VideoPhase::AwaitHello,
            session_id: None,
            stream_id: None,
            role: None,
            cipher: None,
        }
    }
}

struct Session {
    user_id: UserId,
    display_name: String,
    identifier: String,
    tcp_token: Token,
    /// The room whose voice call this session is in, independent of which
    /// room's chat the client is reading.
    voice_room: Option<RoomId>,
    udp_addr: Option<SocketAddr>,
    secrets: Option<SessionSecrets>,
    media_send_counter: u64,
    media_recv_replay: AntiReplay,
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
}

impl Session {
    fn observe_udp_addr(&mut self, addr: SocketAddr) -> Option<SocketAddr> {
        let old_addr = self.udp_addr.replace(addr);
        if old_addr.is_some_and(|old| old != addr) {
            self.reported_server_rtt_ms = None;
            self.server_rtt_reported_at = None;
        }
        old_addr
    }

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
        FileContentEncoding::Zstd => original_size == 0 || wire_received != 0,
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
/// [`video::write_video_frame`] plaintext (13-byte header plus H.264), shared so
/// fan-out to many subscribers does not copy it.
struct VideoRingFrame {
    data: Arc<[u8]>,
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

/// Whether a client-supplied display name is acceptable: non-empty after
/// trimming, at most 64 bytes, and free of control characters, which must
/// never reach the persisted user registry.
fn valid_display_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.chars().any(char::is_control)
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
    "usage: chatt-server serve CONFIG_PATH | chatt-server init-config CONFIG_PATH | chatt-server invite USER".to_string()
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

fn is_interrupted_io_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Interrupted || error.raw_os_error() == Some(libc::EINTR)
}

fn is_transient_accept_error(error: &io::Error) -> bool {
    is_interrupted_io_error(error)
        || matches!(error.kind(), io::ErrorKind::ConnectionAborted)
        || matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

fn random_secret_hex(rng: &ring::rand::SystemRandom) -> Result<String, String> {
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes)
        .map_err(|_| "failed to generate invite secret".to_string())?;
    Ok(encode_hex(&bytes))
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
    payload: &media::VoicePayload,
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

fn audio_pop_should_log_packet(flags: u8, payload: &media::VoicePayload) -> bool {
    flags != 0 || matches!(payload, media::VoicePayload::Silence)
}

fn server_voice_payload_kind(payload: &media::VoicePayload) -> &'static str {
    match payload {
        media::VoicePayload::Opus(_) => "opus",
        media::VoicePayload::Silence => "silence",
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn write_bug_report_creates_paired_files() {
        let dir = std::env::temp_dir().join(format!("chatt-bug-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let report = ServerBugReport {
            description: "stuck mute".to_string(),
            metadata: "{\"version\":\"0.1.0\"}".to_string(),
            logs: vec![1, 2, 3, 4],
            expected: 4,
        };
        let logs_path = write_bug_report(
            dir.to_str().unwrap(),
            "Alice Tester",
            BugReportId(7),
            &report,
        )
        .unwrap();

        assert!(logs_path.ends_with(".log.zst"));
        let logs = std::fs::read(&logs_path).unwrap();
        assert_eq!(logs, vec![1, 2, 3, 4]);
        let metadata_path = logs_path.replace(".log.zst", ".json");
        let metadata = std::fs::read_to_string(&metadata_path).unwrap();
        assert_eq!(metadata, "{\"version\":\"0.1.0\"}");

        let _ = std::fs::remove_dir_all(&dir);
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
            display_name: format!("user-{}", user_id.0),
            identifier: format!("user-{}", user_id.0),
            tcp_token: token,
            voice_room,
            udp_addr: None,
            secrets: None,
            media_send_counter: 0,
            media_recv_replay: AntiReplay::new(),
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

    #[test]
    fn udp_address_change_clears_reported_server_rtt() {
        let mut session = test_session(UserId(9), Token(11), Some(RoomId(1)));
        let now = Instant::now();
        let first: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:2000".parse().unwrap();
        session.observe_udp_addr(first);
        session.report_server_rtt(Some(40), now);

        session.observe_udp_addr(first);
        assert_eq!(session.fresh_server_rtt(now), Some(40));

        session.observe_udp_addr(second);
        assert_eq!(session.fresh_server_rtt(now), None);
        assert_eq!(session.server_rtt_reported_at, None);
    }

    fn test_key(id: u32, byte: u8) -> KeyMaterial {
        KeyMaterial {
            id,
            bytes: [byte; KEY_LEN],
        }
    }

    fn test_secrets(media_recv_id: u32) -> SessionSecrets {
        SessionSecrets {
            control_send: test_key(1, 1),
            control_recv: test_key(2, 2),
            media_send: test_key(3, 3),
            media_recv: test_key(media_recv_id, 4),
        }
    }

    #[test]
    fn probe_socket_packet_does_not_clobber_media_addr() {
        // A NAT probe is deliberately sent from the client's single media
        // socket toward the probe port. Behind a symmetric NAT that flow gets
        // its own mapping, so the probe packet's source address is unreachable
        // from the server's main socket and must not become the session's
        // media address (nor wipe the RTT estimate).
        let mut server = test_server();
        let session_id = SessionId(1);
        let secrets = test_secrets(77);
        let client_media_key = secrets.media_recv.clone();
        let mut session = test_session(UserId(9), Token(11), None);
        session.secrets = Some(secrets);
        let media_addr: SocketAddr = "203.0.113.5:4000".parse().unwrap();
        let now = Instant::now();
        session.observe_udp_addr(media_addr);
        session.report_server_rtt(Some(40), now);
        server.sessions.insert(session_id, session);
        server.media_key_to_session.insert(77, session_id);

        let probe_src: SocketAddr = "203.0.113.5:5000".parse().unwrap();
        let packet = media::seal_media(
            &client_media_key,
            1,
            &MediaPayload::NatProbe {
                session_id,
                probe_id: 1,
            },
        )
        .unwrap();
        server.handle_udp_packet(1, probe_src, &packet).unwrap();

        let session = server.sessions.get(&session_id).unwrap();
        assert_eq!(
            session.udp_addr,
            Some(media_addr),
            "a probe-socket packet must not rebind the media address"
        );
        assert_eq!(
            session.fresh_server_rtt(now),
            Some(40),
            "a probe-socket packet must not reset the RTT estimate"
        );

        // The same packet arriving on the main media socket is the client
        // genuinely moving, and must still rebind.
        let packet = media::seal_media(
            &client_media_key,
            2,
            &MediaPayload::NatProbe {
                session_id,
                probe_id: 1,
            },
        )
        .unwrap();
        server.handle_udp_packet(0, probe_src, &packet).unwrap();
        let session = server.sessions.get(&session_id).unwrap();
        assert_eq!(session.udp_addr, Some(probe_src));
    }

    #[test]
    fn media_key_collision_rejects_new_session_and_keeps_survivor() {
        // A 32-bit media key id collision must not silently overwrite the live
        // session's mapping: `teardown_session` removes the shared key when
        // either session ends, permanently stranding the survivor's UDP.
        let mut server = test_server();
        let alice = UserConfig {
            id: UserId(1),
            name: "alice".to_string(),
            display_name: "Alice".to_string(),
            token_hash: String::new(),
        };
        let bob = UserConfig {
            id: UserId(2),
            name: "bob".to_string(),
            display_name: "Bob".to_string(),
            token_hash: String::new(),
        };
        let (conn, _alice_peer) = test_live_client();
        server.clients.insert(Token(11), conn);
        server.clients.get_mut(&Token(11)).unwrap().secrets = Some(test_secrets(77));
        let (conn, _bob_peer) = test_live_client();
        server.clients.insert(Token(22), conn);
        server.clients.get_mut(&Token(22)).unwrap().secrets = Some(test_secrets(77));

        server
            .establish_session(Token(11), &alice, false, 0, false, None)
            .unwrap();
        let alice_session = server.clients.get(&Token(11)).unwrap().session_id.unwrap();
        assert_eq!(server.media_key_to_session.get(&77), Some(&alice_session));

        let result = server.establish_session(Token(22), &bob, false, 0, false, None);
        assert!(
            result.is_err(),
            "a colliding media key must reject the new session"
        );
        assert_eq!(
            server.media_key_to_session.get(&77),
            Some(&alice_session),
            "the live session's mapping must survive the collision"
        );
    }

    #[test]
    fn media_key_of_dead_session_is_reclaimed() {
        // A mapping left behind by a torn-down session (the sweep backstop has
        // not run yet) must not block a new session from taking the key id.
        let mut server = test_server();
        server.media_key_to_session.insert(77, SessionId(999));
        let carol = UserConfig {
            id: UserId(3),
            name: "carol".to_string(),
            display_name: "Carol".to_string(),
            token_hash: String::new(),
        };
        let (conn, _carol_peer) = test_live_client();
        server.clients.insert(Token(33), conn);
        server.clients.get_mut(&Token(33)).unwrap().secrets = Some(test_secrets(77));

        server
            .establish_session(Token(33), &carol, false, 0, false, None)
            .unwrap();
        let carol_session = server.clients.get(&Token(33)).unwrap().session_id.unwrap();
        assert_eq!(server.media_key_to_session.get(&77), Some(&carol_session));
    }

    #[test]
    fn send_udp_payload_seals_decodable_packets_with_incrementing_counters() {
        // Pins the relay's wire bytes: whatever sealing path the server uses
        // internally, the client must keep opening consecutive packets with the
        // shared media key and a fresh anti-replay window.
        let mut server = test_server();
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let session_id = SessionId(1);
        let secrets = test_secrets(77);
        let server_send_key = secrets.media_send.clone();
        let mut session = test_session(UserId(9), Token(11), None);
        session.secrets = Some(secrets);
        session.udp_addr = Some(receiver.local_addr().unwrap());
        server.sessions.insert(session_id, session);

        let mut replay = AntiReplay::new();
        let mut buf = [0u8; 2048];
        for nonce in [7u64, 8, 9] {
            server.send_udp_payload(session_id, &MediaPayload::Pong { nonce });
            let (len, _) = receiver.recv_from(&mut buf).unwrap();
            let (_, payload) =
                media::open_media(&server_send_key, &mut replay, &buf[..len]).unwrap();
            assert_eq!(payload, MediaPayload::Pong { nonce });
        }
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
        let denied = read_until(&mut peer, |control| {
            matches!(control, ServerControl::VoiceJoinFailed { .. })
        });
        server.join_voice(outsider, RoomId(99));
        let missing = read_until(&mut peer, |control| {
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
    fn reconnect_supersedes_stale_muted_session() {
        // A user who muted, then dropped without a clean close, leaves a ghost
        // session in the room carrying its muted voice status. Reconnecting must
        // evict that ghost so it cannot reappear as a stale muted participant.
        let mut server = test_server();
        let room_id = RoomId(1);
        let user_id = UserId(9);
        let stale_id = SessionId(1);
        server
            .rooms
            .insert(room_id, test_room(room_id, &[stale_id]));
        let mut stale = test_session(user_id, Token(11), Some(room_id));
        stale.voice_status = control::ParticipantVoiceStatus {
            muted: true,
            deafened: false,
        };
        server.sessions.insert(stale_id, stale);

        // The reconnecting connection holds no session yet (keep_token differs).
        server.supersede_existing_sessions(user_id, Token(22));

        assert!(
            !server.sessions.contains_key(&stale_id),
            "stale session should be evicted on reconnect"
        );
        assert!(
            server
                .user_summaries()
                .iter()
                .all(|user| user.user_id != user_id || !user.online),
            "the ghost user must not linger online in the directory"
        );
    }

    #[test]
    fn user_summaries_hides_unannounced_sessions() {
        let mut server = test_server();
        server.users.users.push(UserConfig {
            id: UserId(9),
            name: "bob".to_string(),
            display_name: "Bob".to_string(),
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

    #[test]
    fn supersede_keeps_sessions_for_other_users() {
        let mut server = test_server();
        let room_id = RoomId(1);
        let other_id = SessionId(2);
        server
            .rooms
            .insert(room_id, test_room(room_id, &[other_id]));
        server
            .sessions
            .insert(other_id, test_session(UserId(5), Token(33), Some(room_id)));

        server.supersede_existing_sessions(UserId(9), Token(22));

        assert!(
            server.sessions.contains_key(&other_id),
            "another user's session must be left untouched"
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

    fn read_until(
        peer: &mut std::net::TcpStream,
        accept: impl Fn(&ServerControl) -> bool,
    ) -> ServerControl {
        for _ in 0..64 {
            let control = read_plaintext_server_control(peer);
            if accept(&control) {
                return control;
            }
        }
        panic!("expected control was not received");
    }

    fn read_until_history_chunk(peer: &mut std::net::TcpStream) -> history::HistoryChunk {
        for _ in 0..64 {
            let payload = read_plaintext_server_payload(peer);
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
            let control = read_until(peer, |control| {
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
        let denied = read_until(&mut outsider_peer, |control| {
            matches!(control, ServerControl::Error { .. })
        });

        server.fetch_history(outsider, RoomId(99), None, 10);
        let missing = read_until(&mut outsider_peer, |control| {
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
    fn private_room_chat_reaches_members_only() {
        let mut server = test_server();
        let secret = RoomId(3);
        server
            .rooms
            .insert(secret, private_room(secret, &[UserId(1)]));
        let member = SessionId(1);
        let outsider = SessionId(2);
        let mut member_peer = live_user(&mut server, Token(11), member, UserId(1));
        let mut outsider_peer = live_user(&mut server, Token(22), outsider, UserId(2));

        server
            .send_chat(member, secret, "members only".to_string())
            .unwrap();

        let control = read_until(&mut member_peer, |control| {
            matches!(control, ServerControl::Chat { .. })
        });
        let ServerControl::Chat { message } = control else {
            unreachable!();
        };
        assert_eq!(message.body, "members only");
        assert_no_control(&mut outsider_peer);
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
        let upserted = read_until(&mut requester_peer, |control| {
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
        let opened = read_until(&mut requester_peer, |control| {
            matches!(control, ServerControl::DmOpened { .. })
        });
        let ServerControl::DmOpened { room_id, peer } = opened else {
            unreachable!();
        };
        assert_eq!(room_id, room.room_id);
        assert_eq!(peer, UserId(2));
        read_until(&mut endpoint_peer, |control| {
            matches!(control, ServerControl::RoomUpserted { .. })
        });

        server.open_dm(peer_session, UserId(1));
        let reopened = read_until(&mut endpoint_peer, |control| {
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
    fn dm_open_with_self_is_rejected() {
        let mut server = test_server();
        let requester = SessionId(1);
        let mut requester_peer = live_user(&mut server, Token(11), requester, UserId(1));

        server.open_dm(requester, UserId(1));

        let control = read_until(&mut requester_peer, |control| {
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

        let chunk = read_until_history_chunk(&mut peer);
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
        let chunk = read_until_history_chunk(&mut peer);
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
            server.send_chat(session, RoomId(1), body.clone()).unwrap();
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
            let payload = read_plaintext_server_payload(&mut peer);
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
            server.drain_history_replies();
            peer.set_read_timeout(Some(Duration::from_millis(20)))
                .expect("set read timeout");
            let mut len = [0u8; 4];
            match peer.peek(&mut len) {
                Ok(4) => {
                    let payload = read_plaintext_server_payload(peer);
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
        let control = read_until(&mut talker_peer, |control| {
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
            read_plaintext_server_control(&mut joining_peer)
        else {
            panic!("voice confirmation must be the first control");
        };
        assert_eq!(user_id, UserId(2));
        let ServerControl::VoiceStarted { user_id, .. } =
            read_plaintext_server_control(&mut joining_peer)
        else {
            panic!("existing voice member must follow local confirmation");
        };
        assert_eq!(user_id, UserId(1));
    }

    #[test]
    fn join_voice_broadcasts_joiner_stored_status() {
        fn read_voice_status(
            peer: &mut std::net::TcpStream,
        ) -> (UserId, control::ParticipantVoiceStatus) {
            let control = read_until(peer, |control| {
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
            statuses.push(read_voice_status(&mut observer_peer));
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

        let available = read_until(&mut viewer_peer, |control| {
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
        let control = read_until(&mut peer, |control| {
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
        read_until(&mut joiner_peer, |control| {
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

        let available = read_until(&mut viewer_peer, |control| {
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
        read_until(&mut watcher_peer, |control| {
            matches!(control, ServerControl::VoiceStarted { .. })
        });
        server.leave_voice(talker, None);

        let stopped = read_until(&mut watcher_peer, |control| {
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

        let control = read_until(&mut outsider_peer, |control| {
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
            data: Arc::from(bytes),
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
    fn write_queue_consumes_through_cursor_and_compacts() {
        let mut queue = WriteQueue::new();
        queue.tail_mut().extend_from_slice(b"abcdef");
        queue.consume(2);
        assert_eq!(queue.pending(), b"cdef");
        queue.tail_mut().extend_from_slice(b"gh");
        assert_eq!(queue.pending(), b"cdefgh");
        queue.consume(6);
        assert!(queue.is_empty());

        let mut queue = WriteQueue::new();
        queue
            .tail_mut()
            .extend_from_slice(&vec![7u8; WRITE_QUEUE_COMPACT_BYTES + 16]);
        queue.consume(WRITE_QUEUE_COMPACT_BYTES);
        assert_eq!(queue.pending(), &[7u8; 16]);
        assert_eq!(queue.start, 0);
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

        let outcome =
            read_socket_into_buffer(&reader, &mut read_buf, &mut readiness, 1, 1).unwrap();

        assert!(outcome.hit_budget);
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

        let outcome =
            read_socket_into_buffer(&reader, &mut read_buf, &mut readiness, 1, 1).unwrap();
        assert!(outcome.hit_budget);
        assert!(readiness.is_ready());

        read_buf.consume(read_buf.len());
        let outcome =
            read_socket_into_buffer(&reader, &mut read_buf, &mut readiness, 1, 1).unwrap();

        assert_eq!(outcome, ReadPumpOutcome::default());
        assert!(!readiness.is_ready());
    }

    #[test]
    fn loop_work_queues_reads_and_forces_zero_timeout() {
        let mut work = LoopWork::default();
        assert_eq!(work.poll_timeout(POLL_TIMEOUT), POLL_TIMEOUT);

        work.queue_client_read(Token(7));
        work.queue_client_read(Token(7));

        assert_eq!(work.poll_timeout(POLL_TIMEOUT), Duration::ZERO);
        assert_eq!(work.queued_client_reads(), vec![Token(7)]);
        assert_eq!(work.take_tasks(), vec![LoopTask::ClientRead(Token(7))]);
        assert_eq!(work.poll_timeout(POLL_TIMEOUT), POLL_TIMEOUT);
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
        assert_eq!(stream.ring_bytes, key.len());
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
        );

        assert!(result.is_ok());
        assert!(
            server.clients.is_empty(),
            "the draining uploader flushes and disconnects during the accept send"
        );
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
            )
            .unwrap();
        assert!(server.reserved_file_names.contains("report.pdf"));

        server.cancel_file_upload(session_id, FileTransferId(1), "client canceled".to_string());

        assert!(server.active_uploads.is_empty());
        assert!(server.reserved_file_names.is_empty());
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
            name: "alice-internal".to_string(),
            display_name: "Alice".to_string(),
            token_hash: hash_secret(token_secret),
        }];

        // The trailing Authenticated send fails because no client socket is
        // registered, but the session is established before that send, so the
        // durable side effect proves the token-only lookup succeeded. The
        // display name matches the stored one, so no config save is attempted.
        let _ = server.authenticate_client(Token(1), "Alice", token_secret, true, 0);

        let session = server
            .sessions
            .values()
            .find(|session| session.tcp_token == Token(1))
            .expect("session established by token");
        assert_eq!(session.identifier, "alice-internal");
        assert_eq!(session.display_name, "Alice");
        assert_eq!(session.user_id, UserId(7));
    }

    #[test]
    fn authenticate_rejects_unknown_token() {
        let mut server = test_server();
        server.users.users = vec![UserConfig {
            id: UserId(7),
            name: "alice-internal".to_string(),
            display_name: "Alice".to_string(),
            token_hash: hash_secret("alice-client-generated-token-with-at-least-32-bytes"),
        }];

        let _ = server.authenticate_client(Token(1), "Mallory", "wrong-token", true, 0);

        assert!(server.sessions.is_empty());
    }

    fn open_pair_test_server() -> Server {
        let mut server = test_server();
        server.config.security.public = true;
        server
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

        let _ = server.open_pair_client(Token(1), "Zoe", "", "", true, 0);

        let session = session_for(&server, Token(1)).expect("session established");
        assert_eq!(session.user_id, UserId(config::FIRST_DYNAMIC_USER_ID));
        assert_eq!(session.display_name, "Zoe");
        assert_eq!(
            session.identifier,
            config::FIRST_DYNAMIC_USER_ID.to_string()
        );
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

        let _ = server.open_pair_client(Token(3), "Zoe", "hunter2", "", true, 0);
        assert!(session_for(&server, Token(3)).is_some());
    }

    #[test]
    fn open_pair_preserves_identity_on_re_pair() {
        let mut server = open_pair_test_server();
        server.config.security.password_hash = Some(hash_secret("hunter2"));
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

        let _ = server.open_pair_client(Token(1), "Zoe", "hunter2", &existing, true, 0);

        let session = session_for(&server, Token(1)).expect("session established");
        assert_eq!(session.user_id, UserId(config::FIRST_DYNAMIC_USER_ID + 5));
    }

    #[test]
    fn open_pair_without_password_does_not_preserve_stale_identity() {
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

        let _ = server.open_pair_client(Token(1), "Zoe", "", &existing, true, 0);

        let session = session_for(&server, Token(1)).expect("session established");
        assert_eq!(session.user_id, UserId(config::FIRST_DYNAMIC_USER_ID));
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

        let response = read_plaintext_server_control(&mut peer);
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

        server
            .open_pair_client(token, "Zoe", "", "", true, 0)
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
    fn authenticate_accepts_issued_dynamic_token() {
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
            server
                .open_pair_client(token, "Zoe", "", "", true, 0)
                .unwrap();
            assert!(session_for(&server, token).is_some());
            peers.push(peer);
        }
        let blocked = Token(200);
        let (conn, peer) = test_live_client();
        server.clients.insert(blocked, conn);
        peers.push(peer);

        server
            .open_pair_client(blocked, "Zoe", "", "", true, 0)
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
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut wrong_peer)
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
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut blocked_peer)
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
            name: "dana".to_string(),
            display_name: "old".to_string(),
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
        let _ = server.pair_client(Token(2), "Dana", code, new_token, true, 0);

        let user = server
            .users
            .users
            .iter()
            .find(|user| user.name == "dana")
            .expect("user exists");
        assert!(verify_secret_hash(&user.token_hash, new_token));
        assert_eq!(user.display_name, "Dana");
        assert!(server.invites.is_empty());
        assert!(
            server
                .sessions
                .values()
                .any(|session| session.identifier == "dana")
        );
    }

    /// Builds a live `ClientConn` over a connected loopback socket with a
    /// plaintext control transport, so server send paths run without a full
    /// crypto handshake. The returned peer socket must be kept alive for the
    /// test so the connection stays open.
    fn test_live_client() -> (ClientConn, std::net::TcpStream) {
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
            control: Some(ControlTransport::plaintext()),
            secrets: None,
            session_id: None,
            user_id: None,
            close: Close::Open,
            created_at: now,
            last_activity: now,
        };
        (conn, peer)
    }

    fn read_plaintext_server_payload(peer: &mut std::net::TcpStream) -> Vec<u8> {
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let mut len = [0u8; 4];
        peer.read_exact(&mut len).expect("read frame length");
        let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
        peer.read_exact(&mut frame).expect("read frame body");
        frame
    }

    fn read_plaintext_server_control(peer: &mut std::net::TcpStream) -> ServerControl {
        let frame = read_plaintext_server_payload(peer);
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
            name: "gwen".to_string(),
            display_name: "old".to_string(),
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
            .find(|session| session.identifier == "gwen")
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
                name: "erin".to_string(),
                display_name: "Erin".to_string(),
                token_hash: hash_secret(new_token),
            },
            UserConfig {
                id: UserId(2),
                name: "frank".to_string(),
                display_name: "old".to_string(),
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
            .find(|user| user.name == "frank")
            .expect("frank exists");
        assert!(frank.token_hash.is_empty());
        assert!(!server.invites.is_empty());
        assert!(server.sessions.is_empty());
    }

    #[test]
    fn pairing_rejects_control_character_display_name() {
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

        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut peer) else {
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

        let _ = std::fs::remove_file(&blocker);
        let ServerControl::Error { code, .. } = read_plaintext_server_control(&mut peer) else {
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

        let _ = server.pair_client(
            Token(2),
            "Hana",
            code,
            "hana-client-generated-token-with-at-least-32-bytes",
            true,
            0,
        );

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
                .any(|session| session.identifier == "hana")
        );
    }

    #[test]
    fn accept_error_classification_is_transient_for_fd_pressure() {
        assert!(is_transient_accept_error(&io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(is_transient_accept_error(&io::Error::from_raw_os_error(
            libc::ENFILE
        )));
        assert!(is_transient_accept_error(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(!is_transient_accept_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }
}
