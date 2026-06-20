#[allow(dead_code)]
mod audio;
mod bindings;
mod chat_buffer;
#[allow(dead_code)]
mod client_net;
mod config;
#[cfg_attr(not(test), allow(dead_code))]
mod network;
#[allow(dead_code)]
mod packet_log;
mod theme;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

use audio::{
    BufferRequest, DeviceInfo, LiveCapture, LiveCaptureConfig, LivePlayback, StatsSnapshot,
};
use bindings::{BindCommand, PendingChord, Resolved};
use chat_buffer::VirtualChatBuffer;
use client_net::{NetworkClient, NetworkCommand, NetworkEvent};
use config::{AudioConfig, BufferChoice, Config};
use extui::{
    Buffer, Ellipsis, HAlign, Rect, Style, Terminal, TerminalFlags,
    event::{self, Event, Events, KeyCode, KeyEvent, KeyModifiers, polling::GlobalWakerConfig},
    vt::Modifier,
};
use extui_bindings::InputKey;
use extui_editor::{
    Editor, Replacement, StyleRun, TextBuffer, TrackedChange, bindings as editor_bindings,
};
use rpc::{
    control::{ChatMessage, ParticipantInfo},
    ids::{SessionId, StreamId, UserId},
};
use tinyhl::{Highlighter, Source};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const BITRATES: [i32; 4] = [16_000, 24_000, 32_000, 48_000];
const KNOWN_USERS: &[&str] = &["alice", "bob", "carol"];
const NAME_COL_WIDTH: u16 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusKind {
    Info,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsFocus {
    Device,
    Bitrate,
    Denoise,
    Buffer,
    Refresh,
    Save,
    Close,
}

impl SettingsFocus {
    const ORDER: [SettingsFocus; 7] = [
        SettingsFocus::Device,
        SettingsFocus::Bitrate,
        SettingsFocus::Denoise,
        SettingsFocus::Buffer,
        SettingsFocus::Refresh,
        SettingsFocus::Save,
        SettingsFocus::Close,
    ];

    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
struct SettingsDraft {
    input_device_index: Option<u32>,
    bitrate_index: usize,
    buffer_index: usize,
    denoise: bool,
}

impl SettingsDraft {
    fn from_audio(config: &AudioConfig) -> Self {
        Self {
            input_device_index: config.input_device_index,
            bitrate_index: BITRATES
                .iter()
                .position(|bitrate| *bitrate == config.bitrate_bps)
                .unwrap_or(1),
            buffer_index: BufferRequest::OPTIONS
                .iter()
                .position(|buffer| *buffer == config.buffer.to_request())
                .unwrap_or(0),
            denoise: config.denoise,
        }
    }

    fn to_audio(&self) -> AudioConfig {
        AudioConfig {
            input_device_index: self.input_device_index,
            bitrate_bps: BITRATES[self.bitrate_index],
            denoise: self.denoise,
            buffer: BufferChoice::from_request(self.buffer_request()),
        }
    }

    fn bitrate_bps(&self) -> i32 {
        BITRATES[self.bitrate_index]
    }

    fn buffer_request(&self) -> BufferRequest {
        BufferRequest::OPTIONS[self.buffer_index]
    }
}

#[derive(Clone, Debug)]
struct ParticipantState {
    user_id: UserId,
    name: String,
    online: bool,
    in_call: bool,
    p2p_direct: bool,
    last_message_ms: Option<u64>,
    last_voice_at: Option<Instant>,
    active_stream: Option<StreamId>,
}

#[derive(Default)]
struct Participants {
    entries: Vec<ParticipantState>,
    scroll: usize,
}

impl Participants {
    fn replace_room(&mut self, participants: Vec<ParticipantInfo>) {
        self.entries.clear();
        for participant in participants {
            self.upsert(participant, true);
        }
        self.sort();
        self.scroll = 0;
    }

    fn upsert(&mut self, participant: ParticipantInfo, online: bool) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| entry.user_id == participant.user_id)
        {
            existing.name = participant.name;
            existing.online = online;
            existing.in_call = participant.in_call;
            if !participant.in_call {
                existing.p2p_direct = false;
            }
        } else {
            self.entries.push(ParticipantState {
                user_id: participant.user_id,
                name: participant.name,
                online,
                in_call: participant.in_call,
                p2p_direct: false,
                last_message_ms: None,
                last_voice_at: None,
                active_stream: None,
            });
        }
        self.sort();
    }

    fn set_presence(&mut self, participant: ParticipantInfo, online: bool) {
        self.upsert(participant, online);
    }

    fn note_message(&mut self, message: &ChatMessage) {
        let entry = self.ensure_user(message.sender, &message.sender_name);
        entry.last_message_ms = Some(message.timestamp_ms);
    }

    fn voice_started(&mut self, user_id: UserId, stream_id: StreamId) {
        let entry = self.ensure_user(user_id, &format!("user {}", user_id.0));
        entry.in_call = true;
        entry.active_stream = Some(stream_id);
        entry.last_voice_at = Some(Instant::now());
    }

    fn voice_stopped(&mut self, user_id: UserId, stream_id: StreamId) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.user_id == user_id)
        {
            entry.in_call = false;
            entry.p2p_direct = false;
            if entry.active_stream == Some(stream_id) {
                entry.active_stream = None;
            }
        }
    }

    fn set_peer_transport(&mut self, user_id: UserId, direct: bool) {
        let entry = self.ensure_user(user_id, &format!("user {}", user_id.0));
        entry.p2p_direct = direct;
        self.sort();
    }

    fn voice_packet(&mut self, stream_id: u32) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.active_stream == Some(StreamId(stream_id)))
        {
            entry.last_voice_at = Some(Instant::now());
        }
    }

    fn online_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.online).count()
    }

    fn in_call_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.in_call).count()
    }

    fn ensure_user(&mut self, user_id: UserId, name: &str) -> &mut ParticipantState {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.user_id == user_id)
        {
            if self.entries[index].name.starts_with("user ") {
                self.entries[index].name = name.to_string();
            }
            return &mut self.entries[index];
        }
        self.entries.push(ParticipantState {
            user_id,
            name: name.to_string(),
            online: true,
            in_call: false,
            p2p_direct: false,
            last_message_ms: None,
            last_voice_at: None,
            active_stream: None,
        });
        let index = self.entries.len() - 1;
        &mut self.entries[index]
    }

    fn sort(&mut self) {
        self.entries.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| b.in_call.cmp(&a.in_call))
                .then_with(|| b.p2p_direct.cmp(&a.p2p_direct))
                .then_with(|| a.name.cmp(&b.name))
        });
    }
}

