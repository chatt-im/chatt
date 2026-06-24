pub(crate) mod dialogs;
pub(crate) mod participants;
pub(crate) mod server;

use hashbrown::{HashMap, HashSet};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use extui_editor::{Editor, bindings as editor_bindings};
use rpc::{
    control::{ChatMessage, InviteTicket},
    ids::{SessionId, UserId},
};

use crate::{
    bindings::{BindCommand, PendingChord},
    chat_buffer::VirtualChatBuffer,
    client_net::{NetworkClient, NetworkCommand, NetworkEvent, spawn_pair_once},
    config::{
        self, Config, MAX_USER_VOLUME_DB, MIN_USER_VOLUME_DB, SoundboardClip, snap_user_volume_db,
        validate_server_entry,
    },
    local_control,
    settings::{
        self, AudioInputPickerState, AudioOutputPickerState, BITRATES, MAX_AMPLIFICATIONS,
        SettingsDraft, SettingsFocus,
    },
    theme,
    tui::{
        Action,
        editor::EditorHighlighter,
        focus::{FocusId, FocusManager, ServerField, SettingsField},
        modes::{ModeKind, ModeStack},
    },
    ui::select::FuzzySelect,
};

use chatt::audio::{
    self, BufferRequest, DeviceInfo, EchoCancellationControl, LiveAudioFileSourceConfig,
    LiveAudioFileSourceReport, LiveAudioPacketLossProfile, LiveCapture, LiveCaptureConfig,
    LiveEncoderProfile, LivePlayback, LivePlaybackConfig, LivePlaybackFeedback, LivePlaybackSink,
    PlaybackStreamControl,
};

pub(crate) use dialogs::UserVolumeDialog;
pub(crate) use participants::{ParticipantState, Participants};
pub(crate) use server::{
    PendingPair, ServerEditDraft, ServerEditFocus, ServerSelectItem, default_join_alias,
    random_token, server_entry_from_invite, title_case_ascii, unique_server_alias,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusKind {
    Info,
    Error,
}

pub(crate) struct App {
    pub(crate) config: Config,
    pub(crate) event_tx: Sender<NetworkEvent>,
    pub(crate) server_alias: String,
    pub(crate) user: String,
    pub(crate) room_name: String,
    pub(crate) status: String,
    pub(crate) status_kind: StatusKind,
    pub(crate) mode: theme::UiMode,
    pub(crate) focus: FocusManager,
    pub(crate) modes: ModeStack,
    pub(crate) composer: Editor,
    pub(crate) composer_hl: EditorHighlighter,
    pub(crate) chat: VirtualChatBuffer,
    pub(crate) participants: Participants,
    pub(crate) last_chat_width: u16,
    pub(crate) pending_chord: Option<PendingChord>,
    pub(crate) event_rx: Receiver<NetworkEvent>,
    pub(crate) audio_device_refresh_tx: mpsc::Sender<AudioDeviceRefresh>,
    pub(crate) audio_device_refresh_rx: Receiver<AudioDeviceRefresh>,
    pub(crate) audio_device_refresh_in_flight: bool,
    pub(crate) next_audio_device_refresh_id: u64,
    pub(crate) network: Option<NetworkClient>,
    pub(crate) control_socket: Option<local_control::ControlSocket>,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) user_id: Option<UserId>,
    pub(crate) server_items: Vec<ServerSelectItem>,
    pub(crate) server_select: FuzzySelect,
    pub(crate) server_select_searching: bool,
    pub(crate) server_edit: Option<ServerEditDraft>,
    pub(crate) pending_pair: Option<PendingPair>,
    pub(crate) input_devices: Vec<DeviceInfo>,
    pub(crate) output_devices: Vec<DeviceInfo>,
    pub(crate) audio_input_items: Vec<settings::AudioInputItem>,
    pub(crate) audio_output_items: Vec<settings::AudioOutputItem>,
    pub(crate) audio_input_picker: AudioInputPickerState,
    pub(crate) audio_output_picker: AudioOutputPickerState,
    pub(crate) settings_focus: SettingsFocus,
    pub(crate) settings: SettingsDraft,
    pub(crate) settings_dirty: bool,
    pub(crate) mic_muted: Arc<AtomicBool>,
    pub(crate) deafened: Arc<AtomicBool>,
    pub(crate) voice_tx_enabled: Arc<AtomicBool>,
    pub(crate) mic_error: Option<String>,
    pub(crate) playback_error: Option<String>,
    pub(crate) capture: Option<LiveCapture>,
    pub(crate) settings_preview_capture: bool,
    pub(crate) allow_settings_preview_capture: bool,
    pub(crate) playback: Option<LivePlayback>,
    pub(crate) soundboard_event_tx: mpsc::Sender<SoundboardEvent>,
    pub(crate) soundboard_event_rx: Receiver<SoundboardEvent>,
    pub(crate) soundboard_busy: Arc<AtomicBool>,
    pub(crate) soundboard_next_sequence: u32,
    pub(crate) echo_control: Arc<EchoCancellationControl>,
    pub(crate) muted_users: HashSet<UserId>,
    pub(crate) stream_users: HashMap<u32, UserId>,
    pub(crate) volume_dialog: Option<UserVolumeDialog>,
    pub(crate) voice_packets_received: u64,
    pub(crate) voice_bytes_received: u64,
    pub(crate) encoder_profile: LiveEncoderProfile,
    pub(crate) last_network_notice: Option<String>,
}
pub(crate) struct AudioDeviceRefresh {
    pub(crate) id: u64,
    pub(crate) input_buffer_request: BufferRequest,
    pub(crate) output_buffer_request: BufferRequest,
    pub(crate) restart_preview: bool,
    pub(crate) input: Result<Vec<DeviceInfo>, String>,
    pub(crate) output: Result<Vec<DeviceInfo>, String>,
}

