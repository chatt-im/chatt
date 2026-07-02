pub(crate) mod audio_diagnostics;
pub(crate) mod audio_supervisor;
pub(crate) mod commands;
pub(crate) mod dialogs;
pub(crate) mod participants;
pub(crate) mod room;
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

use extui::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use rpc::{
    control::{ERROR_TOKEN_STALE_EPOCH, InviteTicket, ParticipantVoiceStatus},
    ids::{RoomId, SessionId, StreamId, UserId},
};

use crate::{
    bindings::BindCommand,
    client_net::{
        NetworkClient, NetworkCommand, NetworkEvent, UploadFileRequest, spawn_open_pair_once,
        spawn_pair_once,
    },
    config::{self, Config, ServerEntry, SoundboardClip, ThemeChoice, validate_server_entry},
    local_control, settings,
    theme::Theme,
    tui::{
        Action,
        chrome::ChromeState,
        form::rect_contains,
        mode::{AppMode, ModeTransition, PendingTransition},
        modes::{RoomMode, ServerEditMode, ServerListMode, SettingsMode, SettingsSession},
        overlay::{DialogMode, PasswordPromptMode, PasteImageUploadMode},
    },
    ui::settings::{
        DeviceAction, DeviceSide, FieldId, FieldIntent, SettingsButton, SettingsOutput,
        capture_device_id, playback_device_id,
    },
};

use chatt::audio::{
    self, AudioStartError, BufferRequest, DeviceInfo, EchoCancellationControl,
    LiveAudioFileSourceConfig, LiveAudioFileSourceReport, LiveAudioMuteState,
    LiveAudioPacketLossProfile, LiveCapture, LiveCaptureConfig, LiveEncoderProfile, LivePlayback,
    LivePlaybackConfig, LivePlaybackFeedback, LivePlaybackSink, LivePlaybackSnapshot,
    LocalVoiceFrame, NotificationSound, PlaybackStreamControl,
};

use audio_diagnostics::AudioDiagnostics;
use audio_supervisor::{
    AudioDeviceEventKind, AudioEventLog, AudioHealthState, AudioStreamSupervisor, RebuildCause,
};
use chatt::audio::{AudioErrorKind, DeviceIdentityProbe};
use commands::slash_command_help;

