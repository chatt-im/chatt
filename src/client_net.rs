use hashbrown::HashMap;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream as StdTcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chatt_p2p::{
    Action as P2pAction, AgentConfig as P2pAgentConfig, Candidate, CandidateKind, IceRole,
    NatClassifier, NatKind, ReflexiveObservation, RestartPortPolicy, TraversalAgent,
    interfaces::{InterfaceSnapshot, host_candidates_with_metadata},
    socket::{UdpSocketOptions, bind_udp_socket, is_ignorable_udp_error},
    stun::{StunMessage, is_stun_message},
};
use mio::{
    Events, Interest, Poll, Token,
    net::{TcpStream, UdpSocket},
};
use ring::rand::SecureRandom;
use rpc::{
    control::{
        ChatMessage, ClientControl, DEFAULT_FILE_SIZE_LIMIT_BYTES, ERROR_AUTH_REJECTED,
        ERROR_PAIRING_CODE_MISMATCH, ERROR_PAIRING_INVALID_REQUEST, ERROR_PAIRING_NOT_ACTIVE,
        FileMetadata, MAX_FILE_CHUNK_BYTES, MAX_FILE_NAME_BYTES, P2pCandidate, P2pCandidateKind,
        P2pKey, P2pNatKind, P2pPeerInfo, P2pRole, ParticipantInfo, RoomInfo, ServerControl,
        decode_server_control, decode_server_hello, encode_client_control, encode_client_hello,
    },
    crypto::{
        AntiReplay, CHANNEL_CONTROL, ControlTransport, HandshakeMode, KEY_LEN, KeyMaterial,
        SessionSecrets, complete_client_transport_handshake, dev_server_public_key,
        ed25519_public_key_from_hex, generate_client_hello,
    },
    frame,
    ids::{FileTransferId, RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload},
};

use crate::audio::{LiveEncoderProfile, LivePlaybackFeedback, LocalVoiceFrame, RemoteVoicePacket};

const TCP: Token = Token(0);
const UDP: Token = Token(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(20);
const INTERFACE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_QUEUED_FILE_BYTES: usize = 128 * 1024;
const MAX_FILE_CHUNKS_PER_TICK: usize = 4;
const FILE_PROGRESS_STEP_BYTES: u64 = 1024 * 1024;
const ENCODER_FEEDBACK_ALPHA: f32 = 0.35;
const ENCODER_PROFILE_HOLD: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub tcp_addr: String,
    pub udp_addr: String,
    pub udp_probe_addr: Option<String>,
    pub user: String,
    pub display_name: String,
    pub token: String,
    pub server_public_key: Option<String>,
    pub room_id: RoomId,
    pub file_receive_dir: Option<PathBuf>,
    pub max_upload_bytes: u64,
    pub max_receive_bytes: u64,
}

#[derive(Debug)]
pub enum NetworkCommand {
    SendChat(String),
    UploadFile(PathBuf),
    LocalVoicePacket(LocalVoiceFrame),
    SequencedLocalVoicePacket {
        sequence: u32,
        frame: LocalVoiceFrame,
    },
    PlaybackFeedback(LivePlaybackFeedback),
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum NetworkEvent {
    Connected,
    Authenticated {
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
    },
    RoomJoined {
        room_id: RoomId,
        history: Vec<ChatMessage>,
        participants: Vec<ParticipantInfo>,
    },
    Chat(ChatMessage),
    Presence {
        room_id: RoomId,
        participant: ParticipantInfo,
        online: bool,
    },
    VoiceStarted {
        user_id: UserId,
        stream_id: StreamId,
    },
    VoiceStopped {
        user_id: UserId,
        stream_id: StreamId,
    },
    PeerTransport {
        user_id: UserId,
        direct: bool,
    },
    VoicePacket(RemoteVoicePacket),
    PlaybackFeedback(LivePlaybackFeedback),
    EncoderProfileChanged(LiveEncoderProfile),
    Status(String),
    Error(String),
    AuthFailed(String),
    PairingSucceeded,
    PairingFailed(String),
    ReconnectScheduled {
        retry_in: Duration,
        reason: String,
    },
    Disconnected,
}

pub struct NetworkClient {
    tx: Sender<NetworkCommand>,
    worker: Option<JoinHandle<()>>,
}

impl NetworkClient {
    pub fn spawn(config: ClientConfig, events: Sender<NetworkEvent>) -> Self {
        kvlog::info!(
            "network client spawning",
            user = config.user.as_str(),
            tcp_addr = %config.tcp_addr,
            udp_addr = %config.udp_addr,
            room_id = config.room_id.0
        );
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || run_worker(config, events, rx));
        Self {
            tx,
            worker: Some(worker),
        }
    }

    pub fn sender(&self) -> Sender<NetworkCommand> {
        self.tx.clone()
    }

    pub fn send(&self, command: NetworkCommand) {
        let _ = self.tx.send(command);
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    pub fn shutdown(&mut self) {
        self.stop_inner();
    }

    #[cfg(test)]
    pub(crate) fn from_parts_for_test(tx: Sender<NetworkCommand>) -> Self {
        Self { tx, worker: None }
    }

    fn stop_inner(&mut self) {
        let _ = self.tx.send(NetworkCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for NetworkClient {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

pub fn spawn_pair_once(
    config: ClientConfig,
    pairing_code: String,
    events: Sender<NetworkEvent>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let result = pair_once(&config, pairing_code);
        let event = match result {
            Ok(()) => NetworkEvent::PairingSucceeded,
            Err(error) => NetworkEvent::PairingFailed(error),
        };
        let _ = events.send(event);
    })
}

fn run_worker(
    config: ClientConfig,
    events: Sender<NetworkEvent>,
    commands: Receiver<NetworkCommand>,
) {
    kvlog::info!(
        "network worker starting",
        user = config.user.as_str(),
        tcp_addr = %config.tcp_addr,
        udp_addr = %config.udp_addr,
        room_id = config.room_id.0
    );
    let mut reconnect = ReconnectSchedule::new();
    loop {
        match run_worker_inner(&config, &events, &commands) {
            SessionEnd::Shutdown => break,
            SessionEnd::ConnectFailed(reason) => {
                kvlog::warn!(
                    "network connection attempt failed",
                    reason = reason.as_str()
                );
                if schedule_reconnect(&events, &commands, &mut reconnect, &reason).is_shutdown() {
                    break;
                }
            }
            SessionEnd::Disconnected(reason) => {
                reconnect.reset();
                kvlog::warn!("network session disconnected", reason = reason.as_str());
                if schedule_reconnect(&events, &commands, &mut reconnect, &reason).is_shutdown() {
                    break;
                }
            }
            SessionEnd::AuthFailed(reason) => {
                kvlog::warn!("network auth failed", reason = reason.as_str());
                let _ = events.send(NetworkEvent::AuthFailed(reason));
                break;
            }
        }
    }
    kvlog::info!("network worker stopped");
}

fn run_worker_inner(
    config: &ClientConfig,
    events: &Sender<NetworkEvent>,
    commands: &Receiver<NetworkCommand>,
) -> SessionEnd {
    let (std_tcp, control, secrets) = match connect_and_handshake(config) {
        Ok(value) => value,
        Err(error) => return SessionEnd::ConnectFailed(error),
    };
    let server_udp_addr = match resolve_endpoint(&config.udp_addr) {
        Ok(addr) => addr,
        Err(error) => return SessionEnd::ConnectFailed(format!("invalid UDP endpoint: {error}")),
    };
    let server_udp_probe_addr = match config.udp_probe_addr.as_deref() {
        Some(addr) => match resolve_endpoint(addr) {
            Ok(addr) => Some(addr),
            Err(error) => {
                return SessionEnd::ConnectFailed(format!("invalid UDP probe endpoint: {error}"));
            }
        },
        None => None,
    };
    let std_udp = match bind_udp_socket(
        if server_udp_addr.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        },
        UdpSocketOptions::default(),
    ) {
        Ok(socket) => socket,
        Err(error) => {
            return SessionEnd::ConnectFailed(format!("failed to bind UDP socket: {error}"));
        }
    };
    let udp_local_addr = match std_udp.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            return SessionEnd::ConnectFailed(format!(
                "failed to read UDP socket address: {error}"
            ));
        }
    };
    kvlog::info!("udp socket bound", addr = %udp_local_addr);
    if let Err(error) = std_tcp.set_nonblocking(true) {
        return SessionEnd::ConnectFailed(format!("failed to make TCP nonblocking: {error}"));
    }
    if let Err(error) = std_udp.set_nonblocking(true) {
        return SessionEnd::ConnectFailed(format!("failed to make UDP nonblocking: {error}"));
    }

    let mut worker = WorkerState {
        config: config.clone(),
        events: events.clone(),
        tcp: TcpStream::from_std(std_tcp),
        udp: UdpSocket::from_std(std_udp),
        udp_local_addr,
        server_udp_addr,
        server_udp_probe_addr,
        read_buf: Vec::new(),
        write_buf: Vec::new(),
        control,
        secrets,
        session_id: None,
        user_id: None,
        room_id: None,
        active_stream: None,
        local_sequence: 0,
        media_send_counter: 0,
        media_recv_replay: AntiReplay::new(),
        p2p_generation: 1,
        p2p_tie_breaker: random_u64().unwrap_or(1),
        p2p_nat: configured_nat_kind(),
        p2p_nat_classifier: NatClassifier::new(),
        p2p_reflexive_addr: None,
        p2p_candidates: Vec::new(),
        p2p_peers: HashMap::new(),
        p2p_stream_owners: HashMap::new(),
        encoder_feedback: EncoderFeedbackController::new(),
        restart_port_policy: RestartPortPolicy::default(),
        udp_rebind_requested: false,
        interface_snapshot: InterfaceSnapshot::capture().ok(),
        next_interface_poll: Instant::now() + INTERFACE_POLL_INTERVAL,
        next_file_transfer: 1,
        outgoing_uploads: VecDeque::new(),
        incoming_files: HashMap::new(),
        shutdown: false,
        disconnect_reason: None,
        auth_failure: None,
    };