pub(crate) struct SoundboardEvent {
    pub(crate) clip_name: String,
    pub(crate) result: Result<LiveAudioFileSourceReport, String>,
}

impl App {
    pub(crate) fn new(
        config: Config,
        pending_invite: Option<InviteTicket>,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::channel();
        let (audio_device_refresh_tx, audio_device_refresh_rx) = mpsc::channel();
        let (soundboard_event_tx, soundboard_event_rx) = mpsc::channel();
        let soundboard_enabled = config.soundboard.enabled;
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
        let echo_control = Arc::new(EchoCancellationControl::new(config.audio.echo_cancellation));
        let mut app = Self {
            event_tx,
            server_alias: String::new(),
            user: String::new(),
            room_name: "servers".to_string(),
            status: "select a server".to_string(),
            status_kind: StatusKind::Info,
            mode: theme::UiMode::ServerSelect,
            focus: FocusManager::new(FocusId::ServerList),
            modes: ModeStack::new(ModeKind::ServerSelect),
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
            network: None,
            control_socket: None,
            session_id: None,
            user_id: None,
            server_items: Vec::new(),
            server_select: FuzzySelect::default(),
            server_select_searching: false,
            server_edit: None,
            pending_pair: None,
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
            playback_error: None,
            capture: None,
            settings_preview_capture: false,
            allow_settings_preview_capture: !soundboard_enabled,
            playback: None,
            soundboard_event_tx,
            soundboard_event_rx,
            soundboard_busy: Arc::new(AtomicBool::new(false)),
            soundboard_next_sequence: 0,
            echo_control,
            muted_users: HashSet::new(),
            stream_users: HashMap::new(),
            volume_dialog: None,
            voice_packets_received: 0,
            voice_bytes_received: 0,
            encoder_profile: LiveEncoderProfile::DRED_20,
            last_network_notice: None,
            config,
        };
        app.rebuild_server_items();
        if let Some(ticket) = pending_invite {
            app.start_join_pairing(ticket);
        } else if app.config.servers.is_empty() {
            app.set_status("no servers configured; run chatt join JOIN_STRING");
        }
        Ok(app)
    }

