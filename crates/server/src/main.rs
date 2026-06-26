use hashbrown::{HashMap, HashSet};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::SocketAddr,
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod config;
mod local_admin;

use mio::{
    Events, Interest, Poll, Token,
    net::{TcpListener, TcpStream, UdpSocket},
};
use ring::rand::SecureRandom;
use ring::signature::KeyPair;
use rpc::{
    control::{
        self, ChatMessage, ClientControl, ERROR_AUTH_REJECTED, ERROR_PAIRING_CODE_MISMATCH,
        ERROR_PAIRING_INVALID_REQUEST, ERROR_PAIRING_NOT_ACTIVE, FileMetadata, InviteTicket,
        MAX_FILE_CHUNK_BYTES, P2pCandidate, P2pKey, P2pNatKind, P2pPeerInfo, P2pRole, RoomInfo,
        ServerControl, decode_client_control, decode_client_hello, encode_invite_ticket,
        encode_server_control, encode_server_hello,
    },
    crypto::{
        AntiReplay, CHANNEL_CONTROL, ControlTransport, KEY_LEN, KeyMaterial, SessionSecrets,
        encode_hex, respond_to_client_hello, respond_to_client_hello_plaintext,
    },
    frame,
    ids::{FileTransferId, MessageId, RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload},
};

use config::{Config as ServerConfig, UserConfig, hash_secret, value_arg, verify_secret_hash};
use local_admin::{AdminCommand, AdminSocket};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const LISTENER: Token = Token(0);
const UDP: Token = Token(1);
const UDP_PROBE: Token = Token(2);
const FIRST_CLIENT: usize = 3;
const DEFAULT_ROOM: RoomId = RoomId(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(20);
const MAX_CLIENTS: usize = 1024;
const MEDIA_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
const INVITE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    // `--logfile PATH` (or CHATT_LOGFILE) writes the same kvlog stream the
    // client `--logfile` produces, so server and client traces can be analyzed
    // together. Without it, logging stays on the env-configured collector.
    let logfile = value_arg(&args, "--logfile").or_else(|| std::env::var("CHATT_LOGFILE").ok());
    let _logger = match logfile {
        Some(logfile) => kvlog::collector::init_file_logger(&logfile),
        None => kvlog::spawn_collector_from_env(Some("chatt-server"), false),
    };
    if args.get(1).is_some_and(|arg| arg == "invite") {
        let user = args
            .get(2)
            .ok_or_else(|| invalid_config("usage: chatt-server invite USER".to_string()))?;
        if args.len() != 3 || user.trim().is_empty() {
            return Err(invalid_config("usage: chatt-server invite USER".to_string()).into());
        }
        let join_string = local_admin::send_invite(user).map_err(invalid_config)?;
        println!("{join_string}");
        return Ok(());
    }

    let config_path = value_arg(&args, "--config");
    let config = ServerConfig::load(config_path.as_deref()).map_err(invalid_config)?;
    let server_public_key = config.server_public_key_hex().map_err(invalid_config)?;
    let udp_probe_addr = config.network.udp_probe_addr;
    let udp_probe_label = udp_probe_addr
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "disabled".to_string());
    kvlog::info!(
        "server starting",
        tcp_addr = %config.network.tcp_addr,
        udp_addr = %config.network.udp_addr,
        udp_probe_addr = udp_probe_label.as_str(),
        server_public_key = server_public_key.as_str(),
        encryption = config.security.encryption,
        p2p_enabled = config.network.p2p_enabled
    );
    let tcp_addr = config.network.tcp_addr;
    let udp_addr = config.network.udp_addr;
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

struct Server {
    config: ServerConfig,
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
    next_token: usize,
    next_session: u64,
    next_message: u64,
    next_stream: u32,
    next_connection_id: u64,
    next_file_transfer: u64,
    active_uploads: HashMap<(SessionId, FileTransferId), ServerUpload>,
    reserved_file_names: HashSet<String>,
    chat_history_limit: usize,
    file_size_limit_bytes: u64,
    invites: HashMap<String, InviteState>,
    rng: ring::rand::SystemRandom,
    server_key_pair: ring::signature::Ed25519KeyPair,
    next_media_sweep_at: Option<Instant>,
}

impl Server {
    fn bind(config: ServerConfig) -> io::Result<Self> {
        let tcp_addr = config.network.tcp_addr;
        let udp_addr = config.network.udp_addr;
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

        let mut rooms = HashMap::new();
        for room in &config.rooms {
            rooms.insert(
                room.room_id(),
                RoomState {
                    id: room.room_id(),
                    name: room.name.clone(),
                    members: HashSet::new(),
                    history: VecDeque::new(),
                    active_streams: HashMap::new(),
                },
            );
        }
        if rooms.is_empty() {
            rooms.insert(
                DEFAULT_ROOM,
                RoomState {
                    id: DEFAULT_ROOM,
                    name: "lobby".to_string(),
                    members: HashSet::new(),
                    history: VecDeque::new(),
                    active_streams: HashMap::new(),
                },
            );
        }

        let server_key_pair = config
            .server_key_pair()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        kvlog::info!(
            "server identity loaded",
            public_key = encode_hex(server_key_pair.public_key().as_ref()).as_str()
        );
        let chat_history_limit =
            usize::try_from(config.security.chat_history_limit).unwrap_or(usize::MAX);
        let file_size_limit_bytes = config.security.max_file_size_bytes;

        Ok(Self {
            config,
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
            next_token: FIRST_CLIENT,
            next_session: 1,
            next_message: 1,
            next_stream: 1,
            next_connection_id: 1,
            next_file_transfer: 1,
            active_uploads: HashMap::new(),
            reserved_file_names: HashSet::new(),
            chat_history_limit,
            file_size_limit_bytes,
            invites: HashMap::new(),
            rng: ring::rand::SystemRandom::new(),
            server_key_pair,
            next_media_sweep_at: None,
        })
    }

