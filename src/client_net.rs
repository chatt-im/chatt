use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream as StdTcpStream, UdpSocket as StdUdpSocket},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
    time::Duration,
};

use mio::{
    Events, Interest, Poll, Token,
    net::{TcpStream, UdpSocket},
};
use rpc::{
    control::{
        ChatMessage, ClientControl, RoomInfo, ServerControl, decode_server_control,
        decode_server_hello, encode_client_control, encode_client_hello,
    },
    crypto::{
        AntiReplay, CHANNEL_CONTROL, SessionSecrets, TransportCipher, complete_client_handshake,
        dev_server_public_key, generate_client_hello,
    },
    frame,
    ids::{RoomId, SessionId, StreamId, UserId},
    media::{self, MediaPayload},
};

use crate::audio::RemoteVoicePacket;

const TCP: Token = Token(0);
const UDP: Token = Token(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(20);

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub tcp_addr: SocketAddr,
    pub udp_addr: SocketAddr,
    pub user: String,
    pub token: String,
    pub room_id: RoomId,
}

#[derive(Debug)]
pub enum NetworkCommand {
    SendChat(String),
    StartVoice,
    StopVoice,
    LocalVoicePacket(Vec<u8>),
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
    },
    Chat(ChatMessage),
    VoiceStarted {
        user_id: UserId,
        stream_id: StreamId,
    },
    VoiceStopped {
        user_id: UserId,
        stream_id: StreamId,
    },
    VoicePacket(RemoteVoicePacket),
    Status(String),
    Error(String),
    Disconnected,
}

pub struct NetworkClient {
    tx: Sender<NetworkCommand>,
    worker: Option<JoinHandle<()>>,
}

