mod appearance;
pub(crate) mod audio_diagnostics;
pub(crate) mod audio_supervisor;
pub(crate) mod command;
pub(crate) mod commands;
pub(crate) mod device_pair;
pub(crate) mod dialogs;
pub(crate) mod frontend;
mod join;
mod pairing;
pub(crate) mod participants;
pub(crate) mod room;
pub(crate) mod room_settings;
mod rpc_identity;
mod rpc_settings;
pub(crate) mod server;
mod server_catalog;
mod shared;
#[cfg(test)]
pub(crate) mod testing;

use hashbrown::{HashMap, HashSet};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use jsony::Jsony;
use rpc::{
    control::{
        ChatMutationKind, DeviceLinkTicket, ERROR_TOKEN_STALE_EPOCH, InviteTicket, VoiceState,
    },
    ids::{
        FileTransferId, MessageId, RoomId, ServerId, SessionId, ShareAttemptId, StreamId, UserId,
    },
};

use crate::{
    client_channel::{
        BaseScreen, DirtySections, NavigationEvent, OverlaySpec, ScreenSpec, ServerEditOutcome,
        TerminalEvent,
    },
    client_net::{
        MediaTransportState, NetworkClient, NetworkCommand, NetworkEvent, PAIRING_CANCELABLE,
        PairingEvent, TerminalVerb, TransferDirection, UploadFileRequest,
    },
    config::{
        self, Config, NotificationSoundMode, ServerEntry, SoundboardClip, ThemeSelection,
        validate_server_entry,
    },
    local_control, settings,
    tui::{modes::SettingsSession, view::ClientView},
    ui::settings::{
        DeviceAction, DeviceSide, FieldId, FieldIntent, SettingsButton, SettingsOutput,
        capture_device_id, playback_device_id,
    },
    ui::welcome::WelcomeDraft,
};

use crate::audio::{
    self, AtomicVoiceState, AudioStartError, BufferRequest, DeviceInfo, EchoCancellationControl,
    LOOPBACK_STREAM_ID, LiveAudioFileSourceConfig, LiveAudioFileSourceReport,
    LiveAudioPacketLossProfile, LiveAudioSourceState, LiveCapture, LiveCaptureConfig,
    LiveEncoderProfile, LivePlayback, LivePlaybackConfig, LivePlaybackFeedback, LivePlaybackSink,
    LivePlaybackSnapshot, LocalVoiceFrame, LoopbackTap, NotificationSound, PlaybackStreamControl,
};

use crate::audio::{AudioErrorKind, DeviceIdentityProbe};
use audio_diagnostics::AudioDiagnostics;
use audio_supervisor::{
    AudioDeviceEventKind, AudioEventLog, AudioHealthState, AudioStreamSupervisor, RebuildCause,
};
use commands::slash_command_help;
pub(crate) use join::{JoinOwner, JoinStart};
pub(crate) use pairing::{PairCompletion, PendingPair};
use pairing::{PairingCoordinator, PairingInput, PairingJob, RetainedTicket};
use shared::CoreRw;

pub(crate) use dialogs::{UserVolumeDialog, UserVolumeEvent};
pub(crate) use participants::{ParticipantState, ParticipantVoiceFeedback, Participants};
pub(crate) use room::{
    ComposerSubmission, DeleteSelection, HistoryChange, RoomSession, ToggleExpandResult,
};
pub(crate) use room_settings::{RoomSettingsDraft, RoomSettingsEvent};
pub(crate) use server::{
    ServerEditDraft, ServerEditEvent, ServerSelectItem, alias_from_tcp_addr, canonical_endpoint,
    default_join_alias, default_join_username, random_open_pair_recovery_token, random_token,
    server_entry_from_invite, unique_server_alias,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusKind {
    Info,
    Error,
}

const STATUS_LIFETIME: Duration = Duration::from_secs(3);
pub(crate) const SERVER_SWITCH_TRANSFER_BLOCKED: &str =
    "wait for or cancel active file transfers before switching servers";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScreencastPhase {
    Idle,
    Off,
    Starting,
    Live,
    Failed,
}

#[derive(Clone, Debug)]
pub(crate) struct ScreencastIssue {
    pub(crate) reason: String,
    pub(crate) at: Instant,
}

#[derive(Clone, Debug)]
pub(crate) struct ScreencastStatus {
    pub(crate) phase: ScreencastPhase,
    pub(crate) stream_id: Option<StreamId>,
    pub(crate) codec: Option<String>,
    pub(crate) coded_width: Option<u32>,
    pub(crate) coded_height: Option<u32>,
    pub(crate) started_at: Option<Instant>,
    pub(crate) ended_at: Option<Instant>,
    pub(crate) total_bytes: u64,
    pub(crate) total_frames: u64,
    pub(crate) rolling_bytes_per_sec: u64,
    pub(crate) last_issue: Option<ScreencastIssue>,
}

impl Default for ScreencastStatus {
    fn default() -> Self {
        Self {
            phase: ScreencastPhase::Idle,
            stream_id: None,
            codec: None,
            coded_width: None,
            coded_height: None,
            started_at: None,
            ended_at: None,
            total_bytes: 0,
            total_frames: 0,
            rolling_bytes_per_sec: 0,
            last_issue: None,
        }
    }
}

impl ScreencastStatus {
    fn start(&mut self) {
        self.phase = ScreencastPhase::Starting;
        self.stream_id = None;
        self.codec = None;
        self.coded_width = None;
        self.coded_height = None;
        self.started_at = Some(Instant::now());
        self.ended_at = None;
        self.total_bytes = 0;
        self.total_frames = 0;
        self.rolling_bytes_per_sec = 0;
    }

    fn live(&mut self, stream_id: StreamId, codec: String, coded_width: u32, coded_height: u32) {
        self.phase = ScreencastPhase::Live;
        self.stream_id = Some(stream_id);
        self.codec = Some(codec);
        self.coded_width = Some(coded_width);
        self.coded_height = Some(coded_height);
        self.started_at.get_or_insert_with(Instant::now);
        self.ended_at = None;
    }

    fn progress(&mut self, stream_id: StreamId, total_bytes: u64, total_frames: u64, rate: u64) {
        if self.stream_id == Some(stream_id) {
            self.total_bytes = total_bytes;
            self.total_frames = total_frames;
            self.rolling_bytes_per_sec = rate;
        }
    }

    fn fail(&mut self, reason: String) {
        let now = Instant::now();
        self.phase = ScreencastPhase::Failed;
        self.ended_at = Some(now);
        self.last_issue = Some(ScreencastIssue { reason, at: now });
    }

    fn clear_active(&mut self) {
        self.phase = ScreencastPhase::Idle;
        self.stream_id = None;
        self.codec = None;
        self.coded_width = None;
        self.coded_height = None;
        self.started_at = None;
        self.ended_at = Some(Instant::now());
        self.total_bytes = 0;
        self.total_frames = 0;
        self.rolling_bytes_per_sec = 0;
    }

    fn turn_off(&mut self) {
        self.phase = ScreencastPhase::Off;
        self.stream_id = None;
        self.codec = None;
        self.coded_width = None;
        self.coded_height = None;
        self.started_at = None;
        self.ended_at = Some(Instant::now());
        self.total_bytes = 0;
        self.total_frames = 0;
        self.rolling_bytes_per_sec = 0;
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StatusState {
    text: String,
    kind: StatusKind,
    expires_at: Option<Instant>,
}

impl StatusState {
    /// Creates a persistent baseline status. Messages posted after construction
    /// use the bounded lifetime applied by [`Self::set`] and [`Self::set_error`].
    pub(crate) fn new(text: impl Into<String>) -> Self {
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

    pub(crate) fn set(&mut self, status: impl Into<String>) {
        self.text = status.into();
        self.kind = StatusKind::Info;
        self.expires_at = Some(Instant::now() + STATUS_LIFETIME);
    }

    pub(crate) fn set_error(&mut self, status: impl Into<String>) {
        self.text = status.into();
        self.kind = StatusKind::Error;
        self.expires_at = Some(Instant::now() + STATUS_LIFETIME);
    }

    pub(crate) fn set_transient(&mut self, status: impl Into<String>, expires_at: Instant) {
        self.set(status);
        self.expires_at = Some(expires_at);
    }

    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        if self.expires_at.is_some_and(|expires_at| now >= expires_at) {
            self.text.clear();
            self.expires_at = None;
            true
        } else {
            false
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

#[derive(Clone, Default)]
pub(crate) struct ServerCatalog {
    items: Vec<ServerSelectItem>,
    generation: u64,
}

impl ServerCatalog {
    pub(crate) fn rebuild(&mut self, config: &Config) -> bool {
        let items = config
            .servers
            .iter()
            .map(|server| ServerSelectItem {
                id: server.id,
                label: server.label.clone(),
                username: server.username.clone(),
                tcp_addr: server.tcp_addr.clone(),
                require_transport_encryption: server.require_transport_encryption,
                search_text: format!("{} {} {}", server.label, server.username, server.tcp_addr),
            })
            .collect();
        if self.items == items {
            return false;
        }
        self.items = items;
        self.generation = self.generation.saturating_add(1);
        true
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

/// Core-side handle to one attached terminal: its wake channel and the view
/// its render thread draws from.
pub(crate) struct ClientHandle {
    pub(crate) channel: Arc<crate::client_channel::ClientChannel>,
}

#[derive(Clone, Debug)]
enum RpcServerSelectionIssue {
    Error(local_rpc::model::ServerSelectionError),
    Prompt(local_rpc::model::ServerSelectionPrompt),
}

#[derive(Clone, Debug)]
struct OwnedRpcServerSelectionIssue {
    owner: crate::client_channel::ClientId,
    attempt_id: u64,
    issue: RpcServerSelectionIssue,
}

#[derive(Clone, Copy)]
struct PendingWebHistoryRequest {
    room_id: RoomId,
    room_generation: u64,
    before: MessageId,
    limit: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialRepairContinuation {
    ActiveSession,
    Join { attempt_id: u64 },
}

/// One in-flight silent repair of a stale saved credential. Only the expected
/// credential is retained: a successful repair patches the current record so
/// edits made while the worker ran cannot be overwritten.
struct CredentialRepair {
    attempt: u64,
    server_id: ServerId,
    expected_token: String,
    expected_server_public_key: String,
    owner: crate::client_channel::ClientId,
    continuation: CredentialRepairContinuation,
}

pub(crate) struct App {
    pub config: CoreRw<Config>,
    events: AppEvents,
    clients: HashMap<crate::client_channel::ClientId, ClientHandle>,
    rpc_clients: HashSet<crate::client_channel::ClientId>,
    command_client: crate::client_channel::ClientId,
    quit_requested: bool,
    /// Advances when configuration mirrored into attached terminal views changes.
    daemon_config_generation: u64,
    /// Last generation copied into every currently attached terminal view.
    synced_daemon_config_generation: u64,
    pairing: PairingCoordinator,
    credential_repair: Option<CredentialRepair>,
    join_attempt: Option<join::JoinAttempt>,
    next_join_attempt_id: u64,
    rpc_server_selection_issue: Option<OwnedRpcServerSelectionIssue>,
    next_connection_generation: u64,
    active_network_generation: Option<u64>,
    rpc_settings: Option<rpc_settings::RpcSettingsSession>,
    next_rpc_settings_session_id: u64,
    rpc_identity: rpc_identity::RpcIdentityHub,
    appearance: appearance::AppearanceHub,
    pub room: CoreRw<RoomSession>,
    pub network: Option<NetworkClient>,
    pub control_socket: Option<local_control::ControlSocket>,
    pub session_id: Option<SessionId>,
    pub user_id: Option<UserId>,
    /// Whether the connected server lets this session open a DM room it has
    /// not opened before. Assumed until a server says otherwise.
    server_dms_enabled: bool,
    e2e_account_id: Option<rpc::ids::AccountId>,
    requested_voice_room: Option<RoomId>,
    /// The user explicitly left voice this session; suppresses the voice
    /// auto-join on (re-)authentication until the next explicit join.
    voice_left: bool,

    pub voice_state: Arc<AtomicVoiceState>,
    pub voice_tx_enabled: Arc<AtomicBool>,
    pub mic_error: Option<String>,
    pub playback_error: Option<String>,
    pub capture: Option<LiveCapture>,
    /// Fast-attack/slow-release smoothing for the mic VU meter and dB readout,
    pub settings_preview_capture: bool,
    pub settings_preview_refresh_id: Option<u64>,
    pub allow_settings_preview_capture: bool,
    pub playback: Option<LivePlayback>,
    audio_report: Arc<audio::AudioReportHub>,
    active_audio_report: Option<ActiveAudioReport>,
    /// Dedicated playback stream backing the settings loopback monitor when no
    /// call playback exists. `None` when loopback is off or reuses the live call
    /// playback. See [`App::set_loopback_enabled`].
    pub loopback_playback: Option<LivePlayback>,
    /// Lazily started output stream that plays notification sounds outside a
    /// call when [`NotificationSoundMode::Always`] is configured. Torn down by
    /// the tick supervisor once [`Self::notification_playback_idle_at`] passes.
    notification_playback: Option<LivePlayback>,
    notification_playback_idle_at: Option<Instant>,
    /// Backoff after a failed lazy start so a broken output device is not
    /// reopened on every incoming message.
    notification_playback_retry_at: Option<Instant>,
    /// Shared route the capture encoder thread reads to feed local frames into
    /// the loopback stream. Cloned into the capture packet handler; whether a
    /// sink is installed ([`LoopbackTap::is_active`]) is the enabled state, so no
    /// separate flag is kept. Loopback is transient, settings-only, never saved.
    loopback_tap: LoopbackTap,
    output_volume_percent_bits: Arc<AtomicU32>,
    pub soundboard_busy: Arc<AtomicBool>,
    pub soundboard_next_sequence: u32,
    pub echo_control: Arc<EchoCancellationControl>,
    pub voice_packets_received: u64,
    pub voice_bytes_received: u64,
    pub encoder_profile: LiveEncoderProfile,
    pub last_network_notice: Option<String>,
    /// Command-line navigation is retained until the primary terminal channel
    /// exists. Emitting an overlay during `App::new` would otherwise drop it.
    pending_startup_join: Option<PendingJoin>,
    pending_after_welcome: Option<PendingJoin>,
    pub pending_audio_apply: Option<PendingAudioApply>,
    /// When set, the deadline at which outbound voice should be hard-disabled
    /// after a deafen. The teardown is deferred so active senders can transmit
    /// their mute fade-out tail before transport closes.
    pending_voice_teardown_at: Option<Instant>,
    pending_network_commands: VecDeque<NetworkCommand>,
    pending_dm_open: HashMap<(RoomId, UserId), VecDeque<crate::client_channel::ClientId>>,
    pending_dm_clients: HashMap<UserId, VecDeque<crate::client_channel::ClientId>>,
    /// Terminals that explicitly requested review through `/identity`.
    /// Authentication discovery and ordinary DM navigation never populate it.
    pending_identity_review: HashMap<UserId, VecDeque<crate::client_channel::ClientId>>,
    /// Open identity reviews, bound to room, exact key, and trust level. A
    /// display-name refresh does not invalidate the review.
    open_e2e_reviews:
        HashMap<crate::client_channel::ClientId, (RoomId, String, crate::config::E2eTrustLevel)>,
    pending_mutation_clients:
        HashMap<(RoomId, MessageId, bool), VecDeque<crate::client_channel::ClientId>>,
    pending_room_catalog_save: Option<PendingRoomCatalogSave>,
    supervisor: SupervisorState,
    /// Recent audio device events (losses, recoveries, default changes) shown
    /// by `/audio`.
    audio_events: AudioEventLog,
    /// The browser chat-log feed, present only when `[web] enabled = true`.
    web_feed: Option<crate::web_server::WebFeedSender>,
    /// Routes each plaintext screen-share frame to web and native viewers.
    video_fanout: crate::video::VideoFrameFanout,
    /// Web-originated deletes awaiting either a mutation echo or an explicit
    /// server rejection, keyed by room because ids are room-local.
    pending_web_deletes: HashSet<(RoomId, MessageId)>,
    /// Browser pages waiting for the canonical owner to finish one ordered
    /// server fetch. Each browser can have only one load outstanding.
    pending_web_history: HashMap<u64, PendingWebHistoryRequest>,
    /// While a frontend-originated slash command runs, status and notice output
    /// is teed here and returned only to the issuing frontend.
    frontend_command_capture: Option<Vec<local_rpc::model::CommandOutputLine>>,
    /// The in-memory download ring buffer, shared with the network worker and
    /// the web server. Held app-wide so it survives web-server respawns.
    download_store: crate::receive_store::DownloadStore,
    /// The active outbound screen share, if this client is sharing.
    screencast: Option<crate::video::ScreencastHandle>,
    /// Monotonic identity allocated before spawning each screen-share capture.
    next_share_attempt_id: u64,
    /// Daemon-local identity for each newly announced inbound or outbound
    /// share, fencing reused remote stream ids across renderer requests.
    next_live_share_generation: u64,
    /// The resolved capture command that last successfully launched an outbound
    /// screen share. Used by the top-bar `VIDEO OFF` badge to restart exactly
    /// what the user had running.
    cached_screencast_start: Option<CachedScreencastStart>,
    /// The stream id of our active outbound share, set on `ShareStarted`.
    screencast_stream_id: Option<StreamId>,
    /// Active inbound viewer connections, keyed by stream id.
    subscribers: HashMap<StreamId, crate::video::SubscriberHandle>,
    /// Streams with at least one browser viewer. The web server reports stop
    /// only after its final subscribed socket leaves.
    web_viewing_shares: HashSet<StreamId>,
    /// Video connection authentication/protection selected by the current
    /// session handshake, including the concrete peer selected by control.
    video_transport: Option<crate::video::VideoTransport>,
}

struct ActiveAudioReport {
    path: PathBuf,
    deadline: Instant,
    completion: Sender<Result<PathBuf, String>>,
    diagnostics_logs_were_enabled: bool,
}

/// A share this client can view: the secret to bring up a viewer connection and
/// the codec metadata to configure the browser decoder.
struct AvailableShare {
    room_id: RoomId,
    generation: u64,
    view_secret: Vec<u8>,
    sender_name: String,
    codec: String,
    coded_width: u32,
    coded_height: u32,
    /// The decoder `extra_data` descriptor (`avcC`/`hvcC`), built by the
    /// publisher from the stream's parameter sets.
    extradata: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedScreencastStart {
    argv: Vec<String>,
    hevc: bool,
}

impl CachedScreencastStart {
    fn into_command(self) -> local_control::ScreencastCommand {
        local_control::ScreencastCommand::Start {
            argv: self.argv,
            hevc: self.hevc,
        }
    }
}

/// A debounced request to restart audio streams so a slow settings-page change
/// (device, bitrate, denoise, buffer size, latency tuning) takes effect. Rapid
/// edits coalesce into one restart once `deadline` passes.
pub(crate) struct PendingAudioApply {
    capture: bool,
    playback: bool,
    deadline: Instant,
}

pub(crate) struct PendingRoomCatalogSave {
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

/// One audio direction's health, for the TUI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioSideHealth {
    pub(crate) state: AudioHealthState,
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

/// Tick cadence while audio streams need liveness polling: stall detection,
/// stats projection, and talking-indicator decay all read callback counters
/// that no event announces.
const TICK_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Idle tick backstop. Every other tick obligation schedules a deadline in
/// [`App::next_tick_timeout`]; this only bounds detection of a worker that
/// died without sending an event.
const TICK_IDLE_INTERVAL: Duration = Duration::from_secs(1);
const RECOVERY_WINDOW: Duration = Duration::from_secs(30);
const RECOVERY_MAX_ATTEMPTS: usize = 3;
const CAPTURE_STALL_TIMEOUT: Duration = Duration::from_millis(750);
/// Device-observer poll cadence while streams are open and healthy.
const DEVICE_PROBE_INTERVAL_HEALTHY: Duration = Duration::from_secs(5);
/// Faster cadence while a stream is recovering or waiting to move back onto
/// its configured device, so a (re)appearing device is noticed promptly.
const DEVICE_PROBE_INTERVAL_RECOVERING: Duration = Duration::from_secs(2);
/// Device events listed in the interactive `/audio` notice. A bug report ships
/// the whole [`AudioEventLog`] instead: a report filed minutes into a failure
/// still needs the transition that started it, and at the capped 30 s retry
/// backoff a short list has already dropped it.
const AUDIO_STATUS_EVENT_LIMIT: usize = 12;
const LOBBY_TALKING_RELEASE: Duration = Duration::from_millis(200);
/// The talking indicator is intentionally more sensitive than NetEQ's
/// time-scaling VAD so quiet but audible decoded speech still registers.
const LOBBY_TALKING_RMS_THRESHOLD: f32 = 0.001; // -60 dBFS

/// Debounce window before a scheduled audio restart fires. Coalesces rapid
/// settings edits (cycling a choice, typing a buffer size) into one restart.
const AUDIO_APPLY_DEBOUNCE: Duration = Duration::from_millis(400);
const ROOM_CATALOG_SAVE_DEBOUNCE: Duration = Duration::from_millis(400);

/// Grace period outbound voice keeps running after a deafen, so active senders
/// transmit their mute fade-out tail (`LIVE_CAPTURE_MUTE_FADE`) plus an entry
/// silence marker before transport is hard-disabled. Sized to comfortably cover
/// the 60 ms fade and the marker that follows it.
const VOICE_DEAFEN_GRACE: Duration = Duration::from_millis(120);

/// How long the lazy notification output stream lingers after its last clip
/// finishes, so notification bursts reuse one stream instead of reopening the
/// device per sound.
const NOTIFICATION_STREAM_LINGER: Duration = Duration::from_secs(5);
/// Cooldown between lazy notification stream start attempts after a failure.
const NOTIFICATION_START_RETRY: Duration = Duration::from_secs(30);

/// When the lazy notification stream becomes idle: the clip has fully played
/// at 48 kHz plus [`NOTIFICATION_STREAM_LINGER`].
fn notification_idle_deadline(now: Instant, clip_samples: usize) -> Instant {
    let clip =
        Duration::from_micros(clip_samples as u64 * 1_000_000 / u64::from(audio::SAMPLE_RATE));
    now + clip + NOTIFICATION_STREAM_LINGER
}

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
        if self.exhausted || self.next_retry_at.is_none_or(|deadline| now < deadline) {
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

    fn due_at(&self) -> Option<Instant> {
        if self.exhausted {
            None
        } else {
            self.next_retry_at
        }
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

/// Kernel release from `uname`, e.g. `24.6.0` for macOS 15 or `6.11.0-rc4` on
/// Linux.
///
/// The OS name alone does not identify a backend's behavior: CoreAudio device
/// and Bluetooth-profile handling shifts between macOS releases, so a bug
/// report that only says `macos` cannot be matched against a known regression.
#[cfg(unix)]
fn platform_release() -> Option<String> {
    let mut info = std::mem::MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: `uname` fills the caller-provided `utsname` and returns 0 on
    // success; `release` is only read after that check and is NUL-terminated.
    unsafe {
        if libc::uname(info.as_mut_ptr()) != 0 {
            return None;
        }
        let info = info.assume_init();
        Some(
            std::ffi::CStr::from_ptr(info.release.as_ptr())
                .to_string_lossy()
                .into_owned(),
        )
    }
}

#[cfg(not(unix))]
fn platform_release() -> Option<String> {
    None
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScreencastProgress {
    pub(crate) attempt_id: ShareAttemptId,
    pub(crate) stream_id: StreamId,
    pub(crate) total_bytes: u64,
    pub(crate) total_frames: u64,
    pub(crate) rolling_bytes_per_sec: u64,
}

/// An event delivered to the core thread over the single application event
/// channel.
pub(crate) enum AppEvent {
    ClientCommand {
        client_id: crate::client_channel::ClientId,
        command: Box<command::CoreCommand>,
    },
    NetworkFor {
        generation: u64,
        event: NetworkEvent,
    },
    Pairing {
        attempt: u64,
        event: PairingEvent,
    },
    AudioDeviceRefresh(AudioDeviceRefresh),
    AudioDeviceProbe(AudioDeviceProbeEvent),
    Soundboard(SoundboardEvent),
    Voice(local_control::VoiceCommand),
    Screencast(local_control::ScreencastCommand),
    Upload {
        request: UploadFileRequest,
        room: Option<String>,
        reply: Sender<Result<String, String>>,
    },
    SendMessage {
        body: String,
        room: Option<String>,
        reply: Sender<Result<String, String>>,
    },
    #[cfg(unix)]
    ClientAttach {
        stream: std::os::unix::net::UnixStream,
        stdin: std::fs::File,
        stdout: std::fs::File,
        hello: local_control::ClientHello,
    },
    #[cfg(unix)]
    RpcClientAttach {
        stream: std::os::unix::net::UnixStream,
        hello: local_rpc::frame::ClientHello,
        peer: local_control::RpcPeer,
    },
    #[cfg(unix)]
    RpcClientFrame {
        client_id: crate::client_channel::ClientId,
        frame: local_rpc::frame::ClientFrame,
    },
    #[cfg(unix)]
    RpcClientExited(crate::client_channel::ClientId),
    #[cfg(unix)]
    AttachmentStreamWorkerExited {
        client_id: crate::client_channel::ClientId,
        request_id: local_rpc::model::RequestId,
        request_count: u64,
        bytes_served: u64,
        highest_requested_offset: u64,
    },
    ClientDetached(crate::client_channel::ClientId),
    ClientExited(crate::client_channel::ClientId),
    OutputVolume {
        command: local_control::OutputVolumeCommand,
        reply: Sender<Result<f32, String>>,
    },
    Web(crate::web_server::WebRequest),
    /// A theme-reload request from `chatt reload-theme`. Re-reads the config file
    /// and re-resolves the theme; the reply carries a status message on success or
    /// the config diagnostics on failure.
    ReloadTheme {
        styled_diagnostics: bool,
        reply: Sender<Result<String, String>>,
    },
    /// A stylesheet-reload request from `chatt reload-web-css`. Tells connected
    /// browsers to re-fetch the user stylesheet; the reply reports whether a
    /// browser view was there to tell.
    ReloadWebCss {
        reply: Sender<Result<String, String>>,
    },
    /// A config-path query from `chatt reload-theme --watch`, so the watcher
    /// tracks the same file the running client will reload.
    ConfigPath {
        reply: Sender<Result<String, String>>,
    },
    /// A bug report request from `chatt report-bug`, carrying the description.
    ReportBug(String),
    AudioReport {
        request: audio::AudioReportRequest,
        completion: Sender<Result<PathBuf, String>>,
    },
    /// The outbound screen share's capture or publisher thread ended abnormally,
    /// carrying a one-line reason for the user.
    ScreencastFailed {
        attempt_id: ShareAttemptId,
        message: String,
    },
    /// The outbound publisher sent frames successfully and has fresh throughput
    /// counters for the top-bar video badge.
    ScreencastProgress(ScreencastProgress),
    /// An inbound viewer connection changed state. The subscriber retries a lost
    /// server indefinitely, so without this a viewer waiting on a reconnect
    /// looks exactly like one waiting for the next keyframe.
    ShareViewStatus {
        stream_id: StreamId,
        generation: u64,
        state: ShareViewState,
    },
}

/// What an inbound viewer connection is doing, as shown on the browser's row for
/// the share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShareViewState {
    Reconnecting,
    WaitingForKeyframe,
}

impl ShareViewState {
    pub(crate) fn as_u8(self) -> u8 {
        match self {
            Self::WaitingForKeyframe => 0,
            Self::Reconnecting => 1,
        }
    }

    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Reconnecting,
            _ => Self::WaitingForKeyframe,
        }
    }

    /// The wire label. The browser writes it straight into its per-share status,
    /// so these are part of the envelope contract with `web/src/App.tsx`.
    fn label(self) -> &'static str {
        match self {
            Self::Reconnecting => "reconnecting",
            Self::WaitingForKeyframe => "waiting-for-keyframe",
        }
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

/// The `command_output` envelope carrying a web command's captured output.
fn command_output_envelope(lines: &[local_rpc::model::CommandOutputLine]) -> String {
    jsony::object! {
        type: "command_output",
        lines: [
            for line in lines;
            {
                error: line.error,
                text: line.text.as_str(),
            }
        ],
    }
}

/// The `command_candidates` envelope answering an autocomplete candidates
/// request.
fn command_candidates_envelope(
    request_id: u64,
    kind: &str,
    items: &[local_rpc::model::CommandCandidate],
) -> String {
    jsony::object! {
        type: "command_candidates",
        request_id: request_id,
        kind: kind,
        items: [
            for item in items;
            {
                value: item.value.as_str(),
                detail: item.detail.as_deref(),
            }
        ],
    }
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
    crate::settings::parse_byte_size(arg).ok_or_else(|| format!("invalid upload rate: {arg}"))
}

/// The `file_progress` envelope updating a placeholder file message's progress
/// bar. Keyed by `file_id` (the server transfer id) plus `timestamp_ms`, matching
/// the browser's placeholder upsert. Dropped once the enriched attachment arrives.
fn file_progress_envelope(
    file_id: u64,
    timestamp_ms: u64,
    transferred: u64,
    total: u64,
    direction: TransferDirection,
) -> String {
    let direction = match direction {
        TransferDirection::Incoming => "incoming",
        TransferDirection::Outgoing => "outgoing",
    };
    jsony::object! {
        type: "file_progress",
        file_id: file_id,
        timestamp_ms: timestamp_ms,
        transferred: transferred,
        total: total,
        direction: direction,
    }
}

/// The `file_terminal` envelope replacing a placeholder file message's progress
/// bar with a persistent `verb: reason` label (skipped/cancelled/failed). Keyed
/// like [`file_progress_envelope`]. `reason` is null for a bare verb.
fn file_terminal_envelope(
    file_id: u64,
    timestamp_ms: u64,
    verb: TerminalVerb,
    reason: Option<&str>,
) -> String {
    jsony::object! {
        type: "file_terminal",
        file_id: file_id,
        timestamp_ms: timestamp_ms,
        verb: verb.label(),
        reason: reason,
    }
}

/// Reports what a share's viewer connection is doing. The browser writes `state`
/// into its per-share status line; a share it is not watching ignores it.
fn share_status_envelope(stream_id: StreamId, state: ShareViewState) -> String {
    jsony::object! {
        type: "share_status",
        stream_id: stream_id.0,
        state: state.label(),
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

fn delete_error_envelope(target: MessageId, message: &str) -> String {
    jsony::object! {
        type: "delete_error",
        target: format!("{:x}", target.0),
        message: message,
    }
}

fn web_request_result_envelope(
    request_id: u64,
    operation: &str,
    accepted: bool,
    message: Option<&str>,
) -> String {
    jsony::object! {
        type: "request_result",
        request_id: request_id,
        operation: operation,
        accepted: accepted,
        message: message,
    }
}

fn web_action_error_envelope(operation: &str, message: &str) -> String {
    jsony::object! {
        type: "action_error",
        operation: operation,
        message: message,
    }
}

/// Projects borrowed canonical records into a bounded wire window.
fn web_messages_from_canonical<'a>(
    messages: impl IntoIterator<Item = &'a rpc::control::ChatMessage>,
    room: &RoomSession,
    local_user: Option<UserId>,
) -> Vec<crate::web_server::WebMessage> {
    let resolver = |target| room.resolve_web_ref(target);
    let messages = messages.into_iter();
    let mut projected = Vec::with_capacity(messages.size_hint().0);
    for message in messages {
        projected.push(web_message_from_canonical(
            message, room, &resolver, local_user,
        ));
    }
    projected
}

fn web_message_from_canonical(
    message: &rpc::control::ChatMessage,
    room: &RoomSession,
    resolver: &impl Fn(rpc::msgref::MessageRef) -> Option<crate::web_wire::ResolvedRef>,
    local_user: Option<UserId>,
) -> crate::web_server::WebMessage {
    let unverified = room.message_unverified(message.room_id, message.message_id, local_user);
    match message.file_transfer_id {
        Some(transfer_id) => match room.resident_file_detail(
            message.room_id,
            &crate::room_history::FileHistoryKey {
                timestamp_ms: message.timestamp_ms,
                transfer_id,
            },
        ) {
            Some(detail) => crate::web_server::WebMessage::from_history_file(
                message,
                transfer_id,
                &detail.file_name,
                detail.length,
                detail.dimensions(),
                local_user,
                unverified,
            ),
            None => {
                crate::web_server::WebMessage::from_chat(message, resolver, local_user, unverified)
            }
        },
        None => crate::web_server::WebMessage::from_chat(message, resolver, local_user, unverified),
    }
}

fn web_system_message(message: room::SystemMessage) -> crate::web_server::WebSystemMessage {
    crate::web_server::WebSystemMessage {
        id: message.id,
        after_message_id: message.after.map(|id| id.0),
        sender: message.sender,
        body: message.body,
        timestamp_ms: message.timestamp_ms,
        level: match message.kind {
            crate::chat_buffer::NoticeKind::Info => crate::web_server::WebSystemMessageLevel::Info,
            crate::chat_buffer::NoticeKind::Warning => {
                crate::web_server::WebSystemMessageLevel::Warning
            }
            crate::chat_buffer::NoticeKind::Error => {
                crate::web_server::WebSystemMessageLevel::Error
            }
        },
    }
}

/// Registers persistent downloads already on disk so the web view can serve them
/// after a restart. Each configured persistent directory is scanned and its
/// files registered under their on-disk names (first-wins on collision),
/// matching the served names history carries. Live transfers register
/// themselves as they complete.
fn register_existing_downloads(
    config: &config::Config,
    store: &crate::receive_store::DownloadStore,
) {
    for dir in config.persistent_download_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if store.name_available(&name) {
                store.register_disk(name, entry.path());
            }
        }
    }
}

fn spawn_web_feed(
    web: &config::WebConfig,
    download_store: crate::receive_store::DownloadStore,
    max_upload_bytes: u64,
    room_name: String,
    custom_css: Option<PathBuf>,
    events: &EventSender,
) -> Option<crate::web_server::WebFeedSender> {
    let (web_tx, web_rx) = mpsc::channel();
    let feed = match crate::web_server::spawn_with_upload_limit(
        web,
        download_store,
        web_tx,
        web.readonly,
        max_upload_bytes,
        room_name,
        custom_css,
    ) {
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

#[derive(Clone)]
pub(crate) struct NetworkEventSender {
    tx: Sender<AppEvent>,
    generation: u64,
}

#[derive(Clone)]
pub(crate) struct PairingEventSender {
    tx: Sender<AppEvent>,
    attempt: u64,
}

impl EventSender {
    // Preserve `SendError<AppEvent>` so a caller can recover the unsent event;
    // boxing would allocate only to wrap the channel's native error.
    #[allow(clippy::result_large_err)]
    pub(crate) fn send<E: Into<AppEvent>>(
        &self,
        event: E,
    ) -> Result<(), mpsc::SendError<AppEvent>> {
        self.0.send(event.into())
    }

    fn for_network(&self, generation: u64) -> NetworkEventSender {
        NetworkEventSender {
            tx: self.0.clone(),
            generation,
        }
    }

    fn for_pairing(&self, attempt: u64) -> PairingEventSender {
        PairingEventSender {
            tx: self.0.clone(),
            attempt,
        }
    }
}

impl NetworkEventSender {
    #[cfg(test)]
    pub(crate) fn for_test(tx: Sender<AppEvent>) -> Self {
        Self { tx, generation: 0 }
    }

    // Preserve the channel's native error and its recoverable unsent event.
    #[allow(clippy::result_large_err)]
    pub(crate) fn send(&self, event: NetworkEvent) -> Result<(), mpsc::SendError<AppEvent>> {
        self.tx.send(AppEvent::NetworkFor {
            generation: self.generation,
            event,
        })
    }
}

impl PairingEventSender {
    #[cfg(test)]
    pub(crate) fn for_test(tx: Sender<AppEvent>, attempt: u64) -> Self {
        Self { tx, attempt }
    }

    // Preserve the channel's native error and its recoverable unsent event.
    #[allow(clippy::result_large_err)]
    pub(crate) fn send(&self, event: PairingEvent) -> Result<(), mpsc::SendError<AppEvent>> {
        self.tx.send(AppEvent::Pairing {
            attempt: self.attempt,
            event,
        })
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

    fn wait(&self, timeout: Duration) -> Option<AppEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// A join requested on the command line, to be started once the app is running.
#[derive(Clone)]
pub(crate) enum PendingJoin {
    /// Disposable device enrollment. The ticket is absent on the documented
    /// path so it can be pasted into a hidden TUI field.
    Device { ticket: Option<DeviceLinkTicket> },
    /// Invite-based pairing from a `tcj1_` join string.
    Invite(InviteTicket),
    /// Open pairing against a bare `host:port` address.
    Open { addr: String },
    /// A `chatt join` request naming a server by label or `host:port`. Resolved
    /// against the configured servers once the app is constructed.
    Named { specifier: String },
    /// A rejoin of a known server record, from the last-server hint a retaken
    /// master left behind.
    ById(ServerId),
}

/// A [`PendingJoin`] as it travels to an already-running master over the attach
/// socket, so a second `chatt join` or `chatt pair` acts on that session instead
/// of failing.
///
/// Tickets stay in their encoded string form here: the socket is reachable only
/// by the same user, and the master decodes them exactly as it would from argv.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub(crate) enum StartupIntent {
    Device { ticket: Option<String> },
    Invite { ticket: String },
    Open { addr: String },
    Named { specifier: String },
}

impl PendingJoin {
    /// Decodes what a command line asked for, from either this process's argv or
    /// an attaching terminal's.
    pub(crate) fn from_intent(intent: &StartupIntent) -> Result<Self, String> {
        match intent {
            StartupIntent::Device { ticket: None } => Ok(Self::Device { ticket: None }),
            StartupIntent::Device {
                ticket: Some(ticket),
            } => Ok(Self::Device {
                ticket: Some(rpc::control::decode_device_link_ticket(ticket)?),
            }),
            StartupIntent::Invite { ticket } => {
                Ok(Self::Invite(rpc::control::decode_invite_ticket(ticket)?))
            }
            StartupIntent::Open { addr } => Ok(Self::Open {
                addr: crate::cli::parse_pair_address(addr)?,
            }),
            StartupIntent::Named { specifier } => Ok(Self::Named {
                specifier: specifier.clone(),
            }),
        }
    }
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
        #[cfg(not(test))]
        let control_socket = Some(local_control::ControlSocket::spawn(events.sender())?);
        #[cfg(test)]
        let control_socket = None;
        let soundboard_enabled = config.soundboard.enabled;
        let room = RoomSession::new(&config);
        let echo_control = Arc::new(EchoCancellationControl::new(config.audio.echo_cancellation));
        let output_volume_percent_bits =
            Arc::new(AtomicU32::new(config.audio.output_volume.to_bits()));
        let download_store =
            crate::receive_store::DownloadStore::new(config.files.download_memory_bytes());
        // Register persistent downloads already on disk so they remain servable
        // after a restart; live transfers register themselves as they complete.
        register_existing_downloads(&config, &download_store);
        let web_feed = if config.web.enabled {
            spawn_web_feed(
                &config.web,
                download_store.clone(),
                config.files.max_upload_bytes(),
                room.room_name.clone(),
                config.web_css_path(),
                &events.tx,
            )
        } else {
            None
        };
        let video_fanout = crate::video::VideoFrameFanout::new(web_feed.clone());
        let voice_state = Arc::new(AtomicVoiceState::default());
        let audio_report = audio::AudioReportHub::new();
        let app = Self {
            events,
            clients: HashMap::new(),
            rpc_clients: HashSet::new(),
            command_client: crate::client_channel::ClientId::PRIMARY,
            quit_requested: false,
            daemon_config_generation: 0,
            synced_daemon_config_generation: 0,
            pairing: PairingCoordinator::default(),
            credential_repair: None,
            join_attempt: None,
            next_join_attempt_id: 0,
            rpc_server_selection_issue: None,
            next_connection_generation: 0,
            active_network_generation: None,
            rpc_settings: None,
            next_rpc_settings_session_id: 1,
            rpc_identity: rpc_identity::RpcIdentityHub::default(),
            appearance: appearance::AppearanceHub::default(),
            room: CoreRw::new(room),
            network: None,
            control_socket,
            session_id: None,
            user_id: None,
            server_dms_enabled: true,
            e2e_account_id: None,
            requested_voice_room: None,
            voice_left: false,
            voice_state,
            voice_tx_enabled: Arc::new(AtomicBool::new(false)),
            mic_error: None,
            playback_error: None,
            capture: None,
            settings_preview_capture: false,
            settings_preview_refresh_id: None,
            allow_settings_preview_capture: !soundboard_enabled,
            playback: None,
            audio_report,
            active_audio_report: None,
            loopback_playback: None,
            notification_playback: None,
            notification_playback_idle_at: None,
            notification_playback_retry_at: None,
            loopback_tap: LoopbackTap::default(),
            output_volume_percent_bits,
            soundboard_busy: Arc::new(AtomicBool::new(false)),
            soundboard_next_sequence: 0,
            echo_control,
            voice_packets_received: 0,
            voice_bytes_received: 0,
            encoder_profile: LiveEncoderProfile::DRED_20,
            last_network_notice: None,
            pending_startup_join: pending_join,
            pending_after_welcome: None,
            pending_audio_apply: None,
            pending_voice_teardown_at: None,
            pending_network_commands: VecDeque::new(),
            pending_dm_open: HashMap::new(),
            pending_dm_clients: HashMap::new(),
            pending_identity_review: HashMap::new(),
            open_e2e_reviews: HashMap::new(),
            pending_mutation_clients: HashMap::new(),
            pending_room_catalog_save: None,
            supervisor: SupervisorState::default(),
            audio_events: AudioEventLog::default(),
            web_feed,
            video_fanout,
            pending_web_deletes: HashSet::new(),
            pending_web_history: HashMap::new(),
            frontend_command_capture: None,
            download_store,
            screencast: None,
            next_share_attempt_id: 0,
            next_live_share_generation: 0,
            cached_screencast_start: None,
            screencast_stream_id: None,
            subscribers: HashMap::new(),
            web_viewing_shares: HashSet::new(),
            video_transport: None,
            config: CoreRw::new(config),
        };
        Ok(app)
    }

    fn start_pending_join(&mut self, pending: PendingJoin) {
        match pending {
            PendingJoin::Device { ticket } => self.start_device_pairing_prompt(ticket),
            PendingJoin::Invite(ticket) => self.start_join_pairing(ticket),
            PendingJoin::Open { addr } => self.start_open_pairing(addr),
            PendingJoin::Named { specifier } => self.start_named_join(specifier),
            PendingJoin::ById(server_id) => {
                if self.config.server_by_id(server_id).is_none() {
                    self.set_error("the last used server is no longer configured");
                    return;
                }
                self.start_join_with_screen(server_id, self.command_client);
            }
        }
    }

    /// Runs the join or pairing an attaching terminal asked for on its command
    /// line, as if this master had been started with it.
    ///
    /// The attaching terminal owns the command for its duration, so the status,
    /// errors, and screens the join produces land there rather than on whichever
    /// terminal happened to start the session.
    pub(crate) fn start_attach_intent(
        &mut self,
        client_id: crate::client_channel::ClientId,
        intent: &StartupIntent,
    ) {
        let previous = std::mem::replace(&mut self.command_client, client_id);
        match PendingJoin::from_intent(intent) {
            Ok(pending) => self.start_pending_join(pending),
            Err(error) => self.set_error(error),
        }
        self.command_client = previous;
    }

    pub(crate) fn finish_welcome(&mut self, pending_join: Option<PendingJoin>) {
        self.pending_after_welcome = pending_join;
        let base = self.base_screen();
        self.send_to(
            self.command_client,
            TerminalEvent::Navigation(NavigationEvent::ResetBase(base)),
        );
    }

    pub(crate) fn shared_session(&self) -> Arc<parking_lot::RwLock<RoomSession>> {
        self.room.shared()
    }

    pub(crate) fn register_client(
        &mut self,
        client_id: crate::client_channel::ClientId,
        channel: Arc<crate::client_channel::ClientChannel>,
    ) -> ClientView {
        let mut view = ClientView::new(&self.config, self.config.ui.resolve_theme());
        view.voice_state = self.voice_state.clone();
        if let Some(room_id) = self.room.viewed_room {
            view.switch_room(room_id, &self.room);
            self.room.prepare_client_view(client_id, room_id);
        }
        self.clients.insert(client_id, ClientHandle { channel });
        if client_id == crate::client_channel::ClientId::PRIMARY {
            if let Some(pending) = self.pending_startup_join.take() {
                self.start_pending_join(pending);
            }
        }
        view
    }

    fn channel_for(
        &self,
        client_id: crate::client_channel::ClientId,
    ) -> Option<Arc<crate::client_channel::ClientChannel>> {
        self.clients
            .get(&client_id)
            .map(|handle| handle.channel.clone())
    }

    /// Sends one event to a single client.
    pub(crate) fn send_to(
        &mut self,
        client_id: crate::client_channel::ClientId,
        event: TerminalEvent,
    ) {
        if let Some(channel) = self.channel_for(client_id) {
            channel.push(event);
        }
    }

    fn broadcast_base(&mut self, base: BaseScreen) {
        for handle in self.clients.values() {
            handle
                .channel
                .push(TerminalEvent::Navigation(NavigationEvent::ResetBase(
                    base.clone(),
                )));
        }
    }

    fn broadcast_config_changed(&self) {
        for handle in self.clients.values() {
            handle.channel.push(TerminalEvent::ConfigChanged);
        }
    }

    fn broadcast_reset_rooms(&self) {
        for handle in self.clients.values() {
            handle.channel.push(TerminalEvent::ResetRooms);
        }
    }

    fn broadcast_cancel_pending_edit(&self) {
        for handle in self.clients.values() {
            handle.channel.push(TerminalEvent::CancelPendingEdit);
        }
    }

    fn navigate_all(&mut self, base: BaseScreen) {
        self.broadcast_base(base);
    }

    fn pop_mutation_owner(
        &mut self,
        room_id: RoomId,
        target: MessageId,
        delete: bool,
    ) -> Option<crate::client_channel::ClientId> {
        let key = (room_id, target, delete);
        let (owner, empty) = {
            let owners = self.pending_mutation_clients.get_mut(&key)?;
            let owner = owners.pop_front();
            (owner, owners.is_empty())
        };
        if empty {
            self.pending_mutation_clients.remove(&key);
        }
        owner
    }

    pub(crate) fn shared_config(&self) -> Arc<parking_lot::RwLock<Config>> {
        self.config.shared()
    }

    /// Opens the shared state to render threads. No core method may run until
    /// [`Self::acquire_core_state`] has reacquired the guards.
    pub(crate) fn release_core_state(&mut self) {
        self.config.release();
        self.room.release();
    }

    /// Reacquires guards in the global lock order used by the render threads.
    pub(crate) fn acquire_core_state(&mut self) {
        self.room.acquire();
        self.config.acquire();
    }

    #[allow(dead_code)]
    fn handle_core_command(&mut self, command: command::CoreCommand) {
        use command::CoreCommand;

        match command {
            CoreCommand::SendChat { room_id, body } => self.send_chat(room_id, body),
            CoreCommand::SubmitEdit {
                room_id,
                target,
                body,
            } => self.submit_edit(room_id, target, body),
            CoreCommand::RunSlash { room_id, input } => self.run_slash_command(room_id, input),
            CoreCommand::DeleteMessages {
                room_id,
                targets,
                skipped,
            } => {
                let _ = skipped;
                if self.delete_chat_messages(room_id, targets) {
                    self.send_to(self.command_client, TerminalEvent::ClearVisualSelection);
                }
            }
            CoreCommand::SetViewedRoom(room_id) => {
                if !self.set_viewed_room(room_id) {
                    self.set_error("room is no longer available");
                }
            }
            CoreCommand::OpenMessageRef {
                target,
                width,
                height,
            } => {
                if self.set_viewed_room(target.room_id) {
                    self.send_to(
                        self.command_client,
                        TerminalEvent::OpenMessageRef {
                            target,
                            width,
                            height,
                        },
                    );
                } else if let Some(preview) = self.room.cross_room_ref_preview(target) {
                    self.set_status(preview);
                } else {
                    self.set_status("reference points to another room");
                }
            }
            CoreCommand::RequestOlderHistory { room_id } => {
                self.request_older_history(room_id);
            }
            CoreCommand::OpenDm(user_id) => {
                if let Err(error) = self.open_dm_with(user_id) {
                    self.set_error(error);
                }
            }
            CoreCommand::JoinVoice(room_id) => self.join_voice_room(room_id),
            CoreCommand::LeaveVoice => self.leave_voice_command(),
            CoreCommand::ToggleMute => self.toggle_mute(),
            CoreCommand::ToggleDeafen => self.toggle_deafen(),
            CoreCommand::SetVoiceState(state) => self.set_voice_state(state),
            CoreCommand::ToggleUserMute(user_id) => self.toggle_user_mute(user_id),
            CoreCommand::BeginVolumePreview { user_id, value_db } => {
                self.room.begin_volume_preview(user_id, value_db);
            }
            CoreCommand::ApplyVolume { event, mut dialog } => {
                if self.apply_volume_event(event, &mut dialog) {
                    self.navigate_owner(NavigationEvent::CloseOverlay);
                } else {
                    self.navigate_owner(NavigationEvent::ReplaceOverlay(Box::new(
                        OverlaySpec::UserVolume(dialog),
                    )));
                }
            }
            CoreCommand::CancelTransfer(transfer_id) => self.cancel_transfer(transfer_id),
            CoreCommand::SetRoomHeight(height) => self.config.ui.room_height = height,
            CoreCommand::OpenSettings => self.open_settings(),
            CoreCommand::Settings(operation) => self.handle_settings_op(operation),
            CoreCommand::PlaySoundboard(slot) => self.trigger_soundboard_slot(slot),
            CoreCommand::ToggleVideo => self.activate_top_bar_video(),
            CoreCommand::AcceptTransportEncryption { attempt_id } => {
                if let Err(error) = self.accept_join_plaintext(attempt_id) {
                    self.set_error(error);
                }
            }
            CoreCommand::CancelTransportEncryption { attempt_id } => {
                self.decline_join_plaintext(attempt_id);
            }
            CoreCommand::CloseE2eIdentity => {
                self.open_e2e_reviews.remove(&self.command_client);
                self.navigate_owner(NavigationEvent::CloseOverlay);
            }
            CoreCommand::ForgetE2eIdentity(identity) => {
                if let Err(error) = self.forget_e2e_identity(identity) {
                    self.set_error(error);
                }
            }
            CoreCommand::ConfirmE2eIdentity(target) => {
                if let Err(error) = self.confirm_e2e_verification(target) {
                    self.set_error(error);
                }
            }
            CoreCommand::Connect { alias } => self.connect_by_label(&alias),
            CoreCommand::AddServer => self.start_device_pairing_prompt(None),
            CoreCommand::DeleteServer { server_id } => self.delete_server(server_id),
            CoreCommand::SubmitServerEdit {
                request_id,
                draft,
                join,
            } => self.submit_server_edit(request_id, &draft, join),
            CoreCommand::RetryJoin { attempt_id } => self.retry_join(attempt_id),
            CoreCommand::CancelJoin { attempt_id } => self.cancel_join(attempt_id),
            CoreCommand::SaveRoomSettings(draft) => {
                if !self.save_room_settings(&draft) {
                    self.navigate_owner(NavigationEvent::ReplaceScreen(Box::new(
                        ScreenSpec::RoomSettings(draft),
                    )));
                }
            }
            CoreCommand::SaveWelcome {
                draft,
                pending_join,
            } => {
                if self.save_welcome(&draft) {
                    self.finish_welcome(pending_join);
                }
            }
            CoreCommand::UploadPastedImage {
                room_id,
                source,
                raw_name,
            } => {
                if let Err(error) = self.confirm_paste_image_upload(room_id, &source, raw_name) {
                    self.set_error(error);
                }
            }
            CoreCommand::SubmitPairPassword(password) => {
                self.submit_open_pair_password(password);
            }
            CoreCommand::SubmitDevicePair {
                pairing_string,
                device_name,
                overwrite_existing,
            } => self.submit_device_pairing(pairing_string, device_name, overwrite_existing),
            CoreCommand::GenerateDeviceLink => {
                self.send_network_command(NetworkCommand::CreateDeviceLink, true);
            }
            CoreCommand::CancelDeviceLink(redemption_secret_hash) => {
                self.send_network_command(
                    NetworkCommand::CancelDeviceLink {
                        redemption_secret_hash,
                    },
                    true,
                );
            }
            CoreCommand::AcceptPairingPlaintext => {
                self.apply_pairing_input(PairingInput::AcceptPlaintext {
                    owner: self.command_client,
                })
            }
            CoreCommand::CancelPairing => self.cancel_open_pairing(),
            CoreCommand::ClosePairing => self.apply_pairing_input(PairingInput::OwnerClosed {
                owner: self.command_client,
            }),
            CoreCommand::AudioManualReset => self.audio_manual_reset(),
            CoreCommand::ReportBug(description) => self.start_bug_report(description),
            CoreCommand::Quit => self.quit_requested = true,
        }
    }

    fn handle_settings_op(&mut self, operation: command::SettingsOp) {
        use command::SettingsOp;

        if self.room.settings_owner != Some(self.command_client) {
            self.set_error("settings session is no longer owned by this client");
            return;
        }

        if matches!(operation, SettingsOp::Finish) {
            self.room.settings_owner = None;
            if let Some(settings) = self.room.settings.take() {
                let mut session = settings
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                self.finish_settings_session(&mut session);
            }
            return;
        }

        let Some(settings) = self.room.settings.clone() else {
            self.set_error("settings session is no longer active");
            return;
        };
        let mut session = settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match operation {
            SettingsOp::Save => self.save_settings(&mut session),
            SettingsOp::Drive {
                intent,
                commit,
                focus_column,
            } => self.drive_settings(&mut session, intent, commit, focus_column),
            SettingsOp::SetTab(tab) => self.set_settings_tab(&mut session, tab),
            SettingsOp::CycleTab(delta) => {
                let tab = session.tab.cycle(delta);
                self.set_settings_tab(&mut session, tab);
            }
            SettingsOp::MoveFocus(delta) => self.move_settings_focus(&mut session, delta),
            SettingsOp::MoveFocusInsert(delta) => {
                self.move_settings_focus(&mut session, delta);
                session.form.enter_insert_mode();
            }
            SettingsOp::MoveSelection(delta) => {
                self.move_settings_selection(&mut session, delta);
            }
            SettingsOp::CancelOrClose => {
                if !self.cancel_open_audio_picker(&mut session) {
                    self.close_settings(&mut session);
                }
            }
            SettingsOp::RefreshDevices => self.refresh_audio_devices_for_settings(&session),
            SettingsOp::MarkDirty => self.mark_settings_dirty(&mut session),
            SettingsOp::PickerKey(key) => {
                self.handle_open_settings_picker_key(&mut session, key);
            }
            SettingsOp::PickerMouse(mouse) => {
                self.handle_open_settings_picker_mouse(&mut session, mouse);
            }
            SettingsOp::ActivatePickerItem { field, item_index } => {
                self.activate_settings_picker_item(&mut session, field, item_index);
            }
            SettingsOp::Finish => unreachable!("handled before taking settings lock"),
        }
    }

    pub(crate) fn take_quit_requested(&mut self) -> bool {
        let requested = self.quit_requested;
        self.quit_requested = false;
        requested
    }

    pub(crate) fn save_welcome(&mut self, draft: &WelcomeDraft) -> bool {
        if let Some(reason) = draft.invalid() {
            self.set_error(format!("not saved: {reason}"));
            return false;
        }
        let previous_bindings = self.config.ui.default_bindings;
        let previous_theme = self.config.ui.resolve_theme();
        draft.apply_to_config(&mut self.config);
        let theme = self.config.ui.resolve_theme();
        let daemon_config_changed =
            previous_bindings != self.config.ui.default_bindings || previous_theme != theme;
        if daemon_config_changed {
            self.mark_daemon_config_changed();
        }
        match self.config.save_runtime() {
            Ok(path) => {
                self.config.config_path = Some(path.clone());
                self.apply_max_messages();
                self.set_status(format!("setup saved to {}", path.display()));
                true
            }
            Err(error) => {
                self.set_error(error);
                false
            }
        }
    }

    fn start_pending_after_welcome(&mut self) -> bool {
        let Some(pending) = self.pending_after_welcome.take() else {
            return false;
        };
        self.start_pending_join(pending);
        if self.network.is_some() {
            self.navigate_owner(NavigationEvent::ResetBase(BaseScreen::Room));
        }
        true
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

    pub(crate) fn wait_event(&self, timeout: Duration) -> Option<AppEvent> {
        self.events.wait(timeout)
    }

    pub(crate) fn event_sender(&self) -> EventSender {
        self.events.sender()
    }

    /// Dispatches one terminal's command through the core handlers and reports
    /// whether that terminal requested detach.
    pub(crate) fn handle_client_command(
        &mut self,
        client_id: crate::client_channel::ClientId,
        command: command::CoreCommand,
    ) -> bool {
        if !self.clients.contains_key(&client_id) {
            return false;
        }
        if matches!(command, command::CoreCommand::Quit) {
            if client_id == crate::client_channel::ClientId::PRIMARY {
                self.quit_requested = true;
                return false;
            }
            return true;
        }
        let previous = std::mem::replace(&mut self.command_client, client_id);
        self.handle_core_command(command);
        self.command_client = previous;
        false
    }

    pub(crate) fn handle_app_event(&mut self, event: AppEvent) -> Option<HistoryChange> {
        let mut history_change = None;
        match event {
            AppEvent::ClientCommand { .. } => {
                unreachable!("client commands are handled by the runtime")
            }
            AppEvent::NetworkFor { generation, event } => {
                if self.active_network_generation == Some(generation) {
                    history_change = self.handle_network_event_change(event);
                } else if self.join_attempt_generation() == Some(generation) {
                    self.handle_join_network_event(event);
                } else {
                    kvlog::debug!("ignored stale network event", generation);
                }
            }
            AppEvent::Pairing { attempt, event } => {
                if self
                    .credential_repair
                    .as_ref()
                    .is_some_and(|repair| repair.attempt == attempt)
                {
                    self.handle_repair_event(event);
                } else {
                    self.apply_pairing_input(PairingInput::Worker { attempt, event })
                }
            }
            AppEvent::AudioDeviceRefresh(refresh) => self.handle_audio_device_refresh(refresh),
            AppEvent::AudioDeviceProbe(probe) => self.handle_audio_device_probe(probe.result),
            AppEvent::Soundboard(event) => self.handle_soundboard_event(event),
            AppEvent::Voice(command) => self.apply_voice_command(command),
            AppEvent::Screencast(command) => self.handle_screencast_command(command),
            AppEvent::Upload {
                request,
                room,
                reply,
            } => self.handle_control_upload(request, room.as_deref(), reply),
            AppEvent::SendMessage { body, room, reply } => {
                self.handle_control_send_message(body, room.as_deref(), reply)
            }
            #[cfg(unix)]
            AppEvent::ClientAttach { .. } => {
                unreachable!("client attach events are owned by the daemon runtime")
            }
            #[cfg(unix)]
            AppEvent::RpcClientAttach { .. }
            | AppEvent::RpcClientFrame { .. }
            | AppEvent::RpcClientExited(_)
            | AppEvent::AttachmentStreamWorkerExited { .. } => {
                unreachable!("RPC client events are owned by the daemon runtime")
            }
            AppEvent::ClientDetached(_) | AppEvent::ClientExited(_) => {
                unreachable!("client lifecycle events are owned by the daemon runtime")
            }
            AppEvent::OutputVolume { command, reply } => {
                self.handle_output_volume_command(command, reply)
            }
            AppEvent::ReloadTheme {
                styled_diagnostics,
                reply,
            } => self.handle_reload_theme(styled_diagnostics, reply),
            AppEvent::ReloadWebCss { reply } => self.handle_reload_web_css(reply),
            AppEvent::ConfigPath { reply } => self.handle_config_path(reply),
            AppEvent::Web(request) => self.handle_web_request(request),
            AppEvent::ReportBug(description) => self.start_bug_report(description),
            AppEvent::AudioReport {
                request,
                completion,
            } => self.start_audio_report(request, completion),
            AppEvent::ScreencastFailed {
                attempt_id,
                message,
            } => self.handle_screencast_failed(attempt_id, message),
            AppEvent::ScreencastProgress(progress) => self.handle_screencast_progress(progress),
            AppEvent::ShareViewStatus {
                stream_id,
                generation,
                state,
            } => self.handle_share_view_status(stream_id, generation, state),
        }
        history_change
    }

    fn project_history_change_to_web(&self, change: &HistoryChange) {
        if self.room.viewed_room != Some(change.room_id) {
            return;
        }
        let Some(feed) = &self.web_feed else {
            return;
        };
        if change.refresh_window {
            self.send_web_history_snapshot(crate::web_server::WebAudience::All);
            return;
        }
        for message_id in &change.removed {
            feed.send_delete(change.room_id, change.room_generation, message_id.0);
        }
        for system_id in &change.systems_removed {
            feed.send_system_message_delete(change.room_id, change.room_generation, *system_id);
        }
        if let Some(message_id) = change.upserted
            && let Some(message) = self.room.resident_message(change.room_id, message_id)
        {
            feed.send(
                change.room_id,
                change.room_generation,
                web_message_from_canonical(
                    message,
                    &self.room,
                    &|target| self.room.resolve_web_ref(target),
                    self.user_id,
                ),
            );
        }
        if let Some(message) = change
            .system_upserted
            .and_then(|id| self.room.system_message(change.room_id, id))
        {
            feed.send_system_message(
                change.room_id,
                change.room_generation,
                web_system_message(message),
            );
        }
    }

    /// Applies a CLI-driven voice command through the same App methods the UI
    /// keybindings and top-bar buttons use.
    fn apply_voice_command(&mut self, command: local_control::VoiceCommand) {
        match command {
            local_control::VoiceCommand::ToggleMute => self.toggle_mute(),
            local_control::VoiceCommand::ToggleDeafen => self.toggle_deafen(),
            local_control::VoiceCommand::SetVoiceState(state) => self.set_voice_state(state),
        }
    }

    fn handle_output_volume_command(
        &mut self,
        command: local_control::OutputVolumeCommand,
        reply: Sender<Result<f32, String>>,
    ) {
        let value = match command {
            local_control::OutputVolumeCommand::Query => self.config.audio.output_volume,
            local_control::OutputVolumeCommand::Set(value) => self.set_output_volume(value),
            local_control::OutputVolumeCommand::Adjust(delta) => {
                self.set_output_volume(self.config.audio.output_volume + delta)
            }
        };
        let _ = reply.send(Ok(value));
    }

    fn handle_control_upload(
        &mut self,
        request: UploadFileRequest,
        room: Option<&str>,
        reply: Sender<Result<String, String>>,
    ) {
        if self.network.is_none() {
            let _ = reply.send(Err("not connected to a server".to_string()));
            return;
        }
        let room_id = match room {
            Some(selector) => match self.room.resolve_room_selector(selector) {
                Ok(room_id) => Some(room_id),
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            },
            None => self.room.viewed_room,
        };
        let message = format!("queued upload {}", request.queued_label());
        if !self.send_network_command(NetworkCommand::UploadFile { room_id, request }, true) {
            let _ = reply.send(Err("not connected to a server".to_string()));
        } else {
            let _ = reply.send(Ok(message));
        }
    }

    fn handle_control_send_message(
        &mut self,
        body: String,
        room: Option<&str>,
        reply: Sender<Result<String, String>>,
    ) {
        if body.trim().is_empty() {
            let _ = reply.send(Err("chat message is empty".to_string()));
            return;
        }
        if body.len() > rpc::control::MAX_CHAT_BODY_BYTES {
            self.handle_control_upload(
                UploadFileRequest::from_bytes(
                    local_control::oversized_message_file_name(),
                    body.into_bytes(),
                ),
                room,
                reply,
            );
            return;
        }
        if self.network.is_none() {
            let _ = reply.send(Err("not connected to a server".to_string()));
            return;
        }
        let room_id = match room {
            Some(selector) => match self.room.resolve_room_selector(selector) {
                Ok(room_id) => room_id,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            },
            None => match self.room.viewed_room {
                Some(room_id) => room_id,
                None => {
                    let _ = reply.send(Err("no room selected".to_string()));
                    return;
                }
            },
        };
        let room_name = self
            .room
            .room_name_of(room_id)
            .unwrap_or("selected room")
            .to_string();
        if !self.send_network_command(NetworkCommand::SendChat { room_id, body }, true) {
            let _ = reply.send(Err("not connected to a server".to_string()));
        } else {
            let _ = reply.send(Ok(format!("queued message to {room_name}")));
        }
    }

    /// Re-reads the config file and re-resolves the theme, replying with a status
    /// message or the config diagnostics. Only the theme-relevant `[ui]` fields
    /// are swapped; every other live config section is left untouched, and the
    /// current theme is kept if the file no longer parses.
    fn handle_reload_theme(
        &mut self,
        styled_diagnostics: bool,
        reply: Sender<Result<String, String>>,
    ) {
        let path = self
            .config
            .config_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let reloaded = match Config::reload(path.as_deref(), styled_diagnostics) {
            Ok(config) => config,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let previous_theme = self.config.ui.resolve_theme();
        self.config.ui.theme = reloaded.ui.theme;
        self.config.ui.themes = reloaded.ui.themes;
        if self.config.ui.resolve_theme() != previous_theme {
            self.mark_daemon_config_changed();
        }
        let _ = reply.send(Ok("theme reloaded".to_string()));
    }

    /// Tells every connected browser to re-fetch the user stylesheet. The web
    /// server reads the file per request, so nothing is reloaded here — this
    /// only saves the reader a manual refresh.
    fn handle_reload_web_css(&mut self, reply: Sender<Result<String, String>>) {
        let Some(feed) = &self.web_feed else {
            let _ = reply.send(Err("browser view is not running".to_string()));
            return;
        };
        feed.reload_css();
        let _ = reply.send(Ok("web css reloaded".to_string()));
    }

    fn handle_config_path(&mut self, reply: Sender<Result<String, String>>) {
        let Some(path) = &self.config.config_path else {
            let _ = reply.send(Err("running client has no config path".to_string()));
            return;
        };
        let _ = reply.send(Ok(path.to_string_lossy().into_owned()));
    }

    fn set_output_volume(&mut self, value: f32) -> f32 {
        let value = config::snap_output_volume_percent(value);
        self.config.audio.output_volume = value;
        self.apply_output_volume_setting();
        self.set_status(format!(
            "output volume {}",
            config::output_volume_percent_label(value)
        ));
        value
    }

    /// Applies a CLI-driven screencast command. `Start` replaces any active
    /// share, while `Toggle` stops an active share or otherwise starts one.
    fn handle_screencast_command(&mut self, command: local_control::ScreencastCommand) {
        match command {
            local_control::ScreencastCommand::Start { argv, hevc } => {
                self.start_screencast(argv, hevc);
            }
            local_control::ScreencastCommand::Toggle { argv, hevc } => {
                if self.screencast.is_some() {
                    self.stop_screencast_to_off();
                    return;
                }
                self.start_screencast(argv, hevc);
            }
            local_control::ScreencastCommand::Stop => {
                self.stop_screencast_to_off();
            }
        }
    }

    fn start_screencast(&mut self, argv: Vec<String>, hevc: bool) {
        if self.screencast.is_some() {
            self.teardown_own_share(true);
        }
        if self.room.voice_room.is_none() {
            self.fail_screencast_start("join a voice call before sharing");
            return;
        }
        let Some(network) = &self.network else {
            self.fail_screencast_start("connect before sharing your screen");
            return;
        };
        let network_sender = network.sender();
        let Some(video_transport) = self.video_transport else {
            self.fail_screencast_start(
                "screen share failed: video transport is not ready".to_string(),
            );
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
        let cached_start = CachedScreencastStart {
            argv: argv.clone(),
            hevc,
        };
        let video_fanout = self.video_fanout.clone();
        let events = self.events.sender();

        self.next_share_attempt_id = self.next_share_attempt_id.wrapping_add(1).max(1);
        let attempt_id = ShareAttemptId(self.next_share_attempt_id);
        match crate::video::start_screencast(
            attempt_id,
            argv,
            codec,
            network_sender,
            video_transport,
            video_fanout,
            events,
        ) {
            Ok(handle) => {
                self.room.screencast_status.start();
                self.screencast = Some(handle);
                self.cached_screencast_start = Some(cached_start);
                self.set_status("starting screen share");
            }
            Err(error) => self.fail_screencast_start(format!("screen share failed: {error}")),
        }
    }

    fn stop_screencast_to_off(&mut self) {
        let had_restartable_video = self.screencast.is_some()
            || matches!(
                self.room.screencast_status.phase,
                ScreencastPhase::Starting | ScreencastPhase::Live | ScreencastPhase::Off
            )
            || self.cached_screencast_start.is_some();
        self.teardown_own_share(true);
        if had_restartable_video {
            self.room.screencast_status.turn_off();
            self.set_status("video off");
        } else {
            self.room.screencast_status.clear_active();
            self.set_status("screen share stopped");
        }
    }

    fn fail_screencast_start(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.room.screencast_status.fail(reason.clone());
        self.set_error(reason);
    }

    /// Handles the publisher reporting that its capture or connection ended
    /// abnormally. Tears the dead share down so a retry starts clean, and surfaces
    /// the reason (the capture's stderr tail explains a bad command).
    fn handle_screencast_failed(&mut self, attempt_id: ShareAttemptId, reason: String) {
        if self
            .screencast
            .as_ref()
            .is_none_or(|handle| handle.attempt_id() != attempt_id)
        {
            kvlog::debug!(
                "ignoring stale screen share failure",
                attempt_id = attempt_id.0
            );
            return;
        }
        self.room.screencast_status.fail(reason.clone());
        self.teardown_own_share(true);
        self.set_error(reason);
    }

    fn fail_screencast_if_running(&mut self, reason: impl Into<String>, notify_server: bool) {
        if self.screencast.is_none()
            && !matches!(
                self.room.screencast_status.phase,
                ScreencastPhase::Starting | ScreencastPhase::Live
            )
        {
            return;
        }
        self.room.screencast_status.fail(reason.into());
        self.teardown_own_share(notify_server);
    }

    fn handle_screencast_progress(&mut self, progress: ScreencastProgress) {
        if self
            .screencast
            .as_ref()
            .is_none_or(|handle| handle.attempt_id() != progress.attempt_id)
        {
            return;
        }
        self.room.screencast_status.progress(
            progress.stream_id,
            progress.total_bytes,
            progress.total_frames,
            progress.rolling_bytes_per_sec,
        );
    }

    fn allocate_live_share_generation(&mut self) -> u64 {
        self.next_live_share_generation = self.next_live_share_generation.wrapping_add(1).max(1);
        self.next_live_share_generation
    }

    fn replace_live_share_stream(&mut self, stream_id: StreamId) {
        self.web_viewing_shares.remove(&stream_id);
        if let Some(mut subscriber) = self.subscribers.remove(&stream_id) {
            subscriber.stop();
        }
        self.video_fanout.close_stream(stream_id);
    }

    /// Relays a viewer connection's state to every browser showing the share.
    /// Nothing else recovers this: the subscriber reconnects on its own, so a
    /// stalled viewer would otherwise sit on a black canvas with no explanation.
    fn handle_share_view_status(
        &mut self,
        stream_id: StreamId,
        generation: u64,
        state: ShareViewState,
    ) {
        // The subscriber may outlive the share by one event.
        if !self.subscribers.contains_key(&stream_id)
            || self
                .room
                .available_shares
                .get(&stream_id)
                .is_none_or(|share| share.generation != generation)
        {
            return;
        }
        if let Some(feed) = &self.web_feed {
            feed.send_share_status(share_status_envelope(stream_id, state));
        }
        if state == ShareViewState::Reconnecting {
            let name = self
                .room
                .available_shares
                .get(&stream_id)
                .map(|share| share.sender_name.as_str())
                .unwrap_or("screen share");
            self.set_status(format!("reconnecting to {name}'s screen share"));
        }
    }

    /// Stops this client's outbound share, notifying the server so viewers tear
    /// down and clearing the local self-view from this client's own browser.
    fn teardown_own_share(&mut self, notify_server: bool) {
        let stream_id = self.screencast_stream_id.take();
        if let Some(stream_id) = stream_id {
            if notify_server && let Some(network) = &self.network {
                let _ = network
                    .sender()
                    .send(NetworkCommand::StopShare { stream_id });
            }
        }
        // Joining the producer is the fence: after it returns, no late local
        // frame can reopen a cache that the following close removes.
        if let Some(mut handle) = self.screencast.take() {
            handle.stop();
        }
        if let Some(stream_id) = stream_id {
            self.room.available_shares.remove(&stream_id);
            self.web_viewing_shares.remove(&stream_id);
            self.video_fanout.close_stream(stream_id);
            if let Some(feed) = &self.web_feed {
                feed.send_share_ended(stream_id.0, share_ended_envelope(stream_id));
            }
        }
    }

    /// Drops one inbound share: its cached frames, its viewer connection, and
    /// the browser's row for it. The browser keys its player state by stream id
    /// and the server restarts ids from 1, so a share the client forgets
    /// without telling the browser leaves the next share to reuse that id
    /// playing against the previous one's cached frames.
    fn drop_share(&mut self, stream_id: StreamId) {
        self.room.available_shares.remove(&stream_id);
        self.web_viewing_shares.remove(&stream_id);
        if let Some(mut subscriber) = self.subscribers.remove(&stream_id) {
            subscriber.stop();
        }
        self.video_fanout.close_stream(stream_id);
        if let Some(feed) = &self.web_feed {
            feed.send_share_ended(stream_id.0, share_ended_envelope(stream_id));
        }
    }

    /// Stops the outbound share and every inbound viewer connection.
    fn stop_all_shares(&mut self) {
        self.teardown_own_share(true);
        if self.room.screencast_status.phase != ScreencastPhase::Failed {
            self.room.screencast_status.clear_active();
        }
        self.screencast_stream_id = None;
        for stream_id in self
            .room
            .available_shares
            .keys()
            .copied()
            .collect::<Vec<_>>()
        {
            self.drop_share(stream_id);
        }
        // Backstop for a viewer whose share is already out of the catalog.
        for (_, mut subscriber) in self.subscribers.drain() {
            subscriber.stop();
        }
        self.web_viewing_shares.clear();
    }

    fn clear_shares_for_voice_room(&mut self, room_id: RoomId) {
        let stream_ids = self
            .room
            .available_shares
            .iter()
            .filter_map(|(stream_id, share)| (share.room_id == room_id).then_some(*stream_id))
            .collect::<Vec<_>>();
        for stream_id in stream_ids {
            self.drop_share(stream_id);
            if self.screencast_stream_id == Some(stream_id) {
                self.room
                    .screencast_status
                    .fail("voice call ended during screen share".to_string());
                self.screencast_stream_id = None;
                if let Some(mut handle) = self.screencast.take() {
                    handle.stop();
                }
            }
        }
    }

    /// Handles a browser request relayed from the web view.
    fn handle_web_request(&mut self, request: crate::web_server::WebRequest) {
        match request {
            crate::web_server::WebRequest::HistorySnapshot { client } => {
                self.send_web_history_snapshot(crate::web_server::WebAudience::One(client));
            }
            crate::web_server::WebRequest::LoadOlder {
                client,
                room_id,
                room_generation,
                before_message_id,
                limit,
            } => {
                self.send_web_older_page(
                    client,
                    room_id,
                    room_generation,
                    before_message_id,
                    limit,
                );
            }
            crate::web_server::WebRequest::RefPreview {
                client,
                room_id,
                room_generation,
                message_id,
            } => {
                self.send_web_ref_preview(client, room_id, room_generation, message_id);
            }
            crate::web_server::WebRequest::PlayShare { client, stream_id } => {
                self.start_view(client, StreamId(stream_id))
            }
            crate::web_server::WebRequest::StopShare { stream_id } => {
                self.stop_view(StreamId(stream_id))
            }
            crate::web_server::WebRequest::SendChat {
                client,
                request_id,
                body,
            } => {
                let accepted = if body.trim().is_empty() {
                    self.report_web_request_result(
                        client,
                        request_id,
                        "send_message",
                        false,
                        Some("chat message is empty"),
                    );
                    false
                } else if self.room.viewed_room.is_none() {
                    self.set_error("no room selected");
                    self.report_web_request_result(
                        client,
                        request_id,
                        "send_message",
                        false,
                        Some("no room selected"),
                    );
                    false
                } else if self.network.is_none() && !self.room.network_disconnected {
                    self.set_error("select a server before sending messages");
                    self.report_web_request_result(
                        client,
                        request_id,
                        "send_message",
                        false,
                        Some("select a server before sending messages"),
                    );
                    false
                } else {
                    self.send_chat_to_viewed(body);
                    true
                };
                if accepted {
                    self.report_web_request_result(client, request_id, "send_message", true, None);
                }
            }
            crate::web_server::WebRequest::EditChat {
                client,
                request_id,
                target,
                body,
            } => {
                let target = MessageId(target);
                match self.room.validate_web_edit(target) {
                    Ok(room_id) if !body.trim().is_empty() => {
                        if self.network.is_none() && !self.room.network_disconnected {
                            let message = "select a server before editing messages";
                            self.set_error(message);
                            self.report_web_request_result(
                                client,
                                request_id,
                                "edit_message",
                                false,
                                Some(message),
                            );
                        } else {
                            self.send_network_command(
                                NetworkCommand::EditChat {
                                    room_id,
                                    target,
                                    body,
                                },
                                true,
                            );
                            self.report_web_request_result(
                                client,
                                request_id,
                                "edit_message",
                                true,
                                None,
                            );
                        }
                    }
                    Ok(_) => {
                        self.set_error("chat message is empty");
                        self.report_web_request_result(
                            client,
                            request_id,
                            "edit_message",
                            false,
                            Some("chat message is empty"),
                        );
                    }
                    Err(denied) => {
                        let message = denied.status();
                        self.set_error(message);
                        self.report_web_request_result(
                            client,
                            request_id,
                            "edit_message",
                            false,
                            Some(message),
                        );
                    }
                }
            }
            crate::web_server::WebRequest::DeleteChat {
                client,
                request_id,
                target,
            } => {
                let target = MessageId(target);
                match self.room.validate_web_delete(target) {
                    Ok(room_id) => {
                        if self.network.is_none() {
                            let message = "select a server before deleting messages";
                            self.set_error(message);
                            self.report_web_delete_error(target, message);
                            self.report_web_request_result(
                                client,
                                request_id,
                                "delete_message",
                                false,
                                Some(message),
                            );
                        } else {
                            self.pending_web_deletes.insert((room_id, target));
                            self.delete_chat_messages(room_id, vec![target]);
                            self.report_web_request_result(
                                client,
                                request_id,
                                "delete_message",
                                true,
                                None,
                            );
                        }
                    }
                    Err(denied) => {
                        let message = denied.status();
                        self.set_error(message);
                        self.report_web_delete_error(target, message);
                        self.report_web_request_result(
                            client,
                            request_id,
                            "delete_message",
                            false,
                            Some(message),
                        );
                    }
                }
            }
            crate::web_server::WebRequest::UploadFile {
                client,
                request_id,
                path,
                name,
            } => {
                if self.room.viewed_room.is_none() {
                    let _ = std::fs::remove_file(&path);
                    self.report_web_request_result(
                        client,
                        request_id,
                        "upload_finish",
                        false,
                        Some("no room selected"),
                    );
                } else if self.network.is_none() && !self.room.network_disconnected {
                    let _ = std::fs::remove_file(&path);
                    self.report_web_request_result(
                        client,
                        request_id,
                        "upload_finish",
                        false,
                        Some("select a server before uploading files"),
                    );
                } else {
                    self.send_network_command(
                        NetworkCommand::UploadFile {
                            room_id: self.room.viewed_room,
                            request: UploadFileRequest {
                                path,
                                name_override: Some(name),
                                delete_after_open: true,
                                inline_bytes: None,
                            },
                        },
                        true,
                    );
                    self.report_web_request_result(client, request_id, "upload_finish", true, None);
                }
            }
            crate::web_server::WebRequest::CancelTransfer {
                client,
                request_id,
                transfer_id,
            } => {
                self.cancel_transfer(FileTransferId(transfer_id));
                self.report_web_request_result(client, request_id, "abort_transfer", true, None);
            }
            crate::web_server::WebRequest::RunCommand {
                client,
                request_id,
                body,
            } => self.run_web_command(client, request_id, body),
            crate::web_server::WebRequest::CommandCandidates {
                client,
                request_id,
                kind,
            } => self.send_command_candidates(client, request_id, kind),
        }
    }

    fn send_web_history_snapshot(&self, audience: crate::web_server::WebAudience) {
        let Some(feed) = &self.web_feed else {
            return;
        };
        let Some(room_id) = self.room.viewed_room else {
            // No room is shown, so there is no generation to stamp. Room
            // generations start at one, so zero can never collide with a real
            // one and every later request re-syncs against this empty window.
            feed.send_history_window(
                audience,
                crate::web_server::HistoryWindowKind::Sync,
                RoomId(0),
                0,
                Vec::new(),
                Vec::new(),
                None,
                true,
            );
            return;
        };
        let Some(history) = self.room.history_ref(room_id) else {
            return;
        };
        let page = history.latest_page(crate::web_server::SYNC_WINDOW);
        let projected =
            web_messages_from_canonical(page.messages.iter().copied(), &self.room, self.user_id);
        let system_messages = self
            .room
            .system_messages(room_id)
            .into_iter()
            .map(web_system_message)
            .collect();
        feed.send_history_window(
            audience,
            crate::web_server::HistoryWindowKind::Sync,
            room_id,
            page.room_generation,
            projected,
            system_messages,
            page.older_cursor,
            page.at_start,
        );
    }

    fn send_web_older_page(
        &mut self,
        client: u64,
        room_id: RoomId,
        requested_generation: u64,
        before: MessageId,
        limit: u64,
    ) {
        self.send_web_older_page_inner(client, room_id, requested_generation, before, limit, true);
    }

    fn send_web_older_page_inner(
        &mut self,
        client: u64,
        room_id: RoomId,
        requested_generation: u64,
        before: MessageId,
        limit: u64,
        allow_fetch: bool,
    ) {
        if self.web_feed.is_none() {
            return;
        }
        let current_generation = self.room.room_generation(room_id);
        if self.room.viewed_room != Some(room_id)
            || current_generation != Some(requested_generation)
        {
            self.send_web_history_snapshot(crate::web_server::WebAudience::One(client));
            return;
        }
        let page_limit = (limit as usize).clamp(1, crate::web_server::MAX_PAGE);
        let (canonical_before, _) = self.room.history_cursor(room_id);
        let Some(history) = self.room.history_ref(room_id) else {
            self.send_web_older_empty(client, room_id, requested_generation);
            return;
        };
        let page = if let Some(page) = history.page_before(before, page_limit) {
            page
        } else if canonical_before == Some(before) || !allow_fetch {
            history.page_before_position(before, page_limit)
        } else {
            // The cursor is neither resident nor the canonical paging cursor.
            // Handing back the canonical one lets this tab resume from a usable
            // position instead of being reset to the bottom.
            self.send_web_older_empty(client, room_id, requested_generation);
            return;
        };
        if !page.messages.is_empty() || page.at_start {
            let projected = web_messages_from_canonical(
                page.messages.iter().copied(),
                &self.room,
                self.user_id,
            );
            if let Some(feed) = &self.web_feed {
                feed.send_history_window(
                    crate::web_server::WebAudience::One(client),
                    crate::web_server::HistoryWindowKind::Older,
                    room_id,
                    requested_generation,
                    projected,
                    Vec::new(),
                    page.older_cursor,
                    page.at_start,
                );
            }
            return;
        }
        if !allow_fetch {
            self.send_web_older_empty(client, room_id, requested_generation);
            return;
        }

        self.pending_web_history.insert(
            client,
            PendingWebHistoryRequest {
                room_id,
                room_generation: requested_generation,
                before,
                limit,
            },
        );
        if self.room.history_fetch_active(room_id) {
            return;
        }
        let Some((_, network_before, network_limit)) = self.room.older_history_request(room_id)
        else {
            self.pending_web_history.remove(&client);
            self.send_web_older_empty(client, room_id, requested_generation);
            return;
        };
        if !self.send_network_command(
            NetworkCommand::FetchHistory {
                room_id,
                before: network_before,
                limit: network_limit,
            },
            false,
        ) {
            self.room.abort_history_fetch(room_id, network_before);
            self.pending_web_history.remove(&client);
            self.send_web_older_empty(client, room_id, requested_generation);
        }
    }

    /// Answers an older-page request that cannot be served with an empty frame
    /// rather than a fresh snapshot.
    ///
    /// A snapshot is an authoritative reset: the browser replaces its window,
    /// re-pins to the bottom and resumes any pending reference jump. Using one
    /// as a "no" turns an un-pageable request into an unbounded page-up /
    /// bounce-down loop, and teleports an offline reader away from what they
    /// were reading. An empty frame leaves them in place, and when nothing
    /// older can ever arrive it tells them to stop asking.
    fn send_web_older_empty(&self, client: u64, room_id: RoomId, requested_generation: u64) {
        let Some(feed) = &self.web_feed else {
            return;
        };
        let (older_cursor, at_start) = self.room.history_cursor(room_id);
        feed.send_history_window(
            crate::web_server::WebAudience::One(client),
            crate::web_server::HistoryWindowKind::Older,
            room_id,
            requested_generation,
            Vec::new(),
            Vec::new(),
            older_cursor,
            at_start || self.room.older_paging_exhausted(room_id),
        );
    }

    /// Releases every outstanding browser page request, so a disconnect cannot
    /// leave a tab spinning on a reply that will never come.
    fn abandon_pending_web_history(&mut self) {
        for (client, request) in std::mem::take(&mut self.pending_web_history) {
            self.send_web_older_empty(client, request.room_id, request.room_generation);
        }
    }

    fn complete_pending_web_history(&mut self, room_id: RoomId) {
        if self.room.history_fetch_active(room_id) {
            return;
        }
        let clients = self
            .pending_web_history
            .iter()
            .filter_map(|(client, request)| (request.room_id == room_id).then_some(*client))
            .collect::<Vec<_>>();
        for client in clients {
            let Some(request) = self.pending_web_history.remove(&client) else {
                continue;
            };
            self.send_web_older_page_inner(
                client,
                request.room_id,
                request.room_generation,
                request.before,
                request.limit,
                false,
            );
        }
    }

    fn send_web_ref_preview(
        &self,
        client: u64,
        room_id: RoomId,
        requested_generation: u64,
        message_id: MessageId,
    ) {
        if self.web_feed.is_none() {
            return;
        }
        // The browser stamps every request with the generation of the room it
        // is showing, which is what `load_older` is validated against too.
        let viewed_generation = self
            .room
            .viewed_room
            .and_then(|viewed| self.room.room_generation(viewed));
        if viewed_generation != Some(requested_generation) {
            self.send_web_history_snapshot(crate::web_server::WebAudience::One(client));
            return;
        }
        let projected =
            self.room
                .reference_message(room_id, message_id)
                .map(
                    |(message, detail)| match (message.file_transfer_id, detail) {
                        (Some(transfer_id), Some(detail)) => {
                            crate::web_server::WebMessage::from_history_file(
                                &message,
                                transfer_id,
                                &detail.file_name,
                                detail.length,
                                detail.dimensions(),
                                self.user_id,
                                self.room
                                    .message_unverified(room_id, message_id, self.user_id),
                            )
                        }
                        _ => crate::web_server::WebMessage::from_chat(
                            &message,
                            &|target| self.room.resolve_web_ref(target),
                            self.user_id,
                            self.room
                                .message_unverified(room_id, message_id, self.user_id),
                        ),
                    },
                );
        if let Some(feed) = &self.web_feed {
            feed.send_ref_preview(client, room_id, requested_generation, message_id, projected);
        }
    }

    /// Runs a browser-composed slash command through the shared dispatch,
    /// returning its status/notice output to the issuing tab.
    fn run_web_command(&mut self, client: u64, request_id: u64, body: String) {
        match self.run_web_command_captured(body) {
            Err(message) => {
                self.report_web_request_result(
                    client,
                    request_id,
                    "run_command",
                    false,
                    Some(&message),
                );
            }
            Ok(lines) => {
                self.report_web_request_result(client, request_id, "run_command", true, None);
                if lines.is_empty() {
                    return;
                }
                if let Some(feed) = &self.web_feed {
                    feed.send_command_reply(client, command_output_envelope(&lines));
                }
            }
        }
    }

    /// Gates and dispatches a web slash command, teeing its output. `Err` is a
    /// gating failure (unknown or TUI-only command); the command did not run.
    fn run_web_command_captured(
        &mut self,
        body: String,
    ) -> Result<Vec<local_rpc::model::CommandOutputLine>, String> {
        self.run_frontend_command_captured(self.command_client, self.room.viewed_room, body)
            .map_err(|error| error.replace("this frontend", "the web view"))
    }

    pub(crate) fn run_frontend_command_captured(
        &mut self,
        client_id: crate::client_channel::ClientId,
        room_id: Option<RoomId>,
        body: String,
    ) -> Result<Vec<local_rpc::model::CommandOutputLine>, String> {
        let body = body.trim().to_string();
        if body.contains('\r') || body.contains('\n') {
            return Err("slash commands must be a single line".into());
        }
        let first_token = body.split_whitespace().next().unwrap_or("");
        commands::frontend_command_gate(first_token)?;
        debug_assert!(self.frontend_command_capture.is_none());
        let previous_client = std::mem::replace(&mut self.command_client, client_id);
        self.frontend_command_capture = Some(Vec::new());
        self.run_slash_command(room_id, body);
        let output = self.frontend_command_capture.take().unwrap_or_default();
        self.command_client = previous_client;
        Ok(output)
    }

    /// Answers the web autocomplete's request for argument candidates.
    fn send_command_candidates(
        &mut self,
        client: u64,
        request_id: u64,
        kind: crate::web_server::CandidateKind,
    ) {
        let rpc_kind = match kind {
            crate::web_server::CandidateKind::User => local_rpc::model::CommandCandidateKind::User,
            crate::web_server::CandidateKind::Room => local_rpc::model::CommandCandidateKind::Room,
            crate::web_server::CandidateKind::Sound => {
                local_rpc::model::CommandCandidateKind::Sound
            }
        };
        let items = self.frontend_command_candidates(rpc_kind);
        if let Some(feed) = &self.web_feed {
            feed.send_command_reply(
                client,
                command_candidates_envelope(request_id, kind.wire_name(), &items),
            );
        }
    }

    pub(crate) fn frontend_command_candidates(
        &self,
        kind: local_rpc::model::CommandCandidateKind,
    ) -> Vec<local_rpc::model::CommandCandidate> {
        let mut items = match kind {
            local_rpc::model::CommandCandidateKind::User => self
                .room
                .username_candidates()
                .into_iter()
                .map(|value| local_rpc::model::CommandCandidate {
                    value,
                    detail: None,
                })
                .collect::<Vec<_>>(),
            local_rpc::model::CommandCandidateKind::Room => self
                .room
                .room_name_candidates()
                .into_iter()
                .map(|value| local_rpc::model::CommandCandidate {
                    value,
                    detail: None,
                })
                .collect::<Vec<_>>(),
            local_rpc::model::CommandCandidateKind::Sound => self
                .config
                .soundboard
                .clips
                .iter()
                .enumerate()
                .map(|(index, clip)| local_rpc::model::CommandCandidate {
                    value: clip.name.clone(),
                    detail: Some(format!("slot {}", index + 1)),
                })
                .collect::<Vec<_>>(),
        };
        items.retain(|item| item.validate().is_ok());
        items.sort_by(|left, right| {
            left.value
                .cmp(&right.value)
                .then_with(|| left.detail.cmp(&right.detail))
        });
        items.sort_by_cached_key(|item| item.value.to_lowercase());
        items.dedup();
        items.truncate(local_rpc::MAX_COMMAND_CANDIDATES);
        items
    }

    fn report_web_delete_error(&self, target: MessageId, message: &str) {
        if let Some(feed) = &self.web_feed {
            feed.send_delete_error(delete_error_envelope(target, message));
        }
    }

    fn report_web_request_result(
        &self,
        client: u64,
        request_id: u64,
        operation: &str,
        accepted: bool,
        message: Option<&str>,
    ) {
        if let Some(feed) = &self.web_feed {
            feed.send_request_result(
                client,
                web_request_result_envelope(request_id, operation, accepted, message),
            );
        }
    }

    /// Aborts the in-flight transfer with server id `transfer_id`: the worker
    /// cancels it if it is an outgoing upload, or skips it if it is an incoming
    /// download. Shared by the TUI cancel/skip button and the web view.
    pub(crate) fn cancel_transfer(&mut self, transfer_id: FileTransferId) {
        self.send_network_command(NetworkCommand::CancelTransfer { transfer_id }, true);
    }

    /// Tells the browser to configure its decoder for `stream_id` and ensures a
    /// viewer connection is feeding it frames.
    ///
    /// The decoder config is targeted to every tab that asks to play. A tab
    /// that connects after a share started receives the retained
    /// `share_available` button but missed earlier transient config, so its play
    /// click must bootstrap its own decoder. The web server scopes frames to
    /// subscribed sockets while one app-level subscriber connection serves all
    /// tabs viewing the same remote stream.
    fn start_view(&mut self, client: u64, stream_id: StreamId) {
        // The play click came from the browser, so failures are reported back to
        // the web view rather than the TUI, which that user is not watching.
        let Some(feed) = self.web_feed.clone() else {
            return;
        };
        let Some(share) = self.room.available_shares.get(&stream_id) else {
            feed.send_share_error(
                client,
                share_error_envelope(stream_id, "that screen share is no longer available"),
            );
            return;
        };
        if self.room.voice_room != Some(share.room_id) {
            feed.send_share_error(
                client,
                share_error_envelope(stream_id, "join the share's voice room before viewing"),
            );
            return;
        }
        let config = share_config_envelope(stream_id, &share.codec, &share.extradata);
        let view_secret = share.view_secret.clone();
        let generation = share.generation;
        self.web_viewing_shares.insert(stream_id);
        feed.send_share_config(client, stream_id.0, config);

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

        // Every path that leaves without a viewer connection has to undo the
        // subscription above, or `stop_rpc_live_share` later reads it as a
        // browser still watching and declines to stop a real subscriber.
        let Some(session_id) = self.session_id else {
            self.web_viewing_shares.remove(&stream_id);
            feed.send_share_error(
                client,
                share_error_envelope(stream_id, "the voice session is no longer active"),
            );
            return;
        };
        let Some(video_transport) = self.video_transport else {
            self.web_viewing_shares.remove(&stream_id);
            feed.send_share_error(
                client,
                share_error_envelope(stream_id, "video transport is not ready"),
            );
            return;
        };
        let handle = crate::video::start_subscriber(
            session_id,
            stream_id,
            generation,
            view_secret,
            video_transport,
            self.video_fanout.clone(),
            self.events.sender(),
        );
        let handle = match handle {
            Ok(handle) => handle,
            Err(error) => {
                self.web_viewing_shares.remove(&stream_id);
                feed.send_share_error(client, share_error_envelope(stream_id, &error));
                return;
            }
        };
        self.subscribers.insert(stream_id, handle);
        self.set_status("viewing screen share");
    }

    fn stop_view(&mut self, stream_id: StreamId) {
        self.web_viewing_shares.remove(&stream_id);
        if self.video_fanout.has_native(stream_id) {
            return;
        }
        if let Some(mut subscriber) = self.subscribers.remove(&stream_id) {
            subscriber.stop();
        }
    }

    fn rebuild_server_items(&mut self) {
        self.mark_daemon_config_changed();
    }

    pub(crate) fn open_server_select(&mut self) {
        self.navigate_owner(NavigationEvent::ResetBase(BaseScreen::Servers {
            query: None,
        }));
        self.rebuild_server_items();
        if self.config.servers.is_empty() {
            self.set_status("no servers configured; run chatt pair JOIN_STRING");
        } else {
            self.set_status("select a server");
        }
    }

    /// Installs `network` as the active session for `server`, tearing down
    /// whatever session came before.
    ///
    /// This is the single boundary where a worker becomes the session: join
    /// promotion runs it on the candidate's `Authenticated`, and an active
    /// session restart runs it at spawn. The ordering is load-bearing — the
    /// room connects before any authentication payload is applied, and the
    /// worker is installed (with `network_disconnected` cleared) before
    /// anything tries to send on it.
    fn promote_worker(&mut self, server: ServerEntry, network: NetworkClient, generation: u64) {
        self.disconnect_network();
        let storage = crate::room_history::HistoryStorage::resolve(&self.config, &server);
        let continuity =
            self.room
                .connect_to_server(server.label.clone(), storage, server.effective_username());
        if continuity == room::ServerContinuity::NewServer {
            self.broadcast_reset_rooms();
            let catalog_dir = self.room.history_storage().catalog_dir();
            if catalog_dir.is_some() {
                let catalog = crate::room_catalog::load(catalog_dir);
                self.room.load_offline_catalog(&catalog, self.user_id);
            }
        }
        if let Some(feed) = &self.web_feed {
            feed.set_room_name(self.room.room_name.clone());
            self.send_web_history_snapshot(crate::web_server::WebAudience::All);
        }
        self.room.active_server_id = Some(server.id);
        self.network = Some(network);
        self.active_network_generation = Some(generation);
        self.room.network_selected = true;
        self.room.network_disconnected = false;
        self.supervisor.network.reset();
        self.room.join_notice = None;
        if let Err(error) = local_control::write_last_server_hint(server.id) {
            kvlog::warn!("failed to update last-server hint", error = %error);
        }
    }

    /// Persists a complete DM identity snapshot. The network worker activates
    /// it only after the acknowledgement sent by the event handler.
    fn persist_e2e_pin(&mut self, pin: crate::config::E2ePeerPin) -> bool {
        let Some(server_id) = self.room.active_server_id else {
            return false;
        };
        if let Err(error) = server_catalog::commit_e2e_pin(&mut self.config, server_id, pin) {
            kvlog::warn!("failed to persist e2e pin", error = error.as_str());
            self.set_error(format!("failed to persist encryption pin: {error}"));
            return false;
        }
        true
    }

    /// Restarts the network worker of the already-active session in place.
    ///
    /// This is supervision of an established connection, not a join: the
    /// session keeps its server, its queued commands, and its recovery budget.
    fn restart_active_session(&mut self) -> bool {
        let Some(server) = self
            .room
            .active_server_id
            .and_then(|server_id| self.config.server_by_id(server_id))
            .cloned()
        else {
            return false;
        };
        self.next_connection_generation = self.next_connection_generation.wrapping_add(1).max(1);
        let generation = self.next_connection_generation;
        // The replacement worker is spawned before the live one is torn down so
        // a spawn failure leaves the existing connection serving; the old
        // worker's events are already dropped by the generation gate.
        let network = match NetworkClient::spawn(
            server.client_config(&self.config, self.download_store.clone()),
            self.events.sender().for_network(generation),
        ) {
            Ok(network) => network,
            Err(error) => {
                self.set_error(format!("failed to start network: {error}"));
                return false;
            }
        };
        self.promote_worker(server, network, generation);
        self.set_status("connecting");
        true
    }

    fn set_rpc_server_selection_error_for(
        &mut self,
        owner: crate::client_channel::ClientId,
        attempt_id: u64,
        label: &str,
        message: impl Into<String>,
    ) {
        if !self.rpc_clients.contains(&owner) {
            return;
        }
        self.rpc_server_selection_issue = Some(OwnedRpcServerSelectionIssue {
            owner,
            attempt_id,
            issue: RpcServerSelectionIssue::Error(local_rpc::model::ServerSelectionError {
                label: Some(label.to_string()),
                message: message.into(),
            }),
        });
    }

    fn set_rpc_transport_encryption_prompt_for(
        &mut self,
        owner: crate::client_channel::ClientId,
        label: String,
        attempt_id: u64,
    ) {
        if !self.rpc_clients.contains(&owner) {
            return;
        }
        self.rpc_server_selection_issue = Some(OwnedRpcServerSelectionIssue {
            owner,
            attempt_id,
            issue: RpcServerSelectionIssue::Prompt(
                local_rpc::model::ServerSelectionPrompt::AllowUnencryptedTransport {
                    label,
                    attempt_id,
                },
            ),
        });
    }

    fn clear_rpc_server_selection_issue_for(&mut self, attempt_id: u64) {
        if self
            .rpc_server_selection_issue
            .as_ref()
            .is_some_and(|issue| issue.attempt_id == attempt_id)
        {
            self.rpc_server_selection_issue = None;
        }
    }

    fn disconnect_network(&mut self) {
        self.active_network_generation = None;
        self.stop_audio();
        self.stop_all_shares();
        self.room.active_server_id = None;
        self.video_transport = None;
        if let Some(network) = self.network.take() {
            network.stop();
        }
        self.room.network_selected = false;
        self.session_id = None;
        self.user_id = None;
        self.server_dms_enabled = true;
        self.e2e_account_id = None;
        self.reset_room_for_disconnect();
        self.room.server_rtt_ms = None;
        self.last_network_notice = None;
        self.room.join_notice = None;
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.pending_voice_teardown_at = None;
        self.pending_network_commands.clear();
        self.room.network_disconnected = true;
        self.room.media_transport = MediaTransportState::Udp;
        self.pending_dm_open.clear();
        self.pending_dm_clients.clear();
        self.pending_identity_review.clear();
        self.open_e2e_reviews.clear();
        self.rpc_identity.close_all("disconnected from the server");
        self.pending_mutation_clients.clear();
        self.abandon_pending_web_history();
        self.supervisor.network.reset();
        self.supervisor.capture.reset();
        self.supervisor.playback.reset();
        self.supervisor.capture_watch = CaptureWatch::default();
        self.supervisor.playback_watch = PlaybackWatch::default();
    }

    /// Resets live session state (presence, voice) while keeping room buffers
    /// browsable offline. Used by every disconnect path, including reconnect
    /// and worker-failure recovery.
    fn reset_room_for_disconnect(&mut self) {
        self.save_room_catalog();
        self.room.voice_room = None;
        self.requested_voice_room = None;
        self.pending_dm_open.clear();
        self.pending_dm_clients.clear();
        self.pending_identity_review.clear();
        self.open_e2e_reviews.clear();
        self.rpc_identity.close_all("disconnected from the server");
        self.broadcast_cancel_pending_edit();
        self.room.clear_e2e_trust_states();
        self.room.reset_for_disconnect();
    }

    /// Mirrors the viewed room into the web feed and tells the worker which
    /// room externally injected uploads target.
    fn sync_viewed_room_to_feeds(&mut self) {
        self.sync_web_room_feed();
        self.sync_web_e2e_security();
        if let Some(room_id) = self.room.viewed_room {
            self.send_network_command(NetworkCommand::SetActiveRoom(room_id), false);
        }
    }

    fn sync_web_room_feed(&mut self) {
        self.pending_web_history.clear();
        if let Some(feed) = &self.web_feed {
            feed.set_room_name(self.room.room_name.clone());
            self.send_web_history_snapshot(crate::web_server::WebAudience::All);
        }
    }

    fn sync_web_e2e_security(&self) {
        let Some(feed) = &self.web_feed else {
            return;
        };
        let Some(state) = self
            .room
            .viewed_room
            .and_then(|room_id| self.room.e2e_trust_state(room_id))
        else {
            feed.set_e2e_security("clear", "");
            return;
        };
        let (level, message) = match state {
            room::DmTrustState::Accepted {
                change_from: None, ..
            } => ("warning", "Identity Unverified (MITM Vulnerable)"),
            room::DmTrustState::Accepted {
                change_from: Some(crate::config::E2eTrustLevel::Accepted),
                ..
            } => ("danger", "Identity Changed (MITM Vulnerable)"),
            room::DmTrustState::Accepted {
                change_from: Some(crate::config::E2eTrustLevel::Verified),
                ..
            } => ("danger", "Verified Identity Changed (Possible MITM Attack)"),
            room::DmTrustState::Verified { .. } => {
                feed.set_e2e_security("clear", "");
                return;
            }
        };
        feed.set_e2e_security(level, message);
    }

    /// Switches the issuing terminal's viewed room. The primary also moves the
    /// shared web/upload projection; attached terminals keep independent room
    /// selection in the shared session catalog.
    pub(crate) fn set_viewed_room(&mut self, room_id: RoomId) -> bool {
        if self.command_client != crate::client_channel::ClientId::PRIMARY {
            return self.set_attached_viewed_room(room_id);
        }
        if !self.room.set_viewed_room(room_id) {
            return false;
        }
        self.send_to(self.command_client, TerminalEvent::SelectRoom(room_id));
        self.after_view_switch();
        true
    }

    fn set_attached_viewed_room(&mut self, room_id: RoomId) -> bool {
        if !self.room.prepare_client_view(self.command_client, room_id) {
            return false;
        }
        self.send_to(self.command_client, TerminalEvent::SelectRoom(room_id));
        self.room.ensure_e2e_security_notice(room_id);
        if self.room.begin_history_fetch(room_id) {
            let limit = self.room.initial_history_limit(room_id);
            if !self.send_network_command(
                NetworkCommand::FetchHistory {
                    room_id,
                    before: None,
                    limit,
                },
                false,
            ) {
                self.room.abort_history_fetch(room_id, None);
            }
        }
        self.mark_room_catalog_dirty();
        let status = match self.room.room_name_of(room_id) {
            Some(name) => format!("viewing {name}"),
            None => format!("viewing room {}", room_id.0),
        };
        self.set_status(status);
        true
    }

    fn request_older_history(&mut self, room_id: RoomId) {
        let Some((room_id, before, limit)) = self.room.older_history_request(room_id) else {
            return;
        };
        if !self.send_network_command(
            NetworkCommand::FetchHistory {
                room_id,
                before,
                limit,
            },
            false,
        ) {
            self.room.abort_history_fetch(room_id, before);
        }
    }

    pub(crate) fn open_room_switcher(&mut self) {
        self.navigate_owner(NavigationEvent::OpenScreen(Box::new(
            ScreenSpec::RoomSwitcher,
        )));
    }

    #[allow(dead_code)] // Removed after all modes dispatch through ViewCx.
    pub(crate) fn open_user_list(&mut self) {
        self.navigate_owner(NavigationEvent::OpenScreen(Box::new(ScreenSpec::UserList)));
    }

    pub(crate) fn open_room_settings(&mut self) {
        let Some(server_id) = self.room.active_server_id else {
            self.set_error("connect to a server first");
            return;
        };
        let Some(room_id) = self.room.viewed_room else {
            self.set_error("view a room first");
            return;
        };
        let Some(server) = self.config.server_by_id(server_id) else {
            self.set_error("the connected server is no longer configured");
            return;
        };
        let draft = RoomSettingsDraft::from_config(
            &self.config,
            server,
            room_id,
            self.room.room_name.clone(),
        );
        self.navigate_owner(NavigationEvent::OpenScreen(Box::new(
            ScreenSpec::RoomSettings(draft),
        )));
    }

    pub(crate) fn save_room_settings(&mut self, draft: &RoomSettingsDraft) -> bool {
        let overrides = match draft.to_overrides() {
            Ok(overrides) => overrides,
            Err(error) => {
                self.set_error(error);
                return false;
            }
        };
        let committed =
            server_catalog::commit_room_overrides(&mut self.config, draft.server_id(), overrides);
        match committed {
            Ok((history_changed, path)) => {
                self.push_file_policy();
                self.navigate_owner(NavigationEvent::CloseScreen);
                if history_changed && self.network.is_some() {
                    self.set_status("room settings saved; persistence changes apply on reconnect");
                } else {
                    self.set_status(format!("room settings saved to {}", path.display()));
                }
                true
            }
            Err(error) => {
                self.set_error(error);
                false
            }
        }
    }

    /// Switches the view to the neighboring room in catalog order, wrapping.
    #[allow(dead_code)] // Retained while App-level behavior tests migrate.
    pub(crate) fn cycle_room(&mut self, delta: isize) {
        let rooms: Vec<RoomId> = self.room.room_metas().map(|(room_id, _)| room_id).collect();
        if rooms.is_empty() {
            self.set_status("no rooms yet");
            return;
        }
        let current = self
            .room
            .viewed_room
            .and_then(|viewed| rooms.iter().position(|room_id| *room_id == viewed));
        let Some(current) = current else {
            let next = if delta < 0 { rooms.len() - 1 } else { 0 };
            self.set_viewed_room(rooms[next]);
            return;
        };
        let next = (current as isize + delta).rem_euclid(rooms.len() as isize) as usize;
        self.set_viewed_room(rooms[next]);
    }

    fn after_view_switch(&mut self) {
        if let Some(room_id) = self.room.viewed_room {
            self.room.ensure_e2e_security_notice(room_id);
        }
        self.sync_viewed_room_to_feeds();
        self.request_initial_history_for_viewed_room();
        self.request_gap_backfill_for_viewed_room();
        self.mark_room_catalog_dirty();
        self.set_status(format!("viewing {}", self.room.room_name));
    }

    fn request_initial_history_for_viewed_room(&mut self) {
        let Some(room_id) = self.room.viewed_room else {
            return;
        };
        if self.room.begin_history_fetch(room_id) {
            let limit = self.room.initial_history_limit(room_id);
            if !self.send_network_command(
                NetworkCommand::FetchHistory {
                    room_id,
                    before: None,
                    limit,
                },
                false,
            ) {
                self.room.abort_history_fetch(room_id, None);
            }
        }
    }

    fn request_gap_backfill_for_viewed_room(&mut self) {
        let Some(viewed_room) = self.room.viewed_room else {
            return;
        };
        let Some((room_id, before, limit)) = self.room.gap_backfill_request(viewed_room) else {
            return;
        };
        if !self.send_network_command(
            NetworkCommand::FetchHistory {
                room_id,
                before,
                limit,
            },
            false,
        ) {
            self.room.abort_history_fetch(room_id, before);
        }
    }

    fn send_chat(&mut self, room_id: Option<RoomId>, body: String) {
        if self.network.is_none() {
            self.set_error("select a server before sending messages");
            return;
        }
        let Some(room_id) = room_id else {
            self.set_error("no room selected");
            return;
        };
        self.send_network_command(NetworkCommand::SendChat { room_id, body }, true);
    }

    fn submit_edit(&mut self, room_id: RoomId, target: MessageId, body: String) {
        if self.network.is_none() {
            self.set_error("select a server before editing messages");
            return;
        }
        self.pending_mutation_clients
            .entry((room_id, target, false))
            .or_default()
            .push_back(self.command_client);
        self.send_network_command(
            NetworkCommand::EditChat {
                room_id,
                target,
                body,
            },
            true,
        );
    }

    /// Sends a chat message to the currently viewed room.
    fn send_chat_to_viewed(&mut self, body: String) {
        self.send_chat(self.room.viewed_room, body);
    }

    fn mark_room_catalog_dirty(&mut self) {
        if self.room.history_storage().catalog_dir().is_none() {
            return;
        }
        self.pending_room_catalog_save = Some(PendingRoomCatalogSave {
            deadline: Instant::now() + ROOM_CATALOG_SAVE_DEBOUNCE,
        });
    }

    /// Persists the room catalog (names, kinds, read state, last viewed/voice
    /// rooms) so rooms stay navigable offline.
    fn save_room_catalog(&mut self) {
        self.pending_room_catalog_save = None;
        self.write_room_catalog();
    }

    fn write_room_catalog(&self) {
        let catalog_dir = self.room.history_storage().catalog_dir();
        if catalog_dir.is_none() {
            return;
        }
        crate::room_catalog::save(catalog_dir, &self.room.catalog(self.room.voice_room));
    }

    fn start_device_pairing_prompt(&mut self, ticket: Option<DeviceLinkTicket>) {
        let pairing_string = ticket
            .as_ref()
            .and_then(|ticket| rpc::control::encode_device_link_ticket(ticket).ok())
            .unwrap_or_default();
        self.apply_pairing_input(PairingInput::StartDevicePrompt {
            owner: self.command_client,
            pairing_string,
        });
    }

    /// Adds a server from whatever the prompt was handed: a device link, an
    /// invite ticket, or a public `host:port`. These are the three inputs
    /// `chatt pair` takes on argv, and the prompt is the private way to give
    /// this client the two that are secrets.
    fn submit_device_pairing(
        &mut self,
        pairing_string: String,
        device_name: String,
        overwrite_existing: bool,
    ) {
        let result = (|| -> Result<(), String> {
            let pairing_string = pairing_string.trim();
            if pairing_string.starts_with(rpc::control::JOIN_STRING_PREFIX) {
                let ticket = rpc::control::decode_invite_ticket(pairing_string)?;
                self.start_join_pairing(ticket);
                return Ok(());
            }
            if !pairing_string.starts_with(rpc::control::DEVICE_LINK_STRING_PREFIX) {
                self.start_open_pairing(crate::cli::parse_pair_address(pairing_string)?);
                return Ok(());
            }
            let ticket = rpc::control::decode_device_link_ticket(pairing_string)?;
            let alias = unique_server_alias(&self.config, &alias_from_tcp_addr(&ticket.tcp_addr));
            let server = ServerEntry {
                id: server::generate_server_id()?,
                label: alias.clone(),
                tcp_addr: ticket.tcp_addr.clone(),
                udp_addr: ticket.udp_addr.clone(),
                udp_probe_addr: ticket.udp_probe_addr.clone(),
                username: "pairing".to_string(),
                token: String::new(),
                server_public_key: rpc::crypto::encode_hex(&ticket.server_public_key),
                ..ServerEntry::default()
            };
            let client_config = server.client_config(&self.config, self.download_store.clone());
            let cancellation = Arc::new(AtomicU8::new(PAIRING_CANCELABLE));
            let pending = PendingPair {
                server,
                open: None,
                open_password: String::new(),
                pairing_code: None,
                completion: PairCompletion::OpenEditor,
            };
            self.apply_pairing_input(PairingInput::Start {
                owner: self.command_client,
                pending,
                job: PairingJob::Device {
                    config: client_config,
                    ticket: RetainedTicket::new(ticket),
                    device_name,
                    overwrite_existing,
                },
                cancellation: Some(cancellation),
            });
            Ok(())
        })();
        if let Err(error) = result {
            if let Some(channel) = self.channel_for(self.command_client) {
                channel.push(TerminalEvent::PairingFailed(error.clone()));
            }
            self.set_error(error);
        }
    }

    fn start_join_pairing(&mut self, ticket: InviteTicket) {
        let alias = unique_server_alias(&self.config, &default_join_alias(&ticket));
        let username = default_join_username();
        let token = match random_token() {
            Ok(token) => token,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        let server = match server_entry_from_invite(&ticket, alias.clone(), username, token) {
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
        let pairing_code = ticket.pairing_code.clone();
        let config = server.client_config(&self.config, self.download_store.clone());
        let pending = PendingPair {
            server,
            open: None,
            open_password: String::new(),
            pairing_code: Some(pairing_code),
            completion: PairCompletion::OpenEditor,
        };
        self.apply_pairing_input(PairingInput::Start {
            owner: self.command_client,
            pending,
            job: PairingJob::Invite {
                config,
                pairing_code: ticket.pairing_code,
            },
            cancellation: None,
        });
    }

    /// Begins self-service pairing against a bare `host:port` address. The
    /// server's public key is trusted on first use, the token is server-issued,
    /// and the server prompts for a password only when it requires one.
    pub(crate) fn start_open_pairing(&mut self, addr: String) {
        let alias = unique_server_alias(&self.config, &alias_from_tcp_addr(&addr));
        let recovery_token = match random_open_pair_recovery_token() {
            Ok(token) => token,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        let id = match server::generate_server_id() {
            Ok(id) => id,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        let server = ServerEntry {
            id,
            label: alias.clone(),
            tcp_addr: addr,
            udp_addr: String::new(),
            udp_probe_addr: None,
            username: default_join_username(),
            token: recovery_token.clone(),
            server_public_key: String::new(),
            ..ServerEntry::default()
        };
        let client_config = server.client_config(&self.config, self.download_store.clone());
        let pending = PendingPair {
            server,
            open: Some(recovery_token),
            open_password: String::new(),
            pairing_code: None,
            completion: PairCompletion::OpenEditor,
        };
        let existing_token = pending.open.clone().unwrap_or_default();
        self.apply_pairing_input(PairingInput::Start {
            owner: self.command_client,
            pending,
            job: PairingJob::Open {
                config: client_config,
                password: String::new(),
                existing_token,
            },
            cancellation: None,
        });
    }

    /// Joins the ready server holding `label`.
    fn connect_by_label(&mut self, label: &str) {
        match self.config.server(label) {
            Ok(server) => {
                let server_id = server.id;
                self.start_join_with_screen(server_id, self.command_client);
            }
            Err(error) => self.set_error(error),
        }
    }

    /// Resolves and acts on a `chatt join` specifier: connect directly, open the
    /// filtered picker, or fall back to open pairing behind a warn banner.
    fn start_named_join(&mut self, specifier: String) {
        match self.resolve_join(&specifier) {
            JoinResolution::Connect(label) => self.connect_by_label(&label),
            JoinResolution::Filter => {
                self.open_filtered_server_select(&specifier);
                self.set_status(format!("servers matching '{specifier}'"));
            }
            JoinResolution::Pair(addr) => {
                self.room.join_notice = Some(format!(
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
    /// An exact match on a single server's `label`, or on its address compared as
    /// a [`canonical_endpoint`], connects. Several matches open the filtered
    /// picker. Otherwise an address pairs, while a label falls back to a
    /// substring search and then the empty picker.
    ///
    /// Substring matching stays behind the address fallback because an address is
    /// an exact request: `host.example:4000` means that server, not the saved
    /// `myhost.example:4000` it happens to be a substring of.
    fn resolve_join(&self, specifier: &str) -> JoinResolution {
        let canonical = canonical_endpoint(specifier);
        let mut exact: Vec<&str> = Vec::new();
        let addresses = self
            .config
            .servers
            .iter()
            .map(|server| (server.label.as_str(), server.tcp_addr.as_str()));
        for (label, tcp_addr) in addresses {
            let matched = label == specifier
                || (canonical.is_some() && canonical_endpoint(tcp_addr) == canonical);
            if matched {
                exact.push(label);
            }
        }
        if exact.len() == 1 {
            return JoinResolution::Connect(exact[0].to_string());
        }
        if !exact.is_empty() {
            return JoinResolution::Filter;
        }
        if let Ok(addr) = crate::cli::parse_pair_address(specifier) {
            return JoinResolution::Pair(addr);
        }
        let has_substring =
            self.config.servers.iter().any(|server| {
                server.label.contains(specifier) || server.tcp_addr.contains(specifier)
            });
        if has_substring {
            return JoinResolution::Filter;
        }
        JoinResolution::NoMatch
    }

    /// Opens the server picker with `query` pre-applied so the list starts filtered
    /// to the servers a `chatt join` specifier could mean.
    fn open_filtered_server_select(&mut self, query: &str) {
        self.navigate_owner(NavigationEvent::ResetBase(BaseScreen::Servers {
            query: Some(query.to_string()),
        }));
        self.rebuild_server_items();
    }

    /// Re-runs the open-pairing worker with a user-entered password, preserving
    /// the pending server and its existing token.
    pub(crate) fn submit_open_pair_password(&mut self, password: String) {
        let Some(server) = self
            .pairing
            .pending_server_for(self.command_client)
            .cloned()
        else {
            // The prompt outlived the attempt; telling it so clears the
            // submitting state it would otherwise wait on forever.
            self.send_to(
                self.command_client,
                TerminalEvent::PairingFailed("no pairing in progress".to_string()),
            );
            return;
        };
        let config = server.client_config(&self.config, self.download_store.clone());
        self.apply_pairing_input(PairingInput::Password {
            owner: self.command_client,
            password,
            config,
        });
    }

    /// Cancels an in-progress open pairing when the user dismisses the password
    /// prompt.
    pub(crate) fn cancel_open_pairing(&mut self) {
        self.apply_pairing_input(PairingInput::Cancel {
            owner: self.command_client,
        });
    }

    /// Repairs the active session's stale credential: the session comes down,
    /// pairing silently re-pairs with the saved token as recovery material,
    /// and a successful commit rejoins by id.
    fn start_stale_token_repair(&mut self, reason: &str) -> bool {
        let Some(server_id) = self.room.active_server_id else {
            return false;
        };
        let Some(server) = self.config.server_by_id(server_id).cloned() else {
            self.push_network_notice("auth", "the connected server is no longer configured");
            return false;
        };
        let owner = self.command_client;
        if !self.pairing.is_busy() && !server.token.trim().is_empty() {
            self.disconnect_network();
            self.start_token_repair(
                server,
                owner,
                reason,
                CredentialRepairContinuation::ActiveSession,
            )
        } else {
            false
        }
    }

    /// Hands `server`'s stale credential to a silent open-pairing repair
    /// worker of its own, outside the interactive pairing coordinator; a
    /// successful commit starts a fresh join for the same id.
    pub(super) fn start_token_repair(
        &mut self,
        server: ServerEntry,
        owner: crate::client_channel::ClientId,
        reason: &str,
        continuation: CredentialRepairContinuation,
    ) -> bool {
        if self.pairing.is_busy() || self.credential_repair.is_some() {
            return false;
        }
        let existing_token = server.token.clone();
        if existing_token.trim().is_empty() {
            return false;
        }
        let attempt = self.pairing.allocate_attempt();
        let events = self.events.sender().for_pairing(attempt);
        let spawned = crate::client_net::spawn_open_pair_once(
            server.client_config(&self.config, self.download_store.clone()),
            String::new(),
            existing_token,
            events,
        );
        if let Err(error) = spawned {
            self.set_error(error);
            return false;
        }
        self.push_network_notice("auth", reason);
        self.set_status(format!("refreshing {}", server.label));
        self.credential_repair = Some(CredentialRepair {
            attempt,
            server_id: server.id,
            expected_token: server.token,
            expected_server_public_key: server.server_public_key,
            owner,
            continuation,
        });
        true
    }

    /// Applies the outcome of a credential-repair worker: a reissued token is
    /// committed by id and rejoined; anything else ends the repair readably.
    fn handle_repair_event(&mut self, event: PairingEvent) {
        let Some(repair) = self.credential_repair.take() else {
            return;
        };
        let owner = repair.owner;
        if let CredentialRepairContinuation::Join { attempt_id } = repair.continuation
            && !self.join_repair_is_current(attempt_id)
        {
            return;
        }
        match event {
            PairingEvent::OpenSucceeded {
                token,
                server_public_key,
            } => {
                match server_catalog::commit_repaired_credentials(
                    &mut self.config,
                    repair.server_id,
                    &repair.expected_token,
                    &repair.expected_server_public_key,
                    token,
                    server_public_key,
                ) {
                    Ok((server, path)) => {
                        self.rebuild_server_items();
                        self.send_to(
                            owner,
                            TerminalEvent::Status(format!(
                                "refreshed {}; config saved to {}",
                                server.label,
                                path.display()
                            )),
                        );
                        match repair.continuation {
                            CredentialRepairContinuation::ActiveSession => {
                                if let JoinStart::Refused(message) =
                                    self.start_join(server.id, JoinOwner::Terminal(owner))
                                {
                                    self.send_to(owner, TerminalEvent::Error(message));
                                }
                            }
                            CredentialRepairContinuation::Join { attempt_id } => {
                                self.restart_join_after_repair(attempt_id, server);
                            }
                        }
                    }
                    Err(error) => match repair.continuation {
                        CredentialRepairContinuation::ActiveSession => {
                            self.send_to(owner, TerminalEvent::Error(error));
                        }
                        CredentialRepairContinuation::Join { attempt_id } => {
                            self.fail_join_repair(attempt_id, error);
                        }
                    },
                }
            }
            PairingEvent::Failed(message)
            | PairingEvent::UsernameTaken { message, .. }
            | PairingEvent::DeviceFailed { message }
            | PairingEvent::DeviceIdentityExists { message } => {
                self.push_network_notice("auth", &message);
                match repair.continuation {
                    CredentialRepairContinuation::ActiveSession => {
                        self.send_to(owner, TerminalEvent::Error(message));
                    }
                    CredentialRepairContinuation::Join { attempt_id } => {
                        self.fail_join_repair(attempt_id, message);
                    }
                }
            }
            PairingEvent::OpenNeedsPassword { .. } => {
                let message =
                    "saved credentials need repair; re-pair with the server password".to_string();
                match repair.continuation {
                    CredentialRepairContinuation::ActiveSession => {
                        self.send_to(owner, TerminalEvent::Error(message));
                    }
                    CredentialRepairContinuation::Join { attempt_id } => {
                        self.fail_join_repair(attempt_id, message);
                    }
                }
            }
            PairingEvent::TransportEncryptionRequired => {
                let message = "server transport encryption is disabled".to_string();
                match repair.continuation {
                    CredentialRepairContinuation::ActiveSession => {
                        self.send_to(owner, TerminalEvent::Error(message));
                    }
                    CredentialRepairContinuation::Join { attempt_id } => {
                        self.fail_join_repair(attempt_id, message);
                    }
                }
            }
            PairingEvent::InviteSucceeded | PairingEvent::DeviceSucceeded { .. } => {}
        }
    }

    fn apply_pairing_input(&mut self, input: PairingInput) {
        let pairing = std::mem::take(&mut self.pairing);
        self.pairing = pairing.handle(self, input);
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
        if refresh.id + 1 != self.room.audio_devices.next_refresh_id {
            return;
        }
        self.room.audio_devices.refresh_in_flight = false;

        let mut input_count = None;
        let mut output_count = None;
        let mut errors = Vec::new();

        match refresh.input {
            Ok(devices) => {
                input_count = Some(devices.len());
                self.room.audio_devices.input_devices = devices;
            }
            Err(error) => {
                self.mic_error = Some(error.clone());
                errors.push(format!("input devices: {error}"));
            }
        }

        match refresh.output {
            Ok(devices) => {
                output_count = Some(devices.len());
                self.room.audio_devices.output_devices = devices;
            }
            Err(error) => {
                errors.push(format!("output devices: {error}"));
            }
        }

        self.room.audio_devices.generation = self.room.audio_devices.generation.saturating_add(1);
        kvlog::info!(
            "audio device refresh completed",
            id = refresh.id,
            input_buffer_request = refresh.input_buffer_request.label(),
            output_buffer_request = refresh.output_buffer_request.label(),
            input_count = input_count.unwrap_or(self.room.audio_devices.input_devices.len()),
            output_count = output_count.unwrap_or(self.room.audio_devices.output_devices.len()),
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
            && !self.local_voice_state().is_deafened()
        {
            self.start_settings_preview_capture();
        }
    }

    #[cfg(test)]
    fn handle_network_event(&mut self, event: NetworkEvent) {
        let _ = self.handle_network_event_change(event);
    }

    fn handle_network_event_change(&mut self, event: NetworkEvent) -> Option<HistoryChange> {
        let mut history_change = None;
        self.handle_network_event_inner(event, &mut history_change);
        history_change
    }

    fn handle_network_event_inner(
        &mut self,
        event: NetworkEvent,
        history_change: &mut Option<HistoryChange>,
    ) {
        match event {
            NetworkEvent::Connected => {
                self.last_network_notice = None;
                self.set_status("connected; authenticating");
            }
            NetworkEvent::Authenticated {
                session_id,
                user_id,
                rooms,
                users,
                default_room,
                dms_enabled,
                video_addr,
                video_transport_mode,
                video_auth_key,
            } => {
                self.rpc_server_selection_issue = None;
                self.session_id = Some(session_id);
                self.user_id = Some(user_id);
                self.server_dms_enabled = dms_enabled;
                self.video_transport = Some(crate::video::VideoTransport::new(
                    video_addr,
                    video_transport_mode,
                    video_auth_key,
                ));
                self.room.network_disconnected = false;
                self.room.clear_e2e_trust_states();
                self.last_network_notice = None;
                let catalog = crate::room_catalog::load(self.room.history_storage().catalog_dir());
                let known = self.room.authenticated(
                    &rooms,
                    users,
                    default_room,
                    catalog.last_viewed_room,
                    Some(user_id),
                );
                self.reconcile_rpc_client_views();
                self.sync_viewed_room_to_feeds();
                for room_id in known {
                    if self.room.begin_history_fetch(room_id) {
                        let limit = self.room.initial_history_limit(room_id);
                        if !self.send_network_command(
                            NetworkCommand::FetchHistory {
                                room_id,
                                before: None,
                                limit,
                            },
                            false,
                        ) {
                            self.room.abort_history_fetch(room_id, None);
                        }
                    }
                }
                if !self.voice_left {
                    let voice_target = catalog
                        .last_voice_room
                        .filter(|room_id| self.room.room_meta(*room_id).is_some())
                        .unwrap_or(default_room);
                    self.requested_voice_room = Some(voice_target);
                    self.send_network_command(NetworkCommand::JoinVoice(voice_target), true);
                    self.publish_voice_state();
                }
                self.mark_room_catalog_dirty();
                self.set_status(format!("authenticated as {}", self.room.local_username));
                self.flush_pending_network_commands();
            }
            NetworkEvent::RoomUpserted(info) => {
                let room_id = info.room_id;
                self.room.upsert_room(&info, self.user_id);
                if self.room.viewed_room == Some(room_id) {
                    self.request_initial_history_for_viewed_room();
                }
                let pending: Vec<_> = self
                    .pending_dm_open
                    .keys()
                    .filter(|(pending_room, _)| *pending_room == room_id)
                    .copied()
                    .collect();
                for (pending_room, peer) in pending {
                    if let Some(clients) = self.pending_dm_open.remove(&(pending_room, peer)) {
                        for client_id in clients {
                            self.open_dm_room_for_client(client_id, room_id, peer);
                        }
                    }
                }
                self.mark_room_catalog_dirty();
            }
            NetworkEvent::DmOpened { room_id, peer } => {
                let Some(clients) = self.pending_dm_clients.remove(&peer) else {
                    kvlog::warn!("dm opened without a pending owner", peer = peer.0);
                    return;
                };
                if self.room.room_meta(room_id).is_some() {
                    for client_id in clients {
                        self.open_dm_room_for_client(client_id, room_id, peer);
                    }
                } else {
                    self.pending_dm_open.insert((room_id, peer), clients);
                }
            }
            NetworkEvent::HistoryChunk {
                room_id,
                before,
                messages,
                at_start,
                complete,
            } => {
                let update = self.room.history_chunk_received(
                    room_id,
                    before,
                    messages,
                    at_start,
                    complete,
                    self.user_id,
                );
                if update.read_advanced {
                    self.mark_room_catalog_dirty();
                }
                if let Some(change) = update.change {
                    self.project_history_change_to_web(&change);
                    *history_change = Some(change);
                }
                if complete {
                    self.complete_pending_web_history(room_id);
                }
                if let Some((room_id, before, limit)) = update.next_backfill
                    && !self.send_network_command(
                        NetworkCommand::FetchHistory {
                            room_id,
                            before,
                            limit,
                        },
                        false,
                    )
                {
                    self.room.abort_history_fetch(room_id, before);
                }
            }
            NetworkEvent::ChatMutationRejected {
                room_id,
                target,
                kind,
                message,
            } => {
                kvlog::warn!(
                    "chat mutation rejected",
                    room_id = room_id.0,
                    target = target.0,
                    error = message.as_str()
                );
                let owner =
                    self.pop_mutation_owner(room_id, target, kind == ChatMutationKind::Delete);
                if let Some(owner) = owner {
                    if self.channel_for(owner).is_some() {
                        self.send_to(owner, TerminalEvent::Error(message.clone()));
                    } else {
                        kvlog::warn!(
                            "mutation rejection owner is no longer connected",
                            client_id = owner.0
                        );
                    }
                }
                if let Some(feed) = &self.web_feed {
                    let operation = match kind {
                        ChatMutationKind::Edit => "edit_message",
                        ChatMutationKind::Delete => "delete_message",
                    };
                    feed.send_action_error(web_action_error_envelope(operation, &message));
                }
                if kind == ChatMutationKind::Delete
                    && self.pending_web_deletes.remove(&(room_id, target))
                {
                    if owner.is_none() {
                        self.set_error(message.clone());
                    }
                    self.report_web_delete_error(target, &message);
                }
            }
            NetworkEvent::Chat(record) => {
                let room_id = record.message.room_id;
                let raw_message_id = record.message.message_id.0;
                let mls_sequence =
                    (raw_message_id & (1 << 63) != 0).then_some(raw_message_id & !(1 << 63));
                (|| {
                    let message = &record.message;
                    if let Some(target) = message.target {
                        let update = self
                            .room
                            .authenticated_mutation_received(&record, self.user_id);
                        let Some(update) = update else {
                            return;
                        };
                        if let Some(regression) = update.message_id_regression {
                            self.report_message_id_regression(regression);
                            return;
                        }
                        if update.read_advanced {
                            self.mark_room_catalog_dirty();
                        }
                        if update
                            .change
                            .as_ref()
                            .is_some_and(|change| change.removed.contains(&target))
                        {
                            self.pending_web_deletes.remove(&(message.room_id, target));
                        }
                        let delete = message.flags.deleted();
                        self.pop_mutation_owner(message.room_id, target, delete);
                        if let Some(change) = update.change {
                            self.project_history_change_to_web(&change);
                            *history_change = Some(change);
                        }
                        return;
                    }
                    let update = RoomSession::chat_received(&mut self.room, record, self.user_id);
                    let Some(update) = update else {
                        return;
                    };
                    if let Some(regression) = update.message_id_regression {
                        self.report_message_id_regression(regression);
                        return;
                    }
                    let Some(change) = update.change else {
                        return;
                    };
                    if update.read_advanced {
                        self.mark_room_catalog_dirty();
                    }
                    self.project_history_change_to_web(&change);
                    *history_change = Some(change);
                    if !update.local {
                        self.play_notification(NotificationSound::MessageReceived);
                    }
                })();
                if let Some(sequence) = mls_sequence {
                    self.send_network_command(
                        NetworkCommand::AcknowledgeMlsUiDispatch { room_id, sequence },
                        false,
                    );
                }
            }
            NetworkEvent::FileReceived {
                metadata,
                served_name,
                dimensions,
            } => {
                let attachment_id = local_rpc::model::AttachmentId {
                    timestamp_ms: metadata.timestamp_ms,
                    transfer_id: metadata.transfer_id,
                };
                if !self
                    .download_store
                    .bind_attachment(attachment_id, &served_name)
                {
                    kvlog::warn!(
                        "received file could not bind durable attachment identity",
                        room_id = metadata.room_id.0,
                        attachment_timestamp_ms = attachment_id.timestamp_ms,
                        attachment_transfer_id = attachment_id.transfer_id.0,
                        served_name = served_name.as_str()
                    );
                }
                self.room
                    .clear_transfer(metadata.room_id, metadata.transfer_id);
                if let Some(change) = self.room.file_received(
                    metadata.room_id,
                    metadata.transfer_id,
                    metadata.timestamp_ms,
                    &served_name,
                    metadata.size,
                    dimensions,
                ) {
                    self.project_history_change_to_web(&change);
                    *history_change = Some(change);
                }
            }
            NetworkEvent::TransferProgress {
                room_id,
                transfer_id,
                timestamp_ms,
                transferred,
                total,
                direction,
            } => {
                self.room
                    .transfer_progress(room_id, transfer_id, transferred, total, direction);
                if self.room.viewed_room == Some(room_id)
                    && let Some(feed) = &self.web_feed
                {
                    feed.send_file_progress(file_progress_envelope(
                        transfer_id.0,
                        timestamp_ms,
                        transferred,
                        total,
                        direction,
                    ));
                }
            }
            NetworkEvent::TransferEnded {
                room_id,
                transfer_id,
                timestamp_ms,
                verb,
                reason,
            } => {
                if self.room.viewed_room == Some(room_id)
                    && let Some(feed) = &self.web_feed
                {
                    feed.send_file_terminal(file_terminal_envelope(
                        transfer_id.0,
                        timestamp_ms,
                        verb,
                        reason.as_deref(),
                    ));
                }
                self.room.end_transfer(room_id, transfer_id, verb, reason);
            }
            NetworkEvent::TransferComplete {
                room_id,
                transfer_id,
            } => {
                self.room.clear_transfer(room_id, transfer_id);
            }
            NetworkEvent::Presence { user, online } => {
                let notice = self.room.presence_changed(user, online, self.user_id);
                if !notice.local && notice.relevant {
                    self.play_notification(if online {
                        NotificationSound::PeerJoin
                    } else {
                        NotificationSound::PeerLeave
                    });
                    self.set_status(format!(
                        "{} {}",
                        notice.username,
                        if online { "joined" } else { "left" }
                    ));
                }
            }
            NetworkEvent::MlsAccountIdentity { account_id } => {
                self.e2e_account_id = Some(account_id);
            }
            NetworkEvent::MlsDeviceBound { device_id } => {
                let _ = device_id;
            }
            NetworkEvent::E2ePeerPinProposed {
                pin,
                manual_verification,
            } => {
                let persisted = self.persist_e2e_pin(pin.clone());
                self.send_network_command(
                    NetworkCommand::ConfirmE2ePeerPin {
                        pin,
                        persisted,
                        manual_verification,
                    },
                    true,
                );
                if !persisted {
                    self.set_error(
                        "could not save the encryption identity; it remains active for this session",
                    );
                }
            }
            NetworkEvent::E2ePeerPinMatched { identity } => {
                let previous_level =
                    self.room
                        .e2e_trust_state(identity.room_id)
                        .and_then(|state| match state {
                            room::DmTrustState::Accepted {
                                peer,
                                identity: current,
                                ..
                            } if *peer == identity.user_id
                                && current.room_id == identity.identity.room_id
                                && current.user_id == identity.identity.user_id
                                && current.public_key == identity.identity.public_key =>
                            {
                                Some(crate::config::E2eTrustLevel::Accepted)
                            }
                            room::DmTrustState::Verified {
                                peer,
                                identity: current,
                            } if *peer == identity.user_id
                                && current.room_id == identity.identity.room_id
                                && current.user_id == identity.identity.user_id
                                && current.public_key == identity.identity.public_key =>
                            {
                                Some(crate::config::E2eTrustLevel::Verified)
                            }
                            _ => None,
                        });
                let state = match identity.trust_level {
                    crate::config::E2eTrustLevel::Accepted => room::DmTrustState::Accepted {
                        peer: identity.user_id,
                        identity: identity.identity.clone(),
                        change_from: identity.change_from,
                    },
                    crate::config::E2eTrustLevel::Verified => room::DmTrustState::Verified {
                        peer: identity.user_id,
                        identity: identity.identity.clone(),
                    },
                };
                self.room.set_e2e_verified_keys(
                    identity.room_id,
                    identity.verified_keys.iter().copied(),
                );
                self.room.set_e2e_trust_state(identity.room_id, state);
                if self.room.viewed_room == Some(identity.room_id) {
                    self.sync_web_room_feed();
                }
                self.sync_web_e2e_security();
                if let Some(previous_level) = previous_level
                    && previous_level != identity.trust_level
                {
                    let status = self.e2e_trust_change_status(&identity);
                    self.set_status(status);
                }
                let stale_clients: Vec<_> = self
                    .open_e2e_reviews
                    .iter()
                    .filter_map(|(client_id, (room_id, public_key, trust_level))| {
                        (*room_id == identity.room_id
                            && (public_key != &identity.identity.public_key
                                || *trust_level != identity.trust_level))
                            .then_some(*client_id)
                    })
                    .collect();
                for client_id in stale_clients {
                    self.open_e2e_reviews.remove(&client_id);
                    if self.rpc_clients.contains(&client_id) {
                        // Terminals read the outcome from the status line the
                        // overlay leaves behind; renderers only see this reason.
                        let reason = self.identity_review_outcome(client_id, &identity);
                        self.rpc_identity.close(client_id, &reason);
                        continue;
                    }
                    self.send_to(
                        client_id,
                        TerminalEvent::Navigation(NavigationEvent::CloseOverlay),
                    );
                }
                if let Some(clients) = self.pending_identity_review.remove(&identity.user_id) {
                    let target = crate::client_channel::E2eIdentityTarget {
                        room_id: identity.room_id,
                        user_id: identity.user_id,
                        username: self.identity_display_username(&identity),
                        public_key: identity.identity.public_key.clone(),
                        accepted: identity.clone(),
                    };
                    for client_id in clients {
                        let previous = std::mem::replace(&mut self.command_client, client_id);
                        self.open_e2e_identity(target.clone(), None);
                        self.command_client = previous;
                    }
                }
            }
            NetworkEvent::VoiceStarted {
                room_id,
                session_id,
                user_id,
                stream_id,
                user_joined,
            } => {
                if Some(session_id) == self.session_id {
                    self.room.voice_room = Some(room_id);
                    self.room.rebuild_roster();
                    self.requested_voice_room = None;
                }
                let voice_room = self.room.voice_room;
                let notice = self.room.voice_started_transition(
                    room_id,
                    session_id,
                    user_id,
                    stream_id,
                    self.session_id,
                    voice_room,
                    user_joined,
                );
                if user_joined
                    && self.room.voice_room == Some(room_id)
                    && let Some(change) = self.room.push_system_message_to(
                        room_id,
                        "call",
                        format!("{} joined the call", notice.username),
                        unix_now_ms(),
                        crate::chat_buffer::NoticeKind::Info,
                    )
                {
                    self.project_history_change_to_web(&change);
                    *history_change = Some(change);
                }
                if self.room.voice_room == Some(room_id) {
                    if let Some(playback) = &self.playback {
                        playback.start_stream(stream_id.0);
                    }
                    self.apply_user_audio_control(user_id);
                    self.apply_remote_sender_mute(user_id, self.room.voice_muted(user_id));
                }
                if notice.local {
                    self.start_room_voice();
                    if self.config.soundboard.enabled {
                        self.set_status("soundboard ready");
                    } else {
                        self.set_status("voice stream ready");
                    }
                    self.mark_room_catalog_dirty();
                } else if self.room.voice_room == Some(room_id) {
                    self.set_status(format!("{} voice ready", notice.username));
                }
            }
            NetworkEvent::VoiceStopped {
                room_id,
                session_id,
                user_id,
                stream_id,
                user_left,
            } => {
                let was_participating = self.room.voice_room == Some(room_id);
                let notice = self.room.voice_stopped_transition(
                    room_id,
                    session_id,
                    user_id,
                    stream_id,
                    self.session_id,
                    user_left,
                );
                if user_left
                    && was_participating
                    && let Some(change) = self.room.push_system_message_to(
                        room_id,
                        "call",
                        format!("{} left the call", notice.username),
                        unix_now_ms(),
                        crate::chat_buffer::NoticeKind::Info,
                    )
                {
                    self.project_history_change_to_web(&change);
                    *history_change = Some(change);
                }
                if notice.local {
                    if self.room.voice_room == Some(room_id) {
                        self.clear_shares_for_voice_room(room_id);
                        self.room.voice_room = None;
                        self.room.rebuild_roster();
                        self.stop_audio();
                        self.set_status("voice stopped");
                    }
                } else {
                    if let Some(playback) = &self.playback {
                        playback.stop_stream(stream_id.0);
                    }
                    if self.room.voice_room == Some(room_id) {
                        self.set_status(format!("{} left voice", notice.username));
                    }
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
            NetworkEvent::PlaybackFeedback(feedback) => {
                self.room.playback_feedback(feedback);
            }
            NetworkEvent::OutboundFeedback { reporter, feedback } => {
                self.room.outbound_feedback(reporter, feedback);
            }
            NetworkEvent::ServerRtt { rtt_ms } => {
                self.room.server_rtt_ms = rtt_ms;
            }
            NetworkEvent::PeerRtt { user_id, rtt_ms } => {
                self.room.peer_rtt(user_id, rtt_ms);
            }
            NetworkEvent::VoiceStateChanged { user_id, state } => {
                self.room.voice_state_changed(user_id, state);
                self.apply_remote_sender_mute(user_id, state.is_muted());
            }
            NetworkEvent::VoiceJoinFailed { room_id, message } => {
                if self.requested_voice_room == Some(room_id) {
                    self.requested_voice_room = None;
                }
                self.set_error(format!("voice join failed: {message}"));
            }
            NetworkEvent::EncoderProfileChanged(profile) => {
                self.encoder_profile = profile;
                if let Some(capture) = &self.capture {
                    capture.set_encoder_profile(profile);
                }
            }
            NetworkEvent::ShareStarted {
                attempt_id,
                room_id,
                stream_id,
                publish_secret,
                codec,
                coded_width,
                coded_height,
                extradata,
            } => {
                // The capture is already gone: the share was torn down inside
                // the window between `StartShare` and this reply, so nothing
                // will ever publish to the stream the server just created.
                // Stop it rather than announcing a share that can only leave
                // viewers waiting for a keyframe.
                if self
                    .screencast
                    .as_ref()
                    .is_none_or(|handle| handle.attempt_id() != attempt_id)
                {
                    kvlog::warn!(
                        "stale share started without its active capture",
                        attempt_id = attempt_id.0,
                        stream_id = stream_id.0
                    );
                    if let Some(network) = &self.network {
                        let _ = network
                            .sender()
                            .send(NetworkCommand::StopShare { stream_id });
                    }
                    return;
                }
                self.screencast_stream_id = Some(stream_id);
                self.replace_live_share_stream(stream_id);
                let generation = self.allocate_live_share_generation();
                self.video_fanout.start_stream(stream_id);
                self.room.screencast_status.live(
                    stream_id,
                    codec.clone(),
                    coded_width,
                    coded_height,
                );
                // Register the user's own share so their browser can watch it.
                // The publisher tees frames straight to the web feed, so the
                // local share needs no view secret or subscriber connection.
                let sender = self
                    .user_id
                    .map(|user_id| self.room.participants.username_for(user_id).to_string())
                    .unwrap_or_else(|| "you".to_string());
                self.room.available_shares.insert(
                    stream_id,
                    AvailableShare {
                        room_id,
                        generation,
                        view_secret: Vec::new(),
                        sender_name: sender.clone(),
                        codec: codec.clone(),
                        coded_width,
                        coded_height,
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
                // The announcement must be queued before the publisher is
                // released: delivering the secret can synchronously flush the
                // buffered keyframe into the web feed.
                if let (Some(handle), Some(session_id)) = (&self.screencast, self.session_id) {
                    handle.deliver_secret(session_id, stream_id, publish_secret);
                } else {
                    kvlog::warn!("share started without a session", stream_id = stream_id.0);
                }
                self.set_status("screen share live");
            }
            NetworkEvent::ShareAvailable {
                room_id,
                stream_id,
                sender_name,
                codec,
                coded_width,
                coded_height,
                extradata,
                view_secret,
            } => {
                if self.room.voice_room != Some(room_id) {
                    return;
                }
                let existing_generation = self
                    .room
                    .available_shares
                    .get(&stream_id)
                    .filter(|share| share.view_secret == view_secret)
                    .map(|share| share.generation);
                let generation = if let Some(generation) = existing_generation {
                    generation
                } else {
                    self.replace_live_share_stream(stream_id);
                    self.allocate_live_share_generation()
                };
                self.video_fanout.start_stream(stream_id);
                self.room.available_shares.insert(
                    stream_id,
                    AvailableShare {
                        room_id,
                        generation,
                        view_secret,
                        sender_name: sender_name.clone(),
                        codec: codec.clone(),
                        coded_width,
                        coded_height,
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
            NetworkEvent::ShareEnded { stream_id } => {
                if self.screencast_stream_id == Some(stream_id) {
                    self.room
                        .screencast_status
                        .fail("screen share ended by server".to_string());
                    self.teardown_own_share(false);
                } else {
                    self.drop_share(stream_id);
                }
            }
            NetworkEvent::ShareStartRejected {
                attempt_id,
                message,
            } => {
                self.handle_screencast_failed(attempt_id, message);
            }
            NetworkEvent::MediaTransport { state } => self.room.media_transport = state,
            NetworkEvent::Status(status) => self.set_status(status),
            NetworkEvent::Error(error) => {
                kvlog::warn!("app network error", error = error.as_str());
                self.set_error(format!("error: {error}"));
            }
            NetworkEvent::AuthFailed { code, message } => {
                // The active session's reconnect re-authenticated and was
                // refused: a stale credential is silently repaired, anything
                // else takes the session down with the error readable.
                kvlog::warn!("app auth failed", code, error = message.as_str());
                if code == ERROR_TOKEN_STALE_EPOCH && self.start_stale_token_repair(&message) {
                    return;
                }
                self.fail_screencast_if_running(
                    format!("screen share stopped: authentication failed: {message}"),
                    false,
                );
                self.disconnect_network();
                self.navigate_all(BaseScreen::Servers { query: None });
                self.push_network_notice("auth", &message);
                self.set_error(message);
            }
            NetworkEvent::TransportEncryptionRequired => {
                // Only a join negotiates transport policy; an established
                // session refusing it mid-flight is a plain failure.
                self.disconnect_network();
                self.navigate_all(BaseScreen::Servers { query: None });
                self.set_error("server transport encryption is disabled");
            }
            NetworkEvent::DeviceLinkCreated {
                redemption_secret_hash,
                pairing_string,
                expires_at_ms,
            } => {
                self.navigate_owner(NavigationEvent::ShowOverlay(Box::new(
                    OverlaySpec::DeviceLink(device_pair::DeviceLinkDialog::new(
                        redemption_secret_hash,
                        pairing_string,
                        expires_at_ms,
                        self.config.ui.default_bindings,
                    )),
                )));
                self.set_status("one-time device link created");
            }
            NetworkEvent::DeviceLinkRedeemed {
                device_id,
                device_name,
            } => {
                let _ = device_id;
                self.navigate_owner(NavigationEvent::CloseOverlay);
                self.set_status(format!("device linked: {device_name}"));
            }
            NetworkEvent::DeviceLinkCanceled => {
                self.navigate_owner(NavigationEvent::CloseOverlay);
                self.set_status("device link canceled");
            }
            NetworkEvent::ReconnectScheduled { retry_in, reason } => {
                self.room.network_disconnected = true;
                self.room.media_transport = MediaTransportState::Udp;
                self.video_transport = None;
                self.stop_audio();
                self.fail_screencast_if_running(
                    format!("screen share stopped: connection failed: {reason}"),
                    false,
                );
                // The reconnect issues a fresh session id, so every share and
                // viewer tied to the old one is dead; subscribers would retry
                // against the stale id forever.
                self.stop_all_shares();
                self.reset_room_for_disconnect();
                self.push_network_notice("network", &format!("Connection failed: {reason}"));
                self.set_error(format!(
                    "connection failed; retrying in {}s",
                    retry_in.as_secs()
                ));
            }
            NetworkEvent::LocalIdentityUnavailable { message } => {
                self.push_network_notice("e2e", &message);
                self.set_error(message);
            }
            NetworkEvent::Mls(_control) => {
                kvlog::debug!("MLS transport response", control = ?_control);
            }
            NetworkEvent::WorkerStopped { reason } => {
                self.video_transport = None;
                self.stop_audio();
                self.fail_screencast_if_running(
                    format!("screen share stopped: network worker stopped: {reason}"),
                    false,
                );
                self.stop_all_shares();
                self.reset_room_for_disconnect();
                self.push_network_notice(
                    "network",
                    &format!("Network worker stopped: {reason}; reconnecting"),
                );
                self.schedule_network_recovery(Instant::now(), reason);
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
        if self.room.network_disconnected {
            let kind = app_network_command_kind(&command);
            kvlog::info!("network command queued while disconnected", kind);
            if queue_on_failure {
                self.pending_network_commands.push_back(command);
            }
            return false;
        }
        let Some(network) = &self.network else {
            if queue_on_failure {
                self.pending_network_commands.push_back(command);
            }
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

    /// Queues one delete command per selected target. Returns whether a server
    /// session exists, including a temporarily disconnected session whose
    /// commands are retained for reconnect.
    pub(crate) fn delete_chat_messages(
        &mut self,
        room_id: RoomId,
        targets: Vec<MessageId>,
    ) -> bool {
        if self.network.is_none() {
            kvlog::warn!(
                "chat delete not queued",
                room_id = room_id.0,
                target_count = targets.len(),
                error = "no server selected"
            );
            self.set_error("select a server before deleting messages");
            return false;
        }
        let count = targets.len();
        kvlog::info!(
            "chat delete queueing",
            room_id = room_id.0,
            target_count = count
        );
        let mut sent_immediately = true;
        for target in targets {
            self.pending_mutation_clients
                .entry((room_id, target, true))
                .or_default()
                .push_back(self.command_client);
            sent_immediately &=
                self.send_network_command(NetworkCommand::DeleteChat { room_id, target }, true);
        }
        if self.network.is_none() {
            return false;
        }
        if !sent_immediately && count == 1 {
            self.set_status("delete queued for reconnect");
        } else if !sent_immediately {
            self.set_status(format!("{count} deletions queued for reconnect"));
        } else if count == 1 {
            self.set_status("deleting message");
        } else {
            self.set_status(format!("deleting {count} messages"));
        }
        true
    }

    fn flush_pending_network_commands(&mut self) {
        if self.pending_network_commands.is_empty()
            || self.network.is_none()
            || self.room.network_disconnected
        {
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
        self.push_error_notice(sender, body);
    }

    fn report_message_id_regression(&mut self, regression: room::MessageIdRegression) {
        let room_name = self
            .room
            .room_name_of(regression.room_id)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("room {}", regression.room_id.0));
        let body = format!(
            "dropped out-of-order chat record {} in {room_name}; latest known message ID is {}",
            regression.received.0, regression.high_watermark.0
        );
        kvlog::error!(
            "out-of-order live chat record rejected",
            room_id = regression.room_id.0,
            received_message_id = regression.received.0,
            high_watermark = regression.high_watermark.0
        );
        self.set_error(body.clone());
        if !self
            .room
            .push_error_notice_to(regression.room_id, "network", body.clone())
        {
            self.push_error_notice("network", body);
        }
    }

    /// Journals a system line into the viewed room; before any room is
    /// viewed it lands in the primary view's pre-connect buffer instead.
    pub(crate) fn push_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        let sender = sender.into();
        let body = body.into();
        self.capture_frontend_command_line(false, &body);
        if !self.room.push_notice(sender.clone(), body.clone()) {
            self.send_to(
                self.command_client,
                TerminalEvent::LocalNotice {
                    sender,
                    body,
                    error: false,
                },
            );
        }
    }

    pub(crate) fn push_error_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        let sender = sender.into();
        let body = body.into();
        self.capture_frontend_command_line(true, &body);
        if !self.room.push_error_notice(sender.clone(), body.clone()) {
            self.send_to(
                self.command_client,
                TerminalEvent::LocalNotice {
                    sender,
                    body,
                    error: true,
                },
            );
        }
    }

    pub(crate) fn base_screen(&self) -> BaseScreen {
        if self.network.is_some() || !self.room.server_alias.is_empty() {
            BaseScreen::Room
        } else {
            BaseScreen::Servers { query: None }
        }
    }

    fn navigate_owner(&mut self, event: NavigationEvent) {
        self.send_to(self.command_client, TerminalEvent::Navigation(event));
    }

    #[cfg(test)]
    pub(crate) fn base_mode(&self) -> Box<dyn crate::tui::mode::AppMode> {
        match self.base_screen() {
            BaseScreen::Room => Box::new(crate::tui::modes::RoomMode::default()),
            BaseScreen::Servers { query: Some(query) } => {
                Box::new(crate::tui::modes::ServerListMode::with_query(query))
            }
            BaseScreen::Servers { query: None } => {
                Box::new(crate::tui::modes::ServerListMode::new())
            }
        }
    }

    pub(crate) fn delete_server(&mut self, server_id: ServerId) {
        let Some(label) = self
            .config
            .server_by_id(server_id)
            .map(|server| server.label.clone())
        else {
            self.set_error("server is no longer configured");
            return;
        };
        // The session is dropped only once the deletion is durable: a refused
        // write leaves the entry configured, so the connection it names stays.
        let path = match server_catalog::delete(&mut self.config, server_id) {
            Ok(path) => path,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        if self.room.active_server_id == Some(server_id) {
            self.disconnect_network();
            self.room.reset_for_server_list();
            self.broadcast_reset_rooms();
        }
        // A join pending on the deleted entry waits on a session that can
        // never be promoted, so it is called off with the record.
        self.cancel_join_for_server(server_id);
        self.rebuild_server_items();
        self.set_status(format!(
            "deleted {label}; config saved to {}",
            path.display()
        ));
    }

    /// Commits one editor submission and answers the submitting editor with a
    /// typed result. Every navigation belongs to that mode; the core's only
    /// side effects here are the durable commit and, on request, a join.
    fn submit_server_edit(&mut self, request_id: u64, draft: &ServerEditDraft, join: bool) {
        let owner = self.command_client;
        if self.retry_pairing_edit(owner, request_id, draft, join) {
            return;
        }
        let outcome = self.server_edit_outcome(owner, request_id, draft, join);
        self.send_to(
            owner,
            TerminalEvent::ServerEditResult {
                request_id,
                outcome,
            },
        );
    }

    /// Submits the full editor back into a username-rejected pairing. The
    /// candidate remains transient; only a successful retry completes this
    /// editor request and reaches the catalog commit path.
    fn retry_pairing_edit(
        &mut self,
        owner: crate::client_channel::ClientId,
        request_id: u64,
        draft: &ServerEditDraft,
        join: bool,
    ) -> bool {
        if !self.pairing.awaiting_editor_for(owner, draft.server_id()) {
            return false;
        }
        let candidate = (|| -> Result<ServerEntry, String> {
            let mut candidate = draft
                .new_server()
                .cloned()
                .ok_or_else(|| "pairing editor lost its transient server".to_string())?;
            draft.fields()?.apply_to(&mut candidate);
            validate_server_entry(&candidate)?;
            Ok(candidate)
        })();
        let candidate = match candidate {
            Ok(candidate) => candidate,
            Err(error) => {
                self.set_error(error);
                self.send_to(
                    owner,
                    TerminalEvent::ServerEditResult {
                        request_id,
                        outcome: ServerEditOutcome::Rejected,
                    },
                );
                return true;
            }
        };
        self.apply_pairing_input(PairingInput::RetryServer {
            owner,
            server: candidate,
            request_id,
            join,
        });
        true
    }

    /// Completes a save request which had to retry pairing for the username
    /// selected in the editor.
    fn complete_pairing_edit(
        &mut self,
        owner: crate::client_channel::ClientId,
        request_id: u64,
        server: ServerEntry,
        join: bool,
    ) {
        let draft = ServerEditDraft::from_new_server(server, &self.config);
        let outcome = self.server_edit_outcome(owner, request_id, &draft, join);
        self.send_to(
            owner,
            TerminalEvent::ServerEditResult {
                request_id,
                outcome,
            },
        );
    }

    fn server_edit_outcome(
        &mut self,
        owner: crate::client_channel::ClientId,
        request_id: u64,
        draft: &ServerEditDraft,
        join: bool,
    ) -> ServerEditOutcome {
        let original_label = draft.original_label().to_string();
        let commit = match server_catalog::commit_edit(&mut self.config, draft) {
            Ok(commit) => commit,
            Err(error) => {
                self.set_error(error);
                return ServerEditOutcome::Rejected;
            }
        };
        let (server, path, connection_changed) = match commit {
            server_catalog::EditCommit::Saved {
                server,
                path,
                connection_fields_changed,
            } => (server, path, connection_fields_changed),
            server_catalog::EditCommit::Conflict(reload) => {
                self.set_error(format!(
                    "server {original_label} changed elsewhere; reloaded the current settings"
                ));
                return ServerEditOutcome::Conflict(reload);
            }
            server_catalog::EditCommit::Missing => {
                self.set_error(format!("server {original_label} is no longer configured"));
                return ServerEditOutcome::Missing;
            }
        };
        self.rebuild_server_items();
        let active = self.room.active_server_id == Some(server.id);
        if active {
            // The session names its server by id, so a rename needs only the
            // display alias refreshed.
            self.room.server_alias = server.label.clone();
            self.push_file_policy();
        }
        if join {
            return match self.start_join(
                server.id,
                JoinOwner::ServerEditor {
                    client: owner,
                    request_id,
                },
            ) {
                JoinStart::Started(view) => {
                    self.set_status(format!("connecting to {}", view.server_label));
                    ServerEditOutcome::JoinStarted(view)
                }
                JoinStart::AlreadyActive => {
                    self.send_to(
                        owner,
                        TerminalEvent::Navigation(NavigationEvent::ResetBase(BaseScreen::Room)),
                    );
                    self.set_status(format!("already connected to {}", server.label));
                    ServerEditOutcome::Saved
                }
                JoinStart::Refused(message) => {
                    self.set_error(message);
                    // The entry is durable and only the join was refused, so
                    // the editor stays up over a draft rebuilt from what was
                    // written.
                    ServerEditOutcome::SavedButJoinFailed(Box::new(ServerEditDraft::from_server(
                        &server,
                        &self.config,
                    )))
                }
            };
        }
        // A plain save never reconnects — save and join is the explicit path for
        // that — so a change the running session took at connect only lands on
        // the next one, and the status has to say so.
        if connection_changed && self.network.is_some() && active {
            self.set_status("server saved; changes apply on reconnect");
        } else {
            self.set_status(format!("server saved to {}", path.display()));
        }
        ServerEditOutcome::Saved
    }

    pub(crate) fn cancel_open_audio_picker(&mut self, session: &mut SettingsSession) -> bool {
        let mut canceled = false;
        if session.input_picker.open {
            self.cancel_audio_input_picker(session);
            canceled = true;
        }
        if session.output_picker.open {
            self.cancel_audio_output_picker(session);
            canceled = true;
        }
        canceled
    }

    fn audio_picker_open(session: &SettingsSession) -> bool {
        session.active_audio_picker_open()
    }

    fn cancel_unfocused_audio_pickers(&mut self, session: &mut SettingsSession) {
        let focus = session.form.focus();
        if session.input_picker.open && focus != capture_device_id() {
            self.cancel_audio_input_picker(session);
        }
        if session.output_picker.open && focus != playback_device_id() {
            self.cancel_audio_output_picker(session);
        }
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

    pub(crate) fn open_settings(&mut self) {
        if self.room.settings_owner.is_some() {
            self.set_error("settings are already open in another client");
            return;
        }
        if self.allow_settings_preview_capture
            && (self.room.audio_devices.input_devices.is_empty()
                || self.room.audio_devices.output_devices.is_empty())
        {
            self.refresh_audio_devices();
        }
        self.start_settings_preview_capture();
        self.room.settings_generation = self.room.settings_generation.wrapping_add(1);
        self.room.settings_owner = Some(self.command_client);
        self.room.settings = Some(Arc::new(std::sync::Mutex::new(SettingsSession::new(
            &self.config,
            &self.room.audio_devices,
        ))));
        self.navigate_owner(NavigationEvent::OpenScreen(Box::new(ScreenSpec::Settings)));
    }

    /// Revokes core-owned leases for a terminal on every retirement path. UI
    /// teardown is best-effort; preview resources cannot depend on it.
    pub(crate) fn retire_client(&mut self, client_id: crate::client_channel::ClientId) {
        self.clients.remove(&client_id);
        self.rpc_clients.remove(&client_id);
        self.appearance.retire(client_id);
        if self
            .rpc_server_selection_issue
            .as_ref()
            .is_some_and(|issue| issue.owner == client_id)
        {
            self.rpc_server_selection_issue = None;
        }
        self.open_e2e_reviews.remove(&client_id);
        self.rpc_identity.retire(client_id);
        for clients in self.pending_identity_review.values_mut() {
            clients.retain(|pending| *pending != client_id);
        }
        self.room.remove_client_view(client_id);
        self.apply_pairing_input(PairingInput::OwnerRetired { owner: client_id });
        if self
            .rpc_settings
            .as_ref()
            .is_some_and(|session| session.owner == client_id)
        {
            self.finish_rpc_settings_session();
            return;
        }
        if self.room.settings_owner != Some(client_id) {
            return;
        }
        self.room.settings_owner = None;
        let Some(settings) = self.room.settings.take() else {
            return;
        };
        let mut session = settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.finish_settings_session(&mut session);
    }

    pub(crate) fn reject_server_switch_for_client(
        &mut self,
        client_id: crate::client_channel::ClientId,
    ) {
        let previous = std::mem::replace(&mut self.command_client, client_id);
        self.set_error(SERVER_SWITCH_TRANSFER_BLOCKED);
        self.command_client = previous;
    }

    pub(crate) fn close_settings(&mut self, session: &mut SettingsSession) {
        self.commit_settings_form_text(session);
        self.navigate_owner(NavigationEvent::CloseScreen);
    }

    pub(crate) fn finish_settings_session(&mut self, session: &mut SettingsSession) {
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        // Loopback is settings-only; guarantee it is off before the preview
        // capture stops, regardless of how the session ends (close/cancel/save/quit).
        self.set_loopback_enabled(false);
        session.draft.loopback = false;
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
        // Replay even without an editor commit: the destination may not have
        // been registered in the initial headless state yet, and this keeps
        // focus relocation plus rendering in one core update.
        self.drive_settings(session, FieldIntent::None, commit, None);
    }

    /// Switches the active settings tab: commits any live editor text first so
    /// the leaving tab's field applies, then replays the logic pass so the new
    /// tab's fields register (focus lands on its first field automatically).
    fn set_settings_tab(
        &mut self,
        session: &mut SettingsSession,
        tab: crate::ui::settings::SettingsTab,
    ) {
        if session.tab == tab {
            return;
        }
        self.cancel_open_audio_picker(session);
        let commit = session.form.clear_text();
        session.tab = tab;
        self.drive_settings(session, FieldIntent::None, commit, None);
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
        // A picker is owned by its device row. Mouse focus changes happen in
        // the renderer before this core logic pass, so dismiss a picker that
        // no longer owns focus before applying the clicked field's intent.
        self.cancel_unfocused_audio_pickers(session);
        let output = crate::ui::settings::settings_logic(
            &mut session.form,
            &mut session.draft,
            session.tab,
            &self.config.ui.resolve_theme(),
            &self.config.bindings,
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
        let bindings = session.draft.form_bindings();
        if self.config.ui.default_bindings != bindings {
            self.config.ui.default_bindings = bindings;
            self.mark_daemon_config_changed();
        }
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
        let old_files = self.config.files.clone();
        let old_p2p_enabled = self.config.p2p.enabled;
        let old_history_enabled = self.config.history.enabled;
        self.config.audio = session.draft.to_audio();
        self.config.web = session.draft.to_web();
        self.config.notifications = session.draft.to_notifications();
        self.config.files = session.draft.to_files(&self.config.files);
        self.config.p2p = session.draft.to_p2p(&self.config.p2p);
        self.config.history = session.draft.to_history();
        self.apply_ui_settings(session);
        self.apply_web_setting(&old_web, old_files.max_upload_bytes());
        self.apply_p2p_setting(old_p2p_enabled);
        self.apply_history_setting(old_history_enabled);
        self.apply_file_settings(&old_files);
        self.apply_echo_cancellation_setting();
        self.apply_output_volume_setting();
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        // Loopback is transient runtime state, not part of `AudioConfig`; reconcile
        // it straight from the draft. A failed enable resets the draft toggle so the
        // checkbox reflects the true state.
        self.set_loopback_enabled(session.draft.loopback);
        if session.draft.loopback && !self.loopback_tap.is_active() {
            session.draft.loopback = false;
        }
        let (capture, playback) = audio_restart_flags(&old, &self.config.audio);
        if capture || playback {
            self.schedule_audio_apply(capture, playback);
        }
        // Release the lazy notification stream when a playback restart is due
        // (don't pin the old output device) or when sounds no longer play
        // out-of-call; the next notification rebuilds it if needed.
        if playback || self.config.notifications.sounds != NotificationSoundMode::Always {
            self.drop_notification_playback();
        }
        self.mark_settings_dirty(session);
    }

    /// Re-resolves the active theme and publishes the daemon config change to
    /// every terminal-owned view.
    pub(crate) fn apply_theme(&mut self, selection: ThemeSelection) {
        if self.config.ui.theme == selection {
            return;
        }
        self.config.ui.theme = selection;
        self.mark_daemon_config_changed();
    }

    fn apply_web_setting(&mut self, old: &config::WebConfig, old_max_upload_bytes: u64) {
        if old.enabled && !self.config.web.enabled {
            self.pending_web_history.clear();
            if let Some(feed) = self.web_feed.take() {
                feed.stop();
                self.set_status("browser view stopped");
            }
            return;
        }

        if old.enabled
            && self.config.web.enabled
            && (old.bind != self.config.web.bind
                || old.allowed_origins != self.config.web.allowed_origins)
        {
            self.pending_web_history.clear();
            if let Some(feed) = self.web_feed.take() {
                feed.stop();
            }
        }

        // Behavior-only changes reach the running server and its connected
        // browsers over the feed channel; no restart.
        let web = &self.config.web;
        let max_upload_bytes = self.config.files.max_upload_bytes();
        if let Some(feed) = &self.web_feed
            && (old.readonly != web.readonly
                || old.autoplay != web.autoplay
                || old.viewer != web.viewer
                || old_max_upload_bytes != max_upload_bytes)
        {
            feed.set_config(web.readonly, web.autoplay, web.viewer, max_upload_bytes);
        }

        if self.config.web.enabled && self.web_feed.is_none() {
            let feed = spawn_web_feed(
                &self.config.web,
                self.download_store.clone(),
                self.config.files.max_upload_bytes(),
                self.room.room_name.clone(),
                self.config.web_css_path(),
                &self.events.tx,
            );
            match feed {
                Some(sender) => {
                    self.web_feed = Some(sender);
                    self.set_status(format!(
                        "browser view listening on {}",
                        self.config.web.bind
                    ));
                }
                None => {
                    self.set_error("browser view failed to start".to_string());
                }
            }
        }
    }

    /// Applies the interface knobs and url-open command live. Layout fields
    /// are read from the config per frame, so updating the config plus the
    /// daemon sync is enough; a max-messages change retrims scrollback now.
    fn apply_ui_settings(&mut self, session: &SettingsSession) {
        let old_max_messages = self.config.ui.max_messages;
        let ui = session.draft.to_ui(&self.config.ui);
        let ui_changed = ui.room_height != self.config.ui.room_height
            || ui.max_composer_height != self.config.ui.max_composer_height
            || ui.composer_padding != self.config.ui.composer_padding
            || ui.copy_on_select != self.config.ui.copy_on_select
            || ui.max_messages != self.config.ui.max_messages
            || ui.overscan != self.config.ui.overscan;
        if ui_changed {
            self.config.ui = ui;
            self.mark_daemon_config_changed();
        }
        if old_max_messages != self.config.ui.max_messages {
            self.apply_max_messages();
        }
        let url_open = session.draft.url_open_clean();
        if url_open != self.config.url_open {
            self.config.url_open = url_open;
            self.mark_daemon_config_changed();
        }
    }

    /// Applies file-transfer settings live: the memory ring re-caps, the
    /// network worker's resolved policy refreshes, and the upload throttle
    /// re-paces as soon as a field commits.
    fn apply_file_settings(&mut self, old: &config::FileConfig) {
        let files = self.config.files.clone();
        if old.download_memory_mb != files.download_memory_mb {
            self.download_store.set_cap(files.download_memory_bytes());
        }
        if old.download != files.download
            || old.download_dir != files.download_dir
            || old.max_download_mb != files.max_download_mb
            || old.max_upload_mb != files.max_upload_mb
        {
            self.push_file_policy();
        }
        if old.upload_rate_bytes != files.upload_rate_bytes && self.network.is_some() {
            self.send_network_command(
                NetworkCommand::SetUploadRate(files.upload_rate_bytes),
                false,
            );
        }
    }

    fn apply_p2p_setting(&mut self, old_enabled: bool) {
        if old_enabled == self.config.p2p.enabled {
            return;
        }
        if let Some(network) = &self.network {
            let _ = network
                .sender()
                .send(NetworkCommand::SetP2pEnabled(self.config.p2p.enabled));
        }
        if self.config.p2p.enabled {
            self.set_status("P2P enabled for this session");
        } else {
            self.set_status("P2P disabled; using relay");
        }
    }

    fn apply_history_setting(&mut self, old_enabled: bool) {
        if old_enabled == self.config.history.enabled {
            return;
        }
        if self.config.history.enabled {
            self.set_status("chat persistence enabled for future connections");
        } else {
            self.room.disable_history();
            self.pending_room_catalog_save = None;
            self.set_status("chat persistence disabled");
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

    /// Advances scheduled core work and reports which room-screen sections
    /// changed. Called once per run-loop iteration from [`crate::runtime`].
    /// Internal watchdog bookkeeping and persistence do not make a tick dirty.
    ///
    /// The hot periodic sources map to the sections that render them; rare
    /// changes escalate to [`DirtySections::ALL`] rather than auditing every
    /// surface they might touch.
    pub(crate) fn tick(&mut self) -> DirtySections {
        let now = Instant::now();
        let mut dirty = DirtySections::EMPTY;
        if self
            .active_audio_report
            .as_ref()
            .is_some_and(|report| now >= report.deadline)
        {
            self.finish_audio_report(true);
        }
        if self.start_pending_after_welcome() {
            dirty |= DirtySections::ALL;
        }
        if self.supervise(now) {
            dirty |= DirtySections::ALL;
        }
        if self.update_lobby_talking(now) {
            dirty |= DirtySections::USER_LIST;
        }
        if self.apply_pending_audio_restart() {
            dirty |= DirtySections::ALL;
        }
        self.apply_pending_room_catalog_save(now);
        self.supervise_voice_teardown(now);
        self.supervise_notification_playback(now);
        if self.refresh_session_projection() {
            dirty |= DirtySections::TOP_BAR | DirtySections::LOBBY_BAR | DirtySections::COMPOSE_BAR;
        }
        if self.sync_daemon_config_if_changed() {
            dirty |= DirtySections::ALL;
        }
        dirty
    }

    /// How long the runtime may sleep before the next [`Self::tick`]
    /// obligation comes due: [`TICK_POLL_INTERVAL`] while audio liveness needs
    /// polling, otherwise the earliest scheduled deadline, bounded by
    /// [`TICK_IDLE_INTERVAL`]. Events wake the runtime regardless.
    pub(crate) fn next_tick_timeout(&self, now: Instant) -> Duration {
        if self.tick_poll_active() {
            return TICK_POLL_INTERVAL;
        }
        let deadlines = [
            self.supervisor.network.due_at(),
            self.supervisor.control_socket.due_at(),
            self.supervisor.capture.due_at(),
            self.supervisor.playback.due_at(),
            self.supervisor.device_probe.next_at,
            self.pending_audio_apply
                .as_ref()
                .map(|pending| pending.deadline),
            self.pending_room_catalog_save
                .as_ref()
                .map(|pending| pending.deadline),
            self.pending_voice_teardown_at,
            self.notification_playback_idle_at,
            self.active_audio_report
                .as_ref()
                .map(|report| report.deadline),
        ];
        let mut timeout = TICK_IDLE_INTERVAL;
        for deadline in deadlines.into_iter().flatten() {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        timeout
    }

    /// Whether any tick source polls state that only changes while audio
    /// runs, so no deadline can describe when it next needs attention. The
    /// talking-display check covers the release decay after streams stop.
    fn tick_poll_active(&self) -> bool {
        self.capture.is_some()
            || self.playback.is_some()
            || self.notification_playback.is_some()
            || self
                .room
                .participants
                .entries
                .iter()
                .any(|entry| entry.talking_display)
    }

    fn mark_daemon_config_changed(&mut self) {
        self.daemon_config_generation = self.daemon_config_generation.wrapping_add(1);
    }

    fn sync_daemon_config_if_changed(&mut self) -> bool {
        if self.synced_daemon_config_generation == self.daemon_config_generation {
            return false;
        }
        self.broadcast_config_changed();
        self.synced_daemon_config_generation = self.daemon_config_generation;
        true
    }

    fn apply_max_messages(&mut self) {
        self.room.set_max_messages(self.config.ui.max_messages);
        self.send_web_history_snapshot(crate::web_server::WebAudience::All);
    }

    /// Projects audio display facts into the shared session so every view
    /// renders them without reaching into core state. Runs once per tick.
    fn refresh_session_projection(&mut self) -> bool {
        let network_selected = self.network.is_some();
        let capture_health = self.capture_audio_health();
        let playback_health = self.playback_audio_health();
        let capture_stats = self.capture.as_ref().map(|capture| capture.stats());
        let dirty = self.room.network_selected != network_selected
            || self.room.capture_health != capture_health
            || self.room.playback_health != playback_health
            || self.room.capture_stats.is_some() != capture_stats.is_some();
        self.room.network_selected = network_selected;
        self.room.capture_health = capture_health;
        self.room.playback_health = playback_health;
        self.room.capture_stats = capture_stats;
        dirty
    }

    fn apply_pending_room_catalog_save(&mut self, now: Instant) {
        let Some(pending) = &self.pending_room_catalog_save else {
            return;
        };
        if now < pending.deadline {
            return;
        }
        self.save_room_catalog();
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
        if !self.local_voice_state().is_deafened() {
            return;
        }
        self.voice_tx_enabled.store(false, Ordering::Relaxed);
        self.stop_mic_capture();
    }

    /// Tears down the lazy notification playback stream once its idle deadline
    /// passes, or early when its worker died so the next notification rebuilds
    /// it instead of feeding a zombie stream.
    fn supervise_notification_playback(&mut self, now: Instant) {
        if self
            .notification_playback
            .as_ref()
            .is_some_and(LivePlayback::worker_finished)
        {
            self.drop_notification_playback();
            return;
        }
        let Some(deadline) = self.notification_playback_idle_at else {
            return;
        };
        if now < deadline {
            return;
        }
        self.drop_notification_playback();
    }

    fn update_lobby_talking(&mut self, now: Instant) -> bool {
        let local_user = self.user_id;
        let local_state = self.local_voice_state();
        let local_raw_active = if local_state.is_muted() {
            false
        } else if self.config.soundboard.enabled {
            self.soundboard_busy.load(Ordering::Relaxed)
        } else {
            // Drive the self indicator from the capture transmit gate, not a raw
            // level threshold: residual denoiser noise clears the threshold but is
            // silence-gated out of the outbound stream, so the dot must stay dark.
            self.capture
                .as_ref()
                .is_some_and(|capture| capture.stats().snapshot().voice_active)
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
        updates
            .into_iter()
            .fold(false, |dirty, (user_id, raw_active)| {
                self.room
                    .update_talking_display(user_id, raw_active, now, LOBBY_TALKING_RELEASE)
                    || dirty
            })
    }

    fn apply_pending_audio_restart(&mut self) -> bool {
        let Some(pending) = &self.pending_audio_apply else {
            return false;
        };
        if Instant::now() < pending.deadline {
            return false;
        }
        let Some(PendingAudioApply {
            capture, playback, ..
        }) = self.pending_audio_apply.take()
        else {
            return false;
        };
        let mut applied = Vec::new();
        if capture {
            self.restart_capture_stream();
            applied.push("capture");
        }
        if playback {
            self.supervisor.playback.reset();
            if self.loopback_uses_dedicated_playback() {
                self.restart_loopback_output();
            } else {
                self.restart_playback_stream();
            }
            applied.push("playback");
        }
        if !applied.is_empty() {
            self.set_status(format!("audio settings applied ({})", applied.join(", ")));
        }
        true
    }

    /// Health of the capture side for status-bar error reporting.
    pub(crate) fn capture_audio_health(&self) -> AudioSideHealth {
        AudioSideHealth {
            state: self.supervisor.capture.health().state,
        }
    }

    pub(crate) fn playback_audio_health(&self) -> AudioSideHealth {
        AudioSideHealth {
            state: self.supervisor.playback.health().state,
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
        let playback_should_run = self.voice_tx_enabled.load(Ordering::Relaxed)
            && !self.local_voice_state().is_deafened();
        if playback_should_run || self.playback.is_some() {
            self.restart_playback_stream();
        }
        self.refresh_audio_devices();
        self.set_status("audio reset: rebuilding streams");
    }

    fn supervise(&mut self, now: Instant) -> bool {
        let mut dirty = self.supervise_network(now);
        dirty |= self.supervise_control_socket(now);
        dirty |= self.supervise_capture(now);
        dirty |= self.supervise_playback(now);
        self.supervise_device_probe(now);
        dirty
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

    fn supervise_network(&mut self, now: Instant) -> bool {
        let mut dirty = false;
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
            dirty = self.schedule_network_recovery(now, "network worker stopped");
        }
        if let Some(reason) = self.supervisor.network.take_due(now) {
            self.restart_network_worker(&reason);
            dirty = true;
        }
        dirty
    }

    fn supervise_control_socket(&mut self, now: Instant) -> bool {
        let mut dirty = false;
        if self
            .control_socket
            .as_ref()
            .is_some_and(local_control::ControlSocket::is_finished)
        {
            dirty = self.schedule_control_socket_recovery(now, "control socket worker stopped");
        }
        if let Some(reason) = self.supervisor.control_socket.take_due(now) {
            self.restart_control_socket(&reason);
            dirty = true;
        }
        dirty
    }

    fn supervise_capture(&mut self, now: Instant) -> bool {
        let mut dirty = false;
        let apply_owns_restart = self
            .pending_audio_apply
            .as_ref()
            .is_some_and(|pending| pending.capture);
        if !apply_owns_restart && let Some(cause) = self.supervisor.capture.take_due_rebuild(now) {
            self.recover_capture_stream(now, cause);
            dirty = true;
        }
        let Some(capture) = &self.capture else {
            self.supervisor.capture_watch = CaptureWatch::default();
            let should_run =
                self.voice_tx_enabled.load(Ordering::Relaxed) || self.settings_preview_capture;
            if !should_run {
                self.supervisor.capture.reset();
            }
            return dirty;
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
            dirty = true;
        }
        dirty
    }

    fn supervise_playback(&mut self, now: Instant) -> bool {
        let mut dirty = false;
        let apply_owns_restart = self
            .pending_audio_apply
            .as_ref()
            .is_some_and(|pending| pending.playback);
        if !apply_owns_restart && let Some(cause) = self.supervisor.playback.take_due_rebuild(now) {
            self.recover_playback_stream(now, cause);
            dirty = true;
        }
        let Some(playback) = &self.playback else {
            self.supervisor.playback_watch = PlaybackWatch::default();
            let should_run = self.voice_tx_enabled.load(Ordering::Relaxed)
                && !self.local_voice_state().is_deafened();
            if !should_run {
                self.supervisor.playback.reset();
            }
            return dirty;
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
            dirty = true;
        }
        dirty
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

    fn schedule_network_recovery(&mut self, now: Instant, reason: impl Into<String>) -> bool {
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
                true
            }
            RecoverySchedule::Pending => false,
            RecoverySchedule::Exhausted => {
                self.stop_audio();
                if let Some(network) = self.network.take() {
                    network.stop();
                }
                self.reset_room_for_disconnect();
                self.set_error(format!("network recovery exhausted: {reason}"));
                true
            }
        }
    }

    fn schedule_control_socket_recovery(
        &mut self,
        now: Instant,
        reason: impl Into<String>,
    ) -> bool {
        let reason = reason.into();
        match self.supervisor.control_socket.schedule(now, reason.clone()) {
            RecoverySchedule::Scheduled(delay) => {
                if !delay.is_zero() {
                    self.set_status(format!(
                        "file-upload socket down; retrying in {}s",
                        delay.as_secs()
                    ));
                }
                true
            }
            RecoverySchedule::Pending => false,
            RecoverySchedule::Exhausted => {
                self.control_socket.take();
                self.set_error(format!("file-upload socket down: {reason}"));
                true
            }
        }
    }

    fn restart_network_worker(&mut self, reason: &str) {
        if self.room.active_server_id.is_none() {
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
        self.restart_active_session();
        self.pending_network_commands = queued;
        if self.network.is_some() {
            self.supervisor.network.reset();
        } else {
            self.supervisor.network = network_recovery;
            self.schedule_network_recovery(
                Instant::now(),
                format!("failed to restart network worker after {reason}"),
            );
        }
    }

    fn restart_control_socket(&mut self, reason: &str) {
        kvlog::warn!("restarting local control socket", reason);
        self.control_socket.take();
        match local_control::ControlSocket::spawn(self.events.sender()) {
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
                kvlog::warn!(
                    "capture stream rebuild failed",
                    cause = cause.label(),
                    kind = error.kind.label(),
                    error = error.message.as_str()
                );
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
            kvlog::warn!(
                "capture stream start failed",
                kind = error.kind.label(),
                error = error.message.as_str()
            );
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
        let restore_loopback = self.loopback_tap.is_active() && self.loopback_playback.is_none();
        if restore_loopback {
            self.loopback_tap.clear();
        }
        if self.network.is_none() {
            if restore_loopback {
                self.restart_loopback_output();
            }
            return;
        }
        self.set_network_playback_sink(None);
        self.playback.take();
        self.start_playback_stream(true);
        if restore_loopback {
            if self.playback.is_some() {
                self.restart_loopback_output();
            } else {
                self.fail_loopback(AudioStartError::transient("voice playback is unavailable"));
            }
        }
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
        // Only the default-bindings choice triggers this, so no text edit is in
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
            if self.room.audio_devices.input_devices.is_empty() {
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
            if self.room.audio_devices.output_devices.is_empty() {
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

    #[allow(dead_code)] // Retained while App-level behavior tests migrate.
    pub(crate) fn open_selected_user_volume(&mut self) {
        let selected = match self.room.selected_remote_user(self.user_id) {
            Ok(user) => user,
            Err(error) => {
                self.set_status(error.status_text());
                return;
            }
        };
        let user_id = selected.user_id;
        let name = selected.username;
        let value_db = self
            .config
            .user_volume_db(self.room.active_server_id.unwrap_or_default(), user_id);
        self.room.begin_volume_preview(user_id, value_db);
        let dialog = UserVolumeDialog::new(
            user_id,
            name.clone(),
            value_db,
            &self.config.ui.resolve_theme(),
        );
        self.navigate_owner(NavigationEvent::ShowOverlay(Box::new(
            OverlaySpec::UserVolume(dialog),
        )));
        self.set_status(format!("adjusting local volume for {name}"));
    }

    pub(crate) fn toggle_user_mute(&mut self, user_id: UserId) {
        if Some(user_id) == self.user_id {
            self.set_status("select another user to mute");
            return;
        }
        let name = self.room.username_of(user_id);
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
                username,
                original_db,
            } => {
                self.config.set_user_volume_db(
                    self.room.active_server_id.unwrap_or_default(),
                    user_id,
                    original_db,
                );
                self.apply_user_audio_control_with_volume(user_id, original_db);
                self.room.clear_volume_preview();
                self.set_status(format!("canceled local volume for {username}"));
                return true;
            }
            UserVolumeEvent::Save {
                user_id,
                username,
                value_db,
            } => {
                self.config.set_user_volume_db(
                    self.room.active_server_id.unwrap_or_default(),
                    user_id,
                    value_db,
                );
                self.apply_user_audio_control_with_volume(user_id, value_db);
                match self.config.save_runtime() {
                    Ok(path) => {
                        self.config.config_path = Some(path.clone());
                        self.room.clear_volume_preview();
                        self.set_status(format!(
                            "saved local volume {}dB for {} to {}",
                            format_signed_db(value_db),
                            username,
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

    fn apply_output_volume_setting(&self) {
        self.output_volume_percent_bits.store(
            config::snap_output_volume_percent(self.config.audio.output_volume).to_bits(),
            Ordering::Relaxed,
        );
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
                // Idempotent re-application; the live-apply path already synced
                // these when the fields committed.
                self.apply_max_messages();
                self.download_store
                    .set_cap(self.config.files.download_memory_bytes());
                self.push_file_policy();
                self.set_status(format!("settings saved to {}", path.display()));
            }
            Err(error) => self.set_error(error),
        }
    }

    /// Refreshes the network worker's resolved download policy after a config
    /// change. The join-time advertisement to the server updates on reconnect.
    pub(crate) fn push_file_policy(&mut self) {
        if self.network.is_none() {
            return;
        }
        let Some(server) = self
            .room
            .active_server_id
            .and_then(|server_id| self.config.server_by_id(server_id))
        else {
            return;
        };
        let policy = self.config.file_policy(server);
        self.send_network_command(NetworkCommand::SetFilePolicy(policy), false);
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
        if self.room.audio_devices.refresh_in_flight {
            self.set_status("refreshing audio devices");
            return;
        }

        let restart_preview =
            self.settings_preview_capture && !self.voice_tx_enabled.load(Ordering::Relaxed);
        if restart_preview {
            self.stop_mic_capture();
        }

        let id = self.room.audio_devices.next_refresh_id;
        self.room.audio_devices.next_refresh_id =
            self.room.audio_devices.next_refresh_id.saturating_add(1);
        self.room.audio_devices.refresh_in_flight = true;
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

    fn run_slash_command(&mut self, room_id: Option<RoomId>, input: String) {
        match input.as_str() {
            "/quit" => self.set_status("use Ctrl-C to quit"),
            "/mute" => self.set_voice_state(VoiceState::Muted),
            "/unmute" => self.set_voice_state(VoiceState::Live),
            "/deafen" => self.set_voice_state(VoiceState::Deafened),
            "/undeafen" => self.set_voice_state(VoiceState::Live),
            "/muted" => self.show_mute_status(),
            "/deafened" => self.show_deafen_status(),
            "/audio" => self.show_audio_status(),
            "/audio-reset" => self.audio_manual_reset(),
            "/stats" => self.set_status("stats display is controlled by the terminal view"),
            "/clear" => self.set_status("chat display cleared only in terminal views"),
            "/help" => self.show_command_help(room_id),
            "/config" | "/settings" => self.open_settings(),
            "/servers" if self.network.is_some() => {
                self.navigate_owner(NavigationEvent::CloseScreen)
            }
            "/servers" => self.open_server_select(),
            "/soundboard" => self.show_soundboard(),
            "/users" => self.show_users(),
            "/whoami" => self.show_current_user(),
            "/rooms" => self.open_room_switcher(),
            "/room-settings" => self.open_room_settings(),
            "/room" => self.set_error("usage: /room name"),
            command if command.starts_with("/room ") => self.switch_room_command(command),
            "/dm" => self.set_error("usage: /dm user"),
            command if command.starts_with("/dm ") => self.open_dm_command(command),
            "/identity" => self.identity_command(room_id, "/identity"),
            command if command.starts_with("/identity ") => self.identity_command(room_id, command),
            "/devices" => {
                self.send_network_command(NetworkCommand::ListE2eDevices, true);
            }
            "/devices link" => {
                self.send_network_command(NetworkCommand::CreateDeviceLink, true);
            }
            "/devices recovery" => {
                self.set_error(
                    "offline recovery codes are not supported; use /devices link from an active device"
                        .to_string(),
                );
            }
            command if command.starts_with("/devices revoke ") => {
                let requested = command.trim_start_matches("/devices revoke ").trim();
                let (encoded, confirmed) = requested
                    .strip_suffix(" CONFIRM")
                    .map_or((requested, false), |encoded| (encoded.trim(), true));
                let device_id = rpc::crypto::decode_hex(encoded)
                    .ok()
                    .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
                    .map(rpc::ids::DeviceId);
                match device_id {
                    Some(device_id) if confirmed => {
                        self.send_network_command(
                            NetworkCommand::RevokeE2eDevice { device_id },
                            true,
                        );
                    }
                    Some(_) => self.set_error(format!(
                        "Revoking device {encoded} permanently signs it out. Run /devices revoke {encoded} CONFIRM to continue."
                    )),
                    None => self.set_error("device id must be exactly 16 bytes of hex"),
                }
            }
            "/voice" => match room_id {
                Some(room_id) => self.join_voice_room(room_id),
                None => self.set_error("no room selected"),
            },
            command if command.starts_with("/voice ") => {
                let name = command.trim_start_matches("/voice ").trim().to_string();
                self.join_voice_command(Some(&name));
            }
            "/voice-leave" => self.leave_voice_command(),
            "/video" => self.show_video_status(),
            "/upload" => self.set_error("usage: /upload file_path/filename.ext"),
            command if command.starts_with("/upload ") => {
                self.upload_file_command(room_id, command)
            }
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
            command => self.set_error(format!("unknown command: {command}")),
        }
    }

    fn switch_room_command(&mut self, command: &str) {
        let name = command.trim_start_matches("/room ").trim();
        if name.is_empty() {
            self.set_error("usage: /room name");
            return;
        }
        let Some(room_id) = self.room.find_room_by_name(name) else {
            self.set_error(format!("no room named {name}"));
            return;
        };
        if !self.set_viewed_room(room_id) {
            self.set_error("room is no longer available");
        }
    }

    fn open_dm_command(&mut self, command: &str) {
        let name = command.trim_start_matches("/dm ").trim();
        if name.is_empty() {
            self.set_error("usage: /dm user");
            return;
        }
        let Some(user_id) = self.room.user_id_by_name(name) else {
            self.set_error(format!("no user named {name}"));
            return;
        };
        if let Err(error) = self.open_dm_with(user_id) {
            self.set_error(error);
        }
    }

    /// Drops the independent confirmation pinned for `identity`.
    ///
    /// Returns the reason it was refused, so a caller that owes someone a reply
    /// — an RPC renderer, say — can report the refusal instead of claiming
    /// success for work that never happened. A command merely queued while the
    /// network is down is still `Ok`: it runs on reconnect.
    fn forget_e2e_identity(
        &mut self,
        identity: crate::e2e::AcceptedPeerIdentity,
    ) -> Result<(), String> {
        let matches =
            self.room
                .e2e_trust_state(identity.room_id)
                .is_some_and(|state| match state {
                    room::DmTrustState::Accepted {
                        peer,
                        identity: current,
                        ..
                    }
                    | room::DmTrustState::Verified {
                        peer,
                        identity: current,
                    } => {
                        *peer == identity.user_id
                            && current.room_id == identity.identity.room_id
                            && current.user_id == identity.identity.user_id
                            && current.public_key == identity.identity.public_key
                    }
                });
        if !matches {
            return Err("that saved identity is stale; review the current identity".into());
        }
        if self.send_network_command(
            NetworkCommand::ForgetPeerIdentity { expected: identity },
            true,
        ) {
            self.set_status("forgetting independent identity verification");
        }
        Ok(())
    }

    fn active_server_identity_key(&self) -> Result<[u8; 32], String> {
        let server_id = self
            .room
            .active_server_id
            .ok_or_else(|| "select a server before verifying identities".to_string())?;
        let server = self
            .config
            .server_by_id(server_id)
            .ok_or_else(|| "the connected server is no longer configured".to_string())?;
        if server.server_public_key.trim().is_empty() {
            return Ok(rpc::crypto::dev_server_public_key());
        }
        rpc::crypto::ed25519_public_key_from_hex(server.server_public_key.trim())
            .map_err(|_| "the configured server public key is invalid".to_string())
    }

    fn local_verification_text(&self) -> Result<String, String> {
        let server_key = self.active_server_identity_key()?;
        let account_id = self
            .e2e_account_id
            .ok_or_else(|| "the signed account identity is still being fetched".to_string())?;
        let user_id = self
            .user_id
            .ok_or_else(|| "authentication has not completed".to_string())?;
        crate::e2e_identity::VerificationText::new(&server_key, user_id.0, &account_id.0)
            .map(|text| text.encode())
            .map_err(|_| "could not build the local verification text".to_string())
    }

    /// The name to show for a peer, falling back to the roster when the pinned
    /// profile carries no username.
    pub(super) fn identity_display_username(
        &self,
        identity: &crate::e2e::AcceptedPeerIdentity,
    ) -> String {
        if identity.identity.username.trim().is_empty() {
            self.room.username_of(identity.user_id)
        } else {
            identity.identity.username.clone()
        }
    }

    /// What just happened to a peer's pin, phrased for a status line. Every
    /// frontend reports a trust change in these words.
    pub(super) fn e2e_trust_change_status(
        &self,
        identity: &crate::e2e::AcceptedPeerIdentity,
    ) -> String {
        let username = self.identity_display_username(identity);
        match identity.trust_level {
            crate::config::E2eTrustLevel::Accepted => {
                format!("Forgot independent verification for {username}")
            }
            crate::config::E2eTrustLevel::Verified => {
                format!("Verified {username}'s encryption identity")
            }
        }
    }

    fn open_e2e_identity(
        &mut self,
        target: crate::client_channel::E2eIdentityTarget,
        error: Option<String>,
    ) {
        let local_verification_text = match self.local_verification_text() {
            Ok(text) => text,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        let replaces_open = self
            .open_e2e_reviews
            .get(&self.command_client)
            .is_some_and(|(room_id, _, _)| *room_id == target.room_id);
        self.open_e2e_reviews.insert(
            self.command_client,
            (
                target.room_id,
                target.public_key.clone(),
                target.accepted.trust_level,
            ),
        );
        // Native renderers get the same review as a pushed document; only
        // terminals drive it through the overlay stack.
        if self.rpc_clients.contains(&self.command_client) {
            self.rpc_identity.open(self.command_client, &target, error);
            return;
        }
        let overlay = OverlaySpec::E2eIdentity(crate::client_channel::E2eIdentityOverlay {
            target,
            local_verification_text,
            pasted_verification_text: String::new(),
            result: None,
            error,
        });
        self.navigate_owner(if replaces_open {
            NavigationEvent::ReplaceOverlay(Box::new(overlay))
        } else {
            NavigationEvent::ShowOverlay(Box::new(overlay))
        });
    }

    /// Pins `target` as independently confirmed. Refusals come back as `Err` for
    /// the same reason [`Self::forget_e2e_identity`]'s do.
    fn confirm_e2e_verification(
        &mut self,
        target: crate::client_channel::E2eIdentityTarget,
    ) -> Result<(), String> {
        let room_id = target.room_id;
        let expected = target.accepted.clone();
        let matches = self
            .room
            .e2e_trust_state(room_id)
            .is_some_and(|state| match state {
                room::DmTrustState::Accepted { peer, identity, .. }
                | room::DmTrustState::Verified { peer, identity } => {
                    *peer == expected.user_id
                        && identity.room_id == expected.identity.room_id
                        && identity.user_id == expected.identity.user_id
                        && identity.public_key == expected.identity.public_key
                }
            });
        if !matches || expected.identity.public_key != target.public_key {
            return Err("the accepted identity changed during verification".into());
        }
        let sent = self.send_network_command(NetworkCommand::VerifyPeerIdentity { expected }, true);
        if sent {
            self.set_status("saving independently confirmed identity");
        }
        Ok(())
    }

    /// Opens the one identity screen for a DM peer. The command itself never
    /// changes state; every save, confirmation, or forget action remains bound
    /// to the exact identity shown by the overlay.
    fn identity_command(&mut self, room_id: Option<RoomId>, command: &str) {
        let name = command.strip_prefix("/identity").unwrap_or("").trim();
        let user_id = if name.is_empty() {
            let Some(room_id) = room_id else {
                self.set_error("open a DM first, or use /identity user");
                return;
            };
            let Some(peer) = self.room.dm_peer_of(room_id) else {
                self.set_error("open a DM first, or use /identity user");
                return;
            };
            peer
        } else {
            let Some(user_id) = self.room.user_id_by_name(name) else {
                self.set_error(format!("no user named {name}"));
                return;
            };
            user_id
        };
        if let Err(error) = self.request_identity_review(user_id, self.command_client) {
            self.set_error(error);
        }
    }

    /// Asks the worker for `user_id`'s accepted identity on `client`'s behalf,
    /// opening the DM first if there is not one yet.
    ///
    /// The client is queued for the answer only once the request is actually on
    /// its way: a review that was refused must not leave an entry behind that a
    /// later, unrelated identity event would deliver a surprise document to.
    pub(super) fn request_identity_review(
        &mut self,
        user_id: UserId,
        client: crate::client_channel::ClientId,
    ) -> Result<(), String> {
        if self.room.dm_room_for_peer(user_id).is_none() {
            self.open_dm_with(user_id)?;
        } else {
            self.send_network_command(NetworkCommand::ReviewPeerIdentity { user_id }, true);
        }
        self.pending_identity_review
            .entry(user_id)
            .or_default()
            .push_back(client);
        Ok(())
    }

    /// Asks the server for the DM room with `user_id`; the view switches when
    /// `DmOpened` arrives.
    pub(crate) fn open_dm_with(&mut self, user_id: UserId) -> Result<(), String> {
        if self.network.is_none() {
            return Err("select a server before opening dms".into());
        }
        // The server keeps serving DM rooms opened before the operator turned
        // DMs off, so only a first-time open is refused here.
        if !self.server_dms_enabled && self.room.dm_room_for_peer(user_id).is_none() {
            return Err("this server has direct messages disabled".into());
        }
        if self.send_network_command(NetworkCommand::OpenDm(user_id), true) {
            self.pending_dm_clients
                .entry(user_id)
                .or_default()
                .push_back(self.command_client);
        }
        self.set_status(format!(
            "opening dm with {}",
            self.room.username_of(user_id)
        ));
        Ok(())
    }

    fn open_dm_room_for_client(
        &mut self,
        client_id: crate::client_channel::ClientId,
        room_id: RoomId,
        peer: UserId,
    ) {
        let status = format!("dm with {}", self.room.username_of(peer));
        let previous = std::mem::replace(&mut self.command_client, client_id);
        if self.set_viewed_room(room_id) {
            self.set_status(status);
        }
        self.command_client = previous;
    }

    /// Moves the voice call to `name`'s room, or the viewed room without an
    /// argument. Mirrors the auto-join in the `Authenticated` handler.
    fn join_voice_command(&mut self, name: Option<&str>) {
        let target = match name {
            Some(name) => match self.room.find_room_by_name(name) {
                Some(room_id) => room_id,
                None => {
                    self.set_error(format!("no room named {name}"));
                    return;
                }
            },
            None => match self.room.viewed_room {
                Some(room_id) => room_id,
                None => {
                    self.set_error("no room selected");
                    return;
                }
            },
        };
        self.join_voice_room(target);
    }

    /// Moves the voice call to `target`; which room is viewed is the caller's
    /// concern.
    pub(crate) fn join_voice_room(&mut self, target: RoomId) {
        if self.network.is_none() {
            self.set_error("select a server before joining voice");
            return;
        }
        if self.room.voice_room == Some(target) || self.requested_voice_room == Some(target) {
            self.set_status("already in this room's voice call");
            return;
        }
        self.voice_left = false;
        self.requested_voice_room = Some(target);
        self.send_network_command(NetworkCommand::JoinVoice(target), true);
        self.publish_voice_state();
    }

    fn leave_voice_command(&mut self) {
        if self.room.voice_room.is_none() && self.requested_voice_room.is_none() {
            self.set_status("not in a voice call");
            return;
        }
        self.voice_left = true;
        self.requested_voice_room = None;
        self.send_network_command(NetworkCommand::LeaveVoice, true);
        self.set_status("leaving voice");
    }

    fn upload_file_command(&mut self, room_id: Option<RoomId>, command: &str) {
        let path = command.trim_start_matches("/upload ").trim();
        if path.is_empty() {
            self.set_error("usage: /upload file_path/filename.ext");
            return;
        }
        if self.network.is_some() {
            self.send_network_command(
                NetworkCommand::UploadFile {
                    room_id,
                    request: UploadFileRequest::new(std::path::PathBuf::from(path)),
                },
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
    #[allow(dead_code)] // Removed after all modes dispatch through ViewCx.
    pub(crate) fn open_paste_image_dialog(&mut self, image: crate::clipboard_paste::ImagePaste) {
        self.navigate_owner(NavigationEvent::ShowOverlay(Box::new(
            OverlaySpec::PasteUpload(image),
        )));
    }

    /// Validates the chosen name and queues the pasted upload. Returns `Err`
    /// with a message when the dialog should stay open (no server, bad name).
    pub(crate) fn confirm_paste_image_upload(
        &mut self,
        room_id: Option<RoomId>,
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
            inline_bytes: None,
        };
        self.send_network_command(NetworkCommand::UploadFile { room_id, request }, true);
        self.set_status(format!("queued upload {name}"));
        Ok(())
    }

    fn show_command_help(&mut self, room_id: Option<RoomId>) {
        let body = slash_command_help();
        if !room_id.is_some_and(|room_id| self.room.push_notice_to(room_id, "help", body.clone())) {
            self.send_to(
                self.command_client,
                TerminalEvent::LocalNotice {
                    sender: "help".to_string(),
                    body,
                    error: false,
                },
            );
        }
        self.set_status("slash commands listed");
    }

    fn toggle_mute(&mut self) {
        let state = self.local_voice_state().toggle_mute();
        self.set_voice_state(state);
    }

    fn toggle_deafen(&mut self) {
        let state = self.local_voice_state().toggle_deafen();
        self.set_voice_state(state);
    }

    fn set_voice_state(&mut self, state: VoiceState) {
        self.voice_state.store(state, Ordering::Relaxed);

        if state.is_deafened() {
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
            self.drop_notification_playback();
        } else {
            self.pending_voice_teardown_at = None;
            self.ensure_room_voice_running();
        }
        self.publish_voice_state();
        self.show_voice_state(state);
    }

    fn ensure_room_voice_running(&mut self) {
        if self.room.voice_room.is_none() {
            return;
        }
        if self.voice_tx_enabled.load(Ordering::Relaxed) && self.playback.is_some() {
            return;
        }
        self.start_room_voice();
    }

    fn local_voice_state(&self) -> VoiceState {
        self.voice_state.load(Ordering::Relaxed)
    }

    fn publish_voice_state(&mut self) {
        let state = self.local_voice_state();
        if let Some(user_id) = self.user_id {
            self.room.voice_state_changed(user_id, state);
        }
        self.send_network_command(NetworkCommand::SetVoiceState(state), false);
    }

    fn show_voice_state(&mut self, state: VoiceState) {
        self.set_status(match state {
            VoiceState::Live => "live",
            VoiceState::Muted => "microphone muted",
            VoiceState::Deafened => "deafened",
        });
    }

    fn activate_top_bar_video(&mut self) {
        match self.room.screencast_status.phase {
            ScreencastPhase::Failed => self.show_video_status(),
            ScreencastPhase::Off => self.restart_cached_screencast(),
            ScreencastPhase::Starting | ScreencastPhase::Live => self.stop_screencast_to_off(),
            ScreencastPhase::Idle => self.show_video_status(),
        }
    }

    fn restart_cached_screencast(&mut self) {
        let Some(command) = self.cached_screencast_start.clone() else {
            self.set_error("no cached video command");
            return;
        };
        self.handle_screencast_command(command.into_command());
    }

    fn show_mute_status(&mut self) {
        self.set_status(if self.local_voice_state().is_deafened() {
            "deafened; microphone muted"
        } else if self.local_voice_state().is_muted() {
            "microphone muted"
        } else {
            "microphone unmuted"
        });
    }

    fn show_deafen_status(&mut self) {
        self.set_status(if self.local_voice_state().is_deafened() {
            "deafened"
        } else {
            "not deafened"
        });
    }

    fn show_video_status(&mut self) {
        let notice = self.video_diagnostics_notice();
        if self.room.screencast_status.phase == ScreencastPhase::Failed {
            self.push_error_notice("video", notice);
        } else {
            self.push_notice("video", notice);
        }
        self.set_status(self.video_status_summary());
    }

    fn video_status_summary(&self) -> String {
        match self.room.screencast_status.phase {
            ScreencastPhase::Idle => match &self.room.screencast_status.last_issue {
                Some(issue) => format!("video idle; last issue: {}", issue.reason),
                None => "video idle".to_string(),
            },
            ScreencastPhase::Off => "video off".to_string(),
            ScreencastPhase::Starting => "video starting".to_string(),
            ScreencastPhase::Live => format!(
                "video live: {}",
                video_rate_label(self.room.screencast_status.rolling_bytes_per_sec)
            ),
            ScreencastPhase::Failed => self
                .room
                .screencast_status
                .last_issue
                .as_ref()
                .map(|issue| format!("video failed: {}", issue.reason))
                .unwrap_or_else(|| "video failed".to_string()),
        }
    }

    fn video_diagnostics_notice(&self) -> String {
        let status = &self.room.screencast_status;
        let mut lines = Vec::new();
        lines.push(format!("state: {}", screencast_phase_label(status.phase)));
        if let Some(stream_id) = status.stream_id {
            lines.push(format!("stream: {}", stream_id.0));
        }
        if let Some(codec) = &status.codec {
            let size = match (status.coded_width, status.coded_height) {
                (Some(width), Some(height)) if width != 0 && height != 0 => {
                    format!(" {width}x{height}")
                }
                _ => String::new(),
            };
            lines.push(format!("codec: {codec}{size}"));
        }
        lines.push(format!(
            "transfer: {} frames / {} total / {} recent",
            status.total_frames,
            crate::client_net::format_bytes(status.total_bytes),
            video_rate_label(status.rolling_bytes_per_sec)
        ));
        if let Some(started) = status.started_at {
            lines.push(format!(
                "started: {} ago",
                audio_diagnostics::format_event_age(started.elapsed())
            ));
        }
        if let Some(ended) = status.ended_at {
            lines.push(format!(
                "ended: {} ago",
                audio_diagnostics::format_event_age(ended.elapsed())
            ));
        }
        match &status.last_issue {
            Some(issue) => lines.push(format!(
                "last issue: {} ago: {}",
                audio_diagnostics::format_event_age(issue.at.elapsed()),
                issue.reason
            )),
            None => lines.push("last issue: none".to_string()),
        }
        lines.join("\n")
    }

    /// Formatted `health` and `events` sections for `/audio`. Built even while
    /// streams are down: that is exactly when diagnostics matter.
    fn audio_diagnostics_sections(&self, event_limit: usize) -> (Vec<String>, Vec<String>) {
        let now = Instant::now();
        let health_lines = vec![
            format!("mic: {}", self.supervisor.capture.health().describe(now)),
            format!("spk: {}", self.supervisor.playback.health().describe(now)),
        ];
        let recent_events = self
            .audio_events
            .iter_recent()
            .take(event_limit)
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
        let (health_lines, recent_events) =
            self.audio_diagnostics_sections(AUDIO_STATUS_EVENT_LIMIT);
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
                .map(|capture| capture.device_info_live()),
            self.playback
                .as_ref()
                .map(|playback| playback.device_info_live()),
            health_lines,
            recent_events,
        );
        self.push_notice("audio", diagnostics.notice_body());
        self.set_status(diagnostics.status_summary());
    }

    fn audio_report_snapshot(&self) -> audio::AudioReportSnapshot {
        let (health_lines, recent_events) =
            self.audio_diagnostics_sections(AUDIO_STATUS_EVENT_LIMIT);
        let input_device = self
            .capture
            .as_ref()
            .map(|capture| capture.device_info_live());
        let active_playback = self
            .playback
            .as_ref()
            .or(self.loopback_playback.as_ref())
            .or(self.notification_playback.as_ref());
        let output_device = active_playback.map(|playback| playback.device_info_live());
        let playback = active_playback
            .map(|playback| playback.stats())
            .unwrap_or_default();
        let audio_notice = AudioDiagnostics::new(
            playback.clone(),
            self.encoder_profile,
            self.voice_packets_received,
            self.voice_bytes_received,
            input_device.clone(),
            output_device.clone(),
            health_lines,
            recent_events,
        )
        .notice_body();
        audio::AudioReportSnapshot {
            audio_notice,
            input_device,
            output_device,
            capture: self
                .capture
                .as_ref()
                .map(|capture| capture.stats().snapshot()),
            playback,
        }
    }

    fn audio_report_settings_json(&self) -> String {
        let suppression = self.config.audio.suppression();
        let typing = self.config.audio.typing_suppression();
        let latency = self.config.audio.latency.to_tuning();
        let voice = self.local_voice_state();
        jsony::object! {
            bitrate_bps: self.config.audio.bitrate_bps,
            denoise_engine: self.config.audio.denoise.label(),
            dred_mode: self.config.audio.dred.label(),
            echo_cancellation: self.config.audio.echo_cancellation,
            max_amplification: self.config.audio.max_amplification,
            input_device_id: self.config.audio.input_device_id.as_deref(),
            output_device_id: self.config.audio.output_device_id.as_deref(),
            input_buffer_request: self.input_buffer_request().label(),
            output_buffer_request: self.output_buffer_request().label(),
            suppression: {
                strength: suppression.strength,
                release: suppression.release,
            },
            typing_suppression: {
                enabled: typing.enabled,
                vad_enter: typing.vad_enter,
                vad_release: typing.vad_release,
                release_confirm_ms: typing.release_confirm.as_millis() as u64,
            },
            latency_tuning: {
                capture_silence_gate: latency.capture_silence_gate,
                render_assist: latency.render_assist,
                neteq_start_delay_ms: latency.neteq_start_delay.as_millis() as u64,
                neteq_min_delay_ms: latency.neteq_min_delay.as_millis() as u64,
                neteq_base_minimum_delay_ms: latency.neteq_base_minimum_delay.as_millis() as u64,
                neteq_max_delay_ms: latency.neteq_max_delay.as_millis() as u64,
                hard_queue_bound_ms: latency.hard_queue_bound.as_millis() as u64,
                initial_buffer_ms: latency.initial_buffer.as_millis() as u64,
                max_reorder_delay_ms: latency.max_reorder_delay.as_millis() as u64,
                device_period_margin_ms: latency.device_period_margin.as_millis() as u64,
                silence_vad_max: latency.silence_vad_max,
                capture_long_silence_stop_ms: latency.capture_long_silence_stop.as_millis() as u64,
                capture_silence_preroll_ms: latency.capture_silence_preroll.as_millis() as u64,
                capture_silence_ramp_ms: latency.capture_silence_ramp.as_millis() as u64,
            },
            output_volume: self.config.audio.output_volume,
            muted: matches!(voice, VoiceState::Muted),
            deafened: matches!(voice, VoiceState::Deafened),
            encoder_profile: self.encoder_profile.label(),
        }
    }

    fn start_audio_report(
        &mut self,
        request: audio::AudioReportRequest,
        completion: Sender<Result<PathBuf, String>>,
    ) {
        if let Some(active) = self.active_audio_report.as_ref() {
            let _ = completion.send(Err(format!(
                "audio report already active: {}",
                active.path.display()
            )));
            return;
        }
        if self.audio_report.is_busy() {
            let path = self
                .audio_report
                .active_path()
                .unwrap_or_else(|| request.output.clone());
            let _ = completion.send(Err(format!(
                "audio report already active: {}",
                path.display()
            )));
            return;
        }
        let duration = Duration::from_millis(request.duration_ms);
        let path = request.output.clone();
        let start = audio::AudioReportStart {
            request,
            settings_json: self.audio_report_settings_json(),
            tuning: self.config.audio.latency.to_tuning(),
            snapshot: self.audio_report_snapshot(),
        };
        let diagnostics_logs_were_enabled = audio::AUDIO_DIAGNOSTICS_LOGS.is_enabled();
        audio::AUDIO_DIAGNOSTICS_LOGS.set(true);
        match self.audio_report.start(start) {
            Ok(()) => {
                self.active_audio_report = Some(ActiveAudioReport {
                    path,
                    deadline: Instant::now() + duration,
                    completion,
                    diagnostics_logs_were_enabled,
                });
            }
            Err(error) => {
                audio::AUDIO_DIAGNOSTICS_LOGS.set(diagnostics_logs_were_enabled);
                let _ = completion.send(Err(error));
            }
        }
    }

    fn finish_audio_report(&mut self, complete: bool) {
        let Some(active) = self.active_audio_report.take() else {
            return;
        };
        let finish = audio::AudioReportFinish {
            snapshot: self.audio_report_snapshot(),
            logs: crate::self_log::snapshot_plain_string(),
            complete,
        };
        audio::AUDIO_DIAGNOSTICS_LOGS.set(active.diagnostics_logs_were_enabled);
        self.audio_report.finish_to(finish, active.completion);
    }

    fn finish_audio_report_on_shutdown(&mut self) {
        let Some(active) = self.active_audio_report.take() else {
            return;
        };
        let finish = audio::AudioReportFinish {
            snapshot: self.audio_report_snapshot(),
            logs: crate::self_log::snapshot_plain_string(),
            complete: false,
        };
        audio::AUDIO_DIAGNOSTICS_LOGS.set(active.diagnostics_logs_were_enabled);
        let result = self
            .audio_report
            .finish(finish)
            .recv()
            .unwrap_or_else(|_| Err("audio report writer stopped".to_string()));
        let _ = active.completion.send(result);
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
        let (health_lines, recent_events) = self.audio_diagnostics_sections(usize::MAX);
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
                .map(|capture| capture.device_info_live()),
            self.playback
                .as_ref()
                .map(|playback| playback.device_info_live()),
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
            platform: {
                os: std::env::consts::OS,
                arch: std::env::consts::ARCH,
                release: platform_release(),
            },
            encoder_profile: self.encoder_profile.label(),
            voice_packets_received: self.voice_packets_received,
            voice_bytes_received: self.voice_bytes_received,
            audio: audio,
            device: {
                // The host is also in the `audio` device lines, but only while a
                // stream is open: a report filed for a stream that never started
                // would otherwise not say which backend it failed on.
                host: audio::default_host_name(),
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
                self.room.local_username, self.room.server_alias, user_id.0
            ),
            None => format!(
                "connecting as {} on {}",
                self.room.local_username, self.room.server_alias
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
        self.push_notice(
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
        if self.local_voice_state().is_deafened() {
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
        let network_events = self
            .events
            .sender()
            .for_network(self.active_network_generation.unwrap_or(0));
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
            source_state: LiveAudioSourceState::new(
                Arc::clone(&self.voice_state),
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
                        let _ = network_events.send(NetworkEvent::WorkerStopped {
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
            voice_state: Arc::clone(&self.voice_state),
            audio_report: Arc::clone(&self.audio_report),
        }
    }

    fn capture_packet_handler(&self) -> impl FnMut(LocalVoiceFrame) + Send + 'static {
        let tx = self.network.as_ref().map(|network| network.sender());
        // Failures are reported under the generation of the session the
        // capture was started for, so a preview capture or a stale worker's
        // report can never take down a newer session.
        let event_tx = self
            .events
            .sender()
            .for_network(self.active_network_generation.unwrap_or(0));
        let send_failed = Arc::new(AtomicBool::new(false));
        let voice_tx_enabled = Arc::clone(&self.voice_tx_enabled);
        let loopback_tap = self.loopback_tap.clone();
        // Mute and deafen are handled inside the capture pipeline (fade-out tail
        // plus silence markers), so this handler only gates the hard transport
        // on/off. Dropping muted frames here would look like packet loss to the
        // receiver's jitter buffer.
        move |payload| {
            // Loopback runs off the same captured frame, independent of the
            // transport gate, so it works outside a call while settings is open.
            loopback_tap.push_frame(&payload);
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
            if !self.room.audio_devices.input_devices.is_empty() {
                let input_items =
                    settings::audio_input_items(&self.room.audio_devices.input_devices);
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
            || self.local_voice_state().is_deafened()
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
        if self.local_voice_state().is_deafened() {
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
        let migrate_loopback_to_call =
            self.loopback_tap.is_active() && self.loopback_playback.is_some();
        let (feedback_tx, feedback_rx) = mpsc::channel::<LivePlaybackFeedback>();
        let Some(network) = &self.network else {
            self.set_error("select a server before starting playback");
            return;
        };
        let network_tx = network.sender();
        let event_tx = self
            .events
            .sender()
            .for_network(self.active_network_generation.unwrap_or(0));
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
                // The call stream takes over notification duty; never keep two
                // device streams open.
                self.drop_notification_playback();
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
                        "requested audio config unsupported; opened on a device fallback (see /audio)"
                            .to_string(),
                    );
                } else if capture_ok {
                    if self.config.soundboard.enabled {
                        self.set_status("soundboard voice active");
                    } else {
                        self.set_status("voice active");
                    }
                }
                if migrate_loopback_to_call {
                    self.restart_loopback_output();
                }
            }
            Err(error) => {
                kvlog::warn!(
                    "playback stream start failed",
                    device = resolved_output.as_deref().unwrap_or("<system default>"),
                    kind = error.kind.label(),
                    error = error.message.as_str()
                );
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

    /// Enables or disables the settings-only microphone loopback monitor.
    /// Loopback re-injects captured frames into the live playback pipeline on a
    /// reserved stream id, reusing the full decode/mixer/output path so the
    /// monitor sounds exactly like what peers hear. Idempotent; only meaningful
    /// while settings is open, and torn down by `finish_settings_session`.
    pub(crate) fn set_loopback_enabled(&mut self, enabled: bool) {
        if enabled && self.loopback_tap.is_active() {
            return;
        }
        if !enabled && !self.loopback_tap.is_active() && self.loopback_playback.is_none() {
            return;
        }
        if enabled {
            if let Err(error) = self.enable_loopback() {
                self.fail_loopback(error);
                return;
            }
            self.set_status("loopback active");
        } else {
            self.disable_loopback();
        }
    }

    fn enable_loopback(&mut self) -> Result<(), AudioStartError> {
        self.ensure_loopback_capture()?;
        // Reuse the in-call playback stream when present; otherwise stand up a
        // dedicated monitor stream so loopback works with no server or call.
        let sink = if self.playback.is_some() {
            self.loopback_playback = None;
            self.playback.as_ref().and_then(LivePlayback::sink)
        } else {
            self.loopback_playback = None;
            // The loopback stream takes over notification duty; a second
            // standalone stream on the same device would fight it.
            self.drop_notification_playback();
            let playback = self.start_standalone_playback()?;
            let sink = playback.sink();
            self.loopback_playback = Some(playback);
            sink
        };
        let Some(sink) = sink else {
            return Err(AudioStartError::transient(
                "playback stream has no sink".to_string(),
            ));
        };
        self.loopback_tap.install(sink);
        Ok(())
    }

    fn ensure_loopback_capture(&mut self) -> Result<(), AudioStartError> {
        if self.local_voice_state().is_deafened() {
            return Err(AudioStartError::new(
                AudioErrorKind::ConfigInvalid,
                "undeafen before using loopback",
            ));
        }
        if self.capture.is_none() {
            self.start_settings_preview_capture_inner()?;
        }
        if self.capture.is_some() {
            Ok(())
        } else {
            Err(AudioStartError::new(
                AudioErrorKind::ConfigInvalid,
                "microphone capture is unavailable for loopback",
            ))
        }
    }

    fn loopback_uses_dedicated_playback(&self) -> bool {
        self.loopback_tap.is_active() && self.loopback_playback.is_some() && self.playback.is_none()
    }

    fn restart_loopback_output(&mut self) {
        self.loopback_tap.clear();
        self.loopback_playback = None;
        if let Err(error) = self.enable_loopback() {
            self.fail_loopback(error);
        }
    }

    fn fail_loopback(&mut self, error: AudioStartError) {
        self.loopback_tap.clear();
        self.loopback_playback = None;
        self.set_error(format!("loopback unavailable: {error}"));
    }

    /// Starts a standalone playback stream outside a call (loopback monitor,
    /// out-of-call notifications), mirroring the configured-then-default output
    /// fallback used by `start_playback_stream`.
    fn start_standalone_playback(&self) -> Result<LivePlayback, AudioStartError> {
        let configured_output = self.config.audio.output_device_id.clone();
        let resolved_output = configured_output
            .as_deref()
            .filter(|id| !audio::configured_output_is_default(id))
            .map(|id| id.to_string());
        match audio::start_live_playback(self.live_playback_config(resolved_output.clone(), None)) {
            Ok(playback) => Ok(playback),
            Err(error) if resolved_output.is_some() => {
                kvlog::warn!(
                    "standalone output failed, trying default",
                    error = error.message.as_str()
                );
                audio::start_live_playback(self.live_playback_config(None, None))
            }
            Err(error) => Err(error),
        }
    }

    fn disable_loopback(&mut self) {
        self.loopback_tap.clear();
        if self.loopback_playback.take().is_none() {
            // Loopback rode the live call playback; tear down just its stream,
            // leaving the call audio intact.
            if let Some(playback) = &self.playback {
                playback.stop_stream(LOOPBACK_STREAM_ID);
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
            output_volume_percent: Arc::clone(&self.output_volume_percent_bits),
            audio_report: Arc::clone(&self.audio_report),
        }
    }

    /// Mixes a notification sound into the live output, honoring the configured
    /// [`NotificationSoundMode`]. In-call sounds ride the call playback stream;
    /// with `Always` and no call, the clip goes to the loopback monitor stream
    /// when one is live, otherwise to a lazily started standalone stream that
    /// the tick supervisor tears down after an idle linger. Deafen always
    /// suppresses sounds.
    fn play_notification(&mut self, sound: NotificationSound) {
        if self.local_voice_state().is_deafened() {
            return;
        }
        let mode = self.config.notifications.sounds;
        if mode == NotificationSoundMode::Never {
            return;
        }
        if let Some(playback) = &self.playback {
            playback.play_notification(self.notification_clip(sound));
            return;
        }
        if mode != NotificationSoundMode::Always {
            return;
        }
        if let Some(playback) = &self.loopback_playback {
            playback.play_notification(self.notification_clip(sound));
            return;
        }
        if !self.ensure_notification_playback() {
            return;
        }
        let samples = self.notification_clip(sound);
        let deadline = notification_idle_deadline(Instant::now(), samples.len());
        let Some(playback) = &self.notification_playback else {
            return;
        };
        playback.play_notification(samples);
        self.notification_playback_idle_at = Some(match self.notification_playback_idle_at {
            Some(existing) => existing.max(deadline),
            None => deadline,
        });
    }

    /// The decoded clip for `sound` with the configured per-sound gain applied.
    fn notification_clip(&self, sound: NotificationSound) -> Arc<[f32]> {
        let volume_db = self.config.notifications.volume_db(sound);
        let samples = audio::sound_samples(sound);
        if volume_db == 0.0 {
            return samples;
        }
        let gain = 10.0_f32.powf(volume_db / 20.0);
        samples
            .iter()
            .map(|sample| sample * gain)
            .collect::<Vec<_>>()
            .into()
    }

    /// Ensures the lazy notification playback stream is running, respecting the
    /// failure cooldown. Returns whether a stream is available.
    fn ensure_notification_playback(&mut self) -> bool {
        if self.notification_playback.is_some() {
            return true;
        }
        let now = Instant::now();
        if self
            .notification_playback_retry_at
            .is_some_and(|at| now < at)
        {
            return false;
        }
        match self.start_standalone_playback() {
            Ok(playback) => {
                self.notification_playback = Some(playback);
                self.notification_playback_retry_at = None;
                true
            }
            Err(error) => {
                kvlog::warn!(
                    "notification playback start failed",
                    error = error.message.as_str()
                );
                self.notification_playback_retry_at = Some(now + NOTIFICATION_START_RETRY);
                false
            }
        }
    }

    fn drop_notification_playback(&mut self) {
        self.notification_playback = None;
        self.notification_playback_idle_at = None;
    }

    fn stop_audio(&mut self) {
        let restart_settings_preview =
            self.settings_preview_capture && !self.local_voice_state().is_deafened();
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
            .to_request(config::DEFAULT_INPUT_TARGET_LATENCY)
    }

    fn output_buffer_request(&self) -> BufferRequest {
        self.config
            .audio
            .output_buffer
            .to_request(config::DEFAULT_OUTPUT_TARGET_LATENCY)
    }

    pub(crate) fn set_status(&mut self, status: impl Into<String>) {
        let status = status.into();
        self.capture_frontend_command_line(false, &status);
        self.send_to(self.command_client, TerminalEvent::Status(status));
    }

    pub(crate) fn set_transient_status(&mut self, status: impl Into<String>) {
        let status = status.into();
        self.send_to(self.command_client, TerminalEvent::TransientStatus(status));
    }

    pub(crate) fn set_error(&mut self, status: impl Into<String>) {
        let status = status.into();
        self.capture_frontend_command_line(true, &status);
        self.send_to(self.command_client, TerminalEvent::Error(status));
    }

    fn capture_frontend_command_line(&mut self, error: bool, text: &str) {
        if let Some(capture) = &mut self.frontend_command_capture
            && !text.is_empty()
            && capture.len() < local_rpc::MAX_COMMAND_OUTPUT_LINES
        {
            capture.push(local_rpc::model::CommandOutputLine {
                error,
                text: text.to_string(),
            });
        }
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

fn screencast_phase_label(phase: ScreencastPhase) -> &'static str {
    match phase {
        ScreencastPhase::Idle => "idle",
        ScreencastPhase::Off => "off",
        ScreencastPhase::Starting => "starting",
        ScreencastPhase::Live => "live",
        ScreencastPhase::Failed => "failed",
    }
}

fn video_rate_label(bytes_per_sec: u64) -> String {
    format!("{}/s", crate::client_net::format_bytes(bytes_per_sec))
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[allow(dead_code, reason = "used in debug only logging")]
fn network_event_kind(event: &NetworkEvent) -> &'static str {
    match event {
        NetworkEvent::Connected => "connected",
        NetworkEvent::Authenticated { .. } => "authenticated",
        NetworkEvent::RoomUpserted(_) => "room_upserted",
        NetworkEvent::DmOpened { .. } => "dm_opened",
        NetworkEvent::HistoryChunk { .. } => "history_chunk",
        NetworkEvent::Chat(_) => "chat",
        NetworkEvent::ChatMutationRejected { .. } => "chat_mutation_rejected",
        NetworkEvent::FileReceived { .. } => "file_received",
        NetworkEvent::TransferProgress { .. } => "transfer_progress",
        NetworkEvent::TransferEnded { .. } => "transfer_ended",
        NetworkEvent::TransferComplete { .. } => "transfer_complete",
        NetworkEvent::Presence { .. } => "presence",
        NetworkEvent::MlsAccountIdentity { .. } => "mls_account_identity",
        NetworkEvent::MlsDeviceBound { .. } => "mls_device_bound",
        NetworkEvent::DeviceLinkCreated { .. } => "device_link_created",
        NetworkEvent::DeviceLinkRedeemed { .. } => "device_link_redeemed",
        NetworkEvent::DeviceLinkCanceled => "device_link_canceled",
        NetworkEvent::E2ePeerPinProposed { .. } => "e2e_peer_pin_proposed",
        NetworkEvent::E2ePeerPinMatched { .. } => "e2e_peer_pin_matched",
        NetworkEvent::VoiceStarted { .. } => "voice_started",
        NetworkEvent::VoiceStopped { .. } => "voice_stopped",
        NetworkEvent::PeerTransport { .. } => "peer_transport",
        NetworkEvent::VoicePacketObserved { .. } => "voice_packet_observed",
        NetworkEvent::PlaybackFeedback(_) => "playback_feedback",
        NetworkEvent::OutboundFeedback { .. } => "outbound_feedback",
        NetworkEvent::ServerRtt { .. } => "server_rtt",
        NetworkEvent::PeerRtt { .. } => "peer_rtt",
        NetworkEvent::VoiceStateChanged { .. } => "voice_state",
        NetworkEvent::VoiceJoinFailed { .. } => "voice_join_failed",
        NetworkEvent::EncoderProfileChanged(_) => "encoder_profile_changed",
        NetworkEvent::Status(_) => "status",
        NetworkEvent::Error(_) => "error",
        NetworkEvent::AuthFailed { .. } => "auth_failed",
        NetworkEvent::TransportEncryptionRequired => "transport_encryption_required",
        NetworkEvent::MediaTransport { .. } => "media_transport",
        NetworkEvent::ReconnectScheduled { .. } => "reconnect_scheduled",
        NetworkEvent::LocalIdentityUnavailable { .. } => "local_identity_unavailable",
        NetworkEvent::WorkerStopped { .. } => "worker_stopped",
        NetworkEvent::ShareStarted { .. } => "share_started",
        NetworkEvent::ShareAvailable { .. } => "share_available",
        NetworkEvent::ShareEnded { .. } => "share_ended",
        NetworkEvent::ShareStartRejected { .. } => "share_start_rejected",
        NetworkEvent::Mls(_) => "mls",
    }
}

fn app_network_command_kind(command: &NetworkCommand) -> &'static str {
    match command {
        NetworkCommand::SendChat { .. } => "send_chat",
        NetworkCommand::EditChat { .. } => "edit_chat",
        NetworkCommand::DeleteChat { .. } => "delete_chat",
        NetworkCommand::UploadFile { .. } => "upload_file",
        NetworkCommand::CancelTransfer { .. } => "cancel_transfer",
        NetworkCommand::SetActiveRoom(_) => "set_active_room",
        NetworkCommand::JoinVoice(_) => "join_voice",
        NetworkCommand::LeaveVoice => "leave_voice",
        NetworkCommand::FetchHistory { .. } => "fetch_history",
        NetworkCommand::OpenDm(_) => "open_dm",
        NetworkCommand::LocalVoicePacket(_) => "local_voice_packet",
        NetworkCommand::SequencedLocalVoicePacket { .. } => "sequenced_local_voice_packet",
        NetworkCommand::SetPlaybackSink(_) => "set_playback_sink",
        NetworkCommand::PlaybackFeedback(_) => "playback_feedback",
        NetworkCommand::SetVoiceState(_) => "set_voice_state",
        NetworkCommand::StartShare { .. } => "start_share",
        NetworkCommand::StopShare { .. } => "stop_share",
        NetworkCommand::ReportBug { .. } => "report_bug",
        NetworkCommand::SetUploadRate(_) => "set_upload_rate",
        NetworkCommand::SetFilePolicy(_) => "set_file_policy",
        NetworkCommand::SetP2pEnabled(_) => "set_p2p_enabled",
        NetworkCommand::ReviewPeerIdentity { .. } => "review_peer_identity",
        NetworkCommand::VerifyPeerIdentity { .. } => "verify_peer_identity",
        NetworkCommand::ForgetPeerIdentity { .. } => "forget_peer_identity",
        NetworkCommand::ConfirmE2ePeerPin { .. } => "confirm_e2e_peer_pin",
        NetworkCommand::AcknowledgeMlsUiDispatch { .. } => "acknowledge_mls_ui_dispatch",
        NetworkCommand::RevokeE2eDevice { .. } => "revoke_e2e_device",
        NetworkCommand::ListE2eDevices => "list_e2e_devices",
        NetworkCommand::CreateDeviceLink => "create_device_link",
        NetworkCommand::CancelDeviceLink { .. } => "cancel_device_link",
        #[cfg(test)]
        NetworkCommand::RetryConnection => "retry_connection",
        NetworkCommand::Shutdown => "shutdown",
    }
}

#[cfg(test)]
mod tests {
    use super::command::CoreCommand;
    use super::testing::TestApp;
    use super::*;
    use crate::client_channel::TransportWarningTarget;
    use crate::{bindings::BindCommand, tui::Action};
    use crate::{
        settings::SettingsDraft,
        tui::{
            form::FormState,
            mode::AppMode,
            modes::{RoomMode, ServerListMode, SettingsMode},
        },
    };
    use extui::{
        Buffer, Rect, Style,
        event::{KeyModifiers, MouseButton},
    };
    use extui_editor::Mode as EditorMode;
    use rpc::control::ERROR_USERNAME_TAKEN;
    use rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX;

    fn test_app() -> TestApp {
        TestApp::new(Config::default(), None).expect("test app")
    }

    #[test]
    fn media_transport_events_project_all_app_states() {
        let mut app = test_app();
        assert_eq!(app.room.media_transport, MediaTransportState::Udp);

        for state in [
            MediaTransportState::Unavailable,
            MediaTransportState::Tcp,
            MediaTransportState::Udp,
        ] {
            app.handle_network_event(NetworkEvent::MediaTransport { state });
            assert_eq!(app.room.media_transport, state);
        }
    }

    #[test]
    fn bug_report_metadata_identifies_the_platform_and_audio_host() {
        // A stream that never started leaves `devices` reading `inactive`, so
        // these fields are the only record of where the report came from.
        let app = test_app();
        let metadata = app.bug_report_metadata("no sound after switching headphones");
        assert!(
            metadata.contains(&format!("\"os\":\"{}\"", std::env::consts::OS)),
            "{metadata}"
        );
        assert!(
            metadata.contains(&format!("\"arch\":\"{}\"", std::env::consts::ARCH)),
            "{metadata}"
        );
        assert!(metadata.contains("\"host\":\""), "{metadata}");
        assert!(metadata.contains("output: inactive"), "{metadata}");
    }

    #[test]
    fn audio_report_rejects_concurrent_start_and_deadline_completes_it() {
        let mut app = test_app();
        app.config.audio.echo_cancellation = true;
        app.config.audio.input_device_id = Some("input-for-report".to_string());
        app.config.audio.output_device_id = Some("output-for-report".to_string());
        app.config.audio.input_buffer = config::BufferSize::Samples(240);
        app.config.audio.output_buffer = config::BufferSize::Samples(960);
        let parent = tempfile::tempdir().unwrap();
        let output = parent.path().join("active-report");
        let (completion, result) = mpsc::channel();
        let diagnostics_logs_were_enabled = audio::AUDIO_DIAGNOSTICS_LOGS.is_enabled();
        app.handle_app_event(AppEvent::AudioReport {
            request: audio::AudioReportRequest {
                output: output.clone(),
                duration_ms: 1_000,
                label: Some("deadline test".to_string()),
            },
            completion,
        });
        assert!(app.active_audio_report.is_some());
        assert!(audio::AUDIO_DIAGNOSTICS_LOGS.is_enabled());

        let (second_completion, second_result) = mpsc::channel();
        app.handle_app_event(AppEvent::AudioReport {
            request: audio::AudioReportRequest {
                output: parent.path().join("other-report"),
                duration_ms: 1_000,
                label: None,
            },
            completion: second_completion,
        });
        let error = second_result.recv().unwrap().unwrap_err();
        assert_eq!(
            error,
            format!("audio report already active: {}", output.display())
        );

        app.active_audio_report.as_mut().unwrap().deadline = Instant::now();
        app.tick();
        let completed = result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(completed, output);
        assert_eq!(
            audio::AUDIO_DIAGNOSTICS_LOGS.is_enabled(),
            diagnostics_logs_were_enabled
        );
        let manifest = std::fs::read_to_string(output.join("manifest.json")).unwrap();
        assert!(manifest.contains("\"complete\":true"), "{manifest}");
        for setting in [
            "\"echo_cancellation\":true",
            "\"input_device_id\":\"input-for-report\"",
            "\"output_device_id\":\"output-for-report\"",
            "\"input_buffer_request\":\"240 frames\"",
            "\"output_buffer_request\":\"960 frames\"",
        ] {
            assert!(manifest.contains(setting), "missing {setting}: {manifest}");
        }
    }

    #[test]
    fn bug_report_ships_events_the_audio_notice_truncates_away() {
        // The transition that started a failure ages past the interactive list
        // long before a user gets around to filing, so the report must not
        // inherit that cut.
        let mut app = test_app();
        let now = Instant::now();
        app.audio_events.push(
            now,
            AudioDeviceEventKind::DefaultOutputChanged,
            "headphones → airpods",
        );
        for index in 0..AUDIO_STATUS_EVENT_LIMIT {
            app.audio_events.push(
                now,
                AudioDeviceEventKind::StreamError,
                format!("spk rebuild failed: attempt {index}"),
            );
        }

        let (_, shown) = app.audio_diagnostics_sections(AUDIO_STATUS_EVENT_LIMIT);
        assert_eq!(shown.len(), AUDIO_STATUS_EVENT_LIMIT);
        assert!(
            !shown.iter().any(|event| event.contains("airpods")),
            "the originating transition should have been truncated away"
        );

        assert!(
            app.bug_report_metadata("no sound").contains("airpods"),
            "the report keeps the originating transition"
        );
    }

    fn open_test_input_picker(session: &mut SettingsSession) {
        session.input_items = ["System default", "USB Mic", "Line In"]
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
        let input_selection = session.draft.input_selection().map(ToOwned::to_owned);
        let input_items = session.input_items.clone();
        session
            .input_picker
            .open(&input_items, input_selection.as_deref());
    }

    fn user_summary(user_id: UserId, username: &str) -> rpc::control::UserSummary {
        rpc::control::UserSummary {
            user_id,
            username: username.to_string(),
            online: true,
            connected_at_ms: 0,
            voice_state: VoiceState::default(),
        }
    }

    #[test]
    fn posted_statuses_expire_lazily() {
        let mut status = StatusState::new("idle");

        assert!(!status.expire(Instant::now() + STATUS_LIFETIME));
        assert_eq!(status.text(), "idle", "the baseline status is persistent");

        status.set("updated");
        assert!(!status.expire(Instant::now()));
        assert_eq!(status.text(), "updated");
        assert!(status.expire(Instant::now() + STATUS_LIFETIME));
        assert_eq!(status.text(), "");

        status.set_error("failed");
        assert_eq!(status.kind(), StatusKind::Error);
        assert!(status.expire(Instant::now() + STATUS_LIFETIME));
        assert_eq!(status.text(), "");
    }

    #[test]
    fn tick_reports_only_render_visible_changes() {
        let mut app = test_app();
        assert_eq!(
            app.tick(),
            DirtySections::EMPTY,
            "an idle tick must not wake render threads"
        );

        app.view.status.set("done");
        app.view.status.expires_at = Some(Instant::now());
        assert_eq!(
            app.tick(),
            DirtySections::COMPOSE_BAR,
            "status expiration renders in the compose bar"
        );
        assert_eq!(app.view.status.text(), "");
        assert_eq!(
            app.tick(),
            DirtySections::EMPTY,
            "the expiration edge is reported only once"
        );

        app.room.capture_health.state = AudioHealthState::WaitingForDevice;
        assert_eq!(
            app.tick(),
            DirtySections::TOP_BAR | DirtySections::LOBBY_BAR | DirtySections::COMPOSE_BAR,
            "projection changes are render-visible"
        );
        assert_eq!(app.room.capture_health.state, AudioHealthState::Healthy);
        assert_eq!(
            app.tick(),
            DirtySections::EMPTY,
            "a stable projection must not cause another wake"
        );
    }

    #[test]
    fn notification_suppressed_while_deafened() {
        let mut app = test_app();
        app.config.notifications.sounds = NotificationSoundMode::Always;
        app.voice_state
            .store(VoiceState::Deafened, Ordering::Relaxed);

        app.play_notification(NotificationSound::MessageReceived);

        assert!(app.notification_playback.is_none());
        assert!(app.notification_playback_retry_at.is_none());
    }

    #[test]
    fn notification_out_of_call_needs_always_mode() {
        for mode in [NotificationSoundMode::Never, NotificationSoundMode::InCalls] {
            let mut app = test_app();
            app.config.notifications.sounds = mode;

            app.play_notification(NotificationSound::MessageReceived);

            assert!(app.notification_playback.is_none(), "{mode:?}");
            assert!(app.notification_playback_retry_at.is_none(), "{mode:?}");
        }
    }

    #[test]
    fn notification_retry_cooldown_blocks_lazy_start() {
        let mut app = test_app();
        app.config.notifications.sounds = NotificationSoundMode::Always;
        let retry_at = Instant::now() + NOTIFICATION_START_RETRY;
        app.notification_playback_retry_at = Some(retry_at);

        app.play_notification(NotificationSound::MessageReceived);

        assert!(app.notification_playback.is_none());
        assert_eq!(app.notification_playback_retry_at, Some(retry_at));
    }

    #[test]
    fn notification_idle_deadline_covers_clip_and_linger() {
        let now = Instant::now();
        let deadline = notification_idle_deadline(now, 48_000);
        assert_eq!(
            deadline - now,
            Duration::from_secs(1) + NOTIFICATION_STREAM_LINGER
        );
    }

    #[test]
    fn idle_deadline_teardown_clears_state() {
        let mut app = test_app();
        app.notification_playback_idle_at = Some(Instant::now());

        app.tick();

        assert!(app.notification_playback.is_none());
        assert!(app.notification_playback_idle_at.is_none());
    }

    /// A pending candidate join for `label`, as `start_join` leaves it. The
    /// worker dials a closed port, so nothing arrives unless the test injects
    /// events under [`join_generation`].
    fn pending_join(app: &mut TestApp, label: &str, owner: JoinOwner) -> u64 {
        let server_id = app.config.server(label).expect("configured server").id;
        let JoinStart::Started(view) = app.start_join(server_id, owner) else {
            panic!("join did not start");
        };
        view.attempt_id
    }

    fn join_generation(app: &TestApp) -> u64 {
        app.join_attempt_generation().expect("pending join")
    }

    #[test]
    fn transport_encryption_rejection_prompts_only_the_join_owner() {
        let mut app = test_app();
        app.config.servers.push(saved_server("legacy", "token"));
        let channel = app.terminal_channel();
        let attempt_id = pending_join(
            &mut app,
            "legacy",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        channel.drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::TransportEncryptionRequired,
        });

        let events = channel.drain_events();
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::Navigation(NavigationEvent::ShowOverlay(overlay))
                if matches!(
                    overlay.as_ref(),
                    OverlaySpec::TransportEncryptionWarning {
                        label,
                        target: TransportWarningTarget::Join { attempt_id: prompted },
                    } if label == "legacy" && *prompted == attempt_id
                )
        )));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                TerminalEvent::Navigation(NavigationEvent::ResetBase(_))
            )),
            "a pending join prompts its owner without navigating anyone"
        );
    }

    #[test]
    fn transport_encryption_rejection_projects_prompt_to_rpc_owner() {
        let mut app = test_app();
        app.config.servers.push(saved_server("legacy", "token"));
        let owner = crate::client_channel::ClientId(7);
        app.register_rpc_client(owner);
        let attempt_id = pending_join(&mut app, "legacy", JoinOwner::Rpc(owner));
        let generation = join_generation(&app);

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::TransportEncryptionRequired,
        });

        assert!(matches!(
            app.rpc_snapshot(owner).server_selection.prompt,
            Some(local_rpc::model::ServerSelectionPrompt::AllowUnencryptedTransport {
                label,
                attempt_id: prompted,
            }) if label == "legacy" && prompted == attempt_id
        ));
    }

    #[test]
    fn stale_connection_generation_cannot_publish_navigation() {
        let mut app = test_app();
        app.config.servers.push(saved_server("new", "token"));
        let channel = app.terminal_channel();
        pending_join(
            &mut app,
            "new",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        channel.drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation: generation.wrapping_add(1),
            event: NetworkEvent::TransportEncryptionRequired,
        });

        assert!(channel.drain_events().is_empty());
        assert_eq!(join_generation(&app), generation);
    }

    fn attach_test_client(
        app: &mut TestApp,
        id: crate::client_channel::ClientId,
    ) -> Arc<parking_lot::Mutex<ClientView>> {
        let channel = Arc::new(crate::client_channel::ClientChannel::new().expect("test channel"));
        app.attach_client(id, channel)
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

    #[test]
    fn output_volume_command_updates_live_config_and_atomic() {
        let mut app = test_app();

        let (reply, rx) = mpsc::channel();
        app.handle_output_volume_command(local_control::OutputVolumeCommand::Set(50.0), reply);
        assert_eq!(rx.recv().unwrap().unwrap(), 50.0);
        assert_eq!(app.config.audio.output_volume, 50.0);
        assert_eq!(
            f32::from_bits(app.output_volume_percent_bits.load(Ordering::Relaxed)),
            50.0
        );

        let (reply, rx) = mpsc::channel();
        app.handle_output_volume_command(local_control::OutputVolumeCommand::Adjust(200.0), reply);
        assert_eq!(
            rx.recv().unwrap().unwrap(),
            config::MAX_OUTPUT_VOLUME_PERCENT
        );
        assert_eq!(
            app.config.audio.output_volume,
            config::MAX_OUTPUT_VOLUME_PERCENT
        );

        let (reply, rx) = mpsc::channel();
        app.handle_output_volume_command(local_control::OutputVolumeCommand::Query, reply);
        assert_eq!(
            rx.recv().unwrap().unwrap(),
            config::MAX_OUTPUT_VOLUME_PERCENT
        );
    }

    #[test]
    fn control_upload_replies_cleanly_while_offline() {
        let mut app = test_app();
        let (reply, response) = mpsc::channel();

        app.handle_app_event(AppEvent::Upload {
            request: UploadFileRequest::new("/tmp/offline.txt".into()),
            room: None,
            reply,
        });

        assert_eq!(
            response.recv().unwrap().unwrap_err(),
            "not connected to a server"
        );
    }

    #[test]
    fn control_upload_routes_through_app_network_sender() {
        let mut app = test_app();
        let (network_tx, network_rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(network_tx));
        app.room.network_selected = true;
        let path = std::path::PathBuf::from("/tmp/online.txt");
        let (reply, response) = mpsc::channel();

        app.handle_app_event(AppEvent::Upload {
            request: UploadFileRequest::new(path.clone()),
            room: None,
            reply,
        });

        assert_eq!(
            response.recv().unwrap().unwrap(),
            format!("queued upload {}", path.display())
        );
        assert!(matches!(
            network_rx.recv().unwrap(),
            NetworkCommand::UploadFile { request, .. } if request.path == path
        ));
    }

    #[test]
    fn control_send_uses_viewed_room() {
        let mut app = test_app();
        let (network_tx, network_rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(network_tx));
        app.room.network_selected = true;
        enter_test_room(&mut app);
        let (reply, response) = mpsc::channel();

        app.handle_app_event(AppEvent::SendMessage {
            body: "hello".to_string(),
            room: None,
            reply,
        });

        assert_eq!(
            response.recv().unwrap().unwrap(),
            "queued message to room-1"
        );
        assert!(matches!(
            network_rx.recv().unwrap(),
            NetworkCommand::SendChat { room_id: RoomId(1), body } if body == "hello"
        ));
    }

    #[test]
    fn control_send_uses_chat_at_cap_and_inline_markdown_upload_above_it() {
        let mut app = test_app();
        let (network_tx, network_rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(network_tx));
        app.room.network_selected = true;
        enter_test_room(&mut app);

        let at_cap = "x".repeat(rpc::control::MAX_CHAT_BODY_BYTES);
        let (send_reply, send_response) = mpsc::channel();
        app.handle_app_event(AppEvent::SendMessage {
            body: at_cap.clone(),
            room: None,
            reply: send_reply,
        });
        assert_eq!(
            send_response.recv().unwrap().unwrap(),
            "queued message to room-1"
        );
        assert!(matches!(
            network_rx.recv().unwrap(),
            NetworkCommand::SendChat { room_id: RoomId(1), body } if body == at_cap
        ));

        let over_cap = "y".repeat(rpc::control::MAX_CHAT_BODY_BYTES + 1);
        let (upload_reply, upload_response) = mpsc::channel();
        app.handle_app_event(AppEvent::SendMessage {
            body: over_cap.clone(),
            room: None,
            reply: upload_reply,
        });
        let status = upload_response.recv().unwrap().unwrap();
        assert!(status.starts_with("queued upload message-"), "{status}");
        assert!(status.ends_with("Z.md"), "{status}");
        assert!(matches!(
            network_rx.recv().unwrap(),
            NetworkCommand::UploadFile {
                room_id: Some(RoomId(1)),
                request,
            } if request.path.as_os_str().is_empty()
                && request.name_override.as_deref()
                    == Some(status.trim_start_matches("queued upload "))
                && request.inline_bytes.as_deref() == Some(over_cap.as_bytes())
        ));
    }

    #[test]
    fn control_send_and_upload_resolve_explicit_room_without_switching_view() {
        let mut app = test_app();
        let (network_tx, network_rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(network_tx));
        app.room.network_selected = true;
        app.room.authenticated(
            &[test_room_info(1), test_room_info(20)],
            Vec::new(),
            RoomId(1),
            Some(RoomId(1)),
            None,
        );

        let (send_reply, send_response) = mpsc::channel();
        app.handle_app_event(AppEvent::SendMessage {
            body: "hello elsewhere".to_string(),
            room: Some("id:20".to_string()),
            reply: send_reply,
        });
        assert_eq!(
            send_response.recv().unwrap().unwrap(),
            "queued message to room-20"
        );
        assert!(matches!(
            network_rx.recv().unwrap(),
            NetworkCommand::SendChat { room_id: RoomId(20), body }
                if body == "hello elsewhere"
        ));

        let path = std::path::PathBuf::from("/tmp/elsewhere.txt");
        let (upload_reply, upload_response) = mpsc::channel();
        app.handle_app_event(AppEvent::Upload {
            request: UploadFileRequest::new(path.clone()),
            room: Some("room-20".to_string()),
            reply: upload_reply,
        });
        assert_eq!(
            upload_response.recv().unwrap().unwrap(),
            format!("queued upload {}", path.display())
        );
        assert!(matches!(
            network_rx.recv().unwrap(),
            NetworkCommand::UploadFile { room_id: Some(RoomId(20)), request }
                if request.path == path
        ));
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
    }

    #[test]
    fn config_path_command_reports_running_config_path() {
        let mut app = test_app();
        let path = std::env::temp_dir().join("chatt-config-path-command.toml");
        app.config.config_path = Some(path.clone());

        let (reply, rx) = mpsc::channel();
        app.handle_config_path(reply);

        assert_eq!(rx.recv().unwrap().unwrap(), path.display().to_string());
    }

    fn render_room(app: &mut TestApp, room: &mut RoomMode, buffer: &mut Buffer) {
        // The runtime ticks before every paint; reproduce the projection so
        // renders see fresh session display facts.
        app.refresh_session_projection();
        room.render(app, buffer, 0);
    }

    fn cell_style(buffer: &mut Buffer, column: u16, row: u16) -> Style {
        let grid = buffer.current();
        grid.cells()[(row as usize * grid.width() as usize) + column as usize].style()
    }

    fn cell_text(buffer: &mut Buffer, column: u16, row: u16) -> String {
        let grid = buffer.current();
        let cell = grid.cells()[(row as usize * grid.width() as usize) + column as usize];
        if cell.is_handle() {
            String::from_utf8_lossy(grid.handle_text(cell).unwrap_or_default()).to_string()
        } else {
            cell.text_inline().unwrap_or_default().to_string()
        }
    }

    fn rect_text(buffer: &mut Buffer, rect: Rect) -> String {
        (0..rect.w)
            .map(|column| cell_text(buffer, rect.x + column, rect.y))
            .collect::<String>()
    }

    fn base_mode_label(app: &mut TestApp) -> &'static str {
        let mode = app.base_mode();
        let cx = app.view_cx();
        mode.presentation(&cx)
            .chrome
            .expect("base mode has chrome")
            .status_label
    }

    #[test]
    fn base_mode_stays_in_room_while_a_server_is_selected() {
        let mut app = test_app();
        // No server selected and no network: the server picker is the base.
        assert_eq!(base_mode_label(&mut app), "Servers");

        // A selected server (kept across a disconnect) holds the room view so
        // its offline logs stay readable.
        app.room.server_alias = "lab".to_string();
        assert_eq!(base_mode_label(&mut app), "Compose");
    }

    #[test]
    fn auth_failure_shows_detail_after_returning_to_server_list() {
        let mut app = test_app();
        app.room.server_alias = "public".to_string();
        let mut h = Harness::new(app);
        let message = "authentication failed: invalid public bearer token";

        h.app.handle_network_event(NetworkEvent::AuthFailed {
            code: 1,
            message: message.to_string(),
        });
        h.apply();

        assert_eq!(h.top_theme_mode(), crate::theme::UiMode::ServerSelect);
        assert_eq!(h.app.view.status.text(), message);
        assert_eq!(h.app.view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn remote_auth_username_collision_opens_editor_on_connection_owner() {
        let mut app = test_app();
        let owner = crate::client_channel::ClientId(6);
        let view = attach_test_client(&mut app, owner);
        let channel = app.channel_for(owner).expect("attached channel");
        app.config.servers.push(ServerEntry {
            username: "Zoe".to_string(),
            ..saved_server("public", "public-token")
        });
        pending_join(&mut app, "public", JoinOwner::Terminal(owner));
        let generation = join_generation(&app);
        channel.drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::AuthFailed {
                code: ERROR_USERNAME_TAKEN,
                message: "username already in use".to_string(),
            },
        });

        assert!(channel.drain_events().into_iter().any(|event| matches!(
            event,
            TerminalEvent::Navigation(NavigationEvent::ReplaceScreen(screen))
                if matches!(screen.as_ref(), ScreenSpec::ServerEditor(_))
        )));
        assert_eq!(
            view.lock().status.text(),
            "username already in use; choose another"
        );
        assert_ne!(
            app.view.status.text(),
            "username already in use; choose another"
        );
        assert!(!app.has_pending_join(), "the failed candidate is gone");
    }

    fn test_room_info(id: u32) -> rpc::control::RoomInfo {
        rpc::control::RoomInfo {
            room_id: rpc::ids::RoomId(id),
            name: format!("room-{id}"),
            kind: rpc::control::RoomKind::Public,
            head: None,
            voice_users: Vec::new(),
        }
    }

    fn dm_room_info(id: u32, user_a: UserId, user_b: UserId) -> rpc::control::RoomInfo {
        rpc::control::RoomInfo {
            room_id: rpc::ids::RoomId(id),
            name: format!("dm:{}:{}", user_a.0, user_b.0),
            kind: rpc::control::RoomKind::Dm { user_a, user_b },
            head: None,
            voice_users: Vec::new(),
        }
    }

    fn test_chat_record(room_id: RoomId, message_id: MessageId) -> crate::e2e::AuthenticatedChat {
        crate::e2e::AuthenticatedChat {
            message: rpc::control::ChatMessage {
                message_id,
                room_id,
                sender: UserId(2),
                sender_name: "bob".to_string(),
                timestamp_ms: message_id.0 * 1_000,
                body: "late message".to_string(),
                file_transfer_id: None,
                flags: rpc::control::MessageFlags::default(),
                target: None,
            },
            provenance: None,
        }
    }

    /// Registers room 1 as the viewed room with `users` in the directory.
    fn enter_room_with_users(app: &mut TestApp, users: Vec<rpc::control::UserSummary>) {
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1)],
            users,
            rpc::ids::RoomId(1),
            None,
            user_id,
        );
        let (core, view) = app.parts_mut();
        view.switch_room(rpc::ids::RoomId(1), &core.room);
    }

    fn observe_room_voice(app: &mut TestApp, user_id: UserId, stream_id: u32) {
        let session_id = app.session_id;
        app.room.voice_started(
            RoomId(1),
            SessionId(user_id.0),
            user_id,
            StreamId(stream_id),
            session_id,
            Some(RoomId(1)),
        );
    }

    fn enter_test_room(app: &mut TestApp) {
        enter_room_with_users(app, Vec::new());
    }

    /// Drives an [`App`] through a real mode stack so tests can exercise mode
    /// transitions (push/pop of overlays) the same way the runtime loop does.
    struct Harness {
        app: TestApp,
        stack: crate::tui::mode_stack::ModeStack,
    }

    impl Harness {
        fn new(mut app: TestApp) -> Self {
            let base: Box<dyn AppMode> = if app.room.server_alias.is_empty() {
                app.base_mode()
            } else {
                Box::new(RoomMode::default())
            };
            let stack = crate::tui::mode_stack::ModeStack::new(base, &mut app);
            Self { app, stack }
        }

        fn apply(&mut self) {
            while let Some(event) = self.app.take_terminal_event() {
                let mut cx = self.app.view_cx();
                self.stack.process_terminal_event(&mut cx, event);
            }
            self.stack.apply_pending(&mut self.app);
        }

        fn key(&mut self, key: KeyEvent) -> Action {
            let action = self.stack.process_input(&mut self.app, key);
            self.apply();
            action
        }

        fn overlay_active(&mut self) -> bool {
            self.stack.overlay_active(&mut self.app)
        }

        fn top_theme_mode(&mut self) -> crate::theme::UiMode {
            self.stack
                .top_presentation(&mut self.app)
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
    fn share_status_envelope_carries_stream_and_state() {
        // `state` is written straight into the browser's per-share status, so
        // both labels are part of the contract with web/src/App.tsx.
        assert_eq!(
            share_status_envelope(StreamId(7), ShareViewState::Reconnecting),
            "{\"type\":\"share_status\",\"stream_id\":7,\"state\":\"reconnecting\"}"
        );
        assert_eq!(
            share_status_envelope(StreamId(7), ShareViewState::WaitingForKeyframe),
            "{\"type\":\"share_status\",\"stream_id\":7,\"state\":\"waiting-for-keyframe\"}"
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
    fn loopback_enable_requires_capture_source() {
        let mut app = test_app();
        app.allow_settings_preview_capture = false;

        app.set_loopback_enabled(true);

        assert!(!app.loopback_tap.is_active());
        assert!(app.loopback_playback.is_none());
        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert!(app.view.status.text().contains("loopback unavailable"));
    }

    #[test]
    fn loopback_enable_rejects_deafened_state() {
        let mut app = test_app();
        app.voice_state
            .store(VoiceState::Deafened, Ordering::Relaxed);

        app.set_loopback_enabled(true);

        assert!(!app.loopback_tap.is_active());
        assert!(app.loopback_playback.is_none());
        assert_eq!(
            app.view.status.text(),
            "loopback unavailable: undeafen before using loopback"
        );
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

        let sent = app.send_network_command(
            NetworkCommand::SendChat {
                room_id: rpc::ids::RoomId(1),
                body: "hello".to_string(),
            },
            true,
        );

        assert!(!sent);
        assert_eq!(app.pending_network_commands.len(), 1);
        assert!(matches!(
            app.pending_network_commands.front(),
            Some(NetworkCommand::SendChat { body, .. }) if body == "hello"
        ));
        assert_eq!(app.view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn local_identity_failure_keeps_public_connection_available() {
        let mut app = test_app();
        let (tx, _rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.active_network_generation = Some(7);
        let message = "local identity is unreadable; file preserved".to_string();

        app.handle_network_event(NetworkEvent::LocalIdentityUnavailable {
            message: message.clone(),
        });

        assert!(app.network.is_some());
        assert_eq!(app.active_network_generation, Some(7));
        assert!(!app.supervisor.network.is_pending());
        assert!(!app.supervise_network(Instant::now()));
        assert_eq!(app.last_network_notice.as_deref(), Some(message.as_str()));
        assert_eq!(app.view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn command_during_reconnect_backoff_queues_and_flushes_after_auth() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.voice_left = true;

        app.handle_network_event(NetworkEvent::ReconnectScheduled {
            retry_in: Duration::from_secs(1),
            reason: "reset".to_string(),
        });
        assert!(!app.send_network_command(
            NetworkCommand::SendChat {
                room_id: RoomId(1),
                body: "queued".to_string(),
            },
            true,
        ));
        assert!(rx.try_recv().is_err());
        assert_eq!(app.pending_network_commands.len(), 1);

        app.handle_network_event(NetworkEvent::Authenticated {
            session_id: SessionId(1),
            user_id: UserId(1),
            rooms: vec![test_room_info(1)],
            users: vec![user_summary(UserId(1), "alice")],
            default_room: RoomId(1),
            dms_enabled: true,
            video_addr: "127.0.0.1:41000".parse().unwrap(),
            video_transport_mode: rpc::crypto::TransportMode::Encrypted,
            video_auth_key: [0; rpc::crypto::KEY_LEN],
        });

        let mut flushed = false;
        while let Ok(command) = rx.try_recv() {
            if matches!(command, NetworkCommand::SendChat { body, .. } if body == "queued") {
                flushed = true;
            }
        }
        assert!(flushed);
        assert!(app.pending_network_commands.is_empty());
        assert!(!app.room.network_disconnected);
        assert_eq!(
            app.video_transport.map(|transport| transport.peer_addr()),
            Some("127.0.0.1:41000".parse().unwrap())
        );
    }

    #[test]
    fn authentication_assigns_an_initial_room_to_preconnected_rpc_frontend() {
        let mut app = test_app();
        let client_id = crate::client_channel::ClientId(7);
        app.register_rpc_client(client_id);
        app.voice_left = true;

        app.handle_network_event(NetworkEvent::Authenticated {
            session_id: SessionId(1),
            user_id: UserId(1),
            rooms: vec![test_room_info(1), test_room_info(2)],
            users: vec![user_summary(UserId(1), "alice")],
            default_room: RoomId(2),
            dms_enabled: true,
            video_addr: "127.0.0.1:41000".parse().unwrap(),
            video_transport_mode: rpc::crypto::TransportMode::Encrypted,
            video_auth_key: [0; rpc::crypto::KEY_LEN],
        });

        let snapshot = app.rpc_snapshot(client_id);
        assert_eq!(snapshot.selected_room, Some(RoomId(2)));
    }

    #[test]
    fn failed_initial_history_send_clears_in_flight_state() {
        let mut app = test_app();
        let (tx, _rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.room.network_disconnected = true;
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);

        app.request_initial_history_for_viewed_room();

        assert!(app.room.begin_history_fetch(RoomId(1)));
        app.room.abort_history_fetch(RoomId(1), None);
    }

    #[test]
    fn leading_space_escapes_slash_command_as_chat() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        enter_test_room(&mut app);
        app.view.composer.set_lines(" /help");

        app.submit_input();

        match rx.try_recv().unwrap() {
            NetworkCommand::SendChat { body, .. } => assert_eq!(body, "/help"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn room_command_switches_viewed_room_by_name() {
        let mut app = test_app();
        let (tx, _rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            vec![user_summary(UserId(1), "alice")],
            rpc::ids::RoomId(1),
            None,
            user_id,
        );

        app.view.composer.set_lines("/room room-2");
        app.submit_input();
        assert_eq!(app.room.viewed_room, Some(rpc::ids::RoomId(2)));

        app.view.composer.set_lines("/room nowhere");
        app.submit_input();
        assert_eq!(app.room.viewed_room, Some(rpc::ids::RoomId(2)));
        assert_eq!(app.view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn dm_command_sends_open_dm_for_named_user() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_room_with_users(
            &mut app,
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
        );

        app.view.composer.set_lines("/dm bob");
        app.submit_input();

        match rx.try_recv().unwrap() {
            NetworkCommand::OpenDm(user_id) => assert_eq!(user_id, UserId(2)),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn own_user_presence_produces_no_status_notice() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        enter_room_with_users(&mut app, vec![user_summary(UserId(1), "alice")]);
        app.set_status("steady");

        app.handle_network_event(NetworkEvent::Presence {
            user: user_summary(UserId(1), "alice"),
            online: true,
        });

        assert_eq!(app.view.status.text(), "steady");
    }

    #[test]
    fn disabled_server_dms_refuse_a_new_dm_without_a_round_trip() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));

        app.handle_network_event(NetworkEvent::Authenticated {
            session_id: SessionId(1),
            user_id: UserId(1),
            rooms: vec![
                test_room_info(1),
                dm_room_info(0x8000_0001, UserId(1), UserId(2)),
            ],
            users: vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
                user_summary(UserId(3), "carol"),
            ],
            default_room: RoomId(1),
            dms_enabled: false,
            video_addr: "127.0.0.1:41000".parse().unwrap(),
            video_transport_mode: rpc::crypto::TransportMode::Encrypted,
            video_auth_key: [0; rpc::crypto::KEY_LEN],
        });
        while rx.try_recv().is_ok() {}

        app.view.composer.set_lines("/dm carol");
        app.submit_input();

        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert_eq!(
            app.view.status.text(),
            "this server has direct messages disabled"
        );
        assert!(app.pending_dm_clients.is_empty());
        assert!(
            !std::iter::from_fn(|| rx.try_recv().ok())
                .any(|command| matches!(command, NetworkCommand::OpenDm(_))),
            "a refused dm must not reach the server"
        );

        // The server keeps serving DM rooms opened before it was disabled.
        app.view.composer.set_lines("/dm bob");
        app.submit_input();

        assert!(
            std::iter::from_fn(|| rx.try_recv().ok())
                .any(|command| matches!(command, NetworkCommand::OpenDm(UserId(2)))),
            "an existing dm must still be openable"
        );
    }

    #[test]
    fn dm_irrelevant_presence_produces_no_notice() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[dm_room_info(0x8000_0001, UserId(1), UserId(2))],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(0x8000_0001),
            None,
            user_id,
        );
        app.set_status("steady");

        app.handle_network_event(NetworkEvent::Presence {
            user: user_summary(UserId(3), "carol"),
            online: true,
        });

        assert_eq!(app.view.status.text(), "steady");
    }

    #[test]
    fn renamed_e2e_peer_keeps_the_same_trust_state() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[dm_room_info(0x8000_0001, UserId(1), UserId(2))],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(0x8000_0001),
            None,
            user_id,
        );

        app.handle_network_event(NetworkEvent::Presence {
            user: user_summary(UserId(2), "robert"),
            online: true,
        });
        app.handle_network_event(NetworkEvent::E2ePeerPinMatched {
            identity: crate::e2e::AcceptedPeerIdentity {
                room_id: RoomId(0x8000_0001),
                user_id: UserId(2),
                identity: crate::config::E2ePeerIdentity {
                    room_id: 0x8000_0001,
                    user_id: 2,
                    username: "robert".to_string(),
                    public_key: "11".repeat(32),
                    trust_level: crate::config::E2eTrustLevel::Accepted,
                },
                trust_level: crate::config::E2eTrustLevel::Accepted,
                change_from: None,
                verified_keys: Vec::new(),
            },
        });

        assert_eq!(app.room.username_of(UserId(2)), "robert");
        assert!(matches!(
            app.room.e2e_trust_state(RoomId(0x8000_0001)),
            Some(room::DmTrustState::Accepted {
                change_from: None,
                ..
            })
        ));
        assert!(app.set_viewed_room(RoomId(0x8000_0001)));
        while rx.try_recv().is_ok() {}

        app.view.composer.set_lines("/identity robert");
        app.submit_input();
        assert!(matches!(
            rx.try_recv().unwrap(),
            NetworkCommand::ReviewPeerIdentity { user_id: UserId(2) }
        ));

        app.view.composer.set_lines("/identity");
        app.submit_input();
        assert!(matches!(
            rx.try_recv().unwrap(),
            NetworkCommand::ReviewPeerIdentity { user_id: UserId(2) }
        ));
    }

    #[test]
    fn identity_user_opens_missing_dm_and_routes_review_to_requester() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_room_with_users(
            &mut app,
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
        );

        app.view.composer.set_lines("/identity bob");
        app.submit_input();

        assert!(matches!(
            rx.try_recv().unwrap(),
            NetworkCommand::OpenDm(UserId(2))
        ));
        assert_eq!(
            app.pending_identity_review
                .get(&UserId(2))
                .and_then(|clients| clients.back()),
            Some(&crate::client_channel::ClientId::PRIMARY)
        );
        assert_eq!(
            app.pending_identity_review
                .get(&UserId(2))
                .map(VecDeque::len),
            Some(1)
        );
    }

    #[test]
    fn local_verification_text_uses_development_server_key_fallback() {
        let mut app = test_app();
        app.config.servers.push(ServerEntry {
            id: test_server_id("development"),
            label: "development".to_string(),
            server_public_key: String::new(),
            ..ServerEntry::default()
        });
        app.room.active_server_id = Some(test_server_id("development"));
        app.user_id = Some(UserId(42));
        app.e2e_account_id = Some(rpc::ids::AccountId([0x11; 32]));

        let text = app.local_verification_text().unwrap();

        assert!(text.starts_with(&format!(
            "chatt-e2e:v2:{}:42:",
            rpc::base32::encode(&rpc::crypto::dev_server_public_key())
        )));
        assert_eq!(
            crate::e2e_identity::VerificationText::parse(&text)
                .unwrap()
                .encode(),
            text
        );
    }

    #[test]
    fn forgetting_verification_keeps_the_exact_durable_pin_accepted() {
        let mut app = test_app();
        let path =
            std::env::temp_dir().join(format!("chatt-forget-identity-{}.toml", std::process::id()));
        app.config.config_path = Some(path.clone());
        app.room.active_server_id = Some(test_server_id("test"));
        let mut pin = crate::config::E2ePeerPin {
            room_id: 0x8000_0001,
            user_id: 2,
            username: "bob".to_string(),
            public_key: "11".repeat(32),
            trust_level: crate::config::E2eTrustLevel::Verified,
            change_from: None,
            previous: Vec::new(),
        };
        app.config.servers.push(ServerEntry {
            id: test_server_id("test"),
            label: "test".to_string(),
            e2e_peer_pins: vec![pin.clone()],
            ..ServerEntry::default()
        });

        pin.trust_level = crate::config::E2eTrustLevel::Accepted;
        assert!(app.persist_e2e_pin(pin));
        assert_eq!(app.config.servers[0].e2e_peer_pins.len(), 1);
        assert_eq!(
            app.config.servers[0].e2e_peer_pins[0].trust_level,
            crate::config::E2eTrustLevel::Accepted
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("e2e-peer-pins")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn restored_e2e_peer_pin_match_clears_changed_state_without_claiming_verification() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[dm_room_info(0x8000_0001, UserId(1), UserId(2))],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(0x8000_0001),
            None,
            user_id,
        );

        app.room.set_e2e_trust_state(
            RoomId(0x8000_0001),
            room::DmTrustState::Accepted {
                peer: UserId(2),
                identity: crate::config::E2ePeerIdentity {
                    room_id: 0x8000_0001,
                    user_id: 2,
                    username: "robert".to_string(),
                    public_key: "22".repeat(32),
                    trust_level: crate::config::E2eTrustLevel::Accepted,
                },
                change_from: Some(crate::config::E2eTrustLevel::Accepted),
            },
        );
        assert!(matches!(
            app.room.e2e_trust_state(RoomId(0x8000_0001)),
            Some(room::DmTrustState::Accepted {
                change_from: Some(crate::config::E2eTrustLevel::Accepted),
                ..
            })
        ));
        app.handle_network_event(NetworkEvent::E2ePeerPinMatched {
            identity: crate::e2e::AcceptedPeerIdentity {
                room_id: RoomId(0x8000_0001),
                user_id: UserId(2),
                identity: crate::config::E2ePeerIdentity {
                    room_id: 0x8000_0001,
                    user_id: 2,
                    username: "bob".to_string(),
                    public_key: "11".repeat(32),
                    trust_level: crate::config::E2eTrustLevel::Accepted,
                },
                trust_level: crate::config::E2eTrustLevel::Accepted,
                change_from: None,
                verified_keys: Vec::new(),
            },
        });

        assert!(matches!(
            app.room.e2e_trust_state(RoomId(0x8000_0001)),
            Some(room::DmTrustState::Accepted { .. })
        ));
    }

    #[test]
    fn dm_opened_waits_for_room_upsert_when_room_is_unknown() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1)],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            user_id,
        );
        let dm_id = RoomId(0x8000_0001);
        app.pending_dm_clients
            .entry(UserId(2))
            .or_default()
            .push_back(crate::client_channel::ClientId::PRIMARY);

        app.handle_network_event(NetworkEvent::DmOpened {
            room_id: dm_id,
            peer: UserId(2),
        });
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
        assert_eq!(
            app.pending_dm_open
                .get(&(dm_id, UserId(2)))
                .and_then(|clients| clients.front()),
            Some(&crate::client_channel::ClientId::PRIMARY)
        );

        app.handle_network_event(NetworkEvent::RoomUpserted(dm_room_info(
            dm_id.0,
            UserId(1),
            UserId(2),
        )));

        assert!(app.pending_dm_open.is_empty());
        assert_eq!(app.room.viewed_room, Some(dm_id));
        assert_eq!(app.view.status.text(), "dm with bob");
    }

    #[test]
    fn dm_opened_routes_to_the_requesting_attached_client() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1)],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            user_id,
        );
        let client_id = crate::client_channel::ClientId(7);
        let view = attach_test_client(&mut app, client_id);
        let dm_id = RoomId(0x8000_0001);

        app.handle_client_command(client_id, command::CoreCommand::OpenDm(UserId(2)));
        assert!(matches!(
            rx.try_recv(),
            Ok(NetworkCommand::OpenDm(UserId(2)))
        ));
        app.handle_network_event(NetworkEvent::DmOpened {
            room_id: dm_id,
            peer: UserId(2),
        });
        app.handle_network_event(NetworkEvent::RoomUpserted(dm_room_info(
            dm_id.0,
            UserId(1),
            UserId(2),
        )));

        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
        let view = view.lock();
        assert_eq!(view.viewed_room, Some(dm_id));
        assert_eq!(view.status.text(), "dm with bob");
    }

    #[test]
    fn one_dm_result_routes_to_all_concurrent_requesters() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1)],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            user_id,
        );
        let clients = [
            crate::client_channel::ClientId(7),
            crate::client_channel::ClientId(8),
        ];
        let views = clients.map(|client| attach_test_client(&mut app, client));
        for client in clients {
            app.handle_client_command(client, command::CoreCommand::OpenDm(UserId(2)));
            assert!(matches!(
                rx.try_recv(),
                Ok(NetworkCommand::OpenDm(UserId(2)))
            ));
        }
        let dm_id = RoomId(0x8000_0001);
        app.handle_network_event(NetworkEvent::DmOpened {
            room_id: dm_id,
            peer: UserId(2),
        });
        app.handle_network_event(NetworkEvent::RoomUpserted(dm_room_info(
            dm_id.0,
            UserId(1),
            UserId(2),
        )));

        for view in views {
            assert_eq!(view.lock().viewed_room, Some(dm_id));
        }
    }

    #[test]
    fn app_drop_reacquires_released_core_state() {
        let mut app = test_app();
        app.release_core_state();
        drop(app);
    }

    #[test]
    fn attached_client_sends_to_its_explicit_room_without_moving_primary() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        enter_test_room(&mut app);
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
        let client_id = crate::client_channel::ClientId(9);
        attach_test_client(&mut app, client_id);

        app.handle_client_command(
            client_id,
            command::CoreCommand::SendChat {
                room_id: Some(RoomId(2)),
                body: "from attached".to_string(),
            },
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(NetworkCommand::SendChat { room_id: RoomId(2), body })
                if body == "from attached"
        ));
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
    }

    #[test]
    fn remote_set_viewed_room_switches_only_the_remote_view() {
        let mut app = test_app();
        let (tx, _rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            Vec::new(),
            RoomId(1),
            None,
            user_id,
        );
        let (core, view) = app.parts_mut();
        view.switch_room(RoomId(1), &core.room);
        let client_id = crate::client_channel::ClientId(4);
        let remote_view = attach_test_client(&mut app, client_id);

        app.handle_client_command(client_id, command::CoreCommand::SetViewedRoom(RoomId(2)));

        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
        let view = remote_view.lock();
        assert_eq!(view.viewed_room, Some(RoomId(2)));
        assert_eq!(view.status.text(), "viewing room-2");
    }

    #[test]
    fn remote_quit_requests_detach_without_touching_primary() {
        let mut app = test_app();
        enter_test_room(&mut app);
        let client_id = crate::client_channel::ClientId(5);
        attach_test_client(&mut app, client_id);

        assert!(app.handle_client_command(client_id, command::CoreCommand::Quit));

        assert!(!app.take_quit_requested());
    }

    #[test]
    fn commands_from_unknown_remote_clients_are_dropped() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        enter_test_room(&mut app);

        app.handle_client_command(
            crate::client_channel::ClientId(41),
            command::CoreCommand::SendChat {
                room_id: None,
                body: "ghost".to_string(),
            },
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
    }

    #[test]
    fn voice_command_moves_the_call_to_the_viewed_room() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);
        // The deafened path skips audio device startup, keeping the test
        // hermetic; the join command must still go out.
        app.voice_state
            .store(VoiceState::Deafened, Ordering::Relaxed);

        app.view.composer.set_lines("/voice");
        app.submit_input();

        assert_eq!(app.room.voice_room, None);
        assert_eq!(app.requested_voice_room, Some(rpc::ids::RoomId(1)));
        let mut commands = Vec::new();
        while let Ok(command) = rx.try_recv() {
            commands.push(command);
        }
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, NetworkCommand::JoinVoice(rpc::ids::RoomId(1)))),
            "expected JoinVoice, got {commands:?}"
        );
    }

    #[test]
    fn voice_join_failure_clears_requested_room() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);
        app.voice_state
            .store(VoiceState::Deafened, Ordering::Relaxed);

        app.view.composer.set_lines("/voice");
        app.submit_input();
        assert_eq!(app.requested_voice_room, Some(rpc::ids::RoomId(1)));

        app.handle_network_event(NetworkEvent::VoiceJoinFailed {
            room_id: rpc::ids::RoomId(1),
            message: "room not found".to_string(),
        });

        assert_eq!(app.requested_voice_room, None);
        assert_eq!(app.view.status.kind(), StatusKind::Error);

        app.view.composer.set_lines("/voice");
        app.submit_input();
        assert_eq!(app.requested_voice_room, Some(rpc::ids::RoomId(1)));
        let mut join_count = 0;
        while let Ok(command) = rx.try_recv() {
            if matches!(command, NetworkCommand::JoinVoice(rpc::ids::RoomId(1))) {
                join_count += 1;
            }
        }
        assert_eq!(
            join_count, 2,
            "retrying after a failed join must send JoinVoice again"
        );
    }

    #[test]
    fn voice_leave_command_sends_leave_voice() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        enter_test_room(&mut app);
        app.room.voice_room = Some(rpc::ids::RoomId(1));

        app.view.composer.set_lines("/voice-leave");
        app.submit_input();

        match rx.try_recv().unwrap() {
            NetworkCommand::LeaveVoice => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn reauth_skips_voice_auto_join_after_explicit_leave() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.voice_state
            .store(VoiceState::Deafened, Ordering::Relaxed);
        enter_test_room(&mut app);
        app.room.voice_room = Some(rpc::ids::RoomId(1));

        app.view.composer.set_lines("/voice-leave");
        app.submit_input();
        while rx.try_recv().is_ok() {}

        let authenticated = || NetworkEvent::Authenticated {
            session_id: SessionId(1),
            user_id: UserId(1),
            rooms: vec![test_room_info(1)],
            users: vec![user_summary(UserId(1), "alice")],
            default_room: RoomId(1),
            dms_enabled: true,
            video_addr: "127.0.0.1:41000".parse().unwrap(),
            video_transport_mode: rpc::crypto::TransportMode::Encrypted,
            video_auth_key: [0; rpc::crypto::KEY_LEN],
        };
        app.handle_network_event(authenticated());
        let mut commands = Vec::new();
        while let Ok(command) = rx.try_recv() {
            commands.push(command);
        }
        assert!(
            !commands
                .iter()
                .any(|command| matches!(command, NetworkCommand::JoinVoice(_))),
            "auto-join must stay suppressed after /voice-leave, got {commands:?}"
        );

        app.room.voice_room = None;
        app.view.composer.set_lines("/voice");
        app.submit_input();
        assert!(!app.voice_left);
        while rx.try_recv().is_ok() {}

        app.handle_network_event(authenticated());
        let mut commands = Vec::new();
        while let Ok(command) = rx.try_recv() {
            commands.push(command);
        }
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, NetworkCommand::JoinVoice(_))),
            "explicit join re-enables the auto-join, got {commands:?}"
        );
    }

    #[test]
    fn voice_switch_restarts_audio_after_old_stream_stops() {
        let mut app = test_app();
        let (tx, _rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.config.soundboard.enabled = true;
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            vec![user_summary(UserId(1), "alice")],
            RoomId(1),
            None,
            user_id,
        );
        app.session_id = Some(SessionId(1));
        app.room.voice_room = Some(RoomId(1));
        app.voice_tx_enabled.store(true, Ordering::Relaxed);

        app.handle_network_event(NetworkEvent::VoiceStopped {
            room_id: RoomId(1),
            session_id: SessionId(1),
            user_id: UserId(1),
            stream_id: StreamId(10),
            user_left: true,
        });
        app.handle_network_event(NetworkEvent::VoiceStarted {
            room_id: RoomId(2),
            session_id: SessionId(1),
            user_id: UserId(1),
            stream_id: StreamId(11),
            user_joined: true,
        });

        assert_eq!(app.room.voice_room, Some(RoomId(2)));
        assert!(app.voice_tx_enabled.load(Ordering::Relaxed));
    }

    #[test]
    fn call_status_messages_follow_user_transitions_only_in_current_call() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        app.session_id = Some(SessionId(1));
        let local_user = app.user_id;
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            local_user,
        );
        app.room.voice_room = Some(RoomId(1));

        let event = |stream_id, user_joined| NetworkEvent::VoiceStarted {
            room_id: RoomId(1),
            session_id: SessionId(stream_id),
            user_id: UserId(2),
            stream_id: StreamId(stream_id as u32),
            user_joined,
        };
        let first = app.handle_network_event_change(event(10, true));
        assert!(first.is_some());
        assert_eq!(
            app.room.system_messages(RoomId(1))[0].body,
            "bob joined the call"
        );

        assert!(app.handle_network_event_change(event(11, false)).is_none());
        app.handle_network_event(NetworkEvent::VoiceStarted {
            room_id: RoomId(2),
            session_id: SessionId(12),
            user_id: UserId(2),
            stream_id: StreamId(12),
            user_joined: true,
        });
        assert_eq!(app.room.system_messages(RoomId(1)).len(), 1);
        assert!(app.room.system_messages(RoomId(2)).is_empty());

        assert!(
            app.handle_network_event_change(NetworkEvent::VoiceStopped {
                room_id: RoomId(1),
                session_id: SessionId(10),
                user_id: UserId(2),
                stream_id: StreamId(10),
                user_left: false,
            })
            .is_none()
        );
        let final_leave = app.handle_network_event_change(NetworkEvent::VoiceStopped {
            room_id: RoomId(1),
            session_id: SessionId(11),
            user_id: UserId(2),
            stream_id: StreamId(11),
            user_left: true,
        });
        assert!(final_leave.is_some());
        let messages = app.room.system_messages(RoomId(1));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].body, "bob left the call");
    }

    #[test]
    fn own_confirmed_join_and_leave_are_retained_as_system_messages() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        app.session_id = Some(SessionId(1));
        let local_user = app.user_id;
        app.room.authenticated(
            &[test_room_info(1)],
            vec![user_summary(UserId(1), "alice")],
            RoomId(1),
            None,
            local_user,
        );

        app.handle_network_event(NetworkEvent::VoiceStarted {
            room_id: RoomId(1),
            session_id: SessionId(1),
            user_id: UserId(1),
            stream_id: StreamId(10),
            user_joined: true,
        });
        app.handle_network_event(NetworkEvent::VoiceStopped {
            room_id: RoomId(1),
            session_id: SessionId(1),
            user_id: UserId(1),
            stream_id: StreamId(10),
            user_left: true,
        });

        let messages = app.room.system_messages(RoomId(1));
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].sender, "call");
        assert_eq!(messages[0].body, "alice joined the call");
        assert_eq!(messages[1].body, "alice left the call");
        assert_eq!(app.room.voice_room, None);
        let snapshot = app.rpc_snapshot(crate::client_channel::ClientId::PRIMARY);
        let projected = snapshot.room.expect("selected room").system_messages;
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].sender, "call");
        assert_eq!(projected[0].body, "alice joined the call");
    }

    #[test]
    fn share_availability_follows_the_confirmed_voice_room() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            user_id,
        );
        app.session_id = Some(SessionId(1));
        app.room.voice_room = Some(RoomId(1));
        let available = |room_id, stream_id| NetworkEvent::ShareAvailable {
            room_id,
            stream_id,
            sender_name: "bob".to_string(),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            extradata: Vec::new(),
            view_secret: vec![7; 32],
        };

        app.handle_network_event(available(RoomId(2), StreamId(20)));
        assert!(app.room.available_shares.is_empty());

        app.handle_network_event(available(RoomId(1), StreamId(10)));
        assert!(app.room.available_shares.contains_key(&StreamId(10)));
        let first_generation = app.room.available_shares[&StreamId(10)].generation;
        app.handle_network_event(available(RoomId(1), StreamId(10)));
        assert_eq!(
            app.room.available_shares[&StreamId(10)].generation,
            first_generation
        );
        app.handle_network_event(NetworkEvent::ShareAvailable {
            room_id: RoomId(1),
            stream_id: StreamId(10),
            sender_name: "bob".to_string(),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            extradata: Vec::new(),
            view_secret: vec![8; 32],
        });
        assert_ne!(
            app.room.available_shares[&StreamId(10)].generation,
            first_generation
        );

        app.handle_network_event(NetworkEvent::VoiceStopped {
            room_id: RoomId(1),
            session_id: SessionId(1),
            user_id: UserId(1),
            stream_id: StreamId(1),
            user_left: true,
        });
        assert!(app.room.available_shares.is_empty());
    }

    #[test]
    fn share_started_without_a_capture_stops_the_server_side_share() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.session_id = Some(SessionId(1));
        app.room.voice_room = Some(RoomId(1));

        // The capture died inside the StartShare round trip, so this reply
        // announces a stream nothing will publish to.
        app.handle_network_event(NetworkEvent::ShareStarted {
            attempt_id: ShareAttemptId(1),
            room_id: RoomId(1),
            stream_id: StreamId(10),
            publish_secret: vec![7; 32],
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            extradata: Vec::new(),
        });

        match rx.try_recv().expect("stop share command") {
            NetworkCommand::StopShare { stream_id } => assert_eq!(stream_id, StreamId(10)),
            other => panic!("unexpected command: {other:?}"),
        }
        assert_eq!(app.screencast_stream_id, None);
        assert!(app.room.available_shares.is_empty());
    }

    #[test]
    fn reconnect_clears_shares_tied_to_the_dead_session() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        app.session_id = Some(SessionId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1)],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            user_id,
        );
        app.room.voice_room = Some(RoomId(1));
        app.handle_network_event(NetworkEvent::ShareAvailable {
            room_id: RoomId(1),
            stream_id: StreamId(10),
            sender_name: "bob".to_string(),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            extradata: Vec::new(),
            view_secret: vec![7; 32],
        });
        assert!(app.room.available_shares.contains_key(&StreamId(10)));

        app.handle_network_event(NetworkEvent::ReconnectScheduled {
            retry_in: Duration::from_secs(2),
            reason: "connection reset".to_string(),
        });

        assert!(app.room.available_shares.is_empty());
        assert_eq!(app.screencast_stream_id, None);
    }

    #[test]
    fn cycle_room_wraps_in_catalog_order() {
        let mut app = test_app();
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            Vec::new(),
            rpc::ids::RoomId(1),
            None,
            None,
        );

        app.cycle_room(1);
        assert_eq!(app.room.viewed_room, Some(rpc::ids::RoomId(2)));
        app.cycle_room(1);
        assert_eq!(app.room.viewed_room, Some(rpc::ids::RoomId(1)));
        app.cycle_room(-1);
        assert_eq!(app.room.viewed_room, Some(rpc::ids::RoomId(2)));
    }

    #[test]
    fn cycle_room_without_current_room_uses_directional_edge() {
        let mut app = test_app();
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            Vec::new(),
            RoomId(1),
            None,
            None,
        );
        app.room.viewed_room = None;

        app.cycle_room(1);
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));

        app.room.viewed_room = None;
        app.cycle_room(-1);
        assert_eq!(app.room.viewed_room, Some(RoomId(2)));
    }

    #[test]
    fn background_room_file_completion_updates_its_own_history() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1), test_room_info(2)],
            vec![user_summary(UserId(1), "alice")],
            RoomId(1),
            None,
            user_id,
        );
        let transfer_id = rpc::ids::FileTransferId(9);
        let metadata = rpc::control::FileMetadata {
            mls_event_id: None,
            transfer_id,
            room_id: RoomId(2),
            sender: UserId(2),
            sender_name: "bob".to_string(),
            file_name: "room-two.bin".to_string(),
            original_name: "room-two.bin".to_string(),
            size: 12,
            encoding: rpc::control::FileContentEncoding::Identity,
            timestamp_ms: 44,
        };
        let attachment_id = local_rpc::model::AttachmentId {
            timestamp_ms: metadata.timestamp_ms,
            transfer_id: metadata.transfer_id,
        };
        let served_name = app
            .download_store
            .insert("room-two.bin", vec![7; 12])
            .unwrap();

        app.handle_network_event(NetworkEvent::FileReceived {
            metadata,
            served_name,
            dimensions: None,
        });

        assert!(
            app.download_store
                .resolve_attachment(attachment_id)
                .is_some()
        );
        assert!(
            app.room
                .resident_file_detail(
                    RoomId(1),
                    &crate::room_history::FileHistoryKey {
                        timestamp_ms: attachment_id.timestamp_ms,
                        transfer_id: attachment_id.transfer_id,
                    }
                )
                .is_none()
        );
        assert!(app.set_viewed_room(RoomId(2)));
        assert!(
            app.room
                .resident_file_detail(
                    RoomId(2),
                    &crate::room_history::FileHistoryKey {
                        timestamp_ms: attachment_id.timestamp_ms,
                        transfer_id: attachment_id.transfer_id,
                    }
                )
                .is_some()
        );
    }

    #[test]
    fn reaching_chat_top_requests_one_older_history_page() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);
        assert!(app.room.begin_history_fetch(RoomId(1)));
        let messages = (6..=20)
            .map(|id| rpc::control::ChatMessage {
                message_id: rpc::ids::MessageId(id),
                room_id: RoomId(1),
                sender: UserId(2),
                sender_name: "bob".to_string(),
                timestamp_ms: id * 1_000,
                body: format!("message {id}"),
                file_transfer_id: None,
                flags: rpc::control::MessageFlags::default(),
                target: None,
            })
            .collect::<Vec<_>>();
        app.room
            .complete_history_fetch(RoomId(1), None, Some(MessageId(6)), false);
        app.room.merge_history(RoomId(1), messages);
        let (core, view) = app.parts_mut();
        view.sync_independent(&core.room);

        app.request_older_history_if_at_top(40, 5);
        assert!(rx.try_recv().is_err());

        let (core, view) = app.parts_mut();
        let history = core.room.history_ref(RoomId(1)).unwrap();
        view.active.chat.top(&history, 40, 5);
        app.request_older_history_if_at_top(40, 5);
        match rx.try_recv().unwrap() {
            NetworkCommand::FetchHistory {
                room_id,
                before,
                limit,
            } => {
                assert_eq!(room_id, RoomId(1));
                assert_eq!(before, Some(rpc::ids::MessageId(6)));
                assert_eq!(limit, rpc::control::MAX_HISTORY_FETCH_MESSAGES);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        app.request_older_history_if_at_top(40, 5);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn web_history_fetch_crosses_the_resident_front_without_broadcast_reset() {
        let mut config = Config::default();
        config.web.enabled = true;
        config.web.bind = "127.0.0.1:0".to_string();
        let mut app = TestApp::new(config, None).expect("test app");
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);

        assert!(app.room.begin_history_fetch(RoomId(1)));
        let newest = (101..=200)
            .map(|id| test_chat_record(RoomId(1), MessageId(id)))
            .collect::<Vec<_>>();
        let user_id = app.user_id;
        let initial =
            app.room
                .history_chunk_received(RoomId(1), None, newest, false, true, user_id);
        assert!(initial.change.unwrap().refresh_window);

        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        let generation = app.room.room_generation(RoomId(1)).unwrap();
        app.send_web_older_page(7, RoomId(1), generation, MessageId(101), 100);
        assert!(app.pending_web_history.contains_key(&7));
        assert!(matches!(
            rx.try_recv(),
            Ok(NetworkCommand::FetchHistory {
                room_id: RoomId(1),
                before: Some(MessageId(101)),
                limit: rpc::control::MAX_HISTORY_FETCH_MESSAGES,
            })
        ));

        let older = (1..=100)
            .map(|id| test_chat_record(RoomId(1), MessageId(id)))
            .collect::<Vec<_>>();
        let change = app
            .handle_network_event_change(NetworkEvent::HistoryChunk {
                room_id: RoomId(1),
                before: Some(MessageId(101)),
                messages: older,
                at_start: true,
                complete: true,
            })
            .unwrap();

        assert!(!change.refresh_window);
        assert!(!app.pending_web_history.contains_key(&7));
    }

    #[test]
    fn disconnect_releases_pending_web_history() {
        let mut config = Config::default();
        config.web.enabled = true;
        config.web.bind = "127.0.0.1:0".to_string();
        let mut app = TestApp::new(config, None).expect("test app");
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);

        assert!(app.room.begin_history_fetch(RoomId(1)));
        let newest = (101..=200)
            .map(|id| test_chat_record(RoomId(1), MessageId(id)))
            .collect::<Vec<_>>();
        let user_id = app.user_id;
        app.room
            .history_chunk_received(RoomId(1), None, newest, false, true, user_id);

        let (tx, _rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        let generation = app.room.room_generation(RoomId(1)).unwrap();
        app.send_web_older_page(7, RoomId(1), generation, MessageId(101), 100);
        assert!(app.pending_web_history.contains_key(&7));

        app.disconnect_network();

        // Dropping the request unanswered left the tab's paging wedged until
        // some unrelated sync arrived.
        assert!(app.pending_web_history.is_empty());
    }

    #[test]
    fn web_history_can_continue_after_a_mutation_only_page() {
        let mut config = Config::default();
        config.web.enabled = true;
        config.web.bind = "127.0.0.1:0".to_string();
        let mut app = TestApp::new(config, None).expect("test app");
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);

        assert!(app.room.begin_history_fetch(RoomId(1)));
        let mut mutation = test_chat_record(RoomId(1), MessageId(100));
        mutation.message.target = Some(MessageId(1));
        let user_id = app.user_id;
        app.room
            .history_chunk_received(RoomId(1), None, vec![mutation], false, true, user_id);
        assert_eq!(
            app.room
                .history_ref(RoomId(1))
                .unwrap()
                .latest_page(crate::web_server::SYNC_WINDOW)
                .older_cursor,
            Some(MessageId(100))
        );

        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        let generation = app.room.room_generation(RoomId(1)).unwrap();
        app.send_web_older_page(7, RoomId(1), generation, MessageId(100), 100);

        assert!(app.pending_web_history.contains_key(&7));
        assert!(matches!(
            rx.try_recv(),
            Ok(NetworkCommand::FetchHistory {
                room_id: RoomId(1),
                before: Some(MessageId(100)),
                ..
            })
        ));
    }

    #[test]
    fn rpc_history_returns_messages_fetched_before_a_mutation_only_cursor() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);

        assert!(app.room.begin_history_fetch(RoomId(1)));
        let mut mutation = test_chat_record(RoomId(1), MessageId(100));
        mutation.message.target = Some(MessageId(1));
        let user_id = app.user_id;
        app.room
            .history_chunk_received(RoomId(1), None, vec![mutation], false, true, user_id);
        assert!(matches!(
            app.room.older_history_request(RoomId(1)),
            Some((RoomId(1), Some(MessageId(100)), _))
        ));
        let older = (1..100)
            .map(|id| test_chat_record(RoomId(1), MessageId(id)))
            .collect::<Vec<_>>();
        app.room.history_chunk_received(
            RoomId(1),
            Some(MessageId(100)),
            older,
            true,
            true,
            user_id,
        );

        let page = app
            .rpc_resident_history_page(RoomId(1), MessageId(100), 200)
            .expect("fetched page");
        assert_eq!(
            page.messages.first().map(|message| message.message_id),
            Some(MessageId(1))
        );
        assert_eq!(
            page.messages.last().map(|message| message.message_id),
            Some(MessageId(99))
        );
        assert!(page.at_start);
    }

    #[test]
    fn queued_user_network_commands_flush_when_worker_is_available() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.pending_network_commands
            .push_back(NetworkCommand::SendChat {
                room_id: rpc::ids::RoomId(1),
                body: "hello".to_string(),
            });

        app.flush_pending_network_commands();

        match rx.try_recv().unwrap() {
            NetworkCommand::SendChat { body, .. } => assert_eq!(body, "hello"),
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(app.pending_network_commands.is_empty());
    }

    #[test]
    fn local_mute_and_deafen_publish_voice_state() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_room_with_users(&mut app, vec![user_summary(UserId(1), "alice")]);

        app.set_voice_state(VoiceState::Muted);

        let state = loop {
            match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                NetworkCommand::SetVoiceState(state) => break state,
                NetworkCommand::LocalVoicePacket(_)
                | NetworkCommand::SequencedLocalVoicePacket { .. }
                | NetworkCommand::SetPlaybackSink(_) => {}
                other => panic!("unexpected command: {other:?}"),
            }
        };
        assert_eq!(state, VoiceState::Muted);

        app.set_voice_state(VoiceState::Deafened);

        let state = loop {
            match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                NetworkCommand::SetVoiceState(state) => break state,
                NetworkCommand::LocalVoicePacket(_)
                | NetworkCommand::SequencedLocalVoicePacket { .. }
                | NetworkCommand::SetPlaybackSink(_) => {}
                other => panic!("unexpected command: {other:?}"),
            }
        };
        assert_eq!(state, VoiceState::Deafened);
    }

    #[test]
    fn server_edit_reuses_one_editor_across_text_fields() {
        let mut draft = ServerEditDraft::from_server(
            &crate::config::ServerEntry::default(),
            &crate::config::Config::default(),
        );
        let first_editor = draft.active_editor_address().unwrap();
        draft.set_active_editor_text("local-dev");

        draft.move_focus_for_test(1);

        let second_editor = draft.active_editor_address().unwrap();
        assert_eq!(first_editor, second_editor);
        draft.set_active_editor_text("Alice Dev");

        let server = draft.fields().unwrap();
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
    fn retiring_settings_owner_releases_global_lease() {
        let mut app = test_app();
        app.allow_settings_preview_capture = false;
        let owner = crate::client_channel::ClientId(41);
        attach_test_client(&mut app, owner);
        app.handle_client_command(owner, command::CoreCommand::OpenSettings);
        assert_eq!(app.room.settings_owner, Some(owner));
        assert!(app.room.settings.is_some());

        app.retire_client(owner);

        assert_eq!(app.room.settings_owner, None);
        assert!(app.room.settings.is_none());
        let successor = crate::client_channel::ClientId(42);
        attach_test_client(&mut app, successor);
        app.handle_client_command(successor, command::CoreCommand::OpenSettings);
        assert_eq!(app.room.settings_owner, Some(successor));
    }

    #[test]
    fn mouse_wheel_moves_open_settings_device_picker() {
        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::capture_device_id(),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");
        {
            let mut session = settings_session.lock().unwrap();
            open_test_input_picker(&mut session);
        }

        assert_eq!(
            settings_session
                .lock()
                .unwrap()
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
            settings_session
                .lock()
                .unwrap()
                .input_picker
                .selector
                .current_item_index(),
            Some(1)
        );
    }

    #[test]
    fn switching_settings_tabs_closes_open_audio_picker_and_releases_input() {
        use crate::config::FormBindings;
        use crate::ui::settings::SettingsTab;

        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::capture_device_id(),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");
        open_test_input_picker(&mut settings_session.lock().unwrap());

        mode.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));

        {
            let session = settings_session.lock().unwrap();
            assert_eq!(session.tab, SettingsTab::Interface);
            assert!(!session.input_picker.open);
        }

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
        );

        let session = settings_session.lock().unwrap();
        assert_eq!(session.draft.form_bindings(), FormBindings::Vim);
        assert!(session.dirty);
    }

    #[test]
    fn clicking_another_settings_field_closes_open_audio_picker() {
        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::capture_device_id(),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");
        open_test_input_picker(&mut settings_session.lock().unwrap());
        mode.render(&mut app, &mut Buffer::new(100, 40), 0);

        let bitrate = crate::ui::settings::field_id_for("Capture Settings", "Bitrate");
        let rect = settings_session
            .lock()
            .unwrap()
            .form
            .field_rect_for_test(bitrate)
            .expect("bitrate hit target");
        mode.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );

        let session = settings_session.lock().unwrap();
        assert_eq!(session.form.focus(), bitrate);
        assert!(!session.input_picker.open);
        assert!(session.dirty);
    }

    #[test]
    fn adjusting_a_choice_marks_dirty_and_resyncs_live_config() {
        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::field_id_for("Capture Settings", "Bitrate"),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");
        let before = app.config.audio.bitrate_bps;

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::empty()),
        );

        assert!(settings_session.lock().unwrap().dirty);
        assert_ne!(app.config.audio.bitrate_bps, before);
    }

    #[test]
    fn latency_text_edit_commits_via_key_flow() {
        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::field_id_for("Latency", "Min Delay"),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");
        settings_session.lock().unwrap().draft.show_advanced = true;

        // The first key finds an empty field order, so the focus move is a
        // no-op that still runs a logic pass, registering the advanced fields
        // and seeding the editor for the focused row.
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        assert_eq!(
            settings_session.lock().unwrap().form.focus(),
            crate::ui::settings::field_id_for("Latency", "Min Delay"),
        );

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('5'), KeyModifiers::empty()),
        );
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        let session = settings_session.lock().unwrap();
        assert_eq!(session.draft.latency_ms.neteq_min_delay_ms, "205");
    }

    #[test]
    fn alt_l_and_alt_h_cycle_settings_tabs_and_relocate_focus() {
        use crate::ui::settings::SettingsTab;

        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::capture_device_id(),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");
        assert_eq!(settings_session.lock().unwrap().tab, SettingsTab::Audio);

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT),
        );
        {
            let session = settings_session.lock().unwrap();
            assert_eq!(session.tab, SettingsTab::Interface);
            // The Audio-only device row vanished, so focus fell to the new
            // tab's first field.
            assert_ne!(
                session.form.focus(),
                crate::ui::settings::capture_device_id()
            );
        }

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT),
        );
        assert_eq!(settings_session.lock().unwrap().tab, SettingsTab::Audio);

        // Cycling does not dirty the draft: no config value changed.
        assert!(!settings_session.lock().unwrap().dirty);
    }

    #[test]
    fn tab_and_backtab_cycle_settings_tabs() {
        use crate::ui::settings::SettingsTab;

        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::capture_device_id(),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");

        mode.process_input(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()));
        assert_eq!(settings_session.lock().unwrap().tab, SettingsTab::Interface);

        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        );
        let session = settings_session.lock().unwrap();
        assert_eq!(session.tab, SettingsTab::Audio);
        assert!(!session.dirty);
    }

    #[test]
    fn vim_insert_enter_advances_to_next_list_row_in_insert_mode() {
        let mut app = test_app();
        let first_row = crate::ui::settings::field_id_for("Advanced", "Origin 1");
        let form = FormState::new(first_row, crate::config::FormBindings::Vim);
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");
        settings_session.lock().unwrap().draft.show_advanced = true;

        // Register the Interface fields and seed the focused editor in normal
        // mode, then enter insert mode and type a valid origin.
        settings_session.lock().unwrap().tab = crate::ui::settings::SettingsTab::Interface;
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        for character in "https://chat.example.test".chars() {
            mode.process_input(
                &mut app,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::empty()),
            );
        }
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );

        let mut session = settings_session.lock().unwrap();
        assert_eq!(
            session.form.focus(),
            crate::ui::settings::field_id_for("Advanced", "Origin 2")
        );
        assert_eq!(session.form.editor_mut().mode(), EditorMode::Insert);
    }

    #[test]
    fn alt_l_cycles_tabs_while_a_text_field_is_focused() {
        use crate::ui::settings::SettingsTab;

        let mut app = test_app();
        let form = FormState::new(
            crate::ui::settings::field_id_for("Playback Settings", "Output Volume"),
            app.config.ui.default_bindings,
        );
        let mut mode = SettingsMode::with_form_for_test(form, &mut app);
        let settings_session = app.room.settings.clone().expect("settings session");

        // Register fields and seed the focused row's editor.
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty()),
        );

        // The chord is intercepted before the editor sees the key, while a
        // plain bracket still types into the field.
        mode.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::ALT),
        );
        assert_eq!(settings_session.lock().unwrap().tab, SettingsTab::Interface);
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
        app.view.composer.set_lines("unsent draft");
        let mut h = Harness::new(app);

        h.app.open_settings();
        h.apply();
        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert_eq!(h.stack.len(), 1);
        assert_eq!(h.top_theme_mode(), crate::theme::UiMode::Compose);
        assert_eq!(h.app.view.composer.text(), "unsent draft");
    }

    #[test]
    fn slash_help_pushes_command_notice() {
        let mut app = test_app();
        app.view.composer.set_lines("/help");

        app.submit_input();

        assert_eq!(app.view.active.chat.len(), 1);
        let notice = app.view.active.chat.local_record(0).unwrap();
        assert_eq!(notice.sender, "help");
        assert!(notice.body.contains("/report-bug what went wrong"));
        assert!(notice.body.contains("Press Tab again to cycle matches"));
        assert_eq!(
            notice.notice_kind,
            Some(crate::chat_buffer::NoticeKind::Info)
        );
        assert_eq!(app.view.status.text(), "slash commands listed");
    }

    #[test]
    fn out_of_order_live_chat_posts_an_error_in_the_affected_room() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        let room_1 = test_room_info(1);
        let mut room_2 = test_room_info(2);
        room_2.head = Some(MessageId(10));
        let user_id = app.user_id;
        app.room.authenticated(
            &[room_1, room_2],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            user_id,
        );
        let (core, view) = app.parts_mut();
        view.switch_room(RoomId(1), &core.room);

        app.handle_network_event(NetworkEvent::Chat(test_chat_record(
            RoomId(2),
            MessageId(9),
        )));

        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert!(
            app.view
                .status
                .text()
                .contains("out-of-order chat record 9")
        );
        assert!(app.view.status.text().contains("message ID is 10"));
        assert!(app.room.set_viewed_room(RoomId(2)));
        let (core, view) = app.parts_mut();
        view.switch_room(RoomId(2), &core.room);
        view.sync_independent(&core.room);
        assert_eq!(app.view.active.chat.len(), 1);
        let history = app.room.history_ref(RoomId(2)).unwrap();
        let notice = app.view.active.chat.record(&history, 0).unwrap();
        assert_eq!(notice.sender, "network");
        assert!(notice.body.contains("out-of-order chat record 9"));
        assert!(notice.body.contains("message ID is 10"));
        assert_eq!(
            notice.notice_kind,
            Some(crate::chat_buffer::NoticeKind::Error)
        );
    }

    #[test]
    fn applied_mls_chat_acknowledges_the_durable_ui_dispatch() {
        let mut app = test_app();
        app.user_id = Some(UserId(1));
        let user_id = app.user_id;
        app.room.authenticated(
            &[test_room_info(1)],
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
            RoomId(1),
            None,
            user_id,
        );
        let (tx, rx) = std::sync::mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));

        let mut record = test_chat_record(RoomId(1), MessageId(7));
        record.message.message_id = MessageId((1 << 63) | 7);
        app.handle_network_event(NetworkEvent::Chat(record));

        let acknowledged = (0..4).any(|_| {
            matches!(
                rx.recv_timeout(Duration::from_secs(1)),
                Ok(NetworkCommand::AcknowledgeMlsUiDispatch {
                    room_id: RoomId(1),
                    sequence: 7,
                })
            )
        });
        assert!(acknowledged);
    }

    #[test]
    fn video_command_pushes_diagnostics_notice() {
        let mut app = test_app();
        app.room
            .screencast_status
            .fail("screen capture output is not Annex-B video".to_string());
        app.view.composer.set_lines("/video");

        app.submit_input();

        assert_eq!(app.view.active.chat.len(), 1);
        let notice = app.view.active.chat.local_record(0).unwrap();
        assert_eq!(notice.sender, "video");
        assert!(notice.body.contains("state: failed"));
        assert!(notice.body.contains("last issue:"));
        assert_eq!(
            notice.notice_kind,
            Some(crate::chat_buffer::NoticeKind::Error)
        );
        assert!(app.view.status.text().contains("video failed:"));
    }

    #[test]
    fn screencast_start_without_voice_fails_before_spawning_capture() {
        let mut app = test_app();

        app.handle_screencast_command(local_control::ScreencastCommand::Start {
            argv: Vec::new(),
            hevc: false,
        });

        assert!(app.screencast.is_none());
        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Failed);
        assert_eq!(
            app.room
                .screencast_status
                .last_issue
                .as_ref()
                .map(|issue| issue.reason.as_str()),
            Some("join a voice call before sharing")
        );
        assert_eq!(app.view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn screencast_start_replaces_the_active_share() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.room.voice_room = Some(RoomId(1));
        app.video_transport = Some(crate::video::VideoTransport::new(
            "127.0.0.1:1".parse().unwrap(),
            rpc::crypto::TransportMode::Encrypted,
            [0u8; rpc::crypto::KEY_LEN],
        ));
        app.screencast = Some(crate::video::ScreencastHandle::for_test(ShareAttemptId(1)));
        let old_stream_id = StreamId(7);
        app.screencast_stream_id = Some(old_stream_id);
        app.room
            .screencast_status
            .live(old_stream_id, "h264".to_string(), 1280, 720);
        let missing = format!(
            "/tmp/chatt-missing-replacement-video-command-{}",
            std::process::id()
        );

        app.handle_screencast_command(local_control::ScreencastCommand::Start {
            argv: vec![missing.clone()],
            hevc: false,
        });

        assert!(app.screencast.is_none());
        assert_eq!(app.screencast_stream_id, None);
        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Failed);
        assert!(
            app.room
                .screencast_status
                .last_issue
                .as_ref()
                .is_some_and(|issue| issue.reason.contains(&missing)),
            "the replacement capture command should have been attempted"
        );
        match rx.try_recv().expect("old share stop command") {
            NetworkCommand::StopShare { stream_id } => assert_eq!(stream_id, old_stream_id),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn screencast_toggle_stops_an_active_share_and_ignores_the_command() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.screencast = Some(crate::video::ScreencastHandle::for_test(ShareAttemptId(1)));
        let stream_id = StreamId(8);
        app.screencast_stream_id = Some(stream_id);
        app.room
            .screencast_status
            .live(stream_id, "h264".to_string(), 1280, 720);

        app.handle_screencast_command(local_control::ScreencastCommand::Toggle {
            argv: vec!["/command/must/not/run".to_string()],
            hevc: true,
        });

        assert!(app.screencast.is_none());
        assert_eq!(app.screencast_stream_id, None);
        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Off);
        assert_eq!(app.view.status.text(), "video off");
        match rx.try_recv().expect("share stop command") {
            NetworkCommand::StopShare { stream_id: stopped } => assert_eq!(stopped, stream_id),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn screencast_toggle_without_an_active_share_behaves_like_start() {
        let mut app = test_app();

        app.handle_screencast_command(local_control::ScreencastCommand::Toggle {
            argv: Vec::new(),
            hevc: false,
        });

        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Failed);
        assert_eq!(
            app.room
                .screencast_status
                .last_issue
                .as_ref()
                .map(|issue| issue.reason.as_str()),
            Some("join a voice call before sharing")
        );
    }

    #[test]
    fn share_start_rejection_tears_down_local_screencast() {
        let mut app = test_app();
        let attempt_id = ShareAttemptId(7);
        app.screencast = Some(crate::video::ScreencastHandle::for_test(attempt_id));
        app.room.screencast_status.start();

        app.handle_network_event(NetworkEvent::ShareStartRejected {
            attempt_id,
            message: "join the room's voice call before sharing".to_string(),
        });

        assert!(app.screencast.is_none());
        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Failed);
        assert_eq!(
            app.room
                .screencast_status
                .last_issue
                .as_ref()
                .map(|issue| issue.reason.as_str()),
            Some("join the room's voice call before sharing")
        );
    }

    #[test]
    fn stale_share_attempt_events_do_not_mutate_the_current_capture() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.session_id = Some(SessionId(1));
        app.room.voice_room = Some(RoomId(1));
        let current_attempt = ShareAttemptId(2);
        app.screencast = Some(crate::video::ScreencastHandle::for_test(current_attempt));
        app.screencast_stream_id = Some(StreamId(20));
        app.room
            .screencast_status
            .live(StreamId(20), "h264".to_string(), 1280, 720);

        app.handle_network_event(NetworkEvent::ShareStarted {
            attempt_id: ShareAttemptId(1),
            room_id: RoomId(1),
            stream_id: StreamId(10),
            publish_secret: vec![7; 32],
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            extradata: Vec::new(),
        });
        assert!(matches!(
            rx.try_recv(),
            Ok(NetworkCommand::StopShare {
                stream_id: StreamId(10)
            })
        ));

        app.handle_network_event(NetworkEvent::ShareStartRejected {
            attempt_id: ShareAttemptId(1),
            message: "stale rejection".to_string(),
        });
        app.handle_screencast_failed(ShareAttemptId(1), "stale publisher failure".to_string());
        app.handle_screencast_progress(ScreencastProgress {
            attempt_id: ShareAttemptId(1),
            stream_id: StreamId(20),
            total_bytes: 99,
            total_frames: 9,
            rolling_bytes_per_sec: 33,
        });

        assert_eq!(
            app.screencast.as_ref().map(|handle| handle.attempt_id()),
            Some(current_attempt)
        );
        assert_eq!(app.screencast_stream_id, Some(StreamId(20)));
        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Live);
        assert_eq!(app.room.screencast_status.total_bytes, 0);
        assert_eq!(app.room.screencast_status.total_frames, 0);
        assert!(app.room.screencast_status.last_issue.is_none());
    }

    #[test]
    fn stats_command_toggles_lobby_details() {
        let mut app = test_app();
        assert!(!app.view.lobby_details);

        app.view.composer.set_lines("/stats");
        app.submit_input();
        assert!(app.view.lobby_details);
        assert_eq!(
            app.view.status.text(),
            "lobby detail on (jitter buffer stats)"
        );

        app.view.composer.set_lines("/stats");
        app.submit_input();
        assert!(!app.view.lobby_details);
        assert_eq!(
            app.view.status.text(),
            "lobby detail off (latency estimate)"
        );
    }

    #[test]
    fn volume_dialog_pushes_and_restores_focus() {
        let mut app = test_app();
        app.room.server_alias = "local".to_string();
        app.user_id = Some(UserId(1));
        enter_room_with_users(
            &mut app,
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
        );
        observe_room_voice(&mut app, UserId(1), 1);
        observe_room_voice(&mut app, UserId(2), 2);
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
        let mut config = Config::default();
        config.ui.default_bindings = crate::config::DefaultBindings::Vim;
        let mut app = TestApp::new(config, None).expect("test app");
        let mut room = RoomMode::default();
        room.process_input(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);

        room.process_input(
            &mut app,
            KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty()),
        );

        assert!(app.local_voice_state().is_muted());
        assert_eq!(room.focus(), ChatPanelFocus::Compose);
        assert_eq!(app.view.composer.mode(), EditorMode::Normal);
    }

    #[test]
    fn selected_user_volume_requires_lobby_focus() {
        let mut app = test_app();
        app.room.server_alias = "local".to_string();
        app.user_id = Some(UserId(1));
        enter_room_with_users(
            &mut app,
            vec![
                user_summary(UserId(1), "alice"),
                user_summary(UserId(2), "bob"),
            ],
        );
        observe_room_voice(&mut app, UserId(1), 1);
        observe_room_voice(&mut app, UserId(2), 2);
        let participants = app.room.participant_snapshot(app.view.viewed_room);
        app.view
            .move_participant_selection(&participants.entries, 1, 10);

        let mut h = Harness::new(app);
        let mut chat_room = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        let action = chat_room.process_input(
            &mut h.app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(action, Action::Continue);
        h.apply();
        assert_eq!(h.stack.len(), 1);
        assert_eq!(h.app.view.status.text(), "focus lobby to adjust users");

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
    fn delete_message_confirmation_gates_oldest_first_multi_delete() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.room.server_alias = "local".to_string();
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);
        let user_id = app.user_id;
        for (id, sender) in [(1, UserId(1)), (2, UserId(2)), (3, UserId(1))] {
            app.room.chat_received(
                rpc::control::ChatMessage {
                    message_id: MessageId(id),
                    room_id: RoomId(1),
                    sender,
                    sender_name: format!("user{}", sender.0),
                    timestamp_ms: id * 1_000,
                    body: format!("message {id}"),
                    file_transfer_id: None,
                    flags: rpc::control::MessageFlags::default(),
                    target: None,
                },
                user_id,
            );
        }

        let (core, view) = app.parts_mut();
        view.sync_independent(&core.room);
        let mut mode = RoomMode::with_focus(ChatPanelFocus::ChatLog);
        mode.render(&mut app, &mut Buffer::new(80, 24), 0);
        app.view
            .active
            .chat
            .set_cursor(crate::chat_buffer::HistoryEntryId::Message(MessageId(1)));
        let (core, view) = app.parts_mut();
        let history = core.room.history_ref(RoomId(1)).unwrap();
        assert!(view.active.chat.toggle_visual_anchor(&history, 80));
        view.active.chat.move_cursor_line(&history, 2, 80);
        let stack = crate::tui::mode_stack::ModeStack::new(Box::new(mode), &mut app);
        let mut h = Harness { app, stack };

        h.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
        assert!(h.overlay_active());
        assert!(rx.try_recv().is_err(), "opening the modal must not delete");
        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        assert!(h.app.view.active.chat.has_visual());
        assert!(rx.try_recv().is_err(), "canceling must not delete");

        h.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
        h.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()));
        for expected in [MessageId(1), MessageId(3)] {
            match rx.try_recv().expect("delete command") {
                NetworkCommand::DeleteChat { room_id, target } => {
                    assert_eq!(room_id, RoomId(1));
                    assert_eq!(target, expected);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
        assert!(rx.try_recv().is_err());
        assert!(!h.app.view.active.chat.has_visual());
        assert_eq!(
            h.app.view.active.chat.len(),
            3,
            "deletion waits for server echo"
        );
        assert_eq!(h.app.view.status.text(), "deleting 2 messages");
    }

    #[test]
    fn writable_web_mutations_route_through_current_room_validation() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);
        let user_id = app.user_id;
        app.room.chat_received(
            rpc::control::ChatMessage {
                message_id: MessageId(7),
                room_id: RoomId(1),
                sender: UserId(1),
                sender_name: "alice".to_string(),
                timestamp_ms: 7_000,
                body: "original".to_string(),
                file_transfer_id: None,
                flags: rpc::control::MessageFlags::default(),
                target: None,
            },
            user_id,
        );
        let (core, view) = app.parts_mut();
        view.sync_independent(&core.room);

        app.handle_web_request(crate::web_server::WebRequest::EditChat {
            client: 1,
            request_id: 1,
            target: 7,
            body: "revised".to_string(),
        });
        match rx.try_recv().unwrap() {
            NetworkCommand::EditChat {
                room_id: RoomId(1),
                target: MessageId(7),
                body,
            } => assert_eq!(body, "revised"),
            other => panic!("unexpected command: {other:?}"),
        }

        app.handle_web_request(crate::web_server::WebRequest::DeleteChat {
            client: 1,
            request_id: 2,
            target: 7,
        });
        match rx.try_recv().unwrap() {
            NetworkCommand::DeleteChat {
                room_id: RoomId(1),
                target: MessageId(7),
            } => {}
            other => panic!("unexpected command: {other:?}"),
        }

        app.pending_web_deletes.insert((RoomId(1), MessageId(7)));
        app.handle_network_event(NetworkEvent::ChatMutationRejected {
            room_id: RoomId(1),
            target: MessageId(7),
            kind: ChatMutationKind::Delete,
            message: "message is too old".to_string(),
        });
        assert!(!app.pending_web_deletes.contains(&(RoomId(1), MessageId(7))));
        assert_eq!(app.view.status.text(), "message is too old");
    }

    #[test]
    fn web_command_rejects_tui_only_and_unknown_commands() {
        let mut app = test_app();
        assert_eq!(
            app.run_web_command_captured("/clear".to_string()),
            Err("/clear is not available from the web view".to_string())
        );
        assert_eq!(
            app.run_web_command_captured("/nope".to_string()),
            Err("unknown command: /nope".to_string())
        );
        assert!(app.frontend_command_capture.is_none());
    }

    #[test]
    fn web_command_captures_status_output() {
        let mut app = test_app();
        let lines = app
            .run_web_command_captured("/whoami".to_string())
            .expect("/whoami passes the web gate");
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].error);
        assert!(
            lines[0].text.contains("connecting as"),
            "unexpected output {:?}",
            lines[0].text
        );
        assert!(
            app.frontend_command_capture.is_none(),
            "capture must not outlive the command"
        );
    }

    #[test]
    fn web_command_error_lines_are_marked() {
        let mut app = test_app();
        let lines = app
            .run_web_command_captured("/room nope".to_string())
            .expect("gating passes; the failure is command-internal");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].error);
        assert_eq!(lines[0].text, "no room named nope");
    }

    #[test]
    fn mutation_rejection_is_delivered_to_the_requesting_terminal() {
        let mut app = test_app();
        let (network_tx, network_rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(network_tx));
        enter_test_room(&mut app);
        let owner = crate::client_channel::ClientId(77);
        let view = attach_test_client(&mut app, owner);
        app.handle_client_command(
            owner,
            command::CoreCommand::DeleteMessages {
                room_id: RoomId(1),
                targets: vec![MessageId(9)],
                skipped: 0,
            },
        );
        assert!(matches!(
            network_rx.try_recv(),
            Ok(NetworkCommand::DeleteChat {
                room_id: RoomId(1),
                target: MessageId(9),
            })
        ));
        app.set_status("primary steady");

        app.handle_network_event(NetworkEvent::ChatMutationRejected {
            room_id: RoomId(1),
            target: MessageId(9),
            kind: ChatMutationKind::Delete,
            message: "message is too old".to_string(),
        });

        assert_eq!(app.view.status.text(), "primary steady");
        let view = view.lock();
        assert_eq!(view.status.text(), "message is too old");
        assert_eq!(view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn server_catalog_rebuild_tracks_generation() {
        let mut app = test_app();
        let initial_generation = app.view.server_catalog.generation();
        let initial_daemon_generation = app.daemon_config_generation;
        app.config.servers.push(crate::config::ServerEntry {
            label: "s1".to_string(),
            ..Default::default()
        });

        app.rebuild_server_items();

        assert_eq!(app.server_items().len(), 1);
        assert_eq!(
            app.view.server_catalog.generation(),
            initial_generation.saturating_add(1)
        );
        assert_eq!(
            app.daemon_config_generation,
            initial_daemon_generation.wrapping_add(1)
        );

        app.rebuild_server_items();

        assert_eq!(
            app.daemon_config_generation,
            initial_daemon_generation.wrapping_add(1)
        );
    }

    #[test]
    fn daemon_config_sync_is_generation_gated() {
        let mut app = test_app();
        let remote = attach_test_client(&mut app, crate::client_channel::ClientId(7));

        app.sync_daemon_config_if_changed();
        assert_eq!(
            app.synced_daemon_config_generation,
            app.daemon_config_generation
        );
        let idle_generation = app.synced_daemon_config_generation;

        app.sync_daemon_config_if_changed();
        assert_eq!(app.synced_daemon_config_generation, idle_generation);

        app.apply_theme(ThemeSelection::Builtin(config::ThemeChoice::Base16Light));
        assert_ne!(
            app.synced_daemon_config_generation,
            app.daemon_config_generation
        );
        let expected_theme = app.view.theme;

        app.sync_daemon_config_if_changed();

        assert_eq!(remote.lock().theme, expected_theme);
        assert_eq!(
            app.synced_daemon_config_generation,
            app.daemon_config_generation
        );
    }

    #[test]
    fn daemon_config_sync_preserves_theme_changed_by_secondary_client() {
        let mut app = test_app();
        let client_id = crate::client_channel::ClientId(7);
        let remote = attach_test_client(&mut app, client_id);
        let selection = ThemeSelection::Builtin(config::ThemeChoice::Base16Light);

        app.apply_theme_as(client_id, selection);
        let expected_theme = app.config.ui.resolve_theme();

        assert_ne!(app.view.theme, expected_theme);
        assert_eq!(remote.lock().theme, expected_theme);

        app.sync_daemon_config_if_changed();

        assert_eq!(app.view.theme, expected_theme);
        assert_eq!(remote.lock().theme, expected_theme);
    }

    #[test]
    fn welcome_theme_preview_still_advances_daemon_config_generation() {
        let mut app = test_app();
        let remote = attach_test_client(&mut app, crate::client_channel::ClientId(7));
        let path = std::env::temp_dir().join(format!(
            "chatt-welcome-theme-preview-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        app.config.config_path = Some(path.clone());
        let mut draft = WelcomeDraft::privacy_first();
        draft.theme = ThemeSelection::Builtin(config::ThemeChoice::Base16Light);
        let preview = crate::theme::Theme::resolve(&draft.theme, &app.config.ui.themes);
        app.view.apply_theme(preview);
        let previous_generation = app.daemon_config_generation;

        assert!(app.save_welcome(&draft));
        assert_eq!(
            app.daemon_config_generation,
            previous_generation.wrapping_add(1)
        );

        app.sync_daemon_config_if_changed();

        assert_eq!(remote.lock().theme, preview);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn toggle_mute_while_deafened_undeafens_and_unmutes() {
        let mut app = test_app();
        app.set_voice_state(VoiceState::Deafened);
        assert!(app.local_voice_state().is_deafened());
        assert!(app.local_voice_state().is_muted());

        app.process_global_command(BindCommand::ToggleMute);

        assert!(!app.local_voice_state().is_deafened());
        assert!(!app.local_voice_state().is_muted());
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
        app.room.local_username = "alice".to_string();
        app.room.room_name = "lobby".to_string();

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let expected_chat_top = 1 + app.config.ui.room_height + 1;
        let composer_frame_rows = if app.config.ui.composer_padding { 2 } else { 0 };
        let expected_chat_bottom = buffer.height() - 4 - composer_frame_rows;
        assert_eq!(room.layout().chat_rect.y, expected_chat_top);
        assert_eq!(room.layout().chat_rect.bottom(), expected_chat_bottom);
    }

    #[test]
    fn chat_notice_markers_use_notice_kind_accent() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.view
            .push_local_notice("system", "joined", crate::chat_buffer::NoticeKind::Info);
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let info_marker = cell_style(
            &mut buffer,
            room.layout().chat_rect.x,
            room.layout().chat_rect.y,
        );
        assert_eq!(info_marker.fg(), app.view.theme.muted.fg());

        let mut app = test_app();
        let mut room = RoomMode::default();
        app.view
            .push_local_notice("video", "failed", crate::chat_buffer::NoticeKind::Error);
        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let error_marker = cell_style(
            &mut buffer,
            room.layout().chat_rect.x,
            room.layout().chat_rect.y,
        );
        assert_eq!(error_marker.fg(), app.view.theme.error.fg());
    }

    fn click_top_bar_rect(app: &mut TestApp, room: &mut RoomMode, rect: extui::Rect) {
        assert!(!rect.is_empty());
        room.process_mouse(
            app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );
    }

    #[test]
    fn top_bar_voice_buttons_select_exclusive_modes() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let live_rect = app.view.chrome.top_bar.live;
        let mute_rect = app.view.chrome.top_bar.mute;
        let deafen_rect = app.view.chrome.top_bar.deafen;
        assert!(!live_rect.is_empty());
        assert!(!mute_rect.is_empty());
        assert!(!deafen_rect.is_empty());

        click_top_bar_rect(&mut app, &mut room, mute_rect);
        assert_eq!(app.view.local_voice_state(), VoiceState::Muted);
        assert!(app.local_voice_state().is_muted());
        assert!(!app.local_voice_state().is_deafened());

        click_top_bar_rect(&mut app, &mut room, mute_rect);
        assert_eq!(app.view.local_voice_state(), VoiceState::Live);
        assert!(!app.local_voice_state().is_muted());
        assert!(!app.local_voice_state().is_deafened());

        click_top_bar_rect(&mut app, &mut room, deafen_rect);
        assert_eq!(app.view.local_voice_state(), VoiceState::Deafened);
        assert!(app.local_voice_state().is_muted());
        assert!(app.local_voice_state().is_deafened());

        click_top_bar_rect(&mut app, &mut room, mute_rect);
        assert_eq!(app.view.local_voice_state(), VoiceState::Muted);
        assert!(app.local_voice_state().is_muted());
        assert!(!app.local_voice_state().is_deafened());

        click_top_bar_rect(&mut app, &mut room, live_rect);
        assert_eq!(app.view.local_voice_state(), VoiceState::Live);
        assert!(!app.local_voice_state().is_muted());
        assert!(!app.local_voice_state().is_deafened());

        click_top_bar_rect(&mut app, &mut room, deafen_rect);
        assert_eq!(app.view.local_voice_state(), VoiceState::Deafened);

        click_top_bar_rect(&mut app, &mut room, deafen_rect);
        assert_eq!(app.view.local_voice_state(), VoiceState::Live);
        assert!(!app.local_voice_state().is_muted());
        assert!(!app.local_voice_state().is_deafened());
    }

    #[test]
    fn live_video_badge_stops_to_warn_backed_off_state() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.screencast = Some(crate::video::ScreencastHandle::for_test(ShareAttemptId(1)));
        app.cached_screencast_start = Some(CachedScreencastStart {
            argv: vec!["capture".to_string()],
            hevc: false,
        });
        let stream_id = StreamId(7);
        app.screencast_stream_id = Some(stream_id);
        app.room
            .screencast_status
            .live(stream_id, "h264".to_string(), 1280, 720);

        let mut buffer = Buffer::new(100, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let video_rect = app.view.chrome.top_bar.video;

        click_top_bar_rect(&mut app, &mut room, video_rect);

        assert!(app.screencast.is_none());
        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Off);
        assert_eq!(app.view.status.text(), "video off");
        match rx.try_recv().expect("stop share command") {
            NetworkCommand::StopShare { stream_id: stopped } => assert_eq!(stopped, stream_id),
            other => panic!("unexpected command: {other:?}"),
        }

        let mut buffer = Buffer::new(100, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let off_rect = app.view.chrome.top_bar.video;
        assert!(!off_rect.is_empty());
        assert_eq!(rect_text(&mut buffer, off_rect), " VIDEO OFF ");
        let style = cell_style(&mut buffer, off_rect.x, off_rect.y);
        assert_eq!(style.bg(), app.view.theme.warn.fg());
        assert_eq!(style.fg(), app.view.theme.mode_server_edit.fg());
    }

    #[test]
    fn off_video_badge_restarts_cached_command() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let (tx, _rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.room.voice_room = Some(RoomId(1));
        app.video_transport = Some(crate::video::VideoTransport::new(
            "127.0.0.1:1".parse().unwrap(),
            rpc::crypto::TransportMode::Encrypted,
            [0u8; rpc::crypto::KEY_LEN],
        ));
        let missing = format!(
            "/tmp/chatt-missing-cached-video-command-{}",
            std::process::id()
        );
        app.cached_screencast_start = Some(CachedScreencastStart {
            argv: vec![missing.clone()],
            hevc: false,
        });
        app.room.screencast_status.turn_off();

        let mut buffer = Buffer::new(100, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let off_rect = app.view.chrome.top_bar.video;

        click_top_bar_rect(&mut app, &mut room, off_rect);

        assert_eq!(app.room.screencast_status.phase, ScreencastPhase::Failed);
        assert!(
            app.room
                .screencast_status
                .last_issue
                .as_ref()
                .is_some_and(|issue| issue.reason.contains(&missing)),
            "restart should use the cached command"
        );
        assert_eq!(app.view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn failed_video_badge_opens_video_diagnostics_on_click() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        app.room
            .screencast_status
            .fail("screen publish failed: connection reset".to_string());

        let mut buffer = Buffer::new(100, 24);
        render_room(&mut app, &mut room, &mut buffer);

        let video_rect = app.view.chrome.top_bar.video;
        assert!(!video_rect.is_empty());
        room.process_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: video_rect.x,
                row: video_rect.y,
                modifiers: KeyModifiers::empty(),
            },
        );

        assert_eq!(app.view.active.chat.len(), 1);
        let notice = app.view.active.chat.local_record(0).unwrap();
        assert_eq!(notice.sender, "video");
        assert!(notice.body.contains("connection reset"));
    }

    #[test]
    fn call_bar_shows_only_audio_errors_and_call_action() {
        let mut app = test_app();
        let mut room = RoomMode::default();

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let bar = room.layout().lobby_bar_rect;
        let text = rect_text(&mut buffer, bar);
        assert!(text.contains("Call"));
        assert!(text.contains("JOIN"));
        assert!(!text.contains("Lobby"));
        assert!(!text.contains("in call"));
        assert!(!text.contains("voice:"));
        assert_eq!(
            rect_text(
                &mut buffer,
                Rect {
                    x: room.layout().room_list_rect.x,
                    y: bar.y,
                    w: room.layout().room_list_rect.w,
                    h: 1,
                },
            )
            .trim(),
            "Rooms"
        );
        let join_button = app.view.chrome.lobby_bar.call_button;
        let join_style = cell_style(&mut buffer, join_button.x, join_button.y);
        assert_eq!(join_style.bg(), app.view.theme.good.fg());
        assert_eq!(join_style.fg(), app.view.theme.mode_server_edit.fg());
        assert!(app.view.chrome.lobby_bar.audio_widget.is_empty());
        assert!(app.view.chrome.lobby_bar.audio_reset.is_empty());

        app.supervisor.capture.on_rebuild_failed(
            Instant::now(),
            AudioErrorKind::DeviceGone,
            "device unplugged".to_string(),
        );
        render_room(&mut app, &mut room, &mut buffer);
        let reset_rect = app.view.chrome.lobby_bar.audio_reset;
        assert!(!reset_rect.is_empty());
        assert!(!app.view.chrome.lobby_bar.audio_widget.is_empty());
        assert!(rect_text(&mut buffer, bar).contains("JOIN"));

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
        assert!(app.view.chrome.lobby_bar.audio_widget.is_empty());
        assert!(app.view.chrome.lobby_bar.audio_reset.is_empty());

        app.room.voice_room = Some(RoomId(1));
        render_room(&mut app, &mut room, &mut buffer);
        assert!(rect_text(&mut buffer, bar).contains("LEAVE"));
        let leave_button = app.view.chrome.lobby_bar.call_button;
        let leave_style = cell_style(&mut buffer, leave_button.x, leave_button.y);
        assert_eq!(leave_style.bg(), app.view.theme.status_section.bg());
        assert_eq!(leave_style.fg(), app.view.theme.muted.fg());
    }

    #[test]
    fn call_bar_button_joins_and_leaves_voice() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.user_id = Some(UserId(1));
        enter_test_room(&mut app);
        app.voice_state
            .store(VoiceState::Deafened, Ordering::Relaxed);

        let mut buffer = Buffer::new(80, 24);
        render_room(&mut app, &mut room, &mut buffer);
        let join = app.view.chrome.lobby_bar.call_button;
        click_top_bar_rect(&mut app, &mut room, join);
        assert_eq!(app.requested_voice_room, Some(RoomId(1)));
        assert!(
            rx.try_iter()
                .any(|command| matches!(command, NetworkCommand::JoinVoice(RoomId(1))))
        );

        app.requested_voice_room = None;
        app.room.voice_room = Some(RoomId(1));
        render_room(&mut app, &mut room, &mut buffer);
        let leave = app.view.chrome.lobby_bar.call_button;
        click_top_bar_rect(&mut app, &mut room, leave);
        assert!(
            rx.try_iter()
                .any(|command| matches!(command, NetworkCommand::LeaveVoice))
        );
    }

    #[test]
    fn inactive_mode_headers_use_status_section_background() {
        let mut app = test_app();
        let mut room = RoomMode::default();
        let mut buffer = Buffer::new(80, 24);

        render_room(&mut app, &mut room, &mut buffer);

        let call_header = cell_style(
            &mut buffer,
            room.layout().user_list_rect.x,
            room.layout().lobby_bar_rect.y,
        );
        assert_eq!(call_header.bg(), app.view.theme.status_section.bg());
        assert_ne!(call_header.bg(), app.view.theme.status_fill.bg());
    }

    fn app_with_servers(entries: &[(&str, &str)]) -> TestApp {
        let mut app = test_app();
        app.config.servers.clear();
        for (label, tcp_addr) in entries {
            app.config.servers.push(ServerEntry {
                id: test_server_id(label),
                label: label.to_string(),
                tcp_addr: tcp_addr.to_string(),
                udp_addr: String::new(),
                udp_probe_addr: None,
                username: "Zoe".to_string(),
                token: "tct1_existing-token".to_string(),
                server_public_key: String::new(),
                ..ServerEntry::default()
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

    /// A saved server spelled differently is still that server, so joining it
    /// connects instead of pairing a duplicate entry.
    #[test]
    fn join_equivalent_address_spelling_connects_to_the_saved_server() {
        let app = app_with_servers(&[
            ("prod", "HOST.example:4000"),
            ("six", "[0:0:0:0:0:0:0:1]:4"),
        ]);
        assert_eq!(
            app.resolve_join("host.example.:4000"),
            JoinResolution::Connect("prod".to_string())
        );
        assert_eq!(
            app.resolve_join("[::1]:4"),
            JoinResolution::Connect("six".to_string())
        );
    }

    /// Two profiles against one server are both plausible targets, so the
    /// address they share picks neither.
    #[test]
    fn join_address_shared_by_two_spellings_opens_filtered_picker() {
        let app = app_with_servers(&[
            ("work-a", "[::1]:4000"),
            ("work-b", "[0:0:0:0:0:0:0:1]:4000"),
        ]);
        assert_eq!(app.resolve_join("[::0:1]:4000"), JoinResolution::Filter);
    }

    /// A `chatt join` that reaches a running master resolves exactly as it
    /// would have at startup, and reports to the terminal that asked.
    #[test]
    fn attach_intent_runs_the_join_on_the_attaching_terminal() {
        let mut app = app_with_servers(&[("lab", "10.0.0.1:4000")]);
        let client = crate::client_channel::ClientId(7);
        let view = attach_test_client(&mut app, client);

        app.start_attach_intent(
            client,
            &StartupIntent::Named {
                specifier: "does-not-exist".to_string(),
            },
        );
        app.sync_terminal_events();

        let view = view.lock();
        assert_eq!(view.status.kind(), StatusKind::Error);
        assert!(view.status.text().contains("does-not-exist"));
        assert_eq!(
            app.view.status.kind(),
            StatusKind::Info,
            "the primary terminal did not ask for this join: {}",
            app.view.status.text()
        );
    }

    /// A join naming the server this master already holds only wants to look at
    /// it: reconnecting would drop the session for every other terminal.
    #[test]
    fn join_to_the_active_server_does_not_restart_the_connection() {
        let mut app = app_with_servers(&[("lab", "10.0.0.1:4000")]);
        let (tx, rx) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.room.active_server_id = Some(test_server_id("lab"));

        app.start_named_join("lab".to_string());

        assert!(!app.has_pending_join());
        assert!(rx.try_recv().is_err(), "the live worker was left alone");
        assert_eq!(app.view.status.kind(), StatusKind::Info);
        assert!(app.view.status.text().contains("already connected"));
    }

    /// An address names one server exactly. Pairing with it must not be
    /// diverted by a saved address it is merely a substring of.
    #[test]
    fn join_address_is_not_shadowed_by_a_longer_saved_address() {
        let app = app_with_servers(&[("neighbor", "myexample.com:4000")]);
        assert_eq!(
            app.resolve_join("example.com:4000"),
            JoinResolution::Pair("example.com:4000".to_string())
        );
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
        let path =
            std::env::temp_dir().join(format!("chatt-pair-recovery-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        app.config.config_path = Some(path.clone());
        assert_eq!(
            app.resolve_join("192.168.0.1:4000"),
            JoinResolution::Pair("192.168.0.1:4000".to_string())
        );
        app.start_named_join("192.168.0.1:4000".to_string());
        let pending = app.pairing_pending().expect("pairing started");
        assert_eq!(pending.completion, PairCompletion::OpenEditor);
        let recovery_token = pending.open.clone().expect("recovery secret");
        assert!(recovery_token.starts_with(rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX));
        assert!(app.config.server(&pending.server.label).is_err());
        assert!(!path.exists(), "pairing state was not written to config");
        assert!(app.room.join_notice.is_some());
        let _ = std::fs::remove_file(path);
    }

    /// An awaiting-username pending pair, as the coordinator parks it when the
    /// server rejects the paired name.
    fn awaiting_username_pair(app: &mut TestApp, label: &str) -> ServerEntry {
        let recovery_token = format!("{}retry-secret", rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX);
        let server = ServerEntry {
            id: test_server_id(label),
            label: label.to_string(),
            tcp_addr: "127.0.0.1:1".to_string(),
            username: "User".to_string(),
            token: recovery_token.clone(),
            ..ServerEntry::default()
        };
        app.pairing.set_awaiting_username_for_test(
            crate::client_channel::ClientId::PRIMARY,
            PendingPair {
                server: server.clone(),
                open: Some(recovery_token),
                open_password: String::new(),
                pairing_code: None,
                completion: PairCompletion::OpenEditor,
            },
        );
        server
    }

    #[test]
    fn username_retry_uses_the_full_editor_without_persisting_the_candidate() {
        let mut app = test_app();
        let path =
            std::env::temp_dir().join(format!("chatt-username-retry-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        app.config.config_path = Some(path.clone());
        let server = awaiting_username_pair(&mut app, "public");
        let mut draft = ServerEditDraft::from_new_server(server, &app.config);
        type_into_draft(&mut draft, 1, "Different User");

        app.handle_client_command(
            crate::client_channel::ClientId::PRIMARY,
            CoreCommand::SubmitServerEdit {
                request_id: 1,
                draft,
                join: true,
            },
        );

        let pending = app.pairing_pending().expect("retry running");
        assert_eq!(pending.server.username, "Different User");
        assert_eq!(
            pending.completion,
            PairCompletion::Submit {
                request_id: 1,
                join: true,
            }
        );
        assert!(app.config.servers.is_empty(), "no ready entry yet");
        assert!(!path.exists(), "retry state was not written to config");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn repeated_pairing_username_rejection_stays_in_the_focused_editor() {
        let mut app = test_app();
        let server = awaiting_username_pair(&mut app, "public");
        let draft = ServerEditDraft::from_new_server(server, &app.config);
        app.handle_client_command(
            crate::client_channel::ClientId::PRIMARY,
            CoreCommand::SubmitServerEdit {
                request_id: 9,
                draft,
                join: true,
            },
        );
        let attempt = app
            .pairing
            .running_attempt_for_test()
            .expect("pairing retry running");
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::UsernameTaken {
                message: "username already in use".to_string(),
                server_public_key: "ab".repeat(32),
            },
        });

        let events = app.terminal_channel().drain_events();
        assert!(navigations(&events).is_empty());
        let draft = events
            .iter()
            .find_map(|event| match event {
                TerminalEvent::ServerEditResult {
                    request_id: 9,
                    outcome: ServerEditOutcome::Retry(draft),
                } => Some(draft),
                _ => None,
            })
            .expect("the pairing retry returned to its editor");
        assert!(draft.field_focused_for_test("Username"));
        assert!(draft.username_error_for_test().is_some());
    }

    /// Deterministic per-label id, so fixtures pushing several entries never
    /// collide on the identity that config validation now enforces.
    fn test_server_id(label: &str) -> ServerId {
        let mut bytes = [0x5a; 16];
        for (slot, byte) in bytes.iter_mut().zip(label.bytes()) {
            *slot = byte;
        }
        ServerId(bytes)
    }

    fn saved_server(label: &str, token: &str) -> ServerEntry {
        ServerEntry {
            id: test_server_id(label),
            label: label.to_string(),
            tcp_addr: "127.0.0.1:1".to_string(),
            username: "User".to_string(),
            token: token.to_string(),
            ..ServerEntry::default()
        }
    }

    /// Types `text` into the draft's text field `steps` rows below the label the
    /// form opens on. The form has to be driven once before focus can move: a
    /// draft that has never been laid out has no field order to walk.
    fn type_into_draft(draft: &mut ServerEditDraft, steps: usize, text: &str) {
        draft.active_editor_address().expect("a focused text field");
        for _ in 0..steps {
            draft.move_focus_for_test(1);
        }
        draft.active_editor_address().expect("a focused text field");
        draft.set_active_editor_text(text);
    }

    fn save_draft(
        app: &mut TestApp,
        client: crate::client_channel::ClientId,
        draft: ServerEditDraft,
    ) {
        app.handle_client_command(
            client,
            CoreCommand::SubmitServerEdit {
                request_id: 1,
                draft,
                join: false,
            },
        );
    }

    /// The label of the reloaded draft a conflicted or join-refused submit
    /// answered with, which the submitting editor presents in place.
    fn reopened_editor_label(events: &VecDeque<TerminalEvent>) -> Option<&str> {
        events.iter().find_map(|event| match event {
            TerminalEvent::ServerEditResult {
                outcome:
                    ServerEditOutcome::Conflict(draft) | ServerEditOutcome::SavedButJoinFailed(draft),
                ..
            } => Some(draft.original_label()),
            _ => None,
        })
    }

    #[test]
    fn stale_server_edit_draft_cannot_resurrect_a_renamed_server() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "stale-server-edit");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        let editor = crate::client_channel::ClientId(6);
        let view = attach_test_client(&mut app, editor);
        let channel = app.channel_for(editor).expect("attached channel");
        let stale = ServerEditDraft::from_server(&app.config.servers[0], &app.config);

        let mut rename = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut rename, 0, "community");
        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, rename);
        channel.drain_events();

        save_draft(&mut app, editor, stale);

        assert_eq!(app.config.servers.len(), 1);
        assert_eq!(app.config.servers[0].label, "community");
        assert_eq!(app.config.servers[0].token, "public-token");
        assert_eq!(view.lock().status.kind(), StatusKind::Error);
        assert_eq!(
            reopened_editor_label(&channel.drain_events()),
            Some("community")
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved.matches("[[servers]]").count(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stale_server_edit_draft_cannot_reinsert_a_deleted_server() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "deleted-server-edit");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        let mut stale = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut stale, 1, "Renamed User");

        app.delete_server(test_server_id("public"));
        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, stale);

        assert!(app.config.servers.is_empty());
        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert!(app.terminal_channel().drain_events().iter().any(|event| {
            matches!(
                event,
                TerminalEvent::ServerEditResult {
                    outcome: ServerEditOutcome::Missing,
                    ..
                }
            )
        }));
        let _ = std::fs::remove_file(path);
    }

    /// Two entries may hold the same credential — the token is a value, not an
    /// identity — so an edit of one must land on that one alone.
    #[test]
    fn duplicate_tokens_on_two_servers_remain_independently_editable() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "duplicate-token-edit");
        app.config
            .servers
            .push(saved_server("public", "shared-token"));
        app.config
            .servers
            .push(saved_server("community", "shared-token"));
        let mut draft = ServerEditDraft::from_server(&app.config.servers[1], &app.config);
        type_into_draft(&mut draft, 1, "Renamed User");

        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, draft);

        assert_eq!(app.config.servers[0].username, "User");
        assert_eq!(app.config.servers[1].username, "Renamed User");
        let _ = std::fs::remove_file(path);
    }

    /// DM pins land on the configured entry from the network path, so a draft
    /// opened before one arrived must not carry its empty pin list back.
    #[test]
    fn server_edit_save_keeps_pins_added_while_the_editor_was_open() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "server-edit-pins");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        let mut draft = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut draft, 1, "Renamed User");
        app.config.servers[0]
            .e2e_peer_pins
            .push(crate::config::E2ePeerPin {
                room_id: 0x8000_0001,
                user_id: 2,
                username: "bob".to_string(),
                public_key: "11".repeat(32),
                trust_level: crate::config::E2eTrustLevel::Accepted,
                change_from: None,
                previous: Vec::new(),
            });

        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, draft);

        assert_eq!(app.config.servers[0].username, "Renamed User");
        assert_eq!(app.config.servers[0].e2e_peer_pins.len(), 1);
        let _ = std::fs::remove_file(path);
    }

    /// A re-pair swaps the entry's credential while the form is up. The save
    /// still lands on that entry and adopts the new token rather than writing
    /// the draft's stale one back.
    #[test]
    fn server_edit_save_adopts_a_token_replaced_while_the_editor_was_open() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "server-edit-repair");
        app.config.servers.push(saved_server("public", "old-token"));
        let mut draft = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut draft, 1, "Renamed User");
        app.config.servers[0].token = "new-token".to_string();

        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, draft);

        assert_eq!(app.config.servers.len(), 1);
        assert_eq!(app.config.servers[0].token, "new-token");
        assert_eq!(app.config.servers[0].username, "Renamed User");
        let _ = std::fs::remove_file(path);
    }

    /// A live session on `label`, as a completed connect leaves it. The command
    /// receiver is returned because dropping it would close the worker channel.
    fn connected_session(
        app: &mut TestApp,
        label: &str,
    ) -> mpsc::Receiver<crate::client_net::NetworkCommand> {
        let (tx, commands) = mpsc::channel();
        app.network = Some(NetworkClient::from_parts_for_test(tx));
        app.room.server_alias = label.to_string();
        app.room.active_server_id = Some(
            app.config
                .server(label)
                .map(|server| server.id)
                .unwrap_or_else(|_| test_server_id(label)),
        );
        app.active_network_generation = Some(7);
        app.room.network_selected = true;
        enter_room_with_users(app, Vec::new());
        commands
    }

    /// The same session with one transfer in flight: the state a switch to
    /// another server must refuse to tear down.
    fn connected_with_active_transfer(
        app: &mut TestApp,
        label: &str,
    ) -> mpsc::Receiver<crate::client_net::NetworkCommand> {
        let commands = connected_session(app, label);
        app.room.transfer_progress(
            RoomId(1),
            rpc::ids::FileTransferId(9),
            10,
            100,
            TransferDirection::Outgoing,
        );
        assert!(app.room.has_active_transfers());
        commands
    }

    fn server_labels(app: &TestApp) -> Vec<(String, String)> {
        app.config
            .servers
            .iter()
            .map(|server| (server.label.clone(), server.username.clone()))
            .collect()
    }

    /// Every live reference names a configured entry. The session resolves
    /// servers by id for DM pins, room settings, identity verification and
    /// stale-token repair, so an id the configuration no longer holds breaks
    /// all of them.
    fn assert_live_server_labels_resolve(app: &TestApp) {
        if let Some(server_id) = app.room.active_server_id {
            assert!(
                app.config.server_by_id(server_id).is_some(),
                "active id {server_id} is not configured"
            );
        }
    }

    #[test]
    fn save_and_join_persists_the_edit_but_refuses_to_strand_active_transfers() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "save-and-join-transfer");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        let _commands = connected_with_active_transfer(&mut app, "public");
        let mut draft = ServerEditDraft::from_server(&app.config.servers[1], &app.config);
        type_into_draft(&mut draft, 1, "Renamed User");

        app.handle_client_command(
            crate::client_channel::ClientId::PRIMARY,
            CoreCommand::SubmitServerEdit {
                request_id: 1,
                draft,
                join: true,
            },
        );

        assert_eq!(app.config.servers[1].username, "Renamed User");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Renamed User")
        );
        assert_eq!(app.view.status.text(), SERVER_SWITCH_TRANSFER_BLOCKED);
        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert!(app.network.is_some());
        assert!(app.room.has_active_transfers());
        assert!(!app.has_pending_join(), "nothing was started");
        assert!(app.terminal_channel().drain_events().iter().any(|event| {
            matches!(
                event,
                TerminalEvent::ServerEditResult {
                    outcome: ServerEditOutcome::SavedButJoinFailed(_),
                    ..
                }
            )
        }));
        let _ = std::fs::remove_file(path);
    }

    fn navigations(events: &VecDeque<TerminalEvent>) -> Vec<&NavigationEvent> {
        events
            .iter()
            .filter_map(|event| match event {
                TerminalEvent::Navigation(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    fn authenticated_event() -> NetworkEvent {
        NetworkEvent::Authenticated {
            session_id: SessionId(1),
            user_id: UserId(1),
            rooms: vec![test_room_info(1)],
            users: vec![user_summary(UserId(1), "alice")],
            default_room: RoomId(1),
            dms_enabled: true,
            video_addr: "127.0.0.1:41000".parse().unwrap(),
            video_transport_mode: rpc::crypto::TransportMode::Encrypted,
            video_auth_key: [0; rpc::crypto::KEY_LEN],
        }
    }

    #[test]
    fn authentication_is_the_only_pending_join_transition_to_room() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::Connected,
        });
        assert!(
            navigations(&app.terminal_channel().drain_events()).is_empty(),
            "connecting and authenticating navigate no one"
        );

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: authenticated_event(),
        });

        assert!(!app.has_pending_join());
        assert!(app.network.is_some(), "the candidate was promoted");
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert!(
            navigations(&app.terminal_channel().drain_events())
                .iter()
                .any(|event| matches!(event, NavigationEvent::ResetBase(BaseScreen::Room)))
        );
    }

    #[test]
    fn editor_join_failure_returns_to_the_same_form_without_navigation() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        pending_join(
            &mut app,
            "public",
            JoinOwner::ServerEditor {
                client: crate::client_channel::ClientId::PRIMARY,
                request_id: 17,
            },
        );
        let generation = join_generation(&app);
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::WorkerStopped {
                reason: "connection refused".to_string(),
            },
        });

        assert!(!app.has_pending_join());
        let events = app.terminal_channel().drain_events();
        assert!(navigations(&events).is_empty());
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::ServerEditResult {
                request_id: 17,
                outcome: ServerEditOutcome::SavedButJoinFailed(_),
            }
        )));
    }

    #[test]
    fn editor_join_username_rejection_focuses_username_without_navigation() {
        let mut app = test_app();
        app.config.servers.push(ServerEntry {
            username: "Taken Name".to_string(),
            ..saved_server("public", "public-token")
        });
        pending_join(
            &mut app,
            "public",
            JoinOwner::ServerEditor {
                client: crate::client_channel::ClientId::PRIMARY,
                request_id: 23,
            },
        );
        let generation = join_generation(&app);
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::AuthFailed {
                code: ERROR_USERNAME_TAKEN,
                message: "username already in use".to_string(),
            },
        });

        assert!(!app.has_pending_join());
        let events = app.terminal_channel().drain_events();
        assert!(navigations(&events).is_empty());
        let draft = events
            .iter()
            .find_map(|event| match event {
                TerminalEvent::ServerEditResult {
                    request_id: 23,
                    outcome: ServerEditOutcome::Retry(draft),
                } => Some(draft),
                _ => None,
            })
            .expect("the editor received the rejected server");
        assert!(draft.field_focused_for_test("Username"));
        assert!(draft.username_error_for_test().is_some());
    }

    #[test]
    fn stale_candidate_events_cannot_promote() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation: generation.wrapping_add(1),
            event: authenticated_event(),
        });

        assert!(app.has_pending_join(), "the pending join is untouched");
        assert!(app.network.is_none(), "nothing was promoted");
        assert!(navigations(&app.terminal_channel().drain_events()).is_empty());
    }

    #[test]
    fn starting_a_candidate_leaves_the_active_session_unchanged() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        let _commands = connected_session(&mut app, "public");
        app.terminal_channel().drain_events();

        pending_join(
            &mut app,
            "community",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );

        assert!(app.network.is_some(), "the active worker is untouched");
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert_eq!(app.room.server_alias, "public");
        assert!(navigations(&app.terminal_channel().drain_events()).is_empty());
    }

    #[test]
    fn selecting_the_active_server_cancels_a_pending_switch() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        let _commands = connected_session(&mut app, "public");
        pending_join(
            &mut app,
            "community",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let stale_generation = join_generation(&app);
        app.terminal_channel().drain_events();

        assert!(matches!(
            app.start_join(
                test_server_id("public"),
                JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
            ),
            JoinStart::AlreadyActive
        ));
        assert!(!app.has_pending_join());

        app.handle_app_event(AppEvent::NetworkFor {
            generation: stale_generation,
            event: authenticated_event(),
        });

        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert!(app.network.is_some());
        assert!(
            navigations(&app.terminal_channel().drain_events())
                .iter()
                .any(|event| matches!(event, NavigationEvent::CloseScreen))
        );
    }

    #[test]
    fn superseding_an_editor_consent_closes_the_overlay_before_answering() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        pending_join(
            &mut app,
            "public",
            JoinOwner::ServerEditor {
                client: crate::client_channel::ClientId::PRIMARY,
                request_id: 17,
            },
        );
        let generation = join_generation(&app);
        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::TransportEncryptionRequired,
        });
        app.terminal_channel().drain_events();

        pending_join(
            &mut app,
            "community",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );

        let events = app.terminal_channel().drain_events();
        let close = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    TerminalEvent::Navigation(NavigationEvent::CloseOverlay)
                )
            })
            .expect("the consent overlay closes");
        let result = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    TerminalEvent::ServerEditResult {
                        request_id: 17,
                        outcome: ServerEditOutcome::SavedButJoinFailed(_),
                    }
                )
            })
            .expect("the editor submission is answered");
        assert!(
            close < result,
            "the editor must be active before its result arrives"
        );
    }

    #[test]
    fn deleting_a_server_closes_its_join_consent_surface() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "delete-join-consent");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::TransportEncryptionRequired,
        });
        app.terminal_channel().drain_events();

        app.delete_server(test_server_id("public"));

        assert!(!app.has_pending_join());
        let events = app.terminal_channel().drain_events();
        assert!(matches!(
            navigations(&events).as_slice(),
            [NavigationEvent::CloseOverlay, NavigationEvent::CloseScreen]
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn authentication_restarts_when_the_saved_worker_fields_changed() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "join-edit-restart");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        let attempt_id = pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let stale_generation = join_generation(&app);
        let mut edit = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut edit, 1, "Current User");
        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, edit);
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation: stale_generation,
            event: authenticated_event(),
        });

        let current_generation = join_generation(&app);
        assert_ne!(current_generation, stale_generation);
        assert!(
            app.network.is_none(),
            "the stale candidate was not promoted"
        );
        assert!(app.terminal_channel().drain_events().iter().any(|event| {
            matches!(
                event,
                TerminalEvent::JoinUpdate(crate::client_channel::JoinView {
                    attempt_id: current,
                    phase: crate::client_channel::JoinPhaseView::Connecting,
                    ..
                }) if *current == attempt_id
            )
        }));

        app.handle_app_event(AppEvent::NetworkFor {
            generation: current_generation,
            event: authenticated_event(),
        });
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn promotion_uses_current_non_worker_server_fields() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.config.servers[0].label = "renamed".to_string();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: authenticated_event(),
        });

        assert_eq!(app.room.server_alias, "renamed");
        assert_eq!(app.active_network_generation, Some(generation));
    }

    /// A transfer that appears while the candidate authenticates refuses the
    /// promotion: the active session and its transfers stay, and the join owner
    /// reads a retryable failure.
    #[test]
    fn transfer_appearing_before_promotion_refuses_promotion() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        pending_join(
            &mut app,
            "community",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        let _commands = connected_with_active_transfer(&mut app, "public");
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: authenticated_event(),
        });

        assert!(app.network.is_some(), "the active session stays");
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert!(app.room.has_active_transfers());
        assert!(app.terminal_channel().drain_events().iter().any(|event| {
            matches!(
                event,
                TerminalEvent::JoinUpdate(crate::client_channel::JoinView {
                    phase: crate::client_channel::JoinPhaseView::Failed {
                        retryable: true,
                        ..
                    },
                    ..
                })
            )
        }));
    }

    /// Declining plaintext ends the attempt with a readable failure; only the
    /// warning overlay closes, and the candidate is the only casualty.
    #[test]
    fn declined_transport_encryption_fails_only_the_candidate() {
        let mut app = test_app();
        app.config.servers.push(saved_server("public", "token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        let _commands = connected_session(&mut app, "community");
        let attempt_id = pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::TransportEncryptionRequired,
        });
        app.terminal_channel().drain_events();

        app.decline_join_plaintext(attempt_id);

        assert!(app.network.is_some(), "the active session stays");
        let events = app.terminal_channel().drain_events();
        assert!(matches!(
            navigations(&events).as_slice(),
            [NavigationEvent::CloseOverlay]
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::JoinUpdate(crate::client_channel::JoinView {
                phase: crate::client_channel::JoinPhaseView::Failed { .. },
                ..
            })
        )));
    }

    /// Consent commits the relaxed policy durably, then restarts the same
    /// attempt under a fresh worker generation.
    #[test]
    fn accepted_transport_encryption_commits_before_the_restart() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "candidate-plaintext-consent");
        app.config.servers.push(ServerEntry {
            require_transport_encryption: true,
            ..saved_server("public", "token")
        });
        let attempt_id = pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::TransportEncryptionRequired,
        });
        app.terminal_channel().drain_events();

        app.accept_join_plaintext(attempt_id)
            .expect("consent is persisted");

        assert!(
            !app.config.servers[0].require_transport_encryption,
            "the policy was committed"
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("require-transport-encryption = false")
        );
        assert!(app.has_pending_join(), "the same attempt is running again");
        assert_ne!(join_generation(&app), generation, "under a fresh worker");
        assert!(matches!(
            navigations(&app.terminal_channel().drain_events()).as_slice(),
            [NavigationEvent::CloseOverlay]
        ));
        let _ = std::fs::remove_file(path);
    }

    /// A rejected username replaces the join screen with an editor loaded from
    /// the committed record, focused on the offending field. The old draft is
    /// never resurrected.
    #[test]
    fn a_username_taken_by_a_pending_join_opens_a_fresh_committed_draft() {
        let mut app = test_app();
        app.config.servers.push(ServerEntry {
            username: "Taken Name".to_string(),
            ..saved_server("public", "public-token")
        });
        pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::AuthFailed {
                code: ERROR_USERNAME_TAKEN,
                message: "username already in use".to_string(),
            },
        });

        assert!(!app.has_pending_join());
        assert!(matches!(
            navigations(&app.terminal_channel().drain_events()).as_slice(),
            [NavigationEvent::ReplaceScreen(screen)]
                if matches!(
                    screen.as_ref(),
                    ScreenSpec::ServerEditor(draft)
                        if draft.original_label() == "public"
                            && draft.username_error_for_test().is_some()
                )
        ));
    }

    #[test]
    fn join_credential_repair_preserves_edits_and_restarts_the_same_attempt() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "join-credential-repair");
        app.config
            .servers
            .push(saved_server("public", "stale-token"));
        let attempt_id = pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::AuthFailed {
                code: ERROR_TOKEN_STALE_EPOCH,
                message: "stale credential".to_string(),
            },
        });
        let repair_attempt = app
            .credential_repair
            .as_ref()
            .expect("repair is running")
            .attempt;
        assert!(app.join_repair_is_current(attempt_id));

        let mut edit = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut edit, 1, "Current User");
        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, edit);
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::Pairing {
            attempt: repair_attempt,
            event: PairingEvent::OpenSucceeded {
                token: "fresh-token".to_string(),
                server_public_key: "ab".repeat(32),
            },
        });

        let server = &app.config.servers[0];
        assert_eq!(server.username, "Current User");
        assert_eq!(server.token, "fresh-token");
        assert_eq!(server.server_public_key, "ab".repeat(32));
        assert!(app.has_pending_join());
        assert!(!app.join_repair_is_current(attempt_id));
        assert_ne!(join_generation(&app), generation);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn superseded_join_repair_cannot_commit_or_rejoin() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "stale-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::AuthFailed {
                code: ERROR_TOKEN_STALE_EPOCH,
                message: "stale credential".to_string(),
            },
        });
        let repair_attempt = app
            .credential_repair
            .as_ref()
            .expect("repair is running")
            .attempt;

        pending_join(
            &mut app,
            "community",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let community_generation = join_generation(&app);
        assert!(app.credential_repair.is_none());

        app.handle_app_event(AppEvent::Pairing {
            attempt: repair_attempt,
            event: PairingEvent::OpenSucceeded {
                token: "late-token".to_string(),
                server_public_key: "ab".repeat(32),
            },
        });

        assert_eq!(app.config.servers[0].token, "stale-token");
        assert_eq!(join_generation(&app), community_generation);
    }

    #[test]
    fn join_repair_refuses_to_overwrite_newer_credentials() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "stale-token"));
        let attempt_id = pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::AuthFailed {
                code: ERROR_TOKEN_STALE_EPOCH,
                message: "stale credential".to_string(),
            },
        });
        let repair_attempt = app
            .credential_repair
            .as_ref()
            .expect("repair is running")
            .attempt;
        app.config.servers[0].token = "newer-token".to_string();

        app.handle_app_event(AppEvent::Pairing {
            attempt: repair_attempt,
            event: PairingEvent::OpenSucceeded {
                token: "repair-token".to_string(),
                server_public_key: "ab".repeat(32),
            },
        });

        assert_eq!(app.config.servers[0].token, "newer-token");
        assert!(!app.join_repair_is_current(attempt_id));
        assert!(app.terminal_channel().drain_events().iter().any(|event| {
            matches!(
                event,
                TerminalEvent::JoinUpdate(crate::client_channel::JoinView {
                    phase: crate::client_channel::JoinPhaseView::Failed {
                        retryable: true,
                        ..
                    },
                    ..
                })
            )
        }));
    }

    #[test]
    fn canceling_a_pending_join_does_not_touch_the_active_session() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        let _commands = connected_session(&mut app, "public");
        let attempt_id = pending_join(
            &mut app,
            "community",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        app.terminal_channel().drain_events();

        app.handle_client_command(
            crate::client_channel::ClientId::PRIMARY,
            CoreCommand::CancelJoin { attempt_id },
        );

        assert!(!app.has_pending_join());
        assert!(app.network.is_some(), "the active session is untouched");
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        // The join screen pops itself; nothing else moves the user.
        assert!(navigations(&app.terminal_channel().drain_events()).is_empty());
    }

    /// Pairing runs beside a live session, durably commits its completed
    /// credential, and presents the editor without touching active transfers.
    #[test]
    fn pairing_success_opens_editor_and_keeps_the_running_transfers() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "pairing-join-transfer");
        let attempt = running_invite_pair(&mut app, "invite-token");
        let _commands = connected_with_active_transfer(&mut app, "lab");

        app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::InviteSucceeded,
        });

        assert_eq!(
            app.config.server("public").expect("paired server").token,
            "invite-token"
        );
        assert!(app.terminal_channel().drain_events().iter().any(|event| {
            matches!(
                event,
                TerminalEvent::Navigation(NavigationEvent::OpenScreen(screen))
                    if matches!(
                        screen.as_ref(),
                        ScreenSpec::ServerEditor(draft)
                            if draft.server_id() == test_server_id("invite-pending")
                    )
            )
        }));
        assert!(app.view.status.text().contains("review server settings"));
        assert_eq!(app.room.active_server_id, Some(test_server_id("lab")));
        assert!(app.network.is_some());
        assert!(app.room.has_active_transfers());
        let _ = std::fs::remove_file(path);
    }

    /// The session owns its server by id, so a rename needs no live-reference
    /// fixups: everything resolving through the id keeps working, and only the
    /// display alias is refreshed.
    #[test]
    fn renaming_the_active_server_retains_ownership_by_id() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "rename-active-server");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        let commands = connected_session(&mut app, "public");
        let mut rename = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut rename, 0, "community");

        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, rename);

        assert_eq!(app.config.servers[0].label, "community");
        assert_eq!(app.config.servers[0].id, test_server_id("public"));
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert_eq!(app.room.server_alias, "community");
        assert_live_server_labels_resolve(&app);
        // The push gate compares the active id against the saved entry, so a
        // rename must not stop the policy from reaching the worker.
        assert!(
            commands
                .try_iter()
                .any(|command| matches!(command, NetworkCommand::SetFilePolicy(_)))
        );
        assert!(app.persist_e2e_pin(crate::config::E2ePeerPin {
            room_id: 0x8000_0001,
            user_id: 2,
            username: "bob".to_string(),
            public_key: "11".repeat(32),
            trust_level: crate::config::E2eTrustLevel::Accepted,
            change_from: None,
            previous: Vec::new(),
        }));
        assert_eq!(app.config.servers[0].e2e_peer_pins.len(), 1);
        let _ = std::fs::remove_file(path);
    }

    /// A plain save is deliberately not a reconnect, so an edit the running
    /// session already took at connect has to be reported as pending rather
    /// than read as applied.
    #[test]
    fn saving_the_active_server_reports_what_waits_for_the_next_connect() {
        // The display name and every address reach the worker once, at connect.
        for (field, value) in [(1, "Renamed User"), (2, "10.0.0.9:42000")] {
            let mut app = test_app();
            let path = temp_config_path(&mut app, "active-connection-change");
            app.config
                .servers
                .push(saved_server("public", "public-token"));
            let _commands = connected_session(&mut app, "public");
            let mut edit = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
            type_into_draft(&mut edit, field, value);

            save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, edit);

            assert_eq!(
                app.view.status.text(),
                "server saved; changes apply on reconnect",
                "field {field}"
            );
            assert_eq!(app.view.status.kind(), StatusKind::Info);
            assert!(app.network.is_some(), "the save must not reconnect");
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn saves_that_leave_the_live_session_alone_stay_quiet() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "quiet-server-save");
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .servers
            .push(saved_server("community", "community-token"));
        let _commands = connected_session(&mut app, "public");
        let mut inactive = ServerEditDraft::from_server(&app.config.servers[1], &app.config);
        type_into_draft(&mut inactive, 1, "Renamed User");

        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, inactive);

        assert_eq!(app.config.servers[1].username, "Renamed User");
        assert!(
            app.view.status.text().starts_with("server saved to"),
            "another server's connection settings say nothing about this one: {}",
            app.view.status.text()
        );

        // A rename of the active server moves with the live session instead of
        // waiting for a reconnect.
        let mut rename = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut rename, 0, "renamed");

        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, rename);

        assert_eq!(app.room.server_alias, "renamed");
        assert!(
            app.view.status.text().starts_with("server saved to"),
            "{}",
            app.view.status.text()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn refused_config_write_keeps_the_server_edit_out_of_memory() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        app.config
            .user_audio
            .push(crate::config::UserAudioPreference {
                server_id: test_server_id("public"),
                user_id: UserId(2),
                volume_db: -3.0,
            });
        let _commands = connected_session(&mut app, "public");
        let _dir = unwritable_config_path(&mut app, "refused-server-edit");
        let servers = server_labels(&app);
        let mut rename = ServerEditDraft::from_server(&app.config.servers[0], &app.config);
        type_into_draft(&mut rename, 0, "community");

        save_draft(&mut app, crate::client_channel::ClientId::PRIMARY, rename);

        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert_eq!(server_labels(&app), servers);
        assert_eq!(app.config.user_audio[0].server_id, test_server_id("public"));
        assert_eq!(app.room.server_alias, "public");
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert_live_server_labels_resolve(&app);

        // A later save over a writable path must not carry the refused edit.
        let path = temp_config_path(&mut app, "refused-server-edit-later");
        app.config.save_runtime().expect("later save");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("community"), "{saved}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn refused_config_write_keeps_the_deleted_server_and_its_session() {
        let mut app = test_app();
        app.config
            .servers
            .push(saved_server("public", "public-token"));
        let _commands = connected_session(&mut app, "public");
        let _dir = unwritable_config_path(&mut app, "refused-server-delete");
        let servers = server_labels(&app);

        app.delete_server(test_server_id("public"));
        app.sync_terminal_events();

        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert_eq!(server_labels(&app), servers);
        assert_eq!(app.room.active_server_id, Some(test_server_id("public")));
        assert!(app.network.is_some());
        assert_live_server_labels_resolve(&app);
    }

    #[test]
    fn refused_config_write_restores_the_transport_encryption_requirement() {
        let mut app = test_app();
        app.config.servers.push(ServerEntry {
            require_transport_encryption: true,
            ..saved_server("public", "public-token")
        });
        let attempt_id = pending_join(
            &mut app,
            "public",
            JoinOwner::Terminal(crate::client_channel::ClientId::PRIMARY),
        );
        let generation = join_generation(&app);
        app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::TransportEncryptionRequired,
        });
        let _dir = unwritable_config_path(&mut app, "refused-plaintext-consent");

        app.handle_client_command(
            crate::client_channel::ClientId::PRIMARY,
            CoreCommand::AcceptTransportEncryption { attempt_id },
        );
        app.sync_terminal_events();

        assert!(app.config.servers[0].require_transport_encryption);
        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert!(app.network.is_none());
        assert!(
            app.has_pending_join(),
            "the attempt stays parked on consent for a retry"
        );
    }

    /// An invite attempt parked in `Running`. It persists nothing before it
    /// succeeds, so unlike open pairing there is no durable record yet.
    fn running_invite_pair(app: &mut TestApp, token: &str) -> u64 {
        // A live invite attempt carries a freshly generated id, so the pending
        // entry never shares one with a server saved while the worker ran.
        let server = ServerEntry {
            id: test_server_id("invite-pending"),
            ..saved_server("public", token)
        };
        let config = server.client_config(&app.config, app.download_store.clone());
        app.pairing.set_running_for_test(
            crate::client_channel::ClientId::PRIMARY,
            PendingPair {
                server,
                open: None,
                open_password: String::new(),
                pairing_code: Some("pair-code".to_string()),
                completion: PairCompletion::OpenEditor,
            },
            PairingJob::Invite {
                config,
                pairing_code: "pair-code".to_string(),
            },
            None,
        )
    }

    /// The label an invite attempt dialed with can be taken while its worker
    /// runs. Committing over the configured record would lose its credential,
    /// so pairing fails before opening an editor.
    #[test]
    fn invite_commit_refuses_a_label_claimed_while_pairing() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "invite-commit-race");
        let attempt = running_invite_pair(&mut app, "invite-token");
        app.config
            .servers
            .push(saved_server("public", "other-token"));

        app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::InviteSucceeded,
        });

        assert_eq!(app.config.servers.len(), 1);
        assert_eq!(app.config.servers[0].token, "other-token");
        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert!(
            !app.terminal_channel()
                .drain_events()
                .iter()
                .any(|event| matches!(
                    event,
                    TerminalEvent::Navigation(NavigationEvent::OpenScreen(screen))
                        if matches!(screen.as_ref(), ScreenSpec::ServerEditor(_))
                ))
        );
        let _ = std::fs::remove_file(path);
    }

    /// An open-pairing attempt parked in `Running`, as it is while its worker
    /// runs: all recovery state is retained by the coordinator alone.
    /// The address is a closed port, so the worker that an accepted consent
    /// restarts fails at once instead of reaching a real server.
    fn running_open_pair(app: &mut TestApp, token: &str, server_public_key: &str) -> u64 {
        let server = ServerEntry {
            id: test_server_id("open-pending"),
            label: "public".to_string(),
            tcp_addr: "127.0.0.1:1".to_string(),
            username: "User".to_string(),
            token: token.to_string(),
            server_public_key: server_public_key.to_string(),
            ..ServerEntry::default()
        };
        let config = server.client_config(&app.config, app.download_store.clone());
        let pending = PendingPair {
            server,
            open: Some(token.to_string()),
            open_password: String::new(),
            pairing_code: None,
            completion: PairCompletion::OpenEditor,
        };
        app.pairing.set_running_for_test(
            crate::client_channel::ClientId::PRIMARY,
            pending,
            PairingJob::Open {
                config,
                password: String::new(),
                existing_token: token.to_string(),
            },
            None,
        )
    }

    fn temp_config_path(app: &mut TestApp, label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("chatt-{label}-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);
        app.config.config_path = Some(path.clone());
        path
    }

    #[test]
    fn plaintext_pairing_refusal_prompts_for_consent_and_keeps_the_attempt() {
        let mut app = test_app();
        let channel = app.terminal_channel();
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}consent-secret");
        let attempt = running_open_pair(&mut app, &token, "");

        app.apply_pairing_input(PairingInput::Worker {
            attempt,
            event: PairingEvent::TransportEncryptionRequired,
        });

        assert!(
            app.pairing
                .awaiting_plaintext_consent(crate::client_channel::ClientId::PRIMARY)
        );
        let mut events = channel.drain_events();
        assert!(matches!(
            events.pop_front(),
            Some(TerminalEvent::Navigation(NavigationEvent::ShowOverlay(overlay)))
                if matches!(
                    overlay.as_ref(),
                    OverlaySpec::TransportEncryptionWarning {
                        label,
                        target: TransportWarningTarget::Pairing,
                    } if label == "public"
                )
        ));
    }

    #[test]
    fn accepting_plaintext_pairing_consent_keeps_the_cleared_requirement_in_memory() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "plaintext-consent");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}consent-secret");
        let attempt = running_open_pair(&mut app, &token, "");
        app.apply_pairing_input(PairingInput::Worker {
            attempt,
            event: PairingEvent::TransportEncryptionRequired,
        });

        app.apply_pairing_input(PairingInput::AcceptPlaintext {
            owner: crate::client_channel::ClientId::PRIMARY,
        });

        // The retry is running again, this time against a config that no longer
        // makes the worker refuse the server's plaintext transport.
        assert!(!app.pairing_idle());
        let pending = app.pairing_pending().expect("attempt restarted");
        assert!(!pending.server.require_transport_encryption);
        assert!(
            !path.exists(),
            "consent did not write pairing state to config"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn canceling_plaintext_pairing_consent_leaves_config_untouched() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "plaintext-consent-cancel");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}consent-secret");
        let attempt = running_open_pair(&mut app, &token, "");
        app.apply_pairing_input(PairingInput::Worker {
            attempt,
            event: PairingEvent::TransportEncryptionRequired,
        });

        app.cancel_open_pairing();

        assert!(app.pairing_idle());
        assert!(app.config.servers.is_empty());
        assert!(!path.exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn username_taken_pins_the_observed_server_key_for_the_retry() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "username-taken-pin");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}pin-secret");
        let key = "ab".repeat(32);
        let attempt = running_open_pair(&mut app, &token, "");

        app.apply_pairing_input(PairingInput::Worker {
            attempt,
            event: PairingEvent::UsernameTaken {
                message: "username taken".to_string(),
                server_public_key: key.clone(),
            },
        });

        assert_eq!(app.pairing_pending().unwrap().server.server_public_key, key);
        assert!(!path.exists(), "the TOFU pin remains transient");
        assert!(
            app.terminal_channel().drain_events().iter().any(|event| {
                matches!(
                    event,
                    TerminalEvent::Navigation(NavigationEvent::OpenScreen(screen))
                        if matches!(screen.as_ref(), ScreenSpec::ServerEditor(_))
                )
            }),
            "the retry uses the full server editor"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn username_taken_with_a_changed_server_key_fails_the_pairing() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "username-taken-key-change");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}pin-secret");
        let attempt = running_open_pair(&mut app, &token, &"ab".repeat(32));

        app.apply_pairing_input(PairingInput::Worker {
            attempt,
            event: PairingEvent::UsernameTaken {
                message: "username taken".to_string(),
                server_public_key: "cd".repeat(32),
            },
        });

        assert!(app.pairing_idle());
        assert!(app.config.servers.is_empty());
        assert!(!path.exists());
        let _ = std::fs::remove_file(path);
    }

    /// A config path whose parent is a regular file, so every save fails.
    fn unwritable_config_path(app: &mut TestApp, label: &str) -> crate::test_temp::TempPath {
        let dir = crate::test_temp::TempDir::new(label);
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"").expect("blocking file");
        let path = dir.with_path("blocked/chatt.toml");
        app.config.config_path = Some(path.to_path_buf());
        path
    }

    fn editor_on_top(h: &mut Harness) -> bool {
        h.stack.depth() == 2 && h.top_theme_mode() == crate::theme::UiMode::ServerEdit
    }

    #[test]
    fn invite_pairing_success_saves_then_opens_the_editor() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "invite-editor");
        let attempt = running_invite_pair(&mut app, "invite-token");
        let mut h = Harness::new(app);

        h.app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::InviteSucceeded,
        });
        h.apply();

        assert!(editor_on_top(&mut h));
        assert_eq!(
            h.app.config.server("public").expect("paired server").token,
            "invite-token"
        );
        assert!(path.exists());

        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert_eq!(h.stack.depth(), 1);
        assert!(h.app.config.server("public").is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn canceling_a_username_retry_editor_discards_only_the_transient_attempt() {
        let mut app = test_app();
        let server = awaiting_username_pair(&mut app, "public");
        let draft = ServerEditDraft::from_new_server(server, &app.config);
        let mut h = Harness::new(app);
        h.app.send_to(
            crate::client_channel::ClientId::PRIMARY,
            TerminalEvent::Navigation(NavigationEvent::OpenScreen(Box::new(
                ScreenSpec::ServerEditor(draft),
            ))),
        );
        h.apply();
        assert!(editor_on_top(&mut h));

        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert!(h.app.pairing_idle());
        assert_eq!(h.stack.depth(), 1);
        assert!(h.app.config.servers.is_empty());
    }

    /// Pairing durably inserts the completed credential. Save and Join then
    /// applies the editor values and starts exactly one join by the same id.
    #[test]
    fn paired_editor_save_and_join_commits_then_starts_one_join() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "join-purpose-pair");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}join-secret");
        let server = ServerEntry {
            id: test_server_id("open-pending"),
            label: "public".to_string(),
            tcp_addr: "127.0.0.1:1".to_string(),
            username: "User".to_string(),
            token: token.clone(),
            ..ServerEntry::default()
        };
        let config = server.client_config(&app.config, app.download_store.clone());
        let attempt = app.pairing.set_running_for_test(
            crate::client_channel::ClientId::PRIMARY,
            PendingPair {
                server,
                open: Some(token.clone()),
                open_password: String::new(),
                pairing_code: None,
                completion: PairCompletion::OpenEditor,
            },
            PairingJob::Open {
                config,
                password: String::new(),
                existing_token: token,
            },
            None,
        );
        app.terminal_channel().drain_events();

        app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::OpenSucceeded {
                token: "issued-token".to_string(),
                server_public_key: "ab".repeat(32),
            },
        });

        let draft = app
            .terminal_channel()
            .drain_events()
            .into_iter()
            .find_map(|event| match event {
                TerminalEvent::Navigation(NavigationEvent::OpenScreen(screen)) => match *screen {
                    ScreenSpec::ServerEditor(draft) => Some(draft),
                    _ => None,
                },
                _ => None,
            })
            .expect("paired candidate editor");
        app.handle_client_command(
            crate::client_channel::ClientId::PRIMARY,
            CoreCommand::SubmitServerEdit {
                request_id: 1,
                draft,
                join: true,
            },
        );

        assert_eq!(
            app.config.server("public").expect("committed entry").token,
            "issued-token",
            "the credential is durable before the join runs"
        );
        assert_eq!(
            app.config.server("public").unwrap().id,
            test_server_id("open-pending")
        );
        assert!(app.has_pending_join(), "exactly one candidate is running");
        assert!(
            app.terminal_channel()
                .drain_events()
                .iter()
                .any(|event| matches!(
                    event,
                    TerminalEvent::ServerEditResult {
                        request_id: 1,
                        outcome: ServerEditOutcome::JoinStarted(_),
                    }
                ))
        );
        let _ = std::fs::remove_file(path);
    }

    /// Open pairing commits the completed credential and then opens an editor,
    /// retaining the id allocated at the beginning of the attempt.
    #[test]
    fn open_pairing_success_saves_with_stable_id_then_opens_editor() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "open-pair-editor");
        let attempt = running_open_pair(&mut app, "provisional", "");
        let mut h = Harness::new(app);

        h.app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::OpenSucceeded {
                token: "issued-token".to_string(),
                server_public_key: "ab".repeat(32),
            },
        });
        h.apply();

        assert!(editor_on_top(&mut h));
        let server = h.app.config.server("public").expect("paired server");
        assert_eq!(server.token, "issued-token");
        assert_eq!(server.id, test_server_id("open-pending"));
        assert!(path.exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn password_protected_pairing_ends_in_the_server_editor() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "password-pair-editor");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}password-secret");
        let attempt = running_open_pair(&mut app, &token, "");
        let mut h = Harness::new(app);

        h.app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::OpenNeedsPassword {
                retry: false,
                server_public_key: "ab".repeat(32),
            },
        });
        h.apply();

        assert_eq!(h.stack.depth(), 2);
        assert!(h.overlay_active());

        h.app.handle_client_command(
            crate::client_channel::ClientId::PRIMARY,
            CoreCommand::SubmitPairPassword("hunter2".to_string()),
        );
        let attempt = h
            .app
            .pairing
            .running_attempt_for_test()
            .expect("password retry running");
        h.app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::OpenSucceeded {
                token: "issued-token".to_string(),
                server_public_key: "ab".repeat(32),
            },
        });
        h.apply();

        assert!(editor_on_top(&mut h));
        assert!(h.app.config.server("public").is_ok());
        assert!(path.exists());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn connection_username_rejection_replaces_the_join_screen_with_the_editor() {
        let mut app = test_app();
        app.config.servers.push(ServerEntry {
            username: "Zoe".to_string(),
            ..saved_server("public", "token")
        });
        let mut h = Harness::new(app);
        let server_id = h.app.config.servers[0].id;
        h.app
            .start_join_with_screen(server_id, crate::client_channel::ClientId::PRIMARY);
        h.apply();
        let generation = join_generation(&h.app);

        h.app.handle_app_event(AppEvent::NetworkFor {
            generation,
            event: NetworkEvent::AuthFailed {
                code: ERROR_USERNAME_TAKEN,
                message: "username already in use".to_string(),
            },
        });
        h.apply();

        assert!(editor_on_top(&mut h));

        h.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));

        assert_eq!(h.stack.depth(), 1);
        assert_eq!(h.top_theme_mode(), crate::theme::UiMode::ServerSelect);
    }

    /// A hard worker failure ends the attempt without leaving recovery state
    /// in either the server catalog or the config file.
    #[test]
    fn worker_failure_leaves_no_config_state() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "username-retry-failed");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}retry-secret");
        let attempt = running_open_pair(&mut app, &token, "");
        let mut h = Harness::new(app);

        h.app.handle_app_event(AppEvent::Pairing {
            attempt,
            event: PairingEvent::Failed("pairing failed: connection refused".to_string()),
        });
        h.apply();

        assert!(h.app.pairing_idle());
        assert!(h.app.config.servers.is_empty());
        assert!(!path.exists());
        assert_eq!(h.app.view.status.kind(), StatusKind::Error);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn server_key_change_during_a_password_retry_fails_the_pairing() {
        let mut app = test_app();
        let path = temp_config_path(&mut app, "password-retry-key-change");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}pin-secret");
        let attempt = running_open_pair(&mut app, &token, &"ab".repeat(32));

        app.apply_pairing_input(PairingInput::Worker {
            attempt,
            event: PairingEvent::OpenNeedsPassword {
                retry: true,
                server_public_key: "cd".repeat(32),
            },
        });

        assert!(app.pairing_idle());
        assert!(app.config.servers.is_empty());
        let events = app.terminal_channel().drain_events();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TerminalEvent::PairingFailed(_)))
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                TerminalEvent::Navigation(NavigationEvent::CloseOverlay)
            )),
            "pairing never pops a screen it cannot know is its own"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn password_challenge_keeps_the_prompt_without_touching_config() {
        let mut app = test_app();
        let path = unwritable_config_path(&mut app, "password-challenge-persist");
        let token = format!("{OPEN_PAIR_RECOVERY_PREFIX}challenge-secret");
        let attempt = running_open_pair(&mut app, &token, "");

        app.apply_pairing_input(PairingInput::Worker {
            attempt,
            event: PairingEvent::OpenNeedsPassword {
                retry: false,
                server_public_key: "ab".repeat(32),
            },
        });

        assert!(
            app.pairing
                .pending_server_for(crate::client_channel::ClientId::PRIMARY)
                .is_some()
        );
        assert!(matches!(
            app.take_terminal_event(),
            Some(TerminalEvent::Navigation(NavigationEvent::ShowOverlay(overlay)))
                if matches!(overlay.as_ref(), OverlaySpec::PairingPassword { retry: false })
        ));
        assert!(app.take_terminal_event().is_none());
        assert!(!path.exists());
    }

    /// The paste prompt is the private way to hand this client a secret, so an
    /// invite ticket typed there pairs rather than being rejected as a
    /// malformed device link.
    #[test]
    fn invite_ticket_pasted_into_the_device_prompt_starts_invite_pairing() {
        let mut app = test_app();
        app.start_device_pairing_prompt(None);
        let ticket = rpc::control::encode_invite_ticket(&InviteTicket {
            version: rpc::PROTOCOL_VERSION,
            pairing_code: "pairing-code-long-enough".to_string(),
            tcp_addr: "10.0.0.1:4000".to_string(),
            udp_addr: "10.0.0.1:4001".to_string(),
            udp_probe_addr: None,
            server_public_key: "ab".repeat(32),
        })
        .expect("invite ticket");

        app.submit_device_pairing(ticket, String::new(), false);

        let pending = app.pairing_pending().expect("invite pairing started");
        assert_eq!(pending.server.tcp_addr, "10.0.0.1:4000");
        assert_eq!(
            pending.pairing_code.as_deref(),
            Some("pairing-code-long-enough")
        );
    }

    /// The prompt takes the third input `chatt pair` takes on argv: a public
    /// address, which self-service pairs rather than being read as a link.
    #[test]
    fn server_address_pasted_into_the_device_prompt_starts_open_pairing() {
        let mut app = test_app();
        app.start_device_pairing_prompt(None);

        app.submit_device_pairing("10.0.0.1:4000".to_string(), String::new(), false);

        let pending = app.pairing_pending().expect("open pairing started");
        assert_eq!(pending.server.tcp_addr, "10.0.0.1:4000");
        assert!(pending.open.is_some());
        assert!(pending.pairing_code.is_none());
    }

    /// Anything that is neither a link nor an address is reported as a bad
    /// address, and pairing stays idle so the prompt can be corrected.
    #[test]
    fn unparseable_prompt_input_reports_an_address_error() {
        let mut app = test_app();
        app.start_device_pairing_prompt(None);

        app.submit_device_pairing("not-an-address".to_string(), String::new(), false);
        app.sync_terminal_events();

        assert!(app.pairing_pending().is_none());
        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert!(app.view.status.text().contains("invalid server address"));
    }

    /// A device job cannot spawn without its cancellation flag, which is the
    /// one start failure reachable without a worker.
    #[test]
    fn device_pairing_start_failure_keeps_the_details_dialog() {
        let mut app = test_app();
        app.start_device_pairing_prompt(None);
        let server = ServerEntry {
            label: "device".to_string(),
            tcp_addr: "127.0.0.1:1".to_string(),
            username: "pairing".to_string(),
            ..ServerEntry::default()
        };
        let config = server.client_config(&app.config, app.download_store.clone());
        let ticket = DeviceLinkTicket {
            version: 1,
            pairing_secret: [7u8; rpc::crypto::KEY_LEN],
            tcp_addr: "127.0.0.1:1".to_string(),
            udp_addr: String::new(),
            udp_probe_addr: None,
            server_public_key: [0u8; rpc::crypto::ED25519_PUBLIC_KEY_LEN],
        };

        app.apply_pairing_input(PairingInput::Start {
            owner: crate::client_channel::ClientId::PRIMARY,
            pending: PendingPair {
                server,
                open: None,
                open_password: String::new(),
                pairing_code: None,
                completion: PairCompletion::OpenEditor,
            },
            job: PairingJob::Device {
                config,
                ticket: RetainedTicket::new(ticket),
                device_name: "laptop".to_string(),
                overwrite_existing: false,
            },
            cancellation: None,
        });

        assert!(!app.pairing_idle());
        let mut reported = false;
        while let Some(event) = app.take_terminal_event() {
            assert!(!matches!(
                event,
                TerminalEvent::Navigation(NavigationEvent::CloseOverlay)
            ));
            reported |= matches!(event, TerminalEvent::DevicePairingFailed { .. });
        }
        assert!(reported);
    }

    #[test]
    fn canceling_with_no_pairing_closes_the_stale_prompt() {
        let mut app = test_app();

        app.cancel_open_pairing();

        assert!(app.pairing_idle());
        assert!(matches!(
            app.take_terminal_event(),
            Some(TerminalEvent::Navigation(NavigationEvent::CloseOverlay))
        ));
    }

    #[test]
    fn submitting_a_password_with_no_pairing_reports_the_missing_attempt() {
        let mut app = test_app();

        app.submit_open_pair_password("hunter2".to_string());

        assert!(matches!(
            app.take_terminal_event(),
            Some(TerminalEvent::PairingFailed(message)) if message == "no pairing in progress"
        ));
    }

    #[test]
    fn join_no_match_unspecified_address_does_not_pair() {
        let mut app = app_with_servers(&[("lab", "10.0.0.1:4000")]);
        assert_eq!(app.resolve_join("0.0.0.0:41000"), JoinResolution::NoMatch);

        app.start_named_join("0.0.0.0:41000".to_string());

        assert!(app.pairing_pending().is_none());
        assert_eq!(app.view.status.kind(), StatusKind::Error);
    }

    #[test]
    fn join_no_match_bad_label_opens_picker_without_pairing() {
        let mut app = app_with_servers(&[("lab", "10.0.0.1:4000")]);
        assert_eq!(app.resolve_join("does-not-exist"), JoinResolution::NoMatch);
        app.start_named_join("does-not-exist".to_string());
        assert!(app.pairing_pending().is_none());
        assert_eq!(app.view.status.kind(), StatusKind::Error);
        assert!(matches!(
            app.take_terminal_event(),
            Some(TerminalEvent::Navigation(NavigationEvent::ResetBase(
                BaseScreen::Servers { .. }
            )))
        ));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Runtime normally reacquires before returning, but construction or
        // thread-spawn failures may unwind while render access is open. Drop
        // still needs the core projections to persist history and stop audio.
        self.acquire_core_state();
        self.finish_audio_report_on_shutdown();
        self.save_room_catalog();
        self.stop_audio();
    }
}