    let mut poll = match Poll::new() {
        Ok(poll) => poll,
        Err(error) => return SessionEnd::ConnectFailed(format!("failed to create poll: {error}")),
    };
    if let Err(error) = poll.registry().register(
        &mut worker.tcp,
        TCP,
        Interest::READABLE | Interest::WRITABLE,
    ) {
        return SessionEnd::ConnectFailed(format!("failed to register TCP socket: {error}"));
    }
    if let Err(error) = poll
        .registry()
        .register(&mut worker.udp, UDP, Interest::READABLE)
    {
        return SessionEnd::ConnectFailed(format!("failed to register UDP socket: {error}"));
    }

    let auth_control = ClientControl::Authenticate {
        user: worker.config.user.clone(),
        token: worker.config.token.clone(),
        receive_files: worker.config.file_receive_dir.is_some(),
        file_receive_limit_bytes: worker
            .config
            .max_receive_bytes
            .min(DEFAULT_FILE_SIZE_LIMIT_BYTES),
    };
    if let Err(error) = worker.queue_control(auth_control) {
        return SessionEnd::Disconnected(error);
    }
    kvlog::info!("auth control queued", user = worker.config.user.as_str());
    let _ = worker.events.send(NetworkEvent::Connected);

    let mut poll_events = Events::with_capacity(128);
    while !worker.shutdown {
        if let Err(error) = poll.poll(&mut poll_events, Some(POLL_TIMEOUT)) {
            return SessionEnd::Disconnected(format!("network poll failed: {error}"));
        }
        for event in poll_events.iter() {
            match event.token() {
                TCP => {
                    if event.is_readable() {
                        if let Err(error) = worker.read_tcp() {
                            return SessionEnd::Disconnected(error);
                        }
                    }
                    if event.is_writable() {
                        if let Err(error) = worker.write_tcp() {
                            return SessionEnd::Disconnected(error);
                        }
                    }
                }
                UDP => worker.read_udp(),
                _ => {}
            }
        }

        loop {
            match commands.try_recv() {
                Ok(command) => {
                    if let Err(error) = worker.handle_command(command) {
                        return SessionEnd::Disconnected(error);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    worker.shutdown = true;
                    break;
                }
            }
        }
        if let Err(error) = worker.poll_uploads() {
            return SessionEnd::Disconnected(error);
        }
        let now = Instant::now();
        worker.poll_interfaces(now);
        if worker.udp_rebind_requested {
            if let Err(error) = worker.rebind_udp_socket(&mut poll) {
                return SessionEnd::Disconnected(error);
            }
        }
        worker.poll_p2p(now);
    }
    if let Some(reason) = worker.auth_failure.take() {
        SessionEnd::AuthFailed(reason)
    } else if let Some(reason) = worker.disconnect_reason.take() {
        SessionEnd::Disconnected(reason)
    } else {
        kvlog::info!("network worker shutdown requested");
        SessionEnd::Shutdown
    }
}

enum SessionEnd {
    Shutdown,
    ConnectFailed(String),
    Disconnected(String),
    AuthFailed(String),
}

#[derive(Clone, Copy, Debug, Default)]
struct ReconnectSchedule {
    attempts: u32,
}

impl ReconnectSchedule {
    fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }

    fn next_delay(&mut self) -> Duration {
        self.attempts = self.attempts.saturating_add(1);
        match self.attempts {
            1..=5 => Duration::from_secs(1),
            6..=10 => Duration::from_secs(2),
            _ => Duration::from_secs(5),
        }
    }
}

enum RetryWait {
    Retry,
    Shutdown,
}

impl RetryWait {
    fn is_shutdown(&self) -> bool {
        matches!(self, Self::Shutdown)
    }
}

fn schedule_reconnect(
    events: &Sender<NetworkEvent>,
    commands: &Receiver<NetworkCommand>,
    reconnect: &mut ReconnectSchedule,
    reason: &str,
) -> RetryWait {
    let delay = reconnect.next_delay();
    kvlog::info!(
        "network reconnect scheduled",
        delay_ms = delay.as_millis() as u64,
        reason
    );
    let _ = events.send(NetworkEvent::ReconnectScheduled {
        retry_in: delay,
        reason: reason.to_string(),
    });
    wait_for_reconnect(commands, delay)
}

