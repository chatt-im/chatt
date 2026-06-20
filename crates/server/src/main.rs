use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::{self, Read, Write},
    net::SocketAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mio::{
    Events, Interest, Poll, Token,
    net::{TcpListener, TcpStream, UdpSocket},
};
use rpc::{
    control::{
        self, ChatMessage, ClientControl, RoomInfo, ServerControl, decode_client_control,
        decode_client_hello, encode_server_control, encode_server_hello,
    },
    crypto::{
        AntiReplay, CHANNEL_CONTROL, SessionSecrets, TransportCipher, dev_server_key_pair,
        respond_to_client_hello,
    },
    frame,
    ids::{MessageId, RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload},
};

const TCP_ADDR: &str = "127.0.0.1:41000";
const UDP_ADDR: &str = "127.0.0.1:41001";
const LISTENER: Token = Token(0);
const UDP: Token = Token(1);
const FIRST_CLIENT: usize = 2;
const DEFAULT_ROOM: RoomId = RoomId(1);
const MAX_CHAT_HISTORY: usize = 200;
const POLL_TIMEOUT: Duration = Duration::from_millis(100);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut server = Server::bind(TCP_ADDR.parse()?, UDP_ADDR.parse()?)?;
    println!("tomchat server listening on tcp {TCP_ADDR}, udp {UDP_ADDR}");
    server.run()
}

struct UserConfig {
    id: UserId,
    name: &'static str,
    token: &'static str,
}

const USERS: &[UserConfig] = &[
    UserConfig {
        id: UserId(1),
        name: "alice",
        token: "alice-dev-token",
    },
    UserConfig {
        id: UserId(2),
        name: "bob",
        token: "bob-dev-token",
    },
    UserConfig {
        id: UserId(3),
        name: "carol",
        token: "carol-dev-token",
    },
];

struct Server {
    poll: Poll,
    listener: TcpListener,
    udp: UdpSocket,
    clients: HashMap<Token, ClientConn>,
    sessions: HashMap<SessionId, Session>,
    media_key_to_session: HashMap<u32, SessionId>,
    rooms: HashMap<RoomId, RoomState>,
    next_token: usize,
    next_session: u64,
    next_message: u64,
    next_stream: u32,
    rng: ring::rand::SystemRandom,
    server_key_pair: ring::signature::Ed25519KeyPair,
}

impl Server {
    fn bind(tcp_addr: SocketAddr, udp_addr: SocketAddr) -> io::Result<Self> {
        let poll = Poll::new()?;
        let mut listener = TcpListener::bind(tcp_addr)?;
        let mut udp = UdpSocket::bind(udp_addr)?;
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;
        poll.registry()
            .register(&mut udp, UDP, Interest::READABLE)?;

        let mut rooms = HashMap::new();
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

        Ok(Self {
            poll,
            listener,
            udp,
            clients: HashMap::new(),
            sessions: HashMap::new(),
            media_key_to_session: HashMap::new(),
            rooms,
            next_token: FIRST_CLIENT,
            next_session: 1,
            next_message: 1,
            next_stream: 1,
            rng: ring::rand::SystemRandom::new(),
            server_key_pair: dev_server_key_pair(),
        })
    }

    fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut events = Events::with_capacity(256);
        loop {
            self.poll.poll(&mut events, Some(POLL_TIMEOUT))?;
            for event in events.iter() {
                match event.token() {
                    LISTENER => self.accept_clients()?,
                    UDP => self.receive_udp(),
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
            self.flush_disconnects();
        }
    }

    fn accept_clients(&mut self) -> io::Result<()> {
        loop {
            let (mut socket, _addr) = match self.listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error),
            };
            let token = Token(self.next_token);
            self.next_token += 1;
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
                        disconnected = true;
                        break;
                    }
                    Ok(n) => client.read_buf.extend_from_slice(&buf[..n]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        eprintln!("client {} read failed: {error}", token.0);
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
                        eprintln!("client {} sent invalid frame: {error}", token.0);
                        self.disconnect(token);
                        break;
                    }
                },
                None => break,
            };

            if let Err(error) = self.process_frame(token, frame) {
                eprintln!("client {} protocol error: {error}", token.0);
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

        match state {
            ConnState::AwaitClientHello => {
                let hello = decode_client_hello(&frame_bytes)?;
                let response = respond_to_client_hello(&self.rng, &self.server_key_pair, &hello)
                    .map_err(|error| error.to_string())?;
                let encoded = encode_server_hello(&response.hello);
                let client = self
                    .clients
                    .get_mut(&token)
                    .ok_or_else(|| "unknown client token".to_string())?;
                frame::encode_frame(&encoded, &mut client.write_buf)
                    .map_err(|error| error.to_string())?;
                client.control = Some(TransportCipher::new(
                    response.secrets.control_send.clone(),
                    response.secrets.control_recv.clone(),
                ));
                client.secrets = Some(response.secrets);
                client.state = ConnState::AwaitAuth;
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
                let control = decode_client_control(&plaintext)?;
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

        match (state, control) {
            (
                ConnState::AwaitAuth,
                ClientControl::Authenticate {
                    user,
                    token: auth_token,
                },
            ) => self.authenticate_client(token, &user, &auth_token),
            (ConnState::AwaitAuth, _) => Err("authenticate before sending control messages".into()),
            (ConnState::Ready, ClientControl::Authenticate { .. }) => {
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
                let session_id = self.session_for_token(token)?;
                self.stop_voice(session_id, Some(stream_id));
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
    ) -> Result<(), String> {
        let user = USERS
            .iter()
            .find(|candidate| candidate.name == user_name && candidate.token == auth_token)
            .ok_or_else(|| "invalid user or token".to_string())?;
        let session_id = SessionId(self.next_session);
        self.next_session += 1;

        let secrets = self
            .clients
            .get(&token)
            .and_then(|client| client.secrets.clone())
            .ok_or_else(|| "missing negotiated keys".to_string())?;
        self.media_key_to_session
            .insert(secrets.media_recv.id, session_id);
        self.sessions.insert(
            session_id,
            Session {
                user_id: user.id,
                user_name: user.name.to_string(),
                tcp_token: token,
                room_id: None,
                udp_addr: None,
                secrets,
                media_send_counter: 0,
                media_recv_replay: AntiReplay::new(),
                active_stream: None,
            },
        );

        if let Some(client) = self.clients.get_mut(&token) {
            client.state = ConnState::Ready;
            client.session_id = Some(session_id);
            client.user_id = Some(user.id);
        }

        let rooms = self.room_infos();
        self.send_control_to_token(
            token,
            &ServerControl::Authenticated {
                session_id,
                user_id: user.id,
                rooms,
                current_room: Some(DEFAULT_ROOM),
            },
        )?;
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
        if !self.rooms.contains_key(&room_id) {
            if let Some(session) = self.sessions.get(&session_id) {
                let _ = self.send_control_to_token(
                    session.tcp_token,
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
                self.leave_room(session_id, previous);
            }
        }

        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.room_id = Some(room_id);
        }
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.members.insert(session_id);
        }

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
            self.sessions.get(&session_id).map(|s| s.tcp_token),
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
    }

    fn leave_room(&mut self, session_id: SessionId, room_id: RoomId) {
        self.stop_voice(session_id, None);
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.members.remove(&session_id);
        }
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

    fn send_chat(
        &mut self,
        session_id: SessionId,
        room_id: RoomId,
        body: String,
    ) -> Result<(), String> {
        let (sender, sender_name, member) = match self.sessions.get(&session_id) {
            Some(session) => (
                session.user_id,
                session.user_name.clone(),
                session.room_id == Some(room_id),
            ),
            None => return Err("unknown session".into()),
        };
        if !member {
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
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.history.push_back(message.clone());
            while room.history.len() > MAX_CHAT_HISTORY {
                room.history.pop_front();
            }
        }
        self.broadcast_control(room_id, &ServerControl::Chat { message });
        Ok(())
    }

    fn start_voice(&mut self, session_id: SessionId, room_id: RoomId) -> Result<(), String> {
        let (user_id, in_room) = match self.sessions.get(&session_id) {
            Some(session) => (session.user_id, session.room_id == Some(room_id)),
            None => return Err("unknown session".into()),
        };
        if !in_room {
            return Err("join the room before starting voice".into());
        }
        if self
            .sessions
            .get(&session_id)
            .and_then(|session| session.active_stream)
            .is_some()
        {
            return Ok(());
        }
        let stream_id = StreamId(self.next_stream);
        self.next_stream = self.next_stream.wrapping_add(1).max(1);
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.active_stream = Some(stream_id);
        }
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.active_streams.insert(stream_id, session_id);
        }
        self.broadcast_control(
            room_id,
            &ServerControl::VoiceStarted {
                room_id,
                user_id,
                stream_id,
            },
        );
        Ok(())
    }

    fn stop_voice(&mut self, session_id: SessionId, requested: Option<StreamId>) {
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
        self.broadcast_control(
            room_id,
            &ServerControl::VoiceStopped {
                room_id,
                user_id,
                stream_id,
            },
        );
    }

    fn receive_udp(&mut self) {
        let mut buf = [0u8; 2048];
        loop {
            let (len, src) = match self.udp.recv_from(&mut buf) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    eprintln!("udp receive failed: {error}");
                    break;
                }
            };
            let packet = &buf[..len];
            if let Err(error) = self.handle_udp_packet(src, packet) {
                eprintln!("udp packet from {src} rejected: {error}");
            }
        }
    }

    fn handle_udp_packet(&mut self, src: SocketAddr, packet: &[u8]) -> Result<(), String> {
        let (header, _) = media::parse_header(packet).map_err(|error| error.to_string())?;
        let session_id = *self
            .media_key_to_session
            .get(&header.key_id)
            .ok_or_else(|| "unknown UDP key id".to_string())?;
        let payload = {
            let session = self
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| "unknown UDP session".to_string())?;
            let (_, payload) = media::open_media(
                &session.secrets.media_recv,
                &mut session.media_recv_replay,
                packet,
            )
            .map_err(|error| error.to_string())?;
            session.udp_addr = Some(src);
            payload
        };

        match payload {
            MediaPayload::Bind { session_id: bound } => {
                if bound != session_id {
                    return Err("UDP bind session mismatch".into());
                }
                let token = self.sessions.get(&session_id).map(|s| s.tcp_token);
                if let Some(token) = token {
                    let _ = self.send_control_to_token(token, &ServerControl::UdpBound);
                }
                Ok(())
            }
            MediaPayload::Voice {
                stream_id,
                sequence,
                flags,
                opus,
            } => self.relay_voice(session_id, stream_id, sequence, flags, opus),
            MediaPayload::Ping { nonce } => {
                self.send_udp_payload(session_id, &MediaPayload::Pong { nonce });
                Ok(())
            }
            MediaPayload::Pong { .. } => Ok(()),
        }
    }

    fn relay_voice(
        &mut self,
        sender_session_id: SessionId,
        stream_id: StreamId,
        sequence: u32,
        flags: u8,
        opus: Vec<u8>,
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
        let payload = MediaPayload::Voice {
            stream_id,
            sequence,
            flags,
            opus,
        };
        for session_id in recipients {
            self.send_udp_payload(session_id, &payload);
        }
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
        match media::seal_media(&session.secrets.media_send, counter, payload) {
            Ok(packet) => {
                if let Err(error) = self.udp.send_to(&packet, addr) {
                    eprintln!("udp send to {addr} failed: {error}");
                }
            }
            Err(error) => eprintln!("udp seal failed for session {}: {error}", session_id.0),
        }
    }

    fn send_control_to_token(
        &mut self,
        token: Token,
        control: &ServerControl,
    ) -> Result<(), String> {
        let payload = encode_server_control(control);
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
        frame::encode_frame(&encrypted, &mut client.write_buf).map_err(|error| error.to_string())
    }

    fn broadcast_control(&mut self, room_id: RoomId, control: &ServerControl) {
        let tokens = self
            .rooms
            .get(&room_id)
            .map(|room| {
                room.members
                    .iter()
                    .filter_map(|session_id| self.sessions.get(session_id).map(|s| s.tcp_token))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for token in tokens {
            let _ = self.send_control_to_token(token, control);
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
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => {
                        eprintln!("client {} write failed: {error}", token.0);
                        disconnected = true;
                        break;
                    }
                }
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
            .filter_map(|(token, client)| client.disconnect.then_some(*token))
            .collect::<Vec<_>>();
        for token in tokens {
            self.disconnect(token);
        }
    }

    fn disconnect(&mut self, token: Token) {
        let Some(client) = self.clients.remove(&token) else {
            return;
        };
        if let Some(session_id) = client.session_id {
            let room_id = self.sessions.get(&session_id).and_then(|s| s.room_id);
            if let Some(room_id) = room_id {
                self.leave_room(session_id, room_id);
            }
            if let Some(session) = self.sessions.remove(&session_id) {
                self.media_key_to_session
                    .remove(&session.secrets.media_recv.id);
            }
        }
    }

    fn session_for_token(&self, token: Token) -> Result<SessionId, String> {
        self.clients
            .get(&token)
            .and_then(|client| client.session_id)
            .ok_or_else(|| "client is not authenticated".to_string())
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
                in_call: session.active_stream.is_some(),
            })
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
    control: Option<TransportCipher>,
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
    secrets: SessionSecrets,
    media_send_counter: u64,
    media_recv_replay: AntiReplay,
    active_stream: Option<StreamId>,
}

struct RoomState {
    id: RoomId,
    name: String,
    members: HashSet<SessionId>,
    history: VecDeque<ChatMessage>,
    active_streams: HashMap<StreamId, SessionId>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
