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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

use audio::{
    BufferRequest, DeviceInfo, LiveCapture, LiveCaptureConfig, LivePlayback, StatsSnapshot,
};
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
const BITRATES: [i32; 4] = [16_000, 24_000, 32_000, 48_000];
const KNOWN_USERS: &[&str] = &["alice", "bob", "carol"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Chat,
    Settings,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsFocus {
    Device,
    Bitrate,
    Denoise,
    Buffer,
    Refresh,
    Close,
}

impl SettingsFocus {
    const ORDER: [SettingsFocus; 6] = [
        SettingsFocus::Device,
        SettingsFocus::Bitrate,
        SettingsFocus::Denoise,
        SettingsFocus::Buffer,
        SettingsFocus::Refresh,
        SettingsFocus::Close,
    ];

    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0)
    }
}

struct App {
    config: ClientConfig,
    user: String,
    room_name: String,
    status: String,
    view: View,
    input: String,
    messages: VecDeque<ChatLine>,
    event_rx: Receiver<NetworkEvent>,
    network: NetworkClient,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    devices: Vec<DeviceInfo>,
    selected_input_device: Option<usize>,
    settings_focus: SettingsFocus,
    bitrate_index: usize,
    buffer_index: usize,
    denoise: bool,
    mic_muted: Arc<AtomicBool>,
    voice_tx_enabled: Arc<AtomicBool>,
    mic_error: Option<String>,
    capture: Option<LiveCapture>,
    playback: Option<LivePlayback>,
    call_enabled: bool,
    voice_packets_received: u64,
    voice_bytes_received: u64,
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
            config: config.clone(),
            user: config.user,
            room_name: "lobby".to_string(),
            status: "connecting".to_string(),
            view: View::Chat,
            input: String::new(),
            messages: VecDeque::new(),
            event_rx,
            network,
            session_id: None,
            user_id: None,
            devices: Vec::new(),
            selected_input_device: None,
            settings_focus: SettingsFocus::Device,
            bitrate_index: 1,
            buffer_index: 0,
            denoise: true,
            mic_muted: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            playback: None,
            call_enabled: false,
            voice_packets_received: 0,
            voice_bytes_received: 0,
        }
    }

    fn drain_network_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_network_event(event);
        }
    }

    fn handle_network_event(&mut self, event: NetworkEvent) {
        kvlog::info!("app network event", kind = network_event_kind(&event));
        match event {
            NetworkEvent::Connected => {
                kvlog::info!("app connected", user = self.user.as_str());
                self.status = "connected; authenticating".to_string();
            }
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
                kvlog::info!(
                    "app authenticated",
                    session_id = session_id.0,
                    user_id = user_id.0,
                    user = self.user.as_str(),
                    room_count = rooms.len()
                );
                self.status = format!("authenticated as {}", self.user);
            }
            NetworkEvent::RoomJoined { room_id, history } => {
                kvlog::info!(
                    "app room joined",
                    room_id = room_id.0,
                    history_len = history.len()
                );
                self.messages.clear();
                for message in history {
                    self.push_chat(message);
                }
                self.status = format!("joined room {}", room_id.0);
            }
            NetworkEvent::Chat(message) => {
                kvlog::info!(
                    "app chat event",
                    room_id = message.room_id.0,
                    message_id = message.message_id.0,
                    user_id = message.sender.0,
                    body_size = message.body.len()
                );
                self.push_chat(message);
            }
            NetworkEvent::VoiceStarted { user_id, .. } => {
                if Some(user_id) == self.user_id {
                    self.status = "call connected".to_string();
                    self.call_enabled = true;
                } else {
                    self.status = format!("user {} joined the call", user_id.0);
                }
            }
            NetworkEvent::VoiceStopped { user_id, stream_id } => {
                if Some(user_id) == self.user_id {
                    self.status = "call stopped".to_string();
                    self.call_enabled = false;
                    self.voice_tx_enabled.store(false, Ordering::Relaxed);
                    self.playback.take();
                } else {
                    if let Some(playback) = &self.playback {
                        playback.stop_stream(stream_id.0);
                    }
                    self.status = format!("user {} left the call", user_id.0);
                }
            }
            NetworkEvent::VoicePacket(packet) => {
                self.voice_packets_received = self.voice_packets_received.saturating_add(1);
                self.voice_bytes_received = self
                    .voice_bytes_received
                    .saturating_add(packet.payload.len() as u64);
                if let Some(playback) = &self.playback {
                    playback.push(packet);
                }
            }
            NetworkEvent::Status(status) => self.status = status,
            NetworkEvent::Error(error) => {
                kvlog::warn!("app network error", error = error.as_str());
                self.status = format!("error: {error}");
            }
            NetworkEvent::Disconnected => {
                kvlog::info!("app disconnected", user = self.user.as_str());
                self.voice_tx_enabled.store(false, Ordering::Relaxed);
                self.call_enabled = false;
                self.playback.take();
                self.status = "disconnected".to_string();
            }
        }
    }

    fn push_chat(&mut self, message: ChatMessage) {
        let local = Some(message.sender) == self.user_id;
        kvlog::info!(
            "app chat appended",
            message_id = message.message_id.0,
            room_id = message.room_id.0,
            user_id = message.sender.0,
            local,
            body_size = message.body.len()
        );
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

        if key.code == KeyCode::F2 {
            self.toggle_settings();
            return Action::Continue;
        }

        match self.view {
            View::Chat => self.process_chat_key(key),
            View::Settings => self.process_settings_key(key),
        }
    }

    fn process_chat_key(&mut self, key: KeyEvent) -> Action {
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

    fn process_settings_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => self.close_settings(),
            KeyCode::Up | KeyCode::BackTab => self.move_settings_focus(-1),
            KeyCode::Down | KeyCode::Tab => self.move_settings_focus(1),
            KeyCode::Left => self.adjust_settings_focus(-1),
            KeyCode::Right => self.adjust_settings_focus(1),
            KeyCode::Enter => self.activate_settings_focus(),
            _ => {}
        }

        Action::Continue
    }

    fn toggle_settings(&mut self) {
        match self.view {
            View::Chat => self.open_settings(),
            View::Settings => self.close_settings(),
        }
    }

    fn open_settings(&mut self) {
        self.view = View::Settings;
        if self.devices.is_empty() {
            self.refresh_input_devices();
        }
    }

    fn close_settings(&mut self) {
        self.view = View::Chat;
    }

    fn refresh_input_devices(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            self.status = "stop call before refreshing devices".to_string();
            return;
        }
        self.stop_mic_capture();

        match audio::input_devices(self.buffer_request()) {
            Ok(devices) => {
                let device_count = devices.len();
                self.devices = devices;
                if self.devices.is_empty() {
                    self.selected_input_device = None;
                    self.status = "no input devices found".to_string();
                } else {
                    if self
                        .selected_input_device
                        .is_some_and(|index| index >= self.devices.len())
                    {
                        self.selected_input_device = None;
                    }
                    self.status = format!("found {device_count} input device(s)");
                    self.start_mic_monitor();
                }
            }
            Err(error) => {
                self.devices.clear();
                self.selected_input_device = None;
                self.mic_error = Some(error.clone());
                self.status = error;
            }
        }
    }

    fn move_settings_focus(&mut self, delta: isize) {
        let len = SettingsFocus::ORDER.len() as isize;
        let next = (self.settings_focus.index() as isize + delta).rem_euclid(len) as usize;
        self.settings_focus = SettingsFocus::ORDER[next];
    }

    fn adjust_settings_focus(&mut self, delta: isize) {
        match self.settings_focus {
            SettingsFocus::Device => self.adjust_input_device(delta),
            SettingsFocus::Bitrate => {
                self.bitrate_index = cycle_index(self.bitrate_index, BITRATES.len(), delta);
                self.note_capture_settings_changed();
            }
            SettingsFocus::Denoise => {
                self.denoise = !self.denoise;
                self.note_capture_settings_changed();
            }
            SettingsFocus::Buffer => {
                self.buffer_index =
                    cycle_index(self.buffer_index, BufferRequest::OPTIONS.len(), delta);
                if !self.voice_tx_enabled.load(Ordering::Relaxed) && self.playback.is_none() {
                    self.refresh_input_devices();
                } else {
                    self.note_settings_changed();
                }
            }
            SettingsFocus::Refresh | SettingsFocus::Close => {}
        }
    }

    fn adjust_input_device(&mut self, delta: isize) {
        let len = self.devices.len() + 1;
        let current = self
            .selected_input_device
            .map(|index| index.saturating_add(1))
            .unwrap_or(0);
        let next = cycle_index(current, len, delta);
        self.selected_input_device = (next > 0).then_some(next - 1);
        self.note_capture_settings_changed();
    }

    fn activate_settings_focus(&mut self) {
        match self.settings_focus {
            SettingsFocus::Denoise => {
                self.denoise = !self.denoise;
                self.note_capture_settings_changed();
            }
            SettingsFocus::Refresh => self.refresh_input_devices(),
            SettingsFocus::Close => self.close_settings(),
            SettingsFocus::Device | SettingsFocus::Bitrate | SettingsFocus::Buffer => {
                self.adjust_settings_focus(1);
            }
        }
    }

    fn note_settings_changed(&mut self) {
        self.status = if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            "settings apply to the next call".to_string()
        } else {
            "settings updated".to_string()
        };
    }

    fn note_capture_settings_changed(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            self.note_settings_changed();
            return;
        }
        self.restart_mic_monitor();
    }

    fn bitrate_bps(&self) -> i32 {
        BITRATES[self.bitrate_index]
    }

    fn buffer_request(&self) -> BufferRequest {
        BufferRequest::OPTIONS[self.buffer_index]
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
            "/mute" => self.set_mute(true),
            "/unmute" => self.set_mute(false),
            "/muted" => self.show_mute_status(),
            "/clear" => self.messages.clear(),
            "/config" | "/settings" => self.open_settings(),
            "/users" => self.show_users(),
            "/whoami" => self.show_current_user(),
            command if command.starts_with("/user ") => self.change_user_command(command),
            command if command.starts_with('/') => {
                self.status = format!("unknown command: {command}");
            }
            body => {
                kvlog::info!(
                    "app chat submit",
                    user = self.user.as_str(),
                    body_size = body.len()
                );
                self.network
                    .send(NetworkCommand::SendChat(body.to_string()));
            }
        }
    }

    fn set_mute(&mut self, muted: bool) {
        self.mic_muted.store(muted, Ordering::Relaxed);
        kvlog::info!("app mute changed", muted);
        self.status = if muted {
            "microphone muted".to_string()
        } else {
            "microphone unmuted".to_string()
        };
    }

    fn show_mute_status(&mut self) {
        self.status = if self.mic_muted.load(Ordering::Relaxed) {
            "microphone muted".to_string()
        } else {
            "microphone unmuted".to_string()
        };
    }

    fn show_users(&mut self) {
        self.status = format!("users: {}", KNOWN_USERS.join(", "));
    }

    fn show_current_user(&mut self) {
        self.status = match self.user_id {
            Some(user_id) => format!("signed in as {} (user {})", self.user, user_id.0),
            None => format!("connecting as {}", self.user),
        };
    }

    fn change_user_command(&mut self, command: &str) {
        let mut parts = command.split_whitespace();
        let _ = parts.next();
        let Some(user) = parts.next() else {
            self.status = "usage: /user alice|bob|carol [token]".to_string();
            return;
        };
        if !is_known_user(user) {
            self.status = "known users: alice, bob, carol".to_string();
            return;
        }
        let token = parts
            .next()
            .map(ToString::to_string)
            .unwrap_or_else(|| default_token(user).to_string());
        if parts.next().is_some() {
            self.status = "usage: /user alice|bob|carol [token]".to_string();
            return;
        }
        self.reconnect_as(user.to_string(), token);
    }

    fn reconnect_as(&mut self, user: String, token: String) {
        kvlog::info!(
            "app reconnect requested",
            previous_user = self.user.as_str(),
            user = user.as_str()
        );
        self.stop_audio();
        self.call_enabled = false;
        self.network.shutdown();

        self.config.user = user;
        self.config.token = token;
        self.user = self.config.user.clone();
        self.room_name = "lobby".to_string();
        self.status = format!("connecting as {}", self.user);
        self.input.clear();
        self.messages.clear();
        self.session_id = None;
        self.user_id = None;
        self.voice_packets_received = 0;
        self.voice_bytes_received = 0;

        let (event_tx, event_rx) = mpsc::channel();
        self.network = NetworkClient::spawn(self.config.clone(), event_tx);
        self.event_rx = event_rx;
        self.start_mic_monitor();
    }

    fn start_mic_monitor(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            return;
        }
        match self.ensure_mic_capture() {
            Ok(()) => self.status = "mic monitor active".to_string(),
            Err(error) => self.status = format!("mic monitor failed: {error}"),
        }
    }

    fn restart_mic_monitor(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            self.note_settings_changed();
            return;
        }
        self.stop_mic_capture();
        self.start_mic_monitor();
    }

    fn ensure_mic_capture(&mut self) -> Result<(), String> {
        if self.capture.is_some() {
            return Ok(());
        }
        if let Some(index) = self.selected_input_device {
            let Some(device) = self.devices.get(index) else {
                let error = "selected input device is unavailable".to_string();
                self.mic_error = Some(error.clone());
                return Err(error);
            };
            if !device.supported {
                let error = device
                    .issue
                    .clone()
                    .unwrap_or_else(|| "selected input device is unsupported".to_string());
                self.mic_error = Some(error.clone());
                return Err(error);
            }
        }

        let tx = self.network.sender();
        let mic_muted = Arc::clone(&self.mic_muted);
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        match audio::start_live_capture(
            LiveCaptureConfig {
                input_device_index: self.selected_input_device,
                bitrate_bps: self.bitrate_bps(),
                denoise: self.denoise,
                buffer_request: self.buffer_request(),
            },
            move |payload| {
                if mic_muted.load(Ordering::Relaxed) || !voice_tx_enabled.load(Ordering::Relaxed) {
                    return;
                }
                let _ = tx.send(NetworkCommand::LocalVoicePacket(payload));
            },
        ) {
            Ok(capture) => {
                self.capture = Some(capture);
                self.mic_error = None;
                Ok(())
            }
            Err(error) => {
                self.mic_error = Some(error.clone());
                kvlog::warn!("live capture start failed", error = error.as_str());
                Err(error)
            }
        }
    }

    fn start_call(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            self.status = "call already active".to_string();
            return;
        }
        if let Err(error) = self.ensure_mic_capture() {
            self.status = format!("failed to start capture: {error}");
            return;
        }

        let playback = audio::start_live_playback(self.buffer_request());

        match playback {
            Ok(playback) => {
                self.playback = Some(playback);
                self.voice_tx_enabled.store(true, Ordering::Relaxed);
                self.network.send(NetworkCommand::StartVoice);
                self.voice_packets_received = 0;
                self.voice_bytes_received = 0;
                self.status = "starting call".to_string();
            }
            Err(error) => {
                kvlog::warn!("live playback start failed", error = error.as_str());
                self.playback = None;
                self.voice_tx_enabled.store(true, Ordering::Relaxed);
                self.network.send(NetworkCommand::StartVoice);
                self.voice_packets_received = 0;
                self.voice_bytes_received = 0;
                self.status = format!("starting call; playback unavailable: {error}");
            }
        }
    }

    fn stop_call(&mut self) {
        self.network.send(NetworkCommand::StopVoice);
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.playback.take();
        self.call_enabled = false;
        self.status = "stopping call; mic monitor active".to_string();
    }

    fn stop_audio(&mut self) {
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.stop_mic_capture();
        self.playback.take();
    }

    fn stop_mic_capture(&mut self) {
        self.capture.take();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_audio();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args();
    let mut logfile = None;
    while let Some(arg) = args.next() {
        if arg == "--logfile" {
            logfile = args.next();
        }
    }

    let logfile = logfile.or_else(|| std::env::var("TOMCHAT_LOGFILE").ok());
    let _logger = if let Some(logfile) = logfile {
        kvlog::collector::init_file_logger(&logfile)
    } else {
        kvlog::collector::init_closure_logger(|buf| buf.clear())
    };

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
    app.start_mic_monitor();

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

fn is_known_user(user: &str) -> bool {
    KNOWN_USERS.contains(&user)
}

fn cycle_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).rem_euclid(len as isize) as usize
}