impl NetworkClient {
    pub fn spawn(config: ClientConfig, events: Sender<NetworkEvent>) -> Self {
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

fn run_worker(
    config: ClientConfig,
    events: Sender<NetworkEvent>,
    commands: Receiver<NetworkCommand>,
) {
    if let Err(error) = run_worker_inner(config, &events, commands) {
        let _ = events.send(NetworkEvent::Error(error));
    }
    let _ = events.send(NetworkEvent::Disconnected);
}

fn run_worker_inner(
    config: ClientConfig,
    events: &Sender<NetworkEvent>,
    commands: Receiver<NetworkCommand>,
) -> Result<(), String> {
    let (std_tcp, control, secrets) = connect_and_handshake(&config)?;
    let std_udp = StdUdpSocket::bind(if config.udp_addr.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .map_err(|error| format!("failed to bind UDP socket: {error}"))?;
    std_tcp
        .set_nonblocking(true)
        .map_err(|error| format!("failed to make TCP nonblocking: {error}"))?;
    std_udp
        .set_nonblocking(true)
        .map_err(|error| format!("failed to make UDP nonblocking: {error}"))?;

    let mut worker = WorkerState {
        config,
        events: events.clone(),
        tcp: TcpStream::from_std(std_tcp),
        udp: UdpSocket::from_std(std_udp),
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
        shutdown: false,
    };

    let mut poll = Poll::new().map_err(|error| format!("failed to create poll: {error}"))?;
    poll.registry()
        .register(
            &mut worker.tcp,
            TCP,
            Interest::READABLE | Interest::WRITABLE,
        )
        .map_err(|error| format!("failed to register TCP socket: {error}"))?;
    poll.registry()
        .register(&mut worker.udp, UDP, Interest::READABLE)
        .map_err(|error| format!("failed to register UDP socket: {error}"))?;

    worker.queue_control(ClientControl::Authenticate {
        user: worker.config.user.clone(),
        token: worker.config.token.clone(),
    })?;
    let _ = worker.events.send(NetworkEvent::Connected);

    let mut poll_events = Events::with_capacity(128);
    while !worker.shutdown {
        poll.poll(&mut poll_events, Some(POLL_TIMEOUT))
            .map_err(|error| format!("network poll failed: {error}"))?;
        for event in poll_events.iter() {
            match event.token() {
                TCP => {
                    if event.is_readable() {
                        worker.read_tcp()?;
                    }
                    if event.is_writable() {
                        worker.write_tcp()?;
                    }
                }
                UDP => worker.read_udp(),
                _ => {}
            }
        }

        while let Ok(command) = commands.try_recv() {
            worker.handle_command(command)?;
        }
    }
    Ok(())
}

fn connect_and_handshake(
    config: &ClientConfig,
) -> Result<(StdTcpStream, TransportCipher, SessionSecrets), String> {
    let mut stream = StdTcpStream::connect(config.tcp_addr)
        .map_err(|error| format!("failed to connect to server: {error}"))?;
    stream
        .set_nodelay(true)
        .map_err(|error| format!("failed to set TCP_NODELAY: {error}"))?;
    let rng = ring::rand::SystemRandom::new();
    let client = generate_client_hello(&rng).map_err(|error| error.to_string())?;
    let hello = encode_client_hello(&client.hello);
    let mut framed = Vec::new();
    frame::encode_frame(&hello, &mut framed).map_err(|error| error.to_string())?;
    stream
        .write_all(&framed)
        .map_err(|error| format!("failed to write client hello: {error}"))?;
    let response = read_blocking_frame(&mut stream)
        .map_err(|error| format!("failed to read server hello: {error}"))?;
    let server_hello = decode_server_hello(&response)?;
    let secrets = complete_client_handshake(client, &server_hello, &dev_server_public_key())
        .map_err(|error| error.to_string())?;
    let control = TransportCipher::new(secrets.control_send.clone(), secrets.control_recv.clone());
    Ok((stream, control, secrets))
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
    read_buf: Vec<u8>,
    write_buf: Vec<u8>,
    control: TransportCipher,
    secrets: SessionSecrets,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    room_id: Option<RoomId>,
    active_stream: Option<StreamId>,
    local_sequence: u32,
    media_send_counter: u64,
    media_recv_replay: AntiReplay,
    shutdown: bool,
}

impl WorkerState {
    fn queue_control(&mut self, control: ClientControl) -> Result<(), String> {
        let payload = encode_client_control(&control)?;
        let encrypted = self
            .control
            .seal_next(CHANNEL_CONTROL, &payload)
            .map_err(|error| error.to_string())?;
        frame::encode_frame(&encrypted, &mut self.write_buf).map_err(|error| error.to_string())
    }

    fn read_tcp(&mut self) -> Result<(), String> {
        let mut buf = [0u8; 8192];
        loop {
            match self.tcp.read(&mut buf) {
                Ok(0) => {
                    self.shutdown = true;
                    return Ok(());
                }
                Ok(n) => self.read_buf.extend_from_slice(&buf[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(format!("TCP read failed: {error}")),
            }
        }

        loop {
            let frame = match frame::pop_frame(&mut self.read_buf) {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(error) => return Err(format!("invalid server frame: {error}")),
            };
            let plaintext = self
                .control
                .open_next(CHANNEL_CONTROL, &frame)
                .map_err(|error| error.to_string())?;
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
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(format!("TCP write failed: {error}")),
            }
        }
        Ok(())
    }

    fn read_udp(&mut self) {
        let mut buf = [0u8; 2048];
        loop {
            let (len, src) = match self.udp.recv_from(&mut buf) {
                Ok(value) => value,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP receive failed: {error}")));
                    break;
                }
            };
            if src != self.config.udp_addr {
                continue;
            }
            match media::open_media(
                &self.secrets.media_recv,
                &mut self.media_recv_replay,
                &buf[..len],
            ) {
                Ok((
                    _,
                    MediaPayload::Voice {
                        stream_id,
                        sequence,
                        opus,
                        ..
                    },
                )) => {
                    let _ = self
                        .events
                        .send(NetworkEvent::VoicePacket(RemoteVoicePacket {
                            stream_id: stream_id.0,
                            sequence,
                            payload: opus,
                        }));
                }
                Ok((_, MediaPayload::Pong { .. })) => {}
                Ok((_, MediaPayload::Ping { nonce })) => {
                    self.send_media(&MediaPayload::Pong { nonce });
                }
                Ok((_, MediaPayload::Bind { .. })) => {}
                Err(error) => {
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP packet rejected: {error}")));
                }
            }
        }
    }

    fn handle_command(&mut self, command: NetworkCommand) -> Result<(), String> {
        match command {
            NetworkCommand::SendChat(body) => {
                if let Some(room_id) = self.room_id.or(Some(self.config.room_id)) {
                    self.queue_control(ClientControl::SendChat { room_id, body })?;
                }
            }
            NetworkCommand::StartVoice => {
                let room_id = self.room_id.unwrap_or(self.config.room_id);
                self.queue_control(ClientControl::StartVoice { room_id })?;
            }
            NetworkCommand::StopVoice => {
                if let Some(stream_id) = self.active_stream {
                    self.queue_control(ClientControl::StopVoice { stream_id })?;
                }
            }
            NetworkCommand::LocalVoicePacket(payload) => {
                if let Some(stream_id) = self.active_stream {
                    let payload = MediaPayload::Voice {
                        stream_id,
                        sequence: self.local_sequence,
                        flags: 0,
                        opus: payload,
                    };
                    self.local_sequence = self.local_sequence.wrapping_add(1);
                    self.send_media(&payload);
                }
            }
            NetworkCommand::Shutdown => self.shutdown = true,
        }
        Ok(())
    }

    fn handle_server_control(&mut self, control: ServerControl) {
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
                room_id, history, ..
            } => {
                self.room_id = Some(room_id);
                let _ = self
                    .events
                    .send(NetworkEvent::RoomJoined { room_id, history });
            }
            ServerControl::Chat { message } => {
                let _ = self.events.send(NetworkEvent::Chat(message));
            }
            ServerControl::Presence {
                participant,
                online,
                ..
            } => {
                let verb = if online { "joined" } else { "left" };
                let _ = self
                    .events
                    .send(NetworkEvent::Status(format!("{} {verb}", participant.name)));
            }
            ServerControl::VoiceStarted {
                user_id, stream_id, ..
            } => {
                if Some(user_id) == self.user_id {
                    self.active_stream = Some(stream_id);
                    self.local_sequence = 0;
                }
                let _ = self
                    .events
                    .send(NetworkEvent::VoiceStarted { user_id, stream_id });
            }
            ServerControl::VoiceStopped {
                user_id, stream_id, ..
            } => {
                if Some(stream_id) == self.active_stream {
                    self.active_stream = None;
                }
                let _ = self
                    .events
                    .send(NetworkEvent::VoiceStopped { user_id, stream_id });
            }
            ServerControl::UdpBound => {
                let _ = self
                    .events
                    .send(NetworkEvent::Status("udp media bound".to_string()));
            }
            ServerControl::Pong { .. } => {}
            ServerControl::Error { message, .. } => {
                let _ = self.events.send(NetworkEvent::Error(message));
            }
        }
    }

    fn bind_udp(&mut self) {
        if let Some(session_id) = self.session_id {
            self.send_media(&MediaPayload::Bind { session_id });
        }
    }

    fn send_media(&mut self, payload: &MediaPayload) {
        let counter = self.media_send_counter;
        self.media_send_counter = self.media_send_counter.wrapping_add(1);
        match media::seal_media(&self.secrets.media_send, counter, payload) {
            Ok(packet) => {
                if let Err(error) = self.udp.send_to(&packet, self.config.udp_addr) {
                    let _ = self
                        .events
                        .send(NetworkEvent::Error(format!("UDP send failed: {error}")));
                }
            }
            Err(error) => {
                let _ = self
                    .events
                    .send(NetworkEvent::Error(format!("UDP seal failed: {error}")));
            }
        }
    }
}