struct BufferSource<'a>(&'a TextBuffer);

impl<'a> Source for BufferSource<'a> {
    fn len(&self) -> u32 {
        self.0.len() as u32
    }

    fn page(&self, offset: u32) -> (u32, &[u8]) {
        self.0.page(offset)
    }
}

struct EditorHighlighter {
    hl: Highlighter,
    runs: Vec<StyleRun>,
}

impl EditorHighlighter {
    fn new(editor: &mut Editor) -> Self {
        editor.set_track_replacements(true);
        let mut hl = Highlighter::new(tinyhl::Language::Markdown);
        hl.rebuild(&BufferSource(editor.text_buffer()));
        Self {
            hl,
            runs: Vec::new(),
        }
    }

    fn sync(&mut self, editor: &mut Editor) {
        match editor.take_tracked_change() {
            TrackedChange::None => {}
            TrackedChange::Reset => self.hl.rebuild(&BufferSource(editor.text_buffer())),
            TrackedChange::Merged(Replacement {
                offset,
                old_len,
                new_len,
            }) => self.hl.apply_replacement(
                &BufferSource(editor.text_buffer()),
                tinyhl::Span::new(offset, old_len),
                new_len,
            ),
        }
    }

    fn render(&mut self, editor: &mut Editor, area: Rect, buf: &mut Buffer) {
        self.sync(editor);
        self.runs.clear();
        if let Some(table) = self.hl.table() {
            let visible = editor.visible_byte_span(area);
            let span = tinyhl::Span::new(visible.offset, visible.len);
            for tok in table.query(span) {
                let mut style = theme::TEXT;
                if let Some(render_span) = self
                    .hl
                    .render(tinyhl::Span::new(tok.span.offset, tok.span.len))
                    .next()
                {
                    style = style.patch(theme::syntax_style(&render_span));
                }
                self.runs.push(StyleRun {
                    offset: tok.span.offset,
                    len: tok.span.len,
                    style,
                });
            }
        }
        editor.render_with_styles(area, buf, &self.runs);
    }
}

struct App {
    config: Config,
    user: String,
    room_name: String,
    status: String,
    status_kind: StatusKind,
    mode: theme::UiMode,
    composer: Editor,
    composer_hl: EditorHighlighter,
    chat: VirtualChatBuffer,
    participants: Participants,
    last_chat_width: u16,
    pending_chord: Option<PendingChord>,
    event_rx: Receiver<NetworkEvent>,
    network: NetworkClient,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    devices: Vec<DeviceInfo>,
    settings_focus: SettingsFocus,
    settings: SettingsDraft,
    settings_dirty: bool,
    mic_muted: Arc<AtomicBool>,
    voice_tx_enabled: Arc<AtomicBool>,
    mic_error: Option<String>,
    capture: Option<LiveCapture>,
    playback: Option<LivePlayback>,
    call_enabled: bool,
    voice_packets_received: u64,
    voice_bytes_received: u64,
}

enum Action {
    Continue,
    Quit,
}