fn render(app: &App, buf: &mut Buffer) {
    if app.view == View::Settings {
        render_settings(app, buf);
        return;
    }

    let mut screen = buf.rect().inset(1, 0);
    draw_header(screen.take_top(3), app, buf);

    let footer_height = 3i32;
    let status_height = 4i32;
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
    let call = call_state_label(app);
    let queued = app
        .playback
        .as_ref()
        .map(|playback| playback.queued_samples())
        .unwrap_or(0);
    let capture = app
        .capture
        .as_ref()
        .map(|capture| capture.stats().snapshot());
    draw_row(area.take_top(1), buf, "Status", &app.status);
    draw_row(
        area.take_top(1),
        buf,
        "Mic",
        &mic_status_line(app, capture.as_ref()),
    );
    draw_row(
        area.take_top(1),
        buf,
        "Voice",
        &format!(
            "{call}, playback {}, rx {} pkts/{} B, queued {queued}",
            if app.playback.is_some() { "on" } else { "off" },
            app.voice_packets_received,
            app.voice_bytes_received
        ),
    );
}

fn mic_status_line(app: &App, capture: Option<&StatsSnapshot>) -> String {
    let Some(capture) = capture else {
        let state = if app.mic_muted.load(Ordering::Relaxed) {
            "muted"
        } else {
            "off"
        };
        return match &app.mic_error {
            Some(error) => format!("{state}, inactive {}, error: {error}", vu_meter(0.0)),
            None => format!("{state}, inactive {}", vu_meter(0.0)),
        };
    };
    let mute = if app.mic_muted.load(Ordering::Relaxed) {
        "muted"
    } else {
        "open"
    };
    format!(
        "{mute}, active {} {}, cb {}, vad {}%, tx {} pkts, drop {}",
        vu_meter(capture.rms),
        dbfs_label(capture.rms),
        capture.callbacks,
        (capture.vad_probability.clamp(0.0, 1.0) * 100.0).round() as u32,
        capture.encoded_packets,
        capture.dropped_chunks
    )
}

