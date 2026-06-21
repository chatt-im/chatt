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
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use audio::{
    BufferRequest, DeviceInfo, EchoReference, LiveCapture, LiveCaptureConfig, LiveEncoderProfile,
    LivePlayback, LivePlaybackConfig, LivePlaybackFeedback, PlaybackStreamControl, StatsSnapshot,
};
use bindings::{BindCommand, PendingChord, Resolved};
use chat_buffer::VirtualChatBuffer;
use client_net::{NetworkClient, NetworkCommand, NetworkEvent};
use config::{
    Config, MAX_USER_VOLUME_DB, MIN_USER_VOLUME_DB, ServerEntry, USER_VOLUME_DB_STEP,
    snap_user_volume_db,
};
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
use ring::rand::SecureRandom;
use rpc::{
    control::{ChatMessage, InviteTicket, ParticipantInfo},
    crypto::encode_hex,
    ids::{SessionId, StreamId, UserId},
};
use settings::{
    AudioInputPickerState, AudioOutputPickerState, BITRATES, MAX_AMPLIFICATIONS, SettingsDraft,
    SettingsFocus,
};
use tinyhl::{Highlighter, Source};
use unicode_width::UnicodeWidthStr;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const NAME_COL_WIDTH: u16 = 16;
const ROOM_SELECTED: Style = Style::DEFAULT
    .with_bg_rgb(0x24, 0x28, 0x30)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);
const VOLUME_DIALOG: Style = Style::DEFAULT
    .with_bg_rgb(0x18, 0x1b, 0x20)
    .with_fg_rgb(0xd8, 0xdb, 0xd6);
const VOLUME_DIALOG_HEADER: Style = Style::DEFAULT
    .with_bg_rgb(0x35, 0x3b, 0x46)
    .with_fg_rgb(0xf0, 0xf2, 0xe8);

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
    voice_feedback: Option<ParticipantVoiceFeedback>,
}

#[derive(Clone, Copy, Debug)]
struct ParticipantVoiceFeedback {
    loss_percent: u8,
    max_queue_ms: u16,
    max_interarrival_jitter_ms: u16,
    updated_at: Instant,
}

#[derive(Default)]
struct Participants {
    entries: Vec<ParticipantState>,
    scroll: usize,
    selected_user: Option<UserId>,
}

impl Participants {
    fn replace_room(&mut self, participants: Vec<ParticipantInfo>) {
        let selected_user = self.selected_user;
        self.entries.clear();
        for participant in participants {
            self.upsert(participant, true);
        }
        self.sort();
        self.selected_user = selected_user.filter(|user_id| self.contains_user(*user_id));
        self.ensure_selection();
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
                existing.voice_feedback = None;
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
                voice_feedback: None,
            });
        }
        self.sort();
        self.ensure_selection();
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
            entry.voice_feedback = None;
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

    fn voice_feedback(&mut self, feedback: LivePlaybackFeedback) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.active_stream == Some(StreamId(feedback.stream_id)))
        {
            let loss_packets = feedback.lost_packets.saturating_add(feedback.late_packets);
            let loss_percent = if feedback.expected_packets == 0 {
                0
            } else {
                ((u32::from(loss_packets) * 100) / u32::from(feedback.expected_packets)).min(100)
                    as u8
            };
            entry.voice_feedback = Some(ParticipantVoiceFeedback {
                loss_percent,
                max_queue_ms: feedback.max_queue_ms,
                max_interarrival_jitter_ms: feedback.max_interarrival_jitter_ms,
                updated_at: Instant::now(),
            });
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
            voice_feedback: None,
        });
        if self.selected_user.is_none() {
            self.selected_user = Some(user_id);
        }
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

    fn contains_user(&self, user_id: UserId) -> bool {
        self.entries.iter().any(|entry| entry.user_id == user_id)
    }

    fn ensure_selection(&mut self) {
        if self
            .selected_user
            .is_some_and(|user_id| self.contains_user(user_id))
        {
            return;
        }
        self.selected_user = self.entries.first().map(|entry| entry.user_id);
    }

    fn selected_index(&self) -> Option<usize> {
        let selected_user = self.selected_user?;
        self.entries
            .iter()
            .position(|entry| entry.user_id == selected_user)
    }

    fn selected(&self) -> Option<&ParticipantState> {
        let selected_user = self.selected_user?;
        self.entries
            .iter()
            .find(|entry| entry.user_id == selected_user)
    }

    fn move_selection(&mut self, delta: isize) -> Option<UserId> {
        if self.entries.is_empty() {
            self.selected_user = None;
            self.scroll = 0;
            return None;
        }
        let current = self.selected_index().unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.entries.len() as isize) as usize;
        let user_id = self.entries[next].user_id;
        self.selected_user = Some(user_id);
        Some(user_id)
    }

    fn keep_selected_visible(&mut self, visible_rows: usize) {
        let Some(index) = self.selected_index() else {
            self.scroll = self.scroll.min(self.entries.len().saturating_sub(1));
            return;
        };
        let visible_rows = visible_rows.max(1);
        if index < self.scroll {
            self.scroll = index;
        } else if index >= self.scroll.saturating_add(visible_rows) {
            self.scroll = index.saturating_add(1).saturating_sub(visible_rows);
        }
        self.scroll = self
            .scroll
            .min(self.entries.len().saturating_sub(visible_rows));
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
    server_alias: String,
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
    audio_device_refresh_tx: mpsc::Sender<AudioDeviceRefresh>,
    audio_device_refresh_rx: Receiver<AudioDeviceRefresh>,
    audio_device_refresh_in_flight: bool,
    next_audio_device_refresh_id: u64,
    network: NetworkClient,
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    input_devices: Vec<DeviceInfo>,
    output_devices: Vec<DeviceInfo>,
    audio_input_items: Vec<settings::AudioInputItem>,
    audio_output_items: Vec<settings::AudioOutputItem>,
    audio_input_picker: AudioInputPickerState,
    audio_output_picker: AudioOutputPickerState,
    settings_focus: SettingsFocus,
    settings: SettingsDraft,
    settings_dirty: bool,
    mic_muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    voice_tx_enabled: Arc<AtomicBool>,
    mic_error: Option<String>,
    capture: Option<LiveCapture>,
    settings_preview_capture: bool,
    allow_settings_preview_capture: bool,
    playback: Option<LivePlayback>,
    echo_reference: Option<Arc<EchoReference>>,
    muted_users: HashSet<UserId>,
    stream_users: HashMap<u32, UserId>,
    volume_dialog: Option<UserVolumeDialog>,
    voice_packets_received: u64,
    voice_bytes_received: u64,
    encoder_profile: LiveEncoderProfile,
    last_network_notice: Option<String>,
    save_config_after_auth: bool,
}

struct UserVolumeDialog {
    user_id: UserId,
    user_name: String,
    original_db: f32,
    value_db: f32,
    editor: Editor,
    error: Option<String>,
}

impl UserVolumeDialog {
    fn new(user_id: UserId, user_name: String, value_db: f32) -> Self {
        let mut editor = volume_input_editor(value_db);
        editor.enter_insert_mode();
        Self {
            user_id,
            user_name,
            original_db: value_db,
            value_db,
            editor,
            error: None,
        }
    }

    fn adjust(&mut self, delta_steps: isize) {
        let next = self.value_db + delta_steps as f32 * USER_VOLUME_DB_STEP;
        self.value_db = snap_user_volume_db(next);
        self.editor
            .set_lines(&format_volume_db_value(self.value_db));
        self.editor.enter_insert_mode();
        self.error = None;
    }

    fn parse_editor_value(&self) -> Result<f32, String> {
        parse_user_volume_db(&self.editor.text())
    }

    fn apply_editor_value(&mut self) -> Result<f32, String> {
        let value = self.parse_editor_value()?;
        self.value_db = value;
        self.error = None;
        Ok(value)
    }
}

struct AudioDeviceRefresh {
    id: u64,
    buffer_request: BufferRequest,
    restart_preview: bool,
    input: Result<Vec<DeviceInfo>, String>,
    output: Result<Vec<DeviceInfo>, String>,
}

enum Action {
    Continue,
    Quit,
}

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    RunUi,
    Join { join_string: String },
    Upload { path: PathBuf },
    DebugAudioInputs,
}