fn wait_for_reconnect(commands: &Receiver<NetworkCommand>, delay: Duration) -> RetryWait {
    let deadline = Instant::now() + delay;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return RetryWait::Retry;
        };
        match commands.recv_timeout(remaining) {
            Ok(NetworkCommand::Shutdown) => {
                kvlog::info!("shutdown command handling while disconnected");
                return RetryWait::Shutdown;
            }
            Ok(command) => {
                if !matches!(
                    command,
                    NetworkCommand::LocalVoicePacket(_)
                        | NetworkCommand::SequencedLocalVoicePacket { .. }
                ) {
                    kvlog::info!(
                        "network command dropped while disconnected",
                        kind = network_command_kind(&command)
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => return RetryWait::Retry,
            Err(RecvTimeoutError::Disconnected) => return RetryWait::Shutdown,
        }
    }
}

fn connect_and_handshake(
    config: &ClientConfig,
) -> Result<(StdTcpStream, ControlTransport, Option<SessionSecrets>), String> {
    kvlog::info!(
        "tcp connecting",
        tcp_addr = %config.tcp_addr,
        user = config.user.as_str()
    );
    let mut stream = StdTcpStream::connect(config.tcp_addr.as_str())
        .map_err(|error| format!("failed to connect to server: {error}"))?;
    stream
        .set_nodelay(true)
        .map_err(|error| format!("failed to set TCP_NODELAY: {error}"))?;
    let rng = ring::rand::SystemRandom::new();
    let client = generate_client_hello(&rng).map_err(|error| error.to_string())?;
    let hello = encode_client_hello(&client.hello);
    let mut framed = Vec::new();
    frame::encode_frame(&hello, &mut framed).map_err(|error| error.to_string())?;
    kvlog::info!(
        "client hello sending",
        hello_size = hello.len(),
        frame_size = framed.len()
    );
    stream
        .write_all(&framed)
        .map_err(|error| format!("failed to write client hello: {error}"))?;
    let response = read_blocking_frame(&mut stream)
        .map_err(|error| format!("failed to read server hello: {error}"))?;
    let server_hello = decode_server_hello(&response)?;
    let pinned_server_public_key = pinned_server_public_key(config)?;
    let mode =
        complete_client_transport_handshake(client, &server_hello, &pinned_server_public_key)
            .map_err(|error| error.to_string())?;
    let (control, secrets) = match mode {
        HandshakeMode::Encrypted(secrets) => {
            let control = ControlTransport::encrypted(
                secrets.control_send.clone(),
                secrets.control_recv.clone(),
            );
            kvlog::info!(
                "tcp handshake completed",
                encryption = true,
                control_send_key_id = secrets.control_send.id,
                control_recv_key_id = secrets.control_recv.id,
                media_send_key_id = secrets.media_send.id,
                media_recv_key_id = secrets.media_recv.id
            );
            (control, Some(secrets))
        }
        HandshakeMode::Plaintext => {
            kvlog::info!("tcp handshake completed", encryption = false);
            (ControlTransport::plaintext(), None)
        }
    };
    Ok((stream, control, secrets))
}

fn pair_once(config: &ClientConfig, pairing_code: String) -> Result<(), String> {
    let (mut stream, mut control, _secrets) = connect_and_handshake(config)?;
    write_blocking_control(
        &mut stream,
        &mut control,
        ClientControl::Pair {
            user: config.user.clone(),
            display_name: config.display_name.clone(),
            pairing_code,
            token: config.token.clone(),
            receive_files: config.file_receive_dir.is_some(),
            file_receive_limit_bytes: config.max_receive_bytes.min(DEFAULT_FILE_SIZE_LIMIT_BYTES),
        },
    )?;

    loop {
        let frame = read_blocking_frame(&mut stream)
            .map_err(|error| format!("failed to read pairing response: {error}"))?;
        let plaintext = control
            .open_next(CHANNEL_CONTROL, &frame)
            .map_err(|error| error.to_string())?;
        match decode_server_control(&plaintext)? {
            ServerControl::Authenticated { .. } => return Ok(()),
            ServerControl::Error { message, .. } => return Err(message),
            _ => {}
        }
    }
}

fn write_blocking_control(
    stream: &mut StdTcpStream,
    control: &mut ControlTransport,
    message: ClientControl,
) -> Result<(), String> {
    let payload = encode_client_control(&message)?;
    let encrypted = control
        .seal_next(CHANNEL_CONTROL, &payload)
        .map_err(|error| error.to_string())?;
    let mut framed = Vec::new();
    frame::encode_frame(&encrypted, &mut framed).map_err(|error| error.to_string())?;
    stream
        .write_all(&framed)
        .map_err(|error| format!("failed to write pairing request: {error}"))
}

fn resolve_endpoint(endpoint: &str) -> Result<SocketAddr, String> {
    endpoint
        .to_socket_addrs()
        .map_err(|error| format!("{endpoint}: {error}"))?
        .next()
        .ok_or_else(|| format!("{endpoint}: no socket addresses resolved"))
}

fn pinned_server_public_key(config: &ClientConfig) -> Result<[u8; 32], String> {
    match config.server_public_key.as_deref() {
        Some(public_key) => ed25519_public_key_from_hex(public_key)
            .map_err(|error| format!("invalid configured server-public-key: {error}")),
        None => Ok(dev_server_public_key()),
    }
}

fn read_blocking_frame(stream: &mut StdTcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > frame::MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "server frame exceeds maximum length",
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

struct WorkerState {
    config: ClientConfig,
    events: Sender<NetworkEvent>,
    tcp: TcpStream,
    udp: UdpSocket,
    udp_local_addr: SocketAddr,
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    control: ControlTransport,
    secrets: Option<SessionSecrets>,
    server_udp_addr: SocketAddr,
    server_udp_probe_addr: Option<SocketAddr>,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    room_id: Option<RoomId>,
    active_stream: Option<StreamId>,
    local_sequence: u32,
    media_send_counter: u64,
    media_recv_replay: AntiReplay,
    p2p_generation: u64,
    p2p_tie_breaker: u64,
    p2p_nat: P2pNatKind,
    p2p_nat_classifier: NatClassifier,
    p2p_reflexive_addr: Option<SocketAddr>,
    p2p_candidates: Vec<P2pCandidate>,
    p2p_peers: HashMap<SessionId, PeerConnection>,
    p2p_stream_owners: HashMap<StreamId, SessionId>,
    encoder_feedback: EncoderFeedbackController,
    restart_port_policy: RestartPortPolicy,
    udp_rebind_requested: bool,
    interface_snapshot: Option<InterfaceSnapshot>,
    next_interface_poll: Instant,
    next_file_transfer: u64,
    outgoing_uploads: VecDeque<OutgoingUpload>,
    incoming_files: HashMap<FileTransferId, IncomingFile>,
    shutdown: bool,
    disconnect_reason: Option<String>,
    auth_failure: Option<String>,
}

struct OutgoingUpload {
    transfer_id: FileTransferId,
    room_id: RoomId,
    name: String,
    size: u64,
    file: File,
    offset: u64,
    started: bool,
    next_status_at: u64,
}

struct IncomingFile {
    metadata: FileMetadata,
    path: PathBuf,
    file: File,
    received: u64,
    next_status_at: u64,
}

struct PeerConnection {
    user_id: UserId,
    agent: TraversalAgent,
    send_key: KeyMaterial,
    recv_key: KeyMaterial,
    send_counter: u64,
    recv_replay: AntiReplay,
    connection_id: u64,
}

enum P2pMediaPacket {
    Voice {
        stream_id: StreamId,
        sequence: u32,
        flags: u8,
        silence_ranges: u64,
        opus: Vec<u8>,
        action: Option<P2pAction>,
    },
    Feedback {
        stream_id: StreamId,
        feedback: media::VoiceFeedback,
        action: Option<P2pAction>,
    },
}

struct EncoderFeedbackController {
    current: LiveEncoderProfile,
    smoothed_loss: f32,
    high_loss_windows: u8,
    hold_until: Instant,
}

impl EncoderFeedbackController {
    fn new() -> Self {
        Self {
            current: LiveEncoderProfile::DRED_20,
            smoothed_loss: 0.0,
            high_loss_windows: 0,
            hold_until: Instant::now(),
        }
    }

    fn observe(
        &mut self,
        feedback: LivePlaybackFeedback,
        now: Instant,
    ) -> Option<LiveEncoderProfile> {
        if feedback.expected_packets == 0 {
            return None;
        }
        let effective_loss = f32::from(feedback.lost_packets.saturating_add(feedback.late_packets))
            / f32::from(feedback.expected_packets);
        self.smoothed_loss = ENCODER_FEEDBACK_ALPHA * effective_loss
            + (1.0 - ENCODER_FEEDBACK_ALPHA) * self.smoothed_loss;
        if effective_loss >= 0.45 {
            self.high_loss_windows = self.high_loss_windows.saturating_add(1).min(2);
        } else {
            self.high_loss_windows = 0;
        }

        let target = if effective_loss >= 0.55 || self.high_loss_windows >= 2 {
            LiveEncoderProfile::DRED_60
        } else if effective_loss >= 0.40 {
            LiveEncoderProfile::DRED_50
        } else if effective_loss >= 0.25 {
            LiveEncoderProfile::DRED_35
        } else {
            LiveEncoderProfile::DRED_20
        };

        if target.packet_loss_percent > self.current.packet_loss_percent {
            return self.set_current(target, now);
        }
        if target.packet_loss_percent == self.current.packet_loss_percent
            && self.current.packet_loss_percent > LiveEncoderProfile::DRED_20.packet_loss_percent
        {
            self.hold_until = now + ENCODER_PROFILE_HOLD;
            return None;
        }
        if now < self.hold_until {
            return None;
        }

        let next = match self.current.packet_loss_percent {
            60 if self.smoothed_loss < 0.45 => Some(LiveEncoderProfile::DRED_50),
            50 if self.smoothed_loss < 0.30 => Some(LiveEncoderProfile::DRED_35),
            35 if self.smoothed_loss < 0.15
                && feedback.max_queue_ms < 200
                && feedback.max_interarrival_jitter_ms < 50 =>
            {
                Some(LiveEncoderProfile::DRED_20)
            }
            _ => None,
        };
        next.and_then(|profile| self.set_current(profile, now))
    }

    fn set_current(
        &mut self,
        profile: LiveEncoderProfile,
        now: Instant,
    ) -> Option<LiveEncoderProfile> {
        if profile == self.current {
            return None;
        }
        self.current = profile;
        if profile.packet_loss_percent > LiveEncoderProfile::DRED_20.packet_loss_percent {
            self.hold_until = now + ENCODER_PROFILE_HOLD;
        }
        Some(profile)
    }
}

impl WorkerState {
    fn queue_control(&mut self, control: ClientControl) -> Result<(), String> {
        let kind = client_control_kind(&control);
        let payload = encode_client_control(&control)?;
        let payload_size = payload.len();
        let encrypted = self
            .control
            .seal_next(CHANNEL_CONTROL, &payload)
            .map_err(|error| error.to_string())?;
        let encrypted_size = encrypted.len();
        frame::encode_frame(&encrypted, &mut self.write_buf).map_err(|error| error.to_string())?;
        kvlog::info!(
            "client control queued",
            kind,
            payload_size,
            encrypted_size,
            queued_bytes = self.write_buf.len()
        );
        self.write_tcp()
    }

    fn read_tcp(&mut self) -> Result<(), String> {
        let mut buf = [0u8; 8192];
        loop {
            match self.tcp.read(&mut buf) {
                Ok(0) => {
                    kvlog::info!("tcp server closed connection");
                    self.shutdown = true;
                    self.disconnect_reason = Some("server closed connection".to_string());
                    break;
                }
                Ok(n) => {
                    self.read_buf.extend_from_slice(&buf[..n]);
                    kvlog::info!(
                        "tcp bytes received",
                        size = n,
                        buffered = self.read_buf.len()
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    kvlog::warn!("tcp read failed", error = %error);
                    return Err(format!("TCP read failed: {error}"));
                }
            }
        }

        loop {
            let frame = match frame::pop_frame(&mut self.read_buf) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(error) => return Err(format!("invalid server frame: {error}")),
            };
            kvlog::info!("server frame received", frame_size = frame.len());
            let plaintext = self
                .control
                .open_next(CHANNEL_CONTROL, &frame)
                .map_err(|error| error.to_string())?;
            kvlog::info!("server control decrypted", payload_size = plaintext.len());
            let control = decode_server_control(&plaintext)?;
            self.handle_server_control(control);
        }
        Ok(())
    }

    fn write_tcp(&mut self) -> Result<(), String> {
        while !self.write_buf.is_empty() {
            match self.tcp.write(&self.write_buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.write_buf.drain(..n);
                    kvlog::info!(
                        "tcp bytes written",
                        size = n,
                        remaining = self.write_buf.len()
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    kvlog::warn!("tcp write failed", error = %error);
                    return Err(format!("TCP write failed: {error}"));
                }
            }
        }
        Ok(())
    }

    fn read_udp(&mut self) {
        let mut buf = [0u8; 2048];
        let mut datagrams_this_wake: u32 = 0;
        loop {
            let (len, src) = match self.udp.recv_from(&mut buf) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    // More than one datagram per poll wake means inbound packets
                    // queued in the socket while this single-threaded worker was
                    // busy (sending voice, polling p2p) between wakes. That read
                    // coalescing is what the receiver measures as interarrival
                    // jitter, so surface it.
                    if datagrams_this_wake > 1 {
                        kvlog::info!("udp read coalesced", datagrams = datagrams_this_wake);
                    }
                    break;
                }
                Err(error) => {
                    kvlog::warn!("udp receive failed", error = %error);
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP receive failed: {error}")));
                    break;
                }
            };
            datagrams_this_wake += 1;
            let packet = &buf[..len];
            let now = Instant::now();
            if is_stun_message(packet) {
                self.handle_p2p_stun(now, src, packet);
                continue;
            }
            if self.handle_p2p_media(now, src, packet) {
                continue;
            }
            if src != self.server_udp_addr {
                kvlog::warn!(
                    "udp packet ignored",
                    addr = %src,
                    expected_addr = %self.server_udp_addr,
                    packet_size = len
                );
                continue;
            }
            match self.open_server_media(packet) {
                Ok((
                    _,
                    MediaPayload::Voice {
                        stream_id,
                        sequence,
                        flags,
                        silence_ranges,
                        opus,
                    },
                )) => {
                    kvlog::info!(
                        "voice packet received",
                        stream_id = stream_id.0,
                        sequence,
                        payload_size = opus.len()
                    );
                    let _ = self
                        .events
                        .send(NetworkEvent::VoicePacket(RemoteVoicePacket {
                            stream_id: stream_id.0,
                            sequence,
                            flags,
                            silence_ranges,
                            payload: opus,
                            received_at: now,
                        }));
                }
                Ok((_, MediaPayload::Pong { .. })) => {}
                Ok((
                    _,
                    MediaPayload::VoiceFeedback {
                        stream_id,
                        feedback,
                    },
                )) => {
                    let feedback = live_feedback_from_media(stream_id, feedback);
                    self.handle_encoder_feedback(feedback, now);
                }
                Ok((_, MediaPayload::Ping { nonce })) => {
                    self.send_media(&MediaPayload::Pong { nonce });
                }
                Ok((_, MediaPayload::Bind { .. })) => {}
                Ok((_, MediaPayload::NatProbe { .. })) => {}
                Ok((
                    _,
                    MediaPayload::PeerVoice { .. } | MediaPayload::PeerVoiceFeedback { .. },
                )) => {}
                Err(error) => {
                    kvlog::warn!("udp packet rejected", packet_size = len, error = %error);
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP packet rejected: {error}")));
                }
            }
        }
    }

    fn open_server_media(
        &mut self,
        packet: &[u8],
    ) -> Result<(media::UdpHeader, MediaPayload), media::MediaError> {
        match &self.secrets {
            Some(secrets) => {
                media::open_media(&secrets.media_recv, &mut self.media_recv_replay, packet)
            }
            None => {
                let (header, payload) = media::open_plaintext_media(packet)?;
                if !self.media_recv_replay.update(header.counter) {
                    return Err(media::MediaError::Replay);
                }
                Ok((header, payload))
            }
        }
    }

    fn handle_command(&mut self, command: NetworkCommand) -> Result<(), String> {
        if !matches!(
            command,
            NetworkCommand::LocalVoicePacket(_)
                | NetworkCommand::SequencedLocalVoicePacket { .. }
                | NetworkCommand::PlaybackFeedback(_)
        ) {
            kvlog::info!(
                "network command received",
                kind = network_command_kind(&command)
            );
        }
        match command {
            NetworkCommand::SendChat(body) => {
                let room_id = self.room_id.unwrap_or(self.config.room_id);
                kvlog::info!(
                    "send chat command handling",
                    room_id = room_id.0,
                    body_size = body.len()
                );
                self.queue_control(ClientControl::SendChat { room_id, body })?;
            }
            NetworkCommand::UploadFile(path) => {
                self.queue_file_upload(path);
            }
            NetworkCommand::LocalVoicePacket(frame) => {
                if let Some(stream_id) = self.active_stream {
                    let sequence = self.local_sequence;
                    self.local_sequence = self.local_sequence.wrapping_add(1);
                    self.send_local_voice_packet(stream_id, sequence, frame);
                }
            }
            NetworkCommand::SequencedLocalVoicePacket { sequence, frame } => {
                if let Some(stream_id) = self.active_stream {
                    self.local_sequence = self.local_sequence.max(sequence.wrapping_add(1));
                    self.send_local_voice_packet(stream_id, sequence, frame);
                }
            }
            NetworkCommand::PlaybackFeedback(feedback) => {
                let _ = self.events.send(NetworkEvent::PlaybackFeedback(feedback));
                let stream_id = StreamId(feedback.stream_id);
                self.send_media(&MediaPayload::VoiceFeedback {
                    stream_id,
                    feedback: media_feedback_from_live(feedback),
                });
                if let Some(session_id) = self.p2p_stream_owners.get(&stream_id).copied() {
                    self.send_p2p_voice_feedback(session_id, stream_id, feedback);
                }
            }
            NetworkCommand::Shutdown => {
                kvlog::info!("shutdown command handling");
                self.shutdown = true;
            }
        }
        Ok(())
    }

    fn send_local_voice_packet(
        &mut self,
        stream_id: StreamId,
        sequence: u32,
        frame: LocalVoiceFrame,
    ) {
        let relay_payload = MediaPayload::Voice {
            stream_id,
            sequence,
            flags: frame.flags,
            silence_ranges: frame.silence_ranges,
            opus: frame.payload.clone(),
        };
        self.send_media(&relay_payload);
        self.send_p2p_voice(
            stream_id,
            sequence,
            frame.flags,
            frame.silence_ranges,
            &frame.payload,
        );
    }

    fn queue_file_upload(&mut self, path: PathBuf) {
        match self.prepare_file_upload(path) {
            Ok(upload) => {
                let name = upload.name.clone();
                let size = upload.size;
                self.outgoing_uploads.push_back(upload);
                let _ = self.events.send(NetworkEvent::Status(format!(
                    "queued upload {name} ({})",
                    format_bytes(size)
                )));
            }
            Err(error) => {
                let _ = self.events.send(NetworkEvent::Error(error));
            }
        }
    }

    fn prepare_file_upload(&mut self, path: PathBuf) -> Result<OutgoingUpload, String> {
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("upload path is not a file: {}", path.display()));
        }
        let limit = self
            .config
            .max_upload_bytes
            .min(DEFAULT_FILE_SIZE_LIMIT_BYTES);
        let size = metadata.len();
        if size > limit {
            return Err(format!(
                "file is {}; limit is {}",
                format_bytes(size),
                format_bytes(limit)
            ));
        }
        let name = sanitize_file_name(
            path.file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| "upload path must end in a UTF-8 file name".to_string())?,
        );
        if name.len() > MAX_FILE_NAME_BYTES {
            return Err("file name exceeds maximum length".to_string());
        }
        let file = File::open(&path)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        let transfer_id = FileTransferId(self.next_file_transfer);
        self.next_file_transfer = self.next_file_transfer.wrapping_add(1).max(1);
        Ok(OutgoingUpload {
            transfer_id,
            room_id: self.room_id.unwrap_or(self.config.room_id),
            name,
            size,
            file,
            offset: 0,
            started: false,
            next_status_at: FILE_PROGRESS_STEP_BYTES.min(size),
        })
    }

    fn poll_uploads(&mut self) -> Result<(), String> {
        for _ in 0..MAX_FILE_CHUNKS_PER_TICK {
            if self.write_buf.len() > MAX_QUEUED_FILE_BYTES {
                break;
            }
            if !self.poll_one_upload()? {
                break;
            }
        }
        Ok(())
    }

    fn poll_one_upload(&mut self) -> Result<bool, String> {
        let Some(mut upload) = self.outgoing_uploads.pop_front() else {
            return Ok(false);
        };

        if !upload.started {
            self.queue_control(ClientControl::UploadFileStart {
                room_id: upload.room_id,
                transfer_id: upload.transfer_id,
                name: upload.name.clone(),
                size: upload.size,
            })?;
            upload.started = true;
            let _ = self.events.send(NetworkEvent::Status(format!(
                "uploading {} ({})",
                upload.name,
                format_bytes(upload.size)
            )));
            self.outgoing_uploads.push_front(upload);
            return Ok(true);
        }

        if upload.offset < upload.size {
            let remaining = (upload.size - upload.offset).min(MAX_FILE_CHUNK_BYTES as u64);
            let mut data = vec![0; remaining as usize];
            let read = upload
                .file
                .read(&mut data)
                .map_err(|error| format!("failed to read {}: {error}", upload.name))?;
            if read == 0 {
                self.queue_control(ClientControl::UploadFileCancel {
                    transfer_id: upload.transfer_id,
                    reason: "local file ended early".to_string(),
                })?;
                return Err(format!("file ended early while uploading {}", upload.name));
            }
            data.truncate(read);
            let offset = upload.offset;
            self.queue_control(ClientControl::UploadFileChunk {
                transfer_id: upload.transfer_id,
                offset,
                data,
            })?;
            upload.offset += read as u64;
            if upload.offset >= upload.next_status_at || upload.offset == upload.size {
                let _ = self.events.send(NetworkEvent::Status(format!(
                    "uploaded {} of {} for {}",
                    format_bytes(upload.offset),
                    format_bytes(upload.size),
                    upload.name
                )));
                upload.next_status_at = upload
                    .next_status_at
                    .saturating_add(FILE_PROGRESS_STEP_BYTES);
            }
            self.outgoing_uploads.push_front(upload);
            return Ok(true);
        }

        self.queue_control(ClientControl::UploadFileComplete {
            transfer_id: upload.transfer_id,
        })?;
        let _ = self.events.send(NetworkEvent::Status(format!(
            "upload complete: {} ({})",
            upload.name,
            format_bytes(upload.size)
        )));
        Ok(true)
    }

    fn handle_file_offered(&mut self, file: FileMetadata, contents: bool) {
        kvlog::info!(
            "file offered",
            transfer_id = file.transfer_id.0,
            file_name = file.file_name.as_str(),
            file_size = file.size,
            contents
        );
        if !contents {
            let reason = if self.config.file_receive_dir.is_some() {
                "receive limit"
            } else {
                "receive-dir disabled"
            };
            let _ = self.events.send(NetworkEvent::Status(format!(
                "{} sent {} ({}, metadata only: {reason})",
                file.sender_name,
                file.file_name,
                format_bytes(file.size)
            )));
            return;
        }
        if file.size
            > self
                .config
                .max_receive_bytes
                .min(DEFAULT_FILE_SIZE_LIMIT_BYTES)
        {
            let _ = self.events.send(NetworkEvent::Error(format!(
                "not receiving {}; size {} exceeds local limit {}",
                file.file_name,
                format_bytes(file.size),
                format_bytes(
                    self.config
                        .max_receive_bytes
                        .min(DEFAULT_FILE_SIZE_LIMIT_BYTES)
                )
            )));
            return;
        }
        let Some(receive_dir) = self.config.file_receive_dir.clone() else {
            let _ = self.events.send(NetworkEvent::Status(format!(
                "{} sent {} ({}, metadata only)",
                file.sender_name,
                file.file_name,
                format_bytes(file.size)
            )));
            return;
        };
        match create_receive_file(&receive_dir, &file.file_name) {
            Ok((path, handle)) => {
                let _ = self.events.send(NetworkEvent::Status(format!(
                    "receiving {} from {}",
                    file.file_name, file.sender_name
                )));
                self.incoming_files.insert(
                    file.transfer_id,
                    IncomingFile {
                        metadata: file,
                        path,
                        file: handle,
                        received: 0,
                        next_status_at: FILE_PROGRESS_STEP_BYTES,
                    },
                );
            }
            Err(error) => {
                let _ = self.events.send(NetworkEvent::Error(error));
            }
        }
    }

    fn handle_file_chunk(&mut self, transfer_id: FileTransferId, offset: u64, data: Vec<u8>) {
        let Some(incoming) = self.incoming_files.get_mut(&transfer_id) else {
            return;
        };
        if incoming.received != offset {
            let path = incoming.path.clone();
            let name = incoming.metadata.file_name.clone();
            self.incoming_files.remove(&transfer_id);
            let _ = fs::remove_file(path);
            let _ = self.events.send(NetworkEvent::Error(format!(
                "file transfer offset mismatch for {name}"
            )));
            return;
        }
        if offset.saturating_add(data.len() as u64) > incoming.metadata.size {
            let path = incoming.path.clone();
            let name = incoming.metadata.file_name.clone();
            self.incoming_files.remove(&transfer_id);
            let _ = fs::remove_file(path);
            let _ = self.events.send(NetworkEvent::Error(format!(
                "file transfer exceeded declared size for {name}"
            )));
            return;
        }
        if let Err(error) = incoming.file.write_all(&data) {
            let path = incoming.path.clone();
            let name = incoming.metadata.file_name.clone();
            self.incoming_files.remove(&transfer_id);
            let _ = fs::remove_file(path);
            let _ = self.events.send(NetworkEvent::Error(format!(
                "failed to write {name}: {error}"
            )));
            return;
        }
        incoming.received += data.len() as u64;
        if incoming.received >= incoming.next_status_at
            || incoming.received == incoming.metadata.size
        {
            let _ = self.events.send(NetworkEvent::Status(format!(
                "received {} of {} for {}",
                format_bytes(incoming.received),
                format_bytes(incoming.metadata.size),
                incoming.metadata.file_name
            )));
            incoming.next_status_at = incoming
                .next_status_at
                .saturating_add(FILE_PROGRESS_STEP_BYTES);
        }
    }

    fn handle_file_complete(&mut self, transfer_id: FileTransferId) {
        let Some(mut incoming) = self.incoming_files.remove(&transfer_id) else {
            return;
        };
        if incoming.received != incoming.metadata.size {
            let _ = fs::remove_file(&incoming.path);
            let _ = self.events.send(NetworkEvent::Error(format!(
                "file transfer ended early for {}",
                incoming.metadata.file_name
            )));
            return;
        }
        if let Err(error) = incoming.file.flush() {
            let _ = fs::remove_file(&incoming.path);
            let _ = self.events.send(NetworkEvent::Error(format!(
                "failed to flush {}: {error}",
                incoming.metadata.file_name
            )));
            return;
        }
        let _ = self.events.send(NetworkEvent::Status(format!(
            "saved {} to {}",
            incoming.metadata.file_name,
            incoming.path.display()
        )));
    }

    fn handle_file_canceled(&mut self, transfer_id: FileTransferId, reason: &str) {
        if let Some(incoming) = self.incoming_files.remove(&transfer_id) {
            let _ = fs::remove_file(&incoming.path);
            let _ = self.events.send(NetworkEvent::Status(format!(
                "file transfer canceled for {}: {reason}",
                incoming.metadata.file_name
            )));
        }
    }

    fn handle_server_control(&mut self, control: ServerControl) {
        kvlog::info!(
            "server control handling",
            kind = server_control_kind(&control)
        );
        match control {
            ServerControl::Authenticated {
                session_id,
                user_id,
                rooms,
                current_room,
            } => {
                self.session_id = Some(session_id);
                self.user_id = Some(user_id);
                self.room_id = current_room;
                kvlog::info!(
                    "client authenticated",
                    session_id = session_id.0,
                    user_id = user_id.0,
                    room_id = current_room.map(|room_id| room_id.0),
                    room_count = rooms.len()
                );
                let _ = self.events.send(NetworkEvent::Authenticated {
                    session_id,
                    user_id,
                    rooms,
                });
                self.bind_udp();
                if current_room.is_none() {
                    let _ = self.queue_control(ClientControl::JoinRoom {
                        room_id: self.config.room_id,
                    });
                }
            }
            ServerControl::RoomJoined {
                room_id,
                history,
                participants,
            } => {
                self.room_id = Some(room_id);
                kvlog::info!(
                    "client room joined",
                    room_id = room_id.0,
                    history_len = history.len(),
                    participant_count = participants.len()
                );
                let _ = self.events.send(NetworkEvent::RoomJoined {
                    room_id,
                    history,
                    participants,
                });
            }
            ServerControl::Chat { message } => {
                kvlog::info!(
                    "client chat received",
                    room_id = message.room_id.0,
                    message_id = message.message_id.0,
                    user_id = message.sender.0,
                    body_size = message.body.len()
                );
                let _ = self.events.send(NetworkEvent::Chat(message));
            }
            ServerControl::Presence {
                room_id,
                participant,
                online,
            } => {
                kvlog::info!(
                    "client presence received",
                    user_id = participant.user_id.0,
                    user = participant.name.as_str(),
                    online
                );
                let verb = if online { "joined" } else { "left" };
                let _ = self
                    .events
                    .send(NetworkEvent::Status(format!("{} {verb}", participant.name)));
                let _ = self.events.send(NetworkEvent::Presence {
                    room_id,
                    participant,
                    online,
                });
            }
            ServerControl::VoiceStarted {
                user_id, stream_id, ..
            } => {
                kvlog::info!(
                    "client voice started",
                    user_id = user_id.0,
                    stream_id = stream_id.0
                );
                if Some(user_id) == self.user_id {
                    self.active_stream = Some(stream_id);
                    self.local_sequence = 0;
                    self.encoder_feedback = EncoderFeedbackController::new();
                    let _ = self.events.send(NetworkEvent::EncoderProfileChanged(
                        LiveEncoderProfile::DRED_20,
                    ));
                }
                let _ = self
                    .events
                    .send(NetworkEvent::VoiceStarted { user_id, stream_id });
            }
            ServerControl::VoiceStopped {
                user_id, stream_id, ..
            } => {
                kvlog::info!(
                    "client voice stopped",
                    user_id = user_id.0,
                    stream_id = stream_id.0
                );
                if Some(stream_id) == self.active_stream {
                    self.active_stream = None;
                }
                self.p2p_stream_owners.remove(&stream_id);
                let _ = self
                    .events
                    .send(NetworkEvent::VoiceStopped { user_id, stream_id });
            }
            ServerControl::UdpBound => {
                kvlog::info!("client udp bound");
                let _ = self
                    .events
                    .send(NetworkEvent::Status("udp media bound".to_string()));
            }
            ServerControl::UdpReflexive { addr } => match addr.parse::<SocketAddr>() {
                Ok(addr) => {
                    kvlog::info!("client udp reflexive address received", addr = %addr);
                    self.p2p_reflexive_addr = Some(addr);
                    self.publish_p2p_candidates();
                }
                Err(error) => {
                    kvlog::warn!("invalid udp reflexive address", addr = addr.as_str(), error = %error);
                }
            },
            ServerControl::P2pNatProbe { probe_id, addr } => match addr.parse::<SocketAddr>() {
                Ok(addr) => {
                    kvlog::info!(
                        "client nat probe observation received",
                        probe_id,
                        addr = %addr
                    );
                    let server_addr = self
                        .probe_addr_for_id(probe_id)
                        .unwrap_or(self.server_udp_addr);
                    self.p2p_nat_classifier.observe(ReflexiveObservation {
                        server_addr,
                        mapped_addr: addr,
                    });
                    let classified = self.p2p_nat_classifier.classify();
                    self.p2p_nat = control_nat_kind(classified);
                    self.p2p_reflexive_addr = self.p2p_nat_classifier.primary_reflexive_addr();
                    self.publish_p2p_candidates();
                }
                Err(error) => {
                    kvlog::warn!(
                        "invalid nat probe address",
                        probe_id,
                        addr = addr.as_str(),
                        error = %error
                    );
                }
            },
            ServerControl::P2pPeer { peer } => {
                if let Err(error) = self.install_p2p_peer(peer) {
                    kvlog::warn!("p2p peer rejected", error = %error);
                    let _ = self.events.send(NetworkEvent::Error(error));
                }
            }
            ServerControl::P2pPeerGone {
                session_id,
                user_id,
            } => {
                self.p2p_peers.remove(&session_id);
                let _ = self.events.send(NetworkEvent::PeerTransport {
                    user_id,
                    direct: false,
                });
                kvlog::info!(
                    "p2p peer removed",
                    session_id = session_id.0,
                    user_id = user_id.0
                );
            }
            ServerControl::FileOffered { file, contents } => {
                self.handle_file_offered(file, contents);
            }
            ServerControl::FileChunk {
                transfer_id,
                offset,
                data,
            } => {
                self.handle_file_chunk(transfer_id, offset, data);
            }
            ServerControl::FileComplete { transfer_id } => {
                self.handle_file_complete(transfer_id);
            }
            ServerControl::FileCanceled {
                transfer_id,
                reason,
            } => {
                self.handle_file_canceled(transfer_id, &reason);
            }
            ServerControl::Pong { .. } => {}
            ServerControl::Error { code, message } => {
                kvlog::warn!("server control error", error = message.as_str());
                if self.session_id.is_none() && is_auth_failure_code(code) {
                    self.auth_failure = Some(message);
                    self.shutdown = true;
                } else {
                    let _ = self.events.send(NetworkEvent::Error(message));
                }
            }
        }
    }

    fn bind_udp(&mut self) {
        if let Some(session_id) = self.session_id {
            kvlog::info!("udp bind sending", session_id = session_id.0);
            self.send_media(&MediaPayload::Bind { session_id });
            self.send_nat_probe(session_id, 0, self.server_udp_addr);
            if let Some(udp_probe_addr) = self.server_udp_probe_addr {
                self.send_nat_probe(session_id, 1, udp_probe_addr);
            }
        }
    }

    fn send_nat_probe(&mut self, session_id: SessionId, probe_id: u8, addr: SocketAddr) {
        let payload = MediaPayload::NatProbe {
            session_id,
            probe_id,
        };
        let counter = self.media_send_counter;
        self.media_send_counter = self.media_send_counter.wrapping_add(1);
        match self.seal_server_media(counter, &payload) {
            Ok(packet) => self.send_udp_raw("nat_probe", None, addr, &packet),
            Err(error) => {
                kvlog::warn!("nat probe seal failed", probe_id, error = %error);
            }
        }
    }

    fn probe_addr_for_id(&self, probe_id: u8) -> Option<SocketAddr> {
        match probe_id {
            0 => Some(self.server_udp_addr),
            1 => self.server_udp_probe_addr,
            _ => None,
        }
    }

    fn poll_interfaces(&mut self, now: Instant) {
        if now < self.next_interface_poll {
            return;
        }
        self.next_interface_poll = now + INTERFACE_POLL_INTERVAL;
        let Ok(snapshot) = InterfaceSnapshot::capture() else {
            return;
        };
        let changed = self
            .interface_snapshot
            .as_ref()
            .is_some_and(|previous| snapshot.changed_from(previous));
        self.interface_snapshot = Some(snapshot);
        if changed {
            kvlog::info!("network interfaces changed; requesting p2p restart");
            self.request_p2p_restart();
        }
    }

    fn request_p2p_restart(&mut self) {
        self.p2p_generation = self.p2p_generation.wrapping_add(1).max(1);
        self.p2p_reflexive_addr = None;
        self.p2p_candidates.clear();
        self.p2p_nat_classifier = NatClassifier::new();
        self.p2p_nat = configured_nat_kind();
        self.udp_rebind_requested = true;
    }

    fn rebind_udp_socket(&mut self, poll: &mut Poll) -> Result<(), String> {
        self.udp_rebind_requested = false;
        if let Err(error) = poll.registry().deregister(&mut self.udp) {
            kvlog::warn!("failed to deregister udp socket before rebind", error = %error);
        }
        self.restart_port_policy.record(self.udp_local_addr.port());

        let bind_addr =
            RestartPortPolicy::bind_addr_for_restart(if self.server_udp_addr.is_ipv4() {
                "0.0.0.0:0".parse().unwrap()
            } else {
                "[::]:0".parse().unwrap()
            });
        let mut last_error = None;
        for _ in 0..8 {
            match bind_udp_socket(bind_addr, UdpSocketOptions::default()) {
                Ok(socket) => {
                    let local_addr = socket
                        .local_addr()
                        .map_err(|error| format!("failed to read rebound UDP address: {error}"))?;
                    if !self.restart_port_policy.accepts(local_addr.port()) {
                        self.restart_port_policy.record(local_addr.port());
                        continue;
                    }
                    self.udp_local_addr = local_addr;
                    self.udp = UdpSocket::from_std(socket);
                    poll.registry()
                        .register(&mut self.udp, UDP, Interest::READABLE)
                        .map_err(|error| {
                            format!("failed to register rebound UDP socket: {error}")
                        })?;
                    kvlog::info!("udp socket rebound", addr = %self.udp_local_addr);
                    self.bind_udp();
                    return Ok(());
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        Err(format!(
            "failed to rebind UDP socket to fresh port{}",
            last_error
                .map(|error| format!(": {error}"))
                .unwrap_or_default()
        ))
    }

    fn send_media(&mut self, payload: &MediaPayload) {
        let kind = media_payload_kind(payload);
        let counter = self.media_send_counter;
        self.media_send_counter = self.media_send_counter.wrapping_add(1);
        match self.seal_server_media(counter, payload) {
            Ok(packet) => {
                if let Err(error) = self.udp.send_to(&packet, self.server_udp_addr) {
                    kvlog::warn!(
                        "udp send failed",
                        kind,
                        packet_size = packet.len(),
                        error = %error
                    );
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP send failed: {error}")));
                } else if !matches!(
                    payload,
                    MediaPayload::Voice { .. } | MediaPayload::VoiceFeedback { .. }
                ) {
                    kvlog::info!("udp packet sent", kind, packet_size = packet.len(), counter);
                }
            }
            Err(error) => {
                kvlog::warn!("udp seal failed", kind, error = %error);
                let _ = self
                    .events
                    .send(NetworkEvent::Error(format!("UDP seal failed: {error}")));
            }
        }
    }

    fn seal_server_media(
        &self,
        counter: u64,
        payload: &MediaPayload,
    ) -> Result<Vec<u8>, media::MediaError> {
        match &self.secrets {
            Some(secrets) => media::seal_media(&secrets.media_send, counter, payload),
            None => media::seal_plaintext_media(counter, payload),
        }
    }

    fn publish_p2p_candidates(&mut self) {
        let Some(room_id) = self.room_id else {
            return;
        };
        if self.session_id.is_none() {
            return;
        }
        let candidates = self.gather_p2p_candidates();
        self.p2p_candidates = candidates.clone();
        kvlog::info!(
            "publishing p2p candidates",
            generation = self.p2p_generation,
            candidate_count = candidates.len()
        );
        let _ = self.queue_control(ClientControl::PublishP2p {
            room_id,
            generation: self.p2p_generation,
            nat: self.p2p_nat,
            tie_breaker: self.p2p_tie_breaker,
            candidates,
        });
    }

    fn gather_p2p_candidates(&self) -> Vec<P2pCandidate> {
        let mut next_id = 1;
        let mut candidates = host_candidates_with_metadata(
            1,
            self.p2p_generation,
            self.udp_local_addr.port(),
            true,
            &mut next_id,
        )
        .unwrap_or_else(|error| {
            kvlog::warn!("host candidate discovery failed", error = %error);
            Vec::new()
        });
        if candidates.is_empty() {
            let fallback_ip = if self.server_udp_addr.is_ipv4() {
                "127.0.0.1".parse().unwrap()
            } else {
                "::1".parse().unwrap()
            };
            candidates.push(Candidate::with_metadata(
                next_id,
                1,
                self.p2p_generation,
                CandidateKind::Host,
                SocketAddr::new(fallback_ip, self.udp_local_addr.port()),
                None,
                true,
            ));
            next_id = next_id.wrapping_add(1).max(1);
        }
        if let Some(reflexive) = self.p2p_reflexive_addr {
            candidates.push(Candidate::with_metadata(
                next_id,
                1,
                self.p2p_generation,
                CandidateKind::ServerReflexive,
                reflexive,
                Some(self.udp_local_addr),
                true,
            ));
            next_id = next_id.wrapping_add(1).max(1);
        }
        candidates.push(Candidate::with_metadata(
            next_id,
            1,
            self.p2p_generation,
            CandidateKind::Relay,
            self.server_udp_addr,
            None,
            true,
        ));
        candidates.iter().map(control_candidate).collect()
    }

    fn install_p2p_peer(&mut self, peer: P2pPeerInfo) -> Result<(), String> {
        let send_key = key_from_control(&peer.send_key)?;
        let recv_key = key_from_control(&peer.recv_key)?;
        let local_candidates = self
            .p2p_candidates
            .iter()
            .filter_map(candidate_from_control)
            .collect::<Vec<_>>();
        let remote_candidates = peer
            .candidates
            .iter()
            .filter_map(candidate_from_control)
            .collect::<Vec<_>>();
        if local_candidates.is_empty() || remote_candidates.is_empty() {
            return Err("missing P2P candidates".to_string());
        }
        let config = P2pAgentConfig {
            username: Some(p2p_username(peer.connection_id)),
            ..P2pAgentConfig::default()
        };
        let agent = TraversalAgent::new(
            Instant::now(),
            config,
            ice_role_from_control(peer.role),
            self.p2p_tie_breaker,
            nat_from_control(self.p2p_nat),
            nat_from_control(peer.nat),
            local_candidates,
            remote_candidates,
        );
        kvlog::info!(
            "p2p peer installed",
            session_id = peer.session_id.0,
            user_id = peer.user_id.0,
            generation = peer.generation,
            connection_id = peer.connection_id,
            direct_pair_count = agent.direct_pair_count()
        );
        self.p2p_peers.insert(
            peer.session_id,
            PeerConnection {
                user_id: peer.user_id,
                agent,
                send_key,
                recv_key,
                send_counter: 0,
                recv_replay: AntiReplay::new(),
                connection_id: peer.connection_id,
            },
        );
        Ok(())
    }

    fn poll_p2p(&mut self, now: Instant) {
        let actions = self
            .p2p_peers
            .iter_mut()
            .map(|(session_id, peer)| (*session_id, peer.agent.poll(now)))
            .filter(|(_, actions)| !actions.is_empty())
            .collect::<Vec<_>>();
        for (session_id, actions) in actions {
            self.apply_p2p_actions(session_id, actions);
        }
    }

    fn handle_p2p_stun(&mut self, now: Instant, src: SocketAddr, packet: &[u8]) {
        let username = StunMessage::decode(packet)
            .ok()
            .and_then(|message| message.username);
        let targets = if let Some(connection_id) = username
            .as_deref()
            .and_then(connection_id_from_p2p_username)
        {
            self.p2p_peers
                .iter()
                .filter_map(|(session_id, peer)| {
                    (peer.connection_id == connection_id).then_some(*session_id)
                })
                .collect::<Vec<_>>()
        } else {
            self.p2p_peers.keys().copied().collect::<Vec<_>>()
        };

        let mut pending = Vec::new();
        for session_id in targets {
            let Some(peer) = self.p2p_peers.get_mut(&session_id) else {
                continue;
            };
            match peer.agent.handle_inbound(now, src, packet) {
                Ok(actions) if !actions.is_empty() => pending.push((session_id, actions)),
                Ok(_) => {}
                Err(error) => {
                    kvlog::warn!(
                        "p2p stun packet rejected",
                        session_id = session_id.0,
                        addr = %src,
                        error = %error
                    );
                }
            }
        }
        for (session_id, actions) in pending {
            self.apply_p2p_actions(session_id, actions);
        }
    }

    fn handle_p2p_media(&mut self, now: Instant, src: SocketAddr, packet: &[u8]) -> bool {
        let Ok((header, _)) = media::parse_header(packet) else {
            return false;
        };
        let Some(session_id) = self.p2p_peers.iter().find_map(|(session_id, peer)| {
            (peer.recv_key.id == header.key_id).then_some(*session_id)
        }) else {
            return false;
        };

        let outcome = {
            let peer = self
                .p2p_peers
                .get_mut(&session_id)
                .expect("p2p peer exists");
            match media::open_media(&peer.recv_key, &mut peer.recv_replay, packet) {
                Ok((
                    _,
                    MediaPayload::PeerVoice {
                        connection_id,
                        stream_id,
                        sequence,
                        flags,
                        silence_ranges,
                        opus,
                    },
                )) if connection_id == peer.connection_id => {
                    let action = peer.agent.observe_authenticated_packet(now, src);
                    Ok(P2pMediaPacket::Voice {
                        stream_id,
                        sequence,
                        flags,
                        silence_ranges,
                        opus,
                        action,
                    })
                }
                Ok((
                    _,
                    MediaPayload::PeerVoiceFeedback {
                        connection_id,
                        stream_id,
                        feedback,
                    },
                )) if connection_id == peer.connection_id => {
                    let action = peer.agent.observe_authenticated_packet(now, src);
                    Ok(P2pMediaPacket::Feedback {
                        stream_id,
                        feedback,
                        action,
                    })
                }
                Ok(_) => Err("unexpected P2P media payload".to_string()),
                Err(error) => Err(error.to_string()),
            }
        };

        match outcome {
            Ok(P2pMediaPacket::Voice {
                stream_id,
                sequence,
                flags,
                silence_ranges,
                opus,
                action,
            }) => {
                if let Some(action) = action {
                    self.apply_p2p_actions(session_id, vec![action]);
                }
                self.p2p_stream_owners.insert(stream_id, session_id);
                let _ = self
                    .events
                    .send(NetworkEvent::VoicePacket(RemoteVoicePacket {
                        stream_id: stream_id.0,
                        sequence,
                        flags,
                        silence_ranges,
                        payload: opus,
                        received_at: now,
                    }));
            }
            Ok(P2pMediaPacket::Feedback {
                stream_id,
                feedback,
                action,
            }) => {
                if let Some(action) = action {
                    self.apply_p2p_actions(session_id, vec![action]);
                }
                let feedback = live_feedback_from_media(stream_id, feedback);
                self.handle_encoder_feedback(feedback, now);
            }
            Err(error) => {
                kvlog::warn!(
                    "p2p media packet rejected",
                    session_id = session_id.0,
                    addr = %src,
                    error = error.as_str()
                );
            }
        }
        true
    }

    fn send_p2p_voice(
        &mut self,
        stream_id: StreamId,
        sequence: u32,
        flags: u8,
        silence_ranges: u64,
        opus: &[u8],
    ) {
        let mut packets = Vec::new();
        for (session_id, peer) in &mut self.p2p_peers {
            let Some(selected) = peer.agent.selected() else {
                continue;
            };
            let payload = MediaPayload::PeerVoice {
                connection_id: peer.connection_id,
                stream_id,
                sequence,
                flags,
                silence_ranges,
                opus: opus.to_vec(),
            };
            let counter = peer.send_counter;
            peer.send_counter = peer.send_counter.wrapping_add(1);
            match media::seal_media(&peer.send_key, counter, &payload) {
                Ok(packet) => packets.push((*session_id, selected.remote_addr, packet)),
                Err(error) => {
                    kvlog::warn!(
                        "p2p media seal failed",
                        session_id = session_id.0,
                        error = %error
                    );
                }
            }
        }
        for (session_id, addr, packet) in packets {
            self.send_udp_raw("p2p_voice", Some(session_id), addr, &packet);
        }
    }

    fn send_p2p_voice_feedback(
        &mut self,
        session_id: SessionId,
        stream_id: StreamId,
        feedback: LivePlaybackFeedback,
    ) {
        let Some((addr, packet)) = self.p2p_peers.get_mut(&session_id).and_then(|peer| {
            let addr = peer.agent.selected()?.remote_addr;
            let payload = MediaPayload::PeerVoiceFeedback {
                connection_id: peer.connection_id,
                stream_id,
                feedback: media_feedback_from_live(feedback),
            };
            let counter = peer.send_counter;
            peer.send_counter = peer.send_counter.wrapping_add(1);
            Some((addr, media::seal_media(&peer.send_key, counter, &payload)))
        }) else {
            return;
        };
        match packet {
            Ok(packet) => self.send_udp_raw("p2p_voice_feedback", Some(session_id), addr, &packet),
            Err(error) => {
                kvlog::warn!(
                    "p2p feedback seal failed",
                    session_id = session_id.0,
                    error = %error
                );
            }
        }
    }

    fn handle_encoder_feedback(&mut self, feedback: LivePlaybackFeedback, now: Instant) {
        let _ = self.events.send(NetworkEvent::PlaybackFeedback(feedback));
        if self.active_stream != Some(StreamId(feedback.stream_id)) {
            return;
        }
        if let Some(profile) = self.encoder_feedback.observe(feedback, now) {
            kvlog::info!(
                "live encoder DRED profile changed",
                profile = profile.label(),
                packet_loss_percent = profile.packet_loss_percent
            );
            let _ = self
                .events
                .send(NetworkEvent::EncoderProfileChanged(profile));
        }
    }

    fn apply_p2p_actions(&mut self, session_id: SessionId, actions: Vec<P2pAction>) {
        for action in actions {
            match action {
                P2pAction::UseRelay { reason, .. } => {
                    if let Some(user_id) = self.p2p_peers.get(&session_id).map(|peer| peer.user_id)
                    {
                        let _ = self.events.send(NetworkEvent::PeerTransport {
                            user_id,
                            direct: false,
                        });
                    }
                    kvlog::info!(
                        "p2p using relay",
                        session_id = session_id.0,
                        reason = ?reason
                    );
                }
                P2pAction::SendStun { to, bytes, .. }
                | P2pAction::SendStunResponse { to, bytes, .. }
                | P2pAction::SendKeepalive { to, bytes, .. } => {
                    self.send_udp_raw("p2p_stun", Some(session_id), to, &bytes);
                }
                P2pAction::DirectReady { selected } | P2pAction::Migrated { selected } => {
                    let user_id = self.p2p_peers.get(&session_id).map(|peer| peer.user_id);
                    if let Some(user_id) = user_id {
                        let _ = self.events.send(NetworkEvent::PeerTransport {
                            user_id,
                            direct: true,
                        });
                        let _ = self.events.send(NetworkEvent::Status(format!(
                            "p2p direct path to user {}",
                            user_id.0
                        )));
                    }
                    kvlog::info!(
                        "p2p direct path selected",
                        session_id = session_id.0,
                        user_id = user_id.map(|id| id.0),
                        addr = %selected.remote_addr,
                        peer_reflexive = selected.peer_reflexive
                    );
                }
                P2pAction::IceRestart { .. } => {
                    self.request_p2p_restart();
                }
                P2pAction::Disconnected => {
                    kvlog::warn!("p2p direct path timed out", session_id = session_id.0);
                    if let Some(peer) = self.p2p_peers.remove(&session_id) {
                        let _ = self.events.send(NetworkEvent::PeerTransport {
                            user_id: peer.user_id,
                            direct: false,
                        });
                    }
                    let _ = self.events.send(NetworkEvent::Status(
                        "p2p direct path timed out; using relay".to_string(),
                    ));
                }
            }
        }
    }

    fn send_udp_raw(
        &mut self,
        kind: &'static str,
        session_id: Option<SessionId>,
        addr: SocketAddr,
        packet: &[u8],
    ) {
        match self.udp.send_to(packet, addr) {
            Ok(_) => {}
            Err(error) if is_ignorable_udp_error(&error) => {
                kvlog::warn!(
                    "udp send got ignorable socket error",
                    kind,
                    session_id = session_id.map(|id| id.0),
                    addr = %addr,
                    error = %error
                );
            }
            Err(error) => {
                kvlog::warn!(
                    "udp send failed",
                    kind,
                    session_id = session_id.map(|id| id.0),
                    addr = %addr,
                    error = %error
                );
                let _ = self
                    .events
                    .send(NetworkEvent::Error(format!("UDP send failed: {error}")));
            }
        }
    }
}

fn network_command_kind(command: &NetworkCommand) -> &'static str {
    match command {
        NetworkCommand::SendChat(_) => "send_chat",
        NetworkCommand::UploadFile(_) => "upload_file",
        NetworkCommand::LocalVoicePacket(_) => "local_voice_packet",
        NetworkCommand::SequencedLocalVoicePacket { .. } => "sequenced_local_voice_packet",
        NetworkCommand::PlaybackFeedback(_) => "playback_feedback",
        NetworkCommand::Shutdown => "shutdown",
    }
}

fn media_feedback_from_live(feedback: LivePlaybackFeedback) -> media::VoiceFeedback {
    media::VoiceFeedback {
        highest_contiguous_sequence: feedback.highest_contiguous_sequence,
        expected_packets: feedback.expected_packets,
        lost_packets: feedback.lost_packets,
        late_packets: feedback.late_packets,
        duplicate_packets: feedback.duplicate_packets,
        reordered_packets: feedback.reordered_packets,
        window_ms: feedback.window_ms,
        max_queue_ms: feedback.max_queue_ms,
        max_interarrival_jitter_ms: feedback.max_interarrival_jitter_ms,
    }
}

fn live_feedback_from_media(
    stream_id: StreamId,
    feedback: media::VoiceFeedback,
) -> LivePlaybackFeedback {
    LivePlaybackFeedback {
        stream_id: stream_id.0,
        highest_contiguous_sequence: feedback.highest_contiguous_sequence,
        expected_packets: feedback.expected_packets,
        lost_packets: feedback.lost_packets,
        late_packets: feedback.late_packets,
        duplicate_packets: feedback.duplicate_packets,
        reordered_packets: feedback.reordered_packets,
        window_ms: feedback.window_ms,
        max_queue_ms: feedback.max_queue_ms,
        max_interarrival_jitter_ms: feedback.max_interarrival_jitter_ms,
    }
}

fn client_control_kind(control: &ClientControl) -> &'static str {
    match control {
        ClientControl::Authenticate { .. } => "authenticate",
        ClientControl::Pair { .. } => "pair",
        ClientControl::JoinRoom { .. } => "join_room",
        ClientControl::SendChat { .. } => "send_chat",
        ClientControl::StartVoice { .. } => "start_voice",
        ClientControl::StopVoice { .. } => "stop_voice",
        ClientControl::PublishP2p { .. } => "publish_p2p",
        ClientControl::UploadFileStart { .. } => "upload_file_start",
        ClientControl::UploadFileChunk { .. } => "upload_file_chunk",
        ClientControl::UploadFileComplete { .. } => "upload_file_complete",
        ClientControl::UploadFileCancel { .. } => "upload_file_cancel",
        ClientControl::Ping { .. } => "ping",
    }
}