    fn run(
        &mut self,
        admin_rx: &mpsc::Receiver<AdminCommand>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut events = Events::with_capacity(256);
        loop {
            if let Err(error) = self.poll.poll(&mut events, Some(POLL_TIMEOUT)) {
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
                    token => {
                        if event.is_readable() {
                            self.read_client(token);
                        }
                        if event.is_writable() {
                            self.write_client(token);
                        }
                    }
                }
            }
            self.handle_admin_commands(admin_rx);
            self.flush_disconnects();
            self.sweep_stale_media_keys(Instant::now());
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

        let pairing_code = random_secret_hex(&self.rng)?;
        let ticket = InviteTicket {
            version: rpc::PROTOCOL_VERSION,
            user: user_name.to_string(),
            pairing_code: pairing_code.clone(),
            tcp_addr: self.config.network.public_tcp_addr.clone(),
            udp_addr: self.config.network.public_udp_addr.clone(),
            udp_probe_addr: self.config.network.public_udp_probe_addr.clone(),
            server_public_key: encode_hex(self.server_key_pair.public_key().as_ref()),
            room_id: DEFAULT_ROOM.0,
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
            self.clients.insert(
                token,
                ClientConn {
                    socket,
                    read_buf: Vec::new(),
                    write_buf: Vec::new(),
                    state: ConnState::AwaitClientHello,
                    control: None,
                    secrets: None,
                    session_id: None,
                    user_id: None,
                    disconnect: false,
                },
            );
        }
    }