    pub(crate) fn drain_network_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_network_event(event);
        }
    }

    pub(crate) fn drain_audio_device_refreshes(&mut self) {
        while let Ok(refresh) = self.audio_device_refresh_rx.try_recv() {
            self.handle_audio_device_refresh(refresh);
        }
    }

    pub(crate) fn drain_soundboard_events(&mut self) {
        while let Ok(event) = self.soundboard_event_rx.try_recv() {
            self.handle_soundboard_event(event);
        }
    }

    fn rebuild_server_items(&mut self) {
        self.server_items = self
            .config
            .servers
            .iter()
            .map(|server| ServerSelectItem {
                alias: server.alias.clone(),
                user: server.user.clone(),
                display_name: server.display_name.clone(),
                tcp_addr: server.tcp_addr.clone(),
                room_id: server.room_id,
                search_text: format!(
                    "{} {} {} {} {}",
                    server.alias, server.user, server.display_name, server.tcp_addr, server.room_id
                ),
            })
            .collect();
        self.server_select.refresh(&self.server_items);
    }

    fn selected_server_alias(&self) -> Option<String> {
        self.server_select
            .current_item_index()
            .and_then(|index| self.server_items.get(index))
            .map(|item| item.alias.clone())
    }

    fn open_server_select(&mut self) {
        self.set_mode(theme::UiMode::ServerSelect);
        self.server_select_searching = false;
        self.rebuild_server_items();
        if self.config.servers.is_empty() {
            self.set_status("no servers configured; run chatt join JOIN_STRING");
        } else {
            self.set_status("select a server");
        }
    }

    fn open_server_edit(&mut self, alias: &str) {
        let Ok(server) = self.config.server(alias).cloned() else {
            self.set_error(format!("server {alias} is not configured"));
            self.open_server_select();
            return;
        };
        self.server_edit = Some(ServerEditDraft::from_server(&server));
        self.set_mode(theme::UiMode::ServerEdit);
        self.set_status(format!("editing server {}", server.alias));
    }

    fn start_network(&mut self, alias: &str) {
        let server = match self.config.server(alias) {
            Ok(server) => server.clone(),
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        self.disconnect_network();
        let network = match NetworkClient::spawn(
            server.client_config(&self.config.files),
            self.event_tx.clone(),
        ) {
            Ok(network) => network,
            Err(error) => {
                self.set_error(format!("failed to start network: {error}"));
                return;
            }
        };
        self.control_socket = match local_control::ControlSocket::spawn(network.sender()) {
            Ok(socket) => {
                kvlog::info!(
                    "chatt local control socket ready",
                    path = %socket.path().display()
                );
                Some(socket)
            }
            Err(error) => {
                self.push_network_notice("control", &error);
                None
            }
        };
        self.server_alias = server.alias.clone();
        self.user = server.effective_display_name();
        self.room_name = "lobby".to_string();
        self.set_mode(theme::UiMode::Compose);
        self.composer.enter_insert_mode();
        self.network = Some(network);
        self.set_status("connecting");
    }

    fn disconnect_network(&mut self) {
        self.stop_audio();
        self.control_socket.take();
        if let Some(network) = self.network.take() {
            network.stop();
        }
        self.session_id = None;
        self.user_id = None;
        self.participants = Participants::default();
        self.stream_users.clear();
        self.last_network_notice = None;
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
    }

    fn start_join_pairing(&mut self, ticket: InviteTicket) {
        let alias = unique_server_alias(&self.config, &default_join_alias(&ticket));
        let display_name = title_case_ascii(&ticket.user);
        let token = match random_token() {
            Ok(token) => token,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        let server = match server_entry_from_invite(&ticket, alias.clone(), display_name, token) {
            Ok(server) => server,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        if let Err(error) = validate_server_entry(&server) {
            self.set_error(error);
            return;
        }
        spawn_pair_once(
            server.client_config(&self.config.files),
            ticket.pairing_code,
            self.event_tx.clone(),
        );
        self.pending_pair = Some(PendingPair { server });
        self.set_mode(theme::UiMode::ServerSelect);
        self.set_status(format!("pairing {alias}"));
    }

    fn handle_soundboard_event(&mut self, event: SoundboardEvent) {
        match event.result {
            Ok(report) => {
                self.soundboard_next_sequence = report.next_sequence;
                self.set_status(format!(
                    "soundboard {} done: sent {} dropped {} reordered {}",
                    event.clip_name,
                    report.delivered_packets,
                    report.dropped_packets,
                    report.reordered_packets
                ));
            }
            Err(error) => self.set_error(format!("soundboard {} failed: {error}", event.clip_name)),
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
                errors.push(format!("output devices: {error}"));
            }
        }

        self.rebuild_audio_device_pickers();
        kvlog::info!(
            "audio device refresh completed",
            id = refresh.id,
            input_count = input_count.unwrap_or(self.input_devices.len()),
            output_count = output_count.unwrap_or(self.output_devices.len()),
            input_ok = input_count.is_some(),
            output_ok = output_count.is_some(),
        );

        if self.mode == theme::UiMode::Settings {
            if errors.is_empty() {
                self.set_status(format!(
                    "found {} input device(s), {} output device(s) (in {}, out {})",
                    input_count.unwrap_or(0),
                    output_count.unwrap_or(0),
                    refresh.input_buffer_request.label(),
                    refresh.output_buffer_request.label(),
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
                self.set_status(format!("authenticated as {}", self.user));
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
                    if self.config.soundboard.enabled {
                        self.set_status("soundboard ready");
                    } else {
                        self.set_status("voice stream ready");
                    }
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
            NetworkEvent::VoicePacketObserved {
                stream_id,
                payload_size,
            } => {
                self.observe_voice_packet(stream_id, payload_size);
            }
            NetworkEvent::VoicePacket(packet) => {
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
                self.control_socket.take();
                self.network.take();
                self.stream_users.clear();
                self.push_network_notice("auth", &error);
                self.set_error(auth_failure_status(&error));
            }
            NetworkEvent::PairingSucceeded => {
                let Some(pair) = self.pending_pair.take() else {
                    self.set_status("pairing succeeded");
                    return;
                };
                let alias = pair.server.alias.clone();
                self.config.upsert_server(pair.server);
                match self.config.save_runtime() {
                    Ok(path) => {
                        self.config.config_path = Some(path.clone());
                        self.rebuild_server_items();
                        self.open_server_edit(&alias);
                        self.set_status(format!(
                            "paired {alias}; config saved to {}",
                            path.display()
                        ));
                    }
                    Err(error) => {
                        self.rebuild_server_items();
                        self.open_server_edit(&alias);
                        self.set_error(error);
                    }
                }
            }
            NetworkEvent::PairingFailed(error) => {
                self.pending_pair.take();
                self.set_error(error);
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

    fn observe_voice_packet(&mut self, stream_id: u32, payload_size: usize) {
        self.voice_packets_received = self.voice_packets_received.saturating_add(1);
        self.voice_bytes_received = self
            .voice_bytes_received
            .saturating_add(payload_size as u64);
        self.participants.voice_packet(stream_id);
    }

    fn set_network_playback_sink(&self, sink: Option<LivePlaybackSink>) {
        if let Some(network) = &self.network {
            network.send(NetworkCommand::SetPlaybackSink(sink));
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

    pub(crate) fn process_key(&mut self, key: KeyEvent) -> Action {
        crate::tui::modes::process_key(self, key)
    }

    pub(crate) fn process_server_select_key(&mut self, key: KeyEvent) -> Action {
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if self.server_select_searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.server_select_searching = false;
                    return Action::Continue;
                }
                _ if self.server_select.edit_query(key) => {
                    self.server_select.refresh(&self.server_items);
                    return Action::Continue;
                }
                _ => return Action::Continue,
            }
        }

        match key.code {
            KeyCode::Char('/') => {
                self.server_select_searching = true;
                self.server_select.clear_query();
                self.server_select.refresh(&self.server_items);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.server_select.move_selection(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.server_select.move_selection(-1);
            }
            KeyCode::Enter => self.join_selected_server(),
            KeyCode::Char('e') => self.edit_selected_server(),
            KeyCode::Char('d') => self.delete_selected_server(),
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.network.is_some() {
                    self.set_mode(theme::UiMode::Compose);
                    self.composer.enter_insert_mode();
                }
            }
            KeyCode::F2 => self.open_settings(),
            _ => {}
        }
        Action::Continue
    }

    pub(crate) fn process_server_edit_key(&mut self, key: KeyEvent) -> Action {
        if matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        match key.code {
            KeyCode::Esc => {
                self.server_edit = None;
                self.open_server_select();
            }
            KeyCode::Tab | KeyCode::Down => {
                if let Some(draft) = &mut self.server_edit {
                    draft.move_focus(1);
                    self.sync_focus();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if let Some(draft) = &mut self.server_edit {
                    draft.move_focus(-1);
                    self.sync_focus();
                }
            }
            KeyCode::Enter => {
                let focus = self
                    .server_edit
                    .as_ref()
                    .map(|draft| draft.focus)
                    .unwrap_or(ServerEditFocus::Cancel);
                match focus {
                    ServerEditFocus::Save => self.save_server_edit(false),
                    ServerEditFocus::SaveJoin => self.save_server_edit(true),
                    ServerEditFocus::Cancel => {
                        self.server_edit = None;
                        self.open_server_select();
                    }
                    _ => {
                        if let Some(draft) = &mut self.server_edit {
                            draft.move_focus(1);
                            self.sync_focus();
                        }
                    }
                }
            }
            _ => {
                if let Some(draft) = &mut self.server_edit
                    && draft
                        .focused_editor_mut()
                        .is_some_and(|editor| editor.send_key(&key))
                {}
            }
        }
        Action::Continue
    }

    fn join_selected_server(&mut self) {
        let Some(alias) = self.selected_server_alias() else {
            self.set_error("no server selected");
            return;
        };
        self.start_network(&alias);
    }

    fn edit_selected_server(&mut self) {
        let Some(alias) = self.selected_server_alias() else {
            self.set_error("no server selected");
            return;
        };
        self.open_server_edit(&alias);
    }

    fn delete_selected_server(&mut self) {
        let Some(alias) = self.selected_server_alias() else {
            self.set_error("no server selected");
            return;
        };
        self.config.servers.retain(|server| server.alias != alias);
        self.config
            .user_audio
            .retain(|preference| preference.server_alias != alias);
        if self.server_alias == alias {
            self.disconnect_network();
            self.server_alias.clear();
            self.user.clear();
            self.room_name = "servers".to_string();
        }
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.rebuild_server_items();
                self.set_status(format!(
                    "deleted {alias}; config saved to {}",
                    path.display()
                ));
            }
            Err(error) => self.set_error(error),
        }
    }

    fn save_server_edit(&mut self, join_after_save: bool) {
        let Some(draft) = self.server_edit.as_ref() else {
            self.set_error("no server edit is open");
            return;
        };
        let original_alias = draft.original_alias.clone();
        let server = match draft.to_server() {
            Ok(server) => server,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        if server.alias != original_alias
            && self
                .config
                .servers
                .iter()
                .any(|existing| existing.alias == server.alias)
        {
            self.set_error(format!("server alias {} already exists", server.alias));
            return;
        }
        let alias = server.alias.clone();
        if let Some(existing) = self
            .config
            .servers
            .iter_mut()
            .find(|existing| existing.alias == original_alias)
        {
            *existing = server;
        } else {
            self.config.upsert_server(server);
        }
        if alias != original_alias {
            for preference in &mut self.config.user_audio {
                if preference.server_alias == original_alias {
                    preference.server_alias = alias.clone();
                }
            }
            if self.server_alias == original_alias {
                self.server_alias = alias.clone();
            }
        }
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.server_edit = None;
                self.rebuild_server_items();
                if join_after_save {
                    self.start_network(&alias);
                } else {
                    self.open_server_select();
                    self.set_status(format!("server saved to {}", path.display()));
                }
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(crate) fn handle_settings_search_key(&mut self, key: KeyEvent) -> bool {
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

    /// Routes printable input and in-line editing keys to the focused buffer
    /// editor, leaving navigation keys (arrows up/down, Tab, Enter, Esc) for the
    /// settings bindings layer. Mirrors the audio picker's search editing.
    pub(crate) fn handle_settings_buffer_key(&mut self, key: KeyEvent) -> bool {
        if self.mode != theme::UiMode::Settings || matches!(key.kind, KeyEventKind::Release) {
            return false;
        }
        let focus = self.settings_focus;
        let Some(editor) = self.settings.buffer_editor_mut(focus) else {
            return false;
        };
        let editing = match key.code {
            KeyCode::Char(ch) => !ch.is_control(),
            KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End => true,
            _ => false,
        };
        if !editing {
            return false;
        }
        let mutates = !matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Home | KeyCode::End
        );
        if editor.send_key(&key) && mutates {
            self.mark_settings_dirty();
        }
        true
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

    pub(crate) fn process_command(&mut self, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            EnterCompose => {
                self.set_mode(theme::UiMode::Compose);
                self.composer.enter_insert_mode();
            }
            EnterLog => self.set_mode(theme::UiMode::Log),
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
                    self.set_mode(theme::UiMode::Compose);
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
            PlaySoundboard1 => self.trigger_soundboard_slot(0),
            PlaySoundboard2 => self.trigger_soundboard_slot(1),
            PlaySoundboard3 => self.trigger_soundboard_slot(2),
            PlaySoundboard4 => self.trigger_soundboard_slot(3),
            PlaySoundboard5 => self.trigger_soundboard_slot(4),
            PlaySoundboard6 => self.trigger_soundboard_slot(5),
            PlaySoundboard7 => self.trigger_soundboard_slot(6),
            PlaySoundboard8 => self.trigger_soundboard_slot(7),
            PlaySoundboard9 => self.trigger_soundboard_slot(8),
        }
        Action::Continue
    }

    fn open_settings(&mut self) {
        self.set_mode(theme::UiMode::Settings);
        self.settings = SettingsDraft::from_audio(&self.config.audio);
        self.settings_focus = SettingsFocus::InputDevice;
        self.settings_dirty = false;
        if self.allow_settings_preview_capture
            && (self.input_devices.is_empty() || self.output_devices.is_empty())
        {
            self.refresh_audio_devices();
        }
        self.rebuild_audio_device_pickers();
        self.start_settings_preview_capture();
    }

    fn close_settings(&mut self) {
        self.settings.commit_buffer_editor();
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
        self.set_mode(theme::UiMode::Compose);
    }

    fn move_settings_focus(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        if self.audio_picker_open() {
            self.move_active_audio_picker_selection(delta);
            return;
        }
        self.settings.commit_buffer_editor();
        let len = SettingsFocus::ORDER.len() as isize;
        let next = (self.settings_focus.index() as isize + delta).rem_euclid(len) as usize;
        self.settings_focus = SettingsFocus::ORDER[next];
        self.sync_focus();
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
            SettingsFocus::EchoCancellation => {
                self.settings.echo_cancellation = !self.settings.echo_cancellation;
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
            SettingsFocus::InputBuffer
            | SettingsFocus::OutputBuffer
            | SettingsFocus::Refresh
            | SettingsFocus::Save
            | SettingsFocus::Close => {}
        }
    }

    fn activate_settings_focus(&mut self) {
        match self.settings_focus {
            SettingsFocus::Denoise => {
                self.settings.denoise = !self.settings.denoise;
                self.mark_settings_dirty();
            }
            SettingsFocus::EchoCancellation => {
                self.settings.echo_cancellation = !self.settings.echo_cancellation;
                self.mark_settings_dirty();
            }
            SettingsFocus::Refresh => self.refresh_audio_devices(),
            SettingsFocus::Save => self.save_settings(),
            SettingsFocus::Close => self.close_settings(),
            SettingsFocus::InputDevice => self.activate_audio_input_picker(),
            SettingsFocus::OutputDevice => self.activate_audio_output_picker(),
            SettingsFocus::Bitrate | SettingsFocus::Amplification => {
                self.adjust_settings_focus(1);
            }
            SettingsFocus::InputBuffer | SettingsFocus::OutputBuffer => {}
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
            self.sync_focus();
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
            self.sync_focus();
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
        self.sync_focus();
    }

    fn cancel_audio_input_picker(&mut self) {
        if let Some(selection) = self.audio_input_picker.cancel(&self.audio_input_items) {
            self.settings.input_device_id = selection;
        }
        self.sync_focus();
    }

    fn confirm_audio_output_picker(&mut self) {
        let Some(next) = self.audio_output_picker.confirm(&self.audio_output_items) else {
            return;
        };
        if self.settings.output_device_id != next {
            self.settings.output_device_id = next;
            self.mark_settings_dirty();
        }
        self.sync_focus();
    }

    fn cancel_audio_output_picker(&mut self) {
        if let Some(selection) = self.audio_output_picker.cancel(&self.audio_output_items) {
            self.settings.output_device_id = selection;
        }
        self.sync_focus();
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
        self.focus.set(FocusId::Participants);
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
        self.focus.push_modal(FocusId::Dialog);
        self.modes.push(ModeKind::Dialog);
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

    pub(crate) fn handle_volume_dialog_key(&mut self, key: KeyEvent) -> bool {
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
                self.focus.pop_modal(FocusId::Participants);
                self.modes.pop_or(ModeKind::from(self.mode));
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
                            self.focus.pop_modal(FocusId::Participants);
                            self.modes.pop_or(ModeKind::from(self.mode));
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

    pub(crate) fn effective_user_volume_db(&self, user_id: UserId) -> f32 {
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

    fn apply_echo_cancellation_setting(&self) {
        self.echo_control
            .set_enabled(self.config.audio.echo_cancellation);
    }

    fn save_settings(&mut self) {
        self.settings.commit_buffer_editor();
        let restart_preview =
            self.settings_preview_capture && !self.voice_tx_enabled.load(Ordering::Relaxed);
        self.config.audio = self.settings.to_audio();
        self.apply_echo_cancellation_setting();
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
                        "settings saved to {}; live amplification and echo cancellation updated, other audio applies after deafen/rejoin",
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

        let id = self.next_audio_device_refresh_id;
        self.next_audio_device_refresh_id = self.next_audio_device_refresh_id.saturating_add(1);
        self.audio_device_refresh_in_flight = true;
        let input_buffer_request = self.settings.input_buffer_request();
        let output_buffer_request = self.settings.output_buffer_request();
        let tx = self.audio_device_refresh_tx.clone();
        kvlog::info!(
            "audio device refresh started",
            id,
            input_buffer_request = input_buffer_request.label(),
            output_buffer_request = output_buffer_request.label(),
            capture_active = self.capture.is_some(),
            playback_active = self.playback.is_some(),
            settings_preview_capture = self.settings_preview_capture,
        );
        thread::spawn(move || {
            let input = audio::input_devices(input_buffer_request);
            let output = audio::output_devices(output_buffer_request);
            let _ = tx.send(AudioDeviceRefresh {
                id,
                input_buffer_request,
                output_buffer_request,
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
            "/servers" => self.open_server_select(),
            "/soundboard" => self.show_soundboard(),
            "/users" => self.show_users(),
            "/whoami" => self.show_current_user(),
            command if command.starts_with("/upload ") => self.upload_file_command(command),
            command if command.starts_with("/sound") => self.soundboard_command(command),
            command if command.starts_with('/') => {
                self.set_error(format!("unknown command: {command}"))
            }
            body => match &self.network {
                Some(network) => network.send(NetworkCommand::SendChat(body.to_string())),
                None => self.set_error("select a server before sending messages"),
            },
        }
    }

    fn upload_file_command(&mut self, command: &str) {
        let path = command.trim_start_matches("/upload ").trim();
        if path.is_empty() {
            self.set_error("usage: /upload file_path/filename.ext");
            return;
        }
        match &self.network {
            Some(network) => {
                network.send(NetworkCommand::UploadFile(std::path::PathBuf::from(path)));
                self.set_status(format!("queued upload {}", path));
            }
            None => self.set_error("select a server before uploading files"),
        }
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
        let played_samples = stats.direct_samples;
        let direct_percent = if played_samples == 0 {
            0
        } else {
            stats.direct_samples.saturating_mul(100) / played_samples
        };
        let base_target = if stats.adaptive_target_ms == stats.target_queue_ms {
            String::new()
        } else {
            format!(" base{}ms", stats.target_queue_ms)
        };
        let backend_errors = if stats.backend_stream_errors == 0 {
            String::new()
        } else {
            format!(
                " backend_xruns{}/{}",
                stats.backend_xruns, stats.backend_stream_errors
            )
        };
        self.set_status(format!(
            "audio q{}ms target{}ms{} enc{} direct{}% accel{}ms/{} expand{}ms/{} dred{} plc{} trims{} underruns{}{} rx {}/{}",
            stats.max_queue_ms,
            stats.adaptive_target_ms,
            base_target,
            self.encoder_profile.label(),
            direct_percent,
            live_samples_to_ms(stats.accelerate_samples as usize),
            stats.accelerate_count,
            live_samples_to_ms(stats.expand_samples as usize),
            stats.expand_count,
            stats.dred_recoveries,
            stats.plc_fallbacks,
            stats.hard_trim_count,
            stats.underrun_count,
            backend_errors,
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

    fn show_soundboard(&mut self) {
        if !self.config.soundboard.enabled {
            self.set_status("soundboard is disabled");
            return;
        }
        if self.config.soundboard.clips.is_empty() {
            self.set_status("soundboard has no clips");
            return;
        }
        let clips = self
            .config
            .soundboard
            .clips
            .iter()
            .enumerate()
            .map(|(index, clip)| format!("{}:{}", index + 1, clip.name))
            .collect::<Vec<_>>()
            .join(" ");
        self.chat.push_notice(
            "soundboard",
            format!(
                "clips {clips}; loss {}; trigger with /sound N or bound keys",
                self.config.soundboard.loss
            ),
        );
        self.chat.bottom();
        self.set_status("soundboard clips listed");
    }

    fn soundboard_command(&mut self, command: &str) {
        let value = command.trim_start_matches("/sound").trim();
        if value.is_empty() {
            self.show_soundboard();
            return;
        }
        if let Ok(slot) = value.parse::<usize>() {
            self.trigger_soundboard_slot(slot.saturating_sub(1));
            return;
        }
        if let Some(slot) = self
            .config
            .soundboard
            .clips
            .iter()
            .position(|clip| clip.name.eq_ignore_ascii_case(value))
        {
            self.trigger_soundboard_slot(slot);
            return;
        }
        self.set_error(format!("unknown soundboard clip: {value}"));
    }

    fn trigger_soundboard_slot(&mut self, slot: usize) {
        if !self.config.soundboard.enabled {
            self.set_status("soundboard is disabled");
            return;
        }
        let Some(clip) = self.config.soundboard.clips.get(slot).cloned() else {
            self.set_error(format!("soundboard slot {} is not configured", slot + 1));
            return;
        };
        if self.deafened.load(Ordering::Relaxed) {
            self.set_error("undeafen before using soundboard");
            return;
        }
        if !self.voice_tx_enabled.load(Ordering::Relaxed) || !self.local_voice_stream_ready() {
            self.set_error("soundboard voice stream is not ready yet");
            return;
        }
        if self.soundboard_busy.swap(true, Ordering::AcqRel) {
            self.set_status("soundboard is already playing");
            return;
        }
        let Some(packet_loss) = self.config.soundboard.packet_loss() else {
            self.soundboard_busy.store(false, Ordering::Release);
            self.set_error(format!(
                "invalid soundboard loss {}; expected one of: {}",
                self.config.soundboard.loss,
                LiveAudioPacketLossProfile::NAMES.join(", ")
            ));
            return;
        };

        let input_path = self.soundboard_clip_path(&clip);
        let clip_name = clip.name.clone();
        let Some(network) = &self.network else {
            self.soundboard_busy.store(false, Ordering::Release);
            self.set_error("select a server before using soundboard");
            return;
        };
        let network_tx = network.sender();
        let event_tx = self.soundboard_event_tx.clone();
        let busy = Arc::clone(&self.soundboard_busy);
        let source_config = LiveAudioFileSourceConfig {
            input_path,
            tuning: self.config.audio.latency.to_tuning(),
            packet_loss,
            seed: self.config.soundboard.seed.wrapping_add(slot as u64),
            first_sequence: self.soundboard_next_sequence,
            max_amplification: self.config.audio.max_amplification,
            denoise: self.config.audio.denoise,
            auto_gain: true,
        };
        self.set_status(format!(
            "soundboard playing {} ({})",
            clip.name,
            packet_loss.as_name()
        ));
        thread::spawn(move || {
            let result = audio::run_live_audio_file_source(source_config, |sequence, frame| {
                let _ =
                    network_tx.send(NetworkCommand::SequencedLocalVoicePacket { sequence, frame });
            });
            busy.store(false, Ordering::Release);
            let _ = event_tx.send(SoundboardEvent { clip_name, result });
        });
    }

    fn soundboard_clip_path(&self, clip: &SoundboardClip) -> PathBuf {
        let path = PathBuf::from(&clip.path);
        if path.is_absolute() || path.exists() {
            return path;
        }
        self.config
            .config_path
            .as_ref()
            .and_then(|path| path.parent())
            .map(|parent| parent.join(&clip.path))
            .unwrap_or(path)
    }

    fn local_voice_stream_ready(&self) -> bool {
        let Some(user_id) = self.user_id else {
            return false;
        };
        self.stream_users
            .values()
            .any(|stream_user| *stream_user == user_id)
    }

    fn ensure_mic_capture(&mut self) -> Result<(), String> {
        if self.capture.is_some() {
            return Ok(());
        }
        if let Some(id) = self.config.audio.input_device_id.as_deref() {
            if !self.input_devices.is_empty() {
                if let Some(item) = self
                    .audio_input_items
                    .iter()
                    .find(|item| item.matches_selection(Some(id)))
                {
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
        }

        let tx = self.network.as_ref().map(|network| network.sender());
        let mic_muted = Arc::clone(&self.mic_muted);
        let deafened = Arc::clone(&self.deafened);
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        match audio::start_live_capture(
            LiveCaptureConfig {
                input_device_id: self.config.audio.input_device_id.clone(),
                bitrate_bps: self.config.audio.bitrate_bps,
                denoise: self.config.audio.denoise,
                max_amplification: self.config.audio.max_amplification,
                buffer_request: self.input_buffer_request(),
                tuning: self.config.audio.latency.to_tuning(),
                echo_control: Some(Arc::clone(&self.echo_control)),
            },
            move |payload| {
                if mic_muted.load(Ordering::Relaxed)
                    || deafened.load(Ordering::Relaxed)
                    || !voice_tx_enabled.load(Ordering::Relaxed)
                {
                    return;
                }
                if let Some(tx) = &tx {
                    let _ = tx.send(NetworkCommand::LocalVoicePacket(payload));
                }
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
        if self.network.is_none() {
            self.voice_tx_enabled.store(false, Ordering::Relaxed);
            self.set_error("select a server before starting voice");
            return;
        }
        if self.deafened.load(Ordering::Relaxed) {
            self.voice_tx_enabled.store(false, Ordering::Relaxed);
            self.stop_mic_capture();
            self.set_network_playback_sink(None);
            self.playback.take();
            self.set_status("deafened");
            return;
        }

        self.voice_tx_enabled.store(true, Ordering::Relaxed);
        let mut capture_ok = true;
        if self.config.soundboard.enabled {
            self.settings_preview_capture = false;
            self.mic_error = None;
        } else if let Err(error) = self.ensure_mic_capture() {
            capture_ok = false;
            self.set_error(format!("failed to start capture: {error}"));
        } else {
            self.settings_preview_capture = false;
        }
        if self.playback.is_none() {
            let (feedback_tx, feedback_rx) = mpsc::channel::<LivePlaybackFeedback>();
            let Some(network) = &self.network else {
                self.set_error("select a server before starting playback");
                return;
            };
            let network_tx = network.sender();
            thread::spawn(move || {
                for feedback in feedback_rx {
                    let _ = network_tx.send(NetworkCommand::PlaybackFeedback(feedback));
                }
            });
            match audio::start_live_playback(LivePlaybackConfig {
                output_device_id: self.config.audio.output_device_id.clone(),
                buffer_request: self.output_buffer_request(),
                tuning: self.config.audio.latency.to_tuning(),
                feedback_sender: Some(feedback_tx),
                echo_control: Some(Arc::clone(&self.echo_control)),
            }) {
                Ok(playback) => {
                    let fell_back = playback.buffer_fallback();
                    let sink = playback.sink();
                    self.playback = Some(playback);
                    self.playback_error = None;
                    self.set_network_playback_sink(sink);
                    self.apply_all_user_audio_controls();
                    if fell_back
                        || self
                            .capture
                            .as_ref()
                            .is_some_and(LiveCapture::buffer_fallback)
                    {
                        self.set_error(
                            "requested audio buffer unsupported; using device default (higher latency)"
                                .to_string(),
                        );
                    } else if capture_ok {
                        if self.config.soundboard.enabled {
                            self.set_status("soundboard voice active");
                        } else {
                            self.set_status("voice active");
                        }
                    }
                }
                Err(error) => {
                    self.set_network_playback_sink(None);
                    self.playback = None;
                    self.playback_error = Some(error.clone());
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
        self.set_network_playback_sink(None);
        self.playback.take();
        self.playback_error = None;
        if restart_settings_preview {
            self.start_settings_preview_capture();
        }
    }

    fn stop_mic_capture(&mut self) {
        self.settings_preview_capture = false;
        self.capture.take();
    }

    fn input_buffer_request(&self) -> BufferRequest {
        self.config
            .audio
            .input_buffer
            .to_request(config::DEFAULT_INPUT_BUFFER_SAMPLES)
    }

    fn output_buffer_request(&self) -> BufferRequest {
        self.config
            .audio
            .output_buffer
            .to_request(config::DEFAULT_OUTPUT_BUFFER_SAMPLES)
    }

    fn set_status(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.status_kind = StatusKind::Info;
    }

    fn set_error(&mut self, status: impl Into<String>) {
        self.status = status.into();
        self.status_kind = StatusKind::Error;
    }

    fn set_mode(&mut self, mode: theme::UiMode) {
        self.mode = mode;
        self.modes.set(ModeKind::from(mode));
        self.sync_focus();
    }

    fn sync_focus(&mut self) {
        let focus = match self.mode {
            theme::UiMode::ServerSelect => FocusId::ServerList,
            theme::UiMode::ServerEdit => self
                .server_edit
                .as_ref()
                .map(|draft| FocusId::ServerField(server_field(draft.focus)))
                .unwrap_or(FocusId::ServerList),
            theme::UiMode::Compose => FocusId::Composer,
            theme::UiMode::Log => FocusId::Chat,
            theme::UiMode::Settings => {
                if self.audio_input_picker.open {
                    FocusId::InputPicker
                } else if self.audio_output_picker.open {
                    FocusId::OutputPicker
                } else {
                    FocusId::Settings(settings_field(self.settings_focus))
                }
            }
        };
        self.focus.set(focus);
    }
}

fn server_field(focus: ServerEditFocus) -> ServerField {
    match focus {
        ServerEditFocus::Alias => ServerField::Alias,
        ServerEditFocus::DisplayName => ServerField::DisplayName,
        ServerEditFocus::TcpAddr => ServerField::TcpAddr,
        ServerEditFocus::UdpAddr => ServerField::UdpAddr,
        ServerEditFocus::UdpProbeAddr => ServerField::UdpProbeAddr,
        ServerEditFocus::RoomId => ServerField::RoomId,
        ServerEditFocus::Save => ServerField::Save,
        ServerEditFocus::SaveJoin => ServerField::SaveJoin,
        ServerEditFocus::Cancel => ServerField::Cancel,
    }
}

fn settings_field(focus: SettingsFocus) -> SettingsField {
    match focus {
        SettingsFocus::InputDevice => SettingsField::InputDevice,
        SettingsFocus::OutputDevice => SettingsField::OutputDevice,
        SettingsFocus::Bitrate => SettingsField::Bitrate,
        SettingsFocus::Denoise => SettingsField::Denoise,
        SettingsFocus::EchoCancellation => SettingsField::EchoCancellation,
        SettingsFocus::Amplification => SettingsField::Amplification,
        SettingsFocus::InputBuffer => SettingsField::InputBuffer,
        SettingsFocus::OutputBuffer => SettingsField::OutputBuffer,
        SettingsFocus::Refresh => SettingsField::Refresh,
        SettingsFocus::Save => SettingsField::Save,
        SettingsFocus::Close => SettingsField::Close,
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

pub(crate) fn volume_db_label(value_db: f32) -> String {
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

fn live_samples_to_ms(samples: usize) -> u64 {
    ((samples as f64 / f64::from(audio::SAMPLE_RATE)) * 1_000.0).round() as u64
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
        NetworkEvent::VoicePacketObserved { .. } => "voice_packet_observed",
        NetworkEvent::VoicePacket(_) => "voice_packet",
        NetworkEvent::PlaybackFeedback(_) => "playback_feedback",
        NetworkEvent::EncoderProfileChanged(_) => "encoder_profile_changed",
        NetworkEvent::Status(_) => "status",
        NetworkEvent::Error(_) => "error",
        NetworkEvent::AuthFailed(_) => "auth_failed",
        NetworkEvent::PairingSucceeded => "pairing_succeeded",
        NetworkEvent::PairingFailed(_) => "pairing_failed",
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
    use extui::{Buffer, event::KeyModifiers};
    use rpc::control::ParticipantInfo;

    fn test_app() -> App {
        App::new(Config::default(), None).expect("test app")
    }

    #[test]
    fn server_edit_reuses_one_editor_across_text_fields() {
        let mut draft = ServerEditDraft::from_server(&crate::config::ServerEntry::default());
        let first_editor = draft.focused_editor_mut().unwrap() as *mut _ as usize;
        draft.focused_editor_mut().unwrap().set_lines("local-dev");

        draft.move_focus(1);

        assert_eq!(draft.alias, "local-dev");
        let second_editor = draft.focused_editor_mut().unwrap() as *mut _ as usize;
        assert_eq!(first_editor, second_editor);
        draft.focused_editor_mut().unwrap().set_lines("Alice Dev");

        let server = draft.to_server().unwrap();
        assert_eq!(server.alias, "local-dev");
        assert_eq!(server.display_name, "Alice Dev");
    }

    #[test]
    fn settings_buffers_reuse_one_editor_and_commit_on_focus_change() {
        let mut draft = SettingsDraft::from_audio(&crate::config::AudioConfig::default());
        let input_editor =
            draft.buffer_editor_mut(SettingsFocus::InputBuffer).unwrap() as *mut _ as usize;
        draft
            .buffer_editor_mut(SettingsFocus::InputBuffer)
            .unwrap()
            .set_lines("1024");

        draft.focus_buffer_editor(SettingsFocus::OutputBuffer);

        assert_eq!(draft.input_buffer, "1024");
        let output_editor = draft
            .buffer_editor_mut(SettingsFocus::OutputBuffer)
            .unwrap() as *mut _ as usize;
        assert_eq!(input_editor, output_editor);
    }

    #[test]
    fn volume_dialog_pushes_and_restores_focus() {
        let mut app = test_app();
        app.server_alias = "local".to_string();
        app.user_id = Some(UserId(1));
        app.set_mode(theme::UiMode::Log);
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

        assert_eq!(app.focus.active(), FocusId::Dialog);
        assert_eq!(app.modes.top(), ModeKind::Dialog);

        assert!(app.handle_volume_dialog_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())));

        assert!(app.volume_dialog.is_none());
        assert_eq!(app.focus.active(), FocusId::Participants);
        assert_eq!(app.modes.top(), ModeKind::Workspace);
    }

    #[test]
    fn shift_enter_inserts_newline_in_composer() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);

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

    #[test]
    fn renders_smoke_frame() {
        let mut app = test_app();
        let mut buffer = Buffer::new(80, 24);
        crate::tui::render(&mut app, &mut buffer);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_audio();
    }
}