fn server_control_kind(control: &ServerControl) -> &'static str {
    match control {
        ServerControl::Authenticated { .. } => "authenticated",
        ServerControl::RoomJoined { .. } => "room_joined",
        ServerControl::Chat { .. } => "chat",
        ServerControl::Presence { .. } => "presence",
        ServerControl::VoiceStarted { .. } => "voice_started",
        ServerControl::VoiceStopped { .. } => "voice_stopped",
        ServerControl::UdpBound => "udp_bound",
        ServerControl::UdpReflexive { .. } => "udp_reflexive",
        ServerControl::P2pNatProbe { .. } => "p2p_nat_probe",
        ServerControl::P2pPeer { .. } => "p2p_peer",
        ServerControl::P2pPeerGone { .. } => "p2p_peer_gone",
        ServerControl::FileOffered { .. } => "file_offered",
        ServerControl::FileChunk { .. } => "file_chunk",
        ServerControl::FileComplete { .. } => "file_complete",
        ServerControl::FileCanceled { .. } => "file_canceled",
        ServerControl::Pong { .. } => "pong",
        ServerControl::Error { .. } => "error",
    }
}

fn is_auth_failure_code(code: u16) -> bool {
    matches!(
        code,
        ERROR_AUTH_REJECTED
            | ERROR_PAIRING_NOT_ACTIVE
            | ERROR_PAIRING_CODE_MISMATCH
            | ERROR_PAIRING_INVALID_REQUEST
    )
}