impl App {
    fn new(
        config: Config,
        pairing_code: Option<String>,
        save_config_after_auth: bool,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::channel();
        let (audio_device_refresh_tx, audio_device_refresh_rx) = mpsc::channel();
        let active_server = config.active_server()?.clone();
        let client_config = active_server.client_config(&config.files, pairing_code);
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
        let audio_output_items = settings::audio_output_items(&[]);
        let mut audio_input_picker = AudioInputPickerState::default();
        audio_input_picker.reset(
            &audio_input_items,
            settings_draft.input_device_id.as_deref(),
        );
        let mut audio_output_picker = AudioOutputPickerState::default();
        audio_output_picker.reset(
            &audio_output_items,
            settings_draft.output_device_id.as_deref(),
        );
        let echo_reference = config
            .audio
            .echo_cancellation
            .then(|| Arc::new(EchoReference::new()));
        Ok(Self {
            server_alias: active_server.alias.clone(),
            user: active_server.effective_display_name(),
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
            audio_device_refresh_tx,
            audio_device_refresh_rx,
            audio_device_refresh_in_flight: false,
            next_audio_device_refresh_id: 0,
            network,
            session_id: None,
            user_id: None,
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            audio_input_items,
            audio_output_items,
            audio_input_picker,
            audio_output_picker,
            settings_focus: SettingsFocus::InputDevice,
            settings: settings_draft,
            settings_dirty: false,
            mic_muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            settings_preview_capture: false,
            allow_settings_preview_capture: true,
            playback: None,
            echo_reference,
            muted_users: HashSet::new(),
            stream_users: HashMap::new(),
            volume_dialog: None,
            voice_packets_received: 0,
            voice_bytes_received: 0,
            encoder_profile: LiveEncoderProfile::DRED_20,
            last_network_notice: None,
            save_config_after_auth,
            config,
        })
    }

