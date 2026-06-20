#[allow(dead_code)]
mod audio;
#[allow(dead_code)]
mod client_net;
#[cfg_attr(not(test), allow(dead_code))]
mod network;
#[allow(dead_code)]
mod packet_log;

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

use audio::{BufferRequest, LiveCapture, LiveCaptureConfig, LivePlayback};
use client_net::{ClientConfig, NetworkClient, NetworkCommand, NetworkEvent};
use extui::{
    AnsiColor, BoxStyle, Buffer, Ellipsis, Rect, Style, Terminal, TerminalFlags,
    event::{self, Event, Events, KeyCode, KeyEvent, KeyModifiers, polling::GlobalWakerConfig},
    vt::Modifier,
};
use rpc::{
    control::ChatMessage,
    ids::{RoomId, SessionId, UserId},
};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_MESSAGES: usize = 500;

struct App {
    user: String,
    room_name: String,
    status: String,
    input: String,
    messages: VecDeque<ChatLine>,
    event_rx: Receiver<NetworkEvent>,
    network: NetworkClient,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    capture: Option<LiveCapture>,
    playback: Option<LivePlayback>,
    call_enabled: bool,
}

#[derive(Clone, Debug)]
struct ChatLine {
    sender: String,
    body: String,
    local: bool,
}

enum Action {
    Continue,
    Quit,
}

impl App {
    fn new(config: ClientConfig) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let network = NetworkClient::spawn(config.clone(), event_tx);
        Self {
            user: config.user,
            room_name: "lobby".to_string(),
            status: "connecting".to_string(),
            input: String::new(),
            messages: VecDeque::new(),
            event_rx,
            network,
            session_id: None,
            user_id: None,
            capture: None,
            playback: None,
            call_enabled: false,
        }
    }

    fn drain_network_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_network_event(event);
        }
    }

    fn handle_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::Connected => self.status = "connected; authenticating".to_string(),
            NetworkEvent::Authenticated {
                session_id,
                user_id,
                rooms,
            } => {
                self.session_id = Some(session_id);
                self.user_id = Some(user_id);
                if let Some(room) = rooms.first() {
                    self.room_name = room.name.clone();
                }
                self.status = format!("authenticated as {}", self.user);
            }
            NetworkEvent::RoomJoined { room_id, history } => {
                self.messages.clear();
                for message in history {
                    self.push_chat(message);
                }
                self.status = format!("joined room {}", room_id.0);
            }
            NetworkEvent::Chat(message) => self.push_chat(message),
            NetworkEvent::VoiceStarted { user_id, .. } => {
                if Some(user_id) == self.user_id {
                    self.status = "call connected".to_string();
                    self.call_enabled = true;
                } else {
                    self.status = format!("user {} joined the call", user_id.0);
                }
            }
            NetworkEvent::VoiceStopped { user_id, .. } => {
                if Some(user_id) == self.user_id {
                    self.status = "call stopped".to_string();
                    self.call_enabled = false;
                    self.stop_audio();
                } else {
                    self.status = format!("user {} left the call", user_id.0);
                }
            }
            NetworkEvent::VoicePacket(packet) => {
                if let Some(playback) = &self.playback {
                    playback.push(packet);
                }
            }
            NetworkEvent::Status(status) => self.status = status,
            NetworkEvent::Error(error) => self.status = format!("error: {error}"),
            NetworkEvent::Disconnected => self.status = "disconnected".to_string(),
        }
    }

    fn push_chat(&mut self, message: ChatMessage) {
        let local = Some(message.sender) == self.user_id;
        self.messages.push_back(ChatLine {
            sender: message.sender_name,
            body: message.body,
            local,
        });
        while self.messages.len() > MAX_MESSAGES {
            self.messages.pop_front();
        }
    }

    fn process_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }

        match key.code {
            KeyCode::Enter => self.submit_input(),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => self.input.clear(),
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.push(ch);
            }
            _ => {}
        }

        Action::Continue
    }

    fn submit_input(&mut self) {
        let input = self.input.trim().to_string();
        self.input.clear();
        if input.is_empty() {
            return;
        }

        match input.as_str() {
            "/quit" => {}
            "/call" => self.start_call(),
            "/hangup" => self.stop_call(),
            "/clear" => self.messages.clear(),
            command if command.starts_with('/') => {
                self.status = format!("unknown command: {command}");
            }
            body => {
                self.network
                    .send(NetworkCommand::SendChat(body.to_string()));
            }
        }
    }

    fn start_call(&mut self) {
        if self.capture.is_some() || self.playback.is_some() {
            self.status = "call already active".to_string();
            return;
        }

        let tx = self.network.sender();
        let capture = audio::start_live_capture(
            LiveCaptureConfig {
                bitrate_bps: 24_000,
                denoise: true,
                buffer_request: BufferRequest::Default,
            },
            move |payload| {
                let _ = tx.send(NetworkCommand::LocalVoicePacket(payload));
            },
        );
        let playback = audio::start_live_playback(BufferRequest::Default);

        match (capture, playback) {
            (Ok(capture), Ok(playback)) => {
                self.capture = Some(capture);
                self.playback = Some(playback);
                self.network.send(NetworkCommand::StartVoice);
                self.status = "starting call".to_string();
            }
            (Err(error), playback) => {
                if let Ok(playback) = playback {
                    playback.stop();
                }
                self.status = format!("failed to start capture: {error}");
            }
            (Ok(capture), Err(error)) => {
                capture.stop();
                self.status = format!("failed to start playback: {error}");
            }
        }
    }

    fn stop_call(&mut self) {
        self.network.send(NetworkCommand::StopVoice);
        self.stop_audio();
        self.call_enabled = false;
        self.status = "stopping call".to_string();
    }

    fn stop_audio(&mut self) {
        self.capture.take();
        self.playback.take();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_audio();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    event::polling::initialize_global_waker(GlobalWakerConfig {
        resize: true,
        termination: true,
    })?;

    let flags = TerminalFlags::RAW_MODE
        | TerminalFlags::ALT_SCREEN
        | TerminalFlags::HIDE_CURSOR
        | TerminalFlags::EXTENDED_KEYBOARD_INPUTS;
    let mut terminal = Terminal::open(flags)?;
    let (w, h) = terminal.size()?;
    let mut buffer = Buffer::new(w, h);
    let mut events = Events::default();
    let stdin = std::io::stdin();
    let mut app = App::new(client_config()?);

    loop {
        app.drain_network_events();
        render(&app, &mut buffer);
        buffer.render(&mut terminal);

        if event::poll(&stdin, Some(POLL_INTERVAL))?.is_readable() {
            events.read_from(&stdin)?;
        }

        while let Some(event) = events.next(terminal.is_raw()) {
            match event {
                Event::Key(key) => {
                    if matches!(app.process_key(key), Action::Quit) {
                        return Ok(());
                    }
                }
                Event::Resized => {
                    let (new_w, new_h) = terminal.size()?;
                    buffer.resize(new_w, new_h);
                }
                _ => {}
            }
        }
    }
}