fn create_receive_file(dir: &Path, requested_name: &str) -> Result<(PathBuf, File), String> {
    fs::create_dir_all(dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    let name = sanitize_file_name(requested_name);
    let (stem, extension) = split_extension(&name);
    for index in 0u64..10_000 {
        let candidate = if index == 0 {
            name.clone()
        } else {
            format!("{stem}-{index}{extension}")
        };
        let path = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("failed to create {}: {error}", path.display())),
        }
    }
    Err(format!(
        "could not allocate a unique receive path for {}",
        name
    ))
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

fn media_payload_kind(payload: &MediaPayload) -> &'static str {
    match payload {
        MediaPayload::Bind { .. } => "bind",
        MediaPayload::NatProbe { .. } => "nat_probe",
        MediaPayload::Voice { .. } => "voice",
        MediaPayload::PeerVoice { .. } => "peer_voice",
        MediaPayload::VoiceFeedback { .. } => "voice_feedback",
        MediaPayload::PeerVoiceFeedback { .. } => "peer_voice_feedback",
        MediaPayload::Ping { .. } => "ping",
        MediaPayload::Pong { .. } => "pong",
    }
}

fn random_u64() -> Result<u64, String> {
    let rng = ring::rand::SystemRandom::new();
    let mut bytes = [0u8; 8];
    rng.fill(&mut bytes)
        .map_err(|_| "failed to generate random tie breaker".to_string())?;
    Ok(u64::from_le_bytes(bytes).max(1))
}