fn vu_meter(rms: f32) -> String {
    const WIDTH: usize = 18;
    let db = dbfs(rms);
    let normalized = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let filled = (normalized * WIDTH as f32).round() as usize;
    let mut meter = String::with_capacity(WIDTH + 2);
    meter.push('[');
    for index in 0..WIDTH {
        meter.push(if index < filled { '#' } else { '-' });
    }
    meter.push(']');
    meter
}

fn dbfs_label(rms: f32) -> String {
    let db = dbfs(rms);
    if db <= -59.9 {
        "-inf dBFS".to_string()
    } else {
        format!("{db:.0} dBFS")
    }
}

fn dbfs(rms: f32) -> f32 {
    if rms <= f32::EPSILON {
        -60.0
    } else {
        (20.0 * rms.clamp(f32::EPSILON, 1.0).log10()).max(-60.0)
    }
}

fn draw_input(area: &mut Rect, app: &App, buf: &mut Buffer) {
    draw_row(area.take_top(1), buf, "Message", &format!("{}_", app.input));
    area.take_top(1)
        .with(AnsiColor::Grey[16].as_fg())
        .with(Ellipsis(true))
        .text(
            buf,
            "enter send | /call | /hangup | /mute | /unmute | /user bob | f2 settings | ctrl-c quit",
        );
}