fn client_config() -> Result<ClientConfig, Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let user = value_arg(&args, "--user")
        .or_else(|| std::env::var("TOMCHAT_USER").ok())
        .unwrap_or_else(|| "alice".to_string());
    let token = value_arg(&args, "--token")
        .or_else(|| std::env::var("TOMCHAT_TOKEN").ok())
        .unwrap_or_else(|| default_token(&user).to_string());
    let tcp_addr = value_arg(&args, "--tcp")
        .or_else(|| std::env::var("TOMCHAT_TCP").ok())
        .unwrap_or_else(|| "127.0.0.1:41000".to_string())
        .parse::<SocketAddr>()?;
    let udp_addr = value_arg(&args, "--udp")
        .or_else(|| std::env::var("TOMCHAT_UDP").ok())
        .unwrap_or_else(|| "127.0.0.1:41001".to_string())
        .parse::<SocketAddr>()?;
    Ok(ClientConfig {
        tcp_addr,
        udp_addr,
        user,
        token,
        room_id: RoomId(1),
    })
}

fn value_arg(args: &[String], key: &str) -> Option<String> {
    args.windows(2)
        .find_map(|window| (window[0] == key).then(|| window[1].clone()))
}

fn default_token(user: &str) -> &'static str {
    match user {
        "bob" => "bob-dev-token",
        "carol" => "carol-dev-token",
        _ => "alice-dev-token",
    }
}