    fn drain_network_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_network_event(event);
        }
    }

    fn drain_audio_device_refreshes(&mut self) {
        while let Ok(refresh) = self.audio_device_refresh_rx.try_recv() {
            self.handle_audio_device_refresh(refresh);
        }
    }

    fn handle_audio_device_refresh(&mut self, refresh: AudioDeviceRefresh) {
        if refresh.id + 1 != self.next_audio_device_refresh_id {
            return;
        }
        self.audio_device_refresh_in_flight = false;

        let mut input_count = None;
        let mut output_count = None;
        let mut errors = Vec::new();

        match refresh.input {
            Ok(devices) => {
                input_count = Some(devices.len());
                self.input_devices = devices;
            }
            Err(error) => {
                self.input_devices.clear();
                self.settings.input_device_id = None;
                self.mic_error = Some(error.clone());
                errors.push(format!("input devices: {error}"));
            }
        }

        match refresh.output {
            Ok(devices) => {
                output_count = Some(devices.len());
                self.output_devices = devices;
            }
            Err(error) => {
                self.output_devices.clear();
                self.settings.output_device_id = None;
                errors.push(format!("output devices: {error}"));
            }
        }

        self.rebuild_audio_device_pickers();

        if self.mode == theme::UiMode::Settings {
            if errors.is_empty() {
                self.set_status(format!(
                    "found {} input device(s), {} output device(s) ({})",
                    input_count.unwrap_or(0),
                    output_count.unwrap_or(0),
                    refresh.buffer_request.label()
                ));
            } else {
                self.set_error(format!("failed to refresh {}", errors.join("; ")));
            }
        }

        if refresh.restart_preview
            && self.mode == theme::UiMode::Settings
            && !self.voice_tx_enabled.load(Ordering::Relaxed)
            && !self.deafened.load(Ordering::Relaxed)
        {
            self.start_settings_preview_capture();
        }
    }

    fn handle_network_event(&mut self, event: NetworkEvent) {
        kvlog::info!("app network event", kind = network_event_kind(&event));
        match event {
            NetworkEvent::Connected => {
                self.last_network_notice = None;
                self.set_status("connected; authenticating");
            }
            NetworkEvent::Authenticated {
                session_id,
                user_id,
                rooms,
            } => {
                self.session_id = Some(session_id);
                self.user_id = Some(user_id);
                self.last_network_notice = None;
                if let Some(room) = rooms.first() {
                    self.room_name = room.name.clone();
                }
                if self.save_config_after_auth {
                    match self.config.save_runtime() {
                        Ok(path) => {
                            self.config.config_path = Some(path.clone());
                            self.save_config_after_auth = false;
                            self.set_status(format!(
                                "joined {}; config saved to {}",
                                self.server_alias,
                                path.display()
                            ));
                        }
                        Err(error) => self.set_error(error),
                    }
                } else {
                    self.set_status(format!("authenticated as {}", self.user));
                }
            }
            NetworkEvent::RoomJoined {
                room_id,
                history,
                participants,
            } => {
                self.chat.clear();
                self.stream_users.clear();
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
                self.stream_users.insert(stream_id.0, user_id);
                self.participants.voice_started(user_id, stream_id);
                self.apply_user_audio_control(user_id);
                if Some(user_id) == self.user_id {
                    self.set_status("voice stream ready");
                } else {
                    self.set_status(format!("user {} voice ready", user_id.0));
                }
            }
            NetworkEvent::VoiceStopped { user_id, stream_id } => {
                self.participants.voice_stopped(user_id, stream_id);
                self.stream_users.remove(&stream_id.0);
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
            NetworkEvent::PlaybackFeedback(feedback) => {
                self.participants.voice_feedback(feedback);
            }
            NetworkEvent::EncoderProfileChanged(profile) => {
                self.encoder_profile = profile;
                if let Some(capture) = &self.capture {
                    capture.set_encoder_profile(profile);
                }
            }
            NetworkEvent::Status(status) => self.set_status(status),
            NetworkEvent::Error(error) => {
                kvlog::warn!("app network error", error = error.as_str());
                self.set_error(format!("error: {error}"));
            }
            NetworkEvent::AuthFailed(error) => {
                kvlog::warn!("app auth failed", error = error.as_str());
                self.stop_audio();
                self.stream_users.clear();
                self.push_network_notice("auth", &error);
                self.set_error(auth_failure_status(&error));
            }
            NetworkEvent::ReconnectScheduled { retry_in, reason } => {
                self.stop_audio();
                self.stream_users.clear();
                self.push_network_notice("network", &format!("Connection failed: {reason}"));
                self.set_error(format!(
                    "connection failed; retrying in {}s",
                    retry_in.as_secs()
                ));
            }
            NetworkEvent::Disconnected => {
                self.stop_audio();
                self.stream_users.clear();
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

    fn push_network_notice(&mut self, sender: &str, body: &str) {
        if self.last_network_notice.as_deref() == Some(body) {
            return;
        }
        self.last_network_notice = Some(body.to_string());
        self.chat.push_notice(sender, body);
        self.chat.bottom();
    }

    fn process_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }

        if self.handle_volume_dialog_key(key) {
            return Action::Continue;
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
        if self.mode != theme::UiMode::Settings {
            return false;
        }

        match self.settings_focus {
            SettingsFocus::InputDevice if self.audio_input_picker.open => {
                handle_audio_picker_search_key(
                    key,
                    &mut self.audio_input_picker,
                    &self.audio_input_items,
                )
            }
            SettingsFocus::OutputDevice if self.audio_output_picker.open => {
                handle_audio_picker_search_key(
                    key,
                    &mut self.audio_output_picker,
                    &self.audio_output_items,
                )
            }
            _ => false,
        }
    }

    fn cancel_open_audio_picker(&mut self) -> bool {
        if self.audio_input_picker.open {
            self.cancel_audio_input_picker();
            true
        } else if self.audio_output_picker.open {
            self.cancel_audio_output_picker();
            true
        } else {
            false
        }
    }

    fn audio_picker_open(&self) -> bool {
        self.audio_input_picker.open || self.audio_output_picker.open
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
                    if !self.cancel_open_audio_picker() {
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
            RoomScrollUp => self.move_room_selection(-1),
            RoomScrollDown => self.move_room_selection(1),
            OpenSelectedUserVolume => self.open_selected_user_volume(),
            ToggleSelectedUserMute => self.toggle_selected_user_mute(),
            HalfPageUp => self.chat.scroll_up(10),
            HalfPageDown => self.chat.scroll_down(10),
            Top => self.chat.top(self.last_chat_width),
            Bottom => self.chat.bottom(),
            ToggleMute => self.set_mute(!self.mic_muted.load(Ordering::Relaxed)),
            ToggleDeafen => self.set_deafen(!self.deafened.load(Ordering::Relaxed)),
            RefreshDevices => self.refresh_audio_devices(),
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
        self.settings_focus = SettingsFocus::InputDevice;
        self.settings_dirty = false;
        if self.allow_settings_preview_capture
            && (self.input_devices.is_empty() || self.output_devices.is_empty())
            && self.capture.is_none()
            && self.playback.is_none()
        {
            self.refresh_audio_devices();
        }
        self.rebuild_audio_device_pickers();
        self.start_settings_preview_capture();
    }

    fn close_settings(&mut self) {
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        self.stop_settings_preview_capture();
        self.audio_input_picker.reset(
            &self.audio_input_items,
            self.settings.input_device_id.as_deref(),
        );
        self.audio_output_picker.reset(
            &self.audio_output_items,
            self.settings.output_device_id.as_deref(),
        );
        self.mode = theme::UiMode::Compose;
    }

    fn move_settings_focus(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        if self.audio_picker_open() {
            self.move_active_audio_picker_selection(delta);
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
            SettingsFocus::InputDevice if delta < 0 => self.cancel_audio_input_picker(),
            SettingsFocus::InputDevice => self.activate_audio_input_picker(),
            SettingsFocus::OutputDevice if delta < 0 => self.cancel_audio_output_picker(),
            SettingsFocus::OutputDevice => self.activate_audio_output_picker(),
            SettingsFocus::Bitrate => {
                self.settings.bitrate_index =
                    cycle_index(self.settings.bitrate_index, BITRATES.len(), delta);
                self.mark_settings_dirty();
            }
            SettingsFocus::Denoise => {
                self.settings.denoise = !self.settings.denoise;
                self.mark_settings_dirty();
            }
            SettingsFocus::Amplification => {
                self.settings.amplification_index = cycle_index(
                    self.settings.amplification_index,
                    MAX_AMPLIFICATIONS.len(),
                    delta,
                );
                self.apply_active_capture_amplification(self.settings.max_amplification());
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
            SettingsFocus::Refresh => self.refresh_audio_devices(),
            SettingsFocus::Save => self.save_settings(),
            SettingsFocus::Close => self.close_settings(),
            SettingsFocus::InputDevice => self.activate_audio_input_picker(),
            SettingsFocus::OutputDevice => self.activate_audio_output_picker(),
            SettingsFocus::Bitrate | SettingsFocus::Amplification | SettingsFocus::Buffer => {
                self.adjust_settings_focus(1);
            }
        }
    }

    fn move_settings_selection(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        if self.audio_picker_open() {
            self.move_active_audio_picker_selection(delta);
        } else {
            self.move_settings_focus(delta);
        }
    }

    fn move_active_audio_picker_selection(&mut self, delta: isize) {
        match self.settings_focus {
            SettingsFocus::InputDevice if self.audio_input_picker.open => {
                self.audio_input_picker.move_selection(delta);
            }
            SettingsFocus::OutputDevice if self.audio_output_picker.open => {
                self.audio_output_picker.move_selection(delta);
            }
            _ => {}
        }
    }

    fn activate_audio_input_picker(&mut self) {
        if self.audio_input_picker.open {
            self.confirm_audio_input_picker();
        } else {
            if self.input_devices.is_empty() {
                self.refresh_audio_devices();
            }
            self.audio_input_picker.open(
                &self.audio_input_items,
                self.settings.input_device_id.as_deref(),
            );
        }
    }

    fn activate_audio_output_picker(&mut self) {
        if self.audio_output_picker.open {
            self.confirm_audio_output_picker();
        } else {
            if self.output_devices.is_empty() {
                self.refresh_audio_devices();
            }
            self.audio_output_picker.open(
                &self.audio_output_items,
                self.settings.output_device_id.as_deref(),
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

    fn confirm_audio_output_picker(&mut self) {
        let Some(next) = self.audio_output_picker.confirm(&self.audio_output_items) else {
            return;
        };
        if self.settings.output_device_id != next {
            self.settings.output_device_id = next;
            self.mark_settings_dirty();
        }
    }

    fn cancel_audio_output_picker(&mut self) {
        if let Some(selection) = self.audio_output_picker.cancel(&self.audio_output_items) {
            self.settings.output_device_id = selection;
        }
    }

    fn rebuild_audio_device_pickers(&mut self) {
        self.audio_input_items = settings::audio_input_items(&self.input_devices);
        if self.audio_input_picker.open {
            self.audio_input_picker.refresh_items(
                &self.audio_input_items,
                self.settings.input_device_id.as_deref(),
            );
        } else {
            self.audio_input_picker.reset(
                &self.audio_input_items,
                self.settings.input_device_id.as_deref(),
            );
        }
        self.audio_output_items = settings::audio_output_items(&self.output_devices);
        if self.audio_output_picker.open {
            self.audio_output_picker.refresh_items(
                &self.audio_output_items,
                self.settings.output_device_id.as_deref(),
            );
        } else {
            self.audio_output_picker.reset(
                &self.audio_output_items,
                self.settings.output_device_id.as_deref(),
            );
        }
    }

    fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.set_status("settings draft changed; save config when ready");
    }

    fn move_room_selection(&mut self, delta: isize) {
        if self.participants.move_selection(delta).is_none() {
            self.set_status("no users in the current room yet");
            return;
        }
        self.keep_selected_room_user_visible();
    }

    fn keep_selected_room_user_visible(&mut self) {
        self.participants
            .keep_selected_visible(self.config.ui.room_height as usize);
    }

    fn selected_room_user(&self) -> Option<(UserId, String)> {
        self.participants
            .selected()
            .map(|entry| (entry.user_id, entry.name.clone()))
    }

    fn selected_remote_room_user(&mut self) -> Option<(UserId, String)> {
        let Some((user_id, name)) = self.selected_room_user() else {
            self.set_status("select a user first");
            return None;
        };
        if Some(user_id) == self.user_id {
            self.set_status("select another user for local playback controls");
            return None;
        }
        Some((user_id, name))
    }

    fn open_selected_user_volume(&mut self) {
        let Some((user_id, name)) = self.selected_remote_room_user() else {
            return;
        };
        let value_db = self.config.user_volume_db(&self.server_alias, user_id.0);
        self.volume_dialog = Some(UserVolumeDialog::new(user_id, name.clone(), value_db));
        self.set_status(format!("adjusting local volume for {name}"));
    }

    fn toggle_selected_user_mute(&mut self) {
        let Some((user_id, name)) = self.selected_remote_room_user() else {
            return;
        };
        let muted = if self.muted_users.contains(&user_id) {
            self.muted_users.remove(&user_id);
            false
        } else {
            self.muted_users.insert(user_id);
            true
        };
        self.apply_user_audio_control(user_id);
        self.set_status(format!(
            "{} {name} locally",
            if muted { "muted" } else { "unmuted" }
        ));
    }

    fn handle_volume_dialog_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut dialog) = self.volume_dialog.take() else {
            return false;
        };
        if matches!(key.kind, KeyEventKind::Release) {
            self.volume_dialog = Some(dialog);
            return true;
        }

        match key.code {
            KeyCode::Esc => {
                self.config.set_user_volume_db(
                    &self.server_alias,
                    dialog.user_id.0,
                    dialog.original_db,
                );
                self.apply_user_audio_control_with_volume(dialog.user_id, dialog.original_db);
                self.set_status(format!("canceled local volume for {}", dialog.user_name));
            }
            KeyCode::Enter => match dialog.parse_editor_value() {
                Ok(value_db) => {
                    dialog.value_db = value_db;
                    self.config
                        .set_user_volume_db(&self.server_alias, dialog.user_id.0, value_db);
                    self.apply_user_audio_control_with_volume(dialog.user_id, value_db);
                    match self.config.save_runtime() {
                        Ok(path) => {
                            self.config.config_path = Some(path.clone());
                            self.set_status(format!(
                                "saved local volume {}dB for {} to {}",
                                format_signed_db(value_db),
                                dialog.user_name,
                                path.display()
                            ));
                        }
                        Err(error) => {
                            dialog.error = Some(error.clone());
                            self.volume_dialog = Some(dialog);
                            self.set_error(error);
                        }
                    }
                }
                Err(error) => {
                    dialog.error = Some(error.clone());
                    self.volume_dialog = Some(dialog);
                    self.set_error(error);
                }
            },
            KeyCode::Left | KeyCode::Down => {
                dialog.adjust(-1);
                self.apply_user_audio_control_with_volume(dialog.user_id, dialog.value_db);
                self.volume_dialog = Some(dialog);
            }
            KeyCode::Right | KeyCode::Up => {
                dialog.adjust(1);
                self.apply_user_audio_control_with_volume(dialog.user_id, dialog.value_db);
                self.volume_dialog = Some(dialog);
            }
            _ if dialog.editor.send_key(&key) => {
                match dialog.apply_editor_value() {
                    Ok(value_db) => {
                        self.apply_user_audio_control_with_volume(dialog.user_id, value_db);
                    }
                    Err(error) => {
                        dialog.error = Some(error);
                    }
                }
                self.volume_dialog = Some(dialog);
            }
            _ => {
                self.volume_dialog = Some(dialog);
            }
        }
        true
    }

    fn effective_user_volume_db(&self, user_id: UserId) -> f32 {
        if let Some(dialog) = &self.volume_dialog
            && dialog.user_id == user_id
        {
            return dialog.value_db;
        }
        self.config.user_volume_db(&self.server_alias, user_id.0)
    }

    fn user_playback_control_with_volume(
        &self,
        user_id: UserId,
        volume_db: f32,
    ) -> PlaybackStreamControl {
        PlaybackStreamControl {
            muted: self.muted_users.contains(&user_id),
            volume_db,
        }
    }

    fn user_playback_control(&self, user_id: UserId) -> PlaybackStreamControl {
        self.user_playback_control_with_volume(user_id, self.effective_user_volume_db(user_id))
    }

    fn apply_user_audio_control(&self, user_id: UserId) {
        let control = self.user_playback_control(user_id);
        self.apply_user_audio_control_inner(user_id, control);
    }

    fn apply_user_audio_control_with_volume(&self, user_id: UserId, volume_db: f32) {
        let control = self.user_playback_control_with_volume(user_id, volume_db);
        self.apply_user_audio_control_inner(user_id, control);
    }

    fn apply_user_audio_control_inner(&self, user_id: UserId, control: PlaybackStreamControl) {
        let Some(playback) = &self.playback else {
            return;
        };
        for stream_id in self
            .stream_users
            .iter()
            .filter_map(|(stream_id, stream_user)| (*stream_user == user_id).then_some(*stream_id))
        {
            playback.set_stream_control(stream_id, control);
        }
    }

    fn apply_all_user_audio_controls(&self) {
        let users = self
            .stream_users
            .values()
            .copied()
            .collect::<HashSet<UserId>>();
        for user_id in users {
            self.apply_user_audio_control(user_id);
        }
    }

    fn save_settings(&mut self) {
        let restart_preview =
            self.settings_preview_capture && !self.voice_tx_enabled.load(Ordering::Relaxed);
        self.config.audio = self.settings.to_audio();
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.settings_dirty = false;
                if restart_preview {
                    self.stop_mic_capture();
                    self.start_settings_preview_capture();
                }
                if (self.capture.is_some() && !self.settings_preview_capture)
                    || self.playback.is_some()
                {
                    self.set_status(format!(
                        "settings saved to {}; live amplification updated, other audio applies after deafen/rejoin",
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

    fn refresh_audio_devices(&mut self) {
        if self.audio_device_refresh_in_flight {
            self.set_status("refreshing audio devices");
            return;
        }

        let restart_preview =
            self.settings_preview_capture && !self.voice_tx_enabled.load(Ordering::Relaxed);
        if restart_preview {
            self.stop_mic_capture();
        }
        if self.capture.is_some() || self.playback.is_some() {
            self.set_error("deafen before refreshing devices");
            return;
        }

        let id = self.next_audio_device_refresh_id;
        self.next_audio_device_refresh_id = self.next_audio_device_refresh_id.saturating_add(1);
        self.audio_device_refresh_in_flight = true;
        let buffer_request = self.settings.buffer_request();
        let tx = self.audio_device_refresh_tx.clone();
        thread::spawn(move || {
            let input = audio::input_devices(buffer_request);
            let output = audio::output_devices(buffer_request);
            let _ = tx.send(AudioDeviceRefresh {
                id,
                buffer_request,
                restart_preview,
                input,
                output,
            });
        });
        self.set_status("refreshing audio devices");
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
            "/audio" => self.show_audio_status(),
            "/clear" => self.chat.clear(),
            "/config" | "/settings" => self.open_settings(),
            "/users" => self.show_users(),
            "/whoami" => self.show_current_user(),
            command if command.starts_with("/upload ") => self.upload_file_command(command),
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

    fn show_audio_status(&mut self) {
        let Some(playback) = &self.playback else {
            self.set_status("audio inactive");
            return;
        };
        let stats = playback.stats();
        let played_samples = stats.direct_samples.saturating_add(stats.resampled_samples);
        let direct_percent = if played_samples == 0 {
            0
        } else {
            stats.direct_samples.saturating_mul(100) / played_samples
        };
        self.set_status(format!(
            "audio q{}ms target{}ms speed{:+.1}% enc{} direct{}% skip{}ms/{} rs{} dred{} plc{} trims{} underruns{} rx {}/{}",
            stats.max_queue_ms,
            stats.adaptive_target_ms,
            stats.correction_percent,
            self.encoder_profile.label(),
            direct_percent,
            stats.skipped_silence_ms,
            stats.silence_skip_count,
            stats.resampler_activations,
            stats.dred_recoveries,
            stats.plc_fallbacks,
            stats.hard_trim_count,
            stats.underrun_count,
            self.voice_packets_received,
            format_bytes_compact(self.voice_bytes_received),
        ));
    }

    fn show_users(&mut self) {
        if self.participants.entries.is_empty() {
            self.set_status("no users in the current room yet");
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
            Some(user_id) => format!(
                "signed in as {} on {} (user {})",
                self.user, self.server_alias, user_id.0
            ),
            None => format!("connecting as {} on {}", self.user, self.server_alias),
        });
    }

    fn ensure_mic_capture(&mut self) -> Result<(), String> {
        if self.capture.is_some() {
            return Ok(());
        }
        if let Some(id) = self.config.audio.input_device_id.as_deref() {
            if !self.input_devices.is_empty() {
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
                max_amplification: self.config.audio.max_amplification,
                buffer_request: self.buffer_request(),
                tuning: self.config.audio.latency.to_tuning(),
                echo_reference: self.echo_reference.clone(),
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

    fn apply_active_capture_amplification(&self, max_amplification: f32) {
        if let Some(capture) = &self.capture {
            capture.set_max_amplification(max_amplification);
        }
    }

    fn start_settings_preview_capture(&mut self) {
        if !self.allow_settings_preview_capture
            || self.capture.is_some()
            || self.voice_tx_enabled.load(Ordering::Relaxed)
            || self.deafened.load(Ordering::Relaxed)
        {
            return;
        }

        match self.ensure_mic_capture() {
            Ok(()) => {
                self.settings_preview_capture = true;
            }
            Err(error) => {
                self.mic_error = Some(error);
            }
        }
    }

    fn stop_settings_preview_capture(&mut self) {
        if self.settings_preview_capture && !self.voice_tx_enabled.load(Ordering::Relaxed) {
            self.stop_mic_capture();
        }
        self.settings_preview_capture = false;
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
        } else {
            self.settings_preview_capture = false;
        }
        if self.playback.is_none() {
            let (feedback_tx, feedback_rx) = mpsc::channel::<LivePlaybackFeedback>();
            let network_tx = self.network.sender();
            thread::spawn(move || {
                for feedback in feedback_rx {
                    let _ = network_tx.send(NetworkCommand::PlaybackFeedback(feedback));
                }
            });
            match audio::start_live_playback(LivePlaybackConfig {
                output_device_id: self.config.audio.output_device_id.clone(),
                buffer_request: self.buffer_request(),
                tuning: self.config.audio.latency.to_tuning(),
                feedback_sender: Some(feedback_tx),
                echo_reference: self.echo_reference.clone(),
            }) {
                Ok(playback) => {
                    self.playback = Some(playback);
                    self.apply_all_user_audio_controls();
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
        let restart_settings_preview = self.mode == theme::UiMode::Settings
            && self.allow_settings_preview_capture
            && !self.deafened.load(Ordering::Relaxed);
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.stop_mic_capture();
        self.playback.take();
        if restart_settings_preview {
            self.start_settings_preview_capture();
        }
    }

    fn stop_mic_capture(&mut self) {
        self.settings_preview_capture = false;
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

fn handle_audio_picker_search_key(
    key: KeyEvent,
    picker: &mut settings::AudioDevicePickerState,
    items: &[settings::AudioDeviceItem],
) -> bool {
    if picker.searching {
        return picker.edit_search(key, items);
    }

    if matches!(key.kind, KeyEventKind::Release) {
        return false;
    }
    let mut modifiers = key.modifiers;
    modifiers.remove(KeyModifiers::SHIFT);
    if modifiers.is_empty() && key.code == KeyCode::Char('/') {
        picker.start_search(items);
        return true;
    }

    false
}

fn volume_input_editor(value_db: f32) -> Editor {
    let mut editor = Editor::new();
    editor.set_single_line(true);
    editor.set_wrap(false);
    editor.set_height_bounds(1, 1);
    editor.set_theme(theme::join_input_editor_theme());
    editor.set_lines(&format_volume_db_value(value_db));
    editor.enter_insert_mode();
    editor
}

fn parse_user_volume_db(value: &str) -> Result<f32, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("volume dB value is empty".to_string());
    }
    let parsed = value
        .parse::<f32>()
        .map_err(|_| "volume dB value must be a number".to_string())?;
    if !(MIN_USER_VOLUME_DB..=MAX_USER_VOLUME_DB).contains(&parsed) {
        return Err(format!(
            "volume dB value must be between {:.1} and {:.1}",
            MIN_USER_VOLUME_DB, MAX_USER_VOLUME_DB
        ));
    }
    Ok(snap_user_volume_db(parsed))
}

fn format_volume_db_value(value_db: f32) -> String {
    format!("{value_db:.1}")
}

fn format_signed_db(value_db: f32) -> String {
    if value_db > 0.0 {
        format!("+{value_db:.1}")
    } else {
        format!("{value_db:.1}")
    }
}

fn volume_db_label(value_db: f32) -> String {
    format!("{}dB", format_signed_db(value_db))
}

fn format_bytes_compact(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
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
        config::value_arg(&args, "--logfile").or_else(|| std::env::var("CHATT_LOGFILE").ok());
    let _logger = if let Some(logfile) = logfile {
        kvlog::collector::init_file_logger(&logfile)
    } else {
        kvlog::collector::init_closure_logger(|buf| buf.clear())
    };

    match parse_cli_command(&args)? {
        CliCommand::RunUi => {}
        CliCommand::Join { join_string } => {
            let config_path = config::value_arg(&args, "--config");
            let ticket = rpc::control::decode_invite_ticket(&join_string)?;
            let (config, pairing_code) = run_join_setup(ticket, config_path.as_deref())?;
            return run_app(config, Some(pairing_code), true);
        }
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
    run_app(Config::load(config_path.as_deref())?, None, false)
}

fn run_app(
    config: Config,
    pairing_code: Option<String>,
    save_config_after_auth: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut app = App::new(config, pairing_code, save_config_after_auth)?;
    let control_socket = local_control::ControlSocket::spawn(app.network.sender())?;
    kvlog::info!(
        "chatt local control socket ready",
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
        app.drain_audio_device_refreshes();
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
                .ok_or_else(|| "usage: chatt upload file_path".to_string())?;
            if path.is_empty() {
                return Err("usage: chatt upload file_path".to_string());
            }
            if args.len() != index + 2 {
                return Err("usage: chatt upload file_path".to_string());
            }
            return Ok(CliCommand::Upload {
                path: PathBuf::from(path),
            });
        }
        if arg == "join" {
            let join_string = args
                .get(index + 1)
                .ok_or_else(|| "usage: chatt join JOIN_STRING".to_string())?;
            if join_string.is_empty() {
                return Err("usage: chatt join JOIN_STRING".to_string());
            }
            if args.len() != index + 2 {
                return Err("usage: chatt join JOIN_STRING".to_string());
            }
            return Ok(CliCommand::Join {
                join_string: join_string.clone(),
            });
        }
        if arg == "debug-audio-inputs" {
            if args.len() != index + 1 {
                return Err("usage: chatt debug-audio-inputs".to_string());
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
    matches!(arg, "--config" | "--logfile")
}

fn absolute_upload_path(path: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("usage: chatt upload file_path".to_string());
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinFocus {
    Alias,
    DisplayName,
    Confirm,
}

struct JoinDraft {
    ticket: InviteTicket,
    alias: Editor,
    display_name: Editor,
    focus: JoinFocus,
    status: String,
    status_kind: StatusKind,
}

impl JoinDraft {
    fn new(ticket: InviteTicket) -> Self {
        let mut alias = join_input_editor(&default_join_alias(&ticket));
        let mut display_name = join_input_editor(&title_case_ascii(&ticket.user));
        alias.enter_insert_mode();
        display_name.enter_insert_mode();
        Self {
            ticket,
            alias,
            display_name,
            focus: JoinFocus::Alias,
            status: "review invite".to_string(),
            status_kind: StatusKind::Info,
        }
    }

    fn move_focus(&mut self, delta: isize) {
        const ORDER: [JoinFocus; 3] =
            [JoinFocus::Alias, JoinFocus::DisplayName, JoinFocus::Confirm];
        let index = ORDER
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = (index as isize + delta).rem_euclid(ORDER.len() as isize) as usize;
        self.focus = ORDER[next];
    }

    fn set_error(&mut self, error: impl Into<String>) {
        self.status = error.into();
        self.status_kind = StatusKind::Error;
    }

    fn focused_editor_mut(&mut self) -> Option<&mut Editor> {
        match self.focus {
            JoinFocus::Alias => Some(&mut self.alias),
            JoinFocus::DisplayName => Some(&mut self.display_name),
            JoinFocus::Confirm => None,
        }
    }
}

fn join_input_editor(value: &str) -> Editor {
    let mut editor = Editor::new();
    editor.set_single_line(true);
    editor.set_wrap(false);
    editor.set_height_bounds(1, 1);
    editor.set_theme(theme::join_input_editor_theme());
    editor.set_lines(value);
    editor.enter_insert_mode();
    editor
}

fn run_join_setup(
    ticket: InviteTicket,
    config_path: Option<&str>,
) -> Result<(Config, String), Box<dyn std::error::Error>> {
    let mut draft = JoinDraft::new(ticket);
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

    loop {
        render_join(&mut draft, &mut buffer);
        buffer.render(&mut terminal);

        if event::poll(&stdin, Some(POLL_INTERVAL))?.is_readable() {
            events.read_from(&stdin)?;
        }

        while let Some(event) = events.next(terminal.is_raw()) {
            match event {
                Event::Key(key) => match process_join_key(&mut draft, key, config_path) {
                    JoinAction::Continue => {}
                    JoinAction::Cancel => return Err("join canceled".into()),
                    JoinAction::Done(config, pairing_code) => return Ok((config, pairing_code)),
                },
                Event::Resized => {
                    let (new_w, new_h) = terminal.size()?;
                    buffer.resize(new_w, new_h);
                }
                _ => {}
            }
        }
    }
}

enum JoinAction {
    Continue,
    Cancel,
    Done(Config, String),
}

fn process_join_key(draft: &mut JoinDraft, key: KeyEvent, config_path: Option<&str>) -> JoinAction {
    if matches!(key.kind, KeyEventKind::Release) {
        return JoinAction::Continue;
    }
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return JoinAction::Cancel;
    }
    match key.code {
        KeyCode::Esc => JoinAction::Cancel,
        KeyCode::Tab | KeyCode::Down => {
            draft.move_focus(1);
            if let Some(editor) = draft.focused_editor_mut() {
                editor.enter_insert_mode();
            }
            JoinAction::Continue
        }
        KeyCode::BackTab | KeyCode::Up => {
            draft.move_focus(-1);
            if let Some(editor) = draft.focused_editor_mut() {
                editor.enter_insert_mode();
            }
            JoinAction::Continue
        }
        KeyCode::Enter if draft.focus == JoinFocus::Confirm => confirm_join(draft, config_path),
        KeyCode::Enter => {
            draft.move_focus(1);
            if let Some(editor) = draft.focused_editor_mut() {
                editor.enter_insert_mode();
            }
            JoinAction::Continue
        }
        _ if draft
            .focused_editor_mut()
            .is_some_and(|editor| editor.send_key(&key)) =>
        {
            JoinAction::Continue
        }
        _ => JoinAction::Continue,
    }
}

fn confirm_join(draft: &mut JoinDraft, config_path: Option<&str>) -> JoinAction {
    let alias = draft.alias.text().trim().to_string();
    let display_name = draft.display_name.text().trim().to_string();
    if let Err(error) = config::validate_server_alias(&alias) {
        draft.set_error(error);
        draft.focus = JoinFocus::Alias;
        return JoinAction::Continue;
    }
    if let Err(error) = config::validate_display_name(&display_name) {
        draft.set_error(error);
        draft.focus = JoinFocus::DisplayName;
        return JoinAction::Continue;
    }

    let token = match random_token() {
        Ok(token) => token,
        Err(error) => {
            draft.set_error(error);
            return JoinAction::Continue;
        }
    };
    let server = match server_entry_from_invite(&draft.ticket, alias.clone(), display_name, token) {
        Ok(server) => server,
        Err(error) => {
            draft.set_error(error);
            return JoinAction::Continue;
        }
    };
    let mut config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => {
            draft.set_error(error);
            return JoinAction::Continue;
        }
    };
    let pairing_code = draft.ticket.pairing_code.clone();
    config.upsert_server(server);
    config.set_active_server(alias);
    JoinAction::Done(config, pairing_code)
}

fn server_entry_from_invite(
    ticket: &InviteTicket,
    alias: String,
    display_name: String,
    token: String,
) -> Result<ServerEntry, String> {
    Ok(ServerEntry {
        alias,
        tcp_addr: ticket.tcp_addr.clone(),
        udp_addr: ticket.udp_addr.clone(),
        udp_probe_addr: ticket.udp_probe_addr.clone(),
        user: ticket.user.clone(),
        display_name,
        token,
        server_public_key: ticket.server_public_key.clone(),
        room_id: ticket.room_id,
    })
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "failed to generate pairing token".to_string())?;
    Ok(encode_hex(&bytes))
}

fn default_join_alias(ticket: &InviteTicket) -> String {
    let host = if let Ok(addr) = ticket.tcp_addr.parse::<std::net::SocketAddr>() {
        if addr.ip().is_loopback() {
            return "local".to_string();
        }
        addr.ip().to_string()
    } else {
        ticket
            .tcp_addr
            .rsplit_once(':')
            .map(|(host, _)| host.trim_matches(['[', ']']).to_string())
            .unwrap_or_else(|| "server".to_string())
    };
    if host == "localhost" {
        return "local".to_string();
    }
    let mut alias = String::from("server");
    for ch in host.chars() {
        if ch.is_ascii_alphanumeric() {
            alias.push(ch.to_ascii_lowercase());
        } else if !alias.ends_with('-') {
            alias.push('-');
        }
    }
    while alias.ends_with('-') {
        alias.pop();
    }
    alias
}

fn title_case_ascii(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut word_start = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if word_start {
                out.push(ch.to_ascii_uppercase());
                word_start = false;
            } else {
                out.push(ch);
            }
        } else {
            out.push(' ');
            word_start = true;
        }
    }
    let out = out.trim().to_string();
    if out.is_empty() {
        value.to_string()
    } else {
        out
    }
}

fn render_join(draft: &mut JoinDraft, buf: &mut Buffer) {
    buf.rect().with(theme::BACKGROUND).fill(buf);
    buf.hide_cursor();
    let mut area = buf.rect();
    let status = area.take_bottom(1);
    draw_join_status(status, draft, buf);
    let mut body = area;
    body.take_top(1)
        .with(theme::STATUS_SECTION | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, " JOIN SERVER ");
    body.take_top(1).with(theme::BACKGROUND).fill(buf);
    draw_join_detail(body.take_top(1), buf, "Server", &draft.ticket.tcp_addr);
    draw_join_detail(body.take_top(1), buf, "UDP", &draft.ticket.udp_addr);
    draw_join_detail(
        body.take_top(1),
        buf,
        "Probe",
        draft.ticket.udp_probe_addr.as_deref().unwrap_or("disabled"),
    );
    draw_join_detail(body.take_top(1), buf, "User", &draft.ticket.user);
    draw_join_detail(
        body.take_top(1),
        buf,
        "Room",
        &draft.ticket.room_id.to_string(),
    );
    draw_join_detail(
        body.take_top(1),
        buf,
        "Key",
        &short_key(&draft.ticket.server_public_key),
    );
    body.take_top(1).with(theme::BACKGROUND).fill(buf);
    draw_join_field(
        body.take_top(1),
        buf,
        "Alias",
        &mut draft.alias,
        draft.focus == JoinFocus::Alias,
    );
    draw_join_field(
        body.take_top(1),
        buf,
        "Display",
        &mut draft.display_name,
        draft.focus == JoinFocus::DisplayName,
    );
    body.take_top(1).with(theme::BACKGROUND).fill(buf);
    draw_join_button(
        body.take_top(1),
        buf,
        "Join server",
        draft.focus == JoinFocus::Confirm,
    );
}

fn draw_join_detail(area: Rect, buf: &mut Buffer, label: &str, value: &str) {
    if area.is_empty() {
        return;
    }
    let mut row = area;
    row.take_left(12)
        .with(theme::BACKGROUND.patch(theme::MUTED))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(theme::BACKGROUND.patch(theme::TEXT))
        .with(Ellipsis(true))
        .text(buf, value);
}

fn draw_join_field(area: Rect, buf: &mut Buffer, label: &str, editor: &mut Editor, focused: bool) {
    if area.is_empty() {
        return;
    }
    let label_style = if focused {
        theme::BACKGROUND.patch(theme::GOOD)
    } else {
        theme::BACKGROUND.patch(theme::MUTED)
    };
    let boundary = if focused {
        theme::JOIN_INPUT_BOUNDARY_ACTIVE
    } else {
        theme::JOIN_INPUT_BOUNDARY_INACTIVE
    };
    let input = if focused {
        theme::JOIN_INPUT_ACTIVE
    } else {
        theme::JOIN_INPUT_INACTIVE
    };
    area.with(theme::BACKGROUND).fill(buf);
    let mut row = area;
    row.take_left(12)
        .with(label_style)
        .with(Ellipsis(true))
        .text(buf, label);
    if row.is_empty() {
        return;
    }
    row.with(boundary).fill(buf);
    row.take_left(1).with(boundary).text(buf, " ");
    row.take_right(1).with(boundary).text(buf, " ");
    row.with(input).fill(buf);
    if focused {
        editor.render(row, buf);
    } else {
        row.with(input)
            .with(Ellipsis(true))
            .text(buf, &editor.text());
    }
}

fn draw_join_button(area: Rect, buf: &mut Buffer, label: &str, focused: bool) {
    if area.is_empty() {
        return;
    }
    let style = if focused {
        Style::DEFAULT
            .with_bg_rgb(0x35, 0x3b, 0x46)
            .with_fg_rgb(0xf0, 0xf2, 0xe8)
    } else {
        theme::BACKGROUND.patch(theme::TEXT)
    };
    area.with(style)
        .with(HAlign::Center)
        .text(buf, &format!(" {label} "));
}

fn draw_join_status(area: Rect, draft: &JoinDraft, buf: &mut Buffer) {
    let style = match draft.status_kind {
        StatusKind::Info => theme::STATUS_FILL.patch(theme::MUTED),
        StatusKind::Error => theme::STATUS_FILL.patch(theme::ERROR),
    };
    area.with(theme::STATUS_FILL).fill(buf);
    area.with(style)
        .with(Ellipsis(true))
        .text(buf, &format!(" {}", draft.status));
}

fn short_key(value: &str) -> String {
    if value.len() <= 18 {
        value.to_string()
    } else {
        format!("{}...", &value[..18])
    }
}

fn render(app: &mut App, buf: &mut Buffer) {
    buf.rect().with(theme::BACKGROUND).fill(buf);
    buf.hide_cursor();
    let capture = app
        .capture
        .as_ref()
        .map(|capture| capture.stats().snapshot());

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
            capture.as_ref(),
            &app.audio_input_items,
            &mut app.audio_input_picker,
            &app.audio_output_items,
            &mut app.audio_output_picker,
        ),
        theme::UiMode::Compose | theme::UiMode::Log => draw_chat(screen, app, buf),
    }
    draw_status(status_area, app, buf, capture.as_ref());
    draw_composer(composer_area, app, buf);
    draw_volume_dialog(buf.rect(), app, buf);
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
        let selected = Some(participant.user_id) == app.participants.selected_user;
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
        let base = if selected {
            ROOM_SELECTED
        } else {
            theme::PANEL_ALT
        };
        let style = if selected {
            base.patch(theme::GOOD)
        } else if participant.online {
            theme::TEXT
        } else {
            theme::MUTED
        };
        let marker = if selected { ">" } else { " " };
        let control = room_user_control_label(app, participant);
        let voice = room_user_voice_feedback_label(participant);
        row.with(base).fill(buf);
        row.with(style).with(Ellipsis(true)).text(
            buf,
            &format!(
                "{marker} {:<16} {:<7} {:<5} {:<16} {}",
                participant.name, state, spoke, voice, control
            ),
        );
    }
}

fn room_user_voice_feedback_label(participant: &ParticipantState) -> String {
    let Some(feedback) = participant.voice_feedback else {
        return String::new();
    };
    if !participant.voice_active || feedback.updated_at.elapsed() > Duration::from_secs(10) {
        return String::new();
    }
    format!(
        "loss{} q{} j{}",
        feedback.loss_percent, feedback.max_queue_ms, feedback.max_interarrival_jitter_ms
    )
}

fn room_user_control_label(app: &App, participant: &ParticipantState) -> String {
    if Some(participant.user_id) == app.user_id {
        return String::new();
    }
    let muted = app.muted_users.contains(&participant.user_id);
    let volume_db = app.effective_user_volume_db(participant.user_id);
    match (muted, volume_db == 0.0) {
        (false, true) => String::new(),
        (false, false) => volume_db_label(volume_db),
        (true, true) => "muted".to_string(),
        (true, false) => format!("muted {}", volume_db_label(volume_db)),
    }
}

fn draw_volume_dialog(area: Rect, app: &mut App, buf: &mut Buffer) {
    let Some(dialog) = app.volume_dialog.as_mut() else {
        return;
    };
    if area.w < 24 || area.h < 6 {
        return;
    }

    let width = area.w.min(58);
    let height = area.h.min(7);
    let panel = Rect {
        x: area.x + area.w.saturating_sub(width) / 2,
        y: area.y + area.h.saturating_sub(height) / 2,
        w: width,
        h: height,
    };
    buf.clear_rect(panel, VOLUME_DIALOG);

    let mut rows = panel;
    rows.take_top(1)
        .with(VOLUME_DIALOG_HEADER | Modifier::BOLD)
        .with(Ellipsis(true))
        .text(buf, &format!(" Local volume: {} ", dialog.user_name));

    let mut body = rows.inset(2, 0);
    body.take_top(1)
        .with(VOLUME_DIALOG.patch(theme::MUTED))
        .with(Ellipsis(true))
        .text(
            buf,
            &format!(
                "User {}  saved {}",
                dialog.user_id.0,
                volume_db_label(dialog.original_db)
            ),
        );

    let mut slider_row = body.take_top(1);
    let label = volume_db_label(dialog.value_db);
    let label_width = label.width() as u16 + 1;
    let slider_width = slider_row.w.saturating_sub(label_width).max(8);
    slider_row
        .take_left(slider_width as i32)
        .with(VOLUME_DIALOG.patch(theme::GOOD))
        .with(Ellipsis(true))
        .text(buf, &volume_slider(dialog.value_db, slider_width));
    slider_row
        .with(VOLUME_DIALOG.patch(theme::TEXT))
        .with(Ellipsis(true))
        .text(buf, &format!(" {label}"));

    let mut input_row = body.take_top(1);
    input_row
        .take_left(8)
        .with(VOLUME_DIALOG.patch(theme::MUTED))
        .text(buf, "Offset");
    let field_width = input_row.w.min(14);
    let mut field = input_row.take_left(field_width as i32);
    field.with(theme::JOIN_INPUT_BOUNDARY_ACTIVE).fill(buf);
    if field.w > 2 {
        field
            .take_left(1)
            .with(theme::JOIN_INPUT_BOUNDARY_ACTIVE)
            .text(buf, " ");
        field
            .take_right(1)
            .with(theme::JOIN_INPUT_BOUNDARY_ACTIVE)
            .text(buf, " ");
    }
    field.with(theme::JOIN_INPUT_ACTIVE).fill(buf);
    dialog.editor.render(field, buf);
    input_row
        .with(VOLUME_DIALOG.patch(theme::MUTED))
        .with(Ellipsis(true))
        .text(buf, " dB");

    let footer = body.take_top(1);
    if let Some(error) = &dialog.error {
        footer
            .with(VOLUME_DIALOG.patch(theme::ERROR))
            .with(Ellipsis(true))
            .text(buf, error);
    } else {
        footer
            .with(VOLUME_DIALOG.patch(theme::SUBTLE))
            .with(Ellipsis(true))
            .text(
                buf,
                &format!("Pending {}", volume_db_label(dialog.value_db)),
            );
    }
}

fn volume_slider(value_db: f32, width: u16) -> String {
    let inner = width.saturating_sub(2).max(1) as usize;
    let span = MAX_USER_VOLUME_DB - MIN_USER_VOLUME_DB;
    let value_ratio = ((value_db - MIN_USER_VOLUME_DB) / span).clamp(0.0, 1.0);
    let zero_ratio = ((0.0 - MIN_USER_VOLUME_DB) / span).clamp(0.0, 1.0);
    let value_index = (value_ratio * inner.saturating_sub(1) as f32).round() as usize;
    let zero_index = (zero_ratio * inner.saturating_sub(1) as f32).round() as usize;

    let mut out = String::with_capacity(inner + 2);
    out.push('[');
    for index in 0..inner {
        if index == value_index {
            out.push('|');
        } else if index == zero_index {
            out.push('0');
        } else if index < value_index {
            out.push('=');
        } else {
            out.push('-');
        }
    }
    out.push(']');
    out
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

fn draw_status(area: Rect, app: &App, buf: &mut Buffer, capture: Option<&StatsSnapshot>) {
    area.with(theme::STATUS_FILL).fill(buf);
    let mut row = area;
    draw_status_segment(
        &mut row,
        buf,
        theme::mode_style(app.mode),
        &format!(" {} ", app.mode.label()),
    );
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_SECTION,
        &format!(" {} ", app.room_name),
    );
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_FILL,
        &format!(" {} ", app.user),
    );
    draw_status_segment(
        &mut row,
        buf,
        voice_style(app),
        &format!(" {} ", voice_state_label(app)),
    );
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_FILL,
        &format!(" {} ", mic_status_compact(app, capture)),
    );
    draw_status_segment(&mut row, buf, theme::STATUS_FILL, " ");
    let meter_width = row.w.min(12);
    if meter_width > 0 {
        let meter = row.take_left(meter_width as i32);
        ui::vu::draw_status_vu(meter, buf, capture);
    }
    draw_status_segment(&mut row, buf, theme::STATUS_FILL, " ");
    draw_status_segment(
        &mut row,
        buf,
        theme::STATUS_FILL.patch(theme::SUBTLE),
        &format!(
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

fn draw_status_segment(row: &mut Rect, buf: &mut Buffer, style: Style, text: &str) {
    if row.is_empty() {
        return;
    }
    let width = UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16;
    row.take_left(width as i32)
        .with(style)
        .with(Ellipsis(true))
        .text(buf, text);
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
    } else if app.settings_preview_capture && !app.voice_tx_enabled.load(Ordering::Relaxed) {
        "preview"
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
        NetworkEvent::PlaybackFeedback(_) => "playback_feedback",
        NetworkEvent::EncoderProfileChanged(_) => "encoder_profile_changed",
        NetworkEvent::Status(_) => "status",
        NetworkEvent::Error(_) => "error",
        NetworkEvent::AuthFailed(_) => "auth_failed",
        NetworkEvent::ReconnectScheduled { .. } => "reconnect_scheduled",
        NetworkEvent::Disconnected => "disconnected",
    }
}

fn auth_failure_status(detail: &str) -> &'static str {
    if detail.starts_with("pairing failed") {
        "pairing failed; see chat"
    } else if detail.starts_with("authentication failed") {
        "authentication failed; see chat"
    } else {
        "server rejected login; see chat"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        let (_event_tx, event_rx) = mpsc::channel();
        let (audio_device_refresh_tx, audio_device_refresh_rx) = mpsc::channel();
        let (command_tx, _command_rx) = mpsc::channel();
        let mut composer = Editor::new();
        composer.set_theme(theme::editor_theme());
        composer.set_wrap(true);
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);
        let audio_input_items = settings::audio_input_items(&[]);
        let mut audio_input_picker = AudioInputPickerState::default();
        audio_input_picker.reset(&audio_input_items, None);
        let audio_output_items = settings::audio_output_items(&[]);
        let mut audio_output_picker = AudioOutputPickerState::default();
        audio_output_picker.reset(&audio_output_items, None);
        App {
            config: Config::default(),
            server_alias: "local".to_string(),
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
            audio_device_refresh_tx,
            audio_device_refresh_rx,
            audio_device_refresh_in_flight: false,
            next_audio_device_refresh_id: 0,
            network: NetworkClient::from_parts_for_test(command_tx),
            session_id: Some(SessionId(1)),
            user_id: Some(UserId(1)),
            input_devices: Vec::new(),
            output_devices: Vec::new(),
            audio_input_items,
            audio_output_items,
            audio_input_picker,
            audio_output_picker,
            settings_focus: SettingsFocus::InputDevice,
            settings: SettingsDraft::from_audio(&config::AudioConfig::default()),
            settings_dirty: false,
            mic_muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            capture: None,
            settings_preview_capture: false,
            allow_settings_preview_capture: false,
            playback: None,
            echo_reference: None,
            muted_users: HashSet::new(),
            stream_users: HashMap::new(),
            volume_dialog: None,
            voice_packets_received: 0,
            voice_bytes_received: 0,
            encoder_profile: LiveEncoderProfile::DRED_20,
            last_network_notice: None,
            save_config_after_auth: false,
        }
    }

    #[test]
    fn default_config_parses() {
        let config = Config::default();
        let server = config.active_server().unwrap();
        assert_eq!(server.alias, "local");
        assert_eq!(server.user, "alice");
        assert_eq!(server.display_name, "Alice");
        assert_eq!(config.audio.input_device_id, None);
        assert_eq!(config.audio.output_device_id, None);
        assert_eq!(config.audio.bitrate_bps, 48_000);
        assert_eq!(config.audio.max_amplification, 2.0);
        assert_eq!(config.files.max_upload_bytes, 50 * 1024 * 1024);
        assert_eq!(config.files.max_receive_bytes, 50 * 1024 * 1024);
    }

    #[test]
    fn parses_upload_subcommand_after_value_options() {
        let args = vec![
            "chatt".to_string(),
            "--config".to_string(),
            "dev.toml".to_string(),
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
    fn parses_join_subcommand_after_value_options() {
        let args = vec![
            "chatt".to_string(),
            "--config".to_string(),
            "dev.toml".to_string(),
            "join".to_string(),
            "tcj1_deadbeef".to_string(),
        ];

        assert_eq!(
            parse_cli_command(&args).unwrap(),
            CliCommand::Join {
                join_string: "tcj1_deadbeef".to_string()
            }
        );
    }

    #[test]
    fn upload_subcommand_rejects_extra_args() {
        let args = vec![
            "chatt".to_string(),
            "upload".to_string(),
            "foo.md".to_string(),
            "bar.md".to_string(),
        ];

        assert!(parse_cli_command(&args).is_err());
    }

    #[test]
    fn parses_debug_audio_inputs_subcommand_after_value_options() {
        let args = vec![
            "chatt".to_string(),
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
    fn participant_selection_moves_and_survives_sorting() {
        let mut participants = Participants::default();
        participants.replace_room(vec![
            ParticipantInfo {
                user_id: UserId(1),
                name: "alice".to_string(),
                in_call: false,
            },
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: false,
            },
        ]);

        assert_eq!(participants.selected_user, Some(UserId(1)));
        assert_eq!(participants.move_selection(1), Some(UserId(2)));
        participants.keep_selected_visible(1);
        assert_eq!(participants.scroll, 1);

        participants.upsert(
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: true,
            },
            true,
        );

        assert_eq!(participants.selected_user, Some(UserId(2)));
        assert_eq!(participants.selected_index(), Some(0));
    }

    #[test]
    fn selected_user_mute_is_session_only() {
        let mut app = test_app();
        app.participants.replace_room(vec![
            ParticipantInfo {
                user_id: UserId(1),
                name: "alice".to_string(),
                in_call: true,
            },
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: true,
            },
        ]);
        app.move_room_selection(1);

        app.toggle_selected_user_mute();
        assert!(app.muted_users.contains(&UserId(2)));
        assert!(app.config.user_audio.is_empty());

        app.toggle_selected_user_mute();
        assert!(!app.muted_users.contains(&UserId(2)));
        assert!(app.config.user_audio.is_empty());
    }

    #[test]
    fn volume_dialog_parses_decimal_values() {
        assert_eq!(parse_user_volume_db("-5.5").unwrap(), -5.5);
        assert_eq!(parse_user_volume_db("-5.4").unwrap(), -5.5);
        assert!(parse_user_volume_db("-25").is_err());
        assert!(parse_user_volume_db("loud").is_err());
    }

    #[test]
    fn volume_dialog_saves_persisted_user_offset() {
        let mut app = test_app();
        let path = std::env::temp_dir().join(format!(
            "chatt-user-volume-dialog-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        app.config.config_path = Some(path.clone());
        app.participants.replace_room(vec![
            ParticipantInfo {
                user_id: UserId(1),
                name: "alice".to_string(),
                in_call: true,
            },
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: true,
            },
        ]);
        app.move_room_selection(1);
        app.open_selected_user_volume();
        app.volume_dialog
            .as_mut()
            .expect("dialog")
            .editor
            .set_lines("-5.5");

        assert!(app.handle_volume_dialog_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())));

        assert!(app.volume_dialog.is_none());
        assert_eq!(app.config.user_volume_db("local", 2), -5.5);
        let content = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(path);
        assert!(content.contains("[[user-audio]]"));
        assert!(content.contains("user-id = 2"));
        assert!(content.contains("volume-db = -5.5"));
    }

    #[test]
    fn volume_dialog_cancel_restores_original_offset() {
        let mut app = test_app();
        app.config.set_user_volume_db("local", 2, -2.0);
        app.participants.replace_room(vec![
            ParticipantInfo {
                user_id: UserId(1),
                name: "alice".to_string(),
                in_call: true,
            },
            ParticipantInfo {
                user_id: UserId(2),
                name: "bob".to_string(),
                in_call: true,
            },
        ]);
        app.move_room_selection(1);
        app.open_selected_user_volume();

        assert!(app.handle_volume_dialog_key(KeyEvent::new(KeyCode::Right, KeyModifiers::empty())));
        assert_eq!(app.volume_dialog.as_ref().unwrap().value_db, -1.5);
        assert!(app.handle_volume_dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())));

        assert!(app.volume_dialog.is_none());
        assert_eq!(app.config.user_volume_db("local", 2), -2.0);
    }

    #[test]
    fn renders_smoke_frame() {
        let mut app = test_app();
        let mut buffer = Buffer::new(80, 24);
        render(&mut app, &mut buffer);
    }

    #[test]
    fn opening_settings_focuses_input_without_hardware_refresh_in_tests() {
        let mut app = test_app();
        app.settings_focus = SettingsFocus::Save;

        app.open_settings();

        assert_eq!(app.mode, theme::UiMode::Settings);
        assert_eq!(app.settings_focus, SettingsFocus::InputDevice);
        assert!(!app.audio_input_picker.open);
        assert!(!app.audio_output_picker.open);
        assert!(app.input_devices.is_empty());
        assert!(app.output_devices.is_empty());
        assert_eq!(app.audio_input_items.len(), 1);
        assert_eq!(app.audio_input_items[0].selection, None);
        assert_eq!(app.audio_output_items.len(), 1);
        assert_eq!(app.audio_output_items[0].selection, None);
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
        app.settings_focus = SettingsFocus::InputDevice;
        app.input_devices = vec![
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
        app.rebuild_audio_device_pickers();

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
    fn open_audio_output_picker_uses_j_k_and_arrows_for_list_navigation() {
        let mut app = test_app();
        app.mode = theme::UiMode::Settings;
        app.settings_focus = SettingsFocus::OutputDevice;
        app.output_devices = vec![
            DeviceInfo {
                name: "Alpha Speaker".to_string(),
                supported: true,
                preview: None,
                issue: None,
            },
            DeviceInfo {
                name: "Beta Speaker".to_string(),
                supported: true,
                preview: None,
                issue: None,
            },
        ];
        app.rebuild_audio_device_pickers();

        assert!(!app.audio_output_picker.open);
        app.activate_audio_output_picker();
        assert!(app.audio_output_picker.open);
        assert_eq!(
            app.audio_output_picker.selector.current_item_index(),
            Some(0)
        );

        app.process_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()));
        assert_eq!(
            app.audio_output_picker.selector.current_item_index(),
            Some(1)
        );

        app.process_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()));
        assert_eq!(
            app.audio_output_picker.selector.current_item_index(),
            Some(0)
        );

        app.process_key(KeyEvent::new(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(
            app.audio_output_picker.selector.current_item_index(),
            Some(1)
        );

        app.process_key(KeyEvent::new(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(
            app.audio_output_picker.selector.current_item_index(),
            Some(0)
        );
    }

    #[test]
    fn async_audio_refresh_populates_open_output_picker_without_closing() {
        let mut app = test_app();
        app.mode = theme::UiMode::Settings;
        app.settings_focus = SettingsFocus::OutputDevice;
        app.activate_audio_output_picker();
        assert!(app.audio_output_picker.open);
        assert_eq!(app.audio_output_items.len(), 1);

        app.audio_device_refresh_in_flight = true;
        app.next_audio_device_refresh_id = 1;
        app.handle_audio_device_refresh(AudioDeviceRefresh {
            id: 0,
            buffer_request: BufferRequest::Default,
            restart_preview: false,
            input: Ok(Vec::new()),
            output: Ok(vec![DeviceInfo {
                name: "USB Speakers".to_string(),
                supported: true,
                preview: None,
                issue: None,
            }]),
        });

        assert!(!app.audio_device_refresh_in_flight);
        assert!(app.audio_output_picker.open);
        assert_eq!(app.audio_output_items.len(), 2);
        assert!(
            app.audio_output_items
                .iter()
                .any(|item| item.name == "USB Speakers")
        );
    }

    #[test]
    fn reconnect_scheduled_sets_down_error_status() {
        let mut app = test_app();

        app.handle_network_event(NetworkEvent::ReconnectScheduled {
            retry_in: Duration::from_secs(5),
            reason: "failed to connect to server: connection refused".to_string(),
        });

        assert_eq!(app.status, "connection failed; retrying in 5s");
        assert_eq!(app.status_kind, StatusKind::Error);
        assert_eq!(app.chat.len(), 1);
        assert_eq!(app.chat.message(0).sender, "network");
        assert_eq!(
            app.chat.message(0).body,
            "Connection failed: failed to connect to server: connection refused"
        );
        assert!(!app.voice_tx_enabled.load(Ordering::Relaxed));
        assert!(app.playback.is_none());
        assert!(app.capture.is_none());
    }

    #[test]
    fn auth_failed_sets_specific_error_status() {
        let mut app = test_app();

        app.handle_network_event(NetworkEvent::AuthFailed(
            "pairing failed for 'billy': no active invite exists on this server".to_string(),
        ));

        assert_eq!(app.status, "pairing failed; see chat");
        assert_eq!(app.status_kind, StatusKind::Error);
        assert_eq!(app.chat.len(), 1);
        assert_eq!(app.chat.message(0).sender, "auth");
        assert_eq!(
            app.chat.message(0).body,
            "pairing failed for 'billy': no active invite exists on this server"
        );
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