impl App {
    fn new(config: Config) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let client_config = config.network.client_config();
        let network = NetworkClient::spawn(client_config, event_tx);
        let mut composer =
            Editor::with_bindings(editor_bindings::vim(editor_bindings::VimOptions::default()));
        composer.set_wrap(true);
        composer.set_height_bounds(1, config.ui.max_composer_height.max(1));
        composer.set_theme(theme::editor_theme());
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);
        let settings = SettingsDraft::from_audio(&config.audio);
        Self {
            user: config.network.user.clone(),
            room_name: "lobby".to_string(),
            status: "connecting".to_string(),
            status_kind: StatusKind::Info,
            mode: theme::UiMode::Compose,
            composer,
            composer_hl,
            chat: VirtualChatBuffer::new(config.ui.max_messages as usize),
            participants: Participants::default(),
            last_chat_width: 80,
            pending_chord: None,
            event_rx,
            network,
            session_id: None,
            user_id: None,
            devices: Vec::new(),
            settings_focus: SettingsFocus::Device,
            settings,
            settings_dirty: false,
            mic_muted: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            playback: None,
            call_enabled: false,
            voice_packets_received: 0,
            voice_bytes_received: 0,
            config,
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
            NetworkEvent::Connected => self.set_status("connected; authenticating"),
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
                self.set_status(format!("authenticated as {}", self.user));
            }
            NetworkEvent::RoomJoined {
                room_id,
                history,
                participants,
            } => {
                self.chat.clear();
                self.participants.replace_room(participants);
                for message in history {
                    self.push_chat(message);
                }
                self.chat.bottom();
                self.set_status(format!("joined room {}", room_id.0));
            }
            NetworkEvent::Chat(message) => self.push_chat(message),
            NetworkEvent::Presence {
                participant,
                online,
                ..
            } => {
                let name = participant.name.clone();
                self.participants.set_presence(participant, online);
                self.set_status(format!("{name} {}", if online { "joined" } else { "left" }));
            }
            NetworkEvent::VoiceStarted { user_id, stream_id } => {
                self.participants.voice_started(user_id, stream_id);
                if Some(user_id) == self.user_id {
                    self.call_enabled = true;
                    self.set_status("call connected");
                } else {
                    self.set_status(format!("user {} joined the call", user_id.0));
                }
            }
            NetworkEvent::VoiceStopped { user_id, stream_id } => {
                self.participants.voice_stopped(user_id, stream_id);
                if Some(user_id) == self.user_id {
                    self.call_enabled = false;
                    self.voice_tx_enabled.store(false, Ordering::Relaxed);
                    self.playback.take();
                    self.set_status("call stopped");
                } else {
                    if let Some(playback) = &self.playback {
                        playback.stop_stream(stream_id.0);
                    }
                    self.set_status(format!("user {} left the call", user_id.0));
                }
            }
            NetworkEvent::PeerTransport { user_id, direct } => {
                self.participants.set_peer_transport(user_id, direct);
            }
            NetworkEvent::VoicePacket(packet) => {
                self.voice_packets_received = self.voice_packets_received.saturating_add(1);
                self.voice_bytes_received = self
                    .voice_bytes_received
                    .saturating_add(packet.payload.len() as u64);
                self.participants.voice_packet(packet.stream_id);
                if let Some(playback) = &self.playback {
                    playback.push(packet);
                }
            }
            NetworkEvent::Status(status) => self.set_status(status),
            NetworkEvent::Error(error) => {
                kvlog::warn!("app network error", error = error.as_str());
                self.set_error(format!("error: {error}"));
            }
            NetworkEvent::ReconnectScheduled { retry_in } => {
                self.voice_tx_enabled.store(false, Ordering::Relaxed);
                self.call_enabled = false;
                self.playback.take();
                self.set_error(format!(
                    "server down; disconnected retrying in {}s",
                    retry_in.as_secs()
                ));
            }
            NetworkEvent::Disconnected => {
                self.voice_tx_enabled.store(false, Ordering::Relaxed);
                self.call_enabled = false;
                self.playback.take();
                self.set_error("disconnected");
            }
        }
    }

    fn push_chat(&mut self, message: ChatMessage) {
        let local = Some(message.sender) == self.user_id;
        self.participants.note_message(&message);
        self.chat.push_chat(message, local);
        if self.chat.scroll_offset() == 0 {
            self.chat.bottom();
        }
    }

    fn process_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }

        let base = match self.mode {
            theme::UiMode::Compose => bindings::COMPOSE_LAYER,
            theme::UiMode::Log => bindings::LOG_LAYER,
            theme::UiMode::Settings => bindings::SETTINGS_LAYER,
        };
        if let Some(input) = InputKey::from_event(&key) {
            match bindings::resolve(
                &self.config.bindings.router,
                base,
                &mut self.pending_chord,
                input,
            ) {
                Resolved::Action(id) => {
                    let command = self.config.bindings.actions.get(id).clone();
                    return self.process_command(command);
                }
                Resolved::Consumed => return Action::Continue,
                Resolved::Unmatched => {}
            }
        }

        match self.mode {
            theme::UiMode::Compose => {
                let _ = self.composer.send_key(&key);
            }
            theme::UiMode::Log | theme::UiMode::Settings => {}
        }
        Action::Continue
    }

    fn process_command(&mut self, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            EnterCompose => {
                self.mode = theme::UiMode::Compose;
                self.composer.enter_insert_mode();
            }
            EnterLog => self.mode = theme::UiMode::Log,
            OpenSettings => self.open_settings(),
            CloseSettings => self.close_settings(),
            SubmitMessage => self.submit_input(),
            Cancel => {
                if self.mode == theme::UiMode::Compose {
                    self.composer.clear();
                    self.composer.enter_insert_mode();
                } else {
                    self.mode = theme::UiMode::Compose;
                }
            }
            Quit => return Action::Quit,
            ScrollUp => self.chat.scroll_up(1),
            ScrollDown => self.chat.scroll_down(1),
            RoomScrollUp => self.scroll_room(-1),
            RoomScrollDown => self.scroll_room(1),
            HalfPageUp => self.chat.scroll_up(10),
            HalfPageDown => self.chat.scroll_down(10),
            Top => self.chat.top(self.last_chat_width),
            Bottom => self.chat.bottom(),
            ToggleMute => self.set_mute(!self.mic_muted.load(Ordering::Relaxed)),
            StartCall => self.start_call(),
            StopCall => self.stop_call(),
            RefreshDevices => self.refresh_input_devices(),
            SaveSettings => self.save_settings(),
            Activate => self.activate_settings_focus(),
            FocusNext => self.move_settings_focus(1),
            FocusPrev => self.move_settings_focus(-1),
            AdjustLeft => self.adjust_settings_focus(-1),
            AdjustRight => self.adjust_settings_focus(1),
            ClearChat => self.chat.clear(),
        }
        Action::Continue
    }

    fn open_settings(&mut self) {
        self.mode = theme::UiMode::Settings;
        self.settings = SettingsDraft::from_audio(&self.config.audio);
        self.settings_dirty = false;
        if self.devices.is_empty() {
            self.refresh_input_devices();
        }
    }

    fn close_settings(&mut self) {
        self.mode = theme::UiMode::Compose;
    }

    fn move_settings_focus(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        let len = SettingsFocus::ORDER.len() as isize;
        let next = (self.settings_focus.index() as isize + delta).rem_euclid(len) as usize;
        self.settings_focus = SettingsFocus::ORDER[next];
    }

    fn adjust_settings_focus(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        match self.settings_focus {
            SettingsFocus::Device => self.adjust_input_device(delta),
            SettingsFocus::Bitrate => {
                self.settings.bitrate_index =
                    cycle_index(self.settings.bitrate_index, BITRATES.len(), delta);
                self.mark_settings_dirty();
            }
            SettingsFocus::Denoise => {
                self.settings.denoise = !self.settings.denoise;
                self.mark_settings_dirty();
            }
            SettingsFocus::Buffer => {
                self.settings.buffer_index = cycle_index(
                    self.settings.buffer_index,
                    BufferRequest::OPTIONS.len(),
                    delta,
                );
                self.mark_settings_dirty();
            }
            SettingsFocus::Refresh | SettingsFocus::Save | SettingsFocus::Close => {}
        }
    }

    fn activate_settings_focus(&mut self) {
        match self.settings_focus {
            SettingsFocus::Denoise => {
                self.settings.denoise = !self.settings.denoise;
                self.mark_settings_dirty();
            }
            SettingsFocus::Refresh => self.refresh_input_devices(),
            SettingsFocus::Save => self.save_settings(),
            SettingsFocus::Close => self.close_settings(),
            SettingsFocus::Device | SettingsFocus::Bitrate | SettingsFocus::Buffer => {
                self.adjust_settings_focus(1);
            }
        }
    }

    fn adjust_input_device(&mut self, delta: isize) {
        let len = self.devices.len() + 1;
        let current = self
            .settings
            .input_device_index
            .map(|index| index as usize + 1)
            .unwrap_or(0);
        let next = cycle_index(current, len, delta);
        self.settings.input_device_index = (next > 0).then_some((next - 1) as u32);
        self.mark_settings_dirty();
    }

    fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.set_status("settings draft changed; press w to save");
    }

    fn scroll_room(&mut self, delta: isize) {
        let max = self.participants.entries.len().saturating_sub(1);
        if delta < 0 {
            self.participants.scroll = self
                .participants
                .scroll
                .saturating_sub(delta.unsigned_abs());
        } else {
            self.participants.scroll = (self.participants.scroll + delta as usize).min(max);
        }
    }

    fn save_settings(&mut self) {
        self.config.audio = self.settings.to_audio();
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.settings_dirty = false;
                if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
                    self.set_status(format!(
                        "settings saved to {}; audio applies next call",
                        path.display()
                    ));
                } else {
                    self.restart_mic_monitor();
                    self.chat
                        .set_max_messages(self.config.ui.max_messages as usize);
                    self.set_status(format!("settings saved to {}", path.display()));
                }
            }
            Err(error) => self.set_error(error),
        }
    }

    fn refresh_input_devices(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            self.set_error("stop call before refreshing devices");
            return;
        }
        match audio::input_devices(self.settings.buffer_request()) {
            Ok(devices) => {
                let count = devices.len();
                self.devices = devices;
                if self
                    .settings
                    .input_device_index
                    .is_some_and(|index| index as usize >= self.devices.len())
                {
                    self.settings.input_device_index = None;
                    self.settings_dirty = true;
                }
                self.set_status(format!("found {count} input device(s)"));
            }
            Err(error) => {
                self.devices.clear();
                self.settings.input_device_index = None;
                self.mic_error = Some(error.clone());
                self.set_error(error);
            }
        }
    }

    fn submit_input(&mut self) {
        let text = self.composer.text();
        let input = text.trim();
        if input.is_empty() {
            return;
        }
        let input = input.to_string();
        self.composer.clear();
        self.composer.enter_insert_mode();
        match input.as_str() {
            "/quit" => self.set_status("use Ctrl-C to quit"),
            "/call" => self.start_call(),
            "/hangup" => self.stop_call(),
            "/mute" => self.set_mute(true),
            "/unmute" => self.set_mute(false),
            "/muted" => self.show_mute_status(),
            "/clear" => self.chat.clear(),
            "/config" | "/settings" => self.open_settings(),
            "/users" => self.show_users(),
            "/whoami" => self.show_current_user(),
            command if command.starts_with("/user ") => self.change_user_command(command),
            command if command.starts_with('/') => {
                self.set_error(format!("unknown command: {command}"))
            }
            body => self
                .network
                .send(NetworkCommand::SendChat(body.to_string())),
        }
    }

    fn set_mute(&mut self, muted: bool) {
        self.mic_muted.store(muted, Ordering::Relaxed);
        self.set_status(if muted {
            "microphone muted"
        } else {
            "microphone unmuted"
        });
    }

    fn show_mute_status(&mut self) {
        self.set_status(if self.mic_muted.load(Ordering::Relaxed) {
            "microphone muted"
        } else {
            "microphone unmuted"
        });
    }

    fn show_users(&mut self) {
        if self.participants.entries.is_empty() {
            self.set_status(format!("known users: {}", KNOWN_USERS.join(", ")));
        } else {
            let users = self
                .participants
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.set_status(format!("users: {users}"));
        }
    }

    fn show_current_user(&mut self) {
        self.set_status(match self.user_id {
            Some(user_id) => format!("signed in as {} (user {})", self.user, user_id.0),
            None => format!("connecting as {}", self.user),
        });
    }

    fn change_user_command(&mut self, command: &str) {
        let mut parts = command.split_whitespace();
        let _ = parts.next();
        let Some(user) = parts.next() else {
            self.set_error("usage: /user alice|bob|carol [token]");
            return;
        };
        if !is_known_user(user) {
            self.set_error("known users: alice, bob, carol");
            return;
        }
        let token = parts
            .next()
            .map(ToString::to_string)
            .unwrap_or_else(|| config::default_token(user).to_string());
        if parts.next().is_some() {
            self.set_error("usage: /user alice|bob|carol [token]");
            return;
        }
        self.reconnect_as(user.to_string(), token);
    }

    fn reconnect_as(&mut self, user: String, token: String) {
        self.stop_audio();
        self.call_enabled = false;
        self.network.shutdown();
        self.config.network.user = user;
        self.config.network.token = token;
        self.user = self.config.network.user.clone();
        self.room_name = "lobby".to_string();
        self.chat.clear();
        self.participants = Participants::default();
        self.session_id = None;
        self.user_id = None;
        self.voice_packets_received = 0;
        self.voice_bytes_received = 0;
        self.set_status(format!("connecting as {}", self.user));

        let (event_tx, event_rx) = mpsc::channel();
        self.network = NetworkClient::spawn(self.config.network.client_config(), event_tx);
        self.event_rx = event_rx;
        self.start_mic_monitor();
    }

    fn start_mic_monitor(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            return;
        }
        match self.ensure_mic_capture() {
            Ok(()) => self.set_status("mic monitor active"),
            Err(error) => self.set_error(format!("mic monitor failed: {error}")),
        }
    }

    fn restart_mic_monitor(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            self.set_status("settings apply to the next call");
            return;
        }
        self.stop_mic_capture();
        self.start_mic_monitor();
    }

    fn ensure_mic_capture(&mut self) -> Result<(), String> {
        if self.capture.is_some() {
            return Ok(());
        }
        if let Some(index) = self.config.audio.input_device_index {
            if !self.devices.is_empty() {
                let Some(device) = self.devices.get(index as usize) else {
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
        }

        let tx = self.network.sender();
        let mic_muted = Arc::clone(&self.mic_muted);
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        match audio::start_live_capture(
            LiveCaptureConfig {
                input_device_index: self
                    .config
                    .audio
                    .input_device_index
                    .map(|index| index as usize),
                bitrate_bps: self.config.audio.bitrate_bps,
                denoise: self.config.audio.denoise,
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
                Err(error)
            }
        }
    }

    fn start_call(&mut self) {
        if self.voice_tx_enabled.load(Ordering::Relaxed) || self.playback.is_some() {
            self.set_status("call already active");
            return;
        }
        if let Err(error) = self.ensure_mic_capture() {
            self.set_error(format!("failed to start capture: {error}"));
            return;
        }
        match audio::start_live_playback(self.buffer_request()) {
            Ok(playback) => {
                self.playback = Some(playback);
                self.set_status("starting call");
            }
            Err(error) => {
                self.playback = None;
                self.set_error(format!("starting call; playback unavailable: {error}"));
            }
        }
        self.voice_tx_enabled.store(true, Ordering::Relaxed);
        self.network.send(NetworkCommand::StartVoice);
        self.voice_packets_received = 0;
        self.voice_bytes_received = 0;
    }

    fn stop_call(&mut self) {
        self.network.send(NetworkCommand::StopVoice);
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.playback.take();
        self.call_enabled = false;
        self.set_status("stopping call; mic monitor active");
    }

    fn stop_audio(&mut self) {
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.stop_mic_capture();
        self.playback.take();
    }

    fn stop_mic_capture(&mut self) {
        self.capture.take();
    }

    fn buffer_request(&self) -> BufferRequest {
        self.config.audio.buffer.to_request()
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.status_kind = StatusKind::Info;
    }

    fn set_error(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.status_kind = StatusKind::Error;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_audio();
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().collect::<Vec<_>>();
    let logfile =
        config::value_arg(&args, "--logfile").or_else(|| std::env::var("TOMCHAT_LOGFILE").ok());
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
    buffer.set_rgb_supported(true);
    let mut events = Events::default();
    let stdin = std::io::stdin();
    let config_path = config::value_arg(&args, "--config");
    let mut app = App::new(Config::load(config_path.as_deref())?);
    app.start_mic_monitor();

    loop {
        app.drain_network_events();
        render(&mut app, &mut buffer);
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

fn render(app: &mut App, buf: &mut Buffer) {
    buf.rect().with(theme::BACKGROUND).fill(buf);
    buf.hide_cursor();

    let mut screen = buf.rect();
    let composer_height = composer_height(app, screen.w);
    let composer_area = screen.take_bottom(composer_height as i32);
    let status_area = screen.take_bottom(1);
    let room_height = app.config.ui.room_height.min(screen.h.saturating_sub(1));
    if room_height > 0 {
        let room_area = screen.take_top(room_height as i32);
        draw_room(room_area, app, buf);
    }
    if screen.h > 0 {
        let title_area = screen.take_top(1);
        draw_room_title(title_area, app, buf);
    }

    match app.mode {
        theme::UiMode::Settings => draw_settings(screen, app, buf),
        theme::UiMode::Compose | theme::UiMode::Log => draw_chat(screen, app, buf),
    }
    draw_status(status_area, app, buf);
    draw_composer(composer_area, app, buf);
}

fn composer_height(app: &mut App, width: u16) -> u16 {
    app.composer.resize(width.max(1));
    app.composer
        .desired_height()
        .min(app.config.ui.max_composer_height.max(1))
        .max(1)
}

fn draw_room(area: Rect, app: &App, buf: &mut Buffer) {
    area.with(theme::PANEL_ALT).fill(buf);
    let mut rows = area;
    let visible = rows.h as usize;
    let start = app.participants.scroll.min(app.participants.entries.len());
    for participant in app.participants.entries.iter().skip(start).take(visible) {
        let row = rows.take_top(1);
        let state = if participant.in_call && Some(participant.user_id) == app.user_id {
            "voice"
        } else if participant.in_call && participant.p2p_direct {
            "p2p"
        } else if participant.in_call {
            "relay"
        } else if participant.online {
            "online"
        } else {
            "away"
        };
        let spoke = participant
            .last_voice_at
            .map(age_label)
            .or_else(|| participant.last_message_ms.map(|_| "msg".to_string()))
            .unwrap_or_else(|| "--".to_string());
        let style = if participant.in_call {
            theme::GOOD
        } else if participant.online {
            theme::TEXT
        } else {
            theme::MUTED
        };
        row.with(theme::PANEL_ALT).fill(buf);
        row.with(style).with(Ellipsis(true)).text(
            buf,
            &format!("  {:<16} {:<7} {}", participant.name, state, spoke),
        );
    }
}

fn draw_room_title(area: Rect, app: &App, buf: &mut Buffer) {
    area.with(theme::STATUS_SECTION | Modifier::BOLD).fill(buf);
    area.with(theme::STATUS_SECTION | Modifier::BOLD).text(
        buf,
        &format!(
            " ROOM {}  online {}/{}  call {} ",
            app.room_name,
            app.participants.online_count(),
            app.participants.entries.len(),
            app.participants.in_call_count()
        ),
    );
}

fn draw_chat(area: Rect, app: &mut App, buf: &mut Buffer) {
    area.with(theme::BACKGROUND).fill(buf);
    if area.is_empty() {
        return;
    }
    let name_width = NAME_COL_WIDTH.min(area.w.saturating_sub(1));
    let body_width = area.w.saturating_sub(name_width).max(1);
    app.last_chat_width = body_width;
    if app.chat.is_empty() {
        area.with(theme::SUBTLE)
            .with(HAlign::Center)
            .text(buf, "No messages");
        return;
    }
    let lines = app
        .chat
        .visible_lines(body_width, area.h, app.config.ui.overscan as usize);
    let mut row_area = area;
    let empty_rows = (area.h as usize).saturating_sub(lines.len());
    for _ in 0..empty_rows {
        row_area.take_top(1).with(theme::BACKGROUND).fill(buf);
    }
    for line in lines {
        let msg = app.chat.message(line.message);
        let mut row = row_area.take_top(1);
        let base = if msg.local {
            theme::LOCAL_LINE
        } else {
            theme::BACKGROUND
        };
        row.with(base).fill(buf);
        let name_area = row.take_left(name_width as i32);
        if line.line == 0 {
            name_area
                .with(base.patch(if msg.local {
                    theme::GOOD
                } else {
                    theme::ACCENT
                }))
                .with(HAlign::Right)
                .with(Ellipsis(true))
                .text(buf, &format!("{} ", msg.sender));
        } else {
            name_area
                .with(base.patch(theme::SUBTLE))
                .with(HAlign::Right)
                .text(buf, "│ ");
        }
        for seg in app.chat.line(line.message, line.line) {
            let start = seg.start as usize;
            let end = seg.end as usize;
            let text = &msg.body[start..end];
            let style = base.patch(theme::TEXT).patch(seg.style);
            let max_width = row.w.saturating_sub(seg.col) as usize;
            if max_width > 0 {
                buf.set_stringn(row.x + seg.col, row.y, text, max_width, style);
            }
        }
    }
}

fn draw_settings(area: Rect, app: &App, buf: &mut Buffer) {
    area.with(theme::BACKGROUND).fill(buf);
    let mut rows = area;
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Input",
        &selected_input_device_label(app),
        app.settings_focus == SettingsFocus::Device,
        app.settings_dirty,
    );
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Bitrate",
        &format!("{} kbps", app.settings.bitrate_bps() / 1000),
        app.settings_focus == SettingsFocus::Bitrate,
        app.settings_dirty,
    );
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Denoise",
        if app.settings.denoise { "on" } else { "off" },
        app.settings_focus == SettingsFocus::Denoise,
        app.settings_dirty,
    );
    draw_settings_row(
        rows.take_top(1),
        buf,
        "Buffer",
        app.settings.buffer_request().label(),
        app.settings_focus == SettingsFocus::Buffer,
        app.settings_dirty,
    );
    rows.take_top(1).with(theme::BACKGROUND).fill(buf);
    draw_button_row(
        rows.take_top(1),
        buf,
        "Refresh devices",
        app.settings_focus == SettingsFocus::Refresh,
    );
    draw_button_row(
        rows.take_top(1),
        buf,
        "Save config",
        app.settings_focus == SettingsFocus::Save,
    );
    draw_button_row(
        rows.take_top(1),
        buf,
        "Back to chat",
        app.settings_focus == SettingsFocus::Close,
    );
}

fn draw_settings_row(
    area: Rect,
    buf: &mut Buffer,
    label: &str,
    value: &str,
    focused: bool,
    dirty: bool,
) {
    let style = if focused {
        theme::PANEL_ALT
    } else {
        theme::BACKGROUND
    };
    area.with(style).fill(buf);
    let mut row = area;
    row.take_left(16)
        .with(style.patch(if focused { theme::GOOD } else { theme::MUTED }))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(style.patch(if dirty { theme::WARN } else { theme::TEXT }))
        .with(Ellipsis(true))
        .text(buf, value);
}

fn draw_button_row(area: Rect, buf: &mut Buffer, label: &str, focused: bool) {
    let style = if focused {
        theme::PANEL_ALT
    } else {
        theme::BACKGROUND
    };
    area.with(style).fill(buf);
    area.with(style.patch(if focused { theme::GOOD } else { theme::TEXT }))
        .text(buf, "  ")
        .text(buf, label);
}

fn draw_status(area: Rect, app: &App, buf: &mut Buffer) {
    area.with(theme::STATUS_FILL).fill(buf);
    let capture = app
        .capture
        .as_ref()
        .map(|capture| capture.stats().snapshot());
    let mut row = area
        .with(theme::mode_style(app.mode))
        .fmt(buf, format_args!(" {} ", app.mode.label()));
    row = row
        .with(theme::STATUS_SECTION)
        .fmt(buf, format_args!(" {} ", app.room_name));
    row = row
        .with(theme::STATUS_FILL)
        .fmt(buf, format_args!(" {} ", app.user));
    row = row
        .with(call_style(app))
        .fmt(buf, format_args!(" {} ", call_state_label(app)));
    row = row.with(theme::STATUS_FILL).fmt(
        buf,
        format_args!(" {} ", mic_status_compact(app, capture.as_ref())),
    );
    row = row.with(theme::STATUS_FILL).fmt(
        buf,
        format_args!(" {}", vu_meter(capture.as_ref().map_or(0.0, |s| s.rms))),
    );
    row = row.with(theme::STATUS_FILL.patch(theme::SUBTLE)).fmt(
        buf,
        format_args!(
            " {} msg/{} rows ",
            app.chat.len(),
            app.chat.total_lines_estimate()
        ),
    );

    let right_style = match app.status_kind {
        StatusKind::Info => theme::STATUS_FILL.patch(theme::MUTED),
        StatusKind::Error => theme::STATUS_FILL.patch(theme::ERROR),
    };
    let status_text = if let Some(chord) = &app.pending_chord {
        format!(
            "{} chord {}ms",
            chord.label.as_deref().unwrap_or("pending"),
            chord.activated_at.elapsed().as_millis()
        )
    } else {
        app.status.clone()
    };
    row.with(HAlign::Right)
        .with(right_style)
        .with(Ellipsis(true))
        .text(buf, &format!(" {} ", status_text));
}

fn draw_composer(area: Rect, app: &mut App, buf: &mut Buffer) {
    area.with(theme::PANEL).fill(buf);
    app.composer.resize(area.w.max(1));
    app.composer_hl.render(&mut app.composer, area, buf);
    if app.composer.text_len() == 0 {
        area.with(theme::MUTED)
            .with(Ellipsis(true))
            .text(buf, &format!(" {}", app.config.ui.placeholder));
    }
}

fn call_style(app: &App) -> Style {
    if app.call_enabled {
        theme::GOOD
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        theme::WARN
    } else {
        theme::STATUS_FILL
    }
}

fn mic_status_compact(app: &App, capture: Option<&StatsSnapshot>) -> String {
    let mute = if app.mic_muted.load(Ordering::Relaxed) {
        "muted"
    } else {
        "open"
    };
    match capture {
        Some(capture) => format!(
            "{mute} {}kbps vad{:02}%",
            app.config.audio.bitrate_bps / 1000,
            (capture.vad_probability.clamp(0.0, 1.0) * 100.0).round() as u32
        ),
        None => format!("{mute} inactive"),
    }
}

fn call_state_label(app: &App) -> &'static str {
    if app.call_enabled {
        "call"
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        "starting"
    } else {
        "idle"
    }
}