fn render_settings(app: &App, buf: &mut Buffer) {
    let mut screen = buf.rect().inset(1, 0);
    draw_settings_header(screen.take_top(3), app, buf);

    let footer_height = 1i32;
    let status_height = 4i32;
    let settings_height = screen
        .h
        .saturating_sub((footer_height + status_height) as u16) as i32;
    let mut settings = BoxStyle::LIGHT
        .render(screen.take_top(settings_height), buf)
        .inset(1, 0);
    draw_settings_rows(app, &mut settings, buf);

    let mut status = BoxStyle::LIGHT
        .render(screen.take_top(status_height), buf)
        .inset(1, 0);
    draw_settings_status(&mut status, app, buf);

    draw_settings_footer(screen, buf);
}

fn draw_settings_header(mut area: Rect, app: &App, buf: &mut Buffer) {
    area.take_top(1)
        .with(AnsiColor::White.with_fg(AnsiColor::Black) | Modifier::BOLD)
        .fill(buf)
        .text(buf, " tomchat settings ");
    area.take_top(1)
        .with(AnsiColor::Grey[20].as_fg())
        .with(Ellipsis(true))
        .text(buf, &format!("{} @ {}", app.user, app.room_name));
}

fn draw_settings_rows(app: &App, area: &mut Rect, buf: &mut Buffer) {
    draw_selectable_row(
        area.take_top(1),
        buf,
        "Input device",
        &selected_input_device_label(app),
        app.settings_focus == SettingsFocus::Device,
    );
    draw_selectable_row(
        area.take_top(1),
        buf,
        "Bitrate",
        &format!("{} kbps", app.bitrate_bps() / 1000),
        app.settings_focus == SettingsFocus::Bitrate,
    );
    draw_selectable_row(
        area.take_top(1),
        buf,
        "Denoise",
        if app.denoise { "on" } else { "off" },
        app.settings_focus == SettingsFocus::Denoise,
    );
    draw_selectable_row(
        area.take_top(1),
        buf,
        "CPAL buffer",
        app.buffer_request().label(),
        app.settings_focus == SettingsFocus::Buffer,
    );

    area.take_top(1).display().text(buf, "");

    draw_button_row(
        area.take_top(1),
        buf,
        "Refresh devices",
        app.settings_focus == SettingsFocus::Refresh,
    );
    draw_button_row(
        area.take_top(1),
        buf,
        "Back to chat",
        app.settings_focus == SettingsFocus::Close,
    );
}

