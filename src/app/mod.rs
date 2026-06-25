pub(crate) mod audio_diagnostics;
pub(crate) mod dialogs;
pub(crate) mod participants;
pub(crate) mod server;

use hashbrown::{HashMap, HashSet};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use extui::Rect;
use extui::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use extui_bindings::InputKey;
use extui_editor::{Editor, Mode as EditorMode, Span as EditorSpan, bindings as editor_bindings};
use rpc::{
    control::{ChatMessage, InviteTicket},
    ids::{SessionId, UserId},
};

use crate::{
    bindings::{self, BindCommand, PendingChord, Resolved},
    chat_buffer::{LineKind, VirtualChatBuffer, VisibleLine},
    client_net::{NetworkClient, NetworkCommand, NetworkEvent, spawn_pair_once},
    config::{self, Config, SoundboardClip, ThemeChoice, validate_server_entry},
    local_control,
    settings::{
        self, AudioInputPickerState, AudioOutputPickerState, SettingsDraft, SettingsFocus,
        SettingsMutation,
    },
    theme::{self, Theme},
    tui::{
        Action,
        editor::EditorHighlighter,
        focus::{FocusId, FocusManager, ServerField, SettingsField},
        form::{FormAction, FormMouseIntent, FormState, rect_contains},
        modes::{ModeKind, ModeStack},
    },
    ui::select::FuzzySelect,
};

use chatt::audio::{
    self, BufferRequest, DeviceInfo, EchoCancellationControl, LiveAudioFileSourceConfig,
    LiveAudioFileSourceReport, LiveAudioPacketLossProfile, LiveCapture, LiveCaptureConfig,
    LiveEncoderProfile, LivePlayback, LivePlaybackConfig, LivePlaybackFeedback, LivePlaybackSink,
    LocalVoiceFrame, PlaybackStreamControl,
};

use audio_diagnostics::AudioDiagnostics;