fn configured_nat_kind() -> P2pNatKind {
    match std::env::var("CHATT_P2P_NAT")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "cone" => P2pNatKind::Cone,
        "symmetric" => P2pNatKind::Symmetric,
        _ => P2pNatKind::Unknown,
    }
}

fn control_candidate(candidate: &Candidate) -> P2pCandidate {
    P2pCandidate {
        id: candidate.id,
        socket_id: candidate.socket_id,
        generation: candidate.generation,
        kind: control_candidate_kind(candidate.kind),
        addr: candidate.addr.to_string(),
        priority: candidate.priority,
        foundation: candidate.foundation.clone(),
        verified: candidate.verified,
    }
}

fn candidate_from_control(candidate: &P2pCandidate) -> Option<Candidate> {
    let addr = candidate.addr.parse().ok()?;
    let mut out = Candidate::new(
        candidate.id,
        candidate_kind_from_control(candidate.kind),
        addr,
    );
    out.socket_id = candidate.socket_id;
    out.generation = candidate.generation;
    out.priority = candidate.priority;
    out.foundation = candidate.foundation.clone();
    out.verified = candidate.verified;
    Some(out)
}

fn control_candidate_kind(kind: CandidateKind) -> P2pCandidateKind {
    match kind {
        CandidateKind::Host => P2pCandidateKind::Host,
        CandidateKind::ServerReflexive => P2pCandidateKind::ServerReflexive,
        CandidateKind::PeerReflexive => P2pCandidateKind::PeerReflexive,
        CandidateKind::PortMapped => P2pCandidateKind::PortMapped,
        CandidateKind::Relay => P2pCandidateKind::Relay,
    }
}