fn draw_settings_status(area: &mut Rect, app: &App, buf: &mut Buffer) {
    let call = call_state_label(app);
    draw_row(area.take_top(1), buf, "Status", &app.status);
    draw_row(
        area.take_top(1),
        buf,
        "Voice",
        &format!(
            "{call}, mic {}, {} kbps, denoise {}, {}",
            if app.mic_muted.load(Ordering::Relaxed) {
                "muted"
            } else {
                "open"
            },
            app.bitrate_bps() / 1000,
            if app.denoise { "on" } else { "off" },
            app.buffer_request().label()
        ),
    );
    draw_row(
        area.take_top(1),
        buf,
        "Stream",
        &stream_label(
            app.selected_input_device
                .and_then(|index| app.devices.get(index)),
        ),
    );
}

fn call_state_label(app: &App) -> &'static str {
    if app.call_enabled {
        "call on"
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        "call starting"
    } else {
        "call off"
    }
}

fn draw_settings_footer(area: Rect, buf: &mut Buffer) {
    area.with(AnsiColor::Grey[16].as_fg())
        .with(Ellipsis(true))
        .text(
            buf,
            "f2/esc close | up/down focus | left/right adjust | enter activate | ctrl-c quit",
        );
}

fn draw_row(area: Rect, buf: &mut Buffer, label: &str, value: &str) {
    let mut row = area;
    row.take_left(12)
        .with(AnsiColor::Grey[20].as_fg())
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(Ellipsis(true)).text(buf, value);
}