fn render(app: &App, buf: &mut Buffer) {
    let mut screen = buf.rect().inset(1, 0);
    draw_header(screen.take_top(3), app, buf);

    let footer_height = 3i32;
    let status_height = 3i32;
    let transcript_height = screen
        .h
        .saturating_sub((footer_height + status_height) as u16) as i32;
    let mut transcript = screen.take_top(transcript_height);
    draw_messages(&mut transcript, app, buf);

    let mut status = BoxStyle::LIGHT
        .render(screen.take_top(status_height), buf)
        .inset(1, 0);
    draw_status(&mut status, app, buf);

    let mut input = BoxStyle::LIGHT
        .render(screen.take_top(footer_height), buf)
        .inset(1, 0);
    draw_input(&mut input, app, buf);
}

fn draw_header(mut area: Rect, app: &App, buf: &mut Buffer) {
    area.take_top(1)
        .with(AnsiColor::White.with_fg(AnsiColor::Black) | Modifier::BOLD)
        .fill(buf)
        .text(buf, " tomchat ");
    area.take_top(1)
        .with(AnsiColor::Grey[20].as_fg())
        .text(buf, &format!("{} @ {}", app.user, app.room_name));
}

fn draw_messages(area: &mut Rect, app: &App, buf: &mut Buffer) {
    let height = area.h as usize;
    let skip = app.messages.len().saturating_sub(height);
    for line in app.messages.iter().skip(skip) {
        let style = if line.local {
            AnsiColor::Green1.with_fg(AnsiColor::Black)
        } else {
            Style::DEFAULT
        };
        let mut row = area.take_top(1);
        row.with(style).fill(buf);
        row.take_left(14)
            .with(style.patch(AnsiColor::Grey[20].as_fg()))
            .with(Ellipsis(true))
            .text(buf, &line.sender);
        row.with(style).with(Ellipsis(true)).text(buf, &line.body);
    }
}

fn draw_status(area: &mut Rect, app: &App, buf: &mut Buffer) {
    let call = if app.call_enabled || app.capture.is_some() {
        "call on"
    } else {
        "call off"
    };
    let queued = app
        .playback
        .as_ref()
        .map(|playback| playback.queued_samples())
        .unwrap_or(0);
    draw_row(area.take_top(1), buf, "Status", &app.status);
    draw_row(
        area.take_top(1),
        buf,
        "Media",
        &format!("{call}, queued {queued} samples"),
    );
}

fn draw_input(area: &mut Rect, app: &App, buf: &mut Buffer) {
    draw_row(area.take_top(1), buf, "Message", &format!("{}_", app.input));
    area.take_top(1)
        .with(AnsiColor::Grey[16].as_fg())
        .text(buf, "enter send | /call | /hangup | /clear | ctrl-c quit");
}

fn draw_row(area: Rect, buf: &mut Buffer, label: &str, value: &str) {
    let mut row = area;
    row.take_left(12)
        .with(AnsiColor::Grey[20].as_fg())
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(Ellipsis(true)).text(buf, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Sender;

    #[test]
    fn default_tokens_match_users() {
        assert_eq!(default_token("alice"), "alice-dev-token");
        assert_eq!(default_token("bob"), "bob-dev-token");
        assert_eq!(default_token("carol"), "carol-dev-token");
    }

    #[test]
    fn renders_smoke_frame() {
        let (_event_tx, event_rx) = mpsc::channel();
        let (command_tx, _command_rx) = mpsc::channel();
        struct DummyClient(Sender<NetworkCommand>);
        impl DummyClient {
            fn into_network(self) -> NetworkClient {
                NetworkClient::from_parts_for_test(self.0)
            }
        }

        let app = App {
            user: "alice".to_string(),
            room_name: "lobby".to_string(),
            status: "test".to_string(),
            input: "hello".to_string(),
            messages: VecDeque::from([ChatLine {
                sender: "alice".to_string(),
                body: "hello".to_string(),
                local: true,
            }]),
            event_rx,
            network: DummyClient(command_tx).into_network(),
            session_id: Some(SessionId(1)),
            user_id: Some(UserId(1)),
            capture: None,
            playback: None,
            call_enabled: false,
        };
        let mut buffer = Buffer::new(72, 20);
        render(&app, &mut buffer);
    }
}