fn candidate_kind_from_control(kind: P2pCandidateKind) -> CandidateKind {
    match kind {
        P2pCandidateKind::Host => CandidateKind::Host,
        P2pCandidateKind::ServerReflexive => CandidateKind::ServerReflexive,
        P2pCandidateKind::PeerReflexive => CandidateKind::PeerReflexive,
        P2pCandidateKind::PortMapped => CandidateKind::PortMapped,
        P2pCandidateKind::Relay => CandidateKind::Relay,
    }
}

fn nat_from_control(kind: P2pNatKind) -> NatKind {
    match kind {
        P2pNatKind::Unknown => NatKind::Unknown,
        P2pNatKind::Cone => NatKind::Cone,
        P2pNatKind::Symmetric => NatKind::Symmetric,
    }
}

fn control_nat_kind(kind: NatKind) -> P2pNatKind {
    match kind {
        NatKind::Unknown => P2pNatKind::Unknown,
        NatKind::Cone => P2pNatKind::Cone,
        NatKind::Symmetric => P2pNatKind::Symmetric,
    }
}

fn ice_role_from_control(role: P2pRole) -> IceRole {
    match role {
        P2pRole::Controlling => IceRole::Controlling,
        P2pRole::Controlled => IceRole::Controlled,
    }
}