fn draw_selectable_row(area: Rect, buf: &mut Buffer, label: &str, value: &str, focused: bool) {
    let row_style = if focused {
        AnsiColor::Grey[8].with_fg(AnsiColor::White)
    } else {
        Style::DEFAULT
    };
    area.with(row_style).fill(buf);

    let mut row = area;
    row.take_left(16)
        .with(row_style.patch(AnsiColor::Grey[20].as_fg()))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(row_style).with(Ellipsis(true)).text(buf, value);
}

fn draw_button_row(area: Rect, buf: &mut Buffer, label: &str, focused: bool) {
    let style = if focused {
        AnsiColor::Grey[8].with_fg(AnsiColor::White)
    } else {
        Style::DEFAULT
    };
    area.with(style).fill(buf);
    area.with(style)
        .text(buf, "[ ]")
        .skip(1)
        .with(Ellipsis(true))
        .text(buf, label);
}

fn selected_input_device_label(app: &App) -> String {
    let Some(index) = app.selected_input_device else {
        return "system default".to_string();
    };
    match app.devices.get(index) {
        None => "selected device is unavailable".to_string(),
        Some(device) if device.supported => {
            let preview = device
                .preview
                .as_ref()
                .expect("supported device has preview");
            format!(
                "{} ({} ch, {}, {})",
                device.name, preview.channels, preview.sample_format, preview.buffer_note
            )
        }
        Some(device) => format!(
            "{} ({})",
            device.name,
            device.issue.as_deref().unwrap_or("unsupported")
        ),
    }
}