fn vu_meter(rms: f32) -> String {
    const WIDTH: usize = 10;
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

fn dbfs(rms: f32) -> f32 {
    if rms <= f32::EPSILON {
        -60.0
    } else {
        (20.0 * rms.clamp(f32::EPSILON, 1.0).log10()).max(-60.0)
    }
}

fn selected_input_device_label(app: &App) -> String {
    let Some(index) = app.settings.input_device_index else {
        return "system default".to_string();
    };
    match app.devices.get(index as usize) {
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

fn age_label(instant: Instant) -> String {
    let secs = instant.elapsed().as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

fn cycle_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).rem_euclid(len as isize) as usize
}

fn is_known_user(user: &str) -> bool {
    KNOWN_USERS.contains(&user)
}

fn network_event_kind(event: &NetworkEvent) -> &'static str {
    match event {
        NetworkEvent::Connected => "connected",
        NetworkEvent::Authenticated { .. } => "authenticated",
        NetworkEvent::RoomJoined { .. } => "room_joined",
        NetworkEvent::Chat(_) => "chat",
        NetworkEvent::Presence { .. } => "presence",
        NetworkEvent::VoiceStarted { .. } => "voice_started",
        NetworkEvent::VoiceStopped { .. } => "voice_stopped",
        NetworkEvent::PeerTransport { .. } => "peer_transport",
        NetworkEvent::VoicePacket(_) => "voice_packet",
        NetworkEvent::Status(_) => "status",
        NetworkEvent::Error(_) => "error",
        NetworkEvent::ReconnectScheduled { .. } => "reconnect_scheduled",
        NetworkEvent::Disconnected => "disconnected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let (_event_tx, event_rx) = mpsc::channel();
        let (command_tx, _command_rx) = mpsc::channel();
        let mut composer = Editor::new();
        composer.set_theme(theme::editor_theme());
        composer.set_wrap(true);
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);
        App {
            config: Config::default(),
            user: "alice".to_string(),
            room_name: "lobby".to_string(),
            status: "test".to_string(),
            status_kind: StatusKind::Info,
            mode: theme::UiMode::Compose,
            composer,
            composer_hl,
            chat: VirtualChatBuffer::new(100),
            participants: Participants::default(),
            last_chat_width: 80,
            pending_chord: None,
            event_rx,
            network: NetworkClient::from_parts_for_test(command_tx),
            session_id: Some(SessionId(1)),
            user_id: Some(UserId(1)),
            devices: Vec::new(),
            settings_focus: SettingsFocus::Device,
            settings: SettingsDraft::from_audio(&AudioConfig::default()),
            settings_dirty: false,
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

    #[test]
    fn default_config_parses() {
        let config = Config::default();
        assert_eq!(config.network.user, "alice");
        assert_eq!(config.audio.bitrate_bps, 24_000);
    }

    #[test]
    fn participant_transport_badge_tracks_direct_path() {
        let mut participants = Participants::default();
        participants.upsert(
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: true,
            },
            true,
        );

        participants.set_peer_transport(UserId(2), true);
        let bob = participants
            .entries
            .iter()
            .find(|entry| entry.user_id == UserId(2))
            .expect("bob participant");
        assert!(bob.p2p_direct);

        participants.upsert(
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: true,
            },
            true,
        );
        let bob = participants
            .entries
            .iter()
            .find(|entry| entry.user_id == UserId(2))
            .expect("bob participant");
        assert!(bob.p2p_direct);

        participants.upsert(
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: false,
            },
            true,
        );
        let bob = participants
            .entries
            .iter()
            .find(|entry| entry.user_id == UserId(2))
            .expect("bob participant");
        assert!(!bob.p2p_direct);
    }

    #[test]
    fn renders_smoke_frame() {
        let mut app = test_app();
        let mut buffer = Buffer::new(80, 24);
        render(&mut app, &mut buffer);
    }

    #[test]
    fn reconnect_scheduled_sets_down_error_status() {
        let mut app = test_app();

        app.handle_network_event(NetworkEvent::ReconnectScheduled {
            retry_in: Duration::from_secs(5),
        });

        assert_eq!(app.status, "server down; disconnected retrying in 5s");
        assert_eq!(app.status_kind, StatusKind::Error);
        assert!(!app.call_enabled);
    }

    #[test]
    fn shift_enter_inserts_newline_in_composer() {
        let mut app = test_app();
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty())),
            Action::Continue
        ));
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            Action::Continue
        ));
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty())),
            Action::Continue
        ));
        assert_eq!(app.composer.text(), "a\nb");
    }
}