    fn read_client(&mut self, token: Token) {
        let mut disconnected = false;
        if let Some(client) = self.clients.get_mut(&token) {
            let mut buf = [0u8; 8192];
            loop {
                match client.socket.read(&mut buf) {
                    Ok(0) => {
                        kvlog::info!("tcp client closed", token = token.0);
                        disconnected = true;
                        break;
                    }
                    Ok(n) => {
                        client.read_buf.extend_from_slice(&buf[..n]);
                        kvlog::info!(
                            "tcp client bytes received",
                            token = token.0,
                            size = n,
                            buffered = client.read_buf.len()
                        );
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        kvlog::warn!("tcp client read failed", token = token.0, error = %error);
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        if disconnected {
            self.disconnect(token);
            return;
        }

        loop {
            let frame = match self.clients.get_mut(&token) {
                Some(client) => match frame::pop_frame(&mut client.read_buf) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(error) => {
                        kvlog::warn!(
                            "tcp client sent invalid frame",
                            token = token.0,
                            error = %error
                        );
                        self.disconnect(token);
                        break;
                    }
                },
                None => break,
            };

            if let Err(error) = self.process_frame(token, frame) {
                kvlog::warn!("tcp client protocol error", token = token.0, error = %error);
                self.disconnect(token);
                break;
            }
        }
    }

    fn process_frame(&mut self, token: Token, frame_bytes: Vec<u8>) -> Result<(), String> {
        let state = self
            .clients
            .get(&token)
            .map(|client| client.state)
            .ok_or_else(|| "unknown client token".to_string())?;

        kvlog::info!(
            "tcp frame processing",
            token = token.0,
            state = conn_state_name(state),
            size = frame_bytes.len()
        );
        match state {
            ConnState::AwaitClientHello => {
                let hello = decode_client_hello(&frame_bytes)?;
                kvlog::info!(
                    "client hello decoded",
                    token = token.0,
                    client_nonce_size = hello.client_nonce.len(),
                    client_ephemeral_size = hello.client_ephemeral.len()
                );
                let encryption = self.config.security.encryption;
                let (server_hello, control, secrets) = if encryption {
                    let response =
                        respond_to_client_hello(&self.rng, &self.server_key_pair, &hello)
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
                frame::encode_frame(&encoded, &mut client.write_buf)
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
            ConnState::AwaitAuth | ConnState::Ready => {
                let plaintext = {
                    let client = self
                        .clients
                        .get_mut(&token)
                        .ok_or_else(|| "unknown client token".to_string())?;
                    client
                        .control
                        .as_mut()
                        .ok_or_else(|| "missing control cipher".to_string())?
                        .open_next(CHANNEL_CONTROL, &frame_bytes)
                        .map_err(|error| error.to_string())?
                };
                kvlog::info!(
                    "client control decrypted",
                    token = token.0,
                    payload_size = plaintext.len()
                );
                let control = decode_client_control(&plaintext)?;
                kvlog::info!(
                    "client control decoded",
                    token = token.0,
                    kind = client_control_kind(&control)
                );
                self.handle_control(token, control)
            }
        }
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
                    user,
                    token: auth_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.authenticate_client(
                token,
                &user,
                &auth_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (
                ConnState::AwaitAuth,
                ClientControl::Pair {
                    user,
                    display_name,
                    pairing_code,
                    token: new_token,
                    receive_files,
                    file_receive_limit_bytes,
                },
            ) => self.pair_client(
                token,
                &user,
                &display_name,
                &pairing_code,
                &new_token,
                receive_files,
                file_receive_limit_bytes,
            ),
            (ConnState::AwaitAuth, _) => Err("authenticate before sending control messages".into()),
            (ConnState::Ready, ClientControl::Authenticate { .. } | ClientControl::Pair { .. }) => {
                Err("session is already authenticated".into())
            }
            (ConnState::Ready, ClientControl::JoinRoom { room_id }) => {
                let session_id = self.session_for_token(token)?;
                self.join_room(session_id, room_id);
                Ok(())
            }
            (ConnState::Ready, ClientControl::SendChat { room_id, body }) => {
                let session_id = self.session_for_token(token)?;
                self.send_chat(session_id, room_id, body)
            }
            (ConnState::Ready, ClientControl::StartVoice { room_id }) => {
                let session_id = self.session_for_token(token)?;
                self.start_voice(session_id, room_id)
            }
            (ConnState::Ready, ClientControl::StopVoice { stream_id }) => {
                kvlog::info!(
                    "client stop voice ignored; voice follows room membership",
                    stream_id = stream_id.0
                );
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
                self.publish_p2p(
                    session_id,
                    room_id,
                    generation,
                    nat,
                    tie_breaker,
                    candidates,
                )
            }
            (
                ConnState::Ready,
                ClientControl::UploadFileStart {
                    room_id,
                    transfer_id,
                    name,
                    size,
                },
            ) => {
                let session_id = self.session_for_token(token)?;
                self.start_file_upload(session_id, room_id, transfer_id, name, size)
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
            (ConnState::Ready, ClientControl::Ping { nonce }) => {
                self.send_control_to_token(token, &ServerControl::Pong { nonce })
            }
            (ConnState::AwaitClientHello, _) => Err("handshake is not complete".into()),
        }
    }

    fn authenticate_client(
        &mut self,
        token: Token,
        user_name: &str,
        auth_token: &str,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        kvlog::info!("authenticate attempt", token = token.0, user = user_name);
        let Some(user) = self
            .config
            .users
            .iter()
            .find(|candidate| {
                candidate.name == user_name
                    && !candidate.token_hash.trim().is_empty()
                    && verify_secret_hash(&candidate.token_hash, auth_token)
            })
            .cloned()
        else {
            kvlog::warn!(
                "authenticate rejected",
                token = token.0,
                user = user_name,
                reason = "invalid_user_or_token"
            );
            return self.reject_auth(
                token,
                ERROR_AUTH_REJECTED,
                format!(
                    "authentication failed for '{user_name}': the user or token is not valid for this server"
                ),
            );
        };
        self.establish_session(token, &user, receive_files, file_receive_limit_bytes)
    }

    fn pair_client(
        &mut self,
        token: Token,
        user_name: &str,
        display_name: &str,
        pairing_code: &str,
        new_token: &str,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    ) -> Result<(), String> {
        kvlog::info!("pairing attempt", token = token.0, user = user_name);
        self.expire_invites();
        let Some(invite) = self.invites.get(user_name) else {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                user = user_name,
                reason = "no_active_invite"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_NOT_ACTIVE,
                format!(
                    "pairing failed for '{user_name}': no active invite exists on this server; the invite may have expired, been replaced, or already been used. Ask the admin to run `chatt-server invite {user_name}` again"
                ),
            );
        };
        if !verify_secret_hash(&invite.pairing_code_hash, pairing_code) {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                user = user_name,
                reason = "pairing_code_mismatch"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_CODE_MISMATCH,
                format!(
                    "pairing failed for '{user_name}': the join string secret does not match the active invite; use the newest join string from `chatt-server invite {user_name}`"
                ),
            );
        }
        let display_name = display_name.trim();
        if display_name.is_empty() || display_name.len() > 64 {
            kvlog::warn!(
                "pairing rejected",
                token = token.0,
                user = user_name,
                reason = "invalid_display_name"
            );
            return self.reject_auth(
                token,
                ERROR_PAIRING_INVALID_REQUEST,
                "pairing failed: display name must be 1-64 bytes".to_string(),
            );
        }

        let token_hash = hash_secret(new_token);
        let user = self
            .config
            .mark_user_paired(user_name, display_name.to_string(), token_hash)?;
        self.invites.remove(user_name);
        kvlog::info!(
            "pairing accepted",
            token = token.0,
            user_id = user.id,
            user = user.name.as_str()
        );
        self.establish_session(token, &user, receive_files, file_receive_limit_bytes)
    }

    fn reject_auth(&mut self, token: Token, code: u16, message: String) -> Result<(), String> {
        self.send_control_to_token(token, &ServerControl::Error { code, message })?;
        if let Some(client) = self.clients.get_mut(&token) {
            client.disconnect = true;
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
    ) -> Result<(), String> {
        let session_id = SessionId(self.next_session);
        self.next_session += 1;
        let user_id = user.user_id();
        let user_name = user.display_name.clone();

        let secrets = self
            .clients
            .get(&token)
            .and_then(|client| client.secrets.clone());
        if let Some(secrets) = &secrets {
            self.media_key_to_session
                .insert(secrets.media_recv.id, session_id);
        }
        self.sessions.insert(
            session_id,
            Session {
                user_id,
                user_name: user_name.clone(),
                tcp_token: token,
                room_id: None,
                udp_addr: None,
                secrets,
                media_send_counter: 0,
                media_recv_replay: AntiReplay::new(),
                active_stream: None,
                voice_status: control::ParticipantVoiceStatus::default(),
                p2p: None,
                receive_files,
                file_receive_limit_bytes,
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
            user = user_name.as_str(),
            receive_files,
            file_receive_limit_bytes
        );
        let rooms = self.room_infos();
        self.send_control_to_token(
            token,
            &ServerControl::Authenticated {
                session_id,
                user_id,
                rooms,
                current_room: Some(DEFAULT_ROOM),
            },
        )?;
        if self.live_token_for_session(session_id).is_none() {
            return Ok(());
        }
        self.join_room(session_id, DEFAULT_ROOM);
        Ok(())
    }

    fn room_infos(&self) -> Vec<RoomInfo> {
        self.rooms
            .values()
            .map(|room| RoomInfo {
                room_id: room.id,
                name: room.name.clone(),
                participants: room.members.len() as u32,
            })
            .collect()
    }

    fn join_room(&mut self, session_id: SessionId, room_id: RoomId) {
        kvlog::info!(
            "join room requested",
            session_id = session_id.0,
            room_id = room_id.0
        );
        if !self.rooms.contains_key(&room_id) {
            kvlog::warn!(
                "join room rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                error = "room not found"
            );
            if let Some(token) = self.live_token_for_session(session_id) {
                let _ = self.send_control_to_token(
                    token,
                    &ServerControl::Error {
                        code: 404,
                        message: "room not found".to_string(),
                    },
                );
            }
            return;
        }

        if let Some(previous) = self.sessions.get(&session_id).and_then(|s| s.room_id) {
            if previous != room_id {
                kvlog::info!(
                    "leaving previous room before join",
                    session_id = session_id.0,
                    previous_room_id = previous.0,
                    room_id = room_id.0
                );
                self.leave_room(session_id, previous, None);
                if self.live_token_for_session(session_id).is_none() {
                    return;
                }
            }
        }

        let Some(session) = self.sessions.get_mut(&session_id) else {
            return;
        };
        session.room_id = Some(room_id);
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.members.insert(session_id);
            kvlog::info!(
                "room membership updated",
                session_id = session_id.0,
                room_id = room_id.0,
                member_count = room.members.len()
            );
        }

        let voice_started = match self.ensure_voice_stream(session_id, room_id) {
            Ok(voice) => Some(voice),
            Err(error) => {
                kvlog::warn!(
                    "automatic voice stream failed",
                    session_id = session_id.0,
                    room_id = room_id.0,
                    error = error.as_str()
                );
                None
            }
        };

        let participant = self.participant_for_session(session_id);
        if let Some(participant) = participant.clone() {
            self.broadcast_control(
                room_id,
                &ServerControl::Presence {
                    room_id,
                    participant,
                    online: true,
                },
            );
        }

        let (history, participants, token) = match (
            self.rooms.get(&room_id),
            self.live_token_for_session(session_id),
        ) {
            (Some(room), Some(token)) => (
                room.history.iter().cloned().collect::<Vec<_>>(),
                self.participants(room_id),
                token,
            ),
            _ => return,
        };
        let _ = self.send_control_to_token(
            token,
            &ServerControl::RoomJoined {
                room_id,
                history,
                participants,
            },
        );
        if self.live_token_for_session(session_id).is_none() {
            return;
        }
        self.send_existing_voice_streams_to_token(room_id, session_id, token);
        if self.live_token_for_session(session_id).is_none() {
            return;
        }
        if let Some((user_id, stream_id)) = voice_started {
            self.broadcast_voice_started(room_id, user_id, stream_id);
        }
        kvlog::info!(
            "room joined sent",
            session_id = session_id.0,
            room_id = room_id.0
        );
    }

    fn leave_room(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        excluded_broadcast_session: Option<SessionId>,
    ) {
        kvlog::info!(
            "leave room requested",
            session_id = session_id.0,
            room_id = room_id.0
        );
        self.stop_voice(session_id, None, excluded_broadcast_session);
        self.broadcast_p2p_gone(session_id, room_id);
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.members.remove(&session_id);
        }
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.room_id = None;
            session.p2p = None;
        }
        self.remove_peer_links(session_id);
        let participant = self.participant_for_session(session_id);
        if let Some(participant) = participant {
            self.broadcast_control(
                room_id,
                &ServerControl::Presence {
                    room_id,
                    participant,
                    online: false,
                },
            );
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
        let tokens = self
            .rooms
            .get(&room_id)
            .map(|room| {
                room.members
                    .iter()
                    .copied()
                    .filter(|member| *member != session_id)
                    .filter_map(|member| self.live_token_for_session(member))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for token in tokens {
            let _ = self.send_control_to_token(
                token,
                &ServerControl::P2pPeerGone {
                    session_id,
                    user_id,
                },
            );
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
        let (sender, sender_name, member) = match self.sessions.get(&session_id) {
            Some(session) => (
                session.user_id,
                session.user_name.clone(),
                session.room_id == Some(room_id),
            ),
            None => {
                kvlog::warn!(
                    "chat send rejected",
                    session_id = session_id.0,
                    error = "unknown session"
                );
                return Err("unknown session".into());
            }
        };
        if !member {
            kvlog::warn!(
                "chat send rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                error = "not a room member"
            );
            return Err("join the room before sending chat".into());
        }
        let message = ChatMessage {
            message_id: MessageId(self.next_message),
            room_id,
            sender,
            sender_name,
            timestamp_ms: now_ms(),
            body,
        };
        self.next_message += 1;
        let mut history_len = 0;
        if self.chat_history_limit > 0
            && let Some(room) = self.rooms.get_mut(&room_id)
        {
            room.history.push_back(message.clone());
            while room.history.len() > self.chat_history_limit {
                room.history.pop_front();
            }
            history_len = room.history.len();
        }
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
        size: u64,
    ) -> Result<(), String> {
        kvlog::info!(
            "file upload start requested",
            session_id = session_id.0,
            room_id = room_id.0,
            client_transfer_id = client_transfer_id.0,
            file_size = size
        );
        if size > self.file_size_limit_bytes {
            return Err("file exceeds server maximum length".into());
        }
        let key = (session_id, client_transfer_id);
        if self.active_uploads.contains_key(&key) {
            return Err("file transfer id is already active".into());
        }
        let (sender, sender_name, member) = match self.sessions.get(&session_id) {
            Some(session) => (
                session.user_id,
                session.user_name.clone(),
                session.room_id == Some(room_id),
            ),
            None => return Err("unknown session".into()),
        };
        if !member {
            return Err("join the room before uploading files".into());
        }

        let Some(member_ids) = self
            .rooms
            .get(&room_id)
            .map(|room| room.members.iter().copied().collect::<Vec<_>>())
        else {
            return Err("room not found".into());
        };

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
                self.sessions.get(member_id).is_some_and(|session| {
                    session.receive_files && size <= session.file_receive_limit_bytes
                })
            })
            .collect::<HashSet<_>>();
        let metadata = FileMetadata {
            transfer_id: server_transfer_id,
            room_id,
            sender,
            sender_name: sender_name.clone(),
            file_name: file_name.clone(),
            original_name,
            size,
            timestamp_ms,
        };

        let message = ChatMessage {
            message_id: MessageId(self.next_message),
            room_id,
            sender,
            sender_name,
            timestamp_ms,
            body: format!("sent file `{}` ({})", file_name, format_bytes(size)),
        };
        self.next_message += 1;
        if self.chat_history_limit > 0
            && let Some(room) = self.rooms.get_mut(&room_id)
        {
            room.history.push_back(message.clone());
            while room.history.len() > self.chat_history_limit {
                room.history.pop_front();
            }
        }
        self.broadcast_control(room_id, &ServerControl::Chat { message });
        if self.live_token_for_session(session_id).is_none() {
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

        self.active_uploads.insert(
            key,
            ServerUpload {
                server_transfer_id,
                room_id,
                size,
                received: 0,
                recipients,
            },
        );
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
            if upload.received != offset {
                return Err("file chunk offset mismatch".into());
            }
            let end = offset.saturating_add(data.len() as u64);
            if end > upload.size {
                return Err("file chunk exceeds declared file size".into());
            }
            upload.received = end;
            (upload.server_transfer_id, upload.recipients.clone())
        };
        kvlog::info!(
            "file chunk relaying",
            session_id = session_id.0,
            client_transfer_id = client_transfer_id.0,
            server_transfer_id = server_transfer_id.0,
            offset,
            chunk_size = data.len(),
            recipient_count = recipients.len()
        );
        let mut disconnected_recipients = Vec::new();
        for recipient in recipients {
            let Some(token) = self.live_token_for_session(recipient) else {
                disconnected_recipients.push(recipient);
                continue;
            };
            let _ = self.send_control_to_token(
                token,
                &ServerControl::FileChunk {
                    transfer_id: server_transfer_id,
                    offset,
                    data: data.clone(),
                },
            );
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
        if upload.received != upload.size {
            self.send_file_canceled(&upload, "upload ended before all bytes arrived");
            return Err("file upload ended before all bytes arrived".into());
        }
        kvlog::info!(
            "file upload completed",
            session_id = session_id.0,
            room_id = upload.room_id.0,
            client_transfer_id = client_transfer_id.0,
            server_transfer_id = upload.server_transfer_id.0,
            file_size = upload.size
        );
        for recipient in &upload.recipients {
            let Some(token) = self.live_token_for_session(*recipient) else {
                continue;
            };
            let _ = self.send_control_to_token(
                token,
                &ServerControl::FileComplete {
                    transfer_id: upload.server_transfer_id,
                },
            );
        }
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

    fn start_voice(&mut self, session_id: SessionId, room_id: RoomId) -> Result<(), String> {
        let already_active = self
            .sessions
            .get(&session_id)
            .and_then(|session| session.active_stream)
            .is_some();
        let (user_id, stream_id) = self.ensure_voice_stream(session_id, room_id)?;
        if !already_active {
            self.broadcast_voice_started(room_id, user_id, stream_id);
        }
        Ok(())
    }

    fn ensure_voice_stream(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
    ) -> Result<(UserId, StreamId), String> {
        let (user_id, in_room) = match self.sessions.get(&session_id) {
            Some(session) => (session.user_id, session.room_id == Some(room_id)),
            None => return Err("unknown session".into()),
        };
        if !in_room {
            kvlog::warn!(
                "voice start rejected",
                session_id = session_id.0,
                room_id = room_id.0,
                error = "not a room member"
            );
            return Err("join the room before starting voice".into());
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

    fn broadcast_voice_started(&mut self, room_id: RoomId, user_id: UserId, stream_id: StreamId) {
        self.broadcast_control(
            room_id,
            &ServerControl::VoiceStarted {
                room_id,
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
                    user_id,
                    stream_id,
                },
            );
        }
    }

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
                (session.room_id, session.user_id, stream_id)
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
        let Some(room_id) = session.room_id else {
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
        let in_room = self
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.room_id == Some(room_id));
        if !in_room {
            return Err("join the room before publishing P2P candidates".into());
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

        let peers = self
            .rooms
            .get(&room_id)
            .map(|room| {
                room.members
                    .iter()
                    .copied()
                    .filter(|peer| *peer != session_id)
                    .filter(|peer| self.live_token_for_session(*peer).is_some())
                    .filter(|peer| {
                        self.sessions
                            .get(peer)
                            .and_then(|s| s.p2p.as_ref())
                            .is_some()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

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
            room_id: recipient_session.room_id.unwrap_or(DEFAULT_ROOM),
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
        let (session_id, payload) = if header.key_id == media::PLAINTEXT_KEY_ID {
            self.open_plaintext_udp_packet(src, packet)?
        } else {
            let session_id = *self
                .media_key_to_session
                .get(&header.key_id)
                .ok_or_else(|| "unknown UDP key id".to_string())?;
            let payload = {
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
                session.udp_addr = Some(src);
                payload
            };
            (session_id, payload)
        };

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
                flags,
                payload,
            } => self.relay_voice(session_id, stream_id, sequence, flags, payload),
            MediaPayload::VoiceFeedback {
                stream_id,
                feedback,
            } => self.relay_voice_feedback(session_id, stream_id, feedback),
            MediaPayload::PeerVoice { .. } => Ok(()),
            MediaPayload::PeerVoiceFeedback { .. } => Ok(()),
            MediaPayload::Ping { nonce } => {
                self.send_udp_payload(session_id, &MediaPayload::Pong { nonce });
                Ok(())
            }
            MediaPayload::Pong { .. } => Ok(()),
        }
    }

    fn open_plaintext_udp_packet(
        &mut self,
        src: SocketAddr,
        packet: &[u8],
    ) -> Result<(SessionId, MediaPayload), String> {
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
            session.udp_addr.replace(src)
        };
        if let Some(old_addr) = old_addr
            && old_addr != src
            && self.plaintext_addr_to_session.get(&old_addr) == Some(&session_id)
        {
            self.plaintext_addr_to_session.remove(&old_addr);
        }
        self.plaintext_addr_to_session.insert(src, session_id);
        Ok((session_id, payload))
    }

    fn relay_voice(
        &mut self,
        sender_session_id: SessionId,
        stream_id: StreamId,
        sequence: u32,
        flags: u8,
        voice_payload: media::VoicePayload,
    ) -> Result<(), String> {
        let room_id = match self.sessions.get(&sender_session_id) {
            Some(session)
                if session.active_stream == Some(stream_id) && session.room_id.is_some() =>
            {
                session.room_id.unwrap()
            }
            _ => return Ok(()),
        };
        let recipients = self
            .rooms
            .get(&room_id)
            .map(|room| {
                room.members
                    .iter()
                    .copied()
                    .filter(|session_id| *session_id != sender_session_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        kvlog::info!(
            "voice packet relaying",
            session_id = sender_session_id.0,
            room_id = room_id.0,
            stream_id = stream_id.0,
            sequence,
            recipient_count = recipients.len(),
            payload_size = voice_payload.len()
        );
        let payload = MediaPayload::Voice {
            stream_id,
            sequence,
            flags,
            payload: voice_payload,
        };
        for session_id in recipients {
            self.send_udp_payload(session_id, &payload);
        }
        Ok(())
    }

    fn relay_voice_feedback(
        &mut self,
        receiver_session_id: SessionId,
        stream_id: StreamId,
        feedback: media::VoiceFeedback,
    ) -> Result<(), String> {
        let room_id = match self.sessions.get(&receiver_session_id) {
            Some(session) => session.room_id,
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
        kvlog::info!(
            "voice feedback relaying",
            receiver_session_id = receiver_session_id.0,
            owner_session_id = owner_session_id.0,
            room_id = room_id.0,
            stream_id = stream_id.0,
            expected = feedback.expected_packets,
            lost = feedback.lost_packets,
            late = feedback.late_packets,
            queue_ms = feedback.max_queue_ms
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
        let counter = session.media_send_counter;
        session.media_send_counter = session.media_send_counter.wrapping_add(1);
        let packet = match &session.secrets {
            Some(secrets) => media::seal_media(&secrets.media_send, counter, payload),
            None => media::seal_plaintext_media(counter, payload),
        };
        match packet {
            Ok(packet) => {
                if let Err(error) = self.udp.send_to(&packet, addr) {
                    kvlog::warn!(
                        "udp send failed",
                        session_id = session_id.0,
                        addr = %addr,
                        packet_size = packet.len(),
                        error = %error
                    );
                }
            }
            Err(error) => {
                kvlog::warn!("udp seal failed", session_id = session_id.0, error = %error);
            }
        }
    }

    fn send_control_to_token(
        &mut self,
        token: Token,
        control: &ServerControl,
    ) -> Result<(), String> {
        let kind = server_control_kind(control);
        let payload = encode_server_control(control);
        let payload_size = payload.len();
        let client = self
            .clients
            .get_mut(&token)
            .ok_or_else(|| "unknown client token".to_string())?;
        let encrypted = client
            .control
            .as_mut()
            .ok_or_else(|| "missing control cipher".to_string())?
            .seal_next(CHANNEL_CONTROL, &payload)
            .map_err(|error| error.to_string())?;
        let encrypted_size = encrypted.len();
        frame::encode_frame(&encrypted, &mut client.write_buf)
            .map_err(|error| error.to_string())?;
        kvlog::info!(
            "server control queued",
            token = token.0,
            kind,
            payload_size,
            encrypted_size,
            queued_bytes = client.write_buf.len()
        );
        self.write_client(token);
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
        let tokens = room_recipient_tokens(
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
        for token in tokens {
            if let Err(error) = self.send_control_to_token(token, control) {
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
            while !client.write_buf.is_empty() {
                match client.socket.write(&client.write_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        client.write_buf.drain(..n);
                        kvlog::info!(
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
            if client.disconnect && client.write_buf.is_empty() {
                disconnected = true;
            }
        }
        if disconnected {
            self.disconnect(token);
        }
    }

    fn flush_disconnects(&mut self) {
        let tokens = self
            .clients
            .iter()
            .filter_map(|(token, client)| {
                (client.disconnect && client.write_buf.is_empty()).then_some(*token)
            })
            .collect::<Vec<_>>();
        for token in tokens {
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
        if let Some(session_id) = client.session_id {
            self.cancel_uploads_for_session(session_id, "sender disconnected");
            let room_id = self.sessions.get(&session_id).and_then(|s| s.room_id);
            if let Some(room_id) = room_id {
                self.leave_room(session_id, room_id, Some(session_id));
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

    fn participants(&self, room_id: RoomId) -> Vec<control::ParticipantInfo> {
        self.rooms
            .get(&room_id)
            .map(|room| {
                room.members
                    .iter()
                    .filter_map(|session_id| self.participant_for_session(*session_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn participant_for_session(&self, session_id: SessionId) -> Option<control::ParticipantInfo> {
        self.sessions
            .get(&session_id)
            .map(|session| control::ParticipantInfo {
                user_id: session.user_id,
                name: session.user_name.clone(),
                in_call: session.room_id.is_some(),
                voice_status: session.voice_status,
            })
    }

    fn remove_peer_links(&mut self, session_id: SessionId) {
        self.peer_links
            .retain(|(a, b), _| *a != session_id && *b != session_id);
    }

    fn allocate_file_name(&mut self, requested: &str) -> String {
        reserve_unique_file_name(&mut self.reserved_file_names, requested)
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
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    state: ConnState,
    control: Option<ControlTransport>,
    secrets: Option<SessionSecrets>,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    disconnect: bool,
}

struct Session {
    user_id: UserId,
    user_name: String,
    tcp_token: Token,
    room_id: Option<RoomId>,
    udp_addr: Option<SocketAddr>,
    secrets: Option<SessionSecrets>,
    media_send_counter: u64,
    media_recv_replay: AntiReplay,
    active_stream: Option<StreamId>,
    voice_status: control::ParticipantVoiceStatus,
    p2p: Option<P2pSessionState>,
    receive_files: bool,
    file_receive_limit_bytes: u64,
}

struct ServerUpload {
    server_transfer_id: FileTransferId,
    room_id: RoomId,
    size: u64,
    received: u64,
    recipients: HashSet<SessionId>,
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
    members: HashSet<SessionId>,
    history: VecDeque<ChatMessage>,
    active_streams: HashMap<StreamId, SessionId>,
}

fn room_recipient_tokens(
    rooms: &HashMap<RoomId, RoomState>,
    sessions: &HashMap<SessionId, Session>,
    room_id: RoomId,
    excluded_session: Option<SessionId>,
    mut is_token_active: impl FnMut(Token) -> bool,
) -> Vec<Token> {
    rooms
        .get(&room_id)
        .map(|room| {
            room.members
                .iter()
                .copied()
                .filter(|session_id| Some(*session_id) != excluded_session)
                .filter_map(|session_id| sessions.get(&session_id).map(|session| session.tcp_token))
                .filter(|token| is_token_active(*token))
                .collect()
        })
        .unwrap_or_default()
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
        ClientControl::JoinRoom { .. } => "join_room",
        ClientControl::SendChat { .. } => "send_chat",
        ClientControl::StartVoice { .. } => "start_voice",
        ClientControl::StopVoice { .. } => "stop_voice",
        ClientControl::SetVoiceStatus { .. } => "set_voice_status",
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
        ServerControl::VoiceStatus { .. } => "voice_status",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_room(room_id: RoomId, members: &[SessionId]) -> RoomState {
        RoomState {
            id: room_id,
            name: "test".to_string(),
            members: members.iter().copied().collect(),
            history: VecDeque::new(),
            active_streams: HashMap::new(),
        }
    }

    fn test_session(user_id: UserId, token: Token, room_id: Option<RoomId>) -> Session {
        Session {
            user_id,
            user_name: format!("user-{}", user_id.0),
            tcp_token: token,
            room_id,
            udp_addr: None,
            secrets: None,
            media_send_counter: 0,
            media_recv_replay: AntiReplay::new(),
            active_stream: None,
            voice_status: control::ParticipantVoiceStatus::default(),
            p2p: None,
            receive_files: false,
            file_receive_limit_bytes: 0,
        }
    }

    fn test_server() -> Server {
        let mut config = ServerConfig::default();
        config.network.tcp_addr = "127.0.0.1:0".parse().expect("valid tcp addr");
        config.network.udp_addr = "127.0.0.1:0".parse().expect("valid udp addr");
        config.network.udp_probe_addr = None;
        config.network.p2p_enabled = false;
        Server::bind(config).expect("test server")
    }

    #[test]
    fn voice_status_updates_participant_snapshot() {
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

        let participant = server.participant_for_session(session_id).unwrap();
        assert_eq!(
            participant.voice_status,
            control::ParticipantVoiceStatus {
                muted: true,
                deafened: true,
            }
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
    fn room_recipient_tokens_excludes_leaving_session() {
        let room_id = RoomId(1);
        let leaving_session = SessionId(1);
        let remaining_session = SessionId(2);
        let mut rooms = HashMap::new();
        rooms.insert(
            room_id,
            test_room(room_id, &[leaving_session, remaining_session]),
        );
        let mut sessions = HashMap::new();
        sessions.insert(
            leaving_session,
            test_session(UserId(1), Token(3), Some(room_id)),
        );
        sessions.insert(
            remaining_session,
            test_session(UserId(2), Token(4), Some(room_id)),
        );

        let tokens =
            room_recipient_tokens(&rooms, &sessions, room_id, Some(leaving_session), |_| true);

        assert_eq!(tokens, vec![Token(4)]);
    }

    #[test]
    fn room_recipient_tokens_includes_all_sessions_without_exclusion() {
        let room_id = RoomId(1);
        let mut rooms = HashMap::new();
        rooms.insert(room_id, test_room(room_id, &[SessionId(1), SessionId(2)]));
        let mut sessions = HashMap::new();
        sessions.insert(
            SessionId(1),
            test_session(UserId(1), Token(3), Some(room_id)),
        );
        sessions.insert(
            SessionId(2),
            test_session(UserId(2), Token(4), Some(room_id)),
        );

        let mut tokens = room_recipient_tokens(&rooms, &sessions, room_id, None, |_| true);
        tokens.sort_by_key(|token| token.0);

        assert_eq!(tokens, vec![Token(3), Token(4)]);
    }

    #[test]
    fn room_recipient_tokens_skips_inactive_tokens() {
        let room_id = RoomId(1);
        let mut rooms = HashMap::new();
        rooms.insert(room_id, test_room(room_id, &[SessionId(1), SessionId(2)]));
        let mut sessions = HashMap::new();
        sessions.insert(
            SessionId(1),
            test_session(UserId(1), Token(3), Some(room_id)),
        );
        sessions.insert(
            SessionId(2),
            test_session(UserId(2), Token(4), Some(room_id)),
        );

        let tokens =
            room_recipient_tokens(&rooms, &sessions, room_id, None, |token| token != Token(3));

        assert_eq!(tokens, vec![Token(4)]);
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