pub(crate) use dialogs::{UserVolumeDialog, UserVolumeEvent};
pub(crate) use participants::{ParticipantState, ParticipantVoiceFeedback, Participants};
pub(crate) use room::{RoomSession, ToggleExpandResult};
pub(crate) use server::{
    PairCompletion, PendingPair, ServerEditDraft, ServerEditEvent, ServerSelectItem,
    alias_from_tcp_addr, default_join_alias, default_join_display_name, random_token,
    server_entry_from_invite, unique_server_alias,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusKind {
    Info,
    Error,
}

const TRANSIENT_STATUS_LIFETIME: Duration = Duration::from_secs(3);

pub(crate) struct StatusState {
    text: String,
    kind: StatusKind,
    expires_at: Option<Instant>,
}

impl StatusState {
    fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Info,
            expires_at: None,
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn kind(&self) -> StatusKind {
        self.kind
    }

    fn set(&mut self, status: impl Into<String>) {
        self.text = status.into();
        self.kind = StatusKind::Info;
        self.expires_at = None;
    }

    fn set_error(&mut self, status: impl Into<String>) {
        self.text = status.into();
        self.kind = StatusKind::Error;
        self.expires_at = None;
    }

    fn set_transient(&mut self, status: impl Into<String>, expires_at: Instant) {
        self.set(status);
        self.expires_at = Some(expires_at);
    }

    fn expire(&mut self, now: Instant) {
        if self.expires_at.is_some_and(|expires_at| now >= expires_at) {
            self.text.clear();
            self.expires_at = None;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatPanelFocus {
    Lobby,
    ChatLog,
    Compose,
}

impl ChatPanelFocus {
    const ORDER: [Self; 3] = [Self::Lobby, Self::ChatLog, Self::Compose];

    pub(crate) fn moved(self, delta: isize) -> Self {
        let current = Self::ORDER
            .iter()
            .position(|panel| *panel == self)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(Self::ORDER.len() as isize) as usize;
        Self::ORDER[next]
    }
}

#[derive(Default)]
pub(crate) struct ServerCatalog {
    items: Vec<ServerSelectItem>,
    generation: u64,
}

impl ServerCatalog {
    fn rebuild(&mut self, config: &Config) {
        self.items = config
            .servers
            .iter()
            .map(|server| ServerSelectItem {
                label: server.label.clone(),
                username: server.username.clone(),
                tcp_addr: server.tcp_addr.clone(),
                room_id: server.room_id,
                search_text: format!(
                    "{} {} {} {}",
                    server.label, server.username, server.tcp_addr, server.room_id
                ),
            })
            .collect();
        self.generation = self.generation.saturating_add(1);
    }

    pub(crate) fn items(&self) -> &[ServerSelectItem] {
        &self.items
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Default)]
pub(crate) struct AudioDeviceCatalog {
    input_devices: Vec<DeviceInfo>,
    output_devices: Vec<DeviceInfo>,
    generation: u64,
    refresh_in_flight: bool,
    next_refresh_id: u64,
}

impl AudioDeviceCatalog {
    pub(crate) fn input_devices(&self) -> &[DeviceInfo] {
        &self.input_devices
    }

    pub(crate) fn output_devices(&self) -> &[DeviceInfo] {
        &self.output_devices
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

pub(crate) struct App {
    pub config: Config,
    pub theme: Theme,
    events: AppEvents,
    pub room: RoomSession,
    pub status: StatusState,
    pub pending_transition: PendingTransition,
    pub chrome: ChromeState,
    pub audio_devices: AudioDeviceCatalog,
    pub network: Option<NetworkClient>,
    pub control_socket: Option<local_control::ControlSocket>,
    pub session_id: Option<SessionId>,
    pub user_id: Option<UserId>,
    pub server_catalog: ServerCatalog,
    pub pending_pair: Option<PendingPair>,
    pub mic_muted: Arc<AtomicBool>,
    pub deafened: Arc<AtomicBool>,
    pub voice_tx_enabled: Arc<AtomicBool>,
    pub mic_error: Option<String>,
    pub playback_error: Option<String>,
    pub capture: Option<LiveCapture>,
    pub settings_preview_capture: bool,
    pub settings_preview_refresh_id: Option<u64>,
    pub allow_settings_preview_capture: bool,
    pub playback: Option<LivePlayback>,
    pub soundboard_busy: Arc<AtomicBool>,
    pub soundboard_next_sequence: u32,
    pub echo_control: Arc<EchoCancellationControl>,
    pub voice_packets_received: u64,
    pub voice_bytes_received: u64,
    pub encoder_profile: LiveEncoderProfile,
    pub last_network_notice: Option<String>,
    /// A warn banner shown while a `chatt join` falls back to pairing because no
    /// configured server matched. Cleared once the client connects, disconnects,
    /// or cancels the pairing.
    pub join_notice: Option<String>,
    pub pending_audio_apply: Option<PendingAudioApply>,
    /// When `true`, the lobby shows the detailed developer voice stats instead of
    /// the collapsed per-participant latency estimate. Toggled by `/stats`,
    /// session-only (defaults off each launch).
    pub lobby_details: bool,
    /// Smoothed round-trip time to the server relay media socket, milliseconds.
    /// Used as the network leg of the latency estimate for relayed participants.
    pub server_rtt_ms: Option<u16>,
    /// When set, the deadline at which outbound voice should be hard-disabled
    /// after a deafen. The teardown is deferred so active senders can transmit
    /// their mute fade-out tail before transport closes.
    pending_voice_teardown_at: Option<Instant>,
    pending_network_commands: VecDeque<NetworkCommand>,
    supervisor: SupervisorState,
    /// Recent audio device events (losses, recoveries, default changes) shown
    /// by `/audio`.
    audio_events: AudioEventLog,
    /// The browser chat-log feed, present only when `[web] enabled = true`.
    web_feed: Option<crate::web_server::WebFeedSender>,
    /// The active outbound screen share, if this client is sharing.
    screencast: Option<crate::video::ScreencastHandle>,
    /// The stream id of our active outbound share, set on `ShareStarted`.
    screencast_stream_id: Option<StreamId>,
    /// Shares this client can view, keyed by stream id, learned from
    /// `ShareAvailable`. Holds the per-stream view secret and codec metadata.
    available_shares: HashMap<StreamId, AvailableShare>,
    /// Active inbound viewer connections, keyed by stream id.
    subscribers: HashMap<StreamId, crate::video::SubscriberHandle>,
    /// TCP address of the connected server, reused by dedicated video
    /// connections. Set on connect, cleared on disconnect.
    active_tcp_addr: Option<String>,
    active_server_label: Option<String>,
}

/// A share this client can view: the secret to bring up a viewer connection and
/// the codec metadata to configure the browser decoder.
struct AvailableShare {
    view_secret: Vec<u8>,
    codec: String,
    /// The decoder `extra_data` descriptor (`avcC`/`hvcC`), built by the
    /// publisher from the stream's parameter sets.
    extradata: Vec<u8>,
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
    capture: AudioStreamSupervisor,
    playback: AudioStreamSupervisor,
    capture_watch: CaptureWatch,
    playback_watch: PlaybackWatch,
    device_probe: DeviceProbeState,
}

/// Scheduling state for the background device-identity observer.
#[derive(Default)]
struct DeviceProbeState {
    next_at: Option<Instant>,
    in_flight: bool,
    last: Option<DeviceIdentityProbe>,
}

/// One audio direction's health plus its opened device, for the TUI.
pub(crate) struct AudioSideHealth {
    pub(crate) state: AudioHealthState,
    pub(crate) device_name: Option<String>,
}

impl AudioSideHealth {
    pub(crate) fn is_healthy(&self) -> bool {
        self.state == AudioHealthState::Healthy
    }
}

/// Edge detectors over the capture stats snapshot, so each failure episode
/// feeds the supervisor exactly once instead of re-arming it every tick.
#[derive(Default)]
struct CaptureWatch {
    callbacks: u64,
    captured_samples: u64,
    fatal_stream_errors: u64,
    worker_stopped: bool,
    worker_finished: bool,
    stall_reported: bool,
    last_progress_at: Option<Instant>,
}

#[derive(Default)]
struct PlaybackWatch {
    backend_fatal_stream_errors: u64,
    worker_finished: bool,
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
/// Device-observer poll cadence while streams are open and healthy.
const DEVICE_PROBE_INTERVAL_HEALTHY: Duration = Duration::from_secs(5);
/// Faster cadence while a stream is recovering or waiting to move back onto
/// its configured device, so a (re)appearing device is noticed promptly.
const DEVICE_PROBE_INTERVAL_RECOVERING: Duration = Duration::from_secs(2);
const LOBBY_TALKING_RELEASE: Duration = Duration::from_millis(200);
/// The talking indicator is intentionally more sensitive than NetEQ's
/// time-scaling VAD so quiet but audible decoded speech still registers.
const LOBBY_TALKING_RMS_THRESHOLD: f32 = 0.001; // -60 dBFS

/// Debounce window before a scheduled audio restart fires. Coalesces rapid
/// settings edits (cycling a choice, typing a buffer size) into one restart.
const AUDIO_APPLY_DEBOUNCE: Duration = Duration::from_millis(400);

/// Grace period outbound voice keeps running after a deafen, so active senders
/// transmit their mute fade-out tail (`LIVE_CAPTURE_MUTE_FADE`) plus an entry
/// silence marker before transport is hard-disabled. Sized to comfortably cover
/// the 60 ms fade and the marker that follows it.
const VOICE_DEAFEN_GRACE: Duration = Duration::from_millis(120);

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
        || old.dred != new.dred
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

fn playback_backend_failure(
    snapshot: &LivePlaybackSnapshot,
    watch: &PlaybackWatch,
) -> Option<(AudioErrorKind, String)> {
    if snapshot.backend_fatal_stream_errors <= watch.backend_fatal_stream_errors {
        return None;
    }
    let kind = snapshot
        .last_backend_error_kind
        .unwrap_or(AudioErrorKind::Transient);
    let message = snapshot
        .last_backend_error
        .clone()
        .unwrap_or_else(|| "playback stream error".to_string());
    Some((kind, message))
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

/// Result of one background device-identity probe (the audio hotplug and
/// default-device observer).
pub(crate) struct AudioDeviceProbeEvent {
    pub(crate) result: Result<DeviceIdentityProbe, String>,
}

/// An event delivered from a worker thread to the UI thread over the single
/// application event channel.
pub(crate) enum AppEvent {
    Network(NetworkEvent),
    AudioDeviceRefresh(AudioDeviceRefresh),
    AudioDeviceProbe(AudioDeviceProbeEvent),
    Soundboard(SoundboardEvent),
    Voice(local_control::VoiceCommand),
    Screencast(local_control::ScreencastCommand),
    Web(crate::web_server::WebRequest),
    /// A bug report request from `chatt report-bug`, carrying the description.
    ReportBug(String),
}

impl From<NetworkEvent> for AppEvent {
    fn from(event: NetworkEvent) -> Self {
        AppEvent::Network(event)
    }
}

impl From<AudioDeviceRefresh> for AppEvent {
    fn from(refresh: AudioDeviceRefresh) -> Self {
        AppEvent::AudioDeviceRefresh(refresh)
    }
}

impl From<AudioDeviceProbeEvent> for AppEvent {
    fn from(probe: AudioDeviceProbeEvent) -> Self {
        AppEvent::AudioDeviceProbe(probe)
    }
}

impl From<SoundboardEvent> for AppEvent {
    fn from(event: SoundboardEvent) -> Self {
        AppEvent::Soundboard(event)
    }
}

impl From<local_control::VoiceCommand> for AppEvent {
    fn from(command: local_control::VoiceCommand) -> Self {
        AppEvent::Voice(command)
    }
}

impl From<local_control::ScreencastCommand> for AppEvent {
    fn from(command: local_control::ScreencastCommand) -> Self {
        AppEvent::Screencast(command)
    }
}

impl From<crate::web_server::WebRequest> for AppEvent {
    fn from(request: crate::web_server::WebRequest) -> Self {
        AppEvent::Web(request)
    }
}

/// Serializes raw bytes as a JSON array of numbers, the form the browser reads
/// back into a `Uint8Array` for the decoder `description`.
/// The `share_available` envelope announcing a share so the browser shows a play
/// button and pre-knows the codec and its decoder descriptor.
fn share_available_envelope(
    stream_id: StreamId,
    sender: &str,
    codec: &str,
    width: u32,
    height: u32,
    extradata: &[u8],
) -> String {
    jsony::object! {
        type: "share_available",
        stream_id: stream_id.0,
        sender: sender,
        codec: codec,
        width: width,
        height: height,
        extradata: extradata,
    }
}

/// The `share_config` envelope sent when playback starts, carrying the decoder
/// codec string and `extra_data` descriptor.
fn share_config_envelope(stream_id: StreamId, codec: &str, extradata: &[u8]) -> String {
    jsony::object! {
        type: "share_config",
        stream_id: stream_id.0,
        codec: codec,
        extradata: extradata,
    }
}

/// The `share_ended` envelope telling the browser to tear down its decoder.
fn share_ended_envelope(stream_id: StreamId) -> String {
    jsony::object! { type: "share_ended", stream_id: stream_id.0 }
}

/// Parses an `/upload-rate` argument into bytes per second. Accepts `off`/`none`
/// (unlimited, `0`), a plain byte count, or a count with a `K`/`M`/`G` suffix
/// (powers of 1024).
fn parse_upload_rate(arg: &str) -> Result<u64, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err("usage: /upload-rate 200K|off".to_string());
    }
    if arg.eq_ignore_ascii_case("off") || arg.eq_ignore_ascii_case("none") {
        return Ok(0);
    }
    let (digits, multiplier) = match arg.as_bytes().last() {
        Some(b'k' | b'K') => (&arg[..arg.len() - 1], 1024),
        Some(b'm' | b'M') => (&arg[..arg.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&arg[..arg.len() - 1], 1024 * 1024 * 1024),
        _ => (arg, 1),
    };
    let value: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid upload rate: {arg}"))?;
    Ok(value.saturating_mul(multiplier))
}

/// The `file_progress` envelope updating a placeholder file message's progress
/// bar. Keyed by `file_id` (the server transfer id) plus `timestamp_ms`, matching
/// the browser's placeholder upsert. Dropped once the enriched attachment arrives.
fn file_progress_envelope(file_id: u64, timestamp_ms: u64, transferred: u64, total: u64) -> String {
    jsony::object! {
        type: "file_progress",
        file_id: file_id,
        timestamp_ms: timestamp_ms,
        transferred: transferred,
        total: total,
    }
}

/// The `share_error` envelope reporting a failed play request to the browser
/// that issued it, since the requester is watching the web view, not the TUI.
fn share_error_envelope(stream_id: StreamId, message: &str) -> String {
    jsony::object! {
        type: "share_error",
        stream_id: stream_id.0,
        message: message,
    }
}

/// Starts the web server and a relay thread that forwards browser requests into
/// the app event channel, returning the feed handle. The relay bridges the
/// otherwise one-directional web feed so a browser play click reaches the app.
/// Builds the web view's backlog for a room from its loaded history, attaching
/// stored image dimensions and served names to file messages.
fn web_room_messages(
    view: &crate::room_history::LoadedHistory,
) -> Vec<crate::web_server::WebMessage> {
    let mut messages = Vec::with_capacity(view.messages.len());
    for message in &view.messages {
        let web_message = match message.file_transfer_id {
            Some(transfer_id) => match view.files.get(&crate::room_history::FileHistoryKey {
                timestamp_ms: message.timestamp_ms,
                transfer_id,
            }) {
                Some(detail) => crate::web_server::WebMessage::from_history_file(
                    message,
                    &detail.file_name,
                    detail.dimensions(),
                ),
                None => crate::web_server::WebMessage::from(message),
            },
            None => crate::web_server::WebMessage::from(message),
        };
        messages.push(web_message);
    }
    messages
}

fn spawn_web_feed(
    web: &config::WebConfig,
    receive_dir: Option<PathBuf>,
    max_messages: usize,
    events: &EventSender,
) -> Option<crate::web_server::WebFeedSender> {
    let (web_tx, web_rx) = mpsc::channel();
    let feed = match crate::web_server::spawn(web, receive_dir, max_messages, web_tx, web.readonly)
    {
        Ok(feed) => feed,
        Err(error) => {
            kvlog::error!("web server failed to start", error = %error);
            return None;
        }
    };
    let relay = events.clone();
    if let Err(error) = thread::Builder::new()
        .name("chatt-web-relay".to_string())
        .spawn(move || {
            while let Ok(request) = web_rx.recv() {
                if relay.send(request).is_err() {
                    break;
                }
            }
        })
    {
        kvlog::warn!("web request relay failed to start", error = %error);
    }
    Some(feed)
}

/// Sends events into the single application event channel. Worker threads keep
/// constructing their own event types and rely on the `Into<AppEvent>` bound to
/// wrap them.
#[derive(Clone)]
pub(crate) struct EventSender(pub(crate) Sender<AppEvent>);

impl EventSender {
    pub(crate) fn send<E: Into<AppEvent>>(
        &self,
        event: E,
    ) -> Result<(), mpsc::SendError<AppEvent>> {
        self.0.send(event.into())
    }
}

struct AppEvents {
    tx: EventSender,
    rx: Receiver<AppEvent>,
}

impl AppEvents {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx: EventSender(tx),
            rx,
        }
    }

    fn sender(&self) -> EventSender {
        self.tx.clone()
    }

    fn next(&mut self) -> Result<Option<AppEvent>, mpsc::TryRecvError> {
        match self.rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(error @ mpsc::TryRecvError::Disconnected) => Err(error),
        }
    }
}

/// A join requested on the command line, to be started once the app is running.
pub(crate) enum PendingJoin {
    /// Invite-based pairing from a `tcj1_` join string.
    Invite(InviteTicket),
    /// Open pairing against a bare `host:port` address.
    Open { addr: String },
    /// A `chatt join` request naming a server by label or `host:port`. Resolved
    /// against the configured servers once the app is constructed.
    Named { specifier: String },
}

/// The outcome of resolving a `chatt join` specifier against configured servers.
#[derive(Debug, PartialEq, Eq)]
enum JoinResolution {
    /// Exactly one configured server matched; connect to it by label.
    Connect(String),
    /// Several servers could be meant; open the picker filtered to the specifier.
    Filter,
    /// No server matched but the specifier is a pairable `host:port`.
    Pair(String),
    /// No server matched and the specifier is not a pairable address.
    NoMatch,
}

impl App {
    pub(crate) fn new(config: Config, pending_join: Option<PendingJoin>) -> Result<Self, String> {
        let events = AppEvents::new();
        let soundboard_enabled = config.soundboard.enabled;
        let theme = Theme::from_choice(config.ui.theme);
        let room = RoomSession::new(&config, &theme);
        let echo_control = Arc::new(EchoCancellationControl::new(config.audio.echo_cancellation));
        let web_feed = if config.web.enabled {
            spawn_web_feed(
                &config.web,
                config.files.receive_dir_path(),
                config.ui.max_messages as usize,
                &events.tx,
            )
        } else {
            None
        };
        let mut app = Self {
            theme,
            events,
            room,
            status: StatusState::new("select a server"),
            pending_transition: PendingTransition::default(),
            chrome: ChromeState::default(),
            audio_devices: AudioDeviceCatalog::default(),
            network: None,
            control_socket: None,
            session_id: None,
            user_id: None,
            server_catalog: ServerCatalog::default(),
            pending_pair: None,
            mic_muted: Arc::new(AtomicBool::new(false)),
            deafened: Arc::new(AtomicBool::new(false)),
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            playback_error: None,
            capture: None,
            settings_preview_capture: false,
            settings_preview_refresh_id: None,
            allow_settings_preview_capture: !soundboard_enabled,
            playback: None,
            soundboard_busy: Arc::new(AtomicBool::new(false)),
            soundboard_next_sequence: 0,
            echo_control,
            voice_packets_received: 0,
            voice_bytes_received: 0,
            encoder_profile: LiveEncoderProfile::DRED_20,
            last_network_notice: None,
            join_notice: None,
            pending_audio_apply: None,
            lobby_details: false,
            server_rtt_ms: None,
            pending_voice_teardown_at: None,
            pending_network_commands: VecDeque::new(),
            supervisor: SupervisorState::default(),
            audio_events: AudioEventLog::default(),
            web_feed,
            screencast: None,
            screencast_stream_id: None,
            available_shares: HashMap::new(),
            subscribers: HashMap::new(),
            active_tcp_addr: None,
            active_server_label: None,
            config,
        };
        app.rebuild_server_items();
        match pending_join {
            Some(PendingJoin::Invite(ticket)) => app.start_join_pairing(ticket),
            Some(PendingJoin::Open { addr }) => app.start_open_pairing(addr),
            Some(PendingJoin::Named { specifier }) => app.start_named_join(specifier),
            None if app.config.servers.is_empty() => {
                app.set_status("no servers configured; run chatt pair <server>");
            }
            None => {}
        }
        Ok(app)
    }

    pub(crate) fn next_event(&mut self) -> Option<AppEvent> {
        match self.events.next() {
            Ok(event) => event,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.schedule_network_recovery(Instant::now(), "event channel disconnected");
                None
            }
            Err(mpsc::TryRecvError::Empty) => None,
        }
    }

    pub(crate) fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Network(event) => self.handle_network_event(event),
            AppEvent::AudioDeviceRefresh(refresh) => self.handle_audio_device_refresh(refresh),
            AppEvent::AudioDeviceProbe(probe) => self.handle_audio_device_probe(probe.result),
            AppEvent::Soundboard(event) => self.handle_soundboard_event(event),
            AppEvent::Voice(command) => self.apply_voice_command(command),
            AppEvent::Screencast(command) => self.handle_screencast_command(command),
            AppEvent::Web(request) => self.handle_web_request(request),
            AppEvent::ReportBug(description) => self.start_bug_report(description),
        }
    }

    /// Applies a CLI-driven voice command through the same App methods the UI
    /// keybindings and top-bar buttons use.
    fn apply_voice_command(&mut self, command: local_control::VoiceCommand) {
        match command {
            local_control::VoiceCommand::ToggleMute => self.toggle_mute(),
            local_control::VoiceCommand::SetMute(state) => self.set_mute(state),
            local_control::VoiceCommand::ToggleDeafen => {
                self.set_deafen(!self.deafened.load(Ordering::Relaxed))
            }
            local_control::VoiceCommand::SetDeafen(state) => self.set_deafen(state),
        }
    }

    /// Applies a CLI-driven screencast command: spawns capture and the publisher
    /// for `Start`, or tears the active share down for `Stop`.
    fn handle_screencast_command(&mut self, command: local_control::ScreencastCommand) {
        match command {
            local_control::ScreencastCommand::Start { argv, hevc } => {
                if self.screencast.is_some() {
                    self.set_error("a screen share is already active");
                    return;
                }
                let Some(network) = &self.network else {
                    self.set_error("connect before sharing your screen");
                    return;
                };
                let Some(tcp_addr) = self.active_tcp_addr.clone() else {
                    self.set_error("no active server for screen share");
                    return;
                };
                let codec = if hevc {
                    rpc::bitstream::Codec::Hevc
                } else {
                    rpc::bitstream::Codec::H264
                };
                let argv = if !argv.is_empty() {
                    argv
                } else if hevc {
                    crate::video::capture::hevc_ffmpeg_argv()
                } else {
                    crate::video::capture::default_ffmpeg_argv()
                };
                let web_feed = self.web_feed.clone();
                match crate::video::start_screencast(
                    argv,
                    codec,
                    network.sender(),
                    tcp_addr,
                    web_feed,
                ) {
                    Ok(handle) => {
                        self.screencast = Some(handle);
                        self.set_status("starting screen share");
                    }
                    Err(error) => self.set_error(format!("screen share failed: {error}")),
                }
            }
            local_control::ScreencastCommand::Stop => {
                self.stop_own_share();
                self.set_status("screen share stopped");
            }
        }
    }

    /// Stops this client's outbound share, notifying the server so viewers tear
    /// down and clearing the local self-view from this client's own browser.
    fn stop_own_share(&mut self) {
        if let Some(stream_id) = self.screencast_stream_id.take() {
            if let Some(network) = &self.network {
                let _ = network
                    .sender()
                    .send(NetworkCommand::StopShare { stream_id });
            }
            self.available_shares.remove(&stream_id);
            if let Some(feed) = &self.web_feed {
                feed.send_share_ended(stream_id.0, share_ended_envelope(stream_id));
            }
        }
        if let Some(mut handle) = self.screencast.take() {
            handle.stop();
        }
    }

    /// Stops the outbound share and every inbound viewer connection.
    fn stop_all_shares(&mut self) {
        self.stop_own_share();
        self.screencast_stream_id = None;
        self.available_shares.clear();
        for (_, mut subscriber) in self.subscribers.drain() {
            subscriber.stop();
        }
    }

    /// Handles a browser request relayed from the web view.
    fn handle_web_request(&mut self, request: crate::web_server::WebRequest) {
        match request {
            crate::web_server::WebRequest::PlayShare { stream_id } => {
                self.start_view(StreamId(stream_id))
            }
            crate::web_server::WebRequest::StopShare { stream_id } => {
                self.stop_view(StreamId(stream_id))
            }
            crate::web_server::WebRequest::SendChat { body } => {
                self.send_network_command(NetworkCommand::SendChat(body), true);
            }
            crate::web_server::WebRequest::UploadFile { path } => {
                self.send_network_command(
                    NetworkCommand::UploadFile(UploadFileRequest::new(path)),
                    true,
                );
            }
        }
    }

    /// Tells the browser to configure its decoder for `stream_id` and ensures a
    /// viewer connection is feeding it frames.
    ///
    /// The decoder config is broadcast on every play request, not just the
    /// first. A browser tab that connects after a share started receives the
    /// retained `share_available` button but missed the transient
    /// `share_config`, so its play click must re-broadcast the config to
    /// bootstrap its decoder. Frames are broadcast to every web client, so a
    /// single subscriber connection serves all tabs and a play request for an
    /// already-viewed stream reuses it instead of opening a second connection.
    fn start_view(&mut self, stream_id: StreamId) {
        // The play click came from the browser, so failures are reported back to
        // the web view rather than the TUI, which that user is not watching.
        let Some(feed) = self.web_feed.clone() else {
            return;
        };
        let Some(share) = self.available_shares.get(&stream_id) else {
            feed.send_share_error(share_error_envelope(
                stream_id,
                "that screen share is no longer available",
            ));
            return;
        };
        let config = share_config_envelope(stream_id, &share.codec, &share.extradata);
        let view_secret = share.view_secret.clone();
        feed.send_share_config(config);

        // The user's own share is teed to the browser by the publisher, and an
        // already-subscribed remote share is teed by its existing subscriber, so
        // in both cases the decoder config above is all the browser needs.
        if self.screencast_stream_id == Some(stream_id) {
            self.set_status("viewing your screen share");
            return;
        }
        if self.subscribers.contains_key(&stream_id) {
            self.set_status("viewing screen share");
            return;
        }

        let Some(tcp_addr) = self.active_tcp_addr.clone() else {
            feed.send_share_error(share_error_envelope(stream_id, "not connected to a server"));
            return;
        };
        let handle = crate::video::start_subscriber(stream_id, view_secret, tcp_addr, feed);
        self.subscribers.insert(stream_id, handle);
        self.set_status("viewing screen share");
    }

    fn stop_view(&mut self, stream_id: StreamId) {
        if let Some(mut subscriber) = self.subscribers.remove(&stream_id) {
            subscriber.stop();
        }
    }

    fn rebuild_server_items(&mut self) {
        self.server_catalog.rebuild(&self.config);
    }

    pub(crate) fn server_items(&self) -> &[ServerSelectItem] {
        self.server_catalog.items()
    }

    pub(crate) fn open_server_select(&mut self) {
        self.request_mode_transition(ModeTransition::Set(Box::new(ServerListMode::new())));
        self.rebuild_server_items();
        if self.config.servers.is_empty() {
            self.set_status("no servers configured; run chatt pair JOIN_STRING");
        } else {
            self.set_status("select a server");
        }
    }

    pub(crate) fn open_server_edit(&mut self, label: &str) {
        let Some((server_label, draft)) = self.server_edit_draft(label) else {
            return;
        };
        self.push_mode(Box::new(ServerEditMode::new(draft)));
        self.set_status(format!("editing server {server_label}"));
    }

    pub(crate) fn replace_with_server_edit(&mut self, label: &str) {
        let Some((server_label, draft)) = self.server_edit_draft(label) else {
            return;
        };
        self.replace_mode(Box::new(ServerEditMode::new(draft)));
        self.set_status(format!("editing server {server_label}"));
    }

    fn server_edit_draft(&mut self, label: &str) -> Option<(String, ServerEditDraft)> {
        let Ok(server) = self.config.server(label).cloned() else {
            self.set_error(format!("server {label} is not configured"));
            return None;
        };
        let draft = ServerEditDraft::from_server(&server, self.config.ui.form_bindings);
        Some((server.label, draft))
    }

    pub(crate) fn start_network(&mut self, alias: &str) -> bool {
        let server = match self.config.server(alias) {
            Ok(server) => server.clone(),
            Err(error) => {
                self.set_error(error);
                return false;
            }
        };
        self.disconnect_network();
        let network = match NetworkClient::spawn(
            server.client_config(&self.config.files, &self.config.p2p),
            self.events.sender(),
        ) {
            Ok(network) => network,
            Err(error) => {
                self.set_error(format!("failed to start network: {error}"));
                return false;
            }
        };
        self.control_socket =
            match local_control::ControlSocket::spawn(network.sender(), self.events.sender()) {
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
        let history_id = crate::room_history::derive_server_id(&server.token);
        self.room.connect_to_server(
            server.label.clone(),
            history_id,
            server.effective_display_name(),
        );
        let view = self
            .room
            .load_offline_history(RoomId(server.room_id), self.user_id);
        if let Some(feed) = &self.web_feed {
            feed.set_room(web_room_messages(&view));
        }
        self.active_tcp_addr = Some(
            server
                .client_config(&self.config.files, &self.config.p2p)
                .tcp_addr,
        );
        self.active_server_label = Some(server.label.clone());
        self.network = Some(network);
        self.supervisor.network.reset();
        self.join_notice = None;
        self.set_status("connecting");
        true
    }

    fn disconnect_network(&mut self) {
        self.stop_audio();
        self.stop_all_shares();
        self.active_tcp_addr = None;
        self.active_server_label = None;
        self.control_socket.take();
        if let Some(network) = self.network.take() {
            network.stop();
        }
        self.session_id = None;
        self.user_id = None;
        self.reset_room_for_disconnect();
        self.server_rtt_ms = None;
        self.last_network_notice = None;
        self.join_notice = None;
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.pending_voice_teardown_at = None;
        self.pending_network_commands.clear();
        self.supervisor.network.reset();
        self.supervisor.control_socket.reset();
        self.supervisor.capture.reset();
        self.supervisor.playback.reset();
        self.supervisor.capture_watch = CaptureWatch::default();
        self.supervisor.playback_watch = PlaybackWatch::default();
    }

    /// Resets per-room session state and clears the web feed's room so a browser
    /// stops showing a room the client has left. Used by every disconnect path,
    /// including reconnect and worker-failure recovery, so the feed never retains
    /// stale history while the client is detached.
    fn reset_room_for_disconnect(&mut self) {
        self.room.reset_for_disconnect();
        if let Some(feed) = &self.web_feed {
            feed.set_room(Vec::new());
        }
    }

    fn start_join_pairing(&mut self, ticket: InviteTicket) {
        let alias = unique_server_alias(&self.config, &default_join_alias(&ticket));
        let display_name = default_join_display_name();
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
            self.events.sender(),
        );
        self.pending_pair = Some(PendingPair {
            server,
            open: None,
            completion: PairCompletion::OpenEditor,
        });
        self.set_status(format!("pairing {alias}"));
    }

    /// Begins self-service pairing against a bare `host:port` address. The
    /// server's public key is trusted on first use, the token is server-issued,
    /// and the server prompts for a password only when it requires one.
    pub(crate) fn start_open_pairing(&mut self, addr: String) {
        let alias = unique_server_alias(&self.config, &alias_from_tcp_addr(&addr));
        let server = ServerEntry {
            label: alias.clone(),
            tcp_addr: addr,
            udp_addr: String::new(),
            udp_probe_addr: None,
            username: default_join_display_name(),
            token: String::new(),
            server_public_key: String::new(),
            room_id: 1,
        };
        spawn_open_pair_once(
            server.client_config(&self.config.files, &self.config.p2p),
            String::new(),
            String::new(),
            self.events.sender(),
        );
        self.pending_pair = Some(PendingPair {
            server,
            open: Some(String::new()),
            completion: PairCompletion::OpenEditor,
        });
        self.set_status(format!("pairing {alias}"));
    }

    /// Resolves and acts on a `chatt join` specifier: connect directly, open the
    /// filtered picker, or fall back to open pairing behind a warn banner.
    fn start_named_join(&mut self, specifier: String) {
        match self.resolve_join(&specifier) {
            JoinResolution::Connect(label) => {
                self.start_network(&label);
            }
            JoinResolution::Filter => {
                self.open_filtered_server_select(&specifier);
                self.set_status(format!("servers matching '{specifier}'"));
            }
            JoinResolution::Pair(addr) => {
                self.join_notice = Some(format!(
                    "   No saved server matches '{specifier}'; pairing with {addr} instead"
                ));
                self.start_open_pairing(addr);
            }
            JoinResolution::NoMatch => {
                self.open_filtered_server_select(&specifier);
                self.set_error(format!("no server matching '{specifier}'"));
            }
        }
    }

    /// Decides what a `chatt join` specifier means against the configured servers.
    ///
    /// An exact match on a single server's `label` or `tcp_addr` connects. Several
    /// matches, or a non-exact substring match, open the filtered picker. With no
    /// match, a valid `host:port` pairs and anything else opens the empty picker.
    fn resolve_join(&self, specifier: &str) -> JoinResolution {
        let exact: Vec<&str> = self
            .config
            .servers
            .iter()
            .filter(|server| server.label == specifier || server.tcp_addr == specifier)
            .map(|server| server.label.as_str())
            .collect();
        if exact.len() == 1 {
            return JoinResolution::Connect(exact[0].to_string());
        }
        if !exact.is_empty() {
            return JoinResolution::Filter;
        }
        let has_substring =
            self.config.servers.iter().any(|server| {
                server.label.contains(specifier) || server.tcp_addr.contains(specifier)
            });
        if has_substring {
            return JoinResolution::Filter;
        }
        match crate::cli::parse_pair_address(specifier) {
            Ok(addr) => JoinResolution::Pair(addr),
            Err(_) => JoinResolution::NoMatch,
        }
    }

    /// Opens the server picker with `query` pre-applied so the list starts filtered
    /// to the servers a `chatt join` specifier could mean.
    fn open_filtered_server_select(&mut self, query: &str) {
        self.request_mode_transition(ModeTransition::Set(Box::new(ServerListMode::with_query(
            query.to_string(),
        ))));
        self.rebuild_server_items();
    }

    /// Re-runs the open-pairing worker with a user-entered password, preserving
    /// the pending server and its existing token.
    pub(crate) fn submit_open_pair_password(&mut self, password: String) {
        let Some(pending) = self.pending_pair.as_ref() else {
            return;
        };
        let Some(existing_token) = pending.open.clone() else {
            return;
        };
        let alias = pending.server.label.clone();
        let client_config = pending
            .server
            .client_config(&self.config.files, &self.config.p2p);
        spawn_open_pair_once(
            client_config,
            password,
            existing_token,
            self.events.sender(),
        );
        self.set_status(format!("pairing {alias}"));
    }

    /// Pins or verifies the trusted server key carried by an open-pairing
    /// password challenge. The prompt owns the visible retry/error text; this
    /// only mutates durable pairing state.
    pub(crate) fn accept_open_pairing_password_challenge(
        &mut self,
        server_public_key: String,
    ) -> Result<(), String> {
        let Some(pair) = self.pending_pair.as_mut() else {
            return Err("pairing password prompt is no longer active".to_string());
        };
        if pair.server.server_public_key.is_empty() {
            pair.server.server_public_key = server_public_key;
            return Ok(());
        }
        if pair.server.server_public_key != server_public_key {
            self.pending_pair.take();
            return Err("pairing failed: server key changed during password retry".to_string());
        }
        Ok(())
    }

    pub(crate) fn complete_open_pairing(
        &mut self,
        token: String,
        server_public_key: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
    ) {
        self.complete_open_pairing_inner(token, server_public_key, udp_addr, udp_probe_addr, false);
    }

    pub(crate) fn complete_open_pairing_from_password_prompt(
        &mut self,
        token: String,
        server_public_key: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
    ) {
        self.complete_open_pairing_inner(token, server_public_key, udp_addr, udp_probe_addr, true);
    }

    fn complete_open_pairing_inner(
        &mut self,
        token: String,
        server_public_key: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
        close_prompt_if_idle: bool,
    ) {
        let Some(mut pair) = self.pending_pair.take() else {
            self.set_status("pairing succeeded");
            if close_prompt_if_idle && self.pending_transition.is_empty() {
                self.pop_mode();
            }
            return;
        };
        pair.server.token = token;
        pair.server.server_public_key = server_public_key;
        pair.server.udp_addr = udp_addr;
        pair.server.udp_probe_addr = udp_probe_addr;
        if let Err(error) = validate_server_entry(&pair.server) {
            self.set_error(error);
            if close_prompt_if_idle && self.pending_transition.is_empty() {
                self.pop_mode();
            }
            return;
        }
        let close_after_reconnect =
            close_prompt_if_idle && matches!(pair.completion, PairCompletion::Reconnect { .. });
        self.complete_pairing(pair.server, pair.completion);
        if close_after_reconnect && self.pending_transition.is_empty() {
            self.pop_mode();
        }
    }

    /// Cancels an in-progress open pairing when the user dismisses the password
    /// prompt.
    pub(crate) fn cancel_open_pairing(&mut self) {
        self.pop_mode();
        self.pending_pair.take();
        self.join_notice = None;
        self.set_status("pairing canceled");
    }

    fn start_stale_token_repair(&mut self, reason: &str) -> bool {
        let Some(label) = self.active_server_label.clone() else {
            return false;
        };
        let server = match self.config.server(&label).cloned() {
            Ok(server) => server,
            Err(error) => {
                self.push_network_notice("auth", &error);
                return false;
            }
        };
        let existing_token = server.token.clone();
        if existing_token.trim().is_empty() {
            return false;
        }
        let client_config = server.client_config(&self.config.files, &self.config.p2p);
        self.disconnect_network();
        self.push_network_notice("auth", reason);
        spawn_open_pair_once(
            client_config,
            String::new(),
            existing_token.clone(),
            self.events.sender(),
        );
        self.pending_pair = Some(PendingPair {
            server,
            open: Some(existing_token),
            completion: PairCompletion::Reconnect {
                label: label.clone(),
            },
        });
        self.set_status(format!("refreshing {label}"));
        true
    }

    /// Persists a freshly paired server and applies the caller's post-save action.
    fn complete_pairing(&mut self, server: ServerEntry, completion: PairCompletion) {
        let alias = server.label.clone();
        self.config.upsert_server(server);
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.rebuild_server_items();
                match completion {
                    PairCompletion::OpenEditor => {
                        self.replace_with_server_edit(&alias);
                        self.set_status(format!(
                            "paired {alias}; config saved to {}",
                            path.display()
                        ));
                    }
                    PairCompletion::Reconnect { label } => {
                        self.set_status(format!(
                            "refreshed {label}; config saved to {}",
                            path.display()
                        ));
                        if !self.start_network(&label) {
                            self.open_server_select();
                        }
                    }
                }
            }
            Err(error) => {
                self.rebuild_server_items();
                if matches!(completion, PairCompletion::OpenEditor) {
                    self.replace_with_server_edit(&alias);
                } else {
                    self.open_server_select();
                }
                self.set_error(error);
            }
        }
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
        if refresh.id + 1 != self.audio_devices.next_refresh_id {
            return;
        }
        self.audio_devices.refresh_in_flight = false;

        let mut input_count = None;
        let mut output_count = None;
        let mut errors = Vec::new();

        match refresh.input {
            Ok(devices) => {
                input_count = Some(devices.len());
                self.audio_devices.input_devices = devices;
            }
            Err(error) => {
                self.mic_error = Some(error.clone());
                errors.push(format!("input devices: {error}"));
            }
        }

        match refresh.output {
            Ok(devices) => {
                output_count = Some(devices.len());
                self.audio_devices.output_devices = devices;
            }
            Err(error) => {
                errors.push(format!("output devices: {error}"));
            }
        }

        self.audio_devices.generation = self.audio_devices.generation.saturating_add(1);
        kvlog::info!(
            "audio device refresh completed",
            id = refresh.id,
            input_buffer_request = refresh.input_buffer_request.label(),
            output_buffer_request = refresh.output_buffer_request.label(),
            input_count = input_count.unwrap_or(self.audio_devices.input_devices.len()),
            output_count = output_count.unwrap_or(self.audio_devices.output_devices.len()),
            input_ok = input_count.is_some(),
            output_ok = output_count.is_some(),
        );

        if !errors.is_empty() {
            kvlog::warn!(
                "audio device refresh had errors",
                errors = errors.join("; ")
            );
        }

        if refresh.restart_preview
            && self.settings_preview_refresh_id.take() == Some(refresh.id)
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
                self.room
                    .authenticated(rooms.first().map(|room| room.name.clone()));
                self.set_status(format!("authenticated as {}", self.room.local_user_name));
            }
            NetworkEvent::RoomJoined {
                room_id,
                history,
                participants,
            } => {
                let view = self
                    .room
                    .joined(room_id, participants, history, self.user_id);
                if let Some(feed) = &self.web_feed {
                    feed.set_room(web_room_messages(&view));
                }
                self.set_status(format!("joined room {}", room_id.0));
                self.publish_voice_status();
                self.start_room_voice();
                self.flush_pending_network_commands();
            }
            NetworkEvent::Chat(message) => {
                if let Some(feed) = &self.web_feed {
                    feed.send((&message).into());
                }
                let update = RoomSession::chat_received(&mut self.room, message, self.user_id);
                if !update.local {
                    self.play_notification(NotificationSound::MessageReceived);
                }
                let _ = update.should_scroll_bottom;
            }
            NetworkEvent::FileReceived {
                metadata,
                path,
                dimensions,
            } => {
                let served_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&metadata.file_name);
                if let Some(feed) = &self.web_feed {
                    feed.send(crate::web_server::WebMessage::from_file(
                        &metadata,
                        served_name,
                        dimensions,
                    ));
                }
                self.room.clear_transfer(metadata.transfer_id);
                self.room.file_received(
                    metadata.transfer_id,
                    metadata.timestamp_ms,
                    served_name,
                    metadata.size,
                    dimensions,
                );
            }
            NetworkEvent::TransferProgress {
                transfer_id,
                timestamp_ms,
                transferred,
                total,
                direction,
            } => {
                self.room
                    .transfer_progress(transfer_id, transferred, total, direction);
                if let Some(feed) = &self.web_feed {
                    feed.send_file_progress(file_progress_envelope(
                        transfer_id.0,
                        timestamp_ms,
                        transferred,
                        total,
                    ));
                }
            }
            NetworkEvent::TransferCanceled { transfer_id } => {
                self.room.clear_transfer(transfer_id);
            }
            NetworkEvent::Presence {
                participant,
                online,
                ..
            } => {
                let notice = self
                    .room
                    .presence_changed(participant, online, self.user_id);
                if !notice.local {
                    self.play_notification(if online {
                        NotificationSound::PeerJoin
                    } else {
                        NotificationSound::PeerLeave
                    });
                }
                self.set_status(format!(
                    "{} {}",
                    notice.display_name,
                    if online { "joined" } else { "left" }
                ));
            }
            NetworkEvent::VoiceStarted { user_id, stream_id } => {
                let notice = self.room.voice_started(user_id, stream_id, self.user_id);
                self.apply_user_audio_control(user_id);
                self.apply_remote_sender_mute(user_id, self.room.voice_muted(user_id));
                if notice.local {
                    if self.config.soundboard.enabled {
                        self.set_status("soundboard ready");
                    } else {
                        self.set_status("voice stream ready");
                    }
                } else {
                    self.set_status(format!("{} voice ready", notice.display_name));
                }
            }
            NetworkEvent::VoiceStopped { user_id, stream_id } => {
                let notice = self.room.voice_stopped(user_id, stream_id, self.user_id);
                if notice.local {
                    self.stop_audio();
                    self.set_status("voice stopped");
                } else {
                    if let Some(playback) = &self.playback {
                        playback.stop_stream(stream_id.0);
                    }
                    self.set_status(format!("{} left voice", notice.display_name));
                }
            }
            NetworkEvent::PeerTransport { user_id, direct } => {
                self.room.peer_transport_changed(user_id, direct);
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
                self.room.playback_feedback(feedback);
            }
            NetworkEvent::ServerRtt { rtt_ms } => {
                self.server_rtt_ms = rtt_ms;
            }
            NetworkEvent::PeerRtt { user_id, rtt_ms } => {
                self.room.peer_rtt(user_id, rtt_ms);
            }
            NetworkEvent::VoiceStatus { user_id, status } => {
                self.room.voice_status_changed(user_id, status);
                self.apply_remote_sender_mute(user_id, status.muted);
            }
            NetworkEvent::EncoderProfileChanged(profile) => {
                self.encoder_profile = profile;
                if let Some(capture) = &self.capture {
                    capture.set_encoder_profile(profile);
                }
            }
            NetworkEvent::ShareStarted {
                stream_id,
                publish_secret,
                codec,
                coded_width,
                coded_height,
                extradata,
            } => {
                self.screencast_stream_id = Some(stream_id);
                if let Some(handle) = &self.screencast {
                    handle.deliver_secret(stream_id, publish_secret);
                } else {
                    kvlog::warn!(
                        "share started without an active capture",
                        stream_id = stream_id.0
                    );
                }
                // Register the user's own share so their browser can watch it.
                // The publisher tees frames straight to the web feed, so the
                // local share needs no view secret or subscriber connection.
                let sender = self
                    .user_id
                    .map(|user_id| self.room.participants.display_name_for(user_id).to_string())
                    .unwrap_or_else(|| "you".to_string());
                self.available_shares.insert(
                    stream_id,
                    AvailableShare {
                        view_secret: Vec::new(),
                        codec: codec.clone(),
                        extradata: extradata.clone(),
                    },
                );
                if let Some(feed) = &self.web_feed {
                    feed.send_share_available(
                        stream_id.0,
                        share_available_envelope(
                            stream_id,
                            &sender,
                            &codec,
                            coded_width,
                            coded_height,
                            &extradata,
                        ),
                    );
                }
                self.set_status("screen share live");
            }
            NetworkEvent::ShareAvailable {
                stream_id,
                sender_name,
                codec,
                coded_width,
                coded_height,
                extradata,
                view_secret,
                ..
            } => {
                self.available_shares.insert(
                    stream_id,
                    AvailableShare {
                        view_secret,
                        codec: codec.clone(),
                        extradata: extradata.clone(),
                    },
                );
                if let Some(feed) = &self.web_feed {
                    feed.send_share_available(
                        stream_id.0,
                        share_available_envelope(
                            stream_id,
                            &sender_name,
                            &codec,
                            coded_width,
                            coded_height,
                            &extradata,
                        ),
                    );
                }
                self.set_status(format!("{sender_name} is sharing their screen"));
            }
            NetworkEvent::ShareEnded { stream_id, .. } => {
                self.available_shares.remove(&stream_id);
                if let Some(mut subscriber) = self.subscribers.remove(&stream_id) {
                    subscriber.stop();
                }
                if self.screencast_stream_id == Some(stream_id) {
                    self.screencast_stream_id = None;
                    if let Some(mut handle) = self.screencast.take() {
                        handle.stop();
                    }
                }
                if let Some(feed) = &self.web_feed {
                    feed.send_share_ended(stream_id.0, share_ended_envelope(stream_id));
                }
            }
            NetworkEvent::Status(status) => self.set_status(status),
            NetworkEvent::Error(error) => {
                kvlog::warn!("app network error", error = error.as_str());
                self.set_error(format!("error: {error}"));
            }
            NetworkEvent::AuthFailed { code, message } => {
                kvlog::warn!("app auth failed", code, error = message.as_str());
                if code == ERROR_TOKEN_STALE_EPOCH && self.start_stale_token_repair(&message) {
                    return;
                }
                self.disconnect_network();
                self.open_server_select();
                self.push_network_notice("auth", &message);
                self.set_error(auth_failure_status(&message));
            }
            NetworkEvent::PairingSucceeded => {
                let Some(pair) = self.pending_pair.take() else {
                    self.set_status("pairing succeeded");
                    return;
                };
                self.complete_pairing(pair.server, pair.completion);
            }
            NetworkEvent::OpenPairingSucceeded {
                token,
                server_public_key,
                udp_addr,
                udp_probe_addr,
            } => self.complete_open_pairing(token, server_public_key, udp_addr, udp_probe_addr),
            NetworkEvent::OpenPairingNeedsPassword {
                retry,
                server_public_key,
            } => {
                if let Err(error) = self.accept_open_pairing_password_challenge(server_public_key) {
                    self.set_error(error);
                    return;
                }
                self.push_mode(Box::new(PasswordPromptMode::new(retry)));
            }
            NetworkEvent::PairingFailed(error) => {
                self.pending_pair.take();
                self.set_error(error);
            }
            NetworkEvent::ReconnectScheduled { retry_in, reason } => {
                self.stop_audio();
                self.reset_room_for_disconnect();
                self.push_network_notice("network", &format!("Connection failed: {reason}"));
                self.set_error(format!(
                    "connection failed; retrying in {}s",
                    retry_in.as_secs()
                ));
            }
            NetworkEvent::WorkerStopped { reason } => {
                self.stop_audio();
                self.reset_room_for_disconnect();
                self.push_network_notice(
                    "network",
                    &format!("Network worker stopped: {reason}; reconnecting"),
                );
                self.schedule_network_recovery(Instant::now(), reason);
            }
            NetworkEvent::Disconnected => {
                self.disconnect_network();
                self.push_network_notice("network", "disconnected; viewing offline logs");
                self.set_error("disconnected");
            }
        }
    }

    fn observe_voice_packet(&mut self, _stream_id: u32, payload_size: usize) {
        self.voice_packets_received = self.voice_packets_received.saturating_add(1);
        self.voice_bytes_received = self
            .voice_bytes_received
            .saturating_add(payload_size as u64);
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

    fn push_network_notice(&mut self, sender: &str, body: &str) {
        if self.last_network_notice.as_deref() == Some(body) {
            return;
        }
        self.last_network_notice = Some(body.to_string());
        self.room.push_notice(sender, body);
    }

    /// Builds the fallback base mode used if the stack is ever popped empty.
    pub(crate) fn base_mode(&self) -> Box<dyn AppMode> {
        if self.network.is_some() || !self.room.server_alias.is_empty() {
            Box::new(RoomMode::default())
        } else {
            Box::new(ServerListMode::new())
        }
    }

    /// Requests pushing `mode` as an overlay on top of the current stack.
    pub(crate) fn push_mode(&mut self, mode: Box<dyn AppMode>) {
        self.request_mode_transition(ModeTransition::Push(mode));
    }

    pub(crate) fn replace_mode(&mut self, mode: Box<dyn AppMode>) {
        self.request_mode_transition(ModeTransition::Replace(mode));
    }

    /// Requests popping the active mode off the stack.
    pub(crate) fn pop_mode(&mut self) {
        self.request_mode_transition(ModeTransition::Pop);
    }

    pub(crate) fn request_mode_transition(&mut self, transition: ModeTransition) {
        self.pending_transition.request(transition);
    }

    pub(crate) fn take_mode_transition(&mut self) -> Option<ModeTransition> {
        self.pending_transition.take()
    }

    pub(crate) fn process_top_bar_mouse(&mut self, mouse: MouseEvent) -> bool {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return false;
        }
        if rect_contains(self.chrome.top_bar.mute, mouse.column, mouse.row) {
            self.toggle_mute();
            return true;
        }
        if rect_contains(self.chrome.top_bar.deafen, mouse.column, mouse.row) {
            self.set_deafen(!self.deafened.load(Ordering::Relaxed));
            return true;
        }
        false
    }

    pub(crate) fn delete_server(&mut self, label: &str) {
        self.config.servers.retain(|server| server.label != label);
        self.config
            .user_audio
            .retain(|preference| preference.server_alias != label);
        if self.room.server_alias == label {
            self.disconnect_network();
            self.room.reset_for_server_list();
        }
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.rebuild_server_items();
                self.set_status(format!(
                    "deleted {label}; config saved to {}",
                    path.display()
                ));
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(crate) fn save_server_edit_with(&mut self, draft: &ServerEditDraft, join_after_save: bool) {
        let update = match draft.to_update() {
            Ok(update) => update,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        let original_label = update.original_label;
        let server = update.server;
        if server.label != original_label
            && self
                .config
                .servers
                .iter()
                .any(|existing| existing.label == server.label)
        {
            self.set_error(format!("server label {} already exists", server.label));
            return;
        }
        let label = server.label.clone();
        if let Some(existing) = self
            .config
            .servers
            .iter_mut()
            .find(|existing| existing.label == original_label)
        {
            *existing = server;
        } else {
            self.config.upsert_server(server);
        }
        if label != original_label {
            for preference in &mut self.config.user_audio {
                if preference.server_alias == original_label {
                    preference.server_alias = label.clone();
                }
            }
            if self.room.server_alias == original_label {
                self.room.server_alias = label.clone();
            }
        }
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.rebuild_server_items();
                if join_after_save {
                    if self.start_network(&label) {
                        self.replace_mode(Box::new(RoomMode::default()));
                    }
                } else {
                    self.pop_mode();
                    self.set_status(format!("server saved to {}", path.display()));
                }
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(crate) fn cancel_open_audio_picker(&mut self, session: &mut SettingsSession) -> bool {
        if session.input_picker.open {
            self.cancel_audio_input_picker(session);
            true
        } else if session.output_picker.open {
            self.cancel_audio_output_picker(session);
            true
        } else {
            false
        }
    }

    fn audio_picker_open(session: &SettingsSession) -> bool {
        session.input_picker.open || session.output_picker.open
    }

    pub(crate) fn handle_open_settings_picker_mouse(
        &mut self,
        session: &mut SettingsSession,
        mouse: MouseEvent,
    ) -> bool {
        let delta = match mouse.kind {
            MouseEventKind::ScrollDown => 1,
            MouseEventKind::ScrollUp => -1,
            _ => return false,
        };
        let focus = session.form.focus();
        if focus == capture_device_id() && session.input_picker.open {
            session.input_picker.move_selection(delta);
            true
        } else if focus == playback_device_id() && session.output_picker.open {
            session.output_picker.move_selection(delta);
            true
        } else {
            false
        }
    }

    pub(crate) fn handle_open_settings_picker_key(
        &mut self,
        session: &mut SettingsSession,
        key: KeyEvent,
    ) -> bool {
        let focus = session.form.focus();
        if focus == capture_device_id() && session.input_picker.open {
            if !session.input_picker.searching {
                match key.code {
                    KeyCode::Esc => {
                        self.cancel_audio_input_picker(session);
                        return true;
                    }
                    KeyCode::Enter => {
                        self.confirm_audio_input_picker(session);
                        return true;
                    }
                    _ => {}
                }
            }
            handle_audio_picker_key(key, &mut session.input_picker, &session.input_items)
        } else if focus == playback_device_id() && session.output_picker.open {
            if !session.output_picker.searching {
                match key.code {
                    KeyCode::Esc => {
                        self.cancel_audio_output_picker(session);
                        return true;
                    }
                    KeyCode::Enter => {
                        self.confirm_audio_output_picker(session);
                        return true;
                    }
                    _ => {}
                }
            }
            handle_audio_picker_key(key, &mut session.output_picker, &session.output_items)
        } else {
            false
        }
    }

    pub(crate) fn process_global_command(&mut self, command: BindCommand) -> Action {
        use BindCommand::*;
        match command {
            OpenSettings => self.open_settings(),
            Quit => return Action::Quit,
            ToggleMute => self.toggle_mute(),
            ToggleDeafen => self.set_deafen(!self.deafened.load(Ordering::Relaxed)),
            PlaySoundboard1 => self.trigger_soundboard_slot(0),
            PlaySoundboard2 => self.trigger_soundboard_slot(1),
            PlaySoundboard3 => self.trigger_soundboard_slot(2),
            PlaySoundboard4 => self.trigger_soundboard_slot(3),
            PlaySoundboard5 => self.trigger_soundboard_slot(4),
            PlaySoundboard6 => self.trigger_soundboard_slot(5),
            PlaySoundboard7 => self.trigger_soundboard_slot(6),
            PlaySoundboard8 => self.trigger_soundboard_slot(7),
            PlaySoundboard9 => self.trigger_soundboard_slot(8),
            ToggleKeyPreview => {
                self.chrome.key_preview.expanded = !self.chrome.key_preview.expanded
            }
            _ => {}
        }
        Action::Continue
    }

    pub(crate) fn open_settings(&mut self) {
        if self.allow_settings_preview_capture
            && (self.audio_devices.input_devices.is_empty()
                || self.audio_devices.output_devices.is_empty())
        {
            self.refresh_audio_devices();
        }
        self.start_settings_preview_capture();
        self.push_mode(Box::new(SettingsMode::new(self)));
    }

    pub(crate) fn close_settings(&mut self, session: &mut SettingsSession) {
        self.commit_settings_form_text(session);
        self.pop_mode();
    }

    pub(crate) fn finish_settings_session(&mut self, session: &mut SettingsSession) {
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        self.settings_preview_refresh_id = None;
        self.stop_settings_preview_capture();
        session
            .input_picker
            .reset(&session.input_items, session.draft.input_selection());
        session
            .output_picker
            .reset(&session.output_items, session.draft.output_selection());
    }

    pub(crate) fn move_settings_focus(&mut self, session: &mut SettingsSession, delta: isize) {
        if Self::audio_picker_open(session) {
            self.move_active_audio_picker_selection(session, delta);
            return;
        }
        let commit = session.form.move_focus(delta);
        if commit.is_some() {
            self.drive_settings(session, FieldIntent::None, commit, None);
        }
    }

    /// Replays the immediate-mode settings form to apply `intent` (and any
    /// pending editor commit) to the focused field, then applies the resulting
    /// side effects. The single entry the input layer routes every adjust,
    /// activate, text commit, and click through.
    pub(crate) fn drive_settings(
        &mut self,
        session: &mut SettingsSession,
        intent: FieldIntent,
        commit: Option<(FieldId, String)>,
        focus_column: Option<u16>,
    ) {
        let output = crate::ui::settings::settings_logic(
            &mut session.form,
            &mut session.draft,
            &self.theme,
            session.dirty,
            intent,
            commit,
            focus_column,
            &session.input_items,
            &mut session.input_picker,
            &session.output_items,
            &mut session.output_picker,
        );
        self.apply_settings_output(session, output);
    }

    fn apply_settings_output(&mut self, session: &mut SettingsSession, output: SettingsOutput) {
        if let Some(button) = output.button {
            match button {
                SettingsButton::Refresh => self.refresh_audio_devices(),
                SettingsButton::Save => {
                    self.save_settings(session);
                    return;
                }
                SettingsButton::Close => {
                    self.close_settings(session);
                    return;
                }
            }
        }
        match output.device {
            Some(DeviceAction::Activate(DeviceSide::Input)) => {
                self.activate_audio_input_picker(session)
            }
            Some(DeviceAction::Cancel(DeviceSide::Input)) => {
                self.cancel_audio_input_picker(session)
            }
            Some(DeviceAction::Activate(DeviceSide::Output)) => {
                self.activate_audio_output_picker(session)
            }
            Some(DeviceAction::Cancel(DeviceSide::Output)) => {
                self.cancel_audio_output_picker(session)
            }
            None => {}
        }
        if output.changed {
            self.apply_settings_form_bindings(session);
            self.sync_settings_change(session);
        }
    }

    /// Syncs the settings draft into the live config and applies it to running
    /// audio. Cheap fields (amplification, echo cancellation) update in place.
    /// Slow fields (device, bitrate, denoise, buffer, latency) schedule a
    /// debounced stream restart. The on-disk file is only written by `Save`.
    fn sync_settings_change(&mut self, session: &mut SettingsSession) {
        self.config.ui.form_bindings = session.draft.form_bindings();
        self.apply_theme(session.draft.theme());
        // Never place malformed free-form settings into the live config. Hold
        // the last valid state until the text is fixed, then the diff below
        // re-applies every pending change.
        if let Some(reason) = session.draft.settings_text_invalid() {
            self.mark_settings_dirty(session);
            self.set_status(format!("settings not applied: {reason}"));
            return;
        }
        let old = self.config.audio.clone();
        let old_web = self.config.web.clone();
        self.config.audio = session.draft.to_audio();
        self.config.web = session.draft.to_web();
        self.config.notifications = session.draft.to_notifications();
        self.apply_web_setting(&old_web);
        self.apply_echo_cancellation_setting();
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        let (capture, playback) = audio_restart_flags(&old, &self.config.audio);
        if capture || playback {
            self.schedule_audio_apply(capture, playback);
        }
        self.mark_settings_dirty(session);
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
        self.room.apply_theme(&self.theme);
    }

    fn apply_web_setting(&mut self, old: &config::WebConfig) {
        if old.enabled && !self.config.web.enabled {
            if let Some(feed) = self.web_feed.take() {
                feed.stop();
                self.set_status("web log server stopped");
            }
            return;
        }

        if old.enabled && self.config.web.enabled && old.bind != self.config.web.bind {
            if let Some(feed) = self.web_feed.take() {
                feed.stop();
            }
        }

        if self.config.web.enabled && self.web_feed.is_none() {
            let feed = spawn_web_feed(
                &self.config.web,
                self.config.files.receive_dir_path(),
                self.config.ui.max_messages as usize,
                &self.events.tx,
            );
            match feed {
                Some(sender) => {
                    self.web_feed = Some(sender);
                    self.set_status(format!(
                        "web log server listening on {}",
                        self.config.web.bind
                    ));
                }
                None => {
                    self.set_error("web log server failed to start".to_string());
                }
            }
        }
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
        let now = Instant::now();
        self.expire_status(now);
        self.supervise(now);
        self.update_lobby_talking(now);
        self.apply_pending_audio_restart();
        self.supervise_voice_teardown(now);
    }

    /// Completes a deferred outbound-voice teardown once the deafen grace period
    /// has elapsed, after active senders have had time to send their mute
    /// fade-out tail. See [`Self::set_deafen`].
    fn supervise_voice_teardown(&mut self, now: Instant) {
        let Some(deadline) = self.pending_voice_teardown_at else {
            return;
        };
        if now < deadline {
            return;
        }
        self.pending_voice_teardown_at = None;
        // A racing undeafen clears the deadline, so reaching here means we are
        // still deafened and the fade tail has been sent.
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.stop_mic_capture();
    }

    fn update_lobby_talking(&mut self, now: Instant) {
        let local_user = self.user_id;
        let local_status = self.local_voice_status();
        let local_raw_active = if local_status.muted {
            false
        } else if self.config.soundboard.enabled {
            self.soundboard_busy.load(Ordering::Relaxed)
        } else {
            self.capture
                .as_ref()
                .is_some_and(|capture| lobby_voice_level_active(capture.stats().snapshot().rms))
        };
        let playback = self.playback.as_ref().map(|playback| playback.stats());
        let updates = self
            .room
            .participants
            .entries
            .iter()
            .map(|participant| {
                let raw_active = if Some(participant.user_id) == local_user {
                    local_raw_active
                } else {
                    participant
                        .active_stream
                        .and_then(|stream_id| {
                            playback.as_ref().and_then(|snapshot| {
                                snapshot
                                    .stream_activity
                                    .iter()
                                    .find(|activity| activity.stream_id == stream_id.0)
                            })
                        })
                        .is_some_and(|activity| lobby_voice_level_active(activity.rms))
                };
                (participant.user_id, raw_active)
            })
            .collect::<Vec<_>>();
        for (user_id, raw_active) in updates {
            self.room
                .update_talking_display(user_id, raw_active, now, LOBBY_TALKING_RELEASE);
        }
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
        if capture {
            self.restart_capture_stream();
            applied.push("capture");
        }
        if playback {
            self.supervisor.playback.reset();
            self.restart_playback_stream();
            applied.push("playback");
        }
        if !applied.is_empty() {
            self.set_status(format!("audio settings applied ({})", applied.join(", ")));
        }
    }

    /// Health of the capture side plus the opened device name, for the
    /// lobby-bar widget and top-bar indicators.
    pub(crate) fn capture_audio_health(&self) -> AudioSideHealth {
        AudioSideHealth {
            state: self.supervisor.capture.health().state,
            device_name: self
                .capture
                .as_ref()
                .map(|capture| capture.device_info().device_name.clone()),
        }
    }

    pub(crate) fn playback_audio_health(&self) -> AudioSideHealth {
        AudioSideHealth {
            state: self.supervisor.playback.health().state,
            device_name: self
                .playback
                .as_ref()
                .map(|playback| playback.device_info().device_name.clone()),
        }
    }

    /// Full manual audio reset: forgets all recovery state and backoff,
    /// rebuilds both streams, and re-enumerates the device catalog. Wired to
    /// `/audio-reset` and the lobby-bar reset button.
    pub(crate) fn audio_manual_reset(&mut self) {
        let now = Instant::now();
        self.audio_events.push(
            now,
            AudioDeviceEventKind::ManualReset,
            "user requested audio reset",
        );
        self.pending_audio_apply = None;
        self.supervisor.capture.reset();
        self.supervisor.playback.reset();
        self.supervisor.capture_watch = CaptureWatch::default();
        self.supervisor.playback_watch = PlaybackWatch::default();
        self.mic_error = None;
        self.playback_error = None;
        self.restart_capture_stream();
        let playback_should_run =
            self.voice_tx_enabled.load(Ordering::Relaxed) && !self.deafened.load(Ordering::Relaxed);
        if playback_should_run || self.playback.is_some() {
            self.restart_playback_stream();
        }
        self.refresh_audio_devices();
        self.set_status("audio reset: rebuilding streams");
    }

    fn supervise(&mut self, now: Instant) {
        self.supervise_network(now);
        self.supervise_control_socket(now);
        self.supervise_capture(now);
        self.supervise_playback(now);
        self.supervise_device_probe(now);
    }

    /// Schedules the background device-identity probe: paused while no stream
    /// is open and everything is healthy, 5 s while streams run, 2 s while a
    /// stream is recovering or displaced from its configured device.
    /// Enumeration always happens off-thread; only scheduling runs here.
    fn supervise_device_probe(&mut self, now: Instant) {
        let streams_active = self.capture.is_some() || self.playback.is_some();
        let recovering = self.supervisor.capture.is_recovering()
            || self.supervisor.playback.is_recovering()
            || self.supervisor.capture.wants_configured_device()
            || self.supervisor.playback.wants_configured_device();
        if !streams_active && !recovering {
            self.supervisor.device_probe.next_at = None;
            return;
        }
        if self.supervisor.device_probe.in_flight {
            return;
        }
        let interval = if recovering {
            DEVICE_PROBE_INTERVAL_RECOVERING
        } else {
            DEVICE_PROBE_INTERVAL_HEALTHY
        };
        let due = match self.supervisor.device_probe.next_at {
            None => now,
            Some(next_at) => next_at.min(now + interval),
        };
        if now < due {
            self.supervisor.device_probe.next_at = Some(due);
            return;
        }
        self.supervisor.device_probe.in_flight = true;
        self.supervisor.device_probe.next_at = Some(now + interval);
        let tx = self.events.sender();
        thread::Builder::new()
            .name("chatt-dev-probe".to_string())
            .stack_size(256 * 1024)
            .spawn(move || {
                let _ = tx.send(AudioDeviceProbeEvent {
                    result: audio::probe_device_identities(),
                });
            })
            .expect("failed to spawn audio device probe");
    }

    fn handle_audio_device_probe(&mut self, result: Result<DeviceIdentityProbe, String>) {
        self.supervisor.device_probe.in_flight = false;
        let probe = match result {
            Ok(probe) => probe,
            Err(error) => {
                kvlog::warn!("audio device probe failed", error = error.as_str());
                return;
            }
        };
        let now = Instant::now();
        let previous = self.supervisor.device_probe.last.take();
        if let Some(previous) = &previous {
            self.note_default_device_changes(now, previous, &probe);
            self.note_missing_stream_devices(now, previous, &probe);
        }
        self.note_target_device_sightings(now, &probe);
        self.supervisor.device_probe.last = Some(probe);
    }

    /// Follows OS default-device changes for streams opened on the default
    /// path. The rebuild is debounced by the supervisor so an AirPods
    /// A2DP/HFP profile flap that reverts within the window coalesces away.
    fn note_default_device_changes(
        &mut self,
        now: Instant,
        previous: &DeviceIdentityProbe,
        probe: &DeviceIdentityProbe,
    ) {
        if self.capture.is_some()
            && self.config.audio.input_device_id.is_none()
            && let (Some(old), Some(new)) = (&previous.default_input, &probe.default_input)
            && old.stable_id != new.stable_id
        {
            self.audio_events.push(
                now,
                AudioDeviceEventKind::DefaultInputChanged,
                format!("{} → {}", old.name, new.name),
            );
            self.supervisor.capture.on_default_changed(now);
        }
        // A configured output that matched the previous default was opened on
        // the default path; a default change there also warrants a rebuild,
        // which re-resolves onto the now-concrete configured device.
        let output_follows_default = match self.config.audio.output_device_id.as_deref() {
            None => true,
            Some(id) => previous
                .default_output
                .as_ref()
                .is_some_and(|identity| identity.matches_target(id)),
        };
        if self.playback.is_some()
            && output_follows_default
            && let (Some(old), Some(new)) = (&previous.default_output, &probe.default_output)
            && old.stable_id != new.stable_id
        {
            self.audio_events.push(
                now,
                AudioDeviceEventKind::DefaultOutputChanged,
                format!("{} → {}", old.name, new.name),
            );
            self.supervisor.playback.on_default_changed(now);
        }
    }

    /// Detects a concrete stream device dropping out of the enumeration while
    /// its stream still looks healthy (the error callback or stall watchdog
    /// usually fires first; this is the backstop). Edge-triggered on
    /// present-in-previous-probe so identity spelling mismatches can never
    /// produce a stream of false losses.
    fn note_missing_stream_devices(
        &mut self,
        now: Instant,
        previous: &DeviceIdentityProbe,
        probe: &DeviceIdentityProbe,
    ) {
        if let Some(capture) = &self.capture
            && self.supervisor.capture.is_healthy()
        {
            let info = capture.device_info();
            if !info.is_default
                && previous.inputs_contain(&info.stable_id)
                && !probe.inputs_contain(&info.stable_id)
            {
                let message = format!("device `{}` no longer present", info.device_name);
                self.audio_events.push(
                    now,
                    AudioDeviceEventKind::DeviceLost,
                    format!("mic: {}", info.device_name),
                );
                self.supervisor
                    .capture
                    .on_error(now, AudioErrorKind::DeviceGone, message);
            }
        }
        if let Some(playback) = &self.playback
            && self.supervisor.playback.is_healthy()
        {
            let info = playback.device_info();
            if !info.is_default
                && previous.outputs_contain(&info.stable_id)
                && !probe.outputs_contain(&info.stable_id)
            {
                let message = format!("device `{}` no longer present", info.device_name);
                self.audio_events.push(
                    now,
                    AudioDeviceEventKind::DeviceLost,
                    format!("spk: {}", info.device_name),
                );
                self.supervisor
                    .playback
                    .on_error(now, AudioErrorKind::DeviceGone, message);
            }
        }
    }

    /// Rebuilds immediately when the device a stream is waiting for — or the
    /// configured device it was displaced from — shows up in the probe.
    fn note_target_device_sightings(&mut self, now: Instant, probe: &DeviceIdentityProbe) {
        let capture_target_present = match self.config.audio.input_device_id.as_deref() {
            Some(id) => probe.inputs_contain(id),
            None => probe.default_input.is_some(),
        };
        if capture_target_present && self.supervisor.capture.on_target_device_seen(now) {
            self.audio_events.push(
                now,
                AudioDeviceEventKind::DeviceReturned,
                "mic device available again",
            );
        }
        let playback_target_present = match self.config.audio.output_device_id.as_deref() {
            Some(id) => probe.outputs_contain(id),
            None => probe.default_output.is_some(),
        };
        if playback_target_present && self.supervisor.playback.on_target_device_seen(now) {
            self.audio_events.push(
                now,
                AudioDeviceEventKind::DeviceReturned,
                "speaker device available again",
            );
        }
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
            self.reset_room_for_disconnect();
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
        let apply_owns_restart = self
            .pending_audio_apply
            .as_ref()
            .is_some_and(|pending| pending.capture);
        if !apply_owns_restart && let Some(cause) = self.supervisor.capture.take_due_rebuild(now) {
            self.recover_capture_stream(now, cause);
        }
        let Some(capture) = &self.capture else {
            self.supervisor.capture_watch = CaptureWatch::default();
            let should_run =
                self.voice_tx_enabled.load(Ordering::Relaxed) || self.settings_preview_capture;
            if !should_run {
                self.supervisor.capture.reset();
            }
            return;
        };
        let snapshot = capture.stats().snapshot();
        let mut failure = None;
        if snapshot.fatal_stream_errors > self.supervisor.capture_watch.fatal_stream_errors {
            failure = Some((
                snapshot
                    .last_error_kind
                    .unwrap_or(AudioErrorKind::Transient),
                snapshot
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "capture stream error".to_string()),
            ));
        }
        if snapshot.worker_stopped && !self.supervisor.capture_watch.worker_stopped {
            failure = Some((
                AudioErrorKind::Transient,
                "capture worker stopped".to_string(),
            ));
        }
        let worker_finished = capture.worker_finished();
        if worker_finished && !self.supervisor.capture_watch.worker_finished {
            failure = Some((
                AudioErrorKind::Transient,
                "capture worker exited".to_string(),
            ));
        }

        let progressed = snapshot.callbacks != self.supervisor.capture_watch.callbacks
            || snapshot.captured_samples != self.supervisor.capture_watch.captured_samples;
        if progressed || self.supervisor.capture_watch.last_progress_at.is_none() {
            self.supervisor.capture_watch.last_progress_at = Some(now);
            self.supervisor.capture_watch.stall_reported = false;
        } else if self.capture_should_be_live()
            && !self.supervisor.capture_watch.stall_reported
            && self
                .supervisor
                .capture_watch
                .last_progress_at
                .is_some_and(|last| now.saturating_duration_since(last) >= CAPTURE_STALL_TIMEOUT)
        {
            self.supervisor.capture_watch.stall_reported = true;
            // The typical shape of a device vanishing on ALSA and CoreAudio is
            // callbacks silently stopping, not an error callback.
            failure = Some((
                AudioErrorKind::Transient,
                "capture stream stopped delivering audio".to_string(),
            ));
        }

        self.supervisor.capture_watch.callbacks = snapshot.callbacks;
        self.supervisor.capture_watch.captured_samples = snapshot.captured_samples;
        self.supervisor.capture_watch.fatal_stream_errors = snapshot.fatal_stream_errors;
        self.supervisor.capture_watch.worker_stopped = snapshot.worker_stopped;
        self.supervisor.capture_watch.worker_finished = worker_finished;

        if let Some((kind, message)) = failure {
            self.note_capture_failure(now, kind, message);
        }
    }

    fn supervise_playback(&mut self, now: Instant) {
        let apply_owns_restart = self
            .pending_audio_apply
            .as_ref()
            .is_some_and(|pending| pending.playback);
        if !apply_owns_restart && let Some(cause) = self.supervisor.playback.take_due_rebuild(now) {
            self.recover_playback_stream(now, cause);
        }
        let Some(playback) = &self.playback else {
            self.supervisor.playback_watch = PlaybackWatch::default();
            let should_run = self.voice_tx_enabled.load(Ordering::Relaxed)
                && !self.deafened.load(Ordering::Relaxed);
            if !should_run {
                self.supervisor.playback.reset();
            }
            return;
        };
        let snapshot = playback.stats();
        let mut failure = playback_backend_failure(&snapshot, &self.supervisor.playback_watch);
        let worker_finished = playback.worker_finished();
        if worker_finished && !self.supervisor.playback_watch.worker_finished {
            failure = Some((
                AudioErrorKind::Transient,
                "playback decoder worker exited".to_string(),
            ));
        }
        self.supervisor.playback_watch.backend_fatal_stream_errors =
            snapshot.backend_fatal_stream_errors;
        self.supervisor.playback_watch.worker_finished = worker_finished;

        if let Some((kind, message)) = failure {
            self.note_playback_failure(now, kind, message);
        }
    }

    fn note_capture_failure(&mut self, now: Instant, kind: AudioErrorKind, message: String) {
        kvlog::warn!("capture stream failure", reason = message.as_str());
        self.audio_events.push(
            now,
            AudioDeviceEventKind::StreamError,
            format!("mic: {message}"),
        );
        self.supervisor.capture.on_error(now, kind, message);
        self.set_transient_status("microphone error; reconnecting");
    }

    fn note_playback_failure(&mut self, now: Instant, kind: AudioErrorKind, message: String) {
        kvlog::warn!("playback stream failure", reason = message.as_str());
        self.audio_events.push(
            now,
            AudioDeviceEventKind::StreamError,
            format!("spk: {message}"),
        );
        self.supervisor.playback.on_error(now, kind, message);
        self.set_transient_status("playback error; reconnecting");
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
                self.reset_room_for_disconnect();
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

    fn restart_network_worker(&mut self, reason: &str) {
        let alias = self.room.server_alias.clone();
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
        match local_control::ControlSocket::spawn(network.sender(), self.events.sender()) {
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

    fn recover_capture_stream(&mut self, now: Instant, cause: RebuildCause) {
        kvlog::warn!("recovering capture stream", cause = cause.label());
        self.audio_events.push(
            now,
            AudioDeviceEventKind::RebuildStarted,
            format!("mic rebuild ({})", cause.label()),
        );
        match self.restart_capture_stream_inner() {
            Ok(restarted) => {
                self.supervisor.capture.on_rebuild_ok(now);
                self.supervisor.capture_watch = CaptureWatch::default();
                self.mic_error = None;
                if restarted {
                    self.audio_events.push(
                        now,
                        AudioDeviceEventKind::Recovered,
                        "microphone recovered",
                    );
                    self.set_status("microphone recovered");
                }
            }
            Err(error) => {
                self.mic_error = Some(error.message.clone());
                self.audio_events.push(
                    now,
                    AudioDeviceEventKind::StreamError,
                    format!("mic rebuild failed: {}", error.message),
                );
                self.supervisor
                    .capture
                    .on_rebuild_failed(now, error.kind, error.message);
            }
        }
        // Restart the paired stream so the echo canceller's render reference is
        // rebuilt alongside capture, and an AirPods profile flip that changed
        // both directions converges in one pass.
        if self.voice_tx_enabled.load(Ordering::Relaxed) && self.supervisor.playback.is_healthy() {
            self.restart_playback_stream();
        }
    }

    fn recover_playback_stream(&mut self, now: Instant, cause: RebuildCause) {
        kvlog::warn!("recovering playback stream", cause = cause.label());
        self.audio_events.push(
            now,
            AudioDeviceEventKind::RebuildStarted,
            format!("spk rebuild ({})", cause.label()),
        );
        self.restart_playback_stream();
        if self.playback.is_some() {
            self.supervisor.playback_watch = PlaybackWatch::default();
            self.audio_events
                .push(now, AudioDeviceEventKind::Recovered, "playback recovered");
            self.set_status("playback recovered");
        }
        if self.capture_should_be_live() && self.supervisor.capture.is_healthy() {
            self.restart_capture_stream();
        }
    }

    fn restart_capture_stream(&mut self) {
        self.supervisor.capture.reset();
        if let Err(error) = self.restart_capture_stream_inner() {
            self.set_error(format!("failed to restart capture: {error}"));
            self.supervisor
                .capture
                .on_rebuild_failed(Instant::now(), error.kind, error.message);
        }
    }

    fn restart_capture_stream_inner(&mut self) -> Result<bool, AudioStartError> {
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

    /// Flushes the shared editor into the focused text field by replaying one
    /// logic pass. Called before Save and Close so the last keystroke persists.
    fn commit_settings_form_text(&mut self, session: &mut SettingsSession) {
        let commit = session.form.clear_text();
        if commit.is_some() {
            self.drive_settings(session, FieldIntent::None, commit, None);
        }
    }

    fn apply_settings_form_bindings(&mut self, session: &mut SettingsSession) {
        // Only the form-bindings choice triggers this, so no text edit is in
        // flight and the returned commit is always empty.
        let _ = session.form.set_bindings(session.draft.form_bindings());
    }

    pub(crate) fn move_settings_selection(&mut self, session: &mut SettingsSession, delta: isize) {
        if Self::audio_picker_open(session) {
            self.move_active_audio_picker_selection(session, delta);
        } else {
            self.move_settings_focus(session, delta);
        }
    }

    fn move_active_audio_picker_selection(&mut self, session: &mut SettingsSession, delta: isize) {
        let focus = session.form.focus();
        if focus == capture_device_id() && session.input_picker.open {
            session.input_picker.move_selection(delta);
        } else if focus == playback_device_id() && session.output_picker.open {
            session.output_picker.move_selection(delta);
        }
    }

    fn activate_audio_input_picker(&mut self, session: &mut SettingsSession) {
        if session.input_picker.open {
            self.confirm_audio_input_picker(session);
        } else {
            if self.audio_devices.input_devices.is_empty() {
                self.refresh_audio_devices();
            }
            session
                .input_picker
                .open(&session.input_items, session.draft.input_selection());
        }
    }

    fn activate_audio_output_picker(&mut self, session: &mut SettingsSession) {
        if session.output_picker.open {
            self.confirm_audio_output_picker(session);
        } else {
            if self.audio_devices.output_devices.is_empty() {
                self.refresh_audio_devices();
            }
            session
                .output_picker
                .open(&session.output_items, session.draft.output_selection());
        }
    }

    fn confirm_audio_input_picker(&mut self, session: &mut SettingsSession) {
        let Some(next) = session.input_picker.confirm(&session.input_items) else {
            return;
        };
        if session.draft.set_input_selection(next) {
            self.mark_settings_dirty(session);
        }
    }

    fn cancel_audio_input_picker(&mut self, session: &mut SettingsSession) {
        if let Some(selection) = session.input_picker.cancel(&session.input_items) {
            session.draft.restore_input_selection(selection);
        }
    }

    fn confirm_audio_output_picker(&mut self, session: &mut SettingsSession) {
        let Some(next) = session.output_picker.confirm(&session.output_items) else {
            return;
        };
        if session.draft.set_output_selection(next) {
            self.mark_settings_dirty(session);
        }
    }

    fn cancel_audio_output_picker(&mut self, session: &mut SettingsSession) {
        if let Some(selection) = session.output_picker.cancel(&session.output_items) {
            session.draft.restore_output_selection(selection);
        }
    }

    pub(crate) fn activate_settings_picker_item(
        &mut self,
        session: &mut SettingsSession,
        field: FieldId,
        item_index: usize,
    ) {
        if field == capture_device_id() {
            if session.input_picker.selector.select_item_index(item_index) {
                self.confirm_audio_input_picker(session);
            }
        } else if field == playback_device_id()
            && session.output_picker.selector.select_item_index(item_index)
        {
            self.confirm_audio_output_picker(session);
        }
    }

    pub(crate) fn mark_settings_dirty(&mut self, session: &mut SettingsSession) {
        session.dirty = true;
        self.set_status("settings draft changed; save config when ready");
    }

    pub(crate) fn open_selected_user_volume(&mut self) {
        let selected = match self.room.selected_remote_user(self.user_id) {
            Ok(user) => user,
            Err(error) => {
                self.set_status(error.status_text());
                return;
            }
        };
        let user_id = selected.user_id;
        let name = selected.display_name;
        let value_db = self.config.user_volume_db(&self.room.server_alias, user_id);
        self.room.begin_volume_preview(user_id, value_db);
        let dialog = UserVolumeDialog::new(user_id, name.clone(), value_db, &self.theme);
        self.push_mode(Box::new(DialogMode::new(dialog)));
        self.set_status(format!("adjusting local volume for {name}"));
    }

    pub(crate) fn toggle_selected_user_mute(&mut self) {
        let selected = match self.room.selected_remote_user(self.user_id) {
            Ok(user) => user,
            Err(error) => {
                self.set_status(error.status_text());
                return;
            }
        };
        let user_id = selected.user_id;
        let name = selected.display_name;
        let muted = self.room.toggle_user_mute(user_id);
        self.apply_user_audio_control(user_id);
        self.set_status(format!(
            "{} {name} locally",
            if muted { "muted" } else { "unmuted" }
        ));
    }

    /// Applies a [`UserVolumeEvent`] produced by the volume dialog.
    ///
    /// Returns `true` when the dialog overlay should close (the user saved or
    /// canceled). On a save error the dialog stays open with the error shown.
    pub(crate) fn apply_volume_event(
        &mut self,
        event: UserVolumeEvent,
        dialog: &mut UserVolumeDialog,
    ) -> bool {
        match event {
            UserVolumeEvent::Consumed => {}
            UserVolumeEvent::Preview { user_id, value_db } => {
                self.room.begin_volume_preview(user_id, value_db);
                self.apply_user_audio_control_with_volume(user_id, value_db);
            }
            UserVolumeEvent::Invalid(error) => self.set_error(error),
            UserVolumeEvent::Cancel {
                user_id,
                user_name,
                original_db,
            } => {
                self.config
                    .set_user_volume_db(&self.room.server_alias, user_id, original_db);
                self.apply_user_audio_control_with_volume(user_id, original_db);
                self.room.clear_volume_preview();
                self.set_status(format!("canceled local volume for {user_name}"));
                return true;
            }
            UserVolumeEvent::Save {
                user_id,
                user_name,
                value_db,
            } => {
                self.config
                    .set_user_volume_db(&self.room.server_alias, user_id, value_db);
                self.apply_user_audio_control_with_volume(user_id, value_db);
                match self.config.save_runtime() {
                    Ok(path) => {
                        self.config.config_path = Some(path.clone());
                        self.room.clear_volume_preview();
                        self.set_status(format!(
                            "saved local volume {}dB for {} to {}",
                            format_signed_db(value_db),
                            user_name,
                            path.display()
                        ));
                        return true;
                    }
                    Err(error) => {
                        dialog.mark_save_error(error.clone());
                        self.set_error(error);
                    }
                }
            }
        }
        false
    }

    fn apply_user_audio_control(&self, user_id: UserId) {
        let control = self.room.playback_control_for(&self.config, user_id);
        self.apply_user_audio_control_inner(user_id, control);
    }

    fn apply_user_audio_control_with_volume(&self, user_id: UserId, volume_db: f32) {
        let control = self.room.playback_control_for_volume(user_id, volume_db);
        self.apply_user_audio_control_inner(user_id, control);
    }

    fn apply_user_audio_control_inner(&self, user_id: UserId, control: PlaybackStreamControl) {
        let Some(playback) = &self.playback else {
            return;
        };
        for stream_id in self.room.stream_ids_for_user(user_id) {
            playback.set_stream_control(stream_id, control);
        }
    }

    /// Pushes a remote sender's control-stream mute state to the decoder for every
    /// stream that user owns, as a fallback when the in-band media mute markers are
    /// lost. Distinct from [`Self::apply_user_audio_control`], which mutes a peer
    /// locally at the mixer; this halts loss concealment for a sender who muted.
    fn apply_remote_sender_mute(&self, user_id: UserId, muted: bool) {
        let Some(playback) = &self.playback else {
            return;
        };
        for stream_id in self.room.stream_ids_for_user(user_id) {
            playback.set_sender_muted(stream_id, muted);
        }
    }

    fn apply_all_user_audio_controls(&self) {
        let users = self.room.users_with_streams().collect::<HashSet<UserId>>();
        for user_id in users {
            self.apply_user_audio_control(user_id);
            self.apply_remote_sender_mute(user_id, self.room.voice_muted(user_id));
        }
    }

    fn apply_echo_cancellation_setting(&self) {
        self.echo_control
            .set_enabled(self.config.audio.echo_cancellation);
    }

    pub(crate) fn save_settings(&mut self, session: &mut SettingsSession) {
        // Edits already applied live; this captures any uncommitted buffer field
        // then persists the live config to disk.
        self.commit_settings_form_text(session);
        self.sync_settings_change(session);
        if let Some(reason) = session.draft.settings_text_invalid() {
            self.set_error(format!("not saved: {reason}"));
            return;
        }
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                session.dirty = false;
                self.room.set_max_messages(self.config.ui.max_messages);
                self.set_status(format!("settings saved to {}", path.display()));
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(crate) fn refresh_audio_devices(&mut self) {
        self.refresh_audio_devices_with(self.input_buffer_request(), self.output_buffer_request());
    }

    pub(crate) fn refresh_audio_devices_for_settings(&mut self, session: &SettingsSession) {
        self.refresh_audio_devices_with(
            session.draft.input_buffer_request(),
            session.draft.output_buffer_request(),
        );
    }

    fn refresh_audio_devices_with(
        &mut self,
        input_buffer_request: BufferRequest,
        output_buffer_request: BufferRequest,
    ) {
        if self.audio_devices.refresh_in_flight {
            self.set_status("refreshing audio devices");
            return;
        }

        let restart_preview =
            self.settings_preview_capture && !self.voice_tx_enabled.load(Ordering::Relaxed);
        if restart_preview {
            self.stop_mic_capture();
        }

        let id = self.audio_devices.next_refresh_id;
        self.audio_devices.next_refresh_id = self.audio_devices.next_refresh_id.saturating_add(1);
        self.audio_devices.refresh_in_flight = true;
        if restart_preview {
            self.settings_preview_refresh_id = Some(id);
        }
        let tx = self.events.sender();
        kvlog::info!(
            "audio device refresh started",
            id,
            input_buffer_request = input_buffer_request.label(),
            output_buffer_request = output_buffer_request.label(),
            capture_active = self.capture.is_some(),
            playback_active = self.playback.is_some(),
            settings_preview_capture = self.settings_preview_capture,
        );
        thread::Builder::new()
            .name("chatt-dev-refresh".to_string())
            .stack_size(256 * 1024)
            .spawn(move || {
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
            })
            .expect("failed to spawn audio device refresh");
        self.set_status("refreshing audio devices");
    }

    pub(crate) fn submit_input(&mut self) {
        let Some(input) = self.room.submit_composer() else {
            return;
        };
        match input.as_str() {
            "/quit" => self.set_status("use Ctrl-C to quit"),
            "/mute" => self.set_mute(true),
            "/unmute" => self.set_mute(false),
            "/deafen" => self.set_deafen(true),
            "/undeafen" => self.set_deafen(false),
            "/muted" => self.show_mute_status(),
            "/deafened" => self.show_deafen_status(),
            "/audio" => self.show_audio_status(),
            "/audio-reset" => self.audio_manual_reset(),
            "/stats" => self.toggle_lobby_details(),
            "/clear" => self.room.clear_chat(),
            "/help" => self.show_command_help(),
            "/config" | "/settings" => self.open_settings(),
            "/servers" if self.network.is_some() => self.pop_mode(),
            "/servers" => self.open_server_select(),
            "/soundboard" => self.show_soundboard(),
            "/users" => self.show_users(),
            "/whoami" => self.show_current_user(),
            "/upload" => self.set_error("usage: /upload file_path/filename.ext"),
            command if command.starts_with("/upload ") => self.upload_file_command(command),
            "/upload-rate" => self.set_error("usage: /upload-rate 200K|off"),
            command if command.starts_with("/upload-rate ") => {
                self.set_upload_rate_command(command)
            }
            "/report-bug" => self.set_error("usage: /report-bug what went wrong"),
            command if command.starts_with("/report-bug ") => {
                let description = command.trim_start_matches("/report-bug ").trim();
                self.start_bug_report(description.to_string());
            }
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
                NetworkCommand::UploadFile(UploadFileRequest::new(std::path::PathBuf::from(path))),
                true,
            );
            self.set_status(format!("queued upload {}", path));
        } else {
            self.set_error("select a server before uploading files");
        }
    }

    fn set_upload_rate_command(&mut self, command: &str) {
        let arg = command.trim_start_matches("/upload-rate ").trim();
        let rate = match parse_upload_rate(arg) {
            Ok(rate) => rate,
            Err(message) => {
                self.set_error(message);
                return;
            }
        };
        if self.network.is_none() {
            self.set_error("select a server before setting the upload rate");
            return;
        }
        // The worker acknowledges with a `Status` event, so no status is set here.
        self.send_network_command(NetworkCommand::SetUploadRate(rate), true);
    }

    /// Opens the filename confirmation dialog for a pasted image or file.
    pub(crate) fn open_paste_image_dialog(&mut self, image: crate::clipboard_paste::ImagePaste) {
        let dialog = PasteImageUploadMode::new(image, &self.theme);
        self.push_mode(Box::new(dialog));
    }

    /// Validates the chosen name and queues the pasted upload. Returns `Err`
    /// with a message when the dialog should stay open (no server, bad name).
    pub(crate) fn confirm_paste_image_upload(
        &mut self,
        source: &crate::clipboard_paste::ImagePasteSource,
        raw_name: String,
    ) -> Result<(), String> {
        if self.network.is_none() {
            return Err("select a server before uploading files".to_string());
        }
        let name = crate::client_net::sanitize_file_name(&raw_name);
        if name.len() > rpc::control::MAX_FILE_NAME_BYTES {
            return Err("file name is too long".to_string());
        }
        let request = UploadFileRequest {
            path: source.path().clone(),
            name_override: Some(name.clone()),
            delete_after_open: source.is_staged(),
        };
        self.send_network_command(NetworkCommand::UploadFile(request), true);
        self.set_status(format!("queued upload {name}"));
        Ok(())
    }

    fn show_command_help(&mut self) {
        self.room.push_notice("help", slash_command_help());
        self.set_status("slash commands listed");
    }

    fn toggle_lobby_details(&mut self) {
        self.lobby_details = !self.lobby_details;
        if self.lobby_details {
            self.set_status("lobby detail on (jitter buffer stats)");
        } else {
            self.set_status("lobby detail off (latency estimate)");
        }
    }

    fn set_mute(&mut self, muted: bool) {
        if !muted && self.deafened.load(Ordering::Relaxed) {
            self.set_status("deafened; microphone remains muted");
            return;
        }
        self.mic_muted.store(muted, Ordering::Relaxed);
        self.publish_voice_status();
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
            // Keep active senders (and transport) alive briefly so they can send
            // their mute fade-out tail before capture/transport closes; the
            // deferred teardown in `supervise_voice_teardown` finishes the job.
            // With no outbound source there is nothing to fade, so tear down
            // immediately.
            if self.capture.is_some() || self.soundboard_busy.load(Ordering::Relaxed) {
                self.pending_voice_teardown_at = Some(Instant::now() + VOICE_DEAFEN_GRACE);
            } else {
                self.voice_tx_enabled.store(false, Ordering::Relaxed);
                self.stop_mic_capture();
            }
            self.set_network_playback_sink(None);
            self.playback.take();
            self.publish_voice_status();
            self.set_status("deafened");
        } else {
            self.pending_voice_teardown_at = None;
            self.publish_voice_status();
            self.set_status("undeafened");
            self.start_room_voice();
        }
    }

    fn local_voice_status(&self) -> ParticipantVoiceStatus {
        ParticipantVoiceStatus {
            muted: self.mic_muted.load(Ordering::Relaxed) || self.deafened.load(Ordering::Relaxed),
            deafened: self.deafened.load(Ordering::Relaxed),
        }
        .normalized()
    }

    fn publish_voice_status(&mut self) {
        let status = self.local_voice_status();
        if let Some(user_id) = self.user_id {
            self.room.voice_status_changed(user_id, status);
        }
        self.send_network_command(NetworkCommand::SetVoiceStatus(status), false);
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

    /// Formatted `health` and `events` sections for `/audio`. Built even while
    /// streams are down: that is exactly when diagnostics matter.
    fn audio_diagnostics_sections(&self) -> (Vec<String>, Vec<String>) {
        let now = Instant::now();
        let health_lines = vec![
            format!("mic: {}", self.supervisor.capture.health().describe(now)),
            format!("spk: {}", self.supervisor.playback.health().describe(now)),
        ];
        let recent_events = self
            .audio_events
            .iter_recent()
            .take(12)
            .map(|event| {
                format!(
                    "{:>3}  {}: {}",
                    audio_diagnostics::format_event_age(now.saturating_duration_since(event.at)),
                    event.kind.label(),
                    event.detail
                )
            })
            .collect();
        (health_lines, recent_events)
    }

    fn show_audio_status(&mut self) {
        let (health_lines, recent_events) = self.audio_diagnostics_sections();
        let diagnostics = AudioDiagnostics::new(
            self.playback
                .as_ref()
                .map(|playback| playback.stats())
                .unwrap_or_default(),
            self.encoder_profile,
            self.voice_packets_received,
            self.voice_bytes_received,
            self.capture
                .as_ref()
                .map(|capture| capture.device_info().clone()),
            self.playback
                .as_ref()
                .map(|playback| playback.device_info().clone()),
            health_lines,
            recent_events,
        );
        self.room.push_notice("audio", diagnostics.notice_body());
        self.set_status(diagnostics.status_summary());
    }

    /// Bundles recent logs plus audio and device diagnostics and ships them to
    /// the server as a bug report. Invoked by the `/report-bug` TUI command and
    /// the `chatt report-bug` CLI subcommand.
    fn start_bug_report(&mut self, description: String) {
        if description.is_empty() {
            self.set_error("usage: /report-bug what went wrong");
            return;
        }
        if self.network.is_none() {
            self.set_error("select a server before filing a bug report");
            return;
        }
        let metadata = self.bug_report_metadata(&description);
        let logs = crate::self_log::snapshot_plain_string();
        let compressed_logs = match zstd::encode_all(logs.as_bytes(), 9) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.set_error(format!("failed to compress logs: {error}"));
                return;
            }
        };
        self.send_network_command(
            NetworkCommand::ReportBug {
                description,
                metadata,
                compressed_logs,
            },
            true,
        );
        self.set_status("filing bug report");
    }

    /// Builds the JSON metadata sidecar saved alongside the compressed logs:
    /// app version, the `/audio` snapshot, and the device/buffer configuration.
    fn bug_report_metadata(&self, description: &str) -> String {
        let (health_lines, recent_events) = self.audio_diagnostics_sections();
        let audio = AudioDiagnostics::new(
            self.playback
                .as_ref()
                .map(|playback| playback.stats())
                .unwrap_or_default(),
            self.encoder_profile,
            self.voice_packets_received,
            self.voice_bytes_received,
            self.capture
                .as_ref()
                .map(|capture| capture.device_info().clone()),
            self.playback
                .as_ref()
                .map(|playback| playback.device_info().clone()),
            health_lines,
            recent_events,
        )
        .notice_body();
        let unix_time_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        let report = jsony::object! {
            version: env!("CARGO_PKG_VERSION"),
            description: description,
            unix_time_ms: unix_time_ms,
            encoder_profile: self.encoder_profile.label(),
            voice_packets_received: self.voice_packets_received,
            voice_bytes_received: self.voice_bytes_received,
            audio: audio,
            device: {
                input_device_id: self.config.audio.input_device_id.as_deref(),
                output_device_id: self.config.audio.output_device_id.as_deref(),
                input_buffer: format!("{:?}", self.config.audio.input_buffer),
                output_buffer: format!("{:?}", self.config.audio.output_buffer),
                bitrate_bps: self.config.audio.bitrate_bps,
                max_amplification: self.config.audio.max_amplification,
                denoise: self.config.audio.denoise.is_enabled(),
                echo_cancellation: self.config.audio.echo_cancellation,
            },
        };
        report.to_string()
    }

    fn show_users(&mut self) {
        let Some(users) = self.room.participant_names() else {
            self.set_status("no users in the current room yet");
            return;
        };
        self.set_status(format!("users: {users}"));
    }

    fn show_current_user(&mut self) {
        self.set_status(match self.user_id {
            Some(user_id) => format!(
                "signed in as {} on {} (user {})",
                self.room.local_user_name, self.room.server_alias, user_id.0
            ),
            None => format!(
                "connecting as {} on {}",
                self.room.local_user_name, self.room.server_alias
            ),
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
        self.room.push_notice(
            "soundboard",
            &format!(
                "clips {clips}; loss {}; trigger with /sound N or bound keys",
                self.config.soundboard.loss
            ),
        );
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
        if !self.voice_tx_enabled.load(Ordering::Relaxed)
            || !self.room.local_voice_stream_ready(self.user_id)
        {
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
        let events = self.events.sender();
        let send_failed = Arc::new(AtomicBool::new(false));
        let busy = Arc::clone(&self.soundboard_busy);
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        let source_config = LiveAudioFileSourceConfig {
            input_path,
            tuning: self.config.audio.latency.to_tuning(),
            packet_loss,
            seed: self.config.soundboard.seed.wrapping_add(slot as u64),
            first_sequence: self.soundboard_next_sequence,
            max_amplification: self.config.audio.max_amplification,
            denoise: self.config.audio.denoise.is_enabled(),
            auto_gain: true,
            mute_state: LiveAudioMuteState::new(
                Arc::clone(&self.mic_muted),
                Arc::clone(&self.deafened),
                Arc::clone(&self.voice_tx_enabled),
            ),
        };
        self.set_status(format!(
            "soundboard playing {} ({})",
            clip.name,
            packet_loss.as_name()
        ));
        thread::Builder::new()
            .name("chatt-soundboard".to_string())
            // 1M. This thread runs Opus encode via run_live_audio_file_source, whose stack depth
            // is not bounded by inspection. 1M is an overly safe margin over the default 2M.
            .stack_size(1024 * 1024)
            .spawn(move || {
                let send_failed = Arc::clone(&send_failed);
                let result = audio::run_live_audio_file_source(source_config, |sequence, frame| {
                    if !voice_tx_enabled.load(Ordering::Relaxed) {
                        return;
                    }
                    if network_tx
                        .send(NetworkCommand::SequencedLocalVoicePacket { sequence, frame })
                        .is_err()
                        && !send_failed.swap(true, Ordering::AcqRel)
                    {
                        let _ = events.send(NetworkEvent::WorkerStopped {
                            reason: "network command channel closed while sending soundboard audio"
                                .to_string(),
                        });
                    }
                });
                busy.store(false, Ordering::Release);
                let _ = events.send(SoundboardEvent { clip_name, result });
            })
            .expect("failed to spawn soundboard worker");
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

    fn live_capture_config(&self, input_device_id: Option<String>) -> LiveCaptureConfig {
        LiveCaptureConfig {
            input_device_id,
            bitrate_bps: self.config.audio.bitrate_bps,
            denoise: self.config.audio.denoise,
            dred: self.config.audio.dred,
            max_amplification: self.config.audio.max_amplification,
            suppression: self.config.audio.suppression(),
            typing_suppression: self.config.audio.typing_suppression(),
            buffer_request: self.input_buffer_request(),
            tuning: self.config.audio.latency.to_tuning(),
            echo_control: Some(Arc::clone(&self.echo_control)),
            mic_muted: Arc::clone(&self.mic_muted),
            deafened: Arc::clone(&self.deafened),
        }
    }

    fn capture_packet_handler(&self) -> impl FnMut(LocalVoiceFrame) + Send + 'static {
        let tx = self.network.as_ref().map(|network| network.sender());
        let event_tx = self.events.sender();
        let send_failed = Arc::new(AtomicBool::new(false));
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        // Mute and deafen are handled inside the capture pipeline (fade-out tail
        // plus silence markers), so this handler only gates the hard transport
        // on/off. Dropping muted frames here would look like packet loss to the
        // receiver's jitter buffer.
        move |payload| {
            if !voice_tx_enabled.load(Ordering::Relaxed) {
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

    fn ensure_mic_capture(&mut self) -> Result<(), AudioStartError> {
        if self.capture.is_some() {
            return Ok(());
        }
        if let Some(id) = self.config.audio.input_device_id.as_deref() {
            if !self.audio_devices.input_devices.is_empty() {
                let input_items = settings::audio_input_items(&self.audio_devices.input_devices);
                if let Some(item) = input_items
                    .iter()
                    .find(|item| item.matches_selection(Some(id)))
                {
                    if !item.supported {
                        let error = item
                            .issue
                            .clone()
                            .unwrap_or_else(|| "selected input device is unsupported".to_string());
                        self.mic_error = Some(error.clone());
                        return Err(AudioStartError::new(AudioErrorKind::ConfigInvalid, error));
                    }
                }
            }
        }

        let configured_input = self.config.audio.input_device_id.clone();
        let capture = match audio::start_live_capture(
            self.live_capture_config(configured_input.clone()),
            self.capture_packet_handler(),
        ) {
            Ok(capture) => {
                self.supervisor.capture.set_wants_configured_device(false);
                Ok(capture)
            }
            Err(error) if configured_input.is_some() => {
                kvlog::warn!(
                    "configured input failed, trying default",
                    error = error.message.as_str()
                );
                self.push_network_notice(
                    "audio",
                    &format!("Input device failed; trying system default: {error}"),
                );
                match audio::start_live_capture(
                    self.live_capture_config(None),
                    self.capture_packet_handler(),
                ) {
                    Ok(capture) => {
                        self.supervisor.capture.set_wants_configured_device(true);
                        self.audio_events.push(
                            Instant::now(),
                            AudioDeviceEventKind::FallbackToDefault,
                            format!("mic: {error}"),
                        );
                        Ok(capture)
                    }
                    Err(fallback_error) => Err(AudioStartError::new(
                        fallback_error.kind,
                        format!("{error}; default input fallback failed: {fallback_error}"),
                    )),
                }
            }
            Err(error) => Err(error),
        };
        match capture {
            Ok(capture) => {
                self.capture = Some(capture);
                self.mic_error = None;
                self.supervisor.capture.on_rebuild_ok(Instant::now());
                self.supervisor.capture_watch = CaptureWatch::default();
                Ok(())
            }
            Err(error) => {
                self.mic_error = Some(error.message.clone());
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
            self.mic_error = Some(error.message);
        }
    }

    fn start_settings_preview_capture_inner(&mut self) -> Result<(), AudioStartError> {
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
        let event_tx = self.events.sender();
        let send_failed = Arc::new(AtomicBool::new(false));
        thread::Builder::new()
            .name("chatt-fb-router".to_string())
            .stack_size(256 * 1024)
            .spawn(move || {
                for feedback in feedback_rx {
                    if network_tx
                        .send(NetworkCommand::PlaybackFeedback(feedback))
                        .is_err()
                        && !send_failed.swap(true, Ordering::AcqRel)
                    {
                        let _ = event_tx.send(NetworkEvent::WorkerStopped {
                            reason:
                                "network command channel closed while sending playback feedback"
                                    .to_string(),
                        });
                    }
                }
            })
            .expect("failed to spawn playback feedback router");
        let configured_output = self.config.audio.output_device_id.clone();
        let resolved_output = configured_output
            .as_deref()
            .filter(|id| !audio::configured_output_is_default(id))
            .map(|id| id.to_string());
        let playback = match audio::start_live_playback(
            self.live_playback_config(resolved_output.clone(), Some(feedback_tx.clone())),
        ) {
            Ok(playback) => {
                self.supervisor.playback.set_wants_configured_device(false);
                Ok(playback)
            }
            Err(error) if resolved_output.is_some() => {
                kvlog::warn!(
                    "configured output failed, trying default",
                    error = error.message.as_str()
                );
                self.push_network_notice(
                    "audio",
                    &format!("Output device failed; trying system default: {error}"),
                );
                match audio::start_live_playback(self.live_playback_config(None, Some(feedback_tx)))
                {
                    Ok(playback) => {
                        self.supervisor.playback.set_wants_configured_device(true);
                        self.audio_events.push(
                            Instant::now(),
                            AudioDeviceEventKind::FallbackToDefault,
                            format!("spk: {error}"),
                        );
                        Ok(playback)
                    }
                    Err(fallback_error) => Err(AudioStartError::new(
                        fallback_error.kind,
                        format!("{error}; default output fallback failed: {fallback_error}"),
                    )),
                }
            }
            Err(error) => Err(error),
        };
        match playback {
            Ok(playback) => {
                let fell_back = playback.buffer_fallback();
                let sink = playback.sink();
                self.playback = Some(playback);
                self.playback_error = None;
                self.supervisor.playback.on_rebuild_ok(Instant::now());
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
                self.playback_error = Some(error.message.clone());
                self.set_error(format!("voice playback unavailable: {error}"));
                let now = Instant::now();
                self.audio_events.push(
                    now,
                    AudioDeviceEventKind::StreamError,
                    format!("spk start failed: {}", error.message),
                );
                self.supervisor
                    .playback
                    .on_rebuild_failed(now, error.kind, error.message);
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

    /// Mixes a notification sound into the live output. The playback stream
    /// exists only inside a call, so the `Some` guard scopes sounds to in-call.
    fn play_notification(&self, sound: NotificationSound) {
        if let Some(playback) = &self.playback {
            let volume_db = self.config.notifications.volume_db(sound);
            let samples = audio::sound_samples(sound);
            if volume_db == 0.0 {
                playback.play_notification(samples);
            } else {
                let gain = 10.0_f32.powf(volume_db / 20.0);
                let scaled: Arc<[f32]> = samples
                    .iter()
                    .map(|sample| sample * gain)
                    .collect::<Vec<_>>()
                    .into();
                playback.play_notification(scaled);
            }
        }
    }

    fn stop_audio(&mut self) {
        let restart_settings_preview =
            self.settings_preview_capture && !self.deafened.load(Ordering::Relaxed);
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.pending_voice_teardown_at = None;
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

    pub(crate) fn set_status(&mut self, status: impl Into<String>) {
        self.status.set(status);
    }

    pub(crate) fn set_transient_status(&mut self, status: impl Into<String>) {
        self.status
            .set_transient(status, Instant::now() + TRANSIENT_STATUS_LIFETIME);
    }

    pub(crate) fn set_error(&mut self, status: impl Into<String>) {
        self.status.set_error(status);
    }

    fn expire_status(&mut self, now: Instant) {
        self.status.expire(now);
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

fn lobby_voice_level_active(rms: f32) -> bool {
    rms.is_finite() && rms >= LOBBY_TALKING_RMS_THRESHOLD
}

fn network_event_kind(event: &NetworkEvent) -> &'static str {
    match event {
        NetworkEvent::Connected => "connected",
        NetworkEvent::Authenticated { .. } => "authenticated",
        NetworkEvent::RoomJoined { .. } => "room_joined",
        NetworkEvent::Chat(_) => "chat",
        NetworkEvent::FileReceived { .. } => "file_received",
        NetworkEvent::TransferProgress { .. } => "transfer_progress",
        NetworkEvent::TransferCanceled { .. } => "transfer_canceled",
        NetworkEvent::Presence { .. } => "presence",
        NetworkEvent::VoiceStarted { .. } => "voice_started",
        NetworkEvent::VoiceStopped { .. } => "voice_stopped",
        NetworkEvent::PeerTransport { .. } => "peer_transport",
        NetworkEvent::VoicePacketObserved { .. } => "voice_packet_observed",
        NetworkEvent::VoicePacket(_) => "voice_packet",
        NetworkEvent::PlaybackFeedback(_) => "playback_feedback",
        NetworkEvent::ServerRtt { .. } => "server_rtt",
        NetworkEvent::PeerRtt { .. } => "peer_rtt",
        NetworkEvent::VoiceStatus { .. } => "voice_status",
        NetworkEvent::EncoderProfileChanged(_) => "encoder_profile_changed",
        NetworkEvent::Status(_) => "status",
        NetworkEvent::Error(_) => "error",
        NetworkEvent::AuthFailed { .. } => "auth_failed",
        NetworkEvent::PairingSucceeded => "pairing_succeeded",
        NetworkEvent::PairingFailed(_) => "pairing_failed",
        NetworkEvent::OpenPairingSucceeded { .. } => "open_pairing_succeeded",
        NetworkEvent::OpenPairingNeedsPassword { .. } => "open_pairing_needs_password",
        NetworkEvent::ReconnectScheduled { .. } => "reconnect_scheduled",
        NetworkEvent::WorkerStopped { .. } => "worker_stopped",
        NetworkEvent::Disconnected => "disconnected",
        NetworkEvent::ShareStarted { .. } => "share_started",
        NetworkEvent::ShareAvailable { .. } => "share_available",
        NetworkEvent::ShareEnded { .. } => "share_ended",
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
        NetworkCommand::SetVoiceStatus(_) => "set_voice_status",
        NetworkCommand::StartShare { .. } => "start_share",
        NetworkCommand::StopShare { .. } => "stop_share",
        NetworkCommand::ReportBug { .. } => "report_bug",
        NetworkCommand::SetUploadRate(_) => "set_upload_rate",
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
    use crate::{settings::SettingsDraft, tui::form::FormState};
    use extui::{Buffer, event::KeyModifiers};
    use extui_editor::Mode as EditorMode;
    use rpc::control::ParticipantInfo;

    fn test_app() -> App {
        App::new(Config::default(), None).expect("test app")
    }

    #[test]
    fn parse_upload_rate_accepts_suffixes_and_off() {
        assert_eq!(parse_upload_rate("off"), Ok(0));
        assert_eq!(parse_upload_rate("none"), Ok(0));
        assert_eq!(parse_upload_rate("0"), Ok(0));
        assert_eq!(parse_upload_rate("500000"), Ok(500_000));
        assert_eq!(parse_upload_rate("200K"), Ok(200 * 1024));
        assert_eq!(parse_upload_rate("2m"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_upload_rate("1G"), Ok(1024 * 1024 * 1024));
        assert!(parse_upload_rate("").is_err());
        assert!(parse_upload_rate("fast").is_err());
        assert!(parse_upload_rate("12x").is_err());
    }

    fn pending_open_pair(label: &str) -> PendingPair {
        PendingPair {
            server: ServerEntry {
                label: label.to_string(),
                tcp_addr: "chat.example.com:443".to_string(),
                udp_addr: String::new(),
                udp_probe_addr: None,
                username: "Zoe".to_string(),
                token: String::new(),
                server_public_key: String::new(),
                room_id: 1,
            },
            open: Some(String::new()),
            completion: PairCompletion::OpenEditor,
        }
    }

    fn render_room(app: &mut App, room: &mut RoomMode, buffer: &mut Buffer) {
        room.render(app, buffer, 0);
    }

    fn base_mode_label(app: &App) -> &'static str {
        app.base_mode()
            .presentation(app)
            .chrome
            .expect("base mode has chrome")
            .status_label
    }

    #[test]
    fn base_mode_stays_in_room_while_a_server_is_selected() {
        let mut app = test_app();
        // No server selected and no network: the server picker is the base.
        assert_eq!(base_mode_label(&app), "Servers");

        // A selected server (kept across a disconnect) holds the room view so
        // its offline logs stay readable.
        app.room.server_alias = "lab".to_string();
        assert_eq!(base_mode_label(&app), "Compose");
    }

    #[test]
    fn open_pair_password_prompt_pins_trusted_key_before_retry() {
        let mut app = test_app();
        app.pending_pair = Some(pending_open_pair("public"));
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        app.handle_app_event(
            NetworkEvent::OpenPairingNeedsPassword {
                retry: false,
                server_public_key: key.to_string(),
            }
            .into(),
        );

        assert_eq!(
            app.pending_pair.as_ref().unwrap().server.server_public_key,
            key
        );
    }

    #[test]
    fn stale_dynamic_token_auth_failure_starts_repair_pairing() {
        let mut app = test_app();
        app.config.servers.push(ServerEntry {
            label: "public".to_string(),
            tcp_addr: "127.0.0.1:9".to_string(),
            udp_addr: "127.0.0.1:9".to_string(),
            udp_probe_addr: None,
            username: "Zoe".to_string(),
            token: "tct1_existing-token".to_string(),
            server_public_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            room_id: 1,
        });
        app.active_server_label = Some("public".to_string());

        app.handle_app_event(
            NetworkEvent::AuthFailed {
                code: ERROR_TOKEN_STALE_EPOCH,
                message: "authentication failed: the server password changed; re-pair to refresh your token"
                    .to_string(),
            }
            .into(),
        );

        let pending = app.pending_pair.as_ref().expect("repair pairing pending");
        assert_eq!(pending.server.label, "public");
        assert_eq!(pending.open.as_deref(), Some("tct1_existing-token"));
        assert!(matches!(
            &pending.completion,
            PairCompletion::Reconnect { label } if label == "public"
        ));
    }

    #[test]
    fn open_pair_success_persists_token_key_and_udp_endpoints() {
        let path = std::env::temp_dir().join(format!(
            "chatt-open-pair-client-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut app = test_app();
        app.config.config_path = Some(path.clone());
        app.pending_pair = Some(pending_open_pair("public"));
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        app.handle_app_event(
            NetworkEvent::OpenPairingSucceeded {
                token: "tct1_token-with-enough-content".to_string(),
                server_public_key: key.to_string(),
                udp_addr: "198.51.100.20:54100".to_string(),
                udp_probe_addr: Some("198.51.100.20:54101".to_string()),
            }
            .into(),
        );

        let saved = app
            .config
            .servers
            .iter()
            .find(|server| server.label == "public")
            .expect("paired server saved");
        assert_eq!(saved.token, "tct1_token-with-enough-content");
        assert_eq!(saved.server_public_key, key);
        assert_eq!(saved.udp_addr, "198.51.100.20:54100");
        assert_eq!(saved.udp_probe_addr.as_deref(), Some("198.51.100.20:54101"));
        let _ = std::fs::remove_file(path);
    }

    fn participant(user_id: UserId, display_name: &str) -> ParticipantInfo {
        ParticipantInfo {
            user_id,
            display_name: display_name.to_string(),
            identifier: display_name.to_string(),
            in_call: true,
            voice_status: ParticipantVoiceStatus::default(),
            joined_at_ms: 0,
        }
    }

    /// Drives an [`App`] through a real mode stack so tests can exercise mode
    /// transitions (push/pop of overlays) the same way the runtime loop does.
    struct Harness {
        app: App,
        stack: crate::tui::mode_stack::ModeStack,
    }

    impl Harness {
        fn new(mut app: App) -> Self {
            let base: Box<dyn AppMode> = if app.room.server_alias.is_empty() {
                app.base_mode()
            } else {
                Box::new(RoomMode::default())
            };
            let stack = crate::tui::mode_stack::ModeStack::new(base, &mut app);
            Self { app, stack }
        }

        fn apply(&mut self) {
            self.stack.apply_pending(&mut self.app);
        }

        fn key(&mut self, key: KeyEvent) -> Action {
            let action = self.stack.active_mut().process_input(&mut self.app, key);
            self.apply();
            action
        }

        fn overlay_active(&self) -> bool {
            self.stack.overlay_active(&self.app)
        }

        fn top_theme_mode(&self) -> crate::theme::UiMode {
            self.stack
                .top_presentation(&self.app)
                .chrome
                .expect("base mode has chrome")
                .theme_mode
        }
    }

    #[test]
    fn share_error_envelope_carries_stream_and_message() {
        // The web frontend parses this by `type`, `stream_id`, and `message`, so
        // the shape is a cross-language contract with web/src/types.ts.
        let json = share_error_envelope(StreamId(7), "that screen share is no longer available");
        assert_eq!(
            json,
            "{\"type\":\"share_error\",\"stream_id\":7,\"message\":\"that screen share is no longer available\"}"
        );
    }

    #[test]
    fn share_error_envelope_escapes_message() {
        let json = share_error_envelope(StreamId(1), "bad \"quote\"");
        assert!(json.contains(r#""message":"bad \"quote\"""#), "{json}");
    }

    #[test]
    fn lobby_talking_threshold_includes_quiet_decoded_speech() {
        assert!(lobby_voice_level_active(0.005));
        assert!(lobby_voice_level_active(LOBBY_TALKING_RMS_THRESHOLD));
        assert!(!lobby_voice_level_active(LOBBY_TALKING_RMS_THRESHOLD * 0.5));
        assert!(!lobby_voice_level_active(f32::NAN));
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

        let mut dred = base.clone();
        dred.dred = audio::DredConfig::Off;
        assert_eq!(audio_restart_flags(&base, &dred), (true, false));

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
        latency.latency.neteq_start_delay_ms += 10;
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
        assert_eq!(app.status.kind(), StatusKind::Error);
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
    fn local_mute_and_deafen_publish_voice_status() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        app.room.joined(
            rpc::ids::RoomId(1),
            vec![participant(UserId(1), "alice")],
            Vec::new(),
            app.user_id,
        );

        app.set_mute(true);

        let status = match rx.try_recv().unwrap() {
            NetworkCommand::SetVoiceStatus(status) => status,
            other => panic!("unexpected command: {other:?}"),
        };
        assert_eq!(
            status,
            ParticipantVoiceStatus {
                muted: true,
                deafened: false,
            }
        );

        app.set_deafen(true);

        let status = loop {
            match rx.try_recv().unwrap() {
                NetworkCommand::SetVoiceStatus(status) => break status,
                NetworkCommand::SetPlaybackSink(None) => {}
                other => panic!("unexpected command: {other:?}"),
            }
        };
        assert_eq!(
            status,
            ParticipantVoiceStatus {
                muted: true,
                deafened: true,
            }
        );
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
        assert_eq!(server.label, "local-dev");
        assert_eq!(server.username, "Alice Dev");
    }

    #[test]
    fn settings_buffers_reuse_one_editor_and_commit_on_focus_change() {
        let mut draft = SettingsDraft::from_audio(&crate::config::AudioConfig::default());
        let capture = crate::ui::settings::field_id_for("Capture Settings", "Capture Buffer");
        let playback = crate::ui::settings::field_id_for("Playback Settings", "Playback Buffer");
        let mut form = FormState::new(capture, crate::config::FormBindings::Standard);
        form.focus_text(capture, &draft.input_buffer, false);
        let input_editor = form.editor_mut() as *mut _ as usize;
        form.editor_mut().set_lines("1024");

        let commit = form.set_focus(playback);
        if let Some((field, text)) = commit {
            if field == capture {
                draft.input_buffer = text;
            }
        }
        assert_eq!(draft.input_buffer, "1024");

        form.focus_text(playback, &draft.output_buffer, false);
        let output_editor = form.editor_mut() as *mut _ as usize;
        assert_eq!(input_editor, output_editor);
    }

    #[test]
    fn mouse_wheel_moves_open_settings_device_picker() {
        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::capture_device_id(),
            app.config.ui.form_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &app);
        mode.session_mut().input_items = ["System default", "USB Mic", "Line In"]
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
        let session = mode.session_mut();
        let input_selection = session.draft.input_selection().map(ToOwned::to_owned);
        let input_items = session.input_items.clone();
        session
            .input_picker
            .open(&input_items, input_selection.as_deref());

        assert_eq!(
            mode.session_mut()
                .input_picker
                .selector
                .current_item_index(),
            Some(0)
        );

        mode.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 4,
                row: 4,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert_eq!(
            mode.session_mut()
                .input_picker
                .selector
                .current_item_index(),
            Some(1)
        );
    }

    #[test]
    fn adjusting_a_choice_marks_dirty_and_resyncs_live_config() {
        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::field_id_for("Capture Settings", "Bitrate"),
            app.config.ui.form_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &app);
        let before = app.config.audio.bitrate_bps;

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
        );

        assert!(mode.session_mut().dirty);
        assert_ne!(app.config.audio.bitrate_bps, before);
    }

    #[test]
    fn settings_detour_returns_to_server_list() {
        let mut h = Harness::new(test_app());

        h.app.open_settings();
        h.apply();
        assert_eq!(h.stack.len(), 2);
        assert_eq!(h.top_theme_mode(), crate::theme::UiMode::Settings);

        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert_eq!(h.stack.len(), 1);
        assert_eq!(h.top_theme_mode(), crate::theme::UiMode::ServerSelect);
        assert!(!h.app.settings_preview_capture);
        assert_eq!(h.app.settings_preview_refresh_id, None);
    }

    #[test]
    fn settings_detour_preserves_composer_draft() {
        let mut app = test_app();
        app.room.server_alias = "local".to_string();
        app.room.composer.set_lines("unsent draft");
        let mut h = Harness::new(app);

        h.app.open_settings();
        h.apply();
        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert_eq!(h.stack.len(), 1);
        assert_eq!(h.top_theme_mode(), crate::theme::UiMode::Compose);
        assert_eq!(h.app.room.composer.text(), "unsent draft");
    }

    #[test]
    fn slash_help_pushes_command_notice() {
        let mut app = test_app();
        app.room.composer.set_lines("/help");

        app.submit_input();

        assert_eq!(app.room.chat.len(), 1);
        let notice = app.room.chat.message(0);
        assert_eq!(notice.sender, "help");
        assert!(notice.body.contains("/report-bug what went wrong"));
        assert!(notice.body.contains("Press Tab again to cycle matches"));
        assert_eq!(app.status.text(), "slash commands listed");
    }

    #[test]
    fn stats_command_toggles_lobby_details() {
        let mut app = test_app();
        assert!(!app.lobby_details);

        app.room.composer.set_lines("/stats");
        app.submit_input();
        assert!(app.lobby_details);
        assert_eq!(app.status.text(), "lobby detail on (jitter buffer stats)");

        app.room.composer.set_lines("/stats");
        app.submit_input();
        assert!(!app.lobby_details);
        assert_eq!(app.status.text(), "lobby detail off (latency estimate)");
    }

    #[test]
    fn volume_dialog_pushes_and_restores_focus() {
        let mut app = test_app();
        app.room.server_alias = "local".to_string();
        app.user_id = Some(UserId(1));
        app.room.joined(
            rpc::ids::RoomId(1),
            vec![
                participant(UserId(1), "alice"),
                participant(UserId(2), "bob"),
            ],
            Vec::new(),
            app.user_id,
        );
        app.room.move_participant_selection(1);

        let mut h = Harness::new(app);
        h.app.open_selected_user_volume();
        h.apply();

        assert_eq!(h.stack.len(), 2);
        assert!(h.overlay_active());
        assert_eq!(
            h.app.room.preview_volume_for_test().map(|(user, _)| user),
            Some(UserId(2))
        );

        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert_eq!(h.stack.len(), 1);
        assert!(!h.overlay_active());
        assert_eq!(h.app.room.preview_volume_for_test(), None);
    }

    #[test]
    fn compose_normal_m_uses_binding_to_toggle_mute() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.room.composer.mode(), EditorMode::Normal);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty()),
        );

        assert!(app.mic_muted.load(Ordering::Relaxed));
        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.room.composer.mode(), EditorMode::Normal);
    }

    #[test]
    fn selected_user_volume_requires_lobby_focus() {
        let mut app = test_app();
        app.room.server_alias = "local".to_string();
        app.user_id = Some(UserId(1));
        app.room.joined(
            rpc::ids::RoomId(1),
            vec![
                participant(UserId(1), "alice"),
                participant(UserId(2), "bob"),
            ],
            Vec::new(),
            app.user_id,
        );
        app.room.move_participant_selection(1);

        let mut h = Harness::new(app);
        let mut chat_room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        let action = chat_room.process_input(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(action, Action::Continue);
        h.apply();
        assert_eq!(h.stack.len(), 1);
        assert_eq!(h.app.status.text(), "focus lobby to adjust users");

        let mut lobby_room = RoomMode::with_focus(ChatPanelFocus::Lobby);
        let action = lobby_room.process_input(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(action, Action::Continue);
        h.apply();
        assert_eq!(h.stack.len(), 2);
        assert!(h.overlay_active());
    }

    #[test]
    fn delete_server_confirmation_gates_deletion() {
        let mut app = test_app();
        let temp_config =
            std::env::temp_dir().join(format!("chatt-delete-test-{}.toml", std::process::id()));
        app.config.config_path = Some(temp_config.clone());
        app.config.servers.push(crate::config::ServerEntry {
            label: "s1".to_string(),
            ..Default::default()
        });
        app.rebuild_server_items();

        let mut h = Harness::new(app);
        let mut server_mode = ServerListMode::new();
        let mut buffer = Buffer::new(80, 24);
        server_mode.render(&mut h.app, &mut buffer, 0);

        // Opening the confirmation does not delete anything yet.
        server_mode.process_action(&mut h.app, BindCommand::DeleteServer);
        h.apply();
        assert_eq!(h.stack.len(), 2);
        assert!(h.overlay_active());
        assert_eq!(h.app.config.servers.len(), 1);

        // Canceling keeps the server.
        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(h.stack.len(), 1);
        assert!(!h.overlay_active());
        assert_eq!(h.app.config.servers.len(), 1);

        // Confirming with 'y' deletes it and pops the overlay.
        server_mode.process_action(&mut h.app, BindCommand::DeleteServer);
        h.apply();
        h.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()));
        assert_eq!(h.stack.len(), 1);
        assert!(h.app.config.servers.is_empty());

        let _ = std::fs::remove_file(&temp_config);
    }

    #[test]
    fn server_catalog_rebuild_tracks_generation() {
        let mut app = test_app();
        let initial_generation = app.server_catalog.generation();
        app.config.servers.push(crate::config::ServerEntry {
            label: "s1".to_string(),
            ..Default::default()
        });

        app.rebuild_server_items();

        assert_eq!(app.server_items().len(), 1);
        assert_eq!(
            app.server_catalog.generation(),
            initial_generation.saturating_add(1)
        );
    }

    #[test]
    fn toggle_mute_while_deafened_undeafens_and_unmutes() {
        let mut app = test_app();
        app.set_deafen(true);
        assert!(app.deafened.load(Ordering::Relaxed));
        assert!(app.mic_muted.load(Ordering::Relaxed));

        app.process_global_command(BindCommand::ToggleMute);

        assert!(!app.deafened.load(Ordering::Relaxed));
        assert!(!app.mic_muted.load(Ordering::Relaxed));
    }

    #[test]
    fn renders_smoke_frame() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
    }

    #[test]
    fn chat_layout_reserves_top_bar_and_key_preview() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.room.server_alias = "local".to_string();
        app.room.local_user_name = "alice".to_string();
        app.room.room_name = "lobby".to_string();

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let expected_chat_top = 1 + app.config.ui.room_height + 1;
        let expected_chat_bottom = buffer.height() - 4;
        assert_eq!(room.layout().chat_rect.y, expected_chat_top);
        assert_eq!(room.layout().chat_rect.bottom(), expected_chat_bottom);
    }

    #[test]
    fn top_bar_audio_indicators_toggle_on_click() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let mute_rect = app.chrome.top_bar.mute;
        assert!(!mute_rect.is_empty());
        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: mute_rect.x,
                row: mute_rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert!(app.mic_muted.load(Ordering::Relaxed));

        let deafen_rect = app.chrome.top_bar.deafen;
        assert!(!deafen_rect.is_empty());
        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: deafen_rect.x,
                row: deafen_rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert!(app.deafened.load(Ordering::Relaxed));
    }

    #[test]
    fn lobby_bar_shows_recovery_state_and_reset_button_resets_on_click() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        assert!(app.chrome.lobby_bar.audio_reset.is_empty());

        app.supervisor.capture.on_rebuild_failed(
            Instant::now(),
            AudioErrorKind::DeviceGone,
            "device unplugged".to_string(),
        );
        render_room(&mut app, &mut room, &mut buffer);
        let reset_rect = app.chrome.lobby_bar.audio_reset;
        assert!(!reset_rect.is_empty());
        assert!(!app.chrome.lobby_bar.audio_widget.is_empty());

        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: reset_rect.x,
                row: reset_rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );
        assert!(app.supervisor.capture.is_healthy());

        render_room(&mut app, &mut room, &mut buffer);
        assert!(app.chrome.lobby_bar.audio_reset.is_empty());
    }

    fn app_with_servers(entries: &[(&str, &str)]) -> App {
        let mut app = test_app();
        app.config.servers.clear();
        for (label, tcp_addr) in entries {
            app.config.servers.push(ServerEntry {
                label: label.to_string(),
                tcp_addr: tcp_addr.to_string(),
                udp_addr: String::new(),
                udp_probe_addr: None,
                username: "Zoe".to_string(),
                token: "tct1_existing-token".to_string(),
                server_public_key: String::new(),
                room_id: 1,
            });
        }
        app
    }

    #[test]
    fn join_exact_label_resolves_to_direct_connect() {
        let app = app_with_servers(&[("lab", "10.0.0.1:4000"), ("home", "10.0.0.2:4000")]);
        assert_eq!(
            app.resolve_join("home"),
            JoinResolution::Connect("home".to_string())
        );
    }

    #[test]
    fn join_exact_address_shared_by_two_servers_opens_filtered_picker() {
        let app = app_with_servers(&[("work-a", "10.0.0.9:4000"), ("work-b", "10.0.0.9:4000")]);
        assert_eq!(app.resolve_join("10.0.0.9:4000"), JoinResolution::Filter);
    }

    #[test]
    fn join_substring_only_match_opens_filtered_picker() {
        let app = app_with_servers(&[
            ("home-desk", "10.0.0.1:4000"),
            ("home-lap", "10.0.0.2:4000"),
        ]);
        // "home" is exact for neither label, but a substring of both.
        assert_eq!(app.resolve_join("home"), JoinResolution::Filter);
    }

    #[test]
    fn join_no_match_pairable_address_falls_back_to_pairing() {
        let mut app = app_with_servers(&[("lab", "10.0.0.1:4000")]);
        assert_eq!(
            app.resolve_join("192.168.0.1:4000"),
            JoinResolution::Pair("192.168.0.1:4000".to_string())
        );
        app.start_named_join("192.168.0.1:4000".to_string());
        assert!(app.pending_pair.is_some());
        assert!(app.join_notice.is_some());
    }

    #[test]
    fn join_no_match_bad_label_opens_picker_without_pairing() {
        let mut app = app_with_servers(&[("lab", "10.0.0.1:4000")]);
        assert_eq!(app.resolve_join("does-not-exist"), JoinResolution::NoMatch);
        app.start_named_join("does-not-exist".to_string());
        assert!(app.pending_pair.is_none());
        assert_eq!(app.status.kind(), StatusKind::Error);
        assert!(matches!(
            app.take_mode_transition(),
            Some(ModeTransition::Set(_))
        ));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_audio();
    }
}
