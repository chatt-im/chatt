#[allow(dead_code)]
mod audio;
mod bindings;
mod chat_buffer;
#[allow(dead_code)]
mod client_net;
mod config;
mod fuzzy;
mod local_control;
#[cfg_attr(not(test), allow(dead_code))]
mod network;
#[allow(dead_code)]
mod packet_log;
mod settings;
mod theme;
mod ui;

use std::{
    path::{Path, PathBuf},
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
use config::Config;
use extui::{
    Buffer, Ellipsis, HAlign, Rect, Style, Terminal, TerminalFlags,
    event::{
        self, Event, Events, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        polling::GlobalWakerConfig,
    },
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
use settings::{AudioInputPickerState, BITRATES, SettingsDraft, SettingsFocus};
use tinyhl::{Highlighter, Source};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const KNOWN_USERS: &[&str] = &["alice", "bob", "carol"];
const NAME_COL_WIDTH: u16 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusKind {
    Info,
    Error,
}

#[derive(Clone, Debug)]
struct ParticipantState {
    user_id: UserId,
    name: String,
    online: bool,
    voice_active: bool,
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
            existing.voice_active = participant.in_call;
            if !participant.in_call {
                existing.p2p_direct = false;
            }
        } else {
            self.entries.push(ParticipantState {
                user_id: participant.user_id,
                name: participant.name,
                online,
                voice_active: participant.in_call,
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
        entry.voice_active = true;
        entry.active_stream = Some(stream_id);
        entry.last_voice_at = Some(Instant::now());
    }

    fn voice_stopped(&mut self, user_id: UserId, stream_id: StreamId) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.user_id == user_id)
        {
            entry.voice_active = false;
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
            voice_active: false,
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
                .then_with(|| b.voice_active.cmp(&a.voice_active))
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
    audio_input_items: Vec<settings::AudioInputItem>,
    audio_input_picker: AudioInputPickerState,
    settings_focus: SettingsFocus,
    settings: SettingsDraft,
    settings_dirty: bool,
    mic_muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    voice_tx_enabled: Arc<AtomicBool>,
    mic_error: Option<String>,
    capture: Option<LiveCapture>,
    playback: Option<LivePlayback>,
    voice_packets_received: u64,
    voice_bytes_received: u64,
}

enum Action {
    Continue,
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    RunUi,
    Upload { path: PathBuf },
    DebugAudioInputs,
}

impl App {
    fn new(config: Config) -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let client_config = config.network.client_config(&config.files);
        let network = NetworkClient::spawn(client_config, event_tx);
        let mut composer =
            Editor::with_bindings(editor_bindings::vim(editor_bindings::VimOptions::default()));
        composer.set_wrap(true);
        composer.set_height_bounds(1, config.ui.max_composer_height.max(1));
        composer.set_theme(theme::editor_theme());
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);
        let settings_draft = SettingsDraft::from_audio(&config.audio);
        let audio_input_items = settings::audio_input_items(&[]);
        let mut audio_input_picker = AudioInputPickerState::default();
        audio_input_picker.reset(
            &audio_input_items,
            settings_draft.input_device_id.as_deref(),
        );
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
            audio_input_items,
            audio_input_picker,
            settings_focus: SettingsFocus::Device,
            settings: settings_draft,
            settings_dirty: false,
            mic_muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            playback: None,
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
                self.start_room_voice();
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
                    self.set_status("voice stream ready");
                } else {
                    self.set_status(format!("user {} voice ready", user_id.0));
                }
            }
            NetworkEvent::VoiceStopped { user_id, stream_id } => {
                self.participants.voice_stopped(user_id, stream_id);
                if Some(user_id) == self.user_id {
                    self.stop_audio();
                    self.set_status("voice stopped");
                } else {
                    if let Some(playback) = &self.playback {
                        playback.stop_stream(stream_id.0);
                    }
                    self.set_status(format!("user {} left voice", user_id.0));
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
                if !self.deafened.load(Ordering::Relaxed)
                    && let Some(playback) = &self.playback
                {
                    playback.push(packet);
                }
            }
            NetworkEvent::Status(status) => self.set_status(status),
            NetworkEvent::Error(error) => {
                kvlog::warn!("app network error", error = error.as_str());
                self.set_error(format!("error: {error}"));
            }
            NetworkEvent::ReconnectScheduled { retry_in } => {
                self.stop_audio();
                self.set_error(format!(
                    "server down; disconnected retrying in {}s",
                    retry_in.as_secs()
                ));
            }
            NetworkEvent::Disconnected => {
                self.stop_audio();
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

        if self.handle_settings_search_key(key) {
            return Action::Continue;
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

    fn handle_settings_search_key(&mut self, key: KeyEvent) -> bool {
        if self.mode != theme::UiMode::Settings
            || self.settings_focus != SettingsFocus::Device
            || !self.audio_input_picker.open
        {
            return false;
        }

        if self.audio_input_picker.searching {
            return self
                .audio_input_picker
                .edit_search(key, &self.audio_input_items);
        }

        if matches!(key.kind, KeyEventKind::Release) {
            return false;
        }
        let mut modifiers = key.modifiers;
        modifiers.remove(KeyModifiers::SHIFT);
        if modifiers.is_empty() && key.code == KeyCode::Char('/') {
            self.audio_input_picker
                .start_search(&self.audio_input_items);
            return true;
        }

        false
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
                if self.mode == theme::UiMode::Settings {
                    if self.audio_input_picker.open {
                        self.cancel_audio_input_picker();
                    } else {
                        self.close_settings();
                    }
                } else if self.mode == theme::UiMode::Compose {
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
            ToggleDeafen => self.set_deafen(!self.deafened.load(Ordering::Relaxed)),
            RefreshDevices => self.refresh_input_devices(),
            SaveSettings => self.save_settings(),
            Activate => self.activate_settings_focus(),
            FocusNext => self.move_settings_focus(1),
            FocusPrev => self.move_settings_focus(-1),
            SelectNext => self.move_settings_selection(1),
            SelectPrev => self.move_settings_selection(-1),
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
        self.rebuild_audio_input_picker();
    }

    fn close_settings(&mut self) {
        self.audio_input_picker.reset(
            &self.audio_input_items,
            self.settings.input_device_id.as_deref(),
        );
        self.mode = theme::UiMode::Compose;
    }

    fn move_settings_focus(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        if self.audio_input_picker.open {
            if self.settings_focus == SettingsFocus::Device {
                self.audio_input_picker.move_selection(delta);
            }
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
            SettingsFocus::Device if delta < 0 => self.cancel_audio_input_picker(),
            SettingsFocus::Device => self.activate_audio_input_picker(),
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
            SettingsFocus::Device => self.activate_audio_input_picker(),
            SettingsFocus::Bitrate | SettingsFocus::Buffer => {
                self.adjust_settings_focus(1);
            }
        }
    }

    fn move_settings_selection(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        if self.settings_focus == SettingsFocus::Device && self.audio_input_picker.open {
            self.audio_input_picker.move_selection(delta);
        } else {
            self.move_settings_focus(delta);
        }
    }

    fn activate_audio_input_picker(&mut self) {
        if self.audio_input_picker.open {
            self.confirm_audio_input_picker();
        } else {
            if self.devices.is_empty() {
                self.refresh_input_devices();
            }
            self.audio_input_picker.open(
                &self.audio_input_items,
                self.settings.input_device_id.as_deref(),
            );
        }
    }

    fn confirm_audio_input_picker(&mut self) {
        let Some(next) = self.audio_input_picker.confirm(&self.audio_input_items) else {
            return;
        };
        if self.settings.input_device_id != next {
            self.settings.input_device_id = next;
            self.mark_settings_dirty();
        }
    }

    fn cancel_audio_input_picker(&mut self) {
        if let Some(selection) = self.audio_input_picker.cancel(&self.audio_input_items) {
            self.settings.input_device_id = selection;
        }
    }

    fn rebuild_audio_input_picker(&mut self) {
        self.audio_input_items = settings::audio_input_items(&self.devices);
        self.audio_input_picker.reset(
            &self.audio_input_items,
            self.settings.input_device_id.as_deref(),
        );
    }

    fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.set_status("settings draft changed; save config when ready");
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
                if self.capture.is_some() || self.playback.is_some() {
                    self.set_status(format!(
                        "settings saved to {}; audio applies after deafen/rejoin",
                        path.display()
                    ));
                } else {
                    self.chat
                        .set_max_messages(self.config.ui.max_messages as usize);
                    self.set_status(format!("settings saved to {}", path.display()));
                }
            }
            Err(error) => self.set_error(error),
        }
    }

    fn refresh_input_devices(&mut self) {
        if self.capture.is_some() || self.playback.is_some() {
            self.set_error("deafen before refreshing devices");
            return;
        }
        match audio::input_devices(self.settings.buffer_request()) {
            Ok(devices) => {
                let count = devices.len();
                self.devices = devices;
                self.rebuild_audio_input_picker();
                self.set_status(format!("found {count} input device(s)"));
            }
            Err(error) => {
                self.devices.clear();
                self.settings.input_device_id = None;
                self.rebuild_audio_input_picker();
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
            "/mute" => self.set_mute(true),
            "/unmute" => self.set_mute(false),
            "/deafen" => self.set_deafen(true),
            "/undeafen" => self.set_deafen(false),
            "/muted" => self.show_mute_status(),
            "/deafened" => self.show_deafen_status(),
            "/clear" => self.chat.clear(),
            "/config" | "/settings" => self.open_settings(),
            "/users" => self.show_users(),
            "/whoami" => self.show_current_user(),
            command if command.starts_with("/upload ") => self.upload_file_command(command),
            command if command.starts_with("/user ") => self.change_user_command(command),
            command if command.starts_with('/') => {
                self.set_error(format!("unknown command: {command}"))
            }
            body => self
                .network
                .send(NetworkCommand::SendChat(body.to_string())),
        }
    }

    fn upload_file_command(&mut self, command: &str) {
        let path = command.trim_start_matches("/upload ").trim();
        if path.is_empty() {
            self.set_error("usage: /upload file_path/filename.ext");
            return;
        }
        self.network
            .send(NetworkCommand::UploadFile(std::path::PathBuf::from(path)));
        self.set_status(format!("queued upload {}", path));
    }

    fn set_mute(&mut self, muted: bool) {
        if !muted && self.deafened.load(Ordering::Relaxed) {
            self.set_status("deafened; microphone remains muted");
            return;
        }
        self.mic_muted.store(muted, Ordering::Relaxed);
        self.set_status(if muted {
            "microphone muted"
        } else {
            "microphone unmuted"
        });
    }

    fn set_deafen(&mut self, deafened: bool) {
        self.deafened.store(deafened, Ordering::Relaxed);
        if deafened {
            self.mic_muted.store(true, Ordering::Relaxed);
            self.voice_tx_enabled.store(false, Ordering::Relaxed);
            self.stop_mic_capture();
            self.playback.take();
            self.set_status("deafened");
        } else {
            self.set_status("undeafened");
            self.start_room_voice();
        }
    }

    fn show_mute_status(&mut self) {
        self.set_status(if self.deafened.load(Ordering::Relaxed) {
            "deafened; microphone muted"
        } else if self.mic_muted.load(Ordering::Relaxed) {
            "microphone muted"
        } else {
            "microphone unmuted"
        });
    }

    fn show_deafen_status(&mut self) {
        self.set_status(if self.deafened.load(Ordering::Relaxed) {
            "deafened"
        } else {
            "not deafened"
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
        self.network = NetworkClient::spawn(
            self.config.network.client_config(&self.config.files),
            event_tx,
        );
        self.event_rx = event_rx;
    }

    fn ensure_mic_capture(&mut self) -> Result<(), String> {
        if self.capture.is_some() {
            return Ok(());
        }
        if let Some(id) = self.config.audio.input_device_id.as_deref() {
            if !self.devices.is_empty() {
                let Some(item) = self
                    .audio_input_items
                    .iter()
                    .find(|item| item.selection.as_deref() == Some(id))
                else {
                    let error = "selected input device is unavailable".to_string();
                    self.mic_error = Some(error.clone());
                    return Err(error);
                };
                if !item.supported {
                    let error = item
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
        let deafened = Arc::clone(&self.deafened);
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        match audio::start_live_capture(
            LiveCaptureConfig {
                input_device_id: self.config.audio.input_device_id.clone(),
                bitrate_bps: self.config.audio.bitrate_bps,
                denoise: self.config.audio.denoise,
                buffer_request: self.buffer_request(),
            },
            move |payload| {
                if mic_muted.load(Ordering::Relaxed)
                    || deafened.load(Ordering::Relaxed)
                    || !voice_tx_enabled.load(Ordering::Relaxed)
                {
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

    fn start_room_voice(&mut self) {
        if self.deafened.load(Ordering::Relaxed) {
            self.voice_tx_enabled.store(false, Ordering::Relaxed);
            self.stop_mic_capture();
            self.playback.take();
            self.set_status("deafened");
            return;
        }

        self.voice_tx_enabled.store(true, Ordering::Relaxed);
        let mut capture_ok = true;
        if let Err(error) = self.ensure_mic_capture() {
            capture_ok = false;
            self.set_error(format!("failed to start capture: {error}"));
        }
        if self.playback.is_none() {
            match audio::start_live_playback(self.buffer_request()) {
                Ok(playback) => {
                    self.playback = Some(playback);
                    if capture_ok {
                        self.set_status("voice active");
                    }
                }
                Err(error) => {
                    self.playback = None;
                    self.set_error(format!("voice playback unavailable: {error}"));
                }
            }
        }
        self.voice_packets_received = 0;
        self.voice_bytes_received = 0;
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

    match parse_cli_command(&args)? {
        CliCommand::RunUi => {}
        CliCommand::Upload { path } => {
            let path = absolute_upload_path(&path)?;
            let response = local_control::send_upload(&path)?;
            println!("{response}");
            return Ok(());
        }
        CliCommand::DebugAudioInputs => {
            let config_path = config::value_arg(&args, "--config");
            let config = Config::load(config_path.as_deref())?;
            print_debug_audio_inputs(config.audio.buffer.to_request())?;
            return Ok(());
        }
    }

    let config_path = config::value_arg(&args, "--config");
    let mut app = App::new(Config::load(config_path.as_deref())?);
    let control_socket = local_control::ControlSocket::spawn(app.network.sender())?;
    kvlog::info!(
        "tomchat local control socket ready",
        path = %control_socket.path().display()
    );

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
    let _control_socket = control_socket;

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

fn parse_cli_command(args: &[String]) -> Result<CliCommand, String> {
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if arg == "upload" {
            let path = args
                .get(index + 1)
                .ok_or_else(|| "usage: tomchat upload file_path".to_string())?;
            if path.is_empty() {
                return Err("usage: tomchat upload file_path".to_string());
            }
            if args.len() != index + 2 {
                return Err("usage: tomchat upload file_path".to_string());
            }
            return Ok(CliCommand::Upload {
                path: PathBuf::from(path),
            });
        }
        if arg == "debug-audio-inputs" {
            if args.len() != index + 1 {
                return Err("usage: tomchat debug-audio-inputs".to_string());
            }
            return Ok(CliCommand::DebugAudioInputs);
        }

        if cli_option_takes_value(arg) {
            index += 2;
        } else {
            index += 1;
        }
    }
    Ok(CliCommand::RunUi)
}

fn cli_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "--config"
            | "--logfile"
            | "--user"
            | "--token"
            | "--pairing-code"
            | "--server-public-key"
            | "--tcp"
            | "--udp"
            | "--udp-probe"
            | "--receive-dir"
            | "--max-upload-bytes"
            | "--max-receive-bytes"
    )
}

fn absolute_upload_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("usage: tomchat upload file_path".to_string());
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| format!("failed to read current directory: {error}"))
}

fn print_debug_audio_inputs(buffer_request: BufferRequest) -> Result<(), String> {
    let devices = audio::input_devices(buffer_request)?;
    let ranked_items = settings::audio_input_items(&devices);
    let report = jsony::object! {
        buffer_request: buffer_request.label(),
        devices: [
            for (index, device) in devices.iter().enumerate();
            {
                index,
                name: device.name.as_str(),
                supported: device.supported,
                preview: match device.preview.as_ref() {
                    Some(preview) => {
                        channels: preview.channels,
                        sample_format: preview.sample_format.to_string(),
                        buffer_size: format!("{:?}", preview.buffer_size),
                        buffer_note: preview.buffer_note.as_str(),
                    },
                    None => None,
                },
                issue: device.issue.as_deref(),
            }
        ],
        settings_items: [
            for (index, item) in ranked_items.iter().enumerate();
            {
                index,
                selection: item.selection.as_deref(),
                device_index: item.device_index,
                name: item.name.as_str(),
                rank: item.rank,
                search_text: item.search_text.as_str(),
                supported: item.supported,
                variants: [
                    for variant in item.variants.iter();
                    {
                        index: variant.index,
                        rank: variant.rank,
                        supported: variant.supported,
                        preview: match variant.preview.as_ref() {
                            Some(preview) => {
                                channels: preview.channels,
                                sample_format: preview.sample_format.to_string(),
                                buffer_size: format!("{:?}", preview.buffer_size),
                                buffer_note: preview.buffer_note.as_str(),
                            },
                            None => None,
                        },
                        issue: variant.issue.as_deref(),
                    }
                ],
                preview: match item.preview.as_ref() {
                    Some(preview) => {
                        channels: preview.channels,
                        sample_format: preview.sample_format.to_string(),
                        buffer_size: format!("{:?}", preview.buffer_size),
                        buffer_note: preview.buffer_note.as_str(),
                    },
                    None => None,
                },
                issue: item.issue.as_deref(),
            }
        ],
    };
    println!("{report}");
    Ok(())
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
        theme::UiMode::Settings => ui::settings::draw_settings(
            screen,
            buf,
            &app.settings,
            app.settings_focus,
            app.settings_dirty,
            &app.audio_input_items,
            &mut app.audio_input_picker,
        ),
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
        let state =
            if Some(participant.user_id) == app.user_id && app.deafened.load(Ordering::Relaxed) {
                "deaf"
            } else if participant.online && Some(participant.user_id) == app.user_id {
                "voice"
            } else if participant.online && participant.p2p_direct {
                "p2p"
            } else if participant.online {
                "relay"
            } else {
                "away"
            };
        let spoke = participant
            .last_voice_at
            .map(age_label)
            .or_else(|| participant.last_message_ms.map(|_| "msg".to_string()))
            .unwrap_or_else(|| "--".to_string());
        let style = if participant.online {
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
            " ROOM {}  online {}/{}  voice {} ",
            app.room_name,
            app.participants.online_count(),
            app.participants.entries.len(),
            app.participants.online_count()
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
        .with(voice_style(app))
        .fmt(buf, format_args!(" {} ", voice_state_label(app)));
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

fn voice_style(app: &App) -> Style {
    if app.deafened.load(Ordering::Relaxed) {
        theme::WARN
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        theme::GOOD
    } else if app.user_id.is_some() {
        theme::WARN
    } else {
        theme::STATUS_FILL
    }
}

fn mic_status_compact(app: &App, capture: Option<&StatsSnapshot>) -> String {
    let mute = if app.deafened.load(Ordering::Relaxed) {
        "deaf"
    } else if app.mic_muted.load(Ordering::Relaxed) {
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

fn voice_state_label(app: &App) -> &'static str {
    if app.deafened.load(Ordering::Relaxed) {
        "deafened"
    } else if app.voice_tx_enabled.load(Ordering::Relaxed) {
        "voice"
    } else if app.user_id.is_some() {
        "voice"
    } else {
        "offline"
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
        let audio_input_items = settings::audio_input_items(&[]);
        let mut audio_input_picker = AudioInputPickerState::default();
        audio_input_picker.reset(&audio_input_items, None);
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
            audio_input_items,
            audio_input_picker,
            settings_focus: SettingsFocus::Device,
            settings: SettingsDraft::from_audio(&config::AudioConfig::default()),
            settings_dirty: false,
            mic_muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            playback: None,
            voice_packets_received: 0,
            voice_bytes_received: 0,
        }
    }

    #[test]
    fn default_config_parses() {
        let config = Config::default();
        assert_eq!(config.network.user, "alice");
        assert_eq!(config.audio.bitrate_bps, 24_000);
        assert_eq!(config.files.max_upload_bytes, 50 * 1024 * 1024);
        assert_eq!(config.files.max_receive_bytes, 50 * 1024 * 1024);
    }

    #[test]
    fn parses_upload_subcommand_after_value_options() {
        let args = vec![
            "tomchat".to_string(),
            "--config".to_string(),
            "dev.toml".to_string(),
            "--pairing-code".to_string(),
            "one-time-pairing-code".to_string(),
            "--server-public-key".to_string(),
            "de1235b52a8b96f16f91124a8b462d463f2af83756946effa70e842142a6d7cf".to_string(),
            "upload".to_string(),
            "some_file/foo.md".to_string(),
        ];

        assert_eq!(
            parse_cli_command(&args).unwrap(),
            CliCommand::Upload {
                path: PathBuf::from("some_file/foo.md")
            }
        );
    }

    #[test]
    fn upload_subcommand_rejects_extra_args() {
        let args = vec![
            "tomchat".to_string(),
            "upload".to_string(),
            "foo.md".to_string(),
            "bar.md".to_string(),
        ];

        assert!(parse_cli_command(&args).is_err());
    }

    #[test]
    fn parses_debug_audio_inputs_subcommand_after_value_options() {
        let args = vec![
            "tomchat".to_string(),
            "--config".to_string(),
            "dev.toml".to_string(),
            "debug-audio-inputs".to_string(),
        ];

        assert_eq!(
            parse_cli_command(&args).unwrap(),
            CliCommand::DebugAudioInputs
        );
    }

    #[test]
    fn upload_path_is_made_absolute_without_renaming_leaf() {
        let path = absolute_upload_path(Path::new("some_file/foo.md")).unwrap();

        assert!(path.is_absolute());
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("foo.md")
        );
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
    fn opening_settings_does_not_populate_devices() {
        let mut app = test_app();

        app.open_settings();

        assert_eq!(app.mode, theme::UiMode::Settings);
        assert!(app.devices.is_empty());
        assert_eq!(app.audio_input_items.len(), 1);
        assert_eq!(app.audio_input_items[0].selection, None);
    }

    #[test]
    fn deafen_implies_mute_and_blocks_unmute() {
        let mut app = test_app();

        app.set_deafen(true);
        assert!(app.deafened.load(Ordering::Relaxed));
        assert!(app.mic_muted.load(Ordering::Relaxed));
        assert!(!app.voice_tx_enabled.load(Ordering::Relaxed));

        app.set_mute(false);
        assert!(app.mic_muted.load(Ordering::Relaxed));
    }

    #[test]
    fn room_join_while_deafened_does_not_open_audio() {
        let mut app = test_app();
        app.set_deafen(true);

        app.handle_network_event(NetworkEvent::RoomJoined {
            room_id: rpc::ids::RoomId(1),
            history: Vec::new(),
            participants: vec![ParticipantInfo {
                user_id: UserId(1),
                name: "alice".to_string(),
                in_call: true,
            }],
        });

        assert!(app.deafened.load(Ordering::Relaxed));
        assert!(!app.voice_tx_enabled.load(Ordering::Relaxed));
        assert!(app.capture.is_none());
        assert!(app.playback.is_none());
    }

    #[test]
    fn open_audio_input_picker_uses_j_k_and_arrows_for_list_navigation() {
        let mut app = test_app();
        app.mode = theme::UiMode::Settings;
        app.settings_focus = SettingsFocus::Device;
        app.devices = vec![
            DeviceInfo {
                name: "Alpha Microphone".to_string(),
                supported: true,
                preview: None,
                issue: None,
            },
            DeviceInfo {
                name: "Beta Microphone".to_string(),
                supported: true,
                preview: None,
                issue: None,
            },
        ];
        app.rebuild_audio_input_picker();

        assert!(!app.audio_input_picker.open);
        app.activate_audio_input_picker();
        assert!(app.audio_input_picker.open);
        assert_eq!(
            app.audio_input_picker.selector.current_item_index(),
            Some(0)
        );

        app.process_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()));
        assert_eq!(
            app.audio_input_picker.selector.current_item_index(),
            Some(1)
        );

        app.process_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()));
        assert_eq!(
            app.audio_input_picker.selector.current_item_index(),
            Some(0)
        );

        app.process_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(
            app.audio_input_picker.selector.current_item_index(),
            Some(1)
        );

        app.process_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(
            app.audio_input_picker.selector.current_item_index(),
            Some(0)
        );
    }

    #[test]
    fn reconnect_scheduled_sets_down_error_status() {
        let mut app = test_app();

        app.handle_network_event(NetworkEvent::ReconnectScheduled {
            retry_in: Duration::from_secs(5),
        });

        assert_eq!(app.status, "server down; disconnected retrying in 5s");
        assert_eq!(app.status_kind, StatusKind::Error);
        assert!(!app.voice_tx_enabled.load(Ordering::Relaxed));
        assert!(app.playback.is_none());
        assert!(app.capture.is_none());
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