fn key_from_control(key: &P2pKey) -> Result<KeyMaterial, String> {
    let bytes: [u8; KEY_LEN] = key
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| "invalid P2P key length".to_string())?;
    Ok(KeyMaterial { id: key.id, bytes })
}

fn p2p_username(connection_id: u64) -> String {
    format!("chatt-p2p:{connection_id}")
}

fn connection_id_from_p2p_username(username: &str) -> Option<u64> {
    username.strip_prefix("chatt-p2p:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reconnect_schedule_matches_retry_policy() {
        let mut schedule = ReconnectSchedule::new();
        let delays = (0..12)
            .map(|_| schedule.next_delay().as_secs())
            .collect::<Vec<_>>();

        assert_eq!(delays, vec![1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 5, 5]);
    }

    #[test]
    fn reconnect_schedule_resets_after_connected_session() {
        let mut schedule = ReconnectSchedule::new();
        for _ in 0..8 {
            schedule.next_delay();
        }

        schedule.reset();

        assert_eq!(schedule.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn encoder_feedback_controller_escalates_without_bitrate_policy() {
        let start = Instant::now();
        let mut controller = EncoderFeedbackController::new();

        let profile = controller
            .observe(feedback_window(100, 60, 0, 80, 120), start)
            .unwrap();

        assert_eq!(profile, LiveEncoderProfile::DRED_60);
        assert_eq!(profile.packet_loss_percent, 60);
    }

    #[test]
    fn encoder_feedback_controller_holds_and_decays_one_level() {
        let start = Instant::now();
        let mut controller = EncoderFeedbackController::new();
        assert_eq!(
            controller.observe(feedback_window(100, 60, 0, 80, 120), start),
            Some(LiveEncoderProfile::DRED_60)
        );

        assert_eq!(
            controller.observe(
                feedback_window(100, 0, 0, 20, 20),
                start + ENCODER_PROFILE_HOLD - Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(
            controller.observe(
                feedback_window(100, 0, 0, 20, 20),
                start + ENCODER_PROFILE_HOLD + Duration::from_millis(1)
            ),
            Some(LiveEncoderProfile::DRED_50)
        );
    }

    #[test]
    fn receive_file_path_preserves_extension_when_colliding() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-client-net-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("report.pdf"), b"existing").unwrap();

        let (path, _file) = create_receive_file(&dir, "report.pdf").unwrap();

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("report-1.pdf")
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sanitize_file_name_removes_path_components() {
        assert_eq!(sanitize_file_name("../unsafe/report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("bad/name?.txt"), "name_.txt");
    }

    fn feedback_window(
        expected_packets: u16,
        lost_packets: u16,
        late_packets: u16,
        max_queue_ms: u16,
        max_interarrival_jitter_ms: u16,
    ) -> LivePlaybackFeedback {
        LivePlaybackFeedback {
            stream_id: 1,
            highest_contiguous_sequence: 10,
            expected_packets,
            lost_packets,
            late_packets,
            duplicate_packets: 0,
            reordered_packets: 0,
            window_ms: 500,
            max_queue_ms,
            max_interarrival_jitter_ms,
        }
    }
}