pub(crate) use dialogs::{UserVolumeDialog, UserVolumeEvent};
pub(crate) use participants::{ParticipantState, Participants};
pub(crate) use server::{
    PendingPair, ServerEditDraft, ServerEditEvent, ServerEditFocus, ServerSelectItem,
    default_join_alias, random_token, server_entry_from_invite, title_case_ascii,
    unique_server_alias,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusKind {
    Info,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatPanelFocus {
    Lobby,
    ChatLog,
    Compose,
}

impl ChatPanelFocus {
    const ORDER: [Self; 3] = [Self::Lobby, Self::ChatLog, Self::Compose];

    fn moved(self, delta: isize) -> Self {
        let current = Self::ORDER
            .iter()
            .position(|panel| *panel == self)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(Self::ORDER.len() as isize) as usize;
        Self::ORDER[next]
    }

    fn focus_id(self) -> FocusId {
        match self {
            Self::Lobby => FocusId::Participants,
            Self::ChatLog => FocusId::Chat,
            Self::Compose => FocusId::Composer,
        }
    }
}

pub(crate) struct App {
    pub(crate) config: Config,
    pub(crate) theme: Theme,
    pub(crate) event_tx: Sender<NetworkEvent>,
    pub(crate) server_alias: String,
    pub(crate) user: String,
    pub(crate) room_name: String,
    pub(crate) status: String,
    pub(crate) status_kind: StatusKind,
    pub(crate) mode: theme::UiMode,
    pub(crate) chat_focus: ChatPanelFocus,
    pub(crate) focus: FocusManager,
    pub(crate) modes: ModeStack,
    pub(crate) composer: Editor,
    pub(crate) composer_hl: EditorHighlighter,
    pub(crate) chat: VirtualChatBuffer,
    pub(crate) participants: Participants,
    pub(crate) last_chat_width: u16,
    pub(crate) last_chat_height: u16,
    pub(crate) last_chat_rect: Rect,
    pub(crate) last_chat_lines: Vec<VisibleLine>,
    pub(crate) last_room_rect: Rect,
    pub(crate) last_lobby_bar_rect: Rect,
    pub(crate) last_chat_log_bar_rect: Rect,
    pub(crate) last_composer_rect: Rect,
    pub(crate) last_compose_bar_rect: Rect,
    pub(crate) top_bar_mute_rect: Rect,
    pub(crate) top_bar_deafen_rect: Rect,
    pub(crate) pending_clipboard: Option<String>,
    pub(crate) pending_chord: Option<PendingChord>,
    pub(crate) key_preview_expanded: bool,
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
    pub(crate) settings_form: FormState<SettingsFocus>,
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
    pub(crate) pending_audio_apply: Option<PendingAudioApply>,
    pending_network_commands: VecDeque<NetworkCommand>,
    supervisor: SupervisorState,
}

/// A debounced request to restart audio streams so a slow settings-page change
/// (device, bitrate, denoise, buffer size, latency tuning) takes effect. Rapid
/// edits coalesce into one restart once `deadline` passes.
pub(crate) struct PendingAudioApply {
    capture: bool,
    playback: bool,
    deadline: Instant,
}

#[derive(Default)]
struct SupervisorState {
    network: RecoveryState,
    control_socket: RecoveryState,
    capture: RecoveryState,
    playback: RecoveryState,
    capture_watch: CaptureWatch,
    playback_watch: PlaybackWatch,
}

#[derive(Default)]
struct CaptureWatch {
    callbacks: u64,
    captured_samples: u64,
    stream_errors: u64,
    worker_stopped: bool,
    last_progress_at: Option<Instant>,
}

#[derive(Default)]
struct PlaybackWatch {
    backend_stream_errors: u64,
}

#[derive(Default)]
struct RecoveryState {
    attempts: Vec<Instant>,
    next_retry_at: Option<Instant>,
    reason: Option<String>,
    exhausted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoverySchedule {
    Scheduled(Duration),
    Pending,
    Exhausted,
}

const RECOVERY_WINDOW: Duration = Duration::from_secs(30);
const RECOVERY_MAX_ATTEMPTS: usize = 3;
const CAPTURE_STALL_TIMEOUT: Duration = Duration::from_millis(750);

/// Debounce window before a scheduled audio restart fires. Coalesces rapid
/// settings edits (cycling a choice, typing a buffer size) into one restart.
const AUDIO_APPLY_DEBOUNCE: Duration = Duration::from_millis(400);

impl RecoveryState {
    fn schedule(&mut self, now: Instant, reason: impl Into<String>) -> RecoverySchedule {
        if self.exhausted {
            return RecoverySchedule::Exhausted;
        }
        if self.next_retry_at.is_some() {
            return RecoverySchedule::Pending;
        }
        self.attempts
            .retain(|attempt| now.saturating_duration_since(*attempt) <= RECOVERY_WINDOW);
        if self.attempts.len() >= RECOVERY_MAX_ATTEMPTS {
            self.exhausted = true;
            return RecoverySchedule::Exhausted;
        }
        let attempt = self.attempts.len() + 1;
        let delay = recovery_delay(attempt);
        self.attempts.push(now);
        self.next_retry_at = Some(now + delay);
        self.reason = Some(reason.into());
        RecoverySchedule::Scheduled(delay)
    }

    fn take_due(&mut self, now: Instant) -> Option<String> {
        if self.exhausted || !self.next_retry_at.is_some_and(|deadline| now >= deadline) {
            return None;
        }
        self.next_retry_at = None;
        self.reason.take()
    }

    fn reset(&mut self) {
        self.attempts.clear();
        self.next_retry_at = None;
        self.reason = None;
        self.exhausted = false;
    }

    fn is_pending(&self) -> bool {
        self.next_retry_at.is_some()
    }
}

/// Backoff before the n-th recovery attempt. `schedule` only ever passes
/// attempts `1..=RECOVERY_MAX_ATTEMPTS`, so the first attempt is immediate and
/// the rest ramp up before exhaustion.
fn recovery_delay(attempt: usize) -> Duration {
    match attempt {
        0 | 1 => Duration::ZERO,
        2 => Duration::from_secs(1),
        _ => Duration::from_secs(2),
    }
}

/// Returns which audio streams must restart for an audio-config change to take
/// effect, as `(capture, playback)`. Cheap in-place fields (amplification, echo
/// cancellation) do not appear here because they never require a restart.
fn audio_restart_flags(old: &config::AudioConfig, new: &config::AudioConfig) -> (bool, bool) {
    let capture = old.input_device_id != new.input_device_id
        || old.bitrate_bps != new.bitrate_bps
        || old.denoise != new.denoise
        || old.denoise_suppression != new.denoise_suppression
        || old.denoise_release != new.denoise_release
        || old.denoise_typing_suppression != new.denoise_typing_suppression
        || old.denoise_typing_vad_enter != new.denoise_typing_vad_enter
        || old.denoise_typing_vad_release != new.denoise_typing_vad_release
        || old.input_buffer != new.input_buffer
        || old.latency != new.latency;
    let playback = old.output_device_id != new.output_device_id
        || old.output_buffer != new.output_buffer
        || old.latency != new.latency;
    (capture, playback)
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
        let theme = Theme::from_choice(config.ui.theme);
        let mut composer =
            Editor::with_bindings(editor_bindings::vim(editor_bindings::VimOptions::default()));
        composer.set_wrap(true);
        composer.set_height_bounds(1, config.ui.max_composer_height.max(1));
        composer.set_theme(theme.editor_theme());
        composer.enter_insert_mode();
        let composer_hl = EditorHighlighter::new(&mut composer);
        let mut settings_draft = SettingsDraft::from_audio(&config.audio);
        settings_draft.set_form_bindings_from_config(config.ui.form_bindings);
        settings_draft.set_theme_from_config(config.ui.theme);
        let settings_form = FormState::with_order(
            SettingsFocus::CaptureDevice,
            config.ui.form_bindings,
            SettingsFocus::ORDER,
        );
        let audio_input_items = settings::audio_input_items(&[]);
        let audio_output_items = settings::audio_output_items(&[]);
        let mut audio_input_picker = AudioInputPickerState::default();
        audio_input_picker.reset(&audio_input_items, settings_draft.input_selection());
        let mut audio_output_picker = AudioOutputPickerState::default();
        audio_output_picker.reset(&audio_output_items, settings_draft.output_selection());
        let echo_control = Arc::new(EchoCancellationControl::new(config.audio.echo_cancellation));
        let mut app = Self {
            theme,
            event_tx,
            server_alias: String::new(),
            user: String::new(),
            room_name: "servers".to_string(),
            status: "select a server".to_string(),
            status_kind: StatusKind::Info,
            mode: theme::UiMode::ServerSelect,
            chat_focus: ChatPanelFocus::Compose,
            focus: FocusManager::new(FocusId::ServerList),
            modes: ModeStack::new(ModeKind::ServerSelect),
            composer,
            composer_hl,
            chat: VirtualChatBuffer::new(config.ui.max_messages as usize, theme.syntax),
            participants: Participants::default(),
            last_chat_width: 80,
            last_chat_height: 0,
            last_chat_rect: Rect::EMPTY,
            last_chat_lines: Vec::new(),
            last_room_rect: Rect::EMPTY,
            last_lobby_bar_rect: Rect::EMPTY,
            last_chat_log_bar_rect: Rect::EMPTY,
            last_composer_rect: Rect::EMPTY,
            last_compose_bar_rect: Rect::EMPTY,
            top_bar_mute_rect: Rect::EMPTY,
            top_bar_deafen_rect: Rect::EMPTY,
            pending_clipboard: None,
            pending_chord: None,
            key_preview_expanded: false,
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
            settings_form,
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
            pending_audio_apply: None,
            pending_network_commands: VecDeque::new(),
            supervisor: SupervisorState::default(),
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
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => self.handle_network_event(event),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.schedule_network_recovery(
                        Instant::now(),
                        "network event channel disconnected",
                    );
                    break;
                }
            }
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
        self.server_edit = Some(ServerEditDraft::from_server(
            &server,
            self.config.ui.form_bindings,
        ));
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
            server.client_config(&self.config.files, &self.config.p2p),
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
                self.supervisor.control_socket.reset();
                Some(socket)
            }
            Err(error) => {
                self.push_network_notice("control", &error);
                self.schedule_control_socket_recovery(Instant::now(), error.clone());
                None
            }
        };
        self.server_alias = server.alias.clone();
        self.user = server.effective_display_name();
        self.room_name = "lobby".to_string();
        self.enter_compose_insert_mode();
        self.network = Some(network);
        self.supervisor.network.reset();
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
        self.pending_network_commands.clear();
        self.supervisor.network.reset();
        self.supervisor.control_socket.reset();
        self.supervisor.capture.reset();
        self.supervisor.playback.reset();
        self.supervisor.capture_watch = CaptureWatch::default();
        self.supervisor.playback_watch = PlaybackWatch::default();
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
            server.client_config(&self.config.files, &self.config.p2p),
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
                self.flush_pending_network_commands();
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
            NetworkEvent::WorkerStopped { reason } => {
                self.stop_audio();
                self.stream_users.clear();
                self.push_network_notice(
                    "network",
                    &format!("Network worker stopped: {reason}; reconnecting"),
                );
                self.schedule_network_recovery(Instant::now(), reason);
            }
            NetworkEvent::Disconnected => {
                self.stop_audio();
                self.stream_users.clear();
                self.schedule_network_recovery(Instant::now(), "network worker disconnected");
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

    fn set_network_playback_sink(&mut self, sink: Option<LivePlaybackSink>) {
        if self.network.is_some() {
            self.send_network_command(NetworkCommand::SetPlaybackSink(sink), false);
        }
    }

    fn send_network_command(&mut self, command: NetworkCommand, queue_on_failure: bool) -> bool {
        let Some(network) = &self.network else {
            return false;
        };
        match network.try_send(command) {
            Ok(()) => true,
            Err(error) => {
                let command = error.0;
                let kind = app_network_command_kind(&command);
                kvlog::warn!("network command send failed", kind);
                if queue_on_failure {
                    self.pending_network_commands.push_back(command);
                }
                self.schedule_network_recovery(
                    Instant::now(),
                    format!("network command channel closed while sending {kind}"),
                );
                self.set_error("network worker stopped; reconnecting");
                false
            }
        }
    }

    fn flush_pending_network_commands(&mut self) {
        if self.pending_network_commands.is_empty() || self.network.is_none() {
            return;
        }
        let mut sent = 0usize;
        let mut remaining = VecDeque::new();
        while let Some(command) = self.pending_network_commands.pop_front() {
            let Some(network) = &self.network else {
                remaining.push_back(command);
                break;
            };
            match network.try_send(command) {
                Ok(()) => sent += 1,
                Err(error) => {
                    remaining.push_back(error.0);
                    while let Some(command) = self.pending_network_commands.pop_front() {
                        remaining.push_back(command);
                    }
                    self.schedule_network_recovery(
                        Instant::now(),
                        "network command channel closed while flushing queued commands",
                    );
                    break;
                }
            }
        }
        self.pending_network_commands = remaining;
        if sent > 0 {
            self.set_status(format!("sent {sent} queued network command(s)"));
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

    pub(crate) fn process_mouse(&mut self, mouse: MouseEvent) -> Action {
        if self.process_top_bar_mouse(mouse) {
            return Action::Continue;
        }

        match self.mode {
            theme::UiMode::Settings => self.process_settings_mouse(mouse),
            theme::UiMode::ServerEdit => self.process_server_edit_mouse(mouse),
            theme::UiMode::Compose | theme::UiMode::Log => self.process_chat_mouse(mouse),
            _ => Action::Continue,
        }
    }

    fn process_top_bar_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        if rect_contains(self.top_bar_mute_rect, mouse.column, mouse.row) {
            self.toggle_mute();
            return true;
        }
        if rect_contains(self.top_bar_deafen_rect, mouse.column, mouse.row) {
            self.set_deafen(!self.deafened.load(Ordering::Relaxed));
            return true;
        }
        false
    }

    fn process_chat_mouse(&mut self, mouse: MouseEvent) -> Action {
        let rect = self.last_chat_rect;
        let in_chat = rect_contains(rect, mouse.column, mouse.row);
        let in_chat_bar = rect_contains(self.last_chat_log_bar_rect, mouse.column, mouse.row);
        let in_room = rect_contains(self.last_room_rect, mouse.column, mouse.row);
        let in_lobby_bar = rect_contains(self.last_lobby_bar_rect, mouse.column, mouse.row);
        let in_composer = rect_contains(self.last_composer_rect, mouse.column, mouse.row)
            || rect_contains(self.last_compose_bar_rect, mouse.column, mouse.row);

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if in_composer => {
                self.enter_compose_insert_mode();
            }
            MouseEventKind::Down(MouseButton::Left) if in_lobby_bar => {
                self.set_chat_panel_focus(ChatPanelFocus::Lobby);
            }
            MouseEventKind::ScrollUp if in_room => {
                self.move_room_selection_with_focus(-1);
            }
            MouseEventKind::ScrollDown if in_room => {
                self.move_room_selection_with_focus(1);
            }
            MouseEventKind::Down(MouseButton::Left) if in_room => {
                self.set_chat_panel_focus(ChatPanelFocus::Lobby);
                let row = mouse.row.saturating_sub(self.last_room_rect.y) as usize;
                if self.participants.select_visible_row(row).is_some() {
                    self.keep_selected_room_user_visible();
                    self.refresh_mode_and_focus();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if in_chat_bar => {
                self.set_chat_panel_focus(ChatPanelFocus::ChatLog);
            }
            MouseEventKind::ScrollUp if in_chat => {
                self.set_chat_panel_focus(ChatPanelFocus::ChatLog);
                self.scroll_chat_up(5);
            }
            MouseEventKind::ScrollDown if in_chat => {
                self.set_chat_panel_focus(ChatPanelFocus::ChatLog);
                self.chat.scroll_down(5);
            }
            MouseEventKind::Down(MouseButton::Left) if in_chat => {
                self.set_chat_panel_focus(ChatPanelFocus::ChatLog);
                match self.chat_line_at(mouse.row) {
                    Some(line) => match line.kind {
                        LineKind::Heading | LineKind::Ellipsis => {
                            self.chat
                                .select_header_containing(line.message, self.last_chat_width);
                            self.chat.toggle_expand(line.message, self.last_chat_width);
                            self.chat.clear_selection();
                        }
                        LineKind::Body => {
                            self.chat.begin_selection((line.message, line.line));
                        }
                    },
                    _ => self.chat.clear_selection(),
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.chat.is_selecting() => {
                self.set_chat_panel_focus(ChatPanelFocus::ChatLog);
                self.drag_chat_selection(mouse.row);
            }
            MouseEventKind::Up(MouseButton::Left) => self.chat.end_selection(),
            _ => {}
        }
        Action::Continue
    }

    /// Extends the active selection toward `row`, auto-scrolling when the cursor
    /// is dragged above the top or below the bottom of the chat area.
    fn drag_chat_selection(&mut self, row: u16) {
        let rect = self.last_chat_rect;
        if row < rect.y {
            self.scroll_chat_up(1);
        } else if row >= rect.y.saturating_add(rect.h) {
            self.chat.scroll_down(1);
        }
        let clamped = row.clamp(rect.y, rect.y.saturating_add(rect.h).saturating_sub(1));
        if let Some(line) = self.chat_line_at(clamped)
            && line.kind == LineKind::Body
        {
            self.chat.extend_selection((line.message, line.line));
        }
    }

    /// Maps a screen `row` to the [`VisibleLine`] it renders, using the
    /// top-anchored layout captured during the last render.
    fn chat_line_at(&self, row: u16) -> Option<VisibleLine> {
        let index = row.checked_sub(self.last_chat_rect.y)? as usize;
        self.last_chat_lines.get(index).copied()
    }

    fn toggle_selected_log_collapse(&mut self) {
        let width = self.last_chat_width;
        if self.chat.ensure_selected_header(width).is_none() {
            self.set_status("no messages");
            return;
        }
        self.chat.clear_selection();
        if !self.chat.toggle_selected_expand(width) {
            self.set_status("selected log is not collapsible");
        }
        self.keep_selected_chat_header_visible();
    }

    fn scroll_chat_up(&mut self, rows: usize) {
        self.chat
            .scroll_up(rows, self.last_chat_width, self.last_chat_height);
    }

    fn copy_chat_selection(&mut self) {
        if let Some(text) = self.chat.selected_text() {
            self.pending_clipboard = Some(text);
        } else if let Some(text) = self.chat.selected_header_text(self.last_chat_width) {
            self.pending_clipboard = Some(text);
        }
    }

    pub(crate) fn take_pending_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    /// Inserts terminal-pasted text at the composer cursor while composing.
    pub(crate) fn handle_paste(&mut self, text: String) {
        if self.mode != theme::UiMode::Compose {
            return;
        }
        let span = EditorSpan::empty_at(self.composer.cursor_offset());
        self.composer.replace_range(span, &text);
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
                    self.enter_compose_insert_mode();
                }
            }
            KeyCode::F2 => self.open_settings(),
            _ => {}
        }
        Action::Continue
    }

    pub(crate) fn process_server_edit_key(&mut self, key: KeyEvent) -> Action {
        let Some(draft) = self.server_edit.as_mut() else {
            return Action::Continue;
        };
        match draft.handle_key(key) {
            ServerEditEvent::Consumed => {
                self.sync_focus();
            }
            ServerEditEvent::Cancel => {
                self.server_edit = None;
                self.open_server_select();
            }
            ServerEditEvent::Save { join_after_save } => {
                self.save_server_edit(join_after_save);
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
        let update = match draft.to_update() {
            Ok(update) => update,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        let original_alias = update.original_alias;
        let server = update.server;
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

    pub(crate) fn process_settings_key(&mut self, key: KeyEvent) -> Action {
        if self.mode != theme::UiMode::Settings || matches!(key.kind, KeyEventKind::Release) {
            return Action::Continue;
        }
        if self.handle_open_settings_picker_key(key) {
            self.sync_focus();
            return Action::Continue;
        }

        let focus = self.settings_form.focus();
        let kind = self.settings.field_kind(focus);
        let text_focused = kind == crate::tui::form::FormFieldKind::Text;

        let event = self.settings_form.handle_key(key, kind);
        self.apply_settings_commit(event.commit);
        match event.action {
            FormAction::None => {
                if !text_focused {
                    return self.resolve_settings_binding(key);
                }
            }
            FormAction::Cancel => {
                if !self.cancel_open_audio_picker() {
                    self.close_settings();
                }
            }
            FormAction::Activate => self.activate_settings_focus(),
            FormAction::Adjust(delta) => self.adjust_settings_focus(delta),
            FormAction::FocusMoved => self.sync_focus(),
            FormAction::TextChanged => self.mark_settings_dirty(),
            FormAction::Scrolled => {}
        }
        Action::Continue
    }

    /// Dispatches a key the settings form left unhandled through the
    /// `SETTINGS_LAYER` bindings, so command bindings such as `Ctrl-s`
    /// (`SaveSettings`), `q`, `r`, and `w` work while the form keeps ownership
    /// of movement and text editing.
    fn resolve_settings_binding(&mut self, key: KeyEvent) -> Action {
        let Some(input) = InputKey::from_event(&key) else {
            return Action::Continue;
        };
        match bindings::resolve(
            &self.config.bindings.router,
            bindings::SETTINGS_LAYER,
            &mut self.pending_chord,
            input,
        ) {
            Resolved::Action(id) => {
                let command = self.config.bindings.actions.get(id).clone();
                self.process_command(command)
            }
            Resolved::Consumed | Resolved::Unmatched => Action::Continue,
        }
    }

    fn process_settings_mouse(&mut self, mouse: MouseEvent) -> Action {
        if self.handle_open_settings_picker_mouse(mouse) {
            self.sync_focus();
            return Action::Continue;
        }

        let event = self.settings_form.handle_mouse(mouse);
        self.apply_settings_commit(event.commit);
        match event.intent {
            FormMouseIntent::None => {}
            FormMouseIntent::Activate(field) => {
                let _ = self.settings_form.set_focus(field);
                self.activate_settings_focus();
            }
            FormMouseIntent::Adjust(field, delta) => {
                let commit = self.settings_form.set_focus(field);
                self.apply_settings_commit(commit);
                self.adjust_settings_focus(delta);
            }
            FormMouseIntent::Text(field, area, column) => {
                let value = self.settings.buffer_text(field);
                let commit = self
                    .settings_form
                    .focus_text_at(field, &value, area, column, true);
                self.apply_settings_commit(commit);
            }
            FormMouseIntent::PickerItem(field, item_index) => match field {
                SettingsFocus::CaptureDevice => {
                    if self
                        .audio_input_picker
                        .selector
                        .select_item_index(item_index)
                    {
                        self.confirm_audio_input_picker();
                    }
                }
                SettingsFocus::PlaybackDevice => {
                    if self
                        .audio_output_picker
                        .selector
                        .select_item_index(item_index)
                    {
                        self.confirm_audio_output_picker();
                    }
                }
                _ => {}
            },
        }
        if event.action == FormAction::Scrolled {
            self.sync_focus();
        }
        Action::Continue
    }

    fn handle_open_settings_picker_mouse(&mut self, mouse: MouseEvent) -> bool {
        let delta = match mouse.kind {
            MouseEventKind::ScrollDown => 1,
            MouseEventKind::ScrollUp => -1,
            _ => return false,
        };
        match self.settings_form.focus() {
            SettingsFocus::CaptureDevice if self.audio_input_picker.open => {
                self.audio_input_picker.move_selection(delta);
                true
            }
            SettingsFocus::PlaybackDevice if self.audio_output_picker.open => {
                self.audio_output_picker.move_selection(delta);
                true
            }
            _ => false,
        }
    }

    fn process_server_edit_mouse(&mut self, mouse: MouseEvent) -> Action {
        let Some(draft) = self.server_edit.as_mut() else {
            return Action::Continue;
        };
        match draft.handle_mouse(mouse) {
            ServerEditEvent::Consumed => self.sync_focus(),
            ServerEditEvent::Cancel => {
                self.server_edit = None;
                self.open_server_select();
            }
            ServerEditEvent::Save { join_after_save } => self.save_server_edit(join_after_save),
        }
        Action::Continue
    }

    fn handle_open_settings_picker_key(&mut self, key: KeyEvent) -> bool {
        let focus = self.settings_form.focus();
        match focus {
            SettingsFocus::CaptureDevice if self.audio_input_picker.open => {
                if !self.audio_input_picker.searching {
                    match key.code {
                        KeyCode::Esc => {
                            self.cancel_audio_input_picker();
                            return true;
                        }
                        KeyCode::Enter => {
                            self.confirm_audio_input_picker();
                            return true;
                        }
                        _ => {}
                    }
                }
                handle_audio_picker_key(key, &mut self.audio_input_picker, &self.audio_input_items)
            }
            SettingsFocus::PlaybackDevice if self.audio_output_picker.open => {
                if !self.audio_output_picker.searching {
                    match key.code {
                        KeyCode::Esc => {
                            self.cancel_audio_output_picker();
                            return true;
                        }
                        KeyCode::Enter => {
                            self.confirm_audio_output_picker();
                            return true;
                        }
                        _ => {}
                    }
                }
                handle_audio_picker_key(
                    key,
                    &mut self.audio_output_picker,
                    &self.audio_output_items,
                )
            }
            _ => false,
        }
    }

    pub(crate) fn process_command(&mut self, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            EnterCompose => self.enter_compose_insert_mode(),
            EnterLog => self.set_chat_panel_focus(ChatPanelFocus::ChatLog),
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
                    self.enter_compose_insert_mode();
                } else {
                    self.enter_compose_insert_mode();
                }
            }
            Quit => return Action::Quit,
            ScrollUp => self.scroll_focused_panel(-1, 1),
            ScrollDown => self.scroll_focused_panel(1, 1),
            RoomScrollUp => self.move_room_selection_with_focus(-1),
            RoomScrollDown => self.move_room_selection_with_focus(1),
            OpenSelectedUserVolume => {
                if self.chat_focus == ChatPanelFocus::Lobby {
                    self.open_selected_user_volume();
                } else {
                    self.set_status("focus lobby to adjust users");
                }
            }
            ToggleSelectedUserMute => {
                if self.chat_focus == ChatPanelFocus::Lobby {
                    self.toggle_selected_user_mute();
                } else {
                    self.set_status("focus lobby to mute users");
                }
            }
            HalfPageUp => self.scroll_chat_log_if_focused(-(self.chat_half_page_rows() as isize)),
            HalfPageDown => self.scroll_chat_log_if_focused(self.chat_half_page_rows() as isize),
            Top => {
                if self.chat_focus == ChatPanelFocus::ChatLog {
                    self.chat.top(self.last_chat_width, self.last_chat_height);
                    self.chat.select_first_header();
                    self.chat.clear_selection();
                    self.keep_selected_chat_header_visible();
                }
            }
            Bottom => {
                if self.chat_focus == ChatPanelFocus::ChatLog {
                    self.chat.bottom();
                    self.chat.select_last_header(self.last_chat_width);
                    self.chat.clear_selection();
                    self.keep_selected_chat_header_visible();
                }
            }
            CopySelection => {
                if self.chat_focus == ChatPanelFocus::ChatLog {
                    self.copy_chat_selection();
                }
            }
            ToggleExpand => {
                if self.chat_focus == ChatPanelFocus::ChatLog {
                    self.toggle_selected_log_collapse();
                }
            }
            ToggleMute => self.toggle_mute(),
            ToggleDeafen => self.set_deafen(!self.deafened.load(Ordering::Relaxed)),
            RefreshDevices => self.refresh_audio_devices(),
            SaveSettings => self.save_settings(),
            Activate => self.activate_settings_focus(),
            FocusNext => {
                if self.mode == theme::UiMode::Settings {
                    self.move_settings_focus(1);
                } else {
                    self.move_chat_panel_focus(1);
                }
            }
            FocusPrev => {
                if self.mode == theme::UiMode::Settings {
                    self.move_settings_focus(-1);
                } else {
                    self.move_chat_panel_focus(-1);
                }
            }
            SelectNext => {
                if self.mode == theme::UiMode::Settings {
                    self.move_settings_selection(1);
                } else if self.chat_focus == ChatPanelFocus::Lobby {
                    self.move_room_selection_with_focus(1);
                }
            }
            SelectPrev => {
                if self.mode == theme::UiMode::Settings {
                    self.move_settings_selection(-1);
                } else if self.chat_focus == ChatPanelFocus::Lobby {
                    self.move_room_selection_with_focus(-1);
                }
            }
            AdjustLeft => self.adjust_settings_focus(-1),
            AdjustRight => self.adjust_settings_focus(1),
            ClearChat => {
                if self.chat_focus == ChatPanelFocus::ChatLog {
                    self.chat.clear();
                }
            }
            PlaySoundboard1 => self.trigger_soundboard_slot(0),
            PlaySoundboard2 => self.trigger_soundboard_slot(1),
            PlaySoundboard3 => self.trigger_soundboard_slot(2),
            PlaySoundboard4 => self.trigger_soundboard_slot(3),
            PlaySoundboard5 => self.trigger_soundboard_slot(4),
            PlaySoundboard6 => self.trigger_soundboard_slot(5),
            PlaySoundboard7 => self.trigger_soundboard_slot(6),
            PlaySoundboard8 => self.trigger_soundboard_slot(7),
            PlaySoundboard9 => self.trigger_soundboard_slot(8),
            ToggleKeyPreview => self.key_preview_expanded = !self.key_preview_expanded,
        }
        Action::Continue
    }

    pub(crate) fn open_settings(&mut self) {
        self.set_mode(theme::UiMode::Settings);
        self.settings = SettingsDraft::from_audio(&self.config.audio);
        self.settings
            .set_form_bindings_from_config(self.config.ui.form_bindings);
        self.settings.set_theme_from_config(self.config.ui.theme);
        self.settings_form = FormState::with_order(
            SettingsFocus::CaptureDevice,
            self.config.ui.form_bindings,
            SettingsFocus::ORDER,
        );
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
        self.commit_settings_form_text();
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        self.stop_settings_preview_capture();
        self.audio_input_picker
            .reset(&self.audio_input_items, self.settings.input_selection());
        self.audio_output_picker
            .reset(&self.audio_output_items, self.settings.output_selection());
        self.enter_compose_insert_mode();
    }

    fn move_settings_focus(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        if self.audio_picker_open() {
            self.move_active_audio_picker_selection(delta);
            return;
        }
        let commit = self.settings_form.move_focus(delta);
        self.apply_settings_commit(commit);
        self.sync_focus();
    }

    fn adjust_settings_focus(&mut self, delta: isize) {
        if self.mode != theme::UiMode::Settings {
            return;
        }
        match self.settings_form.focus() {
            SettingsFocus::CaptureDevice if self.settings.input_raw() => {}
            SettingsFocus::CaptureDevice if delta < 0 => self.cancel_audio_input_picker(),
            SettingsFocus::CaptureDevice => self.activate_audio_input_picker(),
            SettingsFocus::PlaybackDevice if self.settings.output_raw() => {}
            SettingsFocus::PlaybackDevice if delta < 0 => self.cancel_audio_output_picker(),
            SettingsFocus::PlaybackDevice => self.activate_audio_output_picker(),
            SettingsFocus::RawCaptureDevice
            | SettingsFocus::RawPlaybackDevice
            | SettingsFocus::Bitrate
            | SettingsFocus::Denoise
            | SettingsFocus::EchoCancellation
            | SettingsFocus::Amplification
            | SettingsFocus::Suppression
            | SettingsFocus::Release
            | SettingsFocus::TypingSuppression
            | SettingsFocus::TypingVadEnter
            | SettingsFocus::TypingVadRelease
            | SettingsFocus::FormBindings
            | SettingsFocus::Theme => {
                let mutation = self.settings.adjust(self.settings_form.focus(), delta);
                self.apply_settings_mutation(mutation);
            }
            SettingsFocus::CaptureBuffer
            | SettingsFocus::PlaybackBuffer
            | SettingsFocus::Refresh
            | SettingsFocus::Save
            | SettingsFocus::Close => {}
        }
    }

    fn activate_settings_focus(&mut self) {
        match self.settings_form.focus() {
            SettingsFocus::Refresh => self.refresh_audio_devices(),
            SettingsFocus::Save => self.save_settings(),
            SettingsFocus::Close => self.close_settings(),
            SettingsFocus::CaptureDevice if self.settings.input_raw() => {
                self.move_settings_focus(1)
            }
            SettingsFocus::CaptureDevice => self.activate_audio_input_picker(),
            SettingsFocus::PlaybackDevice if self.settings.output_raw() => {
                self.move_settings_focus(1)
            }
            SettingsFocus::PlaybackDevice => self.activate_audio_output_picker(),
            SettingsFocus::Denoise
            | SettingsFocus::EchoCancellation
            | SettingsFocus::RawCaptureDevice
            | SettingsFocus::RawPlaybackDevice
            | SettingsFocus::Bitrate
            | SettingsFocus::Amplification
            | SettingsFocus::Suppression
            | SettingsFocus::Release
            | SettingsFocus::TypingSuppression
            | SettingsFocus::TypingVadEnter
            | SettingsFocus::TypingVadRelease
            | SettingsFocus::FormBindings
            | SettingsFocus::Theme => {
                let mutation = self.settings.activate(self.settings_form.focus());
                self.apply_settings_mutation(mutation);
            }
            SettingsFocus::CaptureBuffer | SettingsFocus::PlaybackBuffer => {
                self.move_settings_focus(1);
            }
        }
    }

    fn apply_settings_mutation(&mut self, mutation: SettingsMutation) {
        match mutation {
            SettingsMutation::None => return,
            SettingsMutation::Changed => self.apply_settings_form_bindings(),
            SettingsMutation::AmplificationChanged(_) => {}
        }
        self.sync_settings_change();
    }

    /// Syncs the settings draft into the live config and applies it to running
    /// audio. Cheap fields (amplification, echo cancellation) update in place.
    /// Slow fields (device, bitrate, denoise, buffer, latency) schedule a
    /// debounced stream restart. The on-disk file is only written by `Save`.
    fn sync_settings_change(&mut self) {
        self.config.ui.form_bindings = self.settings.form_bindings();
        self.apply_theme(self.settings.theme());
        // Never open a malformed ALSA string. Hold the audio config at its last
        // valid state until the device string is fixed, then the diff below
        // re-applies every pending change.
        if let Some(reason) = self.settings.device_string_invalid() {
            self.mark_settings_dirty();
            self.set_status(format!("audio not applied: {reason}"));
            return;
        }
        let old = self.config.audio.clone();
        self.config.audio = self.settings.to_audio();
        self.apply_echo_cancellation_setting();
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        let (capture, playback) = audio_restart_flags(&old, &self.config.audio);
        if capture || playback {
            self.schedule_audio_apply(capture, playback);
        }
        self.mark_settings_dirty();
    }

    /// Re-resolves the active theme and applies it to the live UI: the chat
    /// buffer restyles its syntax highlighting and the composer editor adopts
    /// the new selection colors. Every other surface reads `self.theme` per
    /// frame, so a field swap is enough for them.
    fn apply_theme(&mut self, choice: ThemeChoice) {
        if self.config.ui.theme == choice {
            return;
        }
        self.config.ui.theme = choice;
        self.theme = Theme::from_choice(choice);
        self.chat.set_syntax(self.theme.syntax);
        self.composer.set_theme(self.theme.editor_theme());
    }

    fn schedule_audio_apply(&mut self, capture: bool, playback: bool) {
        let deadline = Instant::now() + AUDIO_APPLY_DEBOUNCE;
        match &mut self.pending_audio_apply {
            Some(pending) => {
                pending.capture |= capture;
                pending.playback |= playback;
                pending.deadline = deadline;
            }
            None => {
                self.pending_audio_apply = Some(PendingAudioApply {
                    capture,
                    playback,
                    deadline,
                })
            }
        }
    }

    /// Fires a debounced audio restart once its window elapses. Called once per
    /// run-loop iteration from [`crate::runtime`].
    pub(crate) fn tick(&mut self) {
        self.supervise(Instant::now());
        self.apply_pending_audio_restart();
    }

    fn apply_pending_audio_restart(&mut self) {
        let Some(pending) = &self.pending_audio_apply else {
            return;
        };
        if Instant::now() < pending.deadline {
            return;
        }
        let Some(PendingAudioApply {
            capture, playback, ..
        }) = self.pending_audio_apply.take()
        else {
            return;
        };
        let mut applied = Vec::new();
        if capture && self.capture.is_some() {
            self.restart_capture_stream();
            applied.push("capture");
        }
        if playback && self.playback.is_some() {
            self.restart_playback_stream();
            applied.push("playback");
        }
        if !applied.is_empty() {
            self.set_status(format!("audio settings applied ({})", applied.join(", ")));
        }
    }

    fn supervise(&mut self, now: Instant) {
        self.supervise_network(now);
        self.supervise_control_socket(now);
        self.supervise_capture(now);
        self.supervise_playback(now);
    }

    fn supervise_network(&mut self, now: Instant) {
        if self
            .network
            .as_ref()
            .is_some_and(NetworkClient::is_worker_finished)
            && !self.supervisor.network.is_pending()
        {
            // First detection of a silently-dead worker. Tear down audio bound
            // to its closed command channel and match the WorkerStopped event
            // path, so a muted capture stream cannot keep a stale sender alive
            // until the restart fires. The `is_pending` guard keeps this from
            // re-running every tick while recovery is already scheduled.
            self.stop_audio();
            self.stream_users.clear();
            self.schedule_network_recovery(now, "network worker stopped");
        }
        if let Some(reason) = self.supervisor.network.take_due(now) {
            self.restart_network_worker(&reason);
        }
    }

    fn supervise_control_socket(&mut self, now: Instant) {
        if self.network.is_none() {
            self.supervisor.control_socket.reset();
            return;
        }
        if self
            .control_socket
            .as_ref()
            .is_some_and(local_control::ControlSocket::is_finished)
        {
            self.schedule_control_socket_recovery(now, "control socket worker stopped");
        }
        if let Some(reason) = self.supervisor.control_socket.take_due(now) {
            self.restart_control_socket(&reason);
        }
    }

    fn supervise_capture(&mut self, now: Instant) {
        let Some(capture) = &self.capture else {
            self.supervisor.capture_watch = CaptureWatch::default();
            return;
        };
        let snapshot = capture.stats().snapshot();
        let mut reason = None;
        if snapshot.stream_errors > self.supervisor.capture_watch.stream_errors {
            reason = Some(
                snapshot
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "capture stream error".to_string()),
            );
        }
        if snapshot.worker_stopped && !self.supervisor.capture_watch.worker_stopped {
            reason = Some("capture worker stopped".to_string());
        }
        if capture.worker_finished() {
            reason = Some("capture worker exited".to_string());
        }

        let progressed = snapshot.callbacks != self.supervisor.capture_watch.callbacks
            || snapshot.captured_samples != self.supervisor.capture_watch.captured_samples;
        if progressed || self.supervisor.capture_watch.last_progress_at.is_none() {
            self.supervisor.capture_watch.last_progress_at = Some(now);
        } else if self.capture_should_be_live()
            && self
                .supervisor
                .capture_watch
                .last_progress_at
                .is_some_and(|last| now.saturating_duration_since(last) >= CAPTURE_STALL_TIMEOUT)
        {
            reason = Some("capture stream stopped delivering audio".to_string());
        }

        self.supervisor.capture_watch.callbacks = snapshot.callbacks;
        self.supervisor.capture_watch.captured_samples = snapshot.captured_samples;
        self.supervisor.capture_watch.stream_errors = snapshot.stream_errors;
        self.supervisor.capture_watch.worker_stopped = snapshot.worker_stopped;

        if let Some(reason) = reason {
            self.schedule_capture_recovery(now, reason);
        }
        if let Some(reason) = self.supervisor.capture.take_due(now) {
            self.recover_capture_stream(&reason);
        }
    }

    fn supervise_playback(&mut self, now: Instant) {
        let Some(playback) = &self.playback else {
            self.supervisor.playback_watch = PlaybackWatch::default();
            return;
        };
        let snapshot = playback.stats();
        let mut reason = None;
        if snapshot.backend_stream_errors > self.supervisor.playback_watch.backend_stream_errors {
            reason = Some(
                snapshot
                    .last_backend_error
                    .clone()
                    .unwrap_or_else(|| "playback stream error".to_string()),
            );
        }
        if playback.worker_finished() {
            reason = Some("playback decoder worker exited".to_string());
        }
        self.supervisor.playback_watch.backend_stream_errors = snapshot.backend_stream_errors;

        if let Some(reason) = reason {
            self.schedule_playback_recovery(now, reason);
        }
        if let Some(reason) = self.supervisor.playback.take_due(now) {
            self.recover_playback_stream(&reason);
        }
    }

    fn capture_should_be_live(&self) -> bool {
        self.capture.is_some()
            && (self.settings_preview_capture || self.voice_tx_enabled.load(Ordering::Relaxed))
    }

    fn schedule_network_recovery(&mut self, now: Instant, reason: impl Into<String>) {
        let reason = reason.into();
        match self.supervisor.network.schedule(now, reason.clone()) {
            RecoverySchedule::Scheduled(delay) => {
                if delay.is_zero() {
                    self.set_status("network worker stopped; reconnecting");
                } else {
                    self.set_status(format!(
                        "network worker stopped; retrying in {}s",
                        delay.as_secs()
                    ));
                }
            }
            RecoverySchedule::Pending => {}
            RecoverySchedule::Exhausted => {
                self.stop_audio();
                self.control_socket.take();
                if let Some(network) = self.network.take() {
                    network.stop();
                }
                self.stream_users.clear();
                self.set_error(format!("network recovery exhausted: {reason}"));
            }
        }
    }

    fn schedule_control_socket_recovery(&mut self, now: Instant, reason: impl Into<String>) {
        let reason = reason.into();
        match self.supervisor.control_socket.schedule(now, reason.clone()) {
            RecoverySchedule::Scheduled(delay) => {
                if !delay.is_zero() {
                    self.set_status(format!(
                        "file-upload socket down; retrying in {}s",
                        delay.as_secs()
                    ));
                }
            }
            RecoverySchedule::Pending => {}
            RecoverySchedule::Exhausted => {
                self.control_socket.take();
                self.set_error(format!("file-upload socket down: {reason}"));
            }
        }
    }

    fn schedule_capture_recovery(&mut self, now: Instant, reason: impl Into<String>) {
        let reason = reason.into();
        match self.supervisor.capture.schedule(now, reason.clone()) {
            RecoverySchedule::Scheduled(delay) => {
                if !delay.is_zero() {
                    self.set_status(format!(
                        "microphone recovery retrying in {}s",
                        delay.as_secs()
                    ));
                }
            }
            RecoverySchedule::Pending => {}
            RecoverySchedule::Exhausted => {
                self.mic_error = Some(reason.clone());
                self.set_error(format!("mic unavailable: {reason}"));
            }
        }
    }

    fn schedule_playback_recovery(&mut self, now: Instant, reason: impl Into<String>) {
        let reason = reason.into();
        match self.supervisor.playback.schedule(now, reason.clone()) {
            RecoverySchedule::Scheduled(delay) => {
                if !delay.is_zero() {
                    self.set_status(format!(
                        "playback recovery retrying in {}s",
                        delay.as_secs()
                    ));
                }
            }
            RecoverySchedule::Pending => {}
            RecoverySchedule::Exhausted => {
                self.playback_error = Some(reason.clone());
                self.set_error(format!("playback unavailable: {reason}"));
            }
        }
    }

    fn restart_network_worker(&mut self, reason: &str) {
        let alias = self.server_alias.clone();
        if alias.is_empty() {
            self.set_error(format!("network worker stopped: {reason}"));
            return;
        }
        kvlog::warn!("restarting network worker", reason);
        self.push_network_notice(
            "network",
            &format!("Network worker stopped: {reason}; reconnecting"),
        );
        let queued = std::mem::take(&mut self.pending_network_commands);
        let network_recovery = std::mem::take(&mut self.supervisor.network);
        let control_recovery = std::mem::take(&mut self.supervisor.control_socket);
        self.start_network(&alias);
        self.pending_network_commands = queued;
        if self.network.is_some() {
            self.supervisor.network.reset();
        } else {
            self.supervisor.network = network_recovery;
            self.supervisor.control_socket = control_recovery;
            self.schedule_network_recovery(
                Instant::now(),
                format!("failed to restart network worker after {reason}"),
            );
        }
    }

    fn restart_control_socket(&mut self, reason: &str) {
        kvlog::warn!("restarting local control socket", reason);
        self.control_socket.take();
        let Some(network) = &self.network else {
            self.supervisor.control_socket.reset();
            return;
        };
        match local_control::ControlSocket::spawn(network.sender()) {
            Ok(socket) => {
                kvlog::info!(
                    "chatt local control socket recovered",
                    path = %socket.path().display()
                );
                self.control_socket = Some(socket);
                self.supervisor.control_socket.reset();
                self.set_status("file-upload socket recovered");
            }
            Err(error) => {
                self.push_network_notice("control", &error);
                self.set_error(format!("file-upload socket unavailable: {error}"));
                self.schedule_control_socket_recovery(Instant::now(), error);
            }
        }
    }

    fn recover_capture_stream(&mut self, reason: &str) {
        kvlog::warn!("recovering capture stream", reason);
        match self.restart_capture_stream_inner() {
            Ok(restarted) => {
                self.supervisor.capture.reset();
                self.supervisor.capture_watch = CaptureWatch::default();
                if restarted {
                    self.set_status("microphone recovered");
                }
            }
            Err(error) => {
                self.mic_error = Some(error.clone());
                self.set_error(format!("mic unavailable: {error}"));
                self.schedule_capture_recovery(Instant::now(), error);
            }
        }
    }

    fn recover_playback_stream(&mut self, reason: &str) {
        kvlog::warn!("recovering playback stream", reason);
        self.restart_playback_stream();
        if self.playback.is_some() {
            self.supervisor.playback.reset();
            self.supervisor.playback_watch = PlaybackWatch::default();
            self.set_status("playback recovered");
        } else {
            let error = self
                .playback_error
                .clone()
                .unwrap_or_else(|| reason.to_string());
            self.schedule_playback_recovery(Instant::now(), error);
        }
    }

    fn restart_capture_stream(&mut self) {
        if let Err(error) = self.restart_capture_stream_inner() {
            self.set_error(format!("failed to restart capture: {error}"));
        }
    }

    fn restart_capture_stream_inner(&mut self) -> Result<bool, String> {
        let was_preview = self.settings_preview_capture;
        let in_call = self.voice_tx_enabled.load(Ordering::Relaxed);
        self.stop_mic_capture();
        if in_call {
            self.ensure_mic_capture()?;
            Ok(true)
        } else if was_preview {
            self.start_settings_preview_capture_inner()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn restart_playback_stream(&mut self) {
        if self.network.is_none() {
            return;
        }
        self.set_network_playback_sink(None);
        self.playback.take();
        self.start_playback_stream(true);
    }

    fn apply_settings_commit(&mut self, commit: Option<(SettingsFocus, String)>) {
        let Some((field, text)) = commit else {
            return;
        };
        let mutation = self.settings.commit_field_text(field, text);
        self.apply_settings_mutation(mutation);
    }

    fn commit_settings_form_text(&mut self) {
        let commit = self.settings_form.clear_text();
        self.apply_settings_commit(commit);
    }

    fn apply_settings_form_bindings(&mut self) {
        let commit = self
            .settings_form
            .set_bindings(self.settings.form_bindings());
        self.apply_settings_commit(commit);
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
        match self.settings_form.focus() {
            SettingsFocus::CaptureDevice if self.audio_input_picker.open => {
                self.audio_input_picker.move_selection(delta);
            }
            SettingsFocus::PlaybackDevice if self.audio_output_picker.open => {
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
            self.audio_input_picker
                .open(&self.audio_input_items, self.settings.input_selection());
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
            self.audio_output_picker
                .open(&self.audio_output_items, self.settings.output_selection());
            self.sync_focus();
        }
    }

    fn confirm_audio_input_picker(&mut self) {
        let Some(next) = self.audio_input_picker.confirm(&self.audio_input_items) else {
            return;
        };
        if self.settings.set_input_selection(next) {
            self.mark_settings_dirty();
        }
        self.sync_focus();
    }

    fn cancel_audio_input_picker(&mut self) {
        if let Some(selection) = self.audio_input_picker.cancel(&self.audio_input_items) {
            self.settings.restore_input_selection(selection);
        }
        self.sync_focus();
    }

    fn confirm_audio_output_picker(&mut self) {
        let Some(next) = self.audio_output_picker.confirm(&self.audio_output_items) else {
            return;
        };
        if self.settings.set_output_selection(next) {
            self.mark_settings_dirty();
        }
        self.sync_focus();
    }

    fn cancel_audio_output_picker(&mut self) {
        if let Some(selection) = self.audio_output_picker.cancel(&self.audio_output_items) {
            self.settings.restore_output_selection(selection);
        }
        self.sync_focus();
    }

    fn rebuild_audio_device_pickers(&mut self) {
        self.audio_input_items = settings::audio_input_items(&self.input_devices);
        if self.audio_input_picker.open {
            self.audio_input_picker
                .refresh_items(&self.audio_input_items, self.settings.input_selection());
        } else {
            self.audio_input_picker
                .reset(&self.audio_input_items, self.settings.input_selection());
        }
        self.audio_output_items = settings::audio_output_items(&self.output_devices);
        if self.audio_output_picker.open {
            self.audio_output_picker
                .refresh_items(&self.audio_output_items, self.settings.output_selection());
        } else {
            self.audio_output_picker
                .reset(&self.audio_output_items, self.settings.output_selection());
        }
    }

    fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.set_status("settings draft changed; save config when ready");
    }

    fn scroll_focused_panel(&mut self, direction: isize, _rows: usize) {
        match self.chat_focus {
            ChatPanelFocus::ChatLog => self.move_chat_log_selection(direction),
            ChatPanelFocus::Lobby => self.move_room_selection_with_focus(direction),
            ChatPanelFocus::Compose => {}
        }
    }

    fn scroll_chat_log_if_focused(&mut self, rows: isize) {
        if self.chat_focus != ChatPanelFocus::ChatLog {
            return;
        }
        if rows < 0 {
            self.scroll_chat_up(rows.unsigned_abs());
        } else {
            self.chat.scroll_down(rows as usize);
        }
    }

    fn chat_half_page_rows(&self) -> usize {
        (self.last_chat_height as usize / 2).max(1)
    }

    fn move_chat_log_selection(&mut self, delta: isize) {
        self.set_chat_panel_focus(ChatPanelFocus::ChatLog);
        self.chat.clear_selection();
        if self
            .chat
            .move_selected_header(delta, self.last_chat_width)
            .is_none()
        {
            self.set_status("no messages");
            return;
        }
        self.keep_selected_chat_header_visible();
    }

    fn keep_selected_chat_header_visible(&mut self) {
        self.chat
            .keep_selected_header_visible(self.last_chat_width, self.last_chat_height);
    }

    fn move_room_selection_with_focus(&mut self, delta: isize) {
        self.set_chat_panel_focus(ChatPanelFocus::Lobby);
        self.move_room_selection(delta);
    }

    fn move_room_selection(&mut self, delta: isize) {
        if self.participants.move_selection(delta).is_none() {
            self.set_status("no users in the current room yet");
            return;
        }
        self.keep_selected_room_user_visible();
        self.chat_focus = ChatPanelFocus::Lobby;
        self.refresh_mode_and_focus();
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
        self.volume_dialog = Some(UserVolumeDialog::new(
            user_id,
            name.clone(),
            value_db,
            &self.theme,
        ));
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
        match dialog.handle_key(key) {
            UserVolumeEvent::Consumed => {
                self.volume_dialog = Some(dialog);
            }
            UserVolumeEvent::Preview { user_id, value_db } => {
                self.apply_user_audio_control_with_volume(user_id, value_db);
                self.volume_dialog = Some(dialog);
            }
            UserVolumeEvent::Invalid(error) => {
                self.set_error(error);
                self.volume_dialog = Some(dialog);
            }
            UserVolumeEvent::Cancel {
                user_id,
                user_name,
                original_db,
            } => {
                self.config
                    .set_user_volume_db(&self.server_alias, user_id.0, original_db);
                self.apply_user_audio_control_with_volume(user_id, original_db);
                self.focus.pop_modal(FocusId::Participants);
                self.modes.pop_or(ModeKind::from(self.mode));
                self.set_status(format!("canceled local volume for {user_name}"));
            }
            UserVolumeEvent::Save {
                user_id,
                user_name,
                value_db,
            } => {
                self.config
                    .set_user_volume_db(&self.server_alias, user_id.0, value_db);
                self.apply_user_audio_control_with_volume(user_id, value_db);
                match self.config.save_runtime() {
                    Ok(path) => {
                        self.config.config_path = Some(path.clone());
                        self.focus.pop_modal(FocusId::Participants);
                        self.modes.pop_or(ModeKind::from(self.mode));
                        self.set_status(format!(
                            "saved local volume {}dB for {} to {}",
                            format_signed_db(value_db),
                            user_name,
                            path.display()
                        ));
                    }
                    Err(error) => {
                        dialog.mark_save_error(error.clone());
                        self.volume_dialog = Some(dialog);
                        self.set_error(error);
                    }
                }
            }
        }
        true
    }

    pub(crate) fn effective_user_volume_db(&self, user_id: UserId) -> f32 {
        if let Some(value_db) = self
            .volume_dialog
            .as_ref()
            .and_then(|dialog| dialog.preview_for(user_id))
        {
            return value_db;
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
        // Edits already applied live; this captures any uncommitted buffer field
        // then persists the live config to disk.
        self.commit_settings_form_text();
        self.sync_settings_change();
        if let Some(reason) = self.settings.device_string_invalid() {
            self.set_error(format!("not saved: {reason}"));
            return;
        }
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.settings_dirty = false;
                self.chat
                    .set_max_messages(self.config.ui.max_messages as usize);
                self.set_status(format!("settings saved to {}", path.display()));
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
        self.enter_compose_insert_mode();
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
            body => {
                if self.network.is_some() {
                    self.send_network_command(NetworkCommand::SendChat(body.to_string()), true);
                } else {
                    self.set_error("select a server before sending messages");
                }
            }
        }
    }

    fn upload_file_command(&mut self, command: &str) {
        let path = command.trim_start_matches("/upload ").trim();
        if path.is_empty() {
            self.set_error("usage: /upload file_path/filename.ext");
            return;
        }
        if self.network.is_some() {
            self.send_network_command(
                NetworkCommand::UploadFile(std::path::PathBuf::from(path)),
                true,
            );
            self.set_status(format!("queued upload {}", path));
        } else {
            self.set_error("select a server before uploading files");
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

    fn toggle_mute(&mut self) {
        if self.deafened.load(Ordering::Relaxed) {
            self.mic_muted.store(false, Ordering::Relaxed);
            self.set_deafen(false);
        } else {
            self.set_mute(!self.mic_muted.load(Ordering::Relaxed));
        }
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
        let diagnostics = AudioDiagnostics::new(
            playback.stats(),
            self.encoder_profile,
            self.voice_packets_received,
            self.voice_bytes_received,
        );
        self.chat.push_notice("audio", diagnostics.notice_body());
        self.chat.bottom();
        self.set_status(diagnostics.status_summary());
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
        let network_event_tx = self.event_tx.clone();
        let send_failed = Arc::new(AtomicBool::new(false));
        let busy = Arc::clone(&self.soundboard_busy);
        let source_config = LiveAudioFileSourceConfig {
            input_path,
            tuning: self.config.audio.latency.to_tuning(),
            packet_loss,
            seed: self.config.soundboard.seed.wrapping_add(slot as u64),
            first_sequence: self.soundboard_next_sequence,
            max_amplification: self.config.audio.max_amplification,
            denoise: self.config.audio.denoise.is_enabled(),
            auto_gain: true,
        };
        self.set_status(format!(
            "soundboard playing {} ({})",
            clip.name,
            packet_loss.as_name()
        ));
        thread::spawn(move || {
            let send_failed = Arc::clone(&send_failed);
            let result = audio::run_live_audio_file_source(source_config, |sequence, frame| {
                if network_tx
                    .send(NetworkCommand::SequencedLocalVoicePacket { sequence, frame })
                    .is_err()
                    && !send_failed.swap(true, Ordering::AcqRel)
                {
                    let _ = network_event_tx.send(NetworkEvent::WorkerStopped {
                        reason: "network command channel closed while sending soundboard audio"
                            .to_string(),
                    });
                }
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

    fn live_capture_config(&self, input_device_id: Option<String>) -> LiveCaptureConfig {
        LiveCaptureConfig {
            input_device_id,
            bitrate_bps: self.config.audio.bitrate_bps,
            denoise: self.config.audio.denoise,
            max_amplification: self.config.audio.max_amplification,
            suppression: self.config.audio.suppression(),
            typing_suppression: self.config.audio.typing_suppression(),
            buffer_request: self.input_buffer_request(),
            tuning: self.config.audio.latency.to_tuning(),
            echo_control: Some(Arc::clone(&self.echo_control)),
        }
    }

    fn capture_packet_handler(&self) -> impl FnMut(LocalVoiceFrame) + Send + 'static {
        let tx = self.network.as_ref().map(|network| network.sender());
        let event_tx = self.event_tx.clone();
        let send_failed = Arc::new(AtomicBool::new(false));
        let mic_muted = Arc::clone(&self.mic_muted);
        let deafened = Arc::clone(&self.deafened);
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        move |payload| {
            if mic_muted.load(Ordering::Relaxed)
                || deafened.load(Ordering::Relaxed)
                || !voice_tx_enabled.load(Ordering::Relaxed)
            {
                return;
            }
            if let Some(tx) = &tx
                && tx.send(NetworkCommand::LocalVoicePacket(payload)).is_err()
                && !send_failed.swap(true, Ordering::AcqRel)
            {
                let _ = event_tx.send(NetworkEvent::WorkerStopped {
                    reason: "network command channel closed while sending microphone audio"
                        .to_string(),
                });
            }
        }
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

        let configured_input = self.config.audio.input_device_id.clone();
        let capture = match audio::start_live_capture(
            self.live_capture_config(configured_input.clone()),
            self.capture_packet_handler(),
        ) {
            Ok(capture) => Ok(capture),
            Err(error) if configured_input.is_some() => {
                kvlog::warn!(
                    "configured input failed, trying default",
                    error = error.as_str()
                );
                self.push_network_notice(
                    "audio",
                    &format!("Input device failed; trying system default: {error}"),
                );
                audio::start_live_capture(
                    self.live_capture_config(None),
                    self.capture_packet_handler(),
                )
                .map_err(|fallback_error| {
                    format!("{error}; default input fallback failed: {fallback_error}")
                })
            }
            Err(error) => Err(error),
        };
        match capture {
            Ok(capture) => {
                self.capture = Some(capture);
                self.mic_error = None;
                self.supervisor.capture.reset();
                self.supervisor.capture_watch = CaptureWatch::default();
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
        if let Err(error) = self.start_settings_preview_capture_inner() {
            self.mic_error = Some(error);
        }
    }

    fn start_settings_preview_capture_inner(&mut self) -> Result<(), String> {
        if !self.allow_settings_preview_capture
            || self.capture.is_some()
            || self.voice_tx_enabled.load(Ordering::Relaxed)
            || self.deafened.load(Ordering::Relaxed)
        {
            return Ok(());
        }

        self.ensure_mic_capture()?;
        self.settings_preview_capture = true;
        Ok(())
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
            self.start_playback_stream(capture_ok);
        }
        self.voice_packets_received = 0;
        self.voice_bytes_received = 0;
    }

    /// Builds the live playback stream from the current `config.audio`, wires its
    /// feedback relay to the network, sets the playback sink, and re-applies
    /// per-user audio controls. `capture_ok` gates the "voice active" status so a
    /// failed capture start does not look successful.
    fn start_playback_stream(&mut self, capture_ok: bool) {
        let (feedback_tx, feedback_rx) = mpsc::channel::<LivePlaybackFeedback>();
        let Some(network) = &self.network else {
            self.set_error("select a server before starting playback");
            return;
        };
        let network_tx = network.sender();
        let event_tx = self.event_tx.clone();
        let send_failed = Arc::new(AtomicBool::new(false));
        thread::spawn(move || {
            for feedback in feedback_rx {
                if network_tx
                    .send(NetworkCommand::PlaybackFeedback(feedback))
                    .is_err()
                    && !send_failed.swap(true, Ordering::AcqRel)
                {
                    let _ = event_tx.send(NetworkEvent::WorkerStopped {
                        reason: "network command channel closed while sending playback feedback"
                            .to_string(),
                    });
                }
            }
        });
        let configured_output = self.config.audio.output_device_id.clone();
        let playback = match audio::start_live_playback(
            self.live_playback_config(configured_output.clone(), Some(feedback_tx.clone())),
        ) {
            Ok(playback) => Ok(playback),
            Err(error) if configured_output.is_some() => {
                kvlog::warn!(
                    "configured output failed, trying default",
                    error = error.as_str()
                );
                self.push_network_notice(
                    "audio",
                    &format!("Output device failed; trying system default: {error}"),
                );
                audio::start_live_playback(self.live_playback_config(None, Some(feedback_tx)))
                    .map_err(|fallback_error| {
                        format!("{error}; default output fallback failed: {fallback_error}")
                    })
            }
            Err(error) => Err(error),
        };
        match playback {
            Ok(playback) => {
                let fell_back = playback.buffer_fallback();
                let sink = playback.sink();
                self.playback = Some(playback);
                self.playback_error = None;
                self.supervisor.playback.reset();
                self.supervisor.playback_watch = PlaybackWatch::default();
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

    fn live_playback_config(
        &self,
        output_device_id: Option<String>,
        feedback_sender: Option<Sender<LivePlaybackFeedback>>,
    ) -> LivePlaybackConfig {
        LivePlaybackConfig {
            output_device_id,
            buffer_request: self.output_buffer_request(),
            tuning: self.config.audio.latency.to_tuning(),
            feedback_sender,
            echo_control: Some(Arc::clone(&self.echo_control)),
        }
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
        self.supervisor.capture.reset();
        self.supervisor.playback.reset();
        self.supervisor.capture_watch = CaptureWatch::default();
        self.supervisor.playback_watch = PlaybackWatch::default();
        if restart_settings_preview {
            self.start_settings_preview_capture();
        }
    }

    fn stop_mic_capture(&mut self) {
        self.settings_preview_capture = false;
        self.capture.take();
        self.supervisor.capture_watch = CaptureWatch::default();
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

    pub(crate) fn enter_compose_insert_mode(&mut self) {
        self.composer.enter_insert_mode();
        self.set_chat_panel_focus(ChatPanelFocus::Compose);
    }

    pub(crate) fn move_chat_panel_focus(&mut self, delta: isize) {
        self.set_chat_panel_focus(self.chat_focus.moved(delta));
    }

    pub(crate) fn set_chat_panel_focus(&mut self, focus: ChatPanelFocus) {
        self.chat_focus = focus;
        self.mode = match focus {
            ChatPanelFocus::Compose => theme::UiMode::Compose,
            ChatPanelFocus::Lobby | ChatPanelFocus::ChatLog => theme::UiMode::Log,
        };
        if focus == ChatPanelFocus::Lobby {
            self.keep_selected_room_user_visible();
        } else if focus == ChatPanelFocus::ChatLog {
            self.chat.ensure_selected_header(self.last_chat_width);
        }
        self.refresh_mode_and_focus();
    }

    pub(crate) fn refresh_mode_and_focus(&mut self) {
        self.modes.set(self.active_mode_kind());
        self.sync_focus();
    }

    fn active_mode_kind(&self) -> ModeKind {
        match self.mode {
            theme::UiMode::Compose if self.composer.mode() == EditorMode::Insert => {
                ModeKind::Insert
            }
            theme::UiMode::Compose | theme::UiMode::Log => ModeKind::Workspace,
            mode => ModeKind::from(mode),
        }
    }

    pub(crate) fn active_binding_layer(&self) -> Option<extui_bindings::LayerId> {
        if self.volume_dialog.is_some() {
            return Some(bindings::DIALOG_LAYER);
        }

        match self.mode {
            theme::UiMode::ServerSelect | theme::UiMode::ServerEdit => None,
            theme::UiMode::Settings => Some(bindings::SETTINGS_LAYER),
            theme::UiMode::Compose if self.chat_focus == ChatPanelFocus::Compose => {
                if self.composer.mode() == EditorMode::Insert {
                    Some(bindings::INSERT_LAYER)
                } else {
                    Some(bindings::COMPOSE_NORMAL_LAYER)
                }
            }
            theme::UiMode::Compose | theme::UiMode::Log => Some(bindings::WORKSPACE_LAYER),
        }
    }

    fn set_mode(&mut self, mode: theme::UiMode) {
        match mode {
            theme::UiMode::Compose => {
                self.chat_focus = ChatPanelFocus::Compose;
            }
            theme::UiMode::Log => {
                if self.chat_focus == ChatPanelFocus::Compose {
                    self.chat_focus = ChatPanelFocus::ChatLog;
                }
            }
            theme::UiMode::ServerSelect | theme::UiMode::ServerEdit | theme::UiMode::Settings => {}
        }
        self.mode = mode;
        self.refresh_mode_and_focus();
    }

    fn sync_focus(&mut self) {
        let focus = match self.mode {
            theme::UiMode::ServerSelect => FocusId::ServerList,
            theme::UiMode::ServerEdit => self
                .server_edit
                .as_ref()
                .map(|draft| FocusId::ServerField(server_field(draft.focus())))
                .unwrap_or(FocusId::ServerList),
            theme::UiMode::Compose | theme::UiMode::Log => self.chat_focus.focus_id(),
            theme::UiMode::Settings => {
                if self.audio_input_picker.open {
                    FocusId::InputPicker
                } else if self.audio_output_picker.open {
                    FocusId::OutputPicker
                } else {
                    FocusId::Settings(settings_field(self.settings_form.focus()))
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
        SettingsFocus::CaptureDevice => SettingsField::InputDevice,
        SettingsFocus::RawCaptureDevice => SettingsField::RawInputDevice,
        SettingsFocus::PlaybackDevice => SettingsField::OutputDevice,
        SettingsFocus::RawPlaybackDevice => SettingsField::RawOutputDevice,
        SettingsFocus::Bitrate => SettingsField::Bitrate,
        SettingsFocus::Denoise => SettingsField::Denoise,
        SettingsFocus::EchoCancellation => SettingsField::EchoCancellation,
        SettingsFocus::Amplification => SettingsField::Amplification,
        SettingsFocus::Suppression => SettingsField::Suppression,
        SettingsFocus::Release => SettingsField::Release,
        SettingsFocus::TypingSuppression => SettingsField::TypingSuppression,
        SettingsFocus::TypingVadEnter => SettingsField::TypingVadEnter,
        SettingsFocus::TypingVadRelease => SettingsField::TypingVadRelease,
        SettingsFocus::CaptureBuffer => SettingsField::InputBuffer,
        SettingsFocus::PlaybackBuffer => SettingsField::OutputBuffer,
        SettingsFocus::FormBindings => SettingsField::FormBindings,
        SettingsFocus::Theme => SettingsField::Theme,
        SettingsFocus::Refresh => SettingsField::Refresh,
        SettingsFocus::Save => SettingsField::Save,
        SettingsFocus::Close => SettingsField::Close,
    }
}

fn handle_audio_picker_key(
    key: KeyEvent,
    picker: &mut settings::AudioDevicePickerState,
    items: &[settings::AudioDeviceItem],
) -> bool {
    if picker.searching {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                picker.searching = false;
                return true;
            }
            _ => return picker.edit_search(key, items),
        }
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

    match key.code {
        KeyCode::Esc => {
            return true;
        }
        KeyCode::Enter => {
            return true;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            picker.move_selection(1);
            true
        }
        KeyCode::Up | KeyCode::Char('k') => {
            picker.move_selection(-1);
            true
        }
        _ if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('j')) =>
        {
            picker.move_selection(1);
            true
        }
        _ if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('k')) =>
        {
            picker.move_selection(-1);
            true
        }
        _ => false,
    }
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
        NetworkEvent::WorkerStopped { .. } => "worker_stopped",
        NetworkEvent::Disconnected => "disconnected",
    }
}

fn app_network_command_kind(command: &NetworkCommand) -> &'static str {
    match command {
        NetworkCommand::SendChat(_) => "send_chat",
        NetworkCommand::UploadFile(_) => "upload_file",
        NetworkCommand::LocalVoicePacket(_) => "local_voice_packet",
        NetworkCommand::SequencedLocalVoicePacket { .. } => "sequenced_local_voice_packet",
        NetworkCommand::SetPlaybackSink(_) => "set_playback_sink",
        NetworkCommand::PlaybackFeedback(_) => "playback_feedback",
        NetworkCommand::Shutdown => "shutdown",
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
    fn audio_restart_flags_isolate_capture_and_playback_fields() {
        let base = config::AudioConfig::default();

        let mut bitrate = base.clone();
        bitrate.bitrate_bps += 8_000;
        assert_eq!(audio_restart_flags(&base, &bitrate), (true, false));

        let mut denoise = base.clone();
        denoise.denoise = audio::DenoiseConfig::None;
        let denoise_changed = denoise.denoise != base.denoise;
        assert_eq!(audio_restart_flags(&base, &denoise).0, denoise_changed);

        let mut typing_suppression = base.clone();
        typing_suppression.denoise_typing_suppression = !base.denoise_typing_suppression;
        assert_eq!(
            audio_restart_flags(&base, &typing_suppression),
            (true, false)
        );

        let mut typing_threshold = base.clone();
        typing_threshold.denoise_typing_vad_enter = 0.75;
        assert_eq!(audio_restart_flags(&base, &typing_threshold), (true, false));

        let mut input_buffer = base.clone();
        input_buffer.input_buffer = config::BufferSize::Samples(480);
        assert_eq!(audio_restart_flags(&base, &input_buffer), (true, false));

        let mut output_buffer = base.clone();
        output_buffer.output_buffer = config::BufferSize::Samples(480);
        assert_eq!(audio_restart_flags(&base, &output_buffer), (false, true));

        let mut output_device = base.clone();
        output_device.output_device_id = Some("other".to_string());
        assert_eq!(audio_restart_flags(&base, &output_device), (false, true));

        let mut latency = base.clone();
        latency.latency.target_queue_ms += 10;
        assert_eq!(audio_restart_flags(&base, &latency), (true, true));
    }

    #[test]
    fn audio_restart_flags_ignore_cheap_live_fields() {
        let base = config::AudioConfig::default();

        let mut amplification = base.clone();
        amplification.max_amplification += 6.0;
        assert_eq!(audio_restart_flags(&base, &amplification), (false, false));

        let mut echo = base.clone();
        echo.echo_cancellation = !echo.echo_cancellation;
        assert_eq!(audio_restart_flags(&base, &echo), (false, false));
    }

    #[test]
    fn recovery_state_backs_off_and_exhausts_within_window() {
        let now = Instant::now();
        let mut recovery = RecoveryState::default();

        assert_eq!(
            recovery.schedule(now, "first"),
            RecoverySchedule::Scheduled(Duration::ZERO)
        );
        assert_eq!(recovery.take_due(now).as_deref(), Some("first"));
        assert_eq!(
            recovery.schedule(now + Duration::from_millis(1), "second"),
            RecoverySchedule::Scheduled(Duration::from_secs(1))
        );
        assert_eq!(
            recovery.schedule(now + Duration::from_millis(2), "ignored"),
            RecoverySchedule::Pending
        );
        assert_eq!(recovery.take_due(now + Duration::from_millis(500)), None);
        assert_eq!(
            recovery.take_due(now + Duration::from_secs(2)).as_deref(),
            Some("second")
        );
        assert_eq!(
            recovery.schedule(now + Duration::from_secs(3), "third"),
            RecoverySchedule::Scheduled(Duration::from_secs(2))
        );
        assert_eq!(
            recovery.take_due(now + Duration::from_secs(6)).as_deref(),
            Some("third")
        );
        assert_eq!(
            recovery.schedule(now + Duration::from_secs(7), "fourth"),
            RecoverySchedule::Exhausted
        );
    }

    #[test]
    fn failed_user_network_command_is_queued_for_recovery() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        drop(rx);
        app.network = Some(NetworkClient::from_parts_for_test(tx));

        let sent = app.send_network_command(NetworkCommand::SendChat("hello".to_string()), true);

        assert!(!sent);
        assert_eq!(app.pending_network_commands.len(), 1);
        assert!(matches!(
            app.pending_network_commands.front(),
            Some(NetworkCommand::SendChat(body)) if body == "hello"
        ));
        assert_eq!(app.status_kind, StatusKind::Error);
    }

    #[test]
    fn queued_user_network_commands_flush_when_worker_is_available() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.pending_network_commands
            .push_back(NetworkCommand::SendChat("hello".to_string()));

        app.flush_pending_network_commands();

        match rx.try_recv().unwrap() {
            NetworkCommand::SendChat(body) => assert_eq!(body, "hello"),
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(app.pending_network_commands.is_empty());
    }

    #[test]
    fn server_edit_reuses_one_editor_across_text_fields() {
        let mut draft = ServerEditDraft::from_server(
            &crate::config::ServerEntry::default(),
            crate::config::FormBindings::Standard,
        );
        let first_editor = draft.active_editor_address().unwrap();
        draft.set_active_editor_text("local-dev");

        draft.move_focus_for_test(1);

        let second_editor = draft.active_editor_address().unwrap();
        assert_eq!(first_editor, second_editor);
        draft.set_active_editor_text("Alice Dev");

        let server = draft.to_update().unwrap().server;
        assert_eq!(server.alias, "local-dev");
        assert_eq!(server.display_name, "Alice Dev");
    }

    #[test]
    fn settings_buffers_reuse_one_editor_and_commit_on_focus_change() {
        let mut draft = SettingsDraft::from_audio(&crate::config::AudioConfig::default());
        let mut form = FormState::with_order(
            SettingsFocus::CaptureBuffer,
            crate::config::FormBindings::Standard,
            SettingsFocus::ORDER,
        );
        form.focus_text(
            SettingsFocus::CaptureBuffer,
            &draft.buffer_text(SettingsFocus::CaptureBuffer),
            false,
        );
        let input_editor = form.editor_mut() as *mut _ as usize;
        form.editor_mut().set_lines("1024");

        let commit = form.set_focus(SettingsFocus::PlaybackBuffer);
        if let Some((field, text)) = commit {
            draft.set_buffer_text(field, text);
        }
        assert_eq!(draft.buffer_text(SettingsFocus::CaptureBuffer), "1024");

        form.focus_text(
            SettingsFocus::PlaybackBuffer,
            &draft.buffer_text(SettingsFocus::PlaybackBuffer),
            false,
        );
        let output_editor = form.editor_mut() as *mut _ as usize;
        assert_eq!(input_editor, output_editor);
    }

    #[test]
    fn mouse_wheel_moves_open_settings_device_picker() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Settings);
        app.audio_input_items = ["System default", "USB Mic", "Line In"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| settings::AudioDeviceItem {
                selection: Some(format!("device-{index}")),
                aliases: Vec::new(),
                backend_id: None,
                device_index: Some(index as u32),
                name: name.to_string(),
                search_text: name.to_string(),
                rank: 0,
                supported: true,
                preview: None,
                issue: None,
                variants: Vec::new(),
                default_source: "test",
            })
            .collect();
        app.audio_input_picker
            .open(&app.audio_input_items, app.settings.input_selection());

        assert_eq!(
            app.audio_input_picker.selector.current_item_index(),
            Some(0)
        );

        app.process_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: 4,
            modifiers: KeyModifiers::empty(),
        });

        assert_eq!(
            app.audio_input_picker.selector.current_item_index(),
            Some(1)
        );
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
    fn escape_leaves_compose_focused_in_vim_normal_mode() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);

        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::empty())),
            Action::Continue
        ));
        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            Action::Continue
        ));

        assert_eq!(app.chat_focus, ChatPanelFocus::Compose);
        assert_eq!(app.focus.active(), FocusId::Composer);
        assert_eq!(app.composer.mode(), EditorMode::Normal);
        assert_eq!(app.modes.top(), ModeKind::Workspace);

        assert!(matches!(
            app.process_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::empty())),
            Action::Continue
        ));
        assert_eq!(app.chat_focus, ChatPanelFocus::Compose);
        assert_eq!(app.composer.mode(), EditorMode::Insert);
        assert_eq!(app.modes.top(), ModeKind::Insert);
    }

    #[test]
    fn compose_normal_m_uses_binding_to_toggle_mute() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        app.process_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.composer.mode(), EditorMode::Normal);

        app.process_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty()));

        assert!(app.mic_muted.load(Ordering::Relaxed));
        assert_eq!(app.chat_focus, ChatPanelFocus::Compose);
        assert_eq!(app.composer.mode(), EditorMode::Normal);
    }

    #[test]
    fn compose_vim_text_object_commands_receive_i_key() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        app.composer.set_lines("alpha beta");
        app.composer.set_cursor_offset(2);

        app.process_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.composer.mode(), EditorMode::Normal);

        for key in ['c', 'i', 'w'] {
            app.process_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::empty()));
        }

        assert_eq!(app.chat_focus, ChatPanelFocus::Compose);
        assert_eq!(app.composer.mode(), EditorMode::Insert);
        assert_eq!(app.composer.text(), " beta");
    }

    #[test]
    fn shifted_jk_wraps_chat_panel_focus() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        app.process_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        app.process_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::empty()));
        assert_eq!(app.chat_focus, ChatPanelFocus::ChatLog);
        assert_eq!(app.focus.active(), FocusId::Chat);

        app.process_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::empty()));
        assert_eq!(app.chat_focus, ChatPanelFocus::Lobby);
        assert_eq!(app.focus.active(), FocusId::Participants);

        app.process_key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::empty()));
        assert_eq!(app.chat_focus, ChatPanelFocus::Compose);
        assert_eq!(app.focus.active(), FocusId::Composer);
        assert_eq!(app.composer.mode(), EditorMode::Normal);

        app.process_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::empty()));
        assert_eq!(app.chat_focus, ChatPanelFocus::Lobby);
    }

    #[test]
    fn super_jk_move_chat_panel_focus_from_compose_insert_mode() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        assert_eq!(app.composer.mode(), EditorMode::Insert);

        app.process_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::SUPER));
        assert_eq!(app.chat_focus, ChatPanelFocus::ChatLog);
        assert_eq!(app.focus.active(), FocusId::Chat);

        app.set_chat_panel_focus(ChatPanelFocus::Compose);
        assert_eq!(app.composer.mode(), EditorMode::Insert);

        app.process_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::SUPER));
        assert_eq!(app.chat_focus, ChatPanelFocus::Lobby);
        assert_eq!(app.focus.active(), FocusId::Participants);
    }

    #[test]
    fn selected_user_volume_requires_lobby_focus() {
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
        app.set_chat_panel_focus(ChatPanelFocus::ChatLog);

        app.process_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(app.volume_dialog.is_none());
        assert_eq!(app.status, "focus lobby to adjust users");

        app.set_chat_panel_focus(ChatPanelFocus::Lobby);
        app.process_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
        assert!(app.volume_dialog.is_some());
        assert_eq!(app.focus.active(), FocusId::Dialog);
    }

    #[test]
    fn toggle_mute_while_deafened_undeafens_and_unmutes() {
        let mut app = test_app();
        app.set_deafen(true);
        assert!(app.deafened.load(Ordering::Relaxed));
        assert!(app.mic_muted.load(Ordering::Relaxed));

        app.process_command(BindCommand::ToggleMute);

        assert!(!app.deafened.load(Ordering::Relaxed));
        assert!(!app.mic_muted.load(Ordering::Relaxed));
    }

    #[test]
    fn chat_log_jk_moves_selected_message() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        for index in 0..3 {
            app.push_chat(ChatMessage {
                message_id: rpc::ids::MessageId(index + 1),
                room_id: rpc::ids::RoomId(1),
                sender: UserId(index as u32 + 1),
                sender_name: format!("user{index}"),
                timestamp_ms: index * 120_000,
                body: format!("message {index}"),
            });
        }

        app.set_chat_panel_focus(ChatPanelFocus::ChatLog);
        assert_eq!(app.chat.selected_message(), Some(2));

        app.process_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()));
        assert_eq!(app.chat.selected_message(), Some(1));

        app.process_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::empty()));
        assert_eq!(app.chat.selected_message(), Some(2));
    }

    #[test]
    fn chat_log_gg_and_g_select_edge_headers() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        for index in 0..20 {
            app.push_chat(ChatMessage {
                message_id: rpc::ids::MessageId(index + 1),
                room_id: rpc::ids::RoomId(1),
                sender: UserId(index as u32 + 1),
                sender_name: format!("user{index}"),
                timestamp_ms: index * 120_000,
                body: format!("message {index}"),
            });
        }

        let mut buffer = Buffer::new(80, 12);
        crate::tui::render(&mut app, &mut buffer, 0);
        app.set_chat_panel_focus(ChatPanelFocus::ChatLog);

        app.process_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::empty()));
        app.process_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::empty()));
        assert_eq!(app.chat.selected_message(), Some(0));
        assert!(app.chat.scroll_offset() > 0);

        app.process_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::empty()));
        assert_eq!(app.chat.selected_message(), Some(19));
        assert_eq!(app.chat.scroll_offset(), 0);
    }

    #[test]
    fn chat_log_selection_change_scrolls_selected_header_into_view() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        for index in 0..30 {
            app.push_chat(ChatMessage {
                message_id: rpc::ids::MessageId(index + 1),
                room_id: rpc::ids::RoomId(1),
                sender: UserId(2),
                sender_name: "bob".to_string(),
                timestamp_ms: 1_000 + index * 1_000,
                body: format!("message {index}"),
            });
        }

        let mut buffer = Buffer::new(80, 14);
        crate::tui::render(&mut app, &mut buffer, 0);
        app.set_chat_panel_focus(ChatPanelFocus::ChatLog);
        app.process_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()));
        crate::tui::render(&mut app, &mut buffer, 0);

        let selected = app.chat.selected_message().expect("selected header");
        assert_eq!(selected, 12);
        assert!(
            app.last_chat_lines
                .iter()
                .any(|line| line.kind == LineKind::Heading && line.block_contains(selected)),
            "selected header must be visible after movement"
        );
    }

    #[test]
    fn tab_toggles_selected_chat_log_collapse() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        app.push_chat(ChatMessage {
            message_id: rpc::ids::MessageId(1),
            room_id: rpc::ids::RoomId(1),
            sender: UserId(2),
            sender_name: "bob".to_string(),
            timestamp_ms: 1,
            body: "```\n0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n```".to_string(),
        });

        let mut buffer = Buffer::new(80, 24);
        crate::tui::render(&mut app, &mut buffer, 0);
        app.set_chat_panel_focus(ChatPanelFocus::ChatLog);
        assert!(app.chat.is_collapsed(0));

        app.process_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert!(app.chat.is_expanded(0));
    }

    #[test]
    fn y_copies_selected_log_when_no_lines_are_selected() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        for (index, body) in ["first", "second"].into_iter().enumerate() {
            app.push_chat(ChatMessage {
                message_id: rpc::ids::MessageId(index as u64 + 1),
                room_id: rpc::ids::RoomId(1),
                sender: UserId(2),
                sender_name: "bob".to_string(),
                timestamp_ms: 1_000 + index as u64 * 1_000,
                body: body.to_string(),
            });
        }

        let mut buffer = Buffer::new(80, 24);
        crate::tui::render(&mut app, &mut buffer, 0);
        app.set_chat_panel_focus(ChatPanelFocus::ChatLog);
        app.chat.select_first_header();
        app.process_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()));

        assert_eq!(app.pending_clipboard.as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn mouse_down_on_chat_text_focuses_chat_log_and_selects_message() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        app.push_chat(ChatMessage {
            message_id: rpc::ids::MessageId(1),
            room_id: rpc::ids::RoomId(1),
            sender: UserId(2),
            sender_name: "bob".to_string(),
            timestamp_ms: 1,
            body: "hello".to_string(),
        });

        let mut buffer = Buffer::new(80, 24);
        crate::tui::render(&mut app, &mut buffer, 0);
        let (row_index, line) = app
            .last_chat_lines
            .iter()
            .copied()
            .enumerate()
            .find(|(_, line)| line.kind == LineKind::Body)
            .expect("body line rendered");

        app.process_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_chat_rect.x + 2,
            row: app.last_chat_rect.y + row_index as u16,
            modifiers: KeyModifiers::empty(),
        });

        assert_eq!(app.chat_focus, ChatPanelFocus::ChatLog);
        assert_eq!(app.focus.active(), FocusId::Chat);
        assert_eq!(app.chat.selected_message(), Some(line.message));
        assert!(app.chat.is_selecting());
    }

    #[test]
    fn mouse_down_on_lobby_row_focuses_lobby_and_selects_user() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
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

        let mut buffer = Buffer::new(80, 24);
        crate::tui::render(&mut app, &mut buffer, 0);
        app.process_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_room_rect.x + 1,
            row: app.last_room_rect.y,
            modifiers: KeyModifiers::empty(),
        });

        assert_eq!(app.chat_focus, ChatPanelFocus::Lobby);
        assert_eq!(app.focus.active(), FocusId::Participants);
        assert_eq!(app.participants.selected_user, Some(UserId(1)));
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
        crate::tui::render(&mut app, &mut buffer, 0);
    }

    #[test]
    fn chat_layout_reserves_top_bar_and_key_preview() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);
        app.server_alias = "local".to_string();
        app.user = "alice".to_string();
        app.room_name = "lobby".to_string();

        let mut buffer = Buffer::new(80, 24);
        crate::tui::render(&mut app, &mut buffer, 0);

        let expected_chat_top = 1 + app.config.ui.room_height + 1;
        let expected_chat_bottom = buffer.height() - 4;
        assert_eq!(app.last_chat_rect.y, expected_chat_top);
        assert_eq!(app.last_chat_rect.bottom(), expected_chat_bottom);
    }

    #[test]
    fn top_bar_audio_indicators_toggle_on_click() {
        let mut app = test_app();
        app.set_mode(theme::UiMode::Compose);

        let mut buffer = Buffer::new(80, 24);
        crate::tui::render(&mut app, &mut buffer, 0);

        let mute_rect = app.top_bar_mute_rect;
        assert!(!mute_rect.is_empty());
        app.process_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: mute_rect.x,
            row: mute_rect.y,
            modifiers: KeyModifiers::empty(),
        });
        assert!(app.mic_muted.load(Ordering::Relaxed));

        let deafen_rect = app.top_bar_deafen_rect;
        assert!(!deafen_rect.is_empty());
        app.process_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: deafen_rect.x,
            row: deafen_rect.y,
            modifiers: KeyModifiers::empty(),
        });
        assert!(app.deafened.load(Ordering::Relaxed));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_audio();
    }
}