fn stream_label(device: Option<&DeviceInfo>) -> String {
    let Some(device) = device else {
        return "system default input".to_string();
    };
    let Some(preview) = &device.preview else {
        return device
            .issue
            .clone()
            .unwrap_or_else(|| "unsupported".to_string());
    };

    format!(
        "{} Hz, {} ch, {}, {}",
        audio::SAMPLE_RATE,
        preview.channels,
        preview.sample_format,
        buffer_size_label(preview.buffer_size)
    )
}

fn buffer_size_label(buffer_size: cpal::BufferSize) -> String {
    match buffer_size {
        cpal::BufferSize::Default => "default buffer".to_string(),
        cpal::BufferSize::Fixed(frames) => format!("{frames} frame buffer"),
    }
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
            config: test_config("alice"),
            user: "alice".to_string(),
            room_name: "lobby".to_string(),
            status: "test".to_string(),
            view: View::Chat,
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
            devices: Vec::new(),
            selected_input_device: None,
            settings_focus: SettingsFocus::Device,
            bitrate_index: 1,
            buffer_index: 0,
            denoise: true,
            mic_muted: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            playback: None,
            call_enabled: false,
            voice_packets_received: 0,
            voice_bytes_received: 0,
        };
        let mut buffer = Buffer::new(72, 20);
        render(&app, &mut buffer);
    }

    #[test]
    fn renders_settings_smoke_frame() {
        let (_event_tx, event_rx) = mpsc::channel();
        let (command_tx, _command_rx) = mpsc::channel();
        let app = App {
            config: test_config("alice"),
            user: "alice".to_string(),
            room_name: "lobby".to_string(),
            status: "test".to_string(),
            view: View::Settings,
            input: String::new(),
            messages: VecDeque::new(),
            event_rx,
            network: NetworkClient::from_parts_for_test(command_tx),
            session_id: Some(SessionId(1)),
            user_id: Some(UserId(1)),
            devices: Vec::new(),
            selected_input_device: None,
            settings_focus: SettingsFocus::Device,
            bitrate_index: 1,
            buffer_index: 0,
            denoise: true,
            mic_muted: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            playback: None,
            call_enabled: false,
            voice_packets_received: 0,
            voice_bytes_received: 0,
        };
        let mut buffer = Buffer::new(72, 20);
        render(&app, &mut buffer);
    }

    #[test]
    fn cycles_indices_in_both_directions() {
        assert_eq!(cycle_index(0, 3, -1), 2);
        assert_eq!(cycle_index(2, 3, 1), 0);
        assert_eq!(cycle_index(1, 3, 1), 2);
    }

    fn test_config(user: &str) -> ClientConfig {
        ClientConfig {
            tcp_addr: "127.0.0.1:41000".parse().unwrap(),
            udp_addr: "127.0.0.1:41001".parse().unwrap(),
            user: user.to_string(),
            token: default_token(user).to_string(),
            room_id: RoomId(1),
        }
    }
}

fn network_event_kind(event: &NetworkEvent) -> &'static str {
    match event {
        NetworkEvent::Connected => "connected",
        NetworkEvent::Authenticated { .. } => "authenticated",
        NetworkEvent::RoomJoined { .. } => "room_joined",
        NetworkEvent::Chat(_) => "chat",
        NetworkEvent::VoiceStarted { .. } => "voice_started",
        NetworkEvent::VoiceStopped { .. } => "voice_stopped",
        NetworkEvent::VoicePacket(_) => "voice_packet",
        NetworkEvent::Status(_) => "status",
        NetworkEvent::Error(_) => "error",
        NetworkEvent::Disconnected => "disconnected",
    }
}
