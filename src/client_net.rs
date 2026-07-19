use chatt_mls::LocalInstallation;
use hashbrown::{HashMap, HashSet};
#[cfg(test)]
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream as StdTcpStream, ToSocketAddrs},
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SendError, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use aws_lc_rs::rand::SecureRandom;
#[cfg(test)]
use chatt_p2p::{
    AgentConfig as P2pAgentConfig, StunAuth, TraversalAgent, interfaces::InterfaceSnapshot,
};
use chatt_p2p::{Candidate, CandidateKind, IceRole, NatKind, RestartPortPolicy};
use mio::{Events, Interest, Poll, Token, Waker, net::TcpStream};
use rpc::{
    control::{
        ChatMessage, ChatMutationKind, ClientControl, DeviceLinkTicket, ERROR_AUTH_REJECTED,
        ERROR_PAIRING_CODE_MISMATCH, ERROR_PAIRING_INVALID_REQUEST, ERROR_PAIRING_NOT_ACTIVE,
        ERROR_PASSWORD_MISMATCH, ERROR_PASSWORD_REQUIRED, ERROR_PUBLIC_DISABLED,
        ERROR_TOKEN_STALE_EPOCH, ERROR_USERNAME_TAKEN, FileContentEncoding, FileMetadata,
        MAX_FILE_CHUNK_BYTES, MAX_FILE_NAME_BYTES, MessageFlags, P2pCandidate, P2pCandidateKind,
        P2pKey, P2pNatKind, P2pPeerInfo, P2pRole, ParticipantVoiceStatus, RoomInfo, RoomKind,
        ServerControl, UserSummary, decode_server_control, decode_server_hello,
        encode_client_control, encode_client_hello, encode_device_link_ticket, max_file_wire_bytes,
    },
    crypto::{
        CHANNEL_CONTROL, KEY_LEN, KeyMaterial, RecordProtection, SessionTransport, TransportMode,
        complete_client_transport_handshake, dev_server_public_key, ed25519_public_key_from_hex,
        encode_hex, generate_client_hello,
    },
    evented::{
        MioReady, ReadLimit, Readiness, WriteQueue, is_interrupted_io_error, write_queue_to,
    },
    frame, history,
    ids::{
        AccountId, BugReportId, DeviceId, EventId, FileTransferId, MessageId, RoomId, SessionId,
        StreamId, UserId,
    },
    media::{self, MediaPayload, VoicePayload as MediaVoicePayload},
    recv::RecvBuffer,
};
use zeroize::{Zeroize, ZeroizeOnDrop};

#[cfg(test)]
use rpc::crypto::AntiReplay;
#[cfg(test)]
use rpc::evented::ReadPumpOutcome;
#[cfg(test)]
use std::net::IpAddr;

use crate::app::{NetworkEventSender, PairingEventSender};
use crate::audio::{
    LiveEncoderProfile, LivePlaybackFeedback, LivePlaybackSink, LocalVoiceFrame, RemoteVoicePacket,
    VoicePayload as AudioVoicePayload,
};
use crate::config::{CandidatePrivacy, DownloadTarget, E2ePeerPin, EffectiveFiles};
use crate::e2e::{AcceptedPeerIdentity, AuthenticatedChat, E2eState};
use crate::file_compression::{
    self, COMPRESSION_PROBE_BYTES, FastCompressionDecision, ZSTD_WINDOW_LOG,
};
use crate::mdns::generate_mdns_name;
use crate::receive_store::{DiskReservation, Reservation};

mod mls_actor;
mod voice;

#[cfg(test)]
use voice::{
    EncoderFeedbackController, InterfaceMonitor, RecentVoiceSequenceResult, RecentVoiceSequences,
    direct_path_healthy, relay_suppressed,
};
use voice::{GatheredP2p, PeerConnection};

const TCP: Token = Token(0);
const WAKE: Token = Token(1);
/// Backstop poll timeout while idle. Sockets, the command waker, and the
/// deadline wakes in [`WorkerState::next_poll_timeout`] drive the loop; this
/// only bounds how long a missed schedule could sleep.
const IDLE_POLL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PENDING_MLS_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PENDING_MLS_FILE_ITEMS: usize = 1024;
const MAX_PENDING_MLS_FILE_GLOBAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING_MLS_FILE_GLOBAL_ITEMS: usize = 8192;
/// Keep a small burst reserve without making every fresh installation perform
/// dozens of public-key operations before it can join a room. One package is
/// an RFC 9750 last-resort fallback and the rest are one-time packages. The
/// server notifies the owner after every consume, so this is a latency reserve
/// rather than a long-lived prekey inventory.
const MLS_KEY_PACKAGE_TARGET: u16 = 4;
const MLS_RETRY_INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MLS_RETRY_MAX_BACKOFF: Duration = Duration::from_secs(30);

pub(super) fn mls_installation_dir(
    data_dir: &Path,
    server_public_key: &[u8; 32],
    user_id: UserId,
) -> PathBuf {
    data_dir
        .join("mls")
        .join(encode_hex(server_public_key))
        .join(format!("{:016x}", user_id.0))
}

const INTERFACE_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long a direct path must stay healthy before the client stops relaying
/// voice through the server.
const DIRECT_CONFIRM_WINDOW: Duration = Duration::from_secs(3);
/// Maximum gap in inbound direct traffic before a direct path is treated as
/// degraded and the server relay resumes. Kept above [`P2P_KEEPALIVE_INTERVAL`]
/// so ordinary speech silence does not trip it.
const DIRECT_FAILOVER_IDLE: Duration = Duration::from_millis(1500);
/// Cadence of the server keepalive sent while the relay is suppressed, to keep
/// the on-path NAT binding warm so relay resumes instantly.
const RELAY_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Cadence of media `Ping` probes used to estimate round-trip latency to the
/// server relay and to each direct peer.
const RTT_PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// Retry cadence for the UDP address claim while no `UdpBound` confirmation has
/// arrived. External-link mode cannot use ordinary clear pings to establish the
/// address, so losing the first `Bind` must be recoverable.
const UDP_BIND_RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// Number of unconfirmed `Bind` retries (at [`UDP_BIND_RETRY_INTERVAL`]) after
/// which the UDP media path is reported unreachable to the UI.
const UDP_BIND_FAILURE_ATTEMPTS: u32 = 5;
/// A server RTT without a successful probe for this long no longer describes
/// the current relay path and is reported as unavailable.
const RTT_STALE_AFTER: Duration = Duration::from_secs(15);
/// Smoothing weight applied to each new RTT sample folded into the EWMA.
const RTT_EWMA_WEIGHT: f32 = 0.2;
/// Cap on outstanding RTT probes tracked per destination before the oldest is
/// dropped, bounding memory if replies stop arriving.
const RTT_IN_FLIGHT_CAP: usize = 8;
/// STUN keepalive spacing for direct paths. Tightened from the agent default so
/// path liveness is reconfirmed every second.
const P2P_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
/// RFC 7675 consent lifetime for a direct path. Tightened from the agent default
/// to below [`struct@TraversalAgent`]'s 15 s disconnect timeout so consent expiry
/// is the hard send-stop, while staying well above [`P2P_KEEPALIVE_INTERVAL`] so
/// answered keepalives keep it fresh.
const P2P_CONSENT_TIMEOUT: Duration = Duration::from_secs(10);
const AUDIO_POP_LOG_ENV: &str = "CHATT_AUDIO_POP_LOG";
const AUDIO_POP_PACKET_FLAG_OPUS_RESET: u8 = 0x01;
const AUDIO_POP_PACKET_FLAG_SILENCE_HINT: u8 = 0x02;
const AUDIO_POP_PACKET_FLAG_SILENCE_RESUME: u8 = 0x04;
const AUDIO_POP_PACKET_FLAG_MUTE: u8 = 0x08;
const MAX_QUEUED_FILE_BYTES: usize = 1024 * 1024;
const MAX_FILE_CHUNKS_PER_TICK: usize = 64;
const MAX_FILE_SOURCE_BYTES_PER_TICK: usize = 1024 * 1024;
const MAX_COMPRESSED_UPLOAD_SOURCE_AHEAD_BYTES: u64 = 4 * 1024 * 1024;
const MAX_FILE_WIRE_BYTES_PER_TICK: usize = 1024 * 1024;
const MAX_FILE_DECODED_BYTES_PER_TICK: usize = 1024 * 1024;
const MAX_SERVER_CONTROLS_PER_FILE_PUMP: usize = 8;
const MAX_SERVER_CONTROLS_PER_ITERATION: usize = 64;
const MAX_BUFFERED_SERVER_BYTES: usize = 2 * 1024 * 1024;
const TCP_WRITE_ATTEMPTS: usize = 32;
/// Byte step between [`NetworkEvent::TransferProgress`] ticks. Small enough for a
/// smooth progress bar, coarse enough to keep the event channel and web feed from
/// flooding on a fast transfer. First and final ticks are always emitted.
const FILE_PROGRESS_STEP_BYTES: u64 = 256 * 1024;
const ENCODER_FEEDBACK_ALPHA: f32 = 0.35;
const ENCODER_PROFILE_HOLD: Duration = Duration::from_secs(10);
const MAX_COMMANDS_PER_ITERATION: usize = 8;
const DEFAULT_INITIAL_DEVICE_NAME: &str = "first-device";
const MAX_PENDING_PLAYBACK_PACKETS: usize = 256;
const MAX_RECENT_VOICE_SEQUENCES: usize = 512;
const RECENT_VOICE_SEQUENCE_WORD_BITS: usize = u64::BITS as usize;
const RECENT_VOICE_SEQUENCE_WORDS: usize =
    MAX_RECENT_VOICE_SEQUENCES / RECENT_VOICE_SEQUENCE_WORD_BITS;
const _: () = assert!(MAX_RECENT_VOICE_SEQUENCES % RECENT_VOICE_SEQUENCE_WORD_BITS == 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WakeIntent {
    Idle,
    Now,
    After(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PollSchedule {
    timeout: Duration,
}

impl PollSchedule {
    #[inline]
    fn after(timeout: Duration) -> Self {
        Self { timeout }
    }

    #[inline]
    fn include(&mut self, intent: WakeIntent) {
        match intent {
            WakeIntent::Idle => {}
            WakeIntent::Now => self.timeout = Duration::ZERO,
            WakeIntent::After(delay) => self.timeout = self.timeout.min(delay),
        }
    }

    #[inline]
    fn timeout(self) -> Duration {
        self.timeout
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerTask {
    TcpRead,
    TcpWrite,
}

const WORK_TCP_READ: u8 = 1 << 0;
const WORK_TCP_WRITE: u8 = 1 << 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct WorkerWork {
    bits: u8,
}

impl WorkerWork {
    #[inline]
    fn has_immediate_work(&self) -> bool {
        self.bits != 0
    }

    #[inline]
    fn wake(&self) -> WakeIntent {
        if self.has_immediate_work() {
            WakeIntent::Now
        } else {
            WakeIntent::Idle
        }
    }

    #[inline]
    fn queue_tcp_read(&mut self) {
        self.bits |= WORK_TCP_READ;
    }

    #[inline]
    fn queue_tcp_write(&mut self) {
        self.bits |= WORK_TCP_WRITE;
    }

    #[inline]
    fn take_tasks(&mut self) -> WorkerTasks {
        let bits = self.bits;
        self.bits = 0;
        WorkerTasks { bits }
    }
}

#[derive(Clone, Copy, Debug)]
struct WorkerTasks {
    bits: u8,
}

impl Iterator for WorkerTasks {
    type Item = WorkerTask;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.bits & WORK_TCP_READ != 0 {
            self.bits &= !WORK_TCP_READ;
            return Some(WorkerTask::TcpRead);
        }
        if self.bits & WORK_TCP_WRITE != 0 {
            self.bits &= !WORK_TCP_WRITE;
            return Some(WorkerTask::TcpWrite);
        }
        None
    }
}

#[cfg(test)]
#[inline]
fn tcp_receive_work_ready(readiness: Readiness, read_buf: &RecvBuffer) -> bool {
    readiness.is_ready() || matches!(frame::parse_frame(read_buf.pending()), Ok(Some(_)))
}

#[cfg(test)]
static LAST_RECEIVED_FILE_WIRE_BYTES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub(super) fn last_received_file_wire_bytes() -> u64 {
    LAST_RECEIVED_FILE_WIRE_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub tcp_addr: String,
    pub udp_addr: String,
    pub udp_probe_addr: Option<String>,
    pub username: String,
    pub token: String,
    pub server_public_key: Option<String>,
    /// Installation-local durable state root. Explicitly carried into the
    /// worker so multiple devices can coexist safely in one process.
    pub data_dir: Option<PathBuf>,
    /// Durable DM contact identity tuples and former trusted tuples.
    pub e2e_peer_pins: Vec<E2ePeerPin>,
    pub require_native_encryption: bool,
    pub file_policy: FilePolicy,
    /// The in-memory download ring buffer shared with the web server, filled
    /// when a room's download target resolves to [`DownloadTarget::Memory`].
    ///
    /// [`DownloadTarget::Memory`]: crate::config::DownloadTarget::Memory
    pub download_store: crate::receive_store::DownloadStore,
    pub max_upload_bytes: u64,
    /// Upload pacing ceiling in bytes per second, `0` for unlimited. Seeds
    /// [`UploadThrottle`] and is adjustable at runtime via
    /// [`NetworkCommand::SetUploadRate`].
    pub upload_rate_bytes: u64,
    pub p2p_enabled: bool,
    pub candidate_privacy: crate::config::CandidatePrivacy,
    pub prefer_ipv6: bool,
}

/// The resolved per-room download policy the worker enforces at receive time.
///
/// `default` is the server-level effective config; `rooms` holds the rooms
/// whose overrides differ from it. Built by [`Config::file_policy`] and
/// refreshed over [`NetworkCommand::SetFilePolicy`] when a save changes it.
///
/// [`Config::file_policy`]: crate::config::Config::file_policy
#[derive(Clone, Debug, Default)]
pub struct FilePolicy {
    pub default: EffectiveFiles,
    pub rooms: Vec<(RoomId, EffectiveFiles)>,
}

impl FilePolicy {
    pub fn for_room(&self, room_id: RoomId) -> &EffectiveFiles {
        for (id, files) in &self.rooms {
            if *id == room_id {
                return files;
            }
        }
        &self.default
    }

    /// Whether any room accepts downloads: the `receive_files` flag advertised
    /// to the server at join time.
    pub fn receives_any(&self) -> bool {
        if self.default.target.is_active() {
            return true;
        }
        self.rooms.iter().any(|(_, files)| files.target.is_active())
    }

    /// The receive limit advertised to the server: the largest limit among the
    /// receiving levels. Tighter per-room limits are enforced locally.
    pub fn advertised_limit(&self) -> u64 {
        let mut limit = if self.default.target.is_active() {
            self.default.max_download_bytes
        } else {
            0
        };
        for (_, files) in &self.rooms {
            if files.target.is_active() {
                limit = limit.max(files.max_download_bytes);
            }
        }
        limit
    }
}

/// A request to upload a file to the current room.
///
/// `name_override` supplies the uploaded name when the source path name is not
/// what the user wants shown (e.g. a staged clipboard temp file). `path` is
/// still validated and streamed as-is. `delete_after_open` removes the source
/// after the upload handle is opened, used to clean up staged temp files.
#[derive(Debug)]
pub struct UploadFileRequest {
    pub path: PathBuf,
    pub name_override: Option<String>,
    pub delete_after_open: bool,
}

impl UploadFileRequest {
    /// A plain upload that keeps the source path's file name and leaves the
    /// source in place.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            name_override: None,
            delete_after_open: false,
        }
    }
}

/// Makes the next test upload of `path` fail once its source cursor reaches
/// `bytes`. The fault lives outside [`UploadFileRequest`] so the production
/// request shape and durable-resume path stay unchanged.
#[cfg(test)]
pub(crate) fn fail_upload_source_after_for_test(path: PathBuf, bytes: u64) {
    TEST_UPLOAD_SOURCE_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(path, bytes);
}

#[cfg(test)]
static TEST_UPLOAD_SOURCE_FAILURES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
fn take_test_upload_source_failure(path: &Path, offset: u64) -> bool {
    let mut failures = TEST_UPLOAD_SOURCE_FAILURES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failures
        .get(path)
        .is_some_and(|fail_after| offset >= *fail_after)
    {
        failures.remove(path);
        true
    } else {
        false
    }
}

#[derive(Debug)]
pub enum NetworkCommand {
    SendChat {
        room_id: RoomId,
        body: String,
    },
    /// Replaces the body of a recent text message the local user sent.
    EditChat {
        room_id: RoomId,
        target: MessageId,
        body: String,
    },
    /// Deletes a recent message the local user sent.
    DeleteChat {
        room_id: RoomId,
        target: MessageId,
    },
    UploadFile {
        /// An explicit terminal-local target. `None` is reserved for external
        /// control clients, which intentionally use the daemon active room.
        room_id: Option<RoomId>,
        request: UploadFileRequest,
    },
    /// Aborts an in-flight file transfer identified by its server transfer id.
    /// The worker resolves the direction: an outgoing upload is canceled
    /// ([`ClientControl::UploadFileCancel`]); an incoming download is skipped
    /// ([`ClientControl::SkipFile`]).
    CancelTransfer {
        transfer_id: FileTransferId,
    },
    /// Tells the worker which room the client is viewing, the target for
    /// uploads injected outside the app thread (`chatt upload`, web sends
    /// without a room).
    SetActiveRoom(RoomId),
    /// Joins the room's voice call, leaving any current one.
    JoinVoice(RoomId),
    LeaveVoice,
    FetchHistory {
        room_id: RoomId,
        before: Option<MessageId>,
        limit: u16,
    },
    OpenDm(UserId),
    LocalVoicePacket(LocalVoiceFrame),
    SequencedLocalVoicePacket {
        sequence: u32,
        frame: LocalVoiceFrame,
    },
    SetPlaybackSink(Option<LivePlaybackSink>),
    PlaybackFeedback(LivePlaybackFeedback),
    SetVoiceStatus(ParticipantVoiceStatus),
    StartShare {
        codec: String,
        coded_width: u32,
        coded_height: u32,
        annexb: bool,
        extradata: Vec<u8>,
    },
    StopShare {
        stream_id: StreamId,
    },
    ReportBug {
        description: String,
        /// JSON metadata: app version, device/buffer config, `/audio` snapshot.
        metadata: String,
        /// zstd-compressed recent log text.
        compressed_logs: Vec<u8>,
    },
    /// Sets the upload pacing ceiling in bytes per second, `0` for unlimited.
    SetUploadRate(u64),
    /// Replaces the resolved per-room download policy after a config save.
    /// The join-time advertisement to the server refreshes on reconnect.
    SetFilePolicy(FilePolicy),
    SetP2pEnabled(bool),
    /// Requests the worker's exact current review snapshot. This never changes
    /// trust by itself.
    ReviewPeerIdentity {
        user_id: UserId,
    },
    VerifyPeerIdentity {
        expected: AcceptedPeerIdentity,
    },
    /// Forgets independent verification of the exact active key. The key stays
    /// usable and is persisted again at the ordinary accepted trust level.
    ForgetPeerIdentity {
        expected: AcceptedPeerIdentity,
    },
    /// Result of atomically persisting an E2E pin proposal. TOFU keys are
    /// already active for this session; this acknowledgement makes continuity
    /// durable or applies an explicit verification-level change.
    ConfirmE2ePeerPin {
        pin: E2ePeerPin,
        persisted: bool,
        manual_verification: bool,
    },
    /// App-thread acknowledgement for a cursor-committed MLS event. Kept
    /// separate from channel enqueue so a crash before UI application leaves
    /// the durable dispatch available on restart.
    AcknowledgeMlsUiDispatch {
        room_id: RoomId,
        sequence: u64,
    },
    /// Authority-signs a terminal roster revocation. The removed device can
    /// never bind again.
    RevokeE2eDevice {
        device_id: DeviceId,
    },
    ListE2eDevices,
    CreateDeviceLink,
    CancelDeviceLink {
        redemption_secret_hash: Vec<u8>,
    },
    /// Wakes a disconnected client after an embedded test server has finished
    /// restarting. Production clients discover remote availability through
    /// their bounded reconnect schedule instead.
    #[cfg(test)]
    RetryConnection,
    Shutdown,
}

impl NetworkCommand {
    fn requires_authenticated_session(&self) -> bool {
        matches!(
            self,
            Self::SendChat { .. }
                | Self::EditChat { .. }
                | Self::DeleteChat { .. }
                | Self::UploadFile { .. }
                | Self::CancelTransfer { .. }
                | Self::JoinVoice(_)
                | Self::LeaveVoice
                | Self::FetchHistory { .. }
                | Self::OpenDm(_)
                | Self::SetVoiceStatus(_)
                | Self::StartShare { .. }
                | Self::StopShare { .. }
                | Self::ReportBug { .. }
                | Self::RevokeE2eDevice { .. }
                | Self::ListE2eDevices
                | Self::CreateDeviceLink
                | Self::CancelDeviceLink { .. }
        )
    }
}

/// Which side of a file transfer a [`NetworkEvent::TransferProgress`] describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    /// A file being received from the relay.
    Incoming,
    /// A file being uploaded to the relay.
    Outgoing,
}

/// How a file transfer ended without landing, chosen so the file line's terminal
/// label reads naturally: a declined download is `skipped`, an aborted upload is
/// `cancelled`, and an upstream/local error is `failed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalVerb {
    Skipped,
    Cancelled,
    Failed,
}

impl TerminalVerb {
    /// The lowercase word shown on the terminal file line and in the web
    /// envelope.
    pub fn label(self) -> &'static str {
        match self {
            TerminalVerb::Skipped => "skipped",
            TerminalVerb::Cancelled => "cancelled",
            TerminalVerb::Failed => "failed",
        }
    }
}

/// Why [`NetworkClient::cancel_outgoing_upload`] is tearing an upload down, which
/// decides the terminal label and whether an error notice is raised. An
/// intentional cancel (user or server-declined) must not surface as an error.
enum UploadAbort {
    /// The user canceled their own upload: bare `cancelled`, no error notice.
    UserCancel,
    /// The server reported the upload lost its last recipient:
    /// `cancelled: recipient declined`, no error notice.
    Declined,
    /// The upload failed locally (read/compression): `failed: <error>`, and the
    /// error is also raised as a notice.
    Failure(String),
}

#[derive(Clone, Debug)]
pub enum NetworkEvent {
    Connected,
    Authenticated {
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
        users: Vec<UserSummary>,
        default_room: RoomId,
        video_transport_mode: TransportMode,
        video_auth_key: [u8; KEY_LEN],
    },
    /// A room appeared or changed shape (today: DM creation).
    RoomUpserted(RoomInfo),
    /// Reply to an [`NetworkCommand::OpenDm`] naming the DM room.
    DmOpened {
        room_id: RoomId,
        peer: UserId,
    },
    MlsAccountIdentity {
        account_id: AccountId,
    },
    /// This session proved possession of its active MLS device key and may now
    /// create encrypted events.
    MlsDeviceBound {
        device_id: DeviceId,
    },
    DeviceLinkCreated {
        redemption_secret_hash: Vec<u8>,
        pairing_string: String,
        expires_at_ms: u64,
    },
    DeviceLinkRedeemed {
        device_id: DeviceId,
        device_name: String,
    },
    DeviceLinkCanceled,
    /// A typed MLS delivery response for the MLS runtime boundary.
    Mls(ServerControl),
    /// One chunk of server-retained history for a room.
    HistoryChunk {
        room_id: RoomId,
        /// Echo of the originating request's paging cursor.
        before: Option<MessageId>,
        messages: Vec<AuthenticatedChat>,
        at_start: bool,
        /// True on the final chunk for one fetch request.
        complete: bool,
    },
    Chat(AuthenticatedChat),
    ChatMutationRejected {
        room_id: RoomId,
        target: MessageId,
        kind: ChatMutationKind,
        message: String,
    },
    FileReceived {
        metadata: FileMetadata,
        /// The name the file is served under (`/files/<served_name>` in the web
        /// view), mode-agnostic: an on-disk file name for persistent downloads
        /// or the ring-buffer key for in-memory ones.
        served_name: String,
        /// Intrinsic pixel size, parsed from the file's header as it streamed.
        /// `Some` only for images whose header fit the captured prefix.
        dimensions: Option<(u32, u32)>,
    },
    /// A live byte-count update for an in-flight file transfer, correlated to the
    /// chat announcement by the server `transfer_id`. Emitted at coarse steps plus
    /// mandatory first and final ticks. A tick with `transferred == total` is
    /// terminal for the progress overlay.
    TransferProgress {
        room_id: RoomId,
        transfer_id: FileTransferId,
        /// Announcement timestamp, the web upsert key alongside `transfer_id`.
        timestamp_ms: u64,
        transferred: u64,
        total: u64,
        direction: TransferDirection,
    },
    /// A file transfer ended without landing (skipped, canceled, or failed).
    /// Replaces any progress overlay for `transfer_id` with a persistent terminal
    /// label. `reason` fills the `verb: reason` form; `None` renders the bare verb
    /// (an explicit user skip/cancel). `timestamp_ms` addresses the web
    /// placeholder alongside `transfer_id`.
    TransferEnded {
        room_id: RoomId,
        transfer_id: FileTransferId,
        timestamp_ms: u64,
        verb: TerminalVerb,
        reason: Option<String>,
    },
    /// A file transfer finished successfully with nothing to save locally (an
    /// upload with no receive directory). Clears the progress overlay; the file
    /// line reverts to its plain announcement. Downloads and saved uploads clear
    /// via [`NetworkEvent::FileReceived`] instead.
    TransferComplete {
        room_id: RoomId,
        transfer_id: FileTransferId,
    },
    /// Server-wide presence for one user.
    Presence {
        user: UserSummary,
        online: bool,
    },
    /// A TOFU or verification update for the main thread to persist. Automatic
    /// TOFU keys are already active for this session.
    E2ePeerPinProposed {
        pin: E2ePeerPin,
        manual_verification: bool,
    },
    /// Internal/session fact: the server presented the tuple already stored in
    /// the durable pin. This does not mean independently verified.
    E2ePeerPinMatched {
        identity: AcceptedPeerIdentity,
    },
    VoiceStarted {
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
    },
    VoiceStopped {
        room_id: RoomId,
        session_id: SessionId,
        user_id: UserId,
        stream_id: StreamId,
    },
    PeerTransport {
        user_id: UserId,
        direct: bool,
    },
    VoicePacketObserved {
        stream_id: u32,
        payload_size: usize,
    },
    PlaybackFeedback(LivePlaybackFeedback),
    /// A listener's reception report about *my* outbound stream, attributed to
    /// the reporting user. Drives that user's roster-row outbound latency.
    OutboundFeedback {
        reporter: UserId,
        feedback: LivePlaybackFeedback,
    },
    /// Smoothed round-trip time to the server relay media socket, milliseconds.
    ServerRtt {
        rtt_ms: Option<u16>,
    },
    /// Smoothed round-trip time to a peer over its current transport (direct
    /// p2p path, or end-to-end through the server relay), milliseconds.
    PeerRtt {
        user_id: UserId,
        rtt_ms: Option<u16>,
    },
    VoiceStatus {
        user_id: UserId,
        status: ParticipantVoiceStatus,
    },
    /// The server refused or failed a `JoinVoice` for `room_id`, so the
    /// pending join must be rolled back before the room can be retried.
    VoiceJoinFailed {
        room_id: RoomId,
        message: String,
    },
    EncoderProfileChanged(LiveEncoderProfile),
    ShareStarted {
        room_id: RoomId,
        stream_id: StreamId,
        publish_secret: Vec<u8>,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        extradata: Vec<u8>,
    },
    ShareAvailable {
        room_id: RoomId,
        stream_id: StreamId,
        sender_name: String,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        extradata: Vec<u8>,
        view_secret: Vec<u8>,
    },
    ShareEnded {
        stream_id: StreamId,
    },
    ShareStartRejected {
        message: String,
    },
    Status(String),
    Error(String),
    AuthFailed {
        code: u16,
        message: String,
    },
    /// Durable local account/device state could not be loaded. The connection
    /// remains available for public rooms, while DMs and device administration
    /// are disabled until the identity is repaired or this installation is
    /// linked again from an active device.
    LocalIdentityUnavailable {
        message: String,
    },
    /// The server selected plaintext ExternalSecureLink transport, while this
    /// saved server still requires chatt-native encryption.
    NativeEncryptionRequired,
    ReconnectScheduled {
        retry_in: Duration,
        reason: String,
    },
    /// UDP media reachability to the server changed. `udp_ok: false` after
    /// repeated `Bind` retries go unconfirmed; `true` once a `UdpBound` finally
    /// lands after such a failure.
    MediaConnectivity {
        udp_ok: bool,
    },
    WorkerStopped {
        reason: String,
    },
}

/// Results emitted only by the short-lived invite/open/device pairing workers.
#[derive(Clone, Debug)]
pub(crate) enum PairingEvent {
    InviteSucceeded,
    Failed(String),
    UsernameTaken(String),
    OpenSucceeded {
        token: String,
        server_public_key: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
    },
    OpenNeedsPassword {
        retry: bool,
        server_public_key: String,
    },
    DeviceSucceeded {
        token: String,
        username: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
        server_public_key: String,
    },
    DeviceIdentityExists {
        message: String,
    },
    DeviceFailed {
        message: String,
    },
}

/// Routes a [`NetworkCommand`] to the owning actor and wakes its event loop.
#[derive(Clone)]
pub struct CommandSender {
    tx: Sender<NetworkCommand>,
    waker: Arc<Waker>,
    voice: Option<voice::VoiceInputSender>,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl CommandSender {
    /// Sends a command and wakes the worker's poll loop.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] if the worker has stopped and the channel is closed.
    pub fn send(&self, command: NetworkCommand) -> Result<(), SendError<NetworkCommand>> {
        if !self.alive.load(std::sync::atomic::Ordering::Acquire) {
            return Err(SendError(command));
        }
        if matches!(
            command,
            NetworkCommand::LocalVoicePacket(_)
                | NetworkCommand::SequencedLocalVoicePacket { .. }
                | NetworkCommand::SetPlaybackSink(_)
                | NetworkCommand::PlaybackFeedback(_)
        ) && let Some(voice) = &self.voice
        {
            return voice.send(command);
        }
        self.tx.send(command)?;
        let _ = self.waker.wake();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(tx: Sender<NetworkCommand>) -> Self {
        let poll = Poll::new().expect("create test poll");
        let waker = Arc::new(Waker::new(poll.registry(), WAKE).expect("create test waker"));
        Self {
            tx,
            waker,
            voice: None,
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }
}

pub struct NetworkClient {
    tx: CommandSender,
    worker: Option<JoinHandle<()>>,
    voice: Option<voice::VoiceLoopHandle>,
}

impl NetworkClient {
    pub fn spawn(config: ClientConfig, events: NetworkEventSender) -> Result<Self, String> {
        kvlog::info!(
            "network client spawning",
            username = config.username.as_str(),
            tcp_addr = %config.tcp_addr,
            udp_addr = %config.udp_addr
        );
        let poll = Poll::new().map_err(|error| format!("failed to create poll: {error}"))?;
        let waker = Arc::new(
            Waker::new(poll.registry(), WAKE)
                .map_err(|error| format!("failed to create waker: {error}"))?,
        );
        let mut voice = voice::VoiceLoopHandle::spawn(events.clone(), Arc::clone(&waker))?;
        let voice_input = voice.input_sender();
        let voice_control = voice.control();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (tx, rx) = mpsc::channel();
        let tx = CommandSender {
            tx,
            waker: Arc::clone(&waker),
            voice: Some(voice_input),
            alive: Arc::clone(&alive),
        };
        let panic_events = events.clone();
        let worker_alive = Arc::clone(&alive);
        let voice_shutdown = voice_control.clone();
        let worker = thread::Builder::new()
            .name("chatt-net".to_string())
            // 1M. This thread runs the mio loop, ChaCha20-Poly1305/X25519 crypto, jsony
            // (de)serialization, and P2P/STUN state machines. Serialization depth is not bounded
            // by inspection, so keep an overly safe margin over the default 2M.
            .stack_size(1024 * 1024)
            .spawn(move || {
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    run_worker(config, events, rx, poll, waker, voice_control);
                }));
                let _ = voice_shutdown.submit(voice::VoiceCommand::Shutdown);
                if result.is_err() {
                    kvlog::error!("network worker panicked");
                    let _ = panic_events.send(NetworkEvent::WorkerStopped {
                        reason: "network worker panicked".to_string(),
                    });
                }
                worker_alive.store(false, std::sync::atomic::Ordering::Release);
            })
            .map_err(|error| {
                voice.stop();
                format!("failed to spawn network worker: {error}")
            })?;
        Ok(Self {
            tx,
            worker: Some(worker),
            voice: Some(voice),
        })
    }

    pub fn sender(&self) -> CommandSender {
        self.tx.clone()
    }

    pub fn try_send(&self, command: NetworkCommand) -> Result<(), SendError<NetworkCommand>> {
        self.tx.send(command)
    }

    pub fn is_worker_finished(&self) -> bool {
        self.worker.as_ref().is_some_and(JoinHandle::is_finished)
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    #[cfg(test)]
    pub(crate) fn from_parts_for_test(tx: Sender<NetworkCommand>) -> Self {
        Self {
            tx: CommandSender::for_test(tx),
            worker: None,
            voice: None,
        }
    }

    fn stop_inner(&mut self) {
        let _ = self.tx.send(NetworkCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(mut voice) = self.voice.take() {
            voice.stop();
        }
    }
}

impl Drop for NetworkClient {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

pub fn spawn_pair_once(
    config: ClientConfig,
    pairing_code: String,
    events: PairingEventSender,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("chatt-pair".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let event = match pair_once(&config, pairing_code) {
                Ok(()) => PairingEvent::InviteSucceeded,
                Err(PairFailure::UsernameTaken(message)) => PairingEvent::UsernameTaken(message),
                Err(PairFailure::Other(error)) => PairingEvent::Failed(error),
            };
            let _ = events.send(event);
        })
        .map_err(|error| format!("failed to spawn pairing worker: {error}"))
}

pub(crate) const PAIRING_CANCELABLE: u8 = 0;
pub(crate) const PAIRING_COMMITTING: u8 = 1;
pub(crate) const PAIRING_CANCELED: u8 = 2;

pub fn spawn_device_pair_once(
    config: ClientConfig,
    ticket: DeviceLinkTicket,
    device_name: String,
    overwrite_existing: bool,
    cancellation: Arc<AtomicU8>,
    events: PairingEventSender,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("chatt-device-pair".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut ticket = ticket;
            let event = match device_pair_once(
                &config,
                &ticket,
                &device_name,
                overwrite_existing,
                &cancellation,
            ) {
                Ok((token, username, udp_addr, udp_probe_addr, server_public_key)) => {
                    PairingEvent::DeviceSucceeded {
                        token,
                        username,
                        udp_addr,
                        udp_probe_addr,
                        server_public_key,
                    }
                }
                Err(DevicePairFailure::IdentityExists { message }) => {
                    PairingEvent::DeviceIdentityExists { message }
                }
                Err(DevicePairFailure::Other(message)) => PairingEvent::DeviceFailed { message },
            };
            ticket.pairing_secret.zeroize();
            let _ = events.send(event);
        })
        .map_err(|error| format!("failed to spawn device-pairing worker: {error}"))
}

fn device_pair_once(
    config: &ClientConfig,
    ticket: &DeviceLinkTicket,
    device_name: &str,
    overwrite_existing: bool,
    cancellation: &AtomicU8,
) -> Result<(String, String, String, Option<String>, String), DevicePairFailure> {
    let (mut stream, transport, trusted) = connect_and_handshake(config, false)?;
    let secrets = crate::device_link::derive_pairing_secrets(&ticket.pairing_secret, &trusted)?;
    let redemption_secret = secrets.redemption_secret.as_str();
    let mut control = transport.control_record();
    write_blocking_control(
        &mut stream,
        &mut control,
        ClientControl::FetchDeviceLink {
            redemption_secret: redemption_secret.to_string(),
        },
    )?;
    let (enrollment_bundle, current_roster, user_id, username) = loop {
        let frame = read_blocking_frame(&mut stream)
            .map_err(|error| format!("failed to read device-link bundle: {error}"))?;
        let plaintext = control
            .open_next(CHANNEL_CONTROL, &frame)
            .map_err(|error| error.to_string())?;
        match decode_server_control(&plaintext)? {
            ServerControl::DeviceLinkBundle {
                enrollment_bundle,
                current_roster,
                user_id,
                username,
            } => break (enrollment_bundle, current_roster, user_id, username),
            ServerControl::Error { message, .. } => return Err(message.into()),
            _ => {}
        }
    };
    let secret_hash = crate::device_link::redemption_secret_hash(redemption_secret);
    let enrollment = crate::device_link::open_enrollment(
        &enrollment_bundle,
        &secrets.enrollment_key,
        &trusted,
        &secret_hash,
    )?;
    if cancellation.load(Ordering::Acquire) == PAIRING_CANCELED {
        return Err(DevicePairFailure::Other("pairing canceled".to_string()));
    }
    let data_dir = config.data_dir.as_deref().ok_or_else(|| {
        DevicePairFailure::Other(
            "HOME is not set; cannot store the E2E device identity".to_string(),
        )
    })?;
    if enrollment.current_roster != current_roster {
        return Err(DevicePairFailure::Other(
            "the encrypted pairing roster does not match the server's current roster".to_string(),
        ));
    }
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let mls_dir = mls_installation_dir(data_dir, &trusted, user_id);
    let bootstrap_path = mls_dir.join("mls-bootstrap.bin");
    let attempt_id = rpc::ids::PairAttemptId(
        secret_hash[..16]
            .try_into()
            .expect("SHA-256 device-link hash has 16 bytes"),
    );
    let pending_bearer = match chatt_mls::load_bootstrap(&bootstrap_path) {
        chatt_mls::BootstrapLoad::Loaded(bootstrap) => match bootstrap.state {
            chatt_mls::BootstrapState::PendingPair {
                attempt_id: stored_attempt,
                redemption_secret,
                bearer_token,
            } if stored_attempt == attempt_id
                && redemption_secret == secrets.redemption_secret.as_str() =>
            {
                Some(bearer_token)
            }
            _ if !overwrite_existing => {
                return Err(DevicePairFailure::IdentityExists {
                    message: format!(
                        "Encrypted MLS state already exists at {}. Overwrite will preserve it in a timestamped backup and create a newly linked installation. Continue?",
                        mls_dir.display()
                    ),
                });
            }
            _ => {
                let backup = data_dir.join(format!("mls-replaced-{}", unix_now_ms()));
                fs::rename(&mls_dir, &backup).map_err(|error| {
                    DevicePairFailure::Other(format!(
                        "failed to preserve existing MLS state at {}: {error}",
                        backup.display()
                    ))
                })?;
                None
            }
        },
        chatt_mls::BootstrapLoad::Unreadable(_) if !overwrite_existing => {
            return Err(DevicePairFailure::IdentityExists {
                message: format!(
                    "Encrypted MLS state at {} is unreadable. Overwrite will preserve it in a timestamped backup and create a new linked installation. Continue?",
                    mls_dir.display()
                ),
            });
        }
        chatt_mls::BootstrapLoad::Unreadable(_) => {
            let backup = data_dir.join(format!("mls-replaced-{}", unix_now_ms()));
            fs::rename(&mls_dir, &backup).map_err(|error| {
                DevicePairFailure::Other(format!(
                    "failed to preserve unreadable MLS state at {}: {error}",
                    backup.display()
                ))
            })?;
            None
        }
        chatt_mls::BootstrapLoad::Missing => None,
    };
    let bearer_token = match pending_bearer {
        Some(bearer) => bearer,
        None => {
            let mut bearer_secret = [0u8; KEY_LEN];
            rng.fill(&mut bearer_secret).map_err(|_| {
                DevicePairFailure::Other("failed to generate device bearer".to_string())
            })?;
            format!("tct2_{}", encode_hex(&bearer_secret))
        }
    };
    let mut installation = LocalInstallation::create_pending_pair(
        &mls_dir,
        trusted,
        user_id,
        device_name,
        enrollment.authority_seed,
        &current_roster,
        attempt_id,
        redemption_secret,
        &bearer_token,
    )?;
    let key_packages = installation.client.pending_pair_key_packages(
        installation.bootstrap.device_id,
        usize::from(MLS_KEY_PACKAGE_TARGET),
    )?;
    if cancellation
        .compare_exchange(
            PAIRING_CANCELABLE,
            PAIRING_COMMITTING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return Err(DevicePairFailure::Other("pairing canceled".to_string()));
    }
    write_blocking_control(
        &mut stream,
        &mut control,
        ClientControl::RedeemDeviceLink {
            redemption_secret: redemption_secret.to_string(),
            attempt_id,
            expected_roster: rpc::identity::roster_checkpoint(&current_roster),
            roster: installation.bootstrap.own_roster.clone(),
            key_packages,
            bearer_token: bearer_token.clone(),
            receive_files: config.file_policy.receives_any(),
            file_receive_limit_bytes: config.file_policy.advertised_limit(),
        },
    )?;
    loop {
        let frame = read_blocking_frame(&mut stream)
            .map_err(|error| format!("failed to read device-link redemption: {error}"))?;
        let plaintext = control
            .open_next(CHANNEL_CONTROL, &frame)
            .map_err(|error| error.to_string())?;
        match decode_server_control(&plaintext)? {
            ServerControl::DeviceLinked {
                token,
                username: linked_username,
                udp_addr,
                udp_probe_addr,
                user_id: linked_user_id,
                ..
            } => {
                if linked_user_id != user_id || linked_username != username {
                    return Err("server linked the device to a different account"
                        .to_string()
                        .into());
                }
                if token != bearer_token {
                    return Err("server confirmed a different device bearer token"
                        .to_string()
                        .into());
                }
                installation.mark_pair_active()?;
                return Ok((
                    token,
                    username,
                    udp_addr,
                    udp_probe_addr,
                    encode_hex(&trusted),
                ));
            }
            ServerControl::Error { message, .. } => return Err(message.into()),
            _ => {}
        }
    }
}

enum OpenPairOutcome {
    Paired {
        token: String,
        server_public_key: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
    },
    NeedsPassword {
        retry: bool,
        server_public_key: String,
    },
    UsernameTaken(String),
    Failed(String),
}

/// Runs one open-pairing attempt: TOFU handshake, send `OpenPair`, await the
/// server's decision. Returns the issued token and trusted key on success, a
/// password prompt request when the server demands one, or a fatal error.
fn open_pair_once(
    config: &ClientConfig,
    password: String,
    existing_token: String,
) -> OpenPairOutcome {
    let (mut stream, transport, trusted) = match connect_and_handshake(config, true) {
        Ok(value) => value,
        Err(error) => return OpenPairOutcome::Failed(error),
    };
    let mut control = transport.control_record();
    let request = ClientControl::OpenPair {
        username: config.username.clone(),
        password,
        existing_token,
        receive_files: config.file_policy.receives_any(),
        file_receive_limit_bytes: config.file_policy.advertised_limit(),
    };
    if let Err(error) = write_blocking_control(&mut stream, &mut control, request) {
        return OpenPairOutcome::Failed(error);
    }
    loop {
        let frame = match read_blocking_frame(&mut stream) {
            Ok(frame) => frame,
            Err(error) => {
                return OpenPairOutcome::Failed(format!(
                    "failed to read pairing response: {error}"
                ));
            }
        };
        let plaintext = match control.open_next(CHANNEL_CONTROL, &frame) {
            Ok(plaintext) => plaintext,
            Err(error) => return OpenPairOutcome::Failed(error.to_string()),
        };
        match decode_server_control(&plaintext) {
            Ok(ServerControl::OpenPaired {
                token,
                udp_addr,
                udp_probe_addr,
                ..
            }) => {
                return OpenPairOutcome::Paired {
                    token,
                    server_public_key: encode_hex(&trusted),
                    udp_addr,
                    udp_probe_addr,
                };
            }
            Ok(ServerControl::Error { code, message }) => {
                return match code {
                    ERROR_PASSWORD_REQUIRED => OpenPairOutcome::NeedsPassword {
                        retry: false,
                        server_public_key: encode_hex(&trusted),
                    },
                    ERROR_PASSWORD_MISMATCH => OpenPairOutcome::NeedsPassword {
                        retry: true,
                        server_public_key: encode_hex(&trusted),
                    },
                    ERROR_USERNAME_TAKEN => OpenPairOutcome::UsernameTaken(message),
                    _ => OpenPairOutcome::Failed(message),
                };
            }
            Ok(_) => {}
            Err(error) => return OpenPairOutcome::Failed(error),
        }
    }
}

pub fn spawn_open_pair_once(
    config: ClientConfig,
    password: String,
    existing_token: String,
    events: PairingEventSender,
) -> Result<JoinHandle<()>, String> {
    thread::Builder::new()
        .name("chatt-open-pair".to_string())
        .stack_size(256 * 1024)
        .spawn(move || {
            let event = match open_pair_once(&config, password, existing_token) {
                OpenPairOutcome::Paired {
                    token,
                    server_public_key,
                    udp_addr,
                    udp_probe_addr,
                } => PairingEvent::OpenSucceeded {
                    token,
                    server_public_key,
                    udp_addr,
                    udp_probe_addr,
                },
                OpenPairOutcome::NeedsPassword {
                    retry,
                    server_public_key,
                } => PairingEvent::OpenNeedsPassword {
                    retry,
                    server_public_key,
                },
                OpenPairOutcome::UsernameTaken(message) => PairingEvent::UsernameTaken(message),
                OpenPairOutcome::Failed(error) => PairingEvent::Failed(error),
            };
            let _ = events.send(event);
        })
        .map_err(|error| format!("failed to spawn open pairing worker: {error}"))
}

fn run_worker(
    config: ClientConfig,
    events: NetworkEventSender,
    commands: Receiver<NetworkCommand>,
    mut poll: Poll,
    waker: Arc<Waker>,
    voice: voice::VoiceControl,
) {
    kvlog::info!(
        "network worker starting",
        username = config.username.as_str(),
        tcp_addr = %config.tcp_addr,
        udp_addr = %config.udp_addr
    );
    let mut reconnect = ReconnectSchedule::new();
    let mut pending_authenticated_commands = VecDeque::new();
    let mls_runtime = match mls_actor::Runtime::spawn(config.clone(), events.clone(), waker) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(NetworkEvent::WorkerStopped { reason: error });
            return;
        }
    };
    let mut mls_generation = 0u64;
    loop {
        let mut voice_outputs = voice::VoiceOutputBatch::default();
        let voice_stopped = voice.drain_outputs(&mut voice_outputs);
        if let Some(reason) = voice_outputs.fatal_failure.take() {
            let _ = events.send(NetworkEvent::WorkerStopped { reason });
            break;
        }
        if voice_stopped {
            let _ = events.send(NetworkEvent::WorkerStopped {
                reason: "voice worker stopped".to_string(),
            });
            break;
        }
        mls_generation = mls_generation.wrapping_add(1).max(1);
        let session_end = run_worker_inner(
            &config,
            &events,
            &commands,
            &mut pending_authenticated_commands,
            &mut poll,
            &mls_runtime,
            mls_generation,
            &voice,
        );
        let _ = voice.submit(voice::VoiceCommand::EndSession {
            generation: mls_generation,
        });
        let _ = mls_runtime.try_send(mls_actor::Input::EndSession {
            generation: mls_generation,
        });
        match session_end {
            SessionEnd::Shutdown => break,
            SessionEnd::ConnectFailed(reason) => {
                kvlog::warn!(
                    "network connection attempt failed",
                    reason = reason.as_str()
                );
                if schedule_reconnect(&events, &commands, &mut reconnect, &reason).is_shutdown() {
                    break;
                }
            }
            SessionEnd::Disconnected(reason) => {
                reconnect.reset();
                kvlog::warn!("network session disconnected", reason = reason.as_str());
                // A socket event already told us that an established session
                // ended. Retry it immediately once instead of imposing the
                // connection-attempt backoff before discovering whether the
                // server is already available again. If it is not, the next
                // iteration ends in ConnectFailed and uses the ordinary
                // bounded backoff below.
                report_reconnect_scheduled(&events, Duration::ZERO, &reason);
            }
            SessionEnd::AuthFailed { code, reason } => {
                kvlog::warn!("network auth failed", code, reason = reason.as_str());
                let _ = events.send(NetworkEvent::AuthFailed {
                    code,
                    message: reason,
                });
                break;
            }
            SessionEnd::LocalIdentityUnavailable(message) => {
                kvlog::error!("local E2E identity unavailable", error = message.as_str());
                let _ = events.send(NetworkEvent::LocalIdentityUnavailable { message });
                break;
            }
            SessionEnd::Fatal(reason) => {
                let _ = events.send(NetworkEvent::WorkerStopped { reason });
                break;
            }
        }
    }
    mls_runtime.stop();
    kvlog::info!("network worker stopped");
}

fn run_worker_inner(
    config: &ClientConfig,
    events: &NetworkEventSender,
    commands: &Receiver<NetworkCommand>,
    pending_authenticated_commands: &mut VecDeque<NetworkCommand>,
    poll: &mut Poll,
    mls_runtime: &mls_actor::Runtime,
    mls_generation: u64,
    voice_control: &voice::VoiceControl,
) -> SessionEnd {
    let (std_tcp, transport, _trusted) = match connect_and_handshake(config, false) {
        Ok(value) => value,
        Err(error) => return SessionEnd::ConnectFailed(error),
    };
    let transport_mode = transport.mode;
    let server_public_key = match pinned_server_public_key(config, false) {
        Ok(Some(key)) => key,
        Ok(None) => unreachable!("ordinary connections always pin a server key"),
        Err(error) => return SessionEnd::ConnectFailed(error),
    };
    if transport_mode == TransportMode::ExternalSecureLink && config.require_native_encryption {
        let _ = events.send(NetworkEvent::NativeEncryptionRequired);
        return SessionEnd::Shutdown;
    }
    let video_auth_key = transport.video_auth_key;
    let control = transport.control_record();
    let media = media::MediaProtection::from_transport(&transport);
    // P2P would bypass an outer secure link, so it is only available when chatt
    // secures the wire itself, regardless of the client's p2p config.
    let p2p_enabled = config.p2p_enabled && transport_mode == TransportMode::NativeEncrypted;
    let server_udp_addr = match resolve_endpoint(&config.udp_addr) {
        Ok(addr) => addr,
        Err(error) => return SessionEnd::ConnectFailed(format!("invalid UDP endpoint: {error}")),
    };
    let server_udp_probe_addr = match config.udp_probe_addr.as_deref() {
        Some(addr) => match resolve_endpoint(addr) {
            Ok(addr) => Some(addr),
            Err(error) => {
                return SessionEnd::ConnectFailed(format!("invalid UDP probe endpoint: {error}"));
            }
        },
        None => None,
    };
    let std_udp = match voice::bind_voice_udp_socket(if server_udp_addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    }) {
        Ok(socket) => socket,
        Err(error) => {
            return SessionEnd::ConnectFailed(format!("failed to bind UDP socket: {error}"));
        }
    };
    let udp_local_addr = match std_udp.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            return SessionEnd::ConnectFailed(format!(
                "failed to read UDP socket address: {error}"
            ));
        }
    };
    kvlog::info!("udp socket bound", addr = %udp_local_addr);
    if let Err(error) = std_tcp.set_nonblocking(true) {
        return SessionEnd::ConnectFailed(format!("failed to make TCP nonblocking: {error}"));
    }
    let initial_udp_bind = match voice::InitialUdpBind::prepare(&std_udp, &media, server_udp_addr) {
        Ok(bind) => bind,
        Err(error) => return SessionEnd::ConnectFailed(error),
    };

    let voice_start = voice::VoiceCommand::StartSession {
        generation: mls_generation,
        udp: std_udp,
        media,
        initial_bind_attempted: true,
        transport_mode,
        server_udp_addr,
        server_udp_probe_addr,
        p2p_enabled,
        candidate_privacy: config.candidate_privacy,
        prefer_ipv6: config.prefer_ipv6,
    };
    if voice_control.submit(voice_start).is_err() {
        return SessionEnd::ConnectFailed("client voice worker is unavailable".to_string());
    }

    let mut worker = WorkerState {
        mls_runtime,
        mls_generation,
        pending_mls_input: None,
        mls_server_pending: 0,
        deferred_server_control: None,
        mls_available: false,
        mls_session_ready: false,
        mls_outputs: Vec::new(),
        config: config.clone(),
        events: events.clone(),
        voice: voice_control,
        voice_generation: mls_generation,
        pending_initial_udp_bind: Some(initial_udp_bind),
        tcp: TcpStream::from_std(std_tcp),
        loop_work: WorkerWork::default(),
        read_buf: RecvBuffer::new(),
        // Starts true so the first loop iteration reads anything that arrived
        // before the socket was registered with the poll.
        tcp_readiness: Readiness::primed(),
        write_buf: WriteQueue::new(),
        control,
        transport_mode,
        video_auth_key,
        server_public_key,
        session_id: None,
        user_id: None,
        user_names: HashMap::new(),
        active_room: None,
        voice_room: None,
        pending_share_start: false,
        next_file_transfer: 1,
        outgoing_uploads: VecDeque::new(),
        upload_throttle: UploadThrottle::new(config.upload_rate_bytes),
        pending_local_files: HashMap::new(),
        next_bug_report: 1,
        outgoing_bug_reports: VecDeque::new(),
        incoming_files: HashMap::new(),
        skipped_untrusted_files: HashSet::new(),
        e2e: E2eState::new(None, None, &config.e2e_peer_pins, config.data_dir.clone()),
        mls_file_announcements: HashMap::new(),
        room_kinds: HashMap::new(),
        pending_mls_file_offers: HashMap::new(),
        pending_mls_file_bytes: 0,
        pending_mls_file_items: 0,
        pending_mls_file_warned_rooms: HashSet::new(),
        shutdown: false,
        disconnect_reason: None,
        auth_failure: None,
        local_identity_failure: None,
        shutdown_requested: false,
    };
    worker.loop_work.queue_tcp_read();

    // The poll outlives each session so its waker survives reconnects. A prior
    // session's sockets were dropped on return, closing their fds and clearing
    // them from the poll, so re-registering the same tokens here is sound.
    if let Err(error) = poll.registry().register(
        &mut worker.tcp,
        TCP,
        Interest::READABLE | Interest::WRITABLE,
    ) {
        return SessionEnd::ConnectFailed(format!("failed to register TCP socket: {error}"));
    }

    let auth_control = ClientControl::Authenticate {
        username: worker.config.username.clone(),
        token: worker.config.token.clone(),
        receive_files: worker.config.file_policy.receives_any(),
        file_receive_limit_bytes: worker.config.file_policy.advertised_limit(),
    };
    if let Err(error) = worker.queue_control(auth_control) {
        return SessionEnd::Disconnected(error);
    }
    kvlog::info!(
        "auth control queued",
        username = worker.config.username.as_str()
    );
    let _ = worker.events.send(NetworkEvent::Connected);

    let mut poll_events = Events::with_capacity(128);
    let mut voice_outputs = voice::VoiceOutputBatch::default();
    let mut poll_timeout = worker.next_poll_timeout(CommandDrainOutcome::Empty, Instant::now());
    while !worker.shutdown {
        if let Err(error) = poll.poll(&mut poll_events, Some(poll_timeout)) {
            if is_interrupted_io_error(&error) {
                kvlog::warn!("network poll interrupted", error = %error);
                continue;
            }
            return SessionEnd::Disconnected(format!("network poll failed: {error}"));
        }
        for event in poll_events.iter() {
            match event.token() {
                TCP => {
                    // Reading happens below through `tcp_readiness` so a read
                    // capped by `MAX_BUFFERED_SERVER_BYTES` is retried once
                    // control processing frees buffer space. Consuming the
                    // edge-triggered event without draining the socket would
                    // otherwise strand the remaining bytes forever.
                    let ready = MioReady::from_event(event);
                    if ready.readable_like() {
                        worker.tcp_readiness.mark_ready();
                        worker.loop_work.queue_tcp_read();
                    }
                    if ready.writable_like() && !worker.write_buf.is_empty() {
                        worker.loop_work.queue_tcp_write();
                    }
                }
                WAKE => {}
                _ => {}
            }
        }
        let voice_stopped = voice_control.drain_outputs(&mut voice_outputs);
        if let Some(reason) = voice_outputs.fatal_failure.take() {
            return SessionEnd::Fatal(reason);
        }
        if voice_stopped {
            return SessionEnd::Fatal("voice worker stopped".to_string());
        }
        if let Some((generation, reason)) = voice_outputs.session_failure.take()
            && generation == mls_generation
        {
            return SessionEnd::Disconnected(reason);
        }
        if let Some(publish) = voice_outputs.publish_p2p.take()
            && publish.generation == mls_generation
            && let Err(error) = worker.queue_control(ClientControl::PublishP2p {
                room_id: publish.room_id,
                generation: publish.candidate_generation,
                nat: publish.nat,
                tie_breaker: publish.tie_breaker,
                candidates: publish.candidates,
            })
        {
            return SessionEnd::Disconnected(error);
        }
        if let Err(error) = worker.run_loop_tasks() {
            return SessionEnd::Disconnected(error);
        }
        if let Err(error) = worker.drain_mls_outputs() {
            return SessionEnd::Disconnected(error);
        }
        if let Err(error) = worker.process_server_controls() {
            return SessionEnd::Disconnected(error);
        }

        let mut command_drain = CommandDrainOutcome::Empty;
        if worker.user_id.is_some()
            && worker.mls_session_ready
            && worker.pending_mls_input.is_none()
        {
            let mut handled = 0;
            while handled < MAX_COMMANDS_PER_ITERATION {
                let Some(command) = pending_authenticated_commands.pop_front() else {
                    break;
                };
                if let Err(error) = worker.handle_command(command) {
                    return SessionEnd::Disconnected(error);
                }
                handled += 1;
                if worker.pending_mls_input.is_some() {
                    break;
                }
            }
            if !pending_authenticated_commands.is_empty() {
                command_drain = CommandDrainOutcome::HitLimit;
            }
        }
        if command_drain != CommandDrainOutcome::HitLimit && worker.pending_mls_input.is_none() {
            command_drain =
                match drain_commands_with(commands, MAX_COMMANDS_PER_ITERATION, |command| {
                    if worker.pending_mls_input.is_some() {
                        pending_authenticated_commands.push_back(command);
                        return Ok(());
                    }
                    if (worker.user_id.is_none() || !worker.mls_session_ready)
                        && command.requires_authenticated_session()
                    {
                        pending_authenticated_commands.push_back(command);
                        Ok(())
                    } else {
                        worker.handle_command(command)
                    }
                }) {
                    Ok(outcome) => outcome,
                    Err(error) => return SessionEnd::Disconnected(error),
                };
        }
        if command_drain == CommandDrainOutcome::Disconnected {
            worker.shutdown = true;
            worker.shutdown_requested = true;
        }
        if worker.shutdown {
            break;
        }
        worker.upload_throttle.refill(Instant::now());
        if let Err(error) = worker.poll_uploads() {
            return SessionEnd::Disconnected(error);
        }
        if let Err(error) = worker.poll_bug_reports() {
            return SessionEnd::Disconnected(error);
        }
        let now = Instant::now();
        poll_timeout = worker.next_poll_timeout(command_drain, now);
        #[cfg(debug_assertions)]
        if poll_timeout != Duration::ZERO {
            worker.debug_assert_no_immediate_work(command_drain);
        }
    }
    let session_end = select_session_end(
        worker.shutdown_requested,
        worker.auth_failure.take(),
        worker.local_identity_failure.take(),
        worker.disconnect_reason.take(),
    );
    if matches!(session_end, SessionEnd::Shutdown) {
        kvlog::info!("network worker shutdown requested");
    }
    session_end
}

fn report_initial_udp_bind_send_error(events: &NetworkEventSender, error: Option<io::Error>) {
    if let Some(error) = error {
        kvlog::warn!("initial UDP bind send failed", error = %error);
        let _ = events.send(NetworkEvent::Error(format!("UDP send failed: {error}")));
    }
}

enum SessionEnd {
    Shutdown,
    ConnectFailed(String),
    Disconnected(String),
    AuthFailed { code: u16, reason: String },
    LocalIdentityUnavailable(String),
    Fatal(String),
}

fn select_session_end(
    shutdown_requested: bool,
    auth_failure: Option<(u16, String)>,
    local_identity_failure: Option<String>,
    disconnect_reason: Option<String>,
) -> SessionEnd {
    if shutdown_requested {
        SessionEnd::Shutdown
    } else if let Some((code, reason)) = auth_failure {
        SessionEnd::AuthFailed { code, reason }
    } else if let Some(message) = local_identity_failure {
        SessionEnd::LocalIdentityUnavailable(message)
    } else if let Some(reason) = disconnect_reason {
        SessionEnd::Disconnected(reason)
    } else {
        SessionEnd::Shutdown
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandDrainOutcome {
    Empty,
    HitLimit,
    Disconnected,
}

fn drain_commands_with<F>(
    commands: &Receiver<NetworkCommand>,
    limit: usize,
    mut handle: F,
) -> Result<CommandDrainOutcome, String>
where
    F: FnMut(NetworkCommand) -> Result<(), String>,
{
    debug_assert!(limit > 0);
    let mut handled = 0;
    while handled < limit {
        match commands.try_recv() {
            Ok(command) => {
                handle(command)?;
                handled += 1;
            }
            Err(TryRecvError::Empty) => return Ok(CommandDrainOutcome::Empty),
            Err(TryRecvError::Disconnected) => return Ok(CommandDrainOutcome::Disconnected),
        }
    }
    Ok(CommandDrainOutcome::HitLimit)
}

#[derive(Clone, Copy, Debug, Default)]
struct ReconnectSchedule {
    attempts: u32,
}

impl ReconnectSchedule {
    fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }

    fn next_delay(&mut self) -> Duration {
        self.attempts = self.attempts.saturating_add(1);
        match self.attempts {
            1..=5 => Duration::from_secs(1),
            6..=10 => Duration::from_secs(2),
            _ => Duration::from_secs(5),
        }
    }
}

enum RetryWait {
    Retry,
    Shutdown,
}

impl RetryWait {
    fn is_shutdown(&self) -> bool {
        matches!(self, Self::Shutdown)
    }
}

fn schedule_reconnect(
    events: &NetworkEventSender,
    commands: &Receiver<NetworkCommand>,
    reconnect: &mut ReconnectSchedule,
    reason: &str,
) -> RetryWait {
    let delay = reconnect.next_delay();
    report_reconnect_scheduled(events, delay, reason);
    wait_for_reconnect(commands, delay)
}

fn report_reconnect_scheduled(
    events: &NetworkEventSender,
    delay: Duration,
    reason: &str,
) {
    kvlog::info!(
        "network reconnect scheduled",
        delay_ms = delay.as_millis() as u64,
        reason
    );
    let _ = events.send(NetworkEvent::ReconnectScheduled {
        retry_in: delay,
        reason: reason.to_string(),
    });
}

fn wait_for_reconnect(commands: &Receiver<NetworkCommand>, delay: Duration) -> RetryWait {
    let deadline = Instant::now() + delay;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return RetryWait::Retry;
        };
        match commands.recv_timeout(remaining) {
            #[cfg(test)]
            Ok(NetworkCommand::RetryConnection) => {
                kvlog::info!("network reconnect woken by embedded server lifecycle");
                return RetryWait::Retry;
            }
            Ok(NetworkCommand::Shutdown) => {
                kvlog::info!("shutdown command handling while disconnected");
                return RetryWait::Shutdown;
            }
            Ok(command) => {
                if !matches!(
                    command,
                    NetworkCommand::LocalVoicePacket(_)
                        | NetworkCommand::SequencedLocalVoicePacket { .. }
                        | NetworkCommand::SetPlaybackSink(_)
                ) {
                    kvlog::info!(
                        "network command dropped while disconnected",
                        kind = network_command_kind(&command)
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => return RetryWait::Retry,
            Err(RecvTimeoutError::Disconnected) => return RetryWait::Shutdown,
        }
    }
}

fn connect_and_handshake(
    config: &ClientConfig,
    allow_tofu: bool,
) -> Result<(StdTcpStream, SessionTransport, [u8; 32]), String> {
    kvlog::info!(
        "tcp connecting",
        tcp_addr = %config.tcp_addr,
        username = config.username.as_str()
    );
    let addresses = config
        .tcp_addr
        .to_socket_addrs()
        .map_err(|error| format!("failed to resolve server address: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err("server address resolved to no endpoints".to_string());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_error = None;
    let mut connected = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match StdTcpStream::connect_timeout(&address, remaining.min(Duration::from_secs(10))) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let mut stream = connected.ok_or_else(|| {
        format!(
            "failed to connect to server: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "connection deadline exceeded".to_string())
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("failed to set TCP read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("failed to set TCP write timeout: {error}"))?;
    stream
        .set_nodelay(true)
        .map_err(|error| format!("failed to set TCP_NODELAY: {error}"))?;
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let client = generate_client_hello(&rng).map_err(|error| error.to_string())?;
    let hello = encode_client_hello(&client.hello);
    let mut framed = Vec::new();
    frame::encode_frame(&hello, &mut framed).map_err(|error| error.to_string())?;
    kvlog::info!(
        "client hello sending",
        hello_size = hello.len(),
        frame_size = framed.len()
    );
    stream
        .write_all(&framed)
        .map_err(|error| format!("failed to write client hello: {error}"))?;
    let response = read_blocking_frame(&mut stream)
        .map_err(|error| format!("failed to read server hello: {error}"))?;
    let server_hello = decode_server_hello(&response)?;
    let pinned_server_public_key = pinned_server_public_key(config, allow_tofu)?;
    let (transport, trusted_key) = complete_client_transport_handshake(
        client,
        &server_hello,
        pinned_server_public_key.as_ref(),
    )
    .map_err(|error| error.to_string())?;
    kvlog::info!(
        "tcp handshake completed",
        transport_mode = transport.mode.as_str(),
        media_route_id = transport.route_id
    );
    Ok((stream, transport, trusted_key))
}

/// Why an invite pairing attempt failed. `UsernameTaken` is separated so the app
/// can send the user back to the username field; everything else is `Other`.
enum PairFailure {
    UsernameTaken(String),
    Other(String),
}

enum DevicePairFailure {
    IdentityExists { message: String },
    Other(String),
}

impl From<String> for DevicePairFailure {
    fn from(error: String) -> Self {
        Self::Other(error)
    }
}

fn pair_once(config: &ClientConfig, pairing_code: String) -> Result<(), PairFailure> {
    let (mut stream, transport, _trusted) =
        connect_and_handshake(config, false).map_err(PairFailure::Other)?;
    let mut control = transport.control_record();
    write_blocking_control(
        &mut stream,
        &mut control,
        ClientControl::Pair {
            username: config.username.clone(),
            pairing_code,
            token: config.token.clone(),
            receive_files: config.file_policy.receives_any(),
            file_receive_limit_bytes: config.file_policy.advertised_limit(),
        },
    )
    .map_err(PairFailure::Other)?;

    loop {
        let frame = read_blocking_frame(&mut stream).map_err(|error| {
            PairFailure::Other(format!("failed to read pairing response: {error}"))
        })?;
        let plaintext = control
            .open_next(CHANNEL_CONTROL, &frame)
            .map_err(|error| PairFailure::Other(error.to_string()))?;
        match decode_server_control(&plaintext).map_err(PairFailure::Other)? {
            ServerControl::Authenticated { .. } => return Ok(()),
            ServerControl::Error {
                code: ERROR_USERNAME_TAKEN,
                message,
            } => return Err(PairFailure::UsernameTaken(message)),
            ServerControl::Error { message, .. } => return Err(PairFailure::Other(message)),
            _ => {}
        }
    }
}

fn write_blocking_control(
    stream: &mut StdTcpStream,
    control: &mut RecordProtection,
    message: ClientControl,
) -> Result<(), String> {
    let payload = encode_client_control(&message)?;
    let encrypted = control
        .seal_next(CHANNEL_CONTROL, &payload)
        .map_err(|error| error.to_string())?;
    let mut framed = Vec::new();
    frame::encode_frame(&encrypted, &mut framed).map_err(|error| error.to_string())?;
    stream
        .write_all(&framed)
        .map_err(|error| format!("failed to write pairing request: {error}"))
}

fn resolve_endpoint(endpoint: &str) -> Result<SocketAddr, String> {
    endpoint
        .to_socket_addrs()
        .map_err(|error| format!("{endpoint}: {error}"))?
        .next()
        .ok_or_else(|| format!("{endpoint}: no socket addresses resolved"))
}

/// Resolves the Ed25519 key to pin for a connection.
///
/// A configured key is always pinned. With no configured key, `allow_tofu`
/// returns `None` so the server's presented key is trusted on first use (open
/// pairing), while a normal connection falls back to pinning the well-known dev
/// server key.
fn pinned_server_public_key(
    config: &ClientConfig,
    allow_tofu: bool,
) -> Result<Option<[u8; 32]>, String> {
    match config.server_public_key.as_deref() {
        Some(public_key) => ed25519_public_key_from_hex(public_key)
            .map(Some)
            .map_err(|error| format!("invalid configured server-public-key: {error}")),
        None if allow_tofu => Ok(None),
        None => Ok(Some(dev_server_public_key())),
    }
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

struct WorkerState<'a> {
    mls_runtime: &'a mls_actor::Runtime,
    mls_generation: u64,
    pending_mls_input: Option<mls_actor::Input>,
    mls_server_pending: usize,
    deferred_server_control: Option<ServerControl>,
    mls_available: bool,
    mls_session_ready: bool,
    mls_outputs: Vec<mls_actor::Output>,
    config: ClientConfig,
    events: NetworkEventSender,
    voice: &'a voice::VoiceControl,
    voice_generation: u64,
    pending_initial_udp_bind: Option<voice::InitialUdpBind>,
    tcp: TcpStream,
    loop_work: WorkerWork,
    read_buf: RecvBuffer,
    /// Whether the TCP socket may hold unread bytes. Set on every readable
    /// poll event and cleared only when a read drains to `WouldBlock` or
    /// end-of-stream. [`read_tcp`](WorkerState::read_tcp) stops early once
    /// `read_buf` reaches [`MAX_BUFFERED_SERVER_BYTES`], and the poll is
    /// edge-triggered, so without this state a capped read would strand the
    /// remaining kernel bytes: no new readable edge fires while the sender is
    /// blocked on our zero receive window.
    tcp_readiness: Readiness,
    write_buf: WriteQueue,
    control: RecordProtection,
    /// The negotiated transport mode is retained for control/UI/video checks.
    transport_mode: TransportMode,
    /// Session-authentication key for external-link video connection setup.
    video_auth_key: [u8; KEY_LEN],
    /// Server identity is part of every AccountId and MLS store namespace,
    /// preventing credentials or room state crossing server trust domains.
    server_public_key: [u8; 32],
    session_id: Option<SessionId>,
    user_id: Option<UserId>,
    user_names: HashMap<UserId, String>,
    /// The room the app is viewing, target for uploads injected outside the
    /// app thread. Set by [`NetworkCommand::SetActiveRoom`].
    active_room: Option<RoomId>,
    /// The room whose voice call this client is in, target for screen shares
    /// and P2P publication.
    voice_room: Option<RoomId>,
    pending_share_start: bool,
    next_file_transfer: u64,
    outgoing_uploads: VecDeque<OutgoingUpload>,
    upload_throttle: UploadThrottle,
    pending_local_files: HashMap<FileTransferId, PendingLocalFile>,
    next_bug_report: u64,
    outgoing_bug_reports: VecDeque<OutgoingBugReport>,
    incoming_files: HashMap<FileTransferId, IncomingFile>,
    skipped_untrusted_files: HashSet<FileTransferId>,
    /// Local stable-AccountId pins and manual verification levels.
    e2e: E2eState,
    mls_file_announcements: HashMap<EventId, MlsFileCache>,
    room_kinds: HashMap<RoomId, RoomKind>,
    pending_mls_file_offers: HashMap<RoomId, PendingMlsFileRoom>,
    pending_mls_file_bytes: usize,
    pending_mls_file_items: usize,
    /// Rooms already warned about file offers whose MLS announcement backlog
    /// overflowed during this network session.
    pending_mls_file_warned_rooms: HashSet<RoomId>,
    shutdown: bool,
    disconnect_reason: Option<String>,
    auth_failure: Option<(u16, String)>,
    local_identity_failure: Option<String>,
    /// Distinguishes a terminal local request from a session failure which
    /// should reconnect. Both may be observed in the same poll iteration.
    shutdown_requested: bool,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct PendingCreatedDeviceLink {
    secret_hash: Vec<u8>,
    pairing_string: String,
}

#[derive(Debug)]
struct PendingMlsRoom {
    member_users: Vec<UserId>,
    rosters: HashMap<UserId, rpc::identity::SignedDeviceRoster>,
    descriptor: Option<rpc::mls::EncryptedRoomDescriptor>,
    checkpoints: Vec<rpc::identity::RosterCheckpoint>,
    /// KeyPackage requests currently in flight.
    awaiting_packages: HashSet<DeviceId>,
    /// Devices whose last request found an empty pool. The room remains
    /// intact so a later publication can retry just these devices.
    missing_packages: HashSet<DeviceId>,
    device_accounts: HashMap<DeviceId, AccountId>,
    packages: Vec<(DeviceId, Vec<u8>)>,
}

#[derive(Debug)]
struct PendingMlsGroupInfo {
    descriptor: rpc::mls::EncryptedRoomDescriptor,
    group_info: Vec<u8>,
    awaiting_rosters: HashSet<UserId>,
    /// A Welcome fetch issued after this canonical GroupInfo is waiting for
    /// its response. External rejoin is only a fallback after that response
    /// proves this device has no usable Welcome.
    awaiting_welcome_check: bool,
}

#[derive(Clone, Debug)]
struct MlsFileCache {
    room_id: RoomId,
    sender: UserId,
    timestamp_ms: u64,
    file: rpc::mls::MlsFileAnnouncement,
}

#[derive(Debug)]
enum PendingMlsCommit {
    RoomCreation(rpc::mls::EncryptedRoomDescriptor),
    MemberUpdate {
        descriptor: rpc::mls::EncryptedRoomDescriptor,
        request: ClientControl,
    },
    ExternalRejoin {
        descriptor: rpc::mls::EncryptedRoomDescriptor,
        request: ClientControl,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum MlsRetryKey {
    Application(RoomId, EventId),
    Commit(RoomId),
}

#[derive(Clone, Debug)]
struct DelayedMlsRetry {
    request: ClientControl,
    failures: u32,
    retry_at: Option<Instant>,
}

struct PendingAuthentication {
    session_id: SessionId,
    user_id: UserId,
    rooms: Vec<RoomInfo>,
    users: Vec<UserSummary>,
    default_room: RoomId,
}

enum PendingMlsFileOffer {
    FileOffered {
        file: FileMetadata,
        contents: bool,
        skipped_untrusted: bool,
    },
}

#[derive(Default)]
struct PendingMlsFileRoom {
    items: VecDeque<PendingMlsFileOffer>,
    bytes: usize,
}

impl PendingMlsFileOffer {
    fn room_id(&self) -> Option<RoomId> {
        match self {
            Self::FileOffered { file, .. } => Some(file.room_id),
        }
    }

    fn logical_items(&self) -> usize {
        1
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::FileOffered { file, .. } => {
                if file.mls_event_id.is_some() {
                    272
                } else {
                    256
                }
            }
        }
    }
}
/// transfers cannot bank credit and then burst.
struct UploadThrottle {
    /// Ceiling in bytes per second. `0` disables throttling.
    rate: u64,
    /// Byte budget available for the next chunk.
    tokens: u64,
    /// When `tokens` was last refilled.
    last: Instant,
}

impl UploadThrottle {
    fn new(rate: u64) -> Self {
        Self {
            rate,
            tokens: rate,
            last: Instant::now(),
        }
    }

    /// Replaces the rate, clamping the current budget to the new ceiling.
    fn set_rate(&mut self, rate: u64) {
        self.rate = rate;
        self.tokens = self.tokens.min(rate);
    }

    /// Accrues tokens for the elapsed time, capped at one second's worth.
    fn refill(&mut self, now: Instant) {
        if self.rate == 0 {
            return;
        }
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        let gained = (elapsed * self.rate as f64) as u64;
        self.tokens = self.tokens.saturating_add(gained).min(self.rate);
    }

    /// The byte budget for the next chunk, [`u64::MAX`] when unthrottled.
    fn budget(&self) -> u64 {
        if self.rate == 0 {
            u64::MAX
        } else {
            self.tokens
        }
    }

    fn is_limited(&self) -> bool {
        self.rate != 0
    }

    /// Deducts `bytes` after a chunk is queued.
    fn consume(&mut self, bytes: u64) {
        if self.rate == 0 {
            return;
        }
        self.tokens = self.tokens.saturating_sub(bytes);
    }

    /// The delay until `bytes` of budget accrues, for parking the poll loop
    /// instead of busy-spinning. Zero when already available or unthrottled.
    fn delay_until(&self, bytes: u64) -> Duration {
        if self.rate == 0 || self.tokens >= bytes {
            return Duration::ZERO;
        }
        let needed = bytes - self.tokens;
        Duration::from_secs_f64(needed as f64 / self.rate as f64)
    }
}

struct OutgoingUpload {
    transfer_id: FileTransferId,
    /// Server-assigned identity shared with the room's chat announcement.
    server_metadata: Option<FileMetadata>,
    room_id: RoomId,
    name: String,
    size: u64,
    file: File,
    /// Source path retained for durable encrypted-upload recovery. Staged
    /// sources are unlinked only once their durable intent is completed.
    source_path: PathBuf,
    delete_source_when_done: bool,
    source_offset: u64,
    /// Raw source bytes fed to the compressor since its encoded output was last
    /// fully queued. Used only for throttled compressed uploads to prevent
    /// scanning an entire highly-compressible file ahead of the network pace.
    source_read_ahead: u64,
    wire_offset: u64,
    source_prefix: Vec<u8>,
    source_prefix_offset: usize,
    body: UploadBody,
    source_finished: bool,
    encoder_finished: bool,
    started: bool,
    next_status_at: u64,
    /// A copy of the sender's own upload kept so the uploader's own views (such
    /// as the web log) can serve the file. Written from the same chunks sent to
    /// the server, never round-tripped through it. Present in persistent and
    /// memory download modes, absent when receiving is off.
    local_copy: Option<UploadLocalCopy>,
    /// Intrinsic image size, parsed from the first chunk as it streams.
    dimensions: Option<(u32, u32)>,
    image_prefix: Vec<u8>,
    /// End-to-end sealing state for DM uploads, `None` outside DM rooms.
    seal: Option<UploadSeal>,
}

/// Sealing state for a DM upload: every wire chunk is an AEAD frame under a
/// fresh per-transfer content key, and the finished stream is zero-padded to
/// its Padmé length so the server learns only a coarse size class.
struct UploadSeal {
    content_key: KeyMaterial,
    event_id: EventId,
    /// The server has durably assigned the announcement a delivery sequence.
    /// File relay must not start before this, or its offer can overtake the
    /// MLS event carrying the authenticated key and metadata.
    announcement_delivered: bool,
    digest: [u8; 32],
    /// The encoding of the sealed payloads, hidden from the server behind
    /// [`FileContentEncoding::Sealed`].
    inner_encoding: FileContentEncoding,
    /// Chunk counter, the AEAD nonce for the next frame.
    counter: u64,
    /// Compressed-stream bytes sealed so far, the Padmé input.
    stream_len: u64,
    /// Zero bytes still owed to reach the Padmé total, computed once the
    /// encoder finishes.
    pad_remaining: Option<u64>,
    /// The MLS application event id containing authenticated file metadata.
    mls_event_id: EventId,
}

#[derive(Default)]
struct PendingWire {
    bytes: Vec<u8>,
    offset: usize,
}

impl PendingWire {
    fn len(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn take(&mut self, limit: usize) -> Vec<u8> {
        let end = self.offset.saturating_add(limit).min(self.bytes.len());
        let data = self.bytes[self.offset..end].to_vec();
        self.offset = end;
        if self.offset == self.bytes.len() {
            self.bytes.clear();
            self.offset = 0;
        }
        data
    }

    fn compact(&mut self) {
        if self.offset == 0 {
            return;
        }
        self.bytes.copy_within(self.offset.., 0);
        self.bytes.truncate(self.bytes.len() - self.offset);
        self.offset = 0;
    }
}

impl Write for PendingWire {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.offset > 0
            && (self.offset == self.bytes.len() || self.offset >= MAX_FILE_CHUNK_BYTES)
        {
            self.compact();
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum UploadBody {
    Identity(PendingWire),
    Zstd(zstd::stream::write::Encoder<'static, PendingWire>),
}

impl UploadBody {
    fn encoding(&self) -> FileContentEncoding {
        match self {
            Self::Identity(_) => FileContentEncoding::Identity,
            Self::Zstd(_) => FileContentEncoding::Zstd,
        }
    }

    fn pending(&self) -> &PendingWire {
        match self {
            Self::Identity(pending) => pending,
            Self::Zstd(encoder) => encoder.get_ref(),
        }
    }

    fn pending_mut(&mut self) -> &mut PendingWire {
        match self {
            Self::Identity(pending) => pending,
            Self::Zstd(encoder) => encoder.get_mut(),
        }
    }

    fn feed(&mut self, raw: &[u8]) -> io::Result<()> {
        match self {
            Self::Identity(pending) => pending.write_all(raw),
            Self::Zstd(encoder) => encoder.write_all(raw),
        }
    }

    fn finish(&mut self) -> io::Result<()> {
        match self {
            Self::Identity(_) => Ok(()),
            Self::Zstd(encoder) => encoder.do_finish(),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Identity(pending) => pending.flush(),
            Self::Zstd(encoder) => encoder.flush(),
        }
    }
}

/// The sender's own copy of an in-flight upload, mirroring the room's download
/// mode: written to disk as it streams, or buffered into the in-memory ring.
enum UploadLocalCopy {
    Disk {
        path: PathBuf,
        file: File,
        reservation: DiskReservation,
    },
    Memory {
        reservation: Reservation,
        bytes: Vec<u8>,
    },
}

/// Where a completed local upload copy lives, so it can be surfaced as a
/// received file once the server assigns its metadata.
enum LocalFileLocation {
    Disk(String),
    /// Already inserted into the ring under this served name.
    Memory(String),
}

struct PendingLocalFile {
    location: LocalFileLocation,
    dimensions: Option<(u32, u32)>,
    /// `(name, size, inner encoding)` of a sealed upload, restored over the
    /// wire placeholder when the acceptance arrives after completion.
    sealed_real: Option<(String, u64, FileContentEncoding)>,
}

/// An in-flight bug report streamed to the server as a chunked control
/// transfer, mirroring [`OutgoingUpload`] but sourced from an in-memory buffer.
struct OutgoingBugReport {
    report_id: BugReportId,
    description: String,
    metadata: String,
    logs: Vec<u8>,
    offset: u64,
    started: bool,
}

struct IncomingFile {
    metadata: FileMetadata,
    dest: ReceiveDest,
    body: IncomingBody,
    pending_wire: Vec<u8>,
    pending_wire_offset: usize,
    wire_received: u64,
    complete_received: bool,
    decoder_finished: bool,
    next_status_at: u64,
    /// Chunk-opening state for sealed DM transfers; `None` for plain ones.
    /// When set, `metadata` already carries the true (inner) name, size, and
    /// encoding restored from the MLS file announcement.
    seal: Option<IncomingSeal>,
    /// The memory-ring claim for a [`DownloadTarget::Memory`] transfer, consumed
    /// by [`DownloadStore::insert_reserved`] on finalize and released if the
    /// transfer is dropped, failed, or skipped. `None` for on-disk transfers.
    reservation: Option<Reservation>,
}

/// Chunk-opening state for a sealed DM download.
struct IncomingSeal {
    content_key: KeyMaterial,
    event_id: EventId,
    transfer_id: FileTransferId,
    total_size: u64,
    digest: [u8; 32],
    /// Chunk counter, the AEAD nonce for the next expected frame.
    counter: u64,
    /// Wire-size cap computed from the sealed (outer) metadata; the restored
    /// inner metadata's own bound would not cover frame overhead and padding.
    wire_bound: u64,
}

/// Where an in-flight download is being written. Distinguishes the on-disk path
/// (which needs unlinking on failure/cancel) from an in-memory transfer (which
/// leaves nothing behind).
#[derive(Debug)]
enum ReceiveDest {
    Disk {
        path: PathBuf,
        reservation: DiskReservation,
    },
    Memory,
}

/// The final resting place of a completed download, produced by
/// [`IncomingFile::finalize`].
#[derive(Debug)]
enum FinalizedLocation {
    Disk { path: PathBuf, served_name: String },
    Memory(Vec<u8>),
}

struct ReceiveFile {
    path: PathBuf,
    file: File,
    reservation: DiskReservation,
}

/// The byte sink a [`ReceiveSink`] writes decoded output into: either the
/// on-disk file or an in-memory buffer for [`DownloadMode::Memory`].
///
/// [`DownloadMode::Memory`]: crate::config::DownloadMode::Memory
enum SinkTarget {
    Disk(File),
    Memory(Vec<u8>),
}

impl Write for SinkTarget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            SinkTarget::Disk(file) => file.write(buf),
            SinkTarget::Memory(bytes) => {
                bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            SinkTarget::Disk(file) => file.flush(),
            SinkTarget::Memory(_) => Ok(()),
        }
    }
}

struct ReceiveSink {
    target: SinkTarget,
    expected: u64,
    decoded: u64,
    work_budget: usize,
    capture_image_prefix: bool,
    image_prefix: Vec<u8>,
    digest: aws_lc_rs::digest::Context,
}

impl ReceiveSink {
    fn new(target: SinkTarget, expected: u64, capture_image_prefix: bool) -> Self {
        Self {
            target,
            expected,
            decoded: 0,
            work_budget: 0,
            capture_image_prefix,
            image_prefix: Vec::new(),
            digest: aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256),
        }
    }

    fn set_work_budget(&mut self, budget: usize) {
        self.work_budget = budget;
    }
}

impl Write for ReceiveSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.decoded.saturating_add(buf.len() as u64) > self.expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decoded file exceeds declared size",
            ));
        }
        if self.work_budget == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file decode work budget exhausted",
            ));
        }
        let write_len = buf.len().min(self.work_budget);
        let written = self.target.write(&buf[..write_len])?;
        self.digest.update(&buf[..written]);
        if self.capture_image_prefix && self.image_prefix.len() < MAX_FILE_CHUNK_BYTES {
            let capture = written.min(MAX_FILE_CHUNK_BYTES - self.image_prefix.len());
            self.image_prefix.extend_from_slice(&buf[..capture]);
        }
        self.decoded += written as u64;
        self.work_budget -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.target.flush()
    }
}

enum IncomingBody {
    Identity(ReceiveSink),
    Zstd(zstd::stream::zio::Writer<ReceiveSink, zstd::stream::raw::Decoder<'static>>),
}

impl IncomingBody {
    fn sink(&self) -> &ReceiveSink {
        match self {
            Self::Identity(sink) => sink,
            Self::Zstd(decoder) => decoder.writer(),
        }
    }

    fn sink_mut(&mut self) -> &mut ReceiveSink {
        match self {
            Self::Identity(sink) => sink,
            Self::Zstd(decoder) => decoder.writer_mut(),
        }
    }
}

impl IncomingFile {
    fn pump(&mut self, wire_budget: &mut usize, decoded_budget: &mut usize) -> io::Result<()> {
        self.body.sink_mut().set_work_budget(*decoded_budget);

        loop {
            if self.pending_wire_offset < self.pending_wire.len() && *wire_budget > 0 {
                let input_end = self
                    .pending_wire_offset
                    .saturating_add(*wire_budget)
                    .min(self.pending_wire.len());
                let input = &self.pending_wire[self.pending_wire_offset..input_end];
                let consumed = match &mut self.body {
                    IncomingBody::Identity(sink) => match sink.write(input) {
                        Ok(consumed) => consumed,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    },
                    IncomingBody::Zstd(decoder) => match decoder.write(input) {
                        Ok(consumed) => consumed,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) => return Err(error),
                    },
                };
                if consumed == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "file decoder accepted no input",
                    ));
                }
                self.pending_wire_offset += consumed;
                *wire_budget -= consumed;
                if self.body.sink().work_budget == 0 {
                    break;
                }
                continue;
            }

            if self.pending_wire_offset == self.pending_wire.len() && self.pending_wire_offset != 0
            {
                self.pending_wire.clear();
                self.pending_wire_offset = 0;
            }
            if !self.pending_wire.is_empty() {
                break;
            }
            if !self.complete_received || self.decoder_finished {
                break;
            }
            match &mut self.body {
                IncomingBody::Identity(_) => self.decoder_finished = true,
                IncomingBody::Zstd(decoder) => match decoder.finish() {
                    Ok(()) => self.decoder_finished = true,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error),
                },
            }
            break;
        }

        *decoded_budget = self.body.sink().work_budget;
        if self.decoder_finished && self.body.sink().decoded != self.metadata.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "decoded size {} does not match declared size {}",
                    self.body.sink().decoded,
                    self.metadata.size
                ),
            ));
        }
        Ok(())
    }

    fn ready_to_finalize(&self) -> bool {
        self.complete_received
            && self.decoder_finished
            && self.pending_wire_offset == self.pending_wire.len()
    }

    fn finalize(
        self,
    ) -> Result<
        (
            FileMetadata,
            FinalizedLocation,
            Option<Reservation>,
            Option<(u32, u32)>,
            u64,
        ),
        (ReceiveDest, String, io::Error),
    > {
        let Self {
            metadata,
            dest,
            body,
            wire_received,
            reservation,
            seal,
            ..
        } = self;
        let mut sink = match body {
            IncomingBody::Identity(sink) => sink,
            IncomingBody::Zstd(decoder) => decoder.into_inner().0,
        };
        if let Err(error) = sink.flush() {
            return Err((dest, metadata.file_name, error));
        }
        let actual_digest: [u8; 32] = sink
            .digest
            .finish()
            .as_ref()
            .try_into()
            .expect("SHA-256 digest has 32 bytes");
        if let Some(expected) = seal.map(|seal| seal.digest)
            && actual_digest != expected
        {
            return Err((
                dest,
                metadata.file_name,
                io::Error::new(io::ErrorKind::InvalidData, "file digest mismatch"),
            ));
        }
        let dimensions = sink
            .capture_image_prefix
            .then(|| crate::web_server::image_dimensions(&sink.image_prefix))
            .flatten();
        let location = match (dest, sink.target) {
            (ReceiveDest::Disk { path, reservation }, _) => {
                let served_name = reservation.commit(path.clone());
                FinalizedLocation::Disk { path, served_name }
            }
            (ReceiveDest::Memory, SinkTarget::Memory(bytes)) => FinalizedLocation::Memory(bytes),
            // The dest and sink target are constructed together, so a memory
            // dest always pairs with a memory sink.
            (ReceiveDest::Memory, SinkTarget::Disk(_)) => FinalizedLocation::Memory(Vec::new()),
        };
        Ok((metadata, location, reservation, dimensions, wire_received))
    }
}

impl WorkerState<'_> {
    fn queue_voice_command(&self, command: voice::VoiceCommand) -> Result<(), String> {
        self.voice
            .submit(command)
            .map_err(|_| "client voice command mailbox is full or stopped".to_string())
    }

    fn queue_mls_input(&mut self, input: mls_actor::Input) -> Result<(), String> {
        if self.pending_mls_input.is_some() {
            return Err("client MLS request backlog is full".to_string());
        }
        if let Err(input) = self.mls_runtime.try_send(input) {
            self.pending_mls_input = Some(input);
        }
        Ok(())
    }

    fn flush_pending_mls_input(&mut self) {
        let Some(input) = self.pending_mls_input.take() else {
            return;
        };
        if let Err(input) = self.mls_runtime.try_send(input) {
            self.pending_mls_input = Some(input);
        }
    }

    fn drain_mls_outputs(&mut self) -> Result<(), String> {
        self.flush_pending_mls_input();
        let actor_stopped = self.mls_runtime.drain_outputs(&mut self.mls_outputs);
        let mut outputs = std::mem::take(&mut self.mls_outputs);
        for output in outputs.drain(..) {
            match output {
                mls_actor::Output::Control {
                    generation,
                    control,
                } if generation == self.mls_generation => self.queue_control(control)?,
                mls_actor::Output::FileAnnouncement {
                    generation,
                    event_id,
                    cache,
                } if generation == self.mls_generation => {
                    let room_id = cache.room_id;
                    self.mls_file_announcements.insert(event_id, cache);
                    self.drain_pending_mls_file_offers(room_id)?;
                }
                mls_actor::Output::RestoreFile {
                    generation,
                    intent,
                    entry,
                } if generation == self.mls_generation => {
                    self.restore_mls_file_upload(intent, entry)?;
                }
                mls_actor::Output::MarkFileAnnouncementDelivered {
                    generation,
                    room_id,
                    event_id,
                } if generation == self.mls_generation => {
                    self.mark_mls_upload_announcement_delivered(room_id, event_id);
                }
                mls_actor::Output::ObservePeerAccount {
                    generation,
                    user_id,
                    account_id,
                } if generation == self.mls_generation => {
                    self.observe_peer_account(user_id, account_id);
                }
                mls_actor::Output::Availability {
                    generation,
                    available,
                } if generation == self.mls_generation => {
                    self.mls_available = available;
                }
                mls_actor::Output::ServerComplete { generation }
                    if generation == self.mls_generation =>
                {
                    debug_assert!(self.mls_server_pending != 0);
                    self.mls_server_pending = self.mls_server_pending.saturating_sub(1);
                }
                mls_actor::Output::SessionReady { generation }
                    if generation == self.mls_generation =>
                {
                    self.mls_session_ready = true;
                }
                mls_actor::Output::Fatal {
                    generation,
                    message,
                } if generation == self.mls_generation => {
                    let _ = self.events.send(NetworkEvent::Error(message.clone()));
                    return Err(format!("client MLS failure: {message}"));
                }
                _ => {}
            }
        }
        self.mls_outputs = outputs;
        if actor_stopped {
            return Err("client MLS worker stopped".to_string());
        }
        self.flush_pending_mls_input();
        Ok(())
    }

    fn restore_mls_file_upload(
        &mut self,
        intent: chatt_mls::DurableFileUpload,
        entry: chatt_mls::OutboxEntry,
    ) -> Result<(), String> {
        if self.outgoing_uploads.iter().any(|upload| {
            upload.room_id == intent.room_id
                && upload
                    .seal
                    .as_ref()
                    .is_some_and(|seal| seal.event_id == intent.event_id)
        }) {
            return Ok(());
        }
        let rpc::mls::ChattEventContent::File(announcement) = &entry.event.content else {
            return Err(
                "cannot resume encrypted upload: durable announcement is not a file".to_string(),
            );
        };
        let path = PathBuf::from(std::ffi::OsString::from_vec(intent.source_path.clone()));
        let request = UploadFileRequest {
            path,
            name_override: Some(announcement.name.clone()),
            delete_after_open: false,
        };
        let mut upload = self.prepare_file_upload(Some(intent.room_id), request)?;
        let Some(seal) = upload.seal.as_mut() else {
            return Err(format!(
                "cannot resume encrypted upload {}: room is no longer encrypted",
                announcement.name
            ));
        };
        if upload.size != announcement.size
            || seal.digest != announcement.digest
            || seal.inner_encoding != announcement.encoding
        {
            return Err(format!(
                "cannot resume encrypted upload {}: source file changed",
                announcement.name
            ));
        }
        upload.transfer_id = announcement.transfer_id;
        upload.delete_source_when_done = intent.delete_after_upload;
        seal.content_key.bytes = announcement.file_key;
        seal.event_id = intent.event_id;
        seal.mls_event_id = intent.event_id;
        seal.announcement_delivered =
            matches!(entry.state, chatt_mls::OutboxState::Delivered { .. });
        self.next_file_transfer = self
            .next_file_transfer
            .max(announcement.transfer_id.0.wrapping_add(1).max(1));
        let _ = self.events.send(NetworkEvent::Status(format!(
            "resuming upload {} ({})",
            upload.name,
            format_bytes(upload.size)
        )));
        let room_id = upload.room_id;
        let event_id = seal.event_id;
        self.outgoing_uploads.push_back(upload);
        self.queue_mls_input(mls_actor::Input::Command(
            mls_actor::Command::FileUploadReady { room_id, event_id },
        ))
    }

    #[inline]
    fn next_poll_timeout(&mut self, command_drain: CommandDrainOutcome, _now: Instant) -> Duration {
        self.queue_runnable_io();
        let mut schedule = PollSchedule::after(IDLE_POLL_TIMEOUT);
        schedule.include(self.loop_work.wake());
        schedule.include(self.command_wake(command_drain));
        schedule.include(self.bug_report_wake());
        schedule.include(self.receive_wake());
        schedule.include(self.upload_wake());
        schedule.timeout()
    }

    #[inline]
    fn queue_runnable_io(&mut self) {
        if self.tcp_readiness.is_ready() && self.read_buf.len() < MAX_BUFFERED_SERVER_BYTES {
            self.loop_work.queue_tcp_read();
        }
    }

    fn run_loop_tasks(&mut self) -> Result<(), String> {
        let tasks = self.loop_work.take_tasks();
        for task in tasks {
            match task {
                WorkerTask::TcpRead => {
                    if self.tcp_readiness.is_ready() {
                        self.read_tcp()?;
                    }
                }
                WorkerTask::TcpWrite => self.write_tcp()?,
            }
        }
        Ok(())
    }

    #[inline]
    fn command_wake(&self, command_drain: CommandDrainOutcome) -> WakeIntent {
        if command_drain == CommandDrainOutcome::HitLimit {
            WakeIntent::Now
        } else {
            WakeIntent::Idle
        }
    }

    #[inline]
    fn write_buffer_accepts_file_work(&self) -> bool {
        self.write_buf.len() <= MAX_QUEUED_FILE_BYTES
    }

    #[inline]
    fn bug_report_wake(&self) -> WakeIntent {
        if self.write_buffer_accepts_file_work() && !self.outgoing_bug_reports.is_empty() {
            WakeIntent::Now
        } else {
            WakeIntent::Idle
        }
    }

    #[inline]
    fn receive_wake(&self) -> WakeIntent {
        let ready = !matches!(frame::parse_frame(self.read_buf.pending()), Ok(None))
            || self.incoming_files.values().any(|incoming| {
                incoming.pending_wire_offset < incoming.pending_wire.len()
                    || incoming.complete_received
            });
        if ready {
            WakeIntent::Now
        } else {
            WakeIntent::Idle
        }
    }

    #[inline]
    fn upload_wake(&self) -> WakeIntent {
        if !self.write_buffer_accepts_file_work() {
            return WakeIntent::Idle;
        }
        let Some(front) = self.outgoing_uploads.front() else {
            return WakeIntent::Idle;
        };
        if !front.started
            && front
                .seal
                .as_ref()
                .is_some_and(|seal| !seal.announcement_delivered)
        {
            return WakeIntent::Idle;
        }
        let pending = front.body.pending().len();
        if upload_ready_now(front, pending, &self.upload_throttle) {
            WakeIntent::Now
        } else {
            WakeIntent::After(
                self.upload_throttle
                    .delay_until(pending.min(MAX_FILE_CHUNK_BYTES) as u64),
            )
        }
    }

    #[cfg(debug_assertions)]
    fn debug_assert_no_immediate_work(&self, command_drain: CommandDrainOutcome) {
        debug_assert!(!self.loop_work.has_immediate_work());
        debug_assert!(
            !(self.tcp_readiness.is_ready() && self.read_buf.len() < MAX_BUFFERED_SERVER_BYTES)
        );
        debug_assert_ne!(self.command_wake(command_drain), WakeIntent::Now);
        debug_assert_ne!(self.bug_report_wake(), WakeIntent::Now);
        debug_assert_ne!(self.receive_wake(), WakeIntent::Now);
        debug_assert_ne!(self.upload_wake(), WakeIntent::Now);
    }

    fn queue_control(&mut self, control: ClientControl) -> Result<(), String> {
        let payload = encode_client_control(&control)?;
        let encrypted = self
            .control
            .seal_next(CHANNEL_CONTROL, &payload)
            .map_err(|error| error.to_string())?;
        frame::encode_frame(&encrypted, self.write_buf.tail_mut()).map_err(|error| {
            format!(
                "{error} (control payload: {} bytes, sealed frame: {} bytes)",
                payload.len(),
                encrypted.len()
            )
        })?;
        kvlog::debug!(
            "client control queued",
            payload_size = payload.len(),
            encrypted_size = encrypted.len(),
            queued_bytes = self.write_buf.len()
        );
        self.write_tcp()
    }

    /// Reads from the TCP socket until `read_buf` reaches
    /// [`MAX_BUFFERED_SERVER_BYTES`] or the socket drains. Only a drain
    /// (`WouldBlock` or end-of-stream) clears `tcp_readiness`; stopping at the
    /// buffer cap leaves it set so the main loop retries after
    /// `process_server_controls` frees buffer space.
    fn read_tcp(&mut self) -> Result<(), String> {
        let outcome = rpc::evented::read_into_buffer(
            &self.tcp,
            &mut self.read_buf,
            &mut self.tcp_readiness,
            MAX_BUFFERED_SERVER_BYTES,
            ReadLimit::MaxBuffered(MAX_BUFFERED_SERVER_BYTES),
        )
        .map_err(|error| {
            kvlog::warn!("tcp read failed", error = %error);
            format!("TCP read failed: {error}")
        })?;
        if outcome.bytes_read > 0 {
            kvlog::debug!(
                "tcp bytes received",
                size = outcome.bytes_read,
                buffered = self.read_buf.len()
            );
        }
        if outcome.disconnected {
            kvlog::info!("tcp server closed connection");
            self.shutdown = true;
            self.disconnect_reason = Some("server closed connection".to_string());
        }
        Ok(())
    }

    fn process_server_controls(&mut self) -> Result<(), String> {
        if self.mls_server_pending != 0 || self.pending_mls_input.is_some() {
            return Ok(());
        }
        let mut wire_budget = MAX_FILE_WIRE_BYTES_PER_TICK;
        let mut decoded_budget = MAX_FILE_DECODED_BYTES_PER_TICK;
        let mut controls_since_file_pump = 0;
        let mut controls_processed = 0usize;
        loop {
            if controls_processed >= MAX_SERVER_CONTROLS_PER_ITERATION {
                break;
            }
            if controls_since_file_pump >= MAX_SERVER_CONTROLS_PER_FILE_PUMP {
                self.pump_incoming_files(&mut wire_budget, &mut decoded_budget);
                controls_since_file_pump = 0;
                if wire_budget == 0 || decoded_budget == 0 {
                    break;
                }
            }
            let deferred = self.deferred_server_control.take();
            let control = if let Some(control) = deferred {
                control
            } else {
                let total = match frame::parse_frame(self.read_buf.pending()) {
                    Ok(Some((_, total))) => {
                        kvlog::debug!(
                            "server frame received",
                            frame_size = total - frame::LENGTH_PREFIX_LEN
                        );
                        total
                    }
                    Ok(None) => {
                        self.pump_incoming_files(&mut wire_budget, &mut decoded_budget);
                        break;
                    }
                    Err(error) => return Err(format!("invalid server frame: {error}")),
                };
                let frame = &self.read_buf.pending()[frame::LENGTH_PREFIX_LEN..total];
                let plaintext = self
                    .control
                    .open_next(CHANNEL_CONTROL, frame)
                    .map_err(|error| error.to_string())?;
                self.read_buf.consume(total);
                kvlog::debug!("server control decrypted", payload_size = plaintext.len());
                if self.handle_history_chunk_payload(&plaintext)? {
                    controls_since_file_pump += 1;
                    controls_processed += 1;
                    continue;
                }
                decode_server_control(&plaintext)?
            };
            if self.mls_server_pending != 0 && !mls_actor::handles_server_control(&control) {
                self.deferred_server_control = Some(control);
                break;
            }
            self.handle_server_control(control)?;
            controls_since_file_pump += 1;
            controls_processed += 1;
            if self.pending_mls_input.is_some() {
                break;
            }
        }
        Ok(())
    }

    fn write_tcp(&mut self) -> Result<(), String> {
        let outcome = write_queue_to(&mut self.tcp, &mut self.write_buf, TCP_WRITE_ATTEMPTS)
            .map_err(|error| {
                kvlog::warn!("tcp write failed", error = %error);
                format!("TCP write failed: {error}")
            })?;
        if outcome.bytes_written > 0 {
            kvlog::debug!(
                "tcp bytes written",
                size = outcome.bytes_written,
                attempts = outcome.attempts,
                remaining = self.write_buf.len()
            );
        }
        if outcome.wrote_zero {
            return Err("TCP write returned zero bytes".to_string());
        }
        if outcome.hit_limit {
            self.loop_work.queue_tcp_write();
        }
        Ok(())
    }

    fn handle_command(&mut self, command: NetworkCommand) -> Result<(), String> {
        if !matches!(
            command,
            NetworkCommand::LocalVoicePacket(_)
                | NetworkCommand::SequencedLocalVoicePacket { .. }
                | NetworkCommand::SetPlaybackSink(_)
                | NetworkCommand::PlaybackFeedback(_)
        ) {
            kvlog::info!(
                "network command received",
                kind = network_command_kind(&command)
            );
        }
        match command {
            NetworkCommand::SendChat { room_id, body } => {
                kvlog::info!(
                    "send chat command handling",
                    room_id = room_id.0,
                    body_size = body.len()
                );
                if self.room_requires_mls(room_id) {
                    self.queue_mls_input(mls_actor::Input::Command(
                        mls_actor::Command::SendContent {
                            room_id,
                            content: rpc::mls::ChattEventContent::Text { body },
                        },
                    ))?;
                    return Ok(());
                }
                self.queue_control(ClientControl::SendChat { room_id, body })?;
            }
            NetworkCommand::EditChat {
                room_id,
                target,
                body,
            } => {
                kvlog::info!(
                    "edit chat command handling",
                    room_id = room_id.0,
                    target = target.0,
                    body_size = body.len()
                );
                if self.room_requires_mls(room_id) {
                    self.queue_mls_input(mls_actor::Input::Command(
                        mls_actor::Command::SendContent {
                            room_id,
                            content: rpc::mls::ChattEventContent::Edit { target, body },
                        },
                    ))?;
                    return Ok(());
                }
                self.queue_control(ClientControl::EditChat {
                    room_id,
                    target,
                    body,
                })?;
            }
            NetworkCommand::DeleteChat { room_id, target } => {
                kvlog::info!(
                    "delete chat command handling",
                    room_id = room_id.0,
                    target = target.0
                );
                if self.room_requires_mls(room_id) {
                    self.queue_mls_input(mls_actor::Input::Command(
                        mls_actor::Command::SendContent {
                            room_id,
                            content: rpc::mls::ChattEventContent::Delete { target },
                        },
                    ))?;
                    return Ok(());
                }
                self.queue_control(ClientControl::DeleteChat { room_id, target })?;
            }
            NetworkCommand::UploadFile { room_id, request } => {
                self.queue_file_upload(room_id, request);
            }
            NetworkCommand::CancelTransfer { transfer_id } => {
                self.cancel_transfer(transfer_id)?;
            }
            NetworkCommand::SetActiveRoom(room_id) => {
                self.active_room = Some(room_id);
            }
            NetworkCommand::JoinVoice(room_id) => {
                self.voice.activate(self.voice_generation)?;
                self.queue_control(ClientControl::JoinVoice { room_id })?;
            }
            NetworkCommand::LeaveVoice => {
                self.queue_control(ClientControl::LeaveVoice)?;
            }
            NetworkCommand::FetchHistory {
                room_id,
                before,
                limit,
            } => {
                if self.room_requires_mls(room_id) {
                    self.queue_mls_input(mls_actor::Input::Command(
                        mls_actor::Command::FetchHistory {
                            room_id,
                            before,
                            limit,
                        },
                    ))?;
                    return Ok(());
                }
                self.queue_control(ClientControl::FetchHistory {
                    room_id,
                    before,
                    limit,
                })?;
            }
            NetworkCommand::OpenDm(user_id) => {
                self.queue_control(ClientControl::OpenDm { user_id })?;
            }
            NetworkCommand::SetUploadRate(rate) => {
                self.upload_throttle.set_rate(rate);
                let label = if rate == 0 {
                    "unlimited".to_string()
                } else {
                    format!("{}/s", format_bytes(rate))
                };
                let _ = self
                    .events
                    .send(NetworkEvent::Status(format!("upload rate set to {label}")));
            }
            NetworkCommand::SetFilePolicy(policy) => {
                self.config.file_policy = policy;
            }
            NetworkCommand::ReviewPeerIdentity { user_id } => {
                if let Some(identity) = self.e2e.accepted_identity(user_id) {
                    let _ = self
                        .events
                        .send(NetworkEvent::E2ePeerPinMatched { identity });
                } else {
                    let _ = self.events.send(NetworkEvent::Status(
                        "the peer identity key is still being fetched or is unavailable"
                            .to_string(),
                    ));
                }
            }
            NetworkCommand::VerifyPeerIdentity { expected } => {
                let Some(pin) = self.e2e.proposed_verification(&expected) else {
                    let _ = self.events.send(NetworkEvent::Error(
                        "the accepted encryption identity changed while it was being verified"
                            .to_string(),
                    ));
                    return Ok(());
                };
                let _ = self.events.send(NetworkEvent::E2ePeerPinProposed {
                    pin,
                    manual_verification: true,
                });
            }
            NetworkCommand::ForgetPeerIdentity { expected } => {
                let Some(pin) = self.e2e.proposed_downgrade(&expected) else {
                    let _ = self.events.send(NetworkEvent::Error(
                        "the verified encryption identity changed before verification could be forgotten"
                            .to_string(),
                    ));
                    return Ok(());
                };
                let _ = self.events.send(NetworkEvent::E2ePeerPinProposed {
                    pin,
                    manual_verification: true,
                });
            }
            NetworkCommand::ConfirmE2ePeerPin {
                pin,
                persisted,
                manual_verification,
            } => {
                if !self.e2e.confirm_pin(&pin, persisted) {
                    kvlog::info!(
                        "stale e2e pin acknowledgement ignored",
                        user_id = pin.user_id,
                        room_id = pin.room_id
                    );
                    return Ok(());
                }
                if persisted {
                    if manual_verification {
                        self.e2e.record_verification_update(&pin)?;
                    }
                    if let Some(identity) = self.e2e.accepted_identity(UserId(pin.user_id)) {
                        let _ = self
                            .events
                            .send(NetworkEvent::E2ePeerPinMatched { identity });
                    }
                } else {
                    let _ = self.events.send(NetworkEvent::Error(
                        "encryption identity is active for this session but could not be saved"
                            .to_string(),
                    ));
                }
            }
            NetworkCommand::AcknowledgeMlsUiDispatch { room_id, sequence } => {
                self.queue_mls_input(mls_actor::Input::Command(
                    mls_actor::Command::AcknowledgeUiDispatch { room_id, sequence },
                ))?;
            }
            NetworkCommand::RevokeE2eDevice { device_id } => {
                self.queue_mls_input(mls_actor::Input::Command(mls_actor::Command::RevokeDevice(
                    device_id,
                )))?;
            }
            NetworkCommand::ListE2eDevices => {
                self.queue_mls_input(mls_actor::Input::Command(mls_actor::Command::ListDevices))?;
            }
            NetworkCommand::CreateDeviceLink => {
                self.queue_mls_input(mls_actor::Input::Command(
                    mls_actor::Command::CreateDeviceLink,
                ))?;
            }
            NetworkCommand::CancelDeviceLink {
                redemption_secret_hash,
            } => {
                self.queue_mls_input(mls_actor::Input::Command(
                    mls_actor::Command::CancelDeviceLink {
                        redemption_secret_hash,
                    },
                ))?;
            }
            #[cfg(test)]
            NetworkCommand::RetryConnection => {
                // The embedded server may have restarted quickly enough that
                // this lifecycle wake raced with a successful reconnect.
            }
            NetworkCommand::SetP2pEnabled(enabled) => {
                // P2P would bypass an outer secure link, so it stays off in
                // external-secure-link mode regardless of a runtime toggle.
                if enabled && self.transport_mode != TransportMode::NativeEncrypted {
                    let _ = self.events.send(NetworkEvent::Status(
                        "P2P unavailable in external-secure-link mode".to_string(),
                    ));
                    return Ok(());
                }
                self.queue_voice_command(voice::VoiceCommand::SetP2pEnabled {
                    generation: self.voice_generation,
                    enabled,
                })?;
            }
            NetworkCommand::LocalVoicePacket(_)
            | NetworkCommand::SequencedLocalVoicePacket { .. }
            | NetworkCommand::SetPlaybackSink(_)
            | NetworkCommand::PlaybackFeedback(_) => {
                // Production senders route these directly. The test fallback
                // intentionally drops them rather than doing packet work here.
                kvlog::debug!("media fast-path command reached chatt-net fallback");
            }
            NetworkCommand::SetVoiceStatus(status) => {
                if audio_pop_logging_enabled() {
                    kvlog::info!(
                        "audio pop control voice status tx",
                        muted = status.muted,
                        deafened = status.deafened
                    );
                }
                self.queue_control(ClientControl::SetVoiceStatus { status })?;
            }
            NetworkCommand::StartShare {
                codec,
                coded_width,
                coded_height,
                annexb,
                extradata,
            } => {
                let Some(room_id) = self.voice_room else {
                    let _ = self.events.send(NetworkEvent::ShareStartRejected {
                        message: "join a voice call before sharing".to_string(),
                    });
                    return Ok(());
                };
                self.queue_control(ClientControl::StartShare {
                    room_id,
                    codec,
                    coded_width,
                    coded_height,
                    annexb,
                    extradata,
                })?;
                self.pending_share_start = true;
            }
            NetworkCommand::StopShare { stream_id } => {
                self.queue_control(ClientControl::StopShare { stream_id })?;
            }
            NetworkCommand::ReportBug {
                description,
                metadata,
                compressed_logs,
            } => {
                let report_id = BugReportId(self.next_bug_report);
                self.next_bug_report = self.next_bug_report.wrapping_add(1).max(1);
                self.outgoing_bug_reports.push_back(OutgoingBugReport {
                    report_id,
                    description,
                    metadata,
                    logs: compressed_logs,
                    offset: 0,
                    started: false,
                });
            }
            NetworkCommand::Shutdown => {
                kvlog::info!("shutdown command handling");
                self.shutdown = true;
                self.shutdown_requested = true;
            }
        }
        Ok(())
    }

    fn queue_file_upload(&mut self, room_id: Option<RoomId>, request: UploadFileRequest) {
        match self.prepare_file_upload(room_id, request) {
            Ok(upload) => {
                let name = upload.name.clone();
                let size = upload.size;
                if let Err(error) = self.persist_and_submit_file_announcement(&upload) {
                    let _ = self.events.send(NetworkEvent::Error(error));
                    return;
                }
                self.outgoing_uploads.push_back(upload);
                let _ = self.events.send(NetworkEvent::Status(format!(
                    "queued upload {name} ({})",
                    format_bytes(size)
                )));
            }
            Err(error) => {
                let _ = self.events.send(NetworkEvent::Error(error));
            }
        }
    }

    fn prepare_file_upload(
        &mut self,
        room_id: Option<RoomId>,
        request: UploadFileRequest,
    ) -> Result<OutgoingUpload, String> {
        let UploadFileRequest {
            path,
            name_override,
            delete_after_open,
        } = request;
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("upload path is not a file: {}", path.display()));
        }
        let limit = self.config.max_upload_bytes;
        let size = metadata.len();
        if size > limit {
            return Err(format!(
                "file is {}; limit is {}",
                format_bytes(size),
                format_bytes(limit)
            ));
        }
        let name = upload_username(name_override, &path)?;
        let digest = file_sha256(&path)?;
        let mut file = open_upload_source(&path, false)
            .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
        let mut source_prefix = Vec::new();
        let mut _probe_encoded_len = None;
        let (body, _decision) = match file_compression::fast_compression_decision(&name, size) {
            FastCompressionDecision::BelowMinimum => (
                UploadBody::Identity(PendingWire::default()),
                "below_minimum",
            ),
            FastCompressionDecision::ExcludedExtension => (
                UploadBody::Identity(PendingWire::default()),
                "excluded_extension",
            ),
            FastCompressionDecision::Probe => {
                let probe_len = usize::try_from(size.min(COMPRESSION_PROBE_BYTES as u64))
                    .expect("compression probe length fits usize");
                source_prefix.resize(probe_len, 0);
                file.read_exact(&mut source_prefix).map_err(|error| {
                    format!(
                        "failed to read compression probe for {}: {error}",
                        path.display()
                    )
                })?;
                match file_compression::compressed_probe_len(&source_prefix) {
                    Ok(encoded_len) => {
                        _probe_encoded_len = Some(encoded_len);
                        if file_compression::probe_has_minimum_savings(probe_len, encoded_len) {
                            match file_compression::new_encoder(PendingWire::default(), size) {
                                Ok(encoder) => (UploadBody::Zstd(encoder), "probe_accepted"),
                                Err(error) => {
                                    kvlog::warn!(
                                        "file compression encoder setup failed",
                                        file_name = name.as_str(),
                                        error = %error
                                    );
                                    (UploadBody::Identity(PendingWire::default()), "probe_error")
                                }
                            }
                        } else {
                            (
                                UploadBody::Identity(PendingWire::default()),
                                "probe_rejected",
                            )
                        }
                    }
                    Err(error) => {
                        kvlog::warn!(
                            "file compression probe failed",
                            file_name = name.as_str(),
                            error = %error
                        );
                        (UploadBody::Identity(PendingWire::default()), "probe_error")
                    }
                }
            }
        };
        kvlog::debug!(
            "file compression decision",
            file_name = name.as_str(),
            original_size = size,
            decision = _decision,
            probe_raw_bytes = source_prefix.len(),
            probe_encoded_bytes = _probe_encoded_len.unwrap_or(0)
        );
        let transfer_id = FileTransferId(self.next_file_transfer);
        self.next_file_transfer = self.next_file_transfer.wrapping_add(1).max(1);
        let room_id = room_id
            .or(self.active_room)
            .ok_or_else(|| "no active room for upload".to_string())?;
        let seal = self.prepare_upload_seal(room_id, body.encoding(), digest)?;
        Ok(OutgoingUpload {
            transfer_id,
            server_metadata: None,
            room_id,
            name,
            size,
            file,
            source_path: path.clone(),
            delete_source_when_done: delete_after_open && seal.is_some(),
            source_offset: 0,
            source_read_ahead: 0,
            wire_offset: 0,
            source_prefix,
            source_prefix_offset: 0,
            body,
            source_finished: size == 0,
            encoder_finished: false,
            started: false,
            next_status_at: FILE_PROGRESS_STEP_BYTES.min(size),
            local_copy: None,
            dimensions: None,
            image_prefix: Vec::new(),
            seal,
        })
        .inspect(|upload| {
            if delete_after_open && upload.seal.is_none() {
                let _ = fs::remove_file(&path);
            }
        })
    }

    /// Builds MLS file sealing state. Persistence and submission happen only
    /// after the complete upload has been installed in the in-memory queue.
    fn prepare_upload_seal(
        &mut self,
        room_id: RoomId,
        inner_encoding: FileContentEncoding,
        digest: [u8; 32],
    ) -> Result<Option<UploadSeal>, String> {
        if !self.room_requires_mls(room_id) {
            return Ok(None);
        }
        if !self.mls_available {
            return Err(
                "cannot send file: this installation has no encrypted-room identity".to_string(),
            );
        }
        let mut content_key = [0u8; KEY_LEN];
        aws_lc_rs::rand::SystemRandom::new()
            .fill(&mut content_key)
            .map_err(|_| "failed to generate a file content key".to_string())?;
        let event_id = random_event_id()?;
        Ok(Some(UploadSeal {
            content_key: KeyMaterial {
                id: 1,
                bytes: content_key,
            },
            event_id,
            announcement_delivered: false,
            digest,
            inner_encoding,
            counter: 0,
            stream_len: 0,
            pad_remaining: None,
            mls_event_id: event_id,
        }))
    }

    fn persist_and_submit_file_announcement(
        &mut self,
        upload: &OutgoingUpload,
    ) -> Result<(), String> {
        let Some(seal) = upload.seal.as_ref() else {
            return Ok(());
        };
        let announcement = rpc::mls::MlsFileAnnouncement {
            transfer_id: upload.transfer_id,
            name: upload.name.clone(),
            size: upload.size,
            chunk_size: MAX_FILE_CHUNK_BYTES as u32,
            encoding: seal.inner_encoding,
            file_key: seal.content_key.bytes,
            digest: seal.digest,
        };
        self.queue_mls_input(mls_actor::Input::Command(mls_actor::Command::QueueFile {
            room_id: upload.room_id,
            event_id: seal.event_id,
            timestamp_ms: unix_now_ms(),
            announcement,
            source_path: upload.source_path.as_os_str().as_bytes().to_vec(),
            delete_after_upload: upload.delete_source_when_done,
        }))
    }

    fn finish_durable_file_upload(&mut self, upload: &OutgoingUpload) -> Result<(), String> {
        let Some(seal) = upload.seal.as_ref() else {
            return Ok(());
        };
        self.queue_mls_input(mls_actor::Input::Command(mls_actor::Command::FinishFile {
            room_id: upload.room_id,
            event_id: seal.event_id,
            source_path: upload.source_path.clone(),
            delete_source: upload.delete_source_when_done,
        }))
    }

    fn poll_uploads(&mut self) -> Result<(), String> {
        let mut source_budget = MAX_FILE_SOURCE_BYTES_PER_TICK;
        for _ in 0..MAX_FILE_CHUNKS_PER_TICK {
            if self.write_buf.len() > MAX_QUEUED_FILE_BYTES {
                break;
            }
            if !self.poll_one_upload(&mut source_budget)? {
                break;
            }
        }
        Ok(())
    }

    fn poll_bug_reports(&mut self) -> Result<(), String> {
        for _ in 0..MAX_FILE_CHUNKS_PER_TICK {
            if self.write_buf.len() > MAX_QUEUED_FILE_BYTES {
                break;
            }
            if !self.poll_one_bug_report()? {
                break;
            }
        }
        Ok(())
    }

    fn poll_one_bug_report(&mut self) -> Result<bool, String> {
        let Some(mut report) = self.outgoing_bug_reports.pop_front() else {
            return Ok(false);
        };

        if !report.started {
            self.queue_control(ClientControl::BugReportStart {
                report_id: report.report_id,
                description: report.description.clone(),
                metadata: report.metadata.clone(),
                logs_size: report.logs.len() as u64,
            })?;
            report.started = true;
            let _ = self.events.send(NetworkEvent::Status(format!(
                "filing bug report ({})",
                format_bytes(report.logs.len() as u64)
            )));
            self.outgoing_bug_reports.push_front(report);
            return Ok(true);
        }

        if (report.offset as usize) < report.logs.len() {
            let start = report.offset as usize;
            let end = (start + MAX_FILE_CHUNK_BYTES).min(report.logs.len());
            let data = report.logs[start..end].to_vec();
            self.queue_control(ClientControl::BugReportChunk {
                report_id: report.report_id,
                offset: report.offset,
                data,
            })?;
            report.offset = end as u64;
            self.outgoing_bug_reports.push_front(report);
            return Ok(true);
        }

        self.queue_control(ClientControl::BugReportComplete {
            report_id: report.report_id,
        })?;
        let _ = self
            .events
            .send(NetworkEvent::Status("bug report sent".to_string()));
        Ok(true)
    }

    fn poll_one_upload(&mut self, source_budget: &mut usize) -> Result<bool, String> {
        let Some(mut upload) = self.outgoing_uploads.pop_front() else {
            return Ok(false);
        };

        if !upload.started {
            if upload
                .seal
                .as_ref()
                .is_some_and(|seal| !seal.announcement_delivered)
            {
                self.outgoing_uploads.push_front(upload);
                return Ok(false);
            }
            // Sealed uploads advertise a placeholder name and the Padmé size
            // class; the real metadata rides only inside the MLS event.
            let (name, size, encoding, mls_event_id) = match &upload.seal {
                Some(seal) => (
                    "sealed.bin".to_string(),
                    rpc::mls::padme_len(upload.size),
                    FileContentEncoding::Sealed,
                    Some(seal.mls_event_id),
                ),
                None => (
                    upload.name.clone(),
                    upload.size,
                    upload.body.encoding(),
                    None,
                ),
            };
            self.queue_control(ClientControl::UploadFileStart {
                room_id: upload.room_id,
                transfer_id: upload.transfer_id,
                name,
                size,
                encoding,
                mls_event_id,
            })?;
            upload.started = true;
            // Keep a local copy of the sender's own upload so the uploader's own
            // views can serve it, mirroring the room's download mode. The server
            // excludes the sender from the file fanout, so this local copy is the
            // only way the uploader's web log renders and serves its own upload.
            // Off mode keeps none: the sender already holds the original.
            match self
                .config
                .file_policy
                .for_room(upload.room_id)
                .target
                .clone()
            {
                DownloadTarget::Persistent(receive_dir) => {
                    match create_receive_file(
                        &self.config.download_store,
                        &receive_dir,
                        &upload.name,
                    ) {
                        Ok(receive) => {
                            upload.local_copy = Some(UploadLocalCopy::Disk {
                                path: receive.path,
                                file: receive.file,
                                reservation: receive.reservation,
                            })
                        }
                        Err(error) => {
                            let _ = self.events.send(NetworkEvent::Error(format!(
                                "failed to keep a local copy of {}: {error}",
                                upload.name
                            )));
                        }
                    }
                }
                DownloadTarget::Memory => {
                    // Buffer into the ring like a received file. If it does not
                    // fit, the upload still proceeds; only the local copy is
                    // skipped.
                    if let Some(reservation) = self.config.download_store.reserve(upload.size) {
                        upload.local_copy = Some(UploadLocalCopy::Memory {
                            reservation,
                            bytes: Vec::with_capacity(upload.size as usize),
                        });
                    }
                }
                DownloadTarget::Off => {}
            }
            let _ = self.events.send(NetworkEvent::Status(format!(
                "uploading {} ({})",
                upload.name,
                format_bytes(upload.size)
            )));
            self.outgoing_uploads.push_front(upload);
            return Ok(true);
        }

        let throttle_budget = self.upload_throttle.budget();
        if !upload.body.pending().is_empty() && throttle_budget > 0 {
            let max_payload = match &upload.seal {
                Some(_) => MAX_FILE_CHUNK_BYTES - rpc::mls::FILE_CHUNK_OVERHEAD,
                None => MAX_FILE_CHUNK_BYTES,
            };
            let send_len = upload
                .body
                .pending()
                .len()
                .min(max_payload)
                .min(throttle_budget as usize);
            let data = upload.body.pending_mut().take(send_len);
            let data = match &mut upload.seal {
                None => data,
                Some(seal) => {
                    let Some(sender) = self.user_id else {
                        return self.cancel_outgoing_upload(
                            upload,
                            "not authenticated",
                            UploadAbort::Failure("cannot seal upload before login".to_string()),
                        );
                    };
                    let frame = rpc::mls::seal_file_chunk(
                        &seal.content_key.bytes,
                        upload.room_id,
                        sender,
                        seal.event_id,
                        upload.transfer_id,
                        upload.size,
                        &seal.digest,
                        seal.counter,
                        &data,
                        0,
                    );
                    match frame {
                        Ok(frame) => {
                            seal.counter += 1;
                            seal.stream_len += data.len() as u64;
                            frame
                        }
                        Err(error) => {
                            return self.cancel_outgoing_upload(
                                upload,
                                "sealing failed",
                                UploadAbort::Failure(format!(
                                    "failed to seal upload chunk: {error}"
                                )),
                            );
                        }
                    }
                }
            };
            let offset = upload.wire_offset;
            let wire_len = data.len() as u64;
            self.queue_control(ClientControl::UploadFileChunk {
                transfer_id: upload.transfer_id,
                offset,
                data,
            })?;
            self.upload_throttle.consume(send_len as u64);
            upload.wire_offset += wire_len;
            if upload.body.pending().is_empty() {
                upload.source_read_ahead = 0;
            }
            self.outgoing_uploads.push_front(upload);
            return Ok(true);
        }

        if upload_should_flush_source_read_ahead(&upload, &self.upload_throttle) {
            if let Err(error) = upload.body.flush() {
                return self.cancel_outgoing_upload(
                    upload,
                    "compression failed",
                    UploadAbort::Failure(format!("failed to flush compressed upload: {error}")),
                );
            }
            if upload.body.pending().is_empty() {
                upload.source_read_ahead = 0;
            }
            self.outgoing_uploads.push_front(upload);
            return Ok(true);
        }

        let source_read_capacity = upload_source_read_capacity(&upload, &self.upload_throttle);
        if !upload.source_finished
            && *source_budget > 0
            && upload.body.pending().len() < MAX_QUEUED_FILE_BYTES
            && source_read_capacity > 0
        {
            let read_limit = (*source_budget)
                .min(MAX_FILE_CHUNK_BYTES)
                .min((upload.size - upload.source_offset) as usize)
                .min(source_read_capacity.min(usize::MAX as u64) as usize);
            let data = match read_upload_source(&mut upload, read_limit) {
                Ok(data) if !data.is_empty() => data,
                Ok(_) => {
                    return self.cancel_outgoing_upload(
                        upload,
                        "local file ended early",
                        UploadAbort::Failure("file ended early while uploading".to_string()),
                    );
                }
                Err(error) => {
                    return self.cancel_outgoing_upload(
                        upload,
                        "failed to read local file",
                        UploadAbort::Failure(format!(
                            "failed to read file while uploading: {error}"
                        )),
                    );
                }
            };
            write_upload_local_copy(&self.events, &mut upload, &data);
            capture_upload_image_prefix(&mut upload, &data);
            if let Err(error) = upload.body.feed(&data) {
                return self.cancel_outgoing_upload(
                    upload,
                    "compression failed",
                    UploadAbort::Failure(format!("failed to compress upload: {error}")),
                );
            }
            if compressed_upload_source_read_ahead_is_limited(&upload, &self.upload_throttle) {
                upload.source_read_ahead =
                    upload.source_read_ahead.saturating_add(data.len() as u64);
            } else {
                upload.source_read_ahead = 0;
            }
            upload.source_offset += data.len() as u64;
            *source_budget -= data.len();
            upload.source_finished = upload.source_offset == upload.size;
            if upload.source_offset >= upload.next_status_at || upload.source_offset == upload.size
            {
                upload.next_status_at = upload
                    .next_status_at
                    .saturating_add(FILE_PROGRESS_STEP_BYTES);
                // The overlay keys on the server transfer id, learned only once the
                // upload is accepted. Ticks before then are dropped; acceptance
                // emits the first tick itself.
                if let Some(meta) = upload.server_metadata.as_ref() {
                    let _ = self.events.send(NetworkEvent::TransferProgress {
                        room_id: meta.room_id,
                        transfer_id: meta.transfer_id,
                        timestamp_ms: meta.timestamp_ms,
                        transferred: upload.source_offset,
                        total: upload.size,
                        direction: TransferDirection::Outgoing,
                    });
                }
            }
            self.outgoing_uploads.push_front(upload);
            return Ok(true);
        }

        if upload.source_finished && !upload.encoder_finished {
            if let Err(error) = upload.body.finish() {
                return self.cancel_outgoing_upload(
                    upload,
                    "compression failed",
                    UploadAbort::Failure(format!("failed to finish compressed upload: {error}")),
                );
            }
            upload.encoder_finished = true;
            self.outgoing_uploads.push_front(upload);
            return Ok(true);
        }

        if !upload.body.pending().is_empty() {
            self.outgoing_uploads.push_front(upload);
            return Ok(false);
        }

        if !upload.encoder_finished {
            self.outgoing_uploads.push_front(upload);
            return Ok(false);
        }

        if let Some(seal) = &mut upload.seal {
            let stream_len = seal.stream_len;
            let pad_remaining = *seal
                .pad_remaining
                .get_or_insert_with(|| rpc::mls::padme_len(stream_len) - stream_len);
            if pad_remaining > 0 {
                let Some(sender) = self.user_id else {
                    return self.cancel_outgoing_upload(
                        upload,
                        "not authenticated",
                        UploadAbort::Failure("cannot seal upload before login".to_string()),
                    );
                };
                // Padding frames are all-zero filler and bypass the pacing
                // throttle: charging them would stall completion on a
                // throttled link with no wake scheduled for it, and they
                // overshoot the configured rate by at most the Padmé overhead.
                let pad_len = pad_remaining
                    .min((MAX_FILE_CHUNK_BYTES - rpc::mls::FILE_CHUNK_OVERHEAD) as u64)
                    as usize;
                let frame = rpc::mls::seal_file_chunk(
                    &seal.content_key.bytes,
                    upload.room_id,
                    sender,
                    seal.event_id,
                    upload.transfer_id,
                    upload.size,
                    &seal.digest,
                    seal.counter,
                    &[],
                    pad_len,
                );
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        return self.cancel_outgoing_upload(
                            upload,
                            "sealing failed",
                            UploadAbort::Failure(format!("failed to seal padding: {error}")),
                        );
                    }
                };
                seal.counter += 1;
                seal.pad_remaining = Some(pad_remaining - pad_len as u64);
                let offset = upload.wire_offset;
                let wire_len = frame.len() as u64;
                self.queue_control(ClientControl::UploadFileChunk {
                    transfer_id: upload.transfer_id,
                    offset,
                    data: frame,
                })?;
                upload.wire_offset += wire_len;
                self.outgoing_uploads.push_front(upload);
                return Ok(true);
            }
        }

        self.queue_control(ClientControl::UploadFileComplete {
            transfer_id: upload.transfer_id,
        })?;
        kvlog::debug!(
            "file upload encoding completed",
            file_name = upload.name.as_str(),
            original_bytes = upload.size,
            wire_bytes = upload.wire_offset
        );
        let _ = self.events.send(NetworkEvent::Status(format!(
            "upload complete: {} ({})",
            upload.name,
            format_bytes(upload.size)
        )));
        // Terminal clear for the progress overlay. An uploader with a receive
        // directory also clears via the `FileReceived` in `finish_local_copy`
        // (a redundant but harmless second clear); one without a directory never
        // emits `FileReceived`, so this is its only clear path.
        if let Some(meta) = upload.server_metadata.as_ref() {
            let _ = self.events.send(NetworkEvent::TransferComplete {
                room_id: meta.room_id,
                transfer_id: meta.transfer_id,
            });
        }
        self.finish_local_copy(&mut upload);
        self.finish_durable_file_upload(&upload)?;
        Ok(true)
    }

    /// Aborts the transfer with server id `transfer_id`, resolving the direction
    /// from which map holds it: an outgoing upload is canceled, an incoming
    /// download is skipped. Unknown ids (already finished or canceled) are a
    /// no-op.
    fn cancel_transfer(&mut self, transfer_id: FileTransferId) -> Result<(), String> {
        if let Some(index) = self.outgoing_uploads.iter().position(|upload| {
            upload
                .server_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.transfer_id == transfer_id)
        }) {
            let upload = self
                .outgoing_uploads
                .remove(index)
                .expect("index in bounds");
            self.cancel_outgoing_upload(upload, "canceled by sender", UploadAbort::UserCancel)?;
        } else if self.incoming_files.contains_key(&transfer_id) {
            self.skip_incoming_file(transfer_id)?;
        }
        Ok(())
    }

    /// Declines an in-flight download: tells the server to stop relaying
    /// ([`ClientControl::SkipFile`]), drops the partial file, and clears the
    /// local view. Mirrors [`Self::handle_file_canceled`] for a locally
    /// initiated skip.
    fn skip_incoming_file(&mut self, transfer_id: FileTransferId) -> Result<(), String> {
        self.queue_control(ClientControl::SkipFile { transfer_id })?;
        if let Some(incoming) = self.incoming_files.remove(&transfer_id) {
            let room_id = incoming.metadata.room_id;
            cleanup_partial(&incoming.dest);
            let _ = self.events.send(NetworkEvent::TransferEnded {
                room_id,
                transfer_id,
                timestamp_ms: incoming.metadata.timestamp_ms,
                verb: TerminalVerb::Skipped,
                reason: None,
            });
        }
        Ok(())
    }

    /// Tells the server to stop relaying an offered file this client is declining
    /// at offer time (its per-room policy rejects it). Best-effort: a full control
    /// queue means the connection is already failing.
    fn skip_offered_download(&mut self, file: &FileMetadata) {
        if let Err(error) = self.queue_control(ClientControl::SkipFile {
            transfer_id: file.transfer_id,
        }) {
            kvlog::warn!(
                "failed to queue skip for offered file",
                error = error.as_str()
            );
        }
    }

    /// Emits the persistent `skipped: <reason>` terminal label for an offered file
    /// this client did not accept.
    fn end_transfer_skipped(&self, file: &FileMetadata, reason: String) {
        let _ = self.events.send(NetworkEvent::TransferEnded {
            room_id: file.room_id,
            transfer_id: file.transfer_id,
            timestamp_ms: file.timestamp_ms,
            verb: TerminalVerb::Skipped,
            reason: Some(reason),
        });
    }

    fn cancel_outgoing_upload(
        &mut self,
        mut upload: OutgoingUpload,
        wire_reason: &str,
        abort: UploadAbort,
    ) -> Result<bool, String> {
        self.queue_control(ClientControl::UploadFileCancel {
            transfer_id: upload.transfer_id,
            reason: wire_reason.to_string(),
        })?;
        match upload.local_copy.take() {
            // Drop the on-disk partial; the memory copy's reservation releases as
            // it drops, freeing its ring bytes.
            Some(UploadLocalCopy::Disk { path, .. }) => {
                let _ = fs::remove_file(path);
            }
            Some(UploadLocalCopy::Memory { .. }) | None => {}
        }
        let (verb, reason) = match &abort {
            UploadAbort::UserCancel => (TerminalVerb::Cancelled, None),
            UploadAbort::Declined => (
                TerminalVerb::Cancelled,
                Some("recipient declined".to_string()),
            ),
            UploadAbort::Failure(error) => (TerminalVerb::Failed, Some(error.clone())),
        };
        if let Some(metadata) = upload.server_metadata.as_ref() {
            let _ = self.events.send(NetworkEvent::TransferEnded {
                room_id: metadata.room_id,
                transfer_id: metadata.transfer_id,
                timestamp_ms: metadata.timestamp_ms,
                verb,
                reason,
            });
        }
        if let UploadAbort::Failure(error) = abort {
            let _ = self
                .events
                .send(NetworkEvent::Error(format!("{error} {}", upload.name)));
        }
        self.finish_durable_file_upload(&upload)?;
        Ok(true)
    }

    /// Flushes the uploader's local copy and emits [`NetworkEvent::FileReceived`]
    /// so local views render the file the same way they render a received one.
    fn finish_local_copy(&mut self, upload: &mut OutgoingUpload) {
        let Some(copy) = upload.local_copy.take() else {
            return;
        };
        let location = match copy {
            UploadLocalCopy::Disk {
                path,
                mut file,
                reservation,
            } => {
                if let Err(error) = file.flush() {
                    let _ = fs::remove_file(&path);
                    let _ = self.events.send(NetworkEvent::Error(format!(
                        "failed to flush local copy {}: {error}",
                        path.display()
                    )));
                    return;
                }
                LocalFileLocation::Disk(reservation.commit(path))
            }
            UploadLocalCopy::Memory { reservation, bytes } => {
                match self
                    .config
                    .download_store
                    .insert_reserved(reservation, &upload.name, bytes)
                {
                    Some(name) => LocalFileLocation::Memory(name),
                    None => {
                        let _ = self.events.send(NetworkEvent::Error(format!(
                            "could not keep {} in the in-memory download buffer",
                            upload.name
                        )));
                        return;
                    }
                }
            }
        };
        if let Some(metadata) = upload.server_metadata.take() {
            self.emit_local_file(metadata, location, upload.dimensions);
        } else {
            self.pending_local_files.insert(
                upload.transfer_id,
                PendingLocalFile {
                    location,
                    dimensions: upload.dimensions,
                    sealed_real: upload
                        .seal
                        .as_ref()
                        .map(|seal| (upload.name.clone(), upload.size, seal.inner_encoding)),
                },
            );
        }
    }

    fn handle_upload_accepted(
        &mut self,
        client_transfer_id: FileTransferId,
        metadata: FileMetadata,
    ) {
        if let Some((metadata, local)) = correlate_upload_accepted(
            &mut self.outgoing_uploads,
            &mut self.pending_local_files,
            client_transfer_id,
            metadata,
        ) {
            self.emit_local_file(metadata, local.location, local.dimensions);
        } else if let Some(upload) = self
            .outgoing_uploads
            .iter()
            .find(|upload| upload.transfer_id == client_transfer_id)
            && let Some(meta) = upload.server_metadata.as_ref()
        {
            // Show the bar immediately at acceptance, at whatever offset streaming
            // has already reached.
            let _ = self.events.send(NetworkEvent::TransferProgress {
                room_id: meta.room_id,
                transfer_id: meta.transfer_id,
                timestamp_ms: meta.timestamp_ms,
                transferred: upload.source_offset,
                total: upload.size,
                direction: TransferDirection::Outgoing,
            });
        }
    }

    fn emit_local_file(
        &self,
        metadata: FileMetadata,
        location: LocalFileLocation,
        dimensions: Option<(u32, u32)>,
    ) {
        let served_name = match location {
            LocalFileLocation::Disk(name) => name,
            LocalFileLocation::Memory(name) => name,
        };
        let _ = self.events.send(NetworkEvent::FileReceived {
            metadata,
            served_name,
            dimensions,
        });
    }

    fn handle_file_offered(&mut self, file: FileMetadata, contents: bool) -> Result<(), String> {
        if !self.mls_available && self.room_requires_mls(file.room_id) {
            if contents {
                self.skip_offered_download(&file);
            }
            let _ = self.events.send(NetworkEvent::Error(
                "Encrypted file unavailable while this installation's identity is disabled."
                    .to_string(),
            ));
            return Ok(());
        }
        if !self.file_offer_ready(&file)? {
            if contents {
                self.skip_offered_download(&file);
            }
            self.skipped_untrusted_files.insert(file.transfer_id);
            self.defer_mls_file_offer(PendingMlsFileOffer::FileOffered {
                file,
                contents: false,
                skipped_untrusted: true,
            })?;
            return Ok(());
        }
        self.handle_file_offered_ready(file, contents);
        Ok(())
    }

    fn file_offer_ready(&mut self, file: &FileMetadata) -> Result<bool, String> {
        let is_mls = self.room_requires_mls(file.room_id);
        if is_mls != (file.encoding == FileContentEncoding::Sealed) {
            return Err(format!(
                "server sent a file with forbidden encryption form for room {}",
                file.room_id.0
            ));
        }
        if is_mls {
            let event_id = file
                .mls_event_id
                .ok_or_else(|| "MLS transfer omitted its announcement event id".to_string())?;
            return Ok(self.mls_file_announcements.contains_key(&event_id));
        }
        Ok(true)
    }

    fn handle_file_offered_ready(&mut self, file: FileMetadata, contents: bool) {
        kvlog::info!(
            "file offered",
            transfer_id = file.transfer_id.0,
            file_name = file.file_name.as_str(),
            file_size = file.size,
            contents
        );
        let mut file = file;
        let seal = match self.unseal_offer(&mut file) {
            Ok(seal) => seal,
            Err(reason) => {
                self.skip_offered_download(&file);
                let _ = self.events.send(NetworkEvent::Error(format!(
                    "not receiving {}: {reason}",
                    file.file_name
                )));
                self.end_transfer_skipped(&file, reason);
                return;
            }
        };
        // Take owned copies of the per-room policy so the borrow of `self.config`
        // ends before the `&mut self` skip/label calls below.
        let (target, max_download_bytes) = {
            let policy = self.config.file_policy.for_room(file.room_id);
            (policy.target.clone(), policy.max_download_bytes)
        };
        let size_label = || {
            format!(
                "File exceeds maximum configured size ({})",
                format_bytes(max_download_bytes)
            )
        };
        if !contents {
            // The server already declined to stream (the file exceeds our
            // advertised limit or receiving is off), so no `SkipFile` is needed;
            // just label the line with why.
            let reason = if target.is_active() {
                size_label()
            } else {
                "Automatic file receive disabled".to_string()
            };
            let _ = self.events.send(NetworkEvent::Status(format!(
                "{} sent {} ({}, metadata only)",
                file.sender_name,
                file.file_name,
                format_bytes(file.size)
            )));
            self.end_transfer_skipped(&file, reason);
            return;
        }
        if file.size > max_download_bytes {
            // The server would stream this, but our per-room limit rejects it, so
            // tell it to stop relaying to us rather than letting it waste the
            // whole transfer.
            self.skip_offered_download(&file);
            let _ = self.events.send(NetworkEvent::Error(format!(
                "not receiving {}; size {} exceeds local limit {}",
                file.file_name,
                format_bytes(file.size),
                format_bytes(max_download_bytes)
            )));
            self.end_transfer_skipped(&file, size_label());
            return;
        }
        match target {
            DownloadTarget::Off => {
                self.skip_offered_download(&file);
                let _ = self.events.send(NetworkEvent::Status(format!(
                    "{} sent {} ({}, metadata only)",
                    file.sender_name,
                    file.file_name,
                    format_bytes(file.size)
                )));
                self.end_transfer_skipped(&file, "Automatic file receive disabled".to_string());
            }
            DownloadTarget::Memory => {
                // Reserve the file's bytes in the ring up front so peak memory
                // stays bounded across concurrent transfers. A file that cannot
                // fit the whole ring is rejected here rather than buffered and
                // dropped when it fails to be stored.
                let cap = self.config.download_store.capacity();
                let Some(reservation) = self.config.download_store.reserve(file.size) else {
                    self.skip_offered_download(&file);
                    let reason = format!(
                        "File exceeds the in-memory download buffer ({})",
                        format_bytes(cap)
                    );
                    let _ = self.events.send(NetworkEvent::Error(format!(
                        "not receiving {}; size {} exceeds the in-memory download buffer {}",
                        file.file_name,
                        format_bytes(file.size),
                        format_bytes(cap)
                    )));
                    self.end_transfer_skipped(&file, reason);
                    return;
                };
                let target = SinkTarget::Memory(Vec::with_capacity(file.size as usize));
                self.begin_incoming(file, target, ReceiveDest::Memory, Some(reservation), seal);
            }
            DownloadTarget::Persistent(receive_dir) => {
                match create_receive_file(
                    &self.config.download_store,
                    &receive_dir,
                    &file.file_name,
                ) {
                    Ok(receive) => {
                        self.begin_incoming(
                            file,
                            SinkTarget::Disk(receive.file),
                            ReceiveDest::Disk {
                                path: receive.path,
                                reservation: receive.reservation,
                            },
                            None,
                            seal,
                        );
                    }
                    Err(error) => {
                        // Setup failed after the server began relaying; tell it to
                        // stop and label the line rather than silently dropping
                        // the chunks it keeps sending.
                        self.skip_offered_download(&file);
                        let _ = self.events.send(NetworkEvent::Error(error));
                        self.end_transfer_skipped(&file, "Could not save the file".to_string());
                    }
                }
            }
        }
    }

    fn handle_untrusted_file_offered_ready(&mut self, mut file: FileMetadata) {
        if let Err(reason) = self.unseal_offer(&mut file) {
            let _ = self.events.send(NetworkEvent::Error(format!(
                "deferred file announcement failed after key discovery: {reason}"
            )));
            return;
        }
        self.skipped_untrusted_files.remove(&file.transfer_id);
        self.end_transfer_skipped(
            &file,
            "File not downloaded while identity was untrusted; ask the sender to resend"
                .to_string(),
        );
    }

    /// Resolves an encrypted offer through its authenticated MLS announcement
    /// and substitutes the true name, size, and encoding into `file`.
    fn unseal_offer(&mut self, file: &mut FileMetadata) -> Result<Option<IncomingSeal>, String> {
        if file.encoding != FileContentEncoding::Sealed {
            return Ok(None);
        }
        let event_id = file
            .mls_event_id
            .take()
            .ok_or_else(|| "MLS transfer omitted its announcement event id".to_string())?;
        let cached = self
            .mls_file_announcements
            .get(&event_id)
            .cloned()
            .ok_or_else(|| "MLS transfer announcement has not arrived".to_string())?;
        if cached.room_id != file.room_id {
            return Err("MLS transfer announcement names another room".to_string());
        }
        if cached.sender != file.sender {
            return Err(
                "MLS transfer sender does not match authenticated announcement".to_string(),
            );
        }
        let meta = &cached.file;
        let wire_bound = max_file_wire_bytes(FileContentEncoding::Sealed, file.size);
        file.timestamp_ms = cached.timestamp_ms;
        file.file_name = meta.name.clone();
        file.original_name.clone_from(&meta.name);
        file.size = meta.size;
        file.encoding = meta.encoding;
        Ok(Some(IncomingSeal {
            content_key: KeyMaterial {
                id: 1,
                bytes: meta.file_key,
            },
            event_id,
            transfer_id: meta.transfer_id,
            total_size: meta.size,
            digest: meta.digest,
            counter: 0,
            wire_bound,
        }))
    }

    /// Wraps `target` in an [`IncomingBody`] (applying the zstd decoder for
    /// compressed transfers), registers the [`IncomingFile`], and emits the
    /// initial progress tick. On decoder-init failure the partial destination is
    /// cleaned up and the transfer is abandoned.
    fn begin_incoming(
        &mut self,
        file: FileMetadata,
        target: SinkTarget,
        dest: ReceiveDest,
        reservation: Option<Reservation>,
        seal: Option<IncomingSeal>,
    ) {
        let sink = ReceiveSink::new(target, file.size, is_image_name(&file.file_name));
        let body = match file.encoding {
            FileContentEncoding::Identity => IncomingBody::Identity(sink),
            FileContentEncoding::Zstd => {
                let mut decoder = match zstd::stream::raw::Decoder::new() {
                    Ok(decoder) => decoder,
                    Err(error) => {
                        cleanup_partial(&dest);
                        // Tell the server to stop relaying and label the line;
                        // the reservation is released as it drops here.
                        self.skip_offered_download(&file);
                        let _ = self.events.send(NetworkEvent::Error(format!(
                            "failed to initialize decompression for {}: {error}",
                            file.file_name
                        )));
                        self.end_transfer_skipped(
                            &file,
                            "Could not start decompression".to_string(),
                        );
                        return;
                    }
                };
                if let Err(error) = decoder
                    .set_parameter(zstd::stream::raw::DParameter::WindowLogMax(ZSTD_WINDOW_LOG))
                {
                    cleanup_partial(&dest);
                    self.skip_offered_download(&file);
                    let _ = self.events.send(NetworkEvent::Error(format!(
                        "failed to limit decompression for {}: {error}",
                        file.file_name
                    )));
                    self.end_transfer_skipped(&file, "Could not start decompression".to_string());
                    return;
                }
                IncomingBody::Zstd(zstd::stream::zio::Writer::new(sink, decoder))
            }
            FileContentEncoding::Sealed => {
                // Sealed offers are rewritten to their inner encoding before
                // reaching here; a raw Sealed metadata is a protocol violation.
                cleanup_partial(&dest);
                self.skip_offered_download(&file);
                let _ = self.events.send(NetworkEvent::Error(format!(
                    "cannot receive sealed transfer {} without its metadata",
                    file.file_name
                )));
                self.end_transfer_skipped(&file, "Sealed transfer without metadata".to_string());
                return;
            }
        };
        let _ = self.events.send(NetworkEvent::Status(format!(
            "receiving {} from {}",
            file.file_name, file.sender_name
        )));
        let transfer_id = file.transfer_id;
        let room_id = file.room_id;
        let timestamp_ms = file.timestamp_ms;
        let total = file.size;
        self.incoming_files.insert(
            transfer_id,
            IncomingFile {
                metadata: file,
                dest,
                body,
                pending_wire: Vec::new(),
                pending_wire_offset: 0,
                wire_received: 0,
                complete_received: false,
                decoder_finished: false,
                next_status_at: FILE_PROGRESS_STEP_BYTES,
                seal,
                reservation,
            },
        );
        let _ = self.events.send(NetworkEvent::TransferProgress {
            room_id,
            transfer_id,
            timestamp_ms,
            transferred: 0,
            total,
            direction: TransferDirection::Incoming,
        });
    }

    fn handle_file_chunk(&mut self, transfer_id: FileTransferId, offset: u64, data: Vec<u8>) {
        let Some(incoming) = self.incoming_files.get_mut(&transfer_id) else {
            return;
        };
        if incoming.wire_received != offset {
            self.fail_incoming_file(transfer_id, "file transfer offset mismatch");
            return;
        }
        let end = offset.saturating_add(data.len() as u64);
        let wire_bound = match &incoming.seal {
            Some(seal) => seal.wire_bound,
            None => max_file_wire_bytes(incoming.metadata.encoding, incoming.metadata.size),
        };
        if end > wire_bound {
            self.fail_incoming_file(transfer_id, "file transfer exceeded allowed wire size");
            return;
        }
        // The server relays upload chunk boundaries 1:1, so for sealed
        // transfers each relayed chunk is exactly one AEAD frame, opened with
        // the running counter as its nonce.
        let data = match &mut incoming.seal {
            None => data,
            Some(seal) => {
                let mut frame = data;
                let payload = rpc::mls::open_file_chunk(
                    &seal.content_key.bytes,
                    incoming.metadata.room_id,
                    incoming.metadata.sender,
                    seal.event_id,
                    seal.transfer_id,
                    seal.total_size,
                    &seal.digest,
                    seal.counter,
                    &mut frame,
                );
                match payload {
                    Ok(payload) => {
                        seal.counter += 1;
                        payload
                    }
                    Err(_) => {
                        self.fail_incoming_file(transfer_id, "sealed file chunk failed to open");
                        return;
                    }
                }
            }
        };
        if incoming.pending_wire_offset > 0 {
            incoming
                .pending_wire
                .copy_within(incoming.pending_wire_offset.., 0);
            incoming
                .pending_wire
                .truncate(incoming.pending_wire.len() - incoming.pending_wire_offset);
            incoming.pending_wire_offset = 0;
        }
        incoming.pending_wire.extend_from_slice(&data);
        incoming.wire_received = end;
    }

    fn handle_file_complete(&mut self, transfer_id: FileTransferId) {
        let Some(incoming) = self.incoming_files.get_mut(&transfer_id) else {
            return;
        };
        incoming.complete_received = true;
    }

    fn handle_file_canceled(&mut self, transfer_id: FileTransferId, reason: &str) {
        if let Some(incoming) = self.incoming_files.remove(&transfer_id) {
            let room_id = incoming.metadata.room_id;
            cleanup_partial(&incoming.dest);
            let _ = self.events.send(NetworkEvent::Status(format!(
                "file transfer canceled for {}: {reason}",
                incoming.metadata.file_name
            )));
            // The recipient can't tell an uploader's abort from an upstream
            // failure; both read as the sender pulling the file.
            let _ = self.events.send(NetworkEvent::TransferEnded {
                room_id,
                transfer_id,
                timestamp_ms: incoming.metadata.timestamp_ms,
                verb: TerminalVerb::Skipped,
                reason: Some("Sender aborted transfer".to_string()),
            });
        }
    }

    fn pump_incoming_files(&mut self, wire_budget: &mut usize, decoded_budget: &mut usize) {
        let transfer_ids = self.incoming_files.keys().copied().collect::<Vec<_>>();
        for transfer_id in transfer_ids {
            if *wire_budget == 0 || *decoded_budget == 0 {
                break;
            }
            let before;
            let pump_result;
            {
                let Some(incoming) = self.incoming_files.get_mut(&transfer_id) else {
                    continue;
                };
                before = incoming.body.sink().decoded;
                pump_result = incoming.pump(wire_budget, decoded_budget);
            }
            if let Err(error) = pump_result {
                self.fail_incoming_file(
                    transfer_id,
                    &format!("file transfer decode failed: {error}"),
                );
                continue;
            }

            let Some(incoming) = self.incoming_files.get_mut(&transfer_id) else {
                continue;
            };
            let decoded = incoming.body.sink().decoded;
            if decoded != before
                && (decoded >= incoming.next_status_at || decoded == incoming.metadata.size)
            {
                incoming.next_status_at = incoming
                    .next_status_at
                    .saturating_add(FILE_PROGRESS_STEP_BYTES);
                let _ = self.events.send(NetworkEvent::TransferProgress {
                    room_id: incoming.metadata.room_id,
                    transfer_id,
                    timestamp_ms: incoming.metadata.timestamp_ms,
                    transferred: decoded,
                    total: incoming.metadata.size,
                    direction: TransferDirection::Incoming,
                });
            }
            if !incoming.ready_to_finalize() {
                continue;
            }

            let incoming = self
                .incoming_files
                .remove(&transfer_id)
                .expect("incoming file exists");
            let room_id = incoming.metadata.room_id;
            let timestamp_ms = incoming.metadata.timestamp_ms;
            match incoming.finalize() {
                Ok((metadata, location, reservation, dimensions, _wire_bytes)) => {
                    #[cfg(test)]
                    LAST_RECEIVED_FILE_WIRE_BYTES
                        .store(_wire_bytes, std::sync::atomic::Ordering::Relaxed);
                    kvlog::debug!(
                        "file receive decoding completed",
                        file_name = metadata.file_name.as_str(),
                        original_bytes = metadata.size,
                        wire_bytes = _wire_bytes
                    );
                    let served_name = match location {
                        FinalizedLocation::Disk { path, served_name } => {
                            let _ = self.events.send(NetworkEvent::Status(format!(
                                "saved {} to {}",
                                metadata.file_name,
                                path.display()
                            )));
                            served_name
                        }
                        FinalizedLocation::Memory(bytes) => {
                            let size = bytes.len();
                            // The reservation taken at accept time guarantees the
                            // space; consume it to convert reserved bytes into a
                            // resident entry without exceeding the peak cap.
                            let stored = reservation.and_then(|reservation| {
                                self.config.download_store.insert_reserved(
                                    reservation,
                                    &metadata.file_name,
                                    bytes,
                                )
                            });
                            match stored {
                                Some(name) => {
                                    let _ = self.events.send(NetworkEvent::Status(format!(
                                        "received {} ({}, held in memory)",
                                        metadata.file_name,
                                        format_bytes(size as u64)
                                    )));
                                    name
                                }
                                None => {
                                    // No unique name could be allocated for the
                                    // file; drop it.
                                    let message = format!(
                                        "could not store {} in the in-memory download buffer",
                                        metadata.file_name
                                    );
                                    let _ = self.events.send(NetworkEvent::Error(message.clone()));
                                    let _ = self.events.send(NetworkEvent::TransferEnded {
                                        room_id,
                                        transfer_id,
                                        timestamp_ms,
                                        verb: TerminalVerb::Failed,
                                        reason: Some(message),
                                    });
                                    continue;
                                }
                            }
                        }
                    };
                    let _ = self.events.send(NetworkEvent::FileReceived {
                        metadata,
                        served_name,
                        dimensions,
                    });
                }
                Err((dest, name, error)) => {
                    cleanup_partial(&dest);
                    let message = format!("failed to finish receiving {name}: {error}");
                    let _ = self.events.send(NetworkEvent::Error(message.clone()));
                    let _ = self.events.send(NetworkEvent::TransferEnded {
                        room_id,
                        transfer_id,
                        timestamp_ms,
                        verb: TerminalVerb::Failed,
                        reason: Some(message),
                    });
                }
            }
        }
    }

    fn fail_incoming_file(&mut self, transfer_id: FileTransferId, reason: &str) {
        let Some(incoming) = self.incoming_files.remove(&transfer_id) else {
            return;
        };
        let room_id = incoming.metadata.room_id;
        let timestamp_ms = incoming.metadata.timestamp_ms;
        cleanup_partial(&incoming.dest);
        let message = format!("{reason} for {}", incoming.metadata.file_name);
        let _ = self.events.send(NetworkEvent::Error(message.clone()));
        let _ = self.events.send(NetworkEvent::TransferEnded {
            room_id,
            transfer_id,
            timestamp_ms,
            verb: TerminalVerb::Failed,
            reason: Some(message),
        });
    }

    fn handle_history_chunk_payload(&mut self, payload: &[u8]) -> Result<bool, String> {
        let Some(chunk) = history::decode_chunk(payload)? else {
            return Ok(false);
        };
        kvlog::info!(
            "client history chunk received",
            room_id = chunk.room_id.0,
            message_count = chunk.messages.len(),
            at_start = chunk.at_start,
            complete = chunk.complete
        );
        let opened = chunk
            .messages
            .iter()
            .cloned()
            .map(AuthenticatedChat::from)
            .collect();
        let _ = self.events.send(NetworkEvent::HistoryChunk {
            room_id: chunk.room_id,
            before: chunk.before,
            messages: opened,
            at_start: chunk.at_start,
            complete: chunk.complete,
        });
        Ok(true)
    }

    fn handle_server_control(&mut self, control: ServerControl) -> Result<(), String> {
        kvlog::info!(
            "server control handling",
            kind = server_control_kind(&control)
        );
        if mls_actor::handles_server_control(&control) {
            self.mls_server_pending += 1;
            return self.queue_mls_input(mls_actor::Input::Server {
                generation: self.mls_generation,
                control,
            });
        }
        match control {
            ServerControl::Authenticated {
                session_id,
                user_id,
                rooms,
                users,
                default_room,
                ..
            } => {
                let pending = PendingAuthentication {
                    session_id,
                    user_id,
                    rooms,
                    users,
                    default_room,
                };
                self.finish_authenticated(pending, DEFAULT_INITIAL_DEVICE_NAME)?;
            }
            ServerControl::OpenPaired { .. } => {
                kvlog::warn!("unexpected open-paired on established session; ignoring");
            }
            ServerControl::Chat { message } => {
                kvlog::info!(
                    "client chat received",
                    room_id = message.room_id.0,
                    message_id = message.message_id.0,
                    user_id = message.sender.0,
                    body_size = message.body.len()
                );
                let _ = self
                    .events
                    .send(NetworkEvent::Chat(AuthenticatedChat::from(message)));
            }
            ServerControl::ChatMutationRejected {
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
                let _ = self.events.send(NetworkEvent::ChatMutationRejected {
                    room_id,
                    target,
                    kind,
                    message,
                });
            }
            ServerControl::VoiceStarted {
                room_id,
                session_id,
                user_id,
                stream_id,
            } => {
                kvlog::info!(
                    "client voice started",
                    user_id = user_id.0,
                    stream_id = stream_id.0
                );
                let local = Some(session_id) == self.session_id;
                self.queue_voice_command(voice::VoiceCommand::VoiceStarted {
                    generation: self.voice_generation,
                    room_id,
                    session_id,
                    user_id,
                    stream_id,
                    local,
                })?;
                if local {
                    self.voice_room = Some(room_id);
                }
                let _ = self.events.send(NetworkEvent::VoiceStarted {
                    room_id,
                    session_id,
                    user_id,
                    stream_id,
                });
            }
            ServerControl::VoiceStopped {
                room_id,
                session_id,
                user_id,
                stream_id,
            } => {
                kvlog::info!(
                    "client voice stopped",
                    user_id = user_id.0,
                    stream_id = stream_id.0
                );
                let local = Some(session_id) == self.session_id;
                self.queue_voice_command(voice::VoiceCommand::VoiceStopped {
                    generation: self.voice_generation,
                    room_id,
                    session_id,
                    user_id,
                    stream_id,
                    local,
                })?;
                if local && self.voice_room == Some(room_id) {
                    self.voice_room = None;
                }
                let _ = self.events.send(NetworkEvent::VoiceStopped {
                    room_id,
                    session_id,
                    user_id,
                    stream_id,
                });
            }
            ServerControl::VoiceStatus {
                user_id, status, ..
            } => {
                kvlog::info!(
                    "client voice status received",
                    user_id = user_id.0,
                    muted = status.muted,
                    deafened = status.deafened
                );
                if audio_pop_logging_enabled() {
                    kvlog::info!(
                        "audio pop control voice status rx",
                        user_id = user_id.0,
                        muted = status.muted,
                        deafened = status.deafened
                    );
                }
                let _ = self
                    .events
                    .send(NetworkEvent::VoiceStatus { user_id, status });
            }
            ServerControl::VoiceJoinFailed { room_id, message } => {
                kvlog::warn!(
                    "client voice join failed",
                    room_id = room_id.0,
                    error = message.as_str()
                );
                let _ = self
                    .events
                    .send(NetworkEvent::VoiceJoinFailed { room_id, message });
            }
            ServerControl::RoomRttSnapshot { room_id, members } => {
                self.queue_voice_command(voice::VoiceCommand::RoomRttSnapshot {
                    generation: self.voice_generation,
                    room_id,
                    members,
                })?;
            }
            ServerControl::UdpBound => {
                let _ = self
                    .events
                    .send(NetworkEvent::Status("udp media bound".to_string()));
                self.queue_voice_command(voice::VoiceCommand::UdpBound {
                    generation: self.voice_generation,
                })?;
            }
            ServerControl::UdpReflexive { addr } => match addr.parse::<SocketAddr>() {
                Ok(addr) => {
                    kvlog::info!("client udp reflexive address received", addr = %addr);
                    self.queue_voice_command(voice::VoiceCommand::UdpReflexive {
                        generation: self.voice_generation,
                        addr,
                    })?;
                }
                Err(error) => {
                    kvlog::warn!("invalid udp reflexive address", addr = addr.as_str(), error = %error);
                }
            },
            ServerControl::P2pNatProbe { probe_id, addr } => match addr.parse::<SocketAddr>() {
                Ok(addr) => {
                    kvlog::info!(
                        "client nat probe observation received",
                        probe_id,
                        addr = %addr
                    );
                    self.queue_voice_command(voice::VoiceCommand::NatProbeObserved {
                        generation: self.voice_generation,
                        probe_id,
                        addr,
                    })?;
                }
                Err(error) => {
                    kvlog::warn!(
                        "invalid nat probe address",
                        probe_id,
                        addr = addr.as_str(),
                        error = %error
                    );
                }
            },
            ServerControl::P2pPeer { peer } => {
                self.queue_voice_command(voice::VoiceCommand::InstallPeer {
                    generation: self.voice_generation,
                    peer,
                })?;
            }
            ServerControl::P2pPeerGone {
                session_id,
                user_id,
            } => {
                self.queue_voice_command(voice::VoiceCommand::RemovePeer {
                    generation: self.voice_generation,
                    session_id,
                    user_id,
                })?;
                kvlog::info!(
                    "p2p peer removed",
                    session_id = session_id.0,
                    user_id = user_id.0
                );
            }
            ServerControl::FileOffered { file, contents } => {
                self.handle_file_offered(file, contents)?;
            }
            ServerControl::UploadFileAccepted {
                client_transfer_id,
                file,
            } => {
                self.handle_upload_accepted(client_transfer_id, file);
            }
            ServerControl::FileChunk {
                transfer_id,
                offset,
                data,
            } => {
                if self.skipped_untrusted_files.contains(&transfer_id) {
                    // The transfer was declined before any sealed metadata was
                    // exposed. A server race may still deliver chunks; never
                    // retain attacker-sized pre-trust file ciphertext.
                } else {
                    self.handle_file_chunk(transfer_id, offset, data);
                }
            }
            ServerControl::FileComplete { transfer_id } => {
                if !self.skipped_untrusted_files.contains(&transfer_id) {
                    self.handle_file_complete(transfer_id);
                }
            }
            ServerControl::FileCanceled {
                transfer_id,
                reason,
            } => {
                if !self.skipped_untrusted_files.contains(&transfer_id) {
                    self.handle_file_canceled(transfer_id, &reason);
                }
            }
            ServerControl::ShareStarted {
                room_id,
                stream_id,
                publish_secret,
                codec,
                coded_width,
                coded_height,
                extradata,
            } => {
                self.pending_share_start = false;
                kvlog::info!("client share started", stream_id = stream_id.0);
                let _ = self.events.send(NetworkEvent::ShareStarted {
                    room_id,
                    stream_id,
                    publish_secret,
                    codec,
                    coded_width,
                    coded_height,
                    extradata,
                });
            }
            ServerControl::ShareAvailable {
                room_id,
                stream_id,
                user_id: _,
                sender_name,
                codec,
                coded_width,
                coded_height,
                annexb: _,
                extradata,
                view_secret,
            } => {
                kvlog::info!(
                    "client share available",
                    room_id = room_id.0,
                    stream_id = stream_id.0,
                    codec = codec.as_str()
                );
                let _ = self.events.send(NetworkEvent::ShareAvailable {
                    room_id,
                    stream_id,
                    sender_name,
                    codec,
                    coded_width,
                    coded_height,
                    extradata,
                    view_secret,
                });
            }
            ServerControl::ShareEnded { room_id, stream_id } => {
                kvlog::info!(
                    "client share ended",
                    room_id = room_id.0,
                    stream_id = stream_id.0
                );
                let _ = self.events.send(NetworkEvent::ShareEnded { stream_id });
            }
            ServerControl::Pong { .. } => {}
            ServerControl::BugReportSaved { report_id } => {
                kvlog::info!("server saved bug report", report_id = report_id.0);
            }
            ServerControl::Error { code, message } => {
                kvlog::warn!("server control error", error = message.as_str());
                if self.session_id.is_none() && is_auth_failure_code(code) {
                    self.auth_failure = Some((code, message));
                    self.shutdown = true;
                } else if self.pending_share_start {
                    self.pending_share_start = false;
                    let _ = self
                        .events
                        .send(NetworkEvent::ShareStartRejected { message });
                } else {
                    let _ = self.events.send(NetworkEvent::Error(message));
                }
            }
            ServerControl::RoomUpserted { room } => {
                kvlog::info!(
                    "client room upserted",
                    room_id = room.room_id.0,
                    name = room.name.as_str()
                );
                self.room_kinds.insert(room.room_id, room.kind.clone());
                self.queue_mls_input(mls_actor::Input::RoomUpserted(room.clone()))?;
                let _ = self.events.send(NetworkEvent::RoomUpserted(room));
            }
            ServerControl::DmOpened { room_id, peer } => {
                // Authenticated room metadata is the authority for this
                // mapping. The app separately correlates the peer with the UI
                // clients that requested it, so a late or duplicate response
                // must not tear down the network session.
                self.queue_mls_input(mls_actor::Input::DmOpened { room_id, peer })?;
                let _ = self.events.send(NetworkEvent::DmOpened { room_id, peer });
            }
            ServerControl::Presence { user, online } => {
                kvlog::info!(
                    "client presence received",
                    user_id = user.user_id.0,
                    username = user.username.as_str(),
                    online
                );
                if !online {
                    self.queue_voice_command(voice::VoiceCommand::UserOffline {
                        generation: self.voice_generation,
                        user_id: user.user_id,
                    })?;
                }
                self.user_names.insert(user.user_id, user.username.clone());
                self.queue_mls_input(mls_actor::Input::Presence {
                    user: user.clone(),
                    online,
                })?;
                let _ = self.events.send(NetworkEvent::Presence { user, online });
            }
            ServerControl::UploadDeclined {
                client_transfer_id,
                reason,
            } => self.handle_upload_declined(client_transfer_id, &reason),
            _ => unreachable!("MLS controls are routed to the dedicated worker above"),
        }
        Ok(())
    }

    fn finish_authenticated(
        &mut self,
        pending: PendingAuthentication,
        device_name: &str,
    ) -> Result<(), String> {
        let PendingAuthentication {
            session_id,
            user_id,
            rooms,
            users,
            default_room,
        } = pending;
        self.session_id = Some(session_id);
        self.user_id = Some(user_id);
        self.active_room = Some(default_room);
        kvlog::info!(
            "client authenticated",
            session_id = session_id.0,
            user_id = user_id.0,
            room_count = rooms.len(),
            user_count = users.len()
        );
        self.user_names.clear();
        let mut folded_names = HashSet::new();
        for user in &users {
            if !folded_names.insert(user.username.to_ascii_lowercase()) {
                return Err("server directory contains duplicate usernames".to_string());
            }
            self.user_names.insert(user.user_id, user.username.clone());
        }
        for room in &rooms {
            self.room_kinds.insert(room.room_id, room.kind.clone());
        }
        self.mls_available = false;
        self.mls_session_ready = false;
        self.queue_mls_input(mls_actor::Input::BeginSession {
            generation: self.mls_generation,
            session_id,
            user_id,
            rooms: rooms.clone(),
            users: users.clone(),
            server_public_key: self.server_public_key,
            device_name: device_name.to_string(),
        })?;
        let initial_udp_bind = self
            .pending_initial_udp_bind
            .take()
            .ok_or_else(|| "initial UDP bind was already dispatched".to_string())?;
        let send_error = initial_udp_bind.dispatch()?;
        report_initial_udp_bind_send_error(&self.events, send_error);
        self.queue_voice_command(voice::VoiceCommand::Authenticated {
            generation: self.voice_generation,
            session_id,
        })?;
        let _ = self.events.send(NetworkEvent::Authenticated {
            session_id,
            user_id,
            rooms,
            users,
            default_room,
            video_transport_mode: self.transport_mode,
            video_auth_key: self.video_auth_key,
        });
        Ok(())
    }

    fn observe_peer_account(&mut self, user_id: UserId, account_id: AccountId) {
        let username = self
            .user_names
            .get(&user_id)
            .cloned()
            .unwrap_or_else(|| user_id.to_string());
        let room_ids = self
            .room_kinds
            .iter()
            .filter_map(|(room_id, kind)| match kind {
                RoomKind::Dm { user_a, user_b }
                    if (*user_a == user_id && Some(*user_b) == self.user_id)
                        || (*user_b == user_id && Some(*user_a) == self.user_id) =>
                {
                    Some(*room_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for room_id in room_ids {
            if let Some(pin) = self
                .e2e
                .observe_account(room_id, user_id, &username, account_id)
            {
                let _ = self.events.send(NetworkEvent::E2ePeerPinProposed {
                    pin,
                    manual_verification: false,
                });
            }
            if let Some(identity) = self.e2e.accepted_identity(user_id) {
                let _ = self
                    .events
                    .send(NetworkEvent::E2ePeerPinMatched { identity });
            }
        }
    }

    fn room_requires_mls(&self, room_id: RoomId) -> bool {
        // Room metadata can reach linked workers at different times. Never
        // treat an as-yet unknown room as public: queueing its content in the
        // durable MLS outbox is safe, while an ordinary-chat fallback would
        // either leak plaintext or be rejected by the server for a DM.
        !matches!(self.room_kinds.get(&room_id), Some(RoomKind::Public))
    }

    fn mark_mls_upload_announcement_delivered(&mut self, room_id: RoomId, event_id: EventId) {
        let mut matched = 0;
        for upload in &mut self.outgoing_uploads {
            if upload.room_id == room_id
                && let Some(seal) = upload
                    .seal
                    .as_mut()
                    .filter(|seal| seal.event_id == event_id)
            {
                seal.announcement_delivered = true;
                matched += 1;
            }
        }
        debug_assert!(matched <= 1, "one MLS event unlocked multiple file uploads");
    }

    fn defer_mls_file_offer(&mut self, item: PendingMlsFileOffer) -> Result<(), String> {
        let room_id = item
            .room_id()
            .ok_or_else(|| "encrypted DM data arrived without a room".to_string())?;
        let retained = item.retained_bytes();
        let room_full = self
            .pending_mls_file_offers
            .get(&room_id)
            .is_some_and(|room| {
                room.items.len() >= MAX_PENDING_MLS_FILE_ITEMS
                    || room.bytes.saturating_add(retained) > MAX_PENDING_MLS_FILE_BYTES
            });
        let global_full = self.pending_mls_file_items >= MAX_PENDING_MLS_FILE_GLOBAL_ITEMS
            || self.pending_mls_file_bytes.saturating_add(retained)
                > MAX_PENDING_MLS_FILE_GLOBAL_BYTES;
        if room_full || global_full {
            self.drop_pending_mls_file_offer(room_id, item);
            return Ok(());
        }
        let room = self.pending_mls_file_offers.entry(room_id).or_default();
        room.bytes = room.bytes.saturating_add(retained);
        self.pending_mls_file_bytes = self.pending_mls_file_bytes.saturating_add(retained);
        self.pending_mls_file_items = self.pending_mls_file_items.saturating_add(1);
        room.items.push_back(item);
        Ok(())
    }

    fn drop_pending_mls_file_offer(&mut self, room_id: RoomId, item: PendingMlsFileOffer) {
        match item {
            PendingMlsFileOffer::FileOffered { file, .. } => {
                self.skipped_untrusted_files.remove(&file.transfer_id);
                self.end_transfer_skipped(
                    &file,
                    "encryption identity backlog was full".to_string(),
                );
            }
        }
        if self.pending_mls_file_warned_rooms.insert(room_id) {
            let _ = self.events.send(NetworkEvent::Error(
                "Some encrypted file offers were unavailable because their MLS announcements arrived too late. The connection remains active."
                    .to_string(),
            ));
        }
    }

    fn drain_pending_mls_file_offers(&mut self, room_id: RoomId) -> Result<usize, String> {
        let Some(mut room) = self.pending_mls_file_offers.remove(&room_id) else {
            return Ok(0);
        };
        self.pending_mls_file_bytes = self.pending_mls_file_bytes.saturating_sub(room.bytes);
        self.pending_mls_file_items = self.pending_mls_file_items.saturating_sub(room.items.len());
        room.bytes = 0;
        let mut recovered = 0usize;
        let count = room.items.len();
        for _ in 0..count {
            let Some(item) = room.items.pop_front() else {
                break;
            };
            let logical_items = item.logical_items();
            match item {
                PendingMlsFileOffer::FileOffered {
                    file,
                    contents,
                    skipped_untrusted,
                } => {
                    if self.file_offer_ready(&file)? {
                        if skipped_untrusted {
                            self.handle_untrusted_file_offered_ready(file);
                        } else {
                            self.handle_file_offered_ready(file, contents);
                        }
                    } else {
                        room.items.push_front(PendingMlsFileOffer::FileOffered {
                            file,
                            contents,
                            skipped_untrusted,
                        });
                        break;
                    }
                }
            }
            recovered = recovered.saturating_add(logical_items);
        }
        if !room.items.is_empty() {
            room.bytes = room
                .items
                .iter()
                .map(PendingMlsFileOffer::retained_bytes)
                .sum();
            self.pending_mls_file_bytes = self.pending_mls_file_bytes.saturating_add(room.bytes);
            self.pending_mls_file_items =
                self.pending_mls_file_items.saturating_add(room.items.len());
            self.pending_mls_file_offers.insert(room_id, room);
        }
        Ok(recovered)
    }

    /// Handles the server telling us our upload lost its last recipient: cancel
    /// the now-pointless transfer locally, which also stops streaming and shows
    /// the `cancelled: <reason>` terminal label. Unknown ids (already gone) are a
    /// no-op.
    fn handle_upload_declined(&mut self, client_transfer_id: FileTransferId, reason: &str) {
        kvlog::info!(
            "upload declined by server",
            client_transfer_id = client_transfer_id.0,
            reason
        );
        let Some(index) = self
            .outgoing_uploads
            .iter()
            .position(|upload| upload.transfer_id == client_transfer_id)
        else {
            return;
        };
        let upload = self
            .outgoing_uploads
            .remove(index)
            .expect("index in bounds");
        let _ = self.cancel_outgoing_upload(upload, reason, UploadAbort::Declined);
    }
}

fn correlate_upload_accepted(
    outgoing: &mut VecDeque<OutgoingUpload>,
    pending: &mut HashMap<FileTransferId, PendingLocalFile>,
    client_transfer_id: FileTransferId,
    metadata: FileMetadata,
) -> Option<(FileMetadata, PendingLocalFile)> {
    if let Some(upload) = outgoing
        .iter_mut()
        .find(|upload| upload.transfer_id == client_transfer_id)
    {
        let mut metadata = metadata;
        if let Some(seal) = &upload.seal {
            unseal_own_metadata(
                &mut metadata,
                &upload.name,
                upload.size,
                seal.inner_encoding,
            );
        }
        upload.server_metadata = Some(metadata);
        return None;
    }
    pending.remove(&client_transfer_id).map(|local| {
        let mut metadata = metadata;
        if let Some((name, size, inner_encoding)) = &local.sealed_real {
            unseal_own_metadata(&mut metadata, name, *size, *inner_encoding);
        }
        (metadata, local)
    })
}

/// Restores the uploader's real file metadata over the sealed wire placeholder
/// so the sender's own views (file line, web log) show the true file.
fn unseal_own_metadata(
    metadata: &mut FileMetadata,
    name: &str,
    size: u64,
    inner_encoding: FileContentEncoding,
) {
    metadata.file_name = name.to_string();
    metadata.original_name = name.to_string();
    metadata.size = size;
    metadata.encoding = inner_encoding;
    metadata.mls_event_id = None;
}

fn compressed_upload_source_read_ahead_is_limited(
    upload: &OutgoingUpload,
    throttle: &UploadThrottle,
) -> bool {
    throttle.is_limited() && upload.body.encoding() == FileContentEncoding::Zstd
}

fn upload_source_read_capacity(upload: &OutgoingUpload, throttle: &UploadThrottle) -> u64 {
    if compressed_upload_source_read_ahead_is_limited(upload, throttle) {
        MAX_COMPRESSED_UPLOAD_SOURCE_AHEAD_BYTES.saturating_sub(upload.source_read_ahead)
    } else {
        u64::MAX
    }
}

fn upload_should_flush_source_read_ahead(
    upload: &OutgoingUpload,
    throttle: &UploadThrottle,
) -> bool {
    compressed_upload_source_read_ahead_is_limited(upload, throttle)
        && !upload.source_finished
        && upload.source_read_ahead >= MAX_COMPRESSED_UPLOAD_SOURCE_AHEAD_BYTES
        && upload.body.pending().is_empty()
}

fn upload_ready_now(upload: &OutgoingUpload, pending: usize, throttle: &UploadThrottle) -> bool {
    if !upload.started {
        return upload
            .seal
            .as_ref()
            .is_none_or(|seal| seal.announcement_delivered);
    }
    (!upload.source_finished
        && pending < MAX_QUEUED_FILE_BYTES
        && upload_source_read_capacity(upload, throttle) > 0)
        || upload_should_flush_source_read_ahead(upload, throttle)
        || (upload.source_finished && !upload.encoder_finished)
        || (upload.encoder_finished && pending == 0)
}

fn read_upload_source(upload: &mut OutgoingUpload, limit: usize) -> io::Result<Vec<u8>> {
    #[cfg(test)]
    if take_test_upload_source_failure(&upload.source_path, upload.source_offset) {
        return Err(io::Error::other("injected upload source read failure"));
    }
    if upload.source_prefix_offset < upload.source_prefix.len() {
        let end = upload
            .source_prefix_offset
            .saturating_add(limit)
            .min(upload.source_prefix.len());
        let data = upload.source_prefix[upload.source_prefix_offset..end].to_vec();
        upload.source_prefix_offset = end;
        if upload.source_prefix_offset == upload.source_prefix.len() {
            upload.source_prefix.clear();
            upload.source_prefix_offset = 0;
        }
        return Ok(data);
    }

    let mut data = vec![0; limit];
    let read = upload.file.read(&mut data)?;
    data.truncate(read);
    Ok(data)
}

fn write_upload_local_copy(events: &NetworkEventSender, upload: &mut OutgoingUpload, data: &[u8]) {
    match upload.local_copy.as_mut() {
        Some(UploadLocalCopy::Disk { path, file, .. }) => {
            if let Err(error) = file.write_all(data) {
                let _ = events.send(NetworkEvent::Error(format!(
                    "failed to write local copy {}: {error}",
                    path.display()
                )));
                let _ = fs::remove_file(path);
                upload.local_copy = None;
            }
        }
        Some(UploadLocalCopy::Memory { bytes, .. }) => bytes.extend_from_slice(data),
        None => {}
    }
}

fn capture_upload_image_prefix(upload: &mut OutgoingUpload, data: &[u8]) {
    if upload.dimensions.is_some()
        || !is_image_name(&upload.name)
        || upload.image_prefix.len() >= MAX_FILE_CHUNK_BYTES
    {
        return;
    }
    let capture = data
        .len()
        .min(MAX_FILE_CHUNK_BYTES - upload.image_prefix.len());
    upload.image_prefix.extend_from_slice(&data[..capture]);
    upload.dimensions = crate::web_server::image_dimensions(&upload.image_prefix);
}

fn network_command_kind(command: &NetworkCommand) -> &'static str {
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
        NetworkCommand::SetVoiceStatus(_) => "set_voice_status",
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

/// Wall-clock UNIX milliseconds, stamped inside MLS application events.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn random_event_id() -> Result<rpc::ids::EventId, String> {
    let mut bytes = [0u8; 16];
    aws_lc_rs::rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "failed to generate MLS event id".to_string())?;
    Ok(rpc::ids::EventId(bytes))
}

fn mls_message_id(sequence: u64) -> MessageId {
    debug_assert_eq!(
        sequence & (1 << 63),
        0,
        "MLS delivery sequence exhausted its message-id namespace"
    );
    MessageId(sequence | (1 << 63))
}

/// Records an outstanding RTT probe, evicting the oldest entry once the queue
/// reaches [`RTT_IN_FLIGHT_CAP`] so a destination that stops replying cannot grow
/// the queue without bound.
fn push_rtt_in_flight(queue: &mut VecDeque<(u64, Instant)>, nonce: u64, now: Instant) {
    if queue.len() >= RTT_IN_FLIGHT_CAP {
        queue.pop_front();
    }
    queue.push_back((nonce, now));
}

/// Matches a `Pong` nonce against the outstanding probes and, on a hit, removes
/// it and returns the elapsed round-trip time in milliseconds.
fn take_rtt_sample(queue: &mut VecDeque<(u64, Instant)>, nonce: u64, now: Instant) -> Option<f32> {
    let index = queue.iter().position(|(probe, _)| *probe == nonce)?;
    let (_, sent) = queue.remove(index)?;
    Some(now.saturating_duration_since(sent).as_secs_f32() * 1000.0)
}

/// Folds a fresh RTT sample into the running EWMA, seeding it on the first sample.
fn fold_rtt_ewma(current: Option<f32>, sample_ms: f32) -> f32 {
    match current {
        Some(previous) => previous + RTT_EWMA_WEIGHT * (sample_ms - previous),
        None => sample_ms,
    }
}

/// Rounds a smoothed RTT to whole milliseconds, clamped into `u16` range.
fn clamp_rtt_ms(rtt_ms: f32) -> u16 {
    rtt_ms.round().clamp(0.0, u16::MAX as f32) as u16
}

fn combined_relay_rtt(local_rtt_ms: Option<f32>, remote_rtt_ms: Option<u16>) -> Option<u16> {
    Some(clamp_rtt_ms(local_rtt_ms?).saturating_add(remote_rtt_ms?))
}

fn rtt_sample_is_stale(sample_at: Option<Instant>, now: Instant) -> bool {
    sample_at.is_some_and(|sample_at| now.saturating_duration_since(sample_at) >= RTT_STALE_AFTER)
}

fn media_feedback_from_live(feedback: LivePlaybackFeedback) -> media::VoiceFeedback {
    media::VoiceFeedback {
        highest_contiguous_sequence: feedback.highest_contiguous_sequence,
        expected_packets: feedback.expected_packets,
        lost_packets: feedback.lost_packets,
        late_packets: feedback.late_packets,
        duplicate_packets: feedback.duplicate_packets,
        reordered_packets: feedback.reordered_packets,
        window_ms: feedback.window_ms,
        max_output_ring_ms: feedback.max_output_ring_ms,
        max_neteq_target_ms: feedback.max_neteq_target_ms,
        max_neteq_playout_delay_ms: feedback.max_neteq_playout_delay_ms,
        max_neteq_packet_buffer_ms: feedback.max_neteq_packet_buffer_ms,
        max_interarrival_jitter_ms: feedback.max_interarrival_jitter_ms,
    }
}

fn live_feedback_from_media(
    stream_id: StreamId,
    feedback: media::VoiceFeedback,
) -> LivePlaybackFeedback {
    LivePlaybackFeedback {
        stream_id: stream_id.0,
        highest_contiguous_sequence: feedback.highest_contiguous_sequence,
        expected_packets: feedback.expected_packets,
        lost_packets: feedback.lost_packets,
        late_packets: feedback.late_packets,
        duplicate_packets: feedback.duplicate_packets,
        reordered_packets: feedback.reordered_packets,
        window_ms: feedback.window_ms,
        max_output_ring_ms: feedback.max_output_ring_ms,
        max_neteq_target_ms: feedback.max_neteq_target_ms,
        max_neteq_playout_delay_ms: feedback.max_neteq_playout_delay_ms,
        max_neteq_packet_buffer_ms: feedback.max_neteq_packet_buffer_ms,
        max_interarrival_jitter_ms: feedback.max_interarrival_jitter_ms,
    }
}

fn server_control_kind(control: &ServerControl) -> &'static str {
    match control {
        ServerControl::Authenticated { .. } => "authenticated",
        ServerControl::OpenPaired { .. } => "open_paired",
        ServerControl::Chat { .. } => "chat",
        ServerControl::ChatMutationRejected { .. } => "chat_mutation_rejected",
        ServerControl::Presence { .. } => "presence",
        ServerControl::VoiceStarted { .. } => "voice_started",
        ServerControl::VoiceStopped { .. } => "voice_stopped",
        ServerControl::VoiceStatus { .. } => "voice_status",
        ServerControl::VoiceJoinFailed { .. } => "voice_join_failed",
        ServerControl::RoomRttSnapshot { .. } => "room_rtt_snapshot",
        ServerControl::UdpBound => "udp_bound",
        ServerControl::UdpReflexive { .. } => "udp_reflexive",
        ServerControl::P2pNatProbe { .. } => "p2p_nat_probe",
        ServerControl::P2pPeer { .. } => "p2p_peer",
        ServerControl::P2pPeerGone { .. } => "p2p_peer_gone",
        ServerControl::FileOffered { .. } => "file_offered",
        ServerControl::UploadFileAccepted { .. } => "upload_file_accepted",
        ServerControl::FileChunk { .. } => "file_chunk",
        ServerControl::FileComplete { .. } => "file_complete",
        ServerControl::FileCanceled { .. } => "file_canceled",
        ServerControl::ShareStarted { .. } => "share_started",
        ServerControl::ShareAvailable { .. } => "share_available",
        ServerControl::ShareEnded { .. } => "share_ended",
        ServerControl::Pong { .. } => "pong",
        ServerControl::Error { .. } => "error",
        ServerControl::BugReportSaved { .. } => "bug_report_saved",
        ServerControl::RoomUpserted { .. } => "room_upserted",
        ServerControl::DmOpened { .. } => "dm_opened",
        ServerControl::UploadDeclined { .. } => "upload_declined",
        ServerControl::DeviceLinkCreated { .. } => "device_link_created",
        ServerControl::DeviceLinkCanceled { .. } => "device_link_canceled",
        ServerControl::DeviceLinkBundle { .. } => "device_link_bundle",
        ServerControl::DeviceLinked { .. } => "device_linked",
        ServerControl::DeviceLinkRedeemed { .. } => "device_link_redeemed",
        ServerControl::DeviceRoster { .. } => "device_roster",
        ServerControl::DeviceRosterStored { .. } => "device_roster_stored",
        ServerControl::DeviceRosterConflict { .. } => "device_roster_conflict",
        ServerControl::MlsDeviceBound { .. } => "mls_device_bound",
        ServerControl::KeyPackagesPublished { .. } => "key_packages_published",
        ServerControl::KeyPackagesLow { .. } => "key_packages_low",
        ServerControl::KeyPackage { .. } => "key_package",
        ServerControl::EncryptedRoomCreated { .. } => "encrypted_room_created",
        ServerControl::EncryptedRoomCreationStale { .. } => "encrypted_room_creation_stale",
        ServerControl::GroupInfo { .. } => "group_info",
        ServerControl::MlsCommitSubmitted { .. } => "mls_commit_submitted",
        ServerControl::MlsApplicationSubmitted { .. } => "mls_application_submitted",
        ServerControl::MlsEvents { .. } => "mls_events",
        ServerControl::MlsWelcomes { .. } => "mls_welcomes",
    }
}

fn is_auth_failure_code(code: u16) -> bool {
    matches!(
        code,
        ERROR_AUTH_REJECTED
            | ERROR_PAIRING_NOT_ACTIVE
            | ERROR_PAIRING_CODE_MISMATCH
            | ERROR_PAIRING_INVALID_REQUEST
            | ERROR_PUBLIC_DISABLED
            | ERROR_TOKEN_STALE_EPOCH
            | ERROR_USERNAME_TAKEN
    )
}

/// Whether `name`'s extension marks it as an image worth probing for size.
fn is_image_name(name: &str) -> bool {
    crate::web_server::classify(name) == "image"
}

/// Creates a uniquely named file under `dir` for a persistent download and
/// registers its served name and absolute path in `store`, so the web server can
/// serve it from any directory and the name is never reused. The candidate name
/// must be free in both the store (memory and disk) and the filesystem.
fn create_receive_file(
    store: &crate::receive_store::DownloadStore,
    dir: &Path,
    requested_name: &str,
) -> Result<ReceiveFile, String> {
    fs::create_dir_all(dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    crate::receive_store::allocate_name(requested_name, |candidate| {
        if !store.name_available(candidate) {
            return None;
        }
        let path = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                let Some(reservation) = store.reserve_disk_name(candidate.to_string()) else {
                    let _ = fs::remove_file(&path);
                    return None;
                };
                Some(Ok(ReceiveFile {
                    path,
                    file,
                    reservation,
                }))
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
            Err(error) => Some(Err(format!("failed to create {}: {error}", path.display()))),
        }
    })
    .unwrap_or_else(|| {
        Err(format!(
            "could not allocate a unique receive path for {}",
            sanitize_file_name(requested_name)
        ))
    })
}

/// Removes the on-disk partial of an aborted download. In-memory transfers hold
/// nothing on disk, so there is nothing to clean up.
fn cleanup_partial(dest: &ReceiveDest) {
    if let ReceiveDest::Disk { path, .. } = dest {
        let _ = fs::remove_file(path);
    }
}

/// Resolves the sanitized upload name from an optional override, falling back
/// to the source path's file name. Returns an error when the name is unusable
/// or exceeds the protocol limit.
fn upload_username(name_override: Option<String>, path: &Path) -> Result<String, String> {
    let raw = match name_override {
        Some(name) => name,
        None => path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "upload path must end in a UTF-8 file name".to_string())?
            .to_string(),
    };
    let name = sanitize_file_name(&raw);
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err("file name exceeds maximum length".to_string());
    }
    Ok(name)
}

fn mls_retry_backoff(failures: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(failures.min(16)).unwrap_or(u32::MAX);
    MLS_RETRY_INITIAL_BACKOFF
        .checked_mul(multiplier)
        .unwrap_or(MLS_RETRY_MAX_BACKOFF)
        .min(MLS_RETRY_MAX_BACKOFF)
}

fn defer_mls_retry_in(
    retries: &mut HashMap<MlsRetryKey, DelayedMlsRetry>,
    key: MlsRetryKey,
    request: ClientControl,
    now: Instant,
) -> Duration {
    let retry = retries.entry(key).or_insert(DelayedMlsRetry {
        request: request.clone(),
        failures: 0,
        retry_at: None,
    });
    retry.request = request;
    let delay = mls_retry_backoff(retry.failures);
    retry.failures = retry.failures.saturating_add(1);
    retry.retry_at = Some(now + delay);
    delay
}

fn take_ready_mls_retries(
    retries: &mut HashMap<MlsRetryKey, DelayedMlsRetry>,
    now: Instant,
) -> Vec<ClientControl> {
    retries
        .values_mut()
        .filter_map(|retry| {
            retry.retry_at.filter(|retry_at| *retry_at <= now).map(|_| {
                retry.retry_at = None;
                retry.request.clone()
            })
        })
        .collect()
}

/// Opens the upload source, then unlinks it when `delete_after_open` is set. The
/// returned handle keeps the bytes reachable for streaming, so staged temp files
/// clean themselves up without waiting for the upload to finish.
fn open_upload_source(path: &Path, delete_after_open: bool) -> std::io::Result<File> {
    let file = File::open(path)?;
    if delete_after_open {
        let _ = fs::remove_file(path);
    }
    Ok(file)
}

fn file_sha256(path: &Path) -> Result<[u8; 32], String> {
    let mut file =
        File::open(path).map_err(|error| format!("failed to hash {}: {error}", path.display()))?;
    let mut context = aws_lc_rs::digest::Context::new(&aws_lc_rs::digest::SHA256);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to hash {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(context.finish().as_ref().try_into().unwrap())
}

pub(crate) fn sanitize_file_name(name: &str) -> String {
    let trimmed = name.rsplit(['/', '\\']).next().unwrap_or("file").trim();
    let mut out = String::with_capacity(trimmed.len().max(4));
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches([' ', '.']);
    if out.is_empty() {
        "file".to_string()
    } else {
        out.to_string()
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn media_payload_kind(payload: &MediaPayload) -> &'static str {
    match payload {
        MediaPayload::Bind { .. } => "bind",
        MediaPayload::NatProbe { .. } => "nat_probe",
        MediaPayload::Voice { .. } => "voice",
        MediaPayload::PeerVoice { .. } => "peer_voice",
        MediaPayload::VoiceFeedback { .. } => "voice_feedback",
        MediaPayload::PeerVoiceFeedback { .. } => "peer_voice_feedback",
        MediaPayload::VoiceFeedbackFrom { .. } => "voice_feedback_from",
        MediaPayload::Ping { .. } => "ping",
        MediaPayload::Pong { .. } => "pong",
    }
}

fn media_payload_from_audio(payload: &AudioVoicePayload) -> MediaVoicePayload {
    match payload {
        AudioVoicePayload::Opus(opus) => MediaVoicePayload::Opus(opus.clone()),
        AudioVoicePayload::Silence => MediaVoicePayload::Silence,
    }
}

fn media_voice_payload_kind(payload: &MediaVoicePayload) -> &'static str {
    match payload {
        MediaVoicePayload::Opus(_) => "opus",
        MediaVoicePayload::Silence => "silence",
    }
}

fn allocate_local_voice_sequence(local_sequence: &mut u32) -> u32 {
    let sequence = *local_sequence;
    *local_sequence = (*local_sequence).wrapping_add(1);
    sequence
}

fn advance_local_voice_sequence_past(local_sequence: &mut u32, sequence: u32) {
    *local_sequence = (*local_sequence).max(sequence.wrapping_add(1));
}

fn voice_payload_kind(payload: &AudioVoicePayload) -> &'static str {
    match payload {
        AudioVoicePayload::Opus(_) => "opus",
        AudioVoicePayload::Silence => "silence",
    }
}

fn audio_payload_from_media(payload: MediaVoicePayload) -> AudioVoicePayload {
    match payload {
        MediaVoicePayload::Opus(opus) => AudioVoicePayload::Opus(opus),
        MediaVoicePayload::Silence => AudioVoicePayload::Silence,
    }
}

fn audio_pop_logging_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_flag_enabled(AUDIO_POP_LOG_ENV))
}

fn env_flag_enabled(name: &str) -> bool {
    let Ok(value) = std::env::var(name) else {
        return false;
    };
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    let normalized = value.to_ascii_lowercase();
    !matches!(normalized.as_str(), "0" | "false" | "no" | "off")
}

fn log_audio_pop_media_packet(
    direction: &'static str,
    route: &'static str,
    stream_id: u32,
    sequence: u32,
    timestamp: u32,
    flags: u8,
    payload_size: usize,
    payload_kind: &'static str,
) {
    if !audio_pop_logging_enabled() || !audio_pop_should_log_packet(flags, payload_kind) {
        return;
    }
    kvlog::info!(
        "audio pop media packet",
        direction,
        route,
        stream_id,
        sequence,
        media_timestamp = timestamp,
        flags,
        flag_opus_reset = flags & AUDIO_POP_PACKET_FLAG_OPUS_RESET != 0,
        flag_silence_hint = flags & AUDIO_POP_PACKET_FLAG_SILENCE_HINT != 0,
        flag_silence_resume = flags & AUDIO_POP_PACKET_FLAG_SILENCE_RESUME != 0,
        flag_mute = flags & AUDIO_POP_PACKET_FLAG_MUTE != 0,
        payload_size,
        payload_kind
    );
}

fn audio_pop_should_log_packet(flags: u8, payload_kind: &str) -> bool {
    flags != 0 || payload_kind == "silence"
}

fn dispatch_voice_packet_to(
    events: &NetworkEventSender,
    playback_sink: Option<&LivePlaybackSink>,
    buffer_without_sink: bool,
    pending_playback_packets: &mut VecDeque<RemoteVoicePacket>,
    packet: RemoteVoicePacket,
) {
    let stream_id = packet.stream_id;
    let payload_size = packet.payload.len();
    let _ = events.send(NetworkEvent::VoicePacketObserved {
        stream_id,
        payload_size,
    });
    if let Some(sink) = playback_sink {
        while let Some(packet) = pending_playback_packets.pop_front() {
            sink.push(packet);
        }
        sink.push(packet);
    } else if buffer_without_sink {
        if pending_playback_packets.len() == MAX_PENDING_PLAYBACK_PACKETS {
            pending_playback_packets.pop_front();
        }
        pending_playback_packets.push_back(packet);
    }
}

fn random_u64() -> Result<u64, String> {
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let mut bytes = [0u8; 8];
    rng.fill(&mut bytes)
        .map_err(|_| "failed to generate random tie breaker".to_string())?;
    Ok(u64::from_le_bytes(bytes).max(1))
}

fn configured_nat_kind() -> P2pNatKind {
    match std::env::var("CHATT_P2P_NAT")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "cone" => P2pNatKind::Cone,
        "symmetric" => P2pNatKind::Symmetric,
        _ => P2pNatKind::Unknown,
    }
}

/// Applies the candidate privacy mode to a set of local candidates, producing
/// the literal-address set for the agent and the published set for the server.
/// Only host candidates are affected: `Mdns` replaces each host address with a
/// random `.local` name, `NoHost` drops host candidates, and `Disabled` keeps
/// literal addresses.
fn apply_candidate_privacy(
    candidates: Vec<Candidate>,
    mode: CandidatePrivacy,
    rng: &dyn SecureRandom,
) -> GatheredP2p {
    let mut local = Vec::with_capacity(candidates.len());
    let mut published = Vec::with_capacity(candidates.len());
    let mut mdns_names = HashMap::new();
    for candidate in candidates {
        if candidate.kind == CandidateKind::Host {
            match mode {
                CandidatePrivacy::NoHost => continue,
                CandidatePrivacy::Disabled => {}
                CandidatePrivacy::Mdns => {
                    if let Some(name) = generate_mdns_name(rng) {
                        let mut control = control_candidate(&candidate);
                        control.addr = format!("{}:{}", name, candidate.addr.port());
                        mdns_names.insert(name, candidate.addr.ip());
                        published.push(control);
                        local.push(candidate);
                        continue;
                    }
                    kvlog::warn!("mdns name generation failed; publishing literal host");
                }
            }
        }
        published.push(control_candidate(&candidate));
        local.push(candidate);
    }
    GatheredP2p {
        local,
        published,
        mdns_names,
    }
}

fn control_candidate(candidate: &Candidate) -> P2pCandidate {
    P2pCandidate {
        id: candidate.id,
        socket_id: candidate.socket_id,
        generation: candidate.generation,
        kind: control_candidate_kind(candidate.kind),
        addr: candidate.addr.to_string(),
        priority: candidate.priority,
        foundation: candidate.foundation.clone(),
        verified: candidate.verified,
    }
}

fn candidate_from_control(candidate: &P2pCandidate) -> Option<Candidate> {
    let addr = candidate.addr.parse().ok()?;
    Some(candidate_from_control_with_addr(candidate, addr))
}

/// Builds a [`Candidate`] from control metadata with an externally resolved
/// address, used when an mDNS `.local` candidate's address becomes known.
fn candidate_from_control_with_addr(candidate: &P2pCandidate, addr: SocketAddr) -> Candidate {
    let mut out = Candidate::new(
        candidate.id,
        candidate_kind_from_control(candidate.kind),
        addr,
    );
    out.socket_id = candidate.socket_id;
    out.generation = candidate.generation;
    out.priority = candidate.priority;
    out.foundation = candidate.foundation.clone();
    out.verified = candidate.verified;
    out
}

/// Splits a `{token}.local:{port}` candidate address into its lowercased host
/// name and port, returning `None` for any address that is not a valid single
/// label `.local` mDNS name.
fn split_mdns_addr(addr: &str) -> Option<(String, u16)> {
    let (host, port) = addr.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    if !rpc::control::is_valid_mdns_candidate_name(host) {
        return None;
    }
    Some((host.to_ascii_lowercase(), port))
}

fn control_candidate_kind(kind: CandidateKind) -> P2pCandidateKind {
    match kind {
        CandidateKind::Host => P2pCandidateKind::Host,
        CandidateKind::ServerReflexive => P2pCandidateKind::ServerReflexive,
        CandidateKind::PeerReflexive => P2pCandidateKind::PeerReflexive,
        CandidateKind::PortMapped => P2pCandidateKind::PortMapped,
        CandidateKind::Relay => P2pCandidateKind::Relay,
    }
}

fn candidate_kind_from_control(kind: P2pCandidateKind) -> CandidateKind {
    match kind {
        P2pCandidateKind::Host => CandidateKind::Host,
        P2pCandidateKind::ServerReflexive => CandidateKind::ServerReflexive,
        P2pCandidateKind::PeerReflexive => CandidateKind::PeerReflexive,
        P2pCandidateKind::PortMapped => CandidateKind::PortMapped,
        P2pCandidateKind::Relay => CandidateKind::Relay,
    }
}

fn nat_from_control(kind: P2pNatKind) -> NatKind {
    match kind {
        P2pNatKind::Unknown => NatKind::Unknown,
        P2pNatKind::Cone => NatKind::Cone,
        P2pNatKind::Symmetric => NatKind::Symmetric,
    }
}

fn control_nat_kind(kind: NatKind) -> P2pNatKind {
    match kind {
        NatKind::Unknown => P2pNatKind::Unknown,
        NatKind::Cone => P2pNatKind::Cone,
        NatKind::Symmetric => P2pNatKind::Symmetric,
    }
}

fn ice_role_from_control(role: P2pRole) -> IceRole {
    match role {
        P2pRole::Controlling => IceRole::Controlling,
        P2pRole::Controlled => IceRole::Controlled,
    }
}

fn key_from_control(key: &P2pKey) -> Result<KeyMaterial, String> {
    let bytes: [u8; KEY_LEN] = key
        .bytes
        .as_slice()
        .try_into()
        .map_err(|_| "invalid P2P key length".to_string())?;
    Ok(KeyMaterial { id: key.id, bytes })
}

/// Whether an incoming `P2pPeer` merely republishes the connection already
/// installed for the session: the server link and both sides' candidate
/// generations are unchanged, so the live agent — and its selected direct
/// path — must be kept rather than rebuilt.
fn p2p_peer_is_republish(
    installed: &PeerConnection,
    peer: &P2pPeerInfo,
    local_generation: u64,
) -> bool {
    installed.connection_id == peer.connection_id
        && installed.remote_generation == peer.generation
        && installed.local_generation == local_generation
}

fn p2p_username(connection_id: u64) -> String {
    format!("chatt-p2p:{connection_id}")
}

fn connection_id_from_p2p_username(username: &str) -> Option<u64> {
    username.strip_prefix("chatt-p2p:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatt_p2p::interfaces::LocalInterface;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn voice_udp_bind_applies_ef_qos() {
        let socket = voice::bind_voice_udp_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        assert_eq!(
            socket_int_option(socket.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TOS) & !0b11,
            (rpc::qos::VOICE_DSCP as i32) << 2
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            socket_int_option(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PRIORITY),
            6
        );
    }

    #[test]
    fn initial_udp_bind_send_failure_is_reported_without_becoming_session_error() {
        let (event_tx, event_rx) = mpsc::channel();
        let events = NetworkEventSender::for_test(event_tx);
        report_initial_udp_bind_send_error(
            &events,
            Some(io::Error::new(
                io::ErrorKind::WouldBlock,
                "test backpressure",
            )),
        );

        assert!(matches!(
            event_rx.recv().unwrap(),
            crate::app::AppEvent::Network(NetworkEvent::Error(message))
                if message.contains("test backpressure")
        ));
    }

    fn socket_int_option(fd: libc::c_int, level: libc::c_int, option: libc::c_int) -> i32 {
        let mut value = 0;
        let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                level,
                option,
                &mut value as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(result, 0, "{}", io::Error::last_os_error());
        value
    }

    fn user(id: u64) -> UserId {
        UserId(id)
    }

    fn interface_snapshot(addr: &str) -> InterfaceSnapshot {
        InterfaceSnapshot::from_interfaces(vec![LocalInterface {
            index: 1,
            name: "eth0".to_string(),
            addr: addr.parse().unwrap(),
            is_up: true,
            is_loopback: false,
            is_virtual: false,
        }])
        .unwrap()
    }

    #[test]
    fn inactive_interface_monitor_does_not_capture() {
        let now = Instant::now();
        let mut monitor = InterfaceMonitor::new(now);
        let mut captures = 0;

        assert_eq!(
            monitor
                .poll_with(false, now, || {
                    captures += 1;
                    Ok(interface_snapshot("192.168.1.2"))
                })
                .unwrap(),
            None
        );
        assert_eq!(captures, 0);
        assert!(monitor.snapshot().is_none());
    }

    #[test]
    fn candidate_publications_reuse_interface_baseline() {
        let now = Instant::now();
        let mut monitor = InterfaceMonitor::new(now);
        let mut captures = 0;

        monitor
            .ensure_with(now, || {
                captures += 1;
                Ok(interface_snapshot("192.168.1.2"))
            })
            .unwrap();
        monitor
            .ensure_with(now + INTERFACE_POLL_INTERVAL * 2, || {
                captures += 1;
                Ok(interface_snapshot("192.168.1.3"))
            })
            .unwrap();

        assert_eq!(captures, 1);
        assert_eq!(
            monitor.snapshot().unwrap().interfaces()[0].addr,
            "192.168.1.2".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn active_interface_monitor_polls_on_interval_and_detects_changes() {
        let now = Instant::now();
        let mut monitor = InterfaceMonitor::new(now);
        let mut captures = 0;

        assert_eq!(
            monitor
                .poll_with(true, now, || {
                    captures += 1;
                    Ok(interface_snapshot("192.168.1.2"))
                })
                .unwrap(),
            Some(false)
        );
        assert_eq!(
            monitor
                .poll_with(true, now + INTERFACE_POLL_INTERVAL / 2, || {
                    captures += 1;
                    Ok(interface_snapshot("192.168.1.3"))
                })
                .unwrap(),
            None
        );
        assert_eq!(
            monitor
                .poll_with(true, now + INTERFACE_POLL_INTERVAL, || {
                    captures += 1;
                    Ok(interface_snapshot("192.168.1.3"))
                })
                .unwrap(),
            Some(true)
        );
        assert_eq!(captures, 2);

        monitor
            .poll_with(false, now + INTERFACE_POLL_INTERVAL * 2, || {
                captures += 1;
                Ok(interface_snapshot("192.168.1.4"))
            })
            .unwrap();
        assert_eq!(captures, 2);
        assert!(monitor.snapshot().is_none());

        assert_eq!(
            monitor
                .poll_with(true, now + INTERFACE_POLL_INTERVAL * 2, || {
                    captures += 1;
                    Ok(interface_snapshot("192.168.1.4"))
                })
                .unwrap(),
            Some(false)
        );
        assert_eq!(captures, 3);
    }

    fn receiving(dir: &str, limit: u64) -> EffectiveFiles {
        EffectiveFiles {
            target: DownloadTarget::Persistent(PathBuf::from(dir)),
            max_download_bytes: limit,
        }
    }

    fn not_receiving(limit: u64) -> EffectiveFiles {
        EffectiveFiles {
            target: DownloadTarget::Off,
            max_download_bytes: limit,
        }
    }

    #[test]
    fn file_policy_for_room_falls_back_to_default() {
        let policy = FilePolicy {
            default: receiving("/dl", 100),
            rooms: vec![(RoomId(3), receiving("/room", 300))],
        };

        assert_eq!(policy.for_room(RoomId(3)), &policy.rooms[0].1);
        assert_eq!(policy.for_room(RoomId(9)), &policy.default);
    }

    #[test]
    fn file_policy_advertises_max_limit_across_receiving_rooms() {
        let policy = FilePolicy {
            default: receiving("/dl", 100),
            rooms: vec![
                (RoomId(3), receiving("/room", 300)),
                (RoomId(4), not_receiving(900)),
            ],
        };
        assert!(policy.receives_any());
        // Room 4 has downloads disabled, so its limit is not advertised.
        assert_eq!(policy.advertised_limit(), 300);

        let disabled = FilePolicy {
            default: not_receiving(100),
            rooms: Vec::new(),
        };
        assert!(!disabled.receives_any());
        assert_eq!(disabled.advertised_limit(), 0);
    }

    #[test]
    fn file_policy_receives_any_when_only_a_room_accepts() {
        let policy = FilePolicy {
            default: not_receiving(100),
            rooms: vec![(RoomId(3), receiving("/room", 300))],
        };
        assert!(policy.receives_any());
        assert_eq!(policy.advertised_limit(), 300);
    }

    #[test]
    fn upload_throttle_paces_and_caps_budget() {
        let now = Instant::now();
        let mut throttle = UploadThrottle::new(1000);
        // Starts with one second of budget.
        assert_eq!(throttle.budget(), 1000);
        throttle.consume(600);
        assert_eq!(throttle.budget(), 400);
        // Refill accrues rate * elapsed.
        throttle.last = now;
        throttle.refill(now + Duration::from_millis(500));
        assert_eq!(throttle.budget(), 900);
        // Accrual is capped at one second's worth so a long park cannot bank a
        // burst.
        throttle.refill(now + Duration::from_secs(10));
        assert_eq!(throttle.budget(), 1000);
    }

    #[test]
    fn upload_throttle_unlimited_bypasses() {
        let mut throttle = UploadThrottle::new(0);
        assert_eq!(throttle.budget(), u64::MAX);
        throttle.consume(1_000_000);
        assert_eq!(throttle.budget(), u64::MAX);
        assert_eq!(throttle.delay_until(1_000_000), Duration::ZERO);
    }

    #[test]
    fn upload_throttle_delay_waits_for_refill() {
        let mut throttle = UploadThrottle::new(1000);
        throttle.consume(1000);
        assert_eq!(throttle.budget(), 0);
        // 500 bytes at 1000 B/s is half a second.
        assert_eq!(throttle.delay_until(500), Duration::from_secs_f64(0.5));
        assert_eq!(throttle.delay_until(0), Duration::ZERO);
    }

    #[test]
    fn upload_throttle_set_rate_clamps_budget() {
        let mut throttle = UploadThrottle::new(1000);
        throttle.set_rate(200);
        // The budget cannot exceed the new, lower ceiling.
        assert_eq!(throttle.budget(), 200);
    }

    #[test]
    fn rtt_sample_matches_nonce_and_evicts_when_full() {
        let mut queue = VecDeque::new();
        let base = Instant::now();
        for nonce in 0..(RTT_IN_FLIGHT_CAP as u64 + 2) {
            push_rtt_in_flight(&mut queue, nonce, base);
        }
        // Capped, so the two oldest probes were dropped.
        assert_eq!(queue.len(), RTT_IN_FLIGHT_CAP);
        assert!(take_rtt_sample(&mut queue, 0, base).is_none());

        let later = base + Duration::from_millis(25);
        let sample = take_rtt_sample(&mut queue, 5, later).expect("nonce 5 outstanding");
        assert!((sample - 25.0).abs() < 1.0, "sample was {sample}");
        // The matched probe is consumed, so a duplicate Pong yields nothing.
        assert!(take_rtt_sample(&mut queue, 5, later).is_none());
    }

    #[test]
    fn rtt_ewma_seeds_then_smooths() {
        let seeded = fold_rtt_ewma(None, 100.0);
        assert_eq!(seeded, 100.0);
        // Moves toward the new sample by RTT_EWMA_WEIGHT, not all the way.
        let next = fold_rtt_ewma(Some(seeded), 200.0);
        assert!((next - (100.0 + RTT_EWMA_WEIGHT * 100.0)).abs() < f32::EPSILON);
        assert_eq!(clamp_rtt_ms(next), 120);
        assert_eq!(clamp_rtt_ms(f32::from(u16::MAX) + 1000.0), u16::MAX);
    }

    fn cand(id: u32, kind: CandidateKind, addr: &str) -> Candidate {
        Candidate::with_metadata(id, 1, 0, kind, addr.parse().unwrap(), None, true, true)
    }

    fn ctrl(addr: &str, kind: P2pCandidateKind) -> P2pCandidate {
        P2pCandidate {
            id: 1,
            socket_id: 1,
            generation: 0,
            kind,
            addr: addr.to_string(),
            priority: 1,
            foundation: "host-udp4".to_string(),
            verified: true,
        }
    }

    fn is_private_ip(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => v4.is_private(),
            IpAddr::V6(v6) => (v6.octets()[0] & 0xfe) == 0xfc,
        }
    }

    fn test_peer_connection(
        connection_id: u64,
        remote_generation: u64,
        local_generation: u64,
    ) -> PeerConnection {
        let agent = TraversalAgent::new(
            Instant::now(),
            P2pAgentConfig::with_auth(StunAuth::new([0u8; 32], [0u8; 32])),
            IceRole::Controlling,
            1,
            NatKind::Cone,
            NatKind::Cone,
            vec![cand(1, CandidateKind::Host, "10.0.0.2:5000")],
            vec![cand(2, CandidateKind::Host, "10.0.0.3:5001")],
        );
        PeerConnection {
            user_id: user(2),
            agent,
            send_key: KeyMaterial {
                id: 1,
                bytes: [0u8; KEY_LEN],
            },
            recv_key: KeyMaterial {
                id: 2,
                bytes: [0u8; KEY_LEN],
            },
            send_counter: 0,
            recv_replay: AntiReplay::new(),
            connection_id,
            remote_generation,
            local_generation,
            direct_stable_since: None,
            last_direct_inbound: None,
            rtt_in_flight: VecDeque::new(),
            rtt_ms: None,
        }
    }

    fn test_peer_info(connection_id: u64, generation: u64) -> P2pPeerInfo {
        let key = P2pKey {
            id: 1,
            bytes: vec![0u8; KEY_LEN],
        };
        P2pPeerInfo {
            room_id: RoomId(1),
            session_id: SessionId(9),
            user_id: user(2),
            generation,
            role: P2pRole::Controlled,
            nat: P2pNatKind::Cone,
            tie_breaker: 7,
            candidates: vec![ctrl("10.0.0.3:5001", P2pCandidateKind::Host)],
            send_key: key.clone(),
            recv_key: key.clone(),
            stun_key: key,
            connection_id,
        }
    }

    #[test]
    fn republished_p2p_peer_keeps_installed_agent() {
        let installed = test_peer_connection(14, 1, 1);
        assert!(p2p_peer_is_republish(&installed, &test_peer_info(14, 1), 1));
        // A peer restart bumps its generation and must rebuild the agent.
        assert!(!p2p_peer_is_republish(
            &installed,
            &test_peer_info(14, 2),
            1
        ));
        // A local restart bumps our own generation: the fresh local candidates
        // only reach the agent through a rebuild.
        assert!(!p2p_peer_is_republish(
            &installed,
            &test_peer_info(14, 1),
            2
        ));
        // A new server link means new keys, never a republish.
        assert!(!p2p_peer_is_republish(
            &installed,
            &test_peer_info(15, 1),
            1
        ));
    }

    #[test]
    fn mdns_mode_publishes_no_private_literal() {
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let candidates = vec![
            cand(1, CandidateKind::Host, "192.168.1.50:5000"),
            cand(2, CandidateKind::ServerReflexive, "198.51.100.7:6000"),
            cand(3, CandidateKind::Relay, "203.0.113.9:7000"),
        ];
        let gathered = apply_candidate_privacy(candidates, CandidatePrivacy::Mdns, &rng);

        assert_eq!(gathered.mdns_names.len(), 1);
        assert!(
            gathered
                .mdns_names
                .values()
                .any(|ip| *ip == "192.168.1.50".parse::<IpAddr>().unwrap())
        );
        for candidate in &gathered.published {
            if let Ok(addr) = candidate.addr.parse::<SocketAddr>() {
                assert!(!is_private_ip(addr.ip()), "leaked private literal {addr}");
            }
        }
        assert!(
            gathered
                .local
                .iter()
                .any(|candidate| candidate.addr.to_string() == "192.168.1.50:5000")
        );
    }

    #[test]
    fn no_host_mode_drops_host_candidates() {
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let candidates = vec![
            cand(1, CandidateKind::Host, "192.168.1.50:5000"),
            cand(2, CandidateKind::Relay, "203.0.113.9:7000"),
        ];
        let gathered = apply_candidate_privacy(candidates, CandidatePrivacy::NoHost, &rng);
        assert!(
            gathered
                .published
                .iter()
                .all(|candidate| candidate.kind != P2pCandidateKind::Host)
        );
        assert!(
            gathered
                .local
                .iter()
                .all(|candidate| candidate.kind != CandidateKind::Host)
        );
        assert!(gathered.mdns_names.is_empty());
    }

    #[test]
    fn disabled_mode_publishes_literal_host() {
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let candidates = vec![cand(1, CandidateKind::Host, "192.168.1.50:5000")];
        let gathered = apply_candidate_privacy(candidates, CandidatePrivacy::Disabled, &rng);
        assert_eq!(gathered.published[0].addr, "192.168.1.50:5000");
        assert!(gathered.mdns_names.is_empty());
    }

    #[test]
    fn mdns_remote_candidate_is_split_for_resolution() {
        // A `.local` candidate does not parse to a literal address and is routed
        // to mDNS resolution, while literal candidates are taken directly.
        assert!(
            candidate_from_control(&ctrl("abc123.local:5000", P2pCandidateKind::Host)).is_none()
        );
        assert_eq!(
            split_mdns_addr("abc123.local:5000"),
            Some(("abc123.local".to_string(), 5000))
        );
        assert_eq!(split_mdns_addr("203.0.113.1:5000"), None);
        assert!(
            candidate_from_control(&ctrl("203.0.113.1:5000", P2pCandidateKind::ServerReflexive))
                .is_some()
        );
    }

    #[test]
    fn relay_suppressed_only_when_all_others_direct_stable() {
        let now = Instant::now();
        let window = Duration::from_secs(3);
        let stable = now - Duration::from_secs(4);
        let recent = now - Duration::from_secs(1);
        let others: HashSet<UserId> = [user(2), user(3)].into_iter().collect();

        // Both peers stable past the window: relay can be dropped.
        assert!(relay_suppressed(
            now,
            window,
            &others,
            [(user(2), Some(stable)), (user(3), Some(stable))].into_iter(),
        ));

        // One peer only just became stable, still inside the window.
        assert!(!relay_suppressed(
            now,
            window,
            &others,
            [(user(2), Some(stable)), (user(3), Some(recent))].into_iter(),
        ));

        // One peer has no direct path at all.
        assert!(!relay_suppressed(
            now,
            window,
            &others,
            [(user(2), Some(stable)), (user(3), None)].into_iter(),
        ));

        // A newcomer with no peer entry must keep the relay alive.
        assert!(!relay_suppressed(
            now,
            window,
            &others,
            [(user(2), Some(stable))].into_iter(),
        ));
    }

    #[test]
    fn combined_relay_rtt_requires_both_links_and_saturates() {
        assert_eq!(combined_relay_rtt(Some(35.4), Some(40)), Some(75));
        assert_eq!(combined_relay_rtt(None, Some(40)), None);
        assert_eq!(combined_relay_rtt(Some(35.0), None), None);
        assert_eq!(
            combined_relay_rtt(Some(u16::MAX as f32), Some(1)),
            Some(u16::MAX)
        );
    }

    #[test]
    fn server_rtt_expires_after_three_probe_intervals() {
        let sample_at = Instant::now();
        assert!(!rtt_sample_is_stale(
            Some(sample_at),
            sample_at + RTT_STALE_AFTER - Duration::from_millis(1)
        ));
        assert!(rtt_sample_is_stale(
            Some(sample_at),
            sample_at + RTT_STALE_AFTER
        ));
        assert!(!rtt_sample_is_stale(None, sample_at + RTT_STALE_AFTER));
    }

    #[test]
    fn relay_not_suppressed_without_other_participants() {
        let now = Instant::now();
        let stable = now - Duration::from_secs(4);
        assert!(!relay_suppressed(
            now,
            Duration::from_secs(3),
            &HashSet::new(),
            [(user(2), Some(stable))].into_iter(),
        ));
    }

    #[test]
    fn direct_path_health_arms_clears_and_rearms() {
        let idle = Duration::from_millis(1500);
        let t0 = Instant::now();

        // Healthy: selected with a fresh inbound.
        assert!(direct_path_healthy(true, Some(t0), t0, idle));

        // No selected pair is never healthy.
        assert!(!direct_path_healthy(false, Some(t0), t0, idle));

        // Inbound went stale past the failover window.
        let later = t0 + Duration::from_millis(1600);
        assert!(!direct_path_healthy(true, Some(t0), later, idle));

        // Recovery: a new inbound at `later` is healthy again.
        assert!(direct_path_healthy(true, Some(later), later, idle));
    }

    #[test]
    fn reconnect_schedule_matches_retry_policy() {
        let mut schedule = ReconnectSchedule::new();
        let delays = (0..12)
            .map(|_| schedule.next_delay().as_secs())
            .collect::<Vec<_>>();

        assert_eq!(delays, vec![1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 5, 5]);
    }

    #[test]
    fn reconnect_schedule_resets_after_connected_session() {
        let mut schedule = ReconnectSchedule::new();
        for _ in 0..8 {
            schedule.next_delay();
        }

        schedule.reset();

        assert_eq!(schedule.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn embedded_server_lifecycle_wakes_reconnect_backoff() {
        let (tx, rx) = mpsc::channel();
        tx.send(NetworkCommand::RetryConnection).unwrap();

        assert!(matches!(
            wait_for_reconnect(&rx, Duration::from_secs(60)),
            RetryWait::Retry
        ));
    }

    #[test]
    fn explicit_shutdown_wins_simultaneous_disconnect() {
        assert!(matches!(
            select_session_end(
                false,
                None,
                None,
                Some("server closed connection".to_string()),
            ),
            SessionEnd::Disconnected(_)
        ));
        assert!(matches!(
            select_session_end(
                true,
                None,
                None,
                Some("server closed connection".to_string()),
            ),
            SessionEnd::Shutdown
        ));
    }

    #[test]
    fn dynamic_token_rejections_are_auth_failures() {
        assert!(is_auth_failure_code(ERROR_TOKEN_STALE_EPOCH));
        assert!(is_auth_failure_code(ERROR_PUBLIC_DISABLED));
    }

    #[test]
    fn username_taken_is_an_auth_failure() {
        assert!(is_auth_failure_code(ERROR_USERNAME_TAKEN));
    }

    #[test]
    fn command_drain_stops_at_iteration_limit() {
        let (tx, rx) = mpsc::channel();
        tx.send(NetworkCommand::Shutdown).unwrap();
        tx.send(NetworkCommand::Shutdown).unwrap();
        tx.send(NetworkCommand::Shutdown).unwrap();

        let mut handled = 0;
        let outcome = drain_commands_with(&rx, 2, |_| {
            handled += 1;
            Ok(())
        })
        .unwrap();

        assert_eq!(outcome, CommandDrainOutcome::HitLimit);
        assert_eq!(handled, 2);
        assert!(matches!(rx.try_recv(), Ok(NetworkCommand::Shutdown)));
    }

    #[test]
    fn interrupted_io_errors_are_retryable() {
        assert!(rpc::evented::is_interrupted_io_error(&io::Error::from(
            io::ErrorKind::Interrupted
        )));
        assert!(rpc::evented::is_interrupted_io_error(
            &io::Error::from_raw_os_error(libc::EINTR)
        ));
        assert!(!rpc::evented::is_interrupted_io_error(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
    }

    #[test]
    fn worker_work_queues_tcp_and_forces_zero_timeout() {
        let mut work = WorkerWork::default();
        assert_eq!(work.wake(), WakeIntent::Idle);

        work.queue_tcp_read();
        work.queue_tcp_read();
        work.queue_tcp_write();
        work.queue_tcp_write();
        assert_eq!(work.wake(), WakeIntent::Now);
        let tasks = work.take_tasks().collect::<Vec<_>>();
        assert_eq!(tasks, vec![WorkerTask::TcpRead, WorkerTask::TcpWrite]);
        assert_eq!(work.wake(), WakeIntent::Idle);
    }

    #[test]
    fn command_sender_routes_media_fast_path_away_from_main_receiver() {
        let (main_tx, main_rx) = mpsc::channel();
        let main_poll = Poll::new().unwrap();
        let main_waker = Arc::new(Waker::new(main_poll.registry(), WAKE).unwrap());
        let (voice_sender, voice_rx) = voice::input_for_test();
        let sender = CommandSender {
            tx: main_tx,
            waker: main_waker,
            voice: Some(voice_sender),
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        sender
            .send(NetworkCommand::SequencedLocalVoicePacket {
                sequence: 44,
                frame: LocalVoiceFrame {
                    timestamp: 44 * 960,
                    flags: 0,
                    payload: AudioVoicePayload::Opus(vec![1, 2, 3]),
                },
            })
            .unwrap();
        sender.send(NetworkCommand::JoinVoice(RoomId(7))).unwrap();

        assert!(matches!(
            main_rx.try_recv(),
            Ok(NetworkCommand::JoinVoice(RoomId(7)))
        ));
        assert!(main_rx.try_recv().is_err());
        assert_eq!(voice_rx.drain_microphone_sequences(), vec![Some(44)]);

        sender
            .alive
            .store(false, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            sender.send(NetworkCommand::LocalVoicePacket(LocalVoiceFrame {
                timestamp: 0,
                flags: 0,
                payload: AudioVoicePayload::Opus(vec![4]),
            })),
            Err(SendError(NetworkCommand::LocalVoicePacket(_)))
        ));
        assert!(matches!(
            sender.send(NetworkCommand::JoinVoice(RoomId(8))),
            Err(SendError(NetworkCommand::JoinVoice(RoomId(8))))
        ));
    }

    #[test]
    fn udp_recv_retries_interrupted_before_datagram_or_drain() {
        let src = "127.0.0.1:12345".parse::<SocketAddr>().unwrap();
        let mut calls = 0;
        let mut buf = [0u8; 8];

        let received = rpc::evented::recv_datagram_with(&mut buf, |_| {
            calls += 1;
            match calls {
                1 => Err(io::Error::from_raw_os_error(libc::EINTR)),
                2 => Ok((3, src)),
                _ => unreachable!("receive loop should return after one datagram"),
            }
        })
        .unwrap();

        assert_eq!(received, Some((3, src)));
        assert_eq!(calls, 2);

        let mut calls = 0;
        let drained: Option<(usize, SocketAddr)> =
            rpc::evented::recv_datagram_with(&mut buf, |_| {
                calls += 1;
                match calls {
                    1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                    2 => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                    _ => unreachable!("receive loop should stop at WouldBlock"),
                }
            })
            .unwrap();

        assert_eq!(drained, None);
        assert_eq!(calls, 2);
    }

    #[test]
    fn tcp_buffer_cap_keeps_readiness_for_retry() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"x").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = rpc::evented::read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::MaxBuffered(1),
        )
        .unwrap();

        assert!(outcome.hit_limit);
        assert!(!outcome.disconnected);
        assert!(outcome.bytes_read >= 1);
        assert!(readiness.is_ready());
        assert!(!read_buf.is_empty());
        assert!(tcp_receive_work_ready(readiness, &read_buf));
    }

    #[test]
    fn tcp_would_block_clears_retained_readiness_after_buffer_consumed() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        writer.write_all(b"x").expect("write payload");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = rpc::evented::read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::MaxBuffered(1),
        )
        .unwrap();
        assert!(outcome.bytes_read >= 1);
        assert!(readiness.is_ready());

        read_buf.consume(read_buf.len());
        let outcome = rpc::evented::read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            1,
            ReadLimit::MaxBuffered(1),
        )
        .unwrap();

        assert_eq!(outcome, ReadPumpOutcome::default());
        assert!(!readiness.is_ready());
        assert!(!tcp_receive_work_ready(readiness, &read_buf));
    }

    #[test]
    fn complete_buffered_frame_is_immediate_tcp_work() {
        let (mut writer, reader) = UnixStream::pair().expect("socket pair");
        reader.set_nonblocking(true).expect("set nonblocking");
        let mut encoded = Vec::new();
        frame::encode_frame(b"payload", &mut encoded).expect("encode frame");
        writer.write_all(&encoded).expect("write frame");
        let mut read_buf = RecvBuffer::new();
        let mut readiness = Readiness::primed();

        let outcome = rpc::evented::read_into_buffer(
            &reader,
            &mut read_buf,
            &mut readiness,
            encoded.len(),
            ReadLimit::MaxBuffered(encoded.len() + 1),
        )
        .unwrap();
        assert_eq!(outcome.bytes_read, encoded.len());
        assert!(!readiness.is_ready());

        assert!(tcp_receive_work_ready(Readiness::new(), &read_buf));
    }

    #[test]
    fn voice_dispatch_buffers_audio_until_sink_attaches() {
        let (tx, rx) = mpsc::channel();
        let events = NetworkEventSender::for_test(tx);
        let mut pending = VecDeque::new();
        dispatch_voice_packet_to(
            &events,
            None,
            true,
            &mut pending,
            test_remote_voice_packet(7, 3, vec![1, 2, 3, 4]),
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(crate::app::AppEvent::Network(
                NetworkEvent::VoicePacketObserved {
                    stream_id: 7,
                    payload_size: 4,
                }
            ))
        ));
        assert!(rx.try_recv().is_err());
        assert_eq!(pending.len(), 1);
        let packet = pending.pop_front().unwrap();
        assert_eq!(packet.stream_id, 7);
        assert_eq!(packet.sequence, 3);
        assert_eq!(packet.payload, AudioVoicePayload::Opus(vec![1, 2, 3, 4]));
    }

    #[test]
    fn voice_dispatch_uses_sink_when_attached() {
        let (tx, rx) = mpsc::channel();
        let events = NetworkEventSender::for_test(tx);
        let mut pending = VecDeque::new();
        let sink = LivePlaybackSink::for_test();
        dispatch_voice_packet_to(
            &events,
            Some(&sink),
            true,
            &mut pending,
            test_remote_voice_packet(9, 4, vec![5, 6, 7]),
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(crate::app::AppEvent::Network(
                NetworkEvent::VoicePacketObserved {
                    stream_id: 9,
                    payload_size: 3,
                }
            ))
        ));
        assert!(rx.try_recv().is_err());
        assert!(pending.is_empty());
    }

    #[test]
    fn recent_voice_sequences_tracks_duplicates_out_of_order_and_stale_packets() {
        let mut recent = RecentVoiceSequences::default();

        assert_eq!(recent.observe(10), RecentVoiceSequenceResult::New);
        assert_eq!(recent.observe(12), RecentVoiceSequenceResult::New);
        assert_eq!(recent.observe(11), RecentVoiceSequenceResult::New);
        assert_eq!(recent.observe(11), RecentVoiceSequenceResult::Duplicate);
        assert_eq!(recent.observe(12), RecentVoiceSequenceResult::Duplicate);

        for sequence in 13..(13 + MAX_RECENT_VOICE_SEQUENCES as u32) {
            assert_eq!(recent.observe(sequence), RecentVoiceSequenceResult::New);
        }

        assert!(
            matches!(recent.observe(10), RecentVoiceSequenceResult::Stale),
            "packets outside the fixed dedup window should be dropped as stale"
        );
    }

    #[test]
    fn local_voice_sequence_advances_for_silence_markers() {
        let mut local_sequence = 10;

        let silence_sequence = allocate_local_voice_sequence(&mut local_sequence);
        let opus_sequence = allocate_local_voice_sequence(&mut local_sequence);

        assert_eq!(silence_sequence, 10);
        assert_eq!(opus_sequence, 11);
        assert_eq!(local_sequence, 12);

        advance_local_voice_sequence_past(&mut local_sequence, 20);
        assert_eq!(local_sequence, 21);
    }

    #[test]
    fn encoder_feedback_controller_escalates_without_bitrate_policy() {
        let start = Instant::now();
        let mut controller = EncoderFeedbackController::new();

        let profile = controller
            .observe(feedback_window(100, 60, 0, 80, 120), start)
            .unwrap();

        assert_eq!(profile, LiveEncoderProfile::DRED_60);
        assert_eq!(profile.packet_loss_percent, 60);
    }

    #[test]
    fn encoder_feedback_controller_holds_and_decays_one_level() {
        let start = Instant::now();
        let mut controller = EncoderFeedbackController::new();
        assert_eq!(
            controller.observe(feedback_window(100, 60, 0, 80, 120), start),
            Some(LiveEncoderProfile::DRED_60)
        );

        assert_eq!(
            controller.observe(
                feedback_window(100, 0, 0, 20, 20),
                start + ENCODER_PROFILE_HOLD - Duration::from_millis(1)
            ),
            None
        );
        assert_eq!(
            controller.observe(
                feedback_window(100, 0, 0, 20, 20),
                start + ENCODER_PROFILE_HOLD + Duration::from_millis(1)
            ),
            Some(LiveEncoderProfile::DRED_50)
        );
    }

    fn encode_test_stream(data: &[u8], source_chunk: usize, wire_chunk: usize) -> Vec<u8> {
        let encoder =
            file_compression::new_encoder(PendingWire::default(), data.len() as u64).unwrap();
        let mut body = UploadBody::Zstd(encoder);
        let mut encoded = Vec::new();
        for chunk in data.chunks(source_chunk) {
            body.feed(chunk).unwrap();
            while !body.pending().is_empty() {
                encoded.extend(body.pending_mut().take(wire_chunk));
            }
        }
        body.finish().unwrap();
        while !body.pending().is_empty() {
            encoded.extend(body.pending_mut().take(wire_chunk));
        }
        encoded
    }

    fn incoming_test_file(
        path: &Path,
        original_size: u64,
        encoding: FileContentEncoding,
        image: bool,
    ) -> IncomingFile {
        let file_name = if image { "image.png" } else { "data.bin" };
        let store = crate::receive_store::DownloadStore::new(64 * 1024 * 1024);
        let reservation = store.reserve_disk_name(file_name.to_string()).unwrap();
        let sink = ReceiveSink::new(
            SinkTarget::Disk(File::create(path).unwrap()),
            original_size,
            image,
        );
        let body = match encoding {
            FileContentEncoding::Identity | FileContentEncoding::Sealed => {
                IncomingBody::Identity(sink)
            }
            FileContentEncoding::Zstd => {
                let mut decoder = zstd::stream::raw::Decoder::new().unwrap();
                decoder
                    .set_parameter(zstd::stream::raw::DParameter::WindowLogMax(ZSTD_WINDOW_LOG))
                    .unwrap();
                IncomingBody::Zstd(zstd::stream::zio::Writer::new(sink, decoder))
            }
        };
        IncomingFile {
            seal: None,
            metadata: FileMetadata {
                mls_event_id: None,
                transfer_id: FileTransferId(1),
                room_id: RoomId(1),
                sender: UserId(1),
                sender_name: "sender".to_string(),
                file_name: file_name.to_string(),
                original_name: "data.bin".to_string(),
                size: original_size,
                encoding,
                timestamp_ms: 1,
            },
            dest: ReceiveDest::Disk {
                path: path.to_path_buf(),
                reservation,
            },
            body,
            pending_wire: Vec::new(),
            pending_wire_offset: 0,
            wire_received: 0,
            complete_received: false,
            decoder_finished: false,
            next_status_at: FILE_PROGRESS_STEP_BYTES,
            reservation: None,
        }
    }

    fn pump_test_input(
        incoming: &mut IncomingFile,
        encoded: &[u8],
        wire_chunk: usize,
        decoded_budget: usize,
    ) -> io::Result<Vec<u64>> {
        let mut decoded_deltas = Vec::new();
        for chunk in encoded.chunks(wire_chunk) {
            incoming.pending_wire.extend_from_slice(chunk);
            incoming.wire_received += chunk.len() as u64;
            while incoming.pending_wire_offset < incoming.pending_wire.len() {
                let before_decoded = incoming.body.sink().decoded;
                let before_wire = incoming.pending_wire.len() - incoming.pending_wire_offset;
                let mut wire_budget = usize::MAX;
                let mut budget = decoded_budget;
                incoming.pump(&mut wire_budget, &mut budget)?;
                decoded_deltas.push(incoming.body.sink().decoded - before_decoded);
                let after_wire = incoming.pending_wire.len() - incoming.pending_wire_offset;
                assert!(
                    after_wire < before_wire || incoming.body.sink().decoded > before_decoded,
                    "decoder made no progress"
                );
            }
        }
        incoming.complete_received = true;
        for _ in 0..10_000 {
            if incoming.ready_to_finalize() {
                return Ok(decoded_deltas);
            }
            let before_decoded = incoming.body.sink().decoded;
            let mut wire_budget = usize::MAX;
            let mut budget = decoded_budget;
            incoming.pump(&mut wire_budget, &mut budget)?;
            decoded_deltas.push(incoming.body.sink().decoded - before_decoded);
        }
        panic!("decoder did not finish");
    }

    #[test]
    fn zstd_stream_round_trips_across_source_and_wire_boundaries() {
        for data in [
            b"small source".repeat(4_000),
            b"many zstd windows\n".repeat(180_000),
        ] {
            for source_chunk in [1, 97, 64 * 1024] {
                let encoded = encode_test_stream(&data, source_chunk, 211);
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("received.bin");
                let mut incoming =
                    incoming_test_file(&path, data.len() as u64, FileContentEncoding::Zstd, false);
                pump_test_input(&mut incoming, &encoded, 313, 64 * 1024).unwrap();
                let (_, location, _, _, _) = incoming.finalize().unwrap();
                let FinalizedLocation::Disk { path, served_name } = location else {
                    panic!("disk transfer should finalize to a disk path");
                };
                assert_eq!(served_name, "data.bin");
                assert_eq!(fs::read(path).unwrap(), data);
            }
        }
    }

    #[test]
    fn zstd_finish_rejects_truncation_and_checksum_corruption() {
        let data = b"checksum-protected contents".repeat(10_000);
        let encoded = encode_test_stream(&data, 4096, 1024);

        for broken in [encoded[..encoded.len() - 3].to_vec(), {
            let mut corrupted = encoded.clone();
            let index = corrupted.len() - 2;
            corrupted[index] ^= 0x80;
            corrupted
        }] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("broken.bin");
            let mut incoming =
                incoming_test_file(&path, data.len() as u64, FileContentEncoding::Zstd, false);
            assert!(pump_test_input(&mut incoming, &broken, 127, 32 * 1024).is_err());
        }
    }

    #[test]
    fn decoded_size_limit_stops_destination_growth() {
        let data = vec![b'x'; 512 * 1024];
        let encoded = encode_test_stream(&data, 8192, 4096);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("limited.bin");
        let declared = 64 * 1024;
        let mut incoming = incoming_test_file(&path, declared, FileContentEncoding::Zstd, false);

        assert!(pump_test_input(&mut incoming, &encoded, 1024, 16 * 1024).is_err());
        assert!(fs::metadata(path).unwrap().len() <= declared);
    }

    #[test]
    fn decoder_rejects_frames_requiring_a_larger_window() {
        let data = b"large-window-pattern".repeat(180_000);
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        encoder
            .set_pledged_src_size(Some(data.len() as u64))
            .unwrap();
        encoder.window_log(ZSTD_WINDOW_LOG + 1).unwrap();
        encoder.write_all(&data).unwrap();
        let encoded = encoder.finish().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window.bin");
        let mut incoming =
            incoming_test_file(&path, data.len() as u64, FileContentEncoding::Zstd, false);

        assert!(pump_test_input(&mut incoming, &encoded, 4096, 64 * 1024).is_err());
    }

    #[test]
    fn receive_sink_preserves_image_prefix_for_identity_and_zstd() {
        let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&320u32.to_be_bytes());
        png.extend_from_slice(&180u32.to_be_bytes());
        png.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.resize(32 * 1024, 0);

        for encoding in [FileContentEncoding::Identity, FileContentEncoding::Zstd] {
            let wire = match encoding {
                FileContentEncoding::Identity | FileContentEncoding::Sealed => png.clone(),
                FileContentEncoding::Zstd => encode_test_stream(&png, 777, 333),
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("image.png");
            let mut incoming = incoming_test_file(&path, png.len() as u64, encoding, true);
            pump_test_input(&mut incoming, &wire, 191, 4096).unwrap();
            let (_, _, _, dimensions, _) = incoming.finalize().unwrap();
            assert_eq!(dimensions, Some((320, 180)));
        }
    }

    #[test]
    fn decompression_respects_per_pump_work_budget() {
        let data = vec![b'a'; 4 * 1024 * 1024];
        let encoded = encode_test_stream(&data, 64 * 1024, 64 * 1024);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded.bin");
        let mut incoming =
            incoming_test_file(&path, data.len() as u64, FileContentEncoding::Zstd, false);
        let budget = 32 * 1024;
        let deltas = pump_test_input(&mut incoming, &encoded, encoded.len(), budget).unwrap();
        assert!(deltas.iter().all(|delta| *delta <= budget as u64));
        assert!(deltas.iter().filter(|delta| **delta != 0).count() > 1);
    }

    #[test]
    fn incoming_pump_respects_encoded_input_budget() {
        let data = vec![b'a'; 128 * 1024];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire-bounded.bin");
        let mut incoming = incoming_test_file(
            &path,
            data.len() as u64,
            FileContentEncoding::Identity,
            false,
        );
        incoming.pending_wire = data;
        incoming.wire_received = incoming.pending_wire.len() as u64;
        let mut wire_budget = 4096;
        let mut decoded_budget = usize::MAX;

        incoming
            .pump(&mut wire_budget, &mut decoded_budget)
            .unwrap();

        assert_eq!(wire_budget, 0);
        assert_eq!(incoming.body.sink().decoded, 4096);
        assert_eq!(incoming.pending_wire_offset, 4096);
    }

    #[test]
    fn sealed_upload_waits_for_canonical_mls_announcement() {
        let source = tempfile::tempfile().unwrap();
        let event_id = EventId([7; 16]);
        let mut upload = OutgoingUpload {
            transfer_id: FileTransferId(1),
            server_metadata: None,
            room_id: RoomId(1),
            name: "sealed.bin".to_string(),
            size: 0,
            file: source,
            source_path: PathBuf::new(),
            delete_source_when_done: false,
            source_offset: 0,
            source_read_ahead: 0,
            wire_offset: 0,
            source_prefix: Vec::new(),
            source_prefix_offset: 0,
            body: UploadBody::Identity(PendingWire::default()),
            source_finished: true,
            encoder_finished: false,
            started: false,
            next_status_at: 0,
            local_copy: None,
            dimensions: None,
            image_prefix: Vec::new(),
            seal: Some(UploadSeal {
                content_key: KeyMaterial {
                    id: 1,
                    bytes: [3; KEY_LEN],
                },
                event_id,
                announcement_delivered: false,
                digest: [4; 32],
                inner_encoding: FileContentEncoding::Identity,
                counter: 0,
                stream_len: 0,
                pad_remaining: None,
                mls_event_id: event_id,
            }),
        };
        let throttle = UploadThrottle::new(0);

        assert!(!upload_ready_now(&upload, 0, &throttle));
        upload.seal.as_mut().unwrap().announcement_delivered = true;
        assert!(upload_ready_now(&upload, 0, &throttle));
    }

    #[test]
    fn retained_probe_prefix_is_read_exactly_once() {
        let data = b"prefix and remaining source bytes".repeat(10_000);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        fs::write(&path, &data).unwrap();
        let mut file = File::open(&path).unwrap();
        let prefix_len = 4096;
        let mut prefix = vec![0; prefix_len];
        file.read_exact(&mut prefix).unwrap();
        let mut upload = OutgoingUpload {
            seal: None,
            transfer_id: FileTransferId(1),
            server_metadata: None,
            room_id: RoomId(1),
            name: "source.bin".to_string(),
            size: data.len() as u64,
            file,
            source_path: PathBuf::new(),
            delete_source_when_done: false,
            source_offset: 0,
            source_read_ahead: 0,
            wire_offset: 0,
            source_prefix: prefix,
            source_prefix_offset: 0,
            body: UploadBody::Identity(PendingWire::default()),
            source_finished: false,
            encoder_finished: false,
            started: false,
            next_status_at: FILE_PROGRESS_STEP_BYTES,
            local_copy: None,
            dimensions: None,
            image_prefix: Vec::new(),
        };
        let mut read_back = Vec::new();
        while read_back.len() < data.len() {
            let chunk = read_upload_source(&mut upload, 997).unwrap();
            assert!(!chunk.is_empty());
            read_back.extend_from_slice(&chunk);
        }
        assert_eq!(read_back, data);
    }

    #[test]
    fn compressed_upload_local_copy_stays_uncompressed() {
        let raw = b"local raw upload bytes".repeat(10_000);
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.bin");
        let local_path = dir.path().join("local.bin");
        fs::write(&source_path, &raw).unwrap();
        let mut upload = OutgoingUpload {
            seal: None,
            transfer_id: FileTransferId(1),
            server_metadata: None,
            room_id: RoomId(1),
            name: "source.bin".to_string(),
            size: raw.len() as u64,
            file: File::open(source_path).unwrap(),
            source_path: PathBuf::new(),
            delete_source_when_done: false,
            source_offset: 0,
            source_read_ahead: 0,
            wire_offset: 0,
            source_prefix: Vec::new(),
            source_prefix_offset: 0,
            body: UploadBody::Zstd(
                file_compression::new_encoder(PendingWire::default(), raw.len() as u64).unwrap(),
            ),
            source_finished: false,
            encoder_finished: false,
            started: true,
            next_status_at: FILE_PROGRESS_STEP_BYTES,
            local_copy: Some(UploadLocalCopy::Disk {
                path: local_path.clone(),
                file: File::create(&local_path).unwrap(),
                reservation: crate::receive_store::DownloadStore::new(64 * 1024 * 1024)
                    .reserve_disk_name("local.bin".to_string())
                    .unwrap(),
            }),
            dimensions: None,
            image_prefix: Vec::new(),
        };
        let (tx, _rx) = std::sync::mpsc::channel();
        let events = NetworkEventSender::for_test(tx);

        write_upload_local_copy(&events, &mut upload, &raw);
        upload.body.feed(&raw).unwrap();
        upload.body.finish().unwrap();
        let Some(UploadLocalCopy::Disk { file, .. }) = upload.local_copy.as_mut() else {
            panic!("expected an on-disk local copy");
        };
        file.flush().unwrap();

        assert_eq!(fs::read(local_path).unwrap(), raw);
        assert!(upload.body.pending().len() < raw.len());
    }

    #[test]
    fn throttled_zstd_upload_caps_source_read_ahead() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.bin");
        fs::write(&source_path, b"source").unwrap();
        let mut upload = OutgoingUpload {
            seal: None,
            transfer_id: FileTransferId(1),
            server_metadata: None,
            room_id: RoomId(1),
            name: "source.bin".to_string(),
            size: 16 * 1024 * 1024,
            file: File::open(source_path).unwrap(),
            source_path: PathBuf::new(),
            delete_source_when_done: false,
            source_offset: 0,
            source_read_ahead: MAX_COMPRESSED_UPLOAD_SOURCE_AHEAD_BYTES - 1,
            wire_offset: 0,
            source_prefix: Vec::new(),
            source_prefix_offset: 0,
            body: UploadBody::Zstd(
                file_compression::new_encoder(PendingWire::default(), 16 * 1024 * 1024).unwrap(),
            ),
            source_finished: false,
            encoder_finished: false,
            started: true,
            next_status_at: FILE_PROGRESS_STEP_BYTES,
            local_copy: None,
            dimensions: None,
            image_prefix: Vec::new(),
        };
        let limited = UploadThrottle::new(1024);

        assert_eq!(upload_source_read_capacity(&upload, &limited), 1);
        assert!(!upload_should_flush_source_read_ahead(&upload, &limited));

        upload.source_read_ahead = MAX_COMPRESSED_UPLOAD_SOURCE_AHEAD_BYTES;
        assert_eq!(upload_source_read_capacity(&upload, &limited), 0);
        assert!(upload_should_flush_source_read_ahead(&upload, &limited));

        let unlimited = UploadThrottle::new(0);
        assert_eq!(upload_source_read_capacity(&upload, &unlimited), u64::MAX);
        assert!(!upload_should_flush_source_read_ahead(&upload, &unlimited));

        upload.body = UploadBody::Identity(PendingWire::default());
        assert_eq!(upload_source_read_capacity(&upload, &limited), u64::MAX);
        assert!(!upload_should_flush_source_read_ahead(&upload, &limited));
    }

    #[test]
    fn receive_file_path_preserves_extension_when_colliding() {
        let dir = std::env::temp_dir().join(format!(
            "chatt-client-net-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("report.pdf"), b"existing").unwrap();

        let store = crate::receive_store::DownloadStore::new(64 * 1024 * 1024);
        let receive = create_receive_file(&store, &dir, "report.pdf").unwrap();

        assert_eq!(
            receive.path.file_name().and_then(|name| name.to_str()),
            Some("report-1.pdf")
        );
        assert!(store.resolve("report-1.pdf").is_none());
        drop(receive);
        assert!(store.name_available("report-1.pdf"));
        let _ = fs::remove_dir_all(dir);
    }

    fn accepted_file_metadata() -> FileMetadata {
        FileMetadata {
            mls_event_id: None,
            transfer_id: FileTransferId(20),
            room_id: RoomId(1),
            sender: UserId(3),
            sender_name: "alice".to_string(),
            file_name: "report.pdf".to_string(),
            original_name: "report.pdf".to_string(),
            size: 10,
            encoding: FileContentEncoding::Identity,
            timestamp_ms: 500,
        }
    }

    #[test]
    fn upload_acceptance_correlates_before_or_after_local_completion() {
        let path =
            std::env::temp_dir().join(format!("chatt-upload-correlation-{}", std::process::id()));
        let file = File::create(&path).expect("create");
        let client_id = FileTransferId(7);
        let mut outgoing = VecDeque::from([OutgoingUpload {
            seal: None,
            transfer_id: client_id,
            server_metadata: None,
            room_id: RoomId(1),
            name: "report.pdf".to_string(),
            size: 10,
            file,
            source_path: PathBuf::new(),
            delete_source_when_done: false,
            source_offset: 0,
            source_read_ahead: 0,
            wire_offset: 0,
            source_prefix: Vec::new(),
            source_prefix_offset: 0,
            body: UploadBody::Identity(PendingWire::default()),
            source_finished: false,
            encoder_finished: false,
            started: true,
            next_status_at: 10,
            local_copy: None,
            dimensions: None,
            image_prefix: Vec::new(),
        }]);
        let mut pending = HashMap::new();

        assert!(
            correlate_upload_accepted(
                &mut outgoing,
                &mut pending,
                client_id,
                accepted_file_metadata()
            )
            .is_none()
        );
        assert_eq!(
            outgoing[0]
                .server_metadata
                .as_ref()
                .map(|metadata| (metadata.transfer_id, metadata.timestamp_ms)),
            Some((FileTransferId(20), 500))
        );

        outgoing.clear();
        pending.insert(
            client_id,
            PendingLocalFile {
                sealed_real: None,
                location: LocalFileLocation::Disk("report.pdf".to_string()),
                dimensions: Some((4, 3)),
            },
        );
        let (metadata, local) = correlate_upload_accepted(
            &mut outgoing,
            &mut pending,
            client_id,
            accepted_file_metadata(),
        )
        .expect("completion waiting for acceptance");
        assert_eq!(
            (metadata.transfer_id, metadata.timestamp_ms),
            (FileTransferId(20), 500)
        );
        assert_eq!(local.dimensions, Some((4, 3)));
        assert!(!pending.contains_key(&client_id));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sanitize_file_name_removes_path_components() {
        assert_eq!(sanitize_file_name("../unsafe/report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("bad/name?.txt"), "name_.txt");
    }

    #[test]
    fn upload_username_prefers_override() {
        let path = PathBuf::from("/tmp/staged-abc.png");
        assert_eq!(
            upload_username(Some("holiday.png".to_string()), &path).unwrap(),
            "holiday.png"
        );
        assert_eq!(upload_username(None, &path).unwrap(), "staged-abc.png");
    }

    #[test]
    fn upload_username_rejects_overlong_name() {
        let path = PathBuf::from("/tmp/x.png");
        let long = "a".repeat(MAX_FILE_NAME_BYTES + 1);
        assert!(upload_username(Some(long), &path).is_err());
    }

    #[test]
    fn open_upload_source_deletes_staged_file_but_keeps_handle() {
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staged.png");
        fs::write(&path, b"staged bytes").unwrap();

        let mut file = open_upload_source(&path, true).unwrap();
        assert!(!path.exists());
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"staged bytes");
    }

    #[test]
    fn open_upload_source_keeps_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keep.png");
        fs::write(&path, b"bytes").unwrap();

        let _file = open_upload_source(&path, false).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn temporarily_blocked_mls_requests_retry_only_after_exponential_backoff() {
        let room_id = RoomId(17);
        let request = ClientControl::FetchGroupInfo { room_id };
        let key = MlsRetryKey::Commit(room_id);
        let start = Instant::now();
        let mut retries = HashMap::new();

        assert_eq!(
            defer_mls_retry_in(&mut retries, key, request.clone(), start),
            MLS_RETRY_INITIAL_BACKOFF
        );
        assert!(take_ready_mls_retries(&mut retries, start).is_empty());
        assert!(
            take_ready_mls_retries(
                &mut retries,
                start + MLS_RETRY_INITIAL_BACKOFF - Duration::from_millis(1)
            )
            .is_empty()
        );
        assert_eq!(
            take_ready_mls_retries(&mut retries, start + MLS_RETRY_INITIAL_BACKOFF),
            vec![request.clone()]
        );
        assert!(take_ready_mls_retries(&mut retries, start + MLS_RETRY_INITIAL_BACKOFF).is_empty());

        let second_start = start + MLS_RETRY_INITIAL_BACKOFF;
        assert_eq!(
            defer_mls_retry_in(&mut retries, key, request, second_start),
            MLS_RETRY_INITIAL_BACKOFF * 2
        );
        assert_eq!(mls_retry_backoff(32), MLS_RETRY_MAX_BACKOFF);
    }

    #[test]
    fn mls_message_ids_follow_delivery_sequence() {
        let first = mls_message_id(2);
        let second = mls_message_id(3);
        assert!(first < second);
        assert_eq!(first.0, (1 << 63) | 2);
        assert_eq!(second.0, (1 << 63) | 3);
    }

    fn feedback_window(
        expected_packets: u16,
        lost_packets: u16,
        late_packets: u16,
        max_output_ring_ms: u16,
        max_interarrival_jitter_ms: u16,
    ) -> LivePlaybackFeedback {
        LivePlaybackFeedback {
            stream_id: 1,
            highest_contiguous_sequence: 10,
            expected_packets,
            lost_packets,
            late_packets,
            duplicate_packets: 0,
            reordered_packets: 0,
            window_ms: 500,
            max_output_ring_ms,
            max_neteq_target_ms: max_output_ring_ms,
            max_neteq_playout_delay_ms: max_output_ring_ms,
            max_neteq_packet_buffer_ms: 0,
            max_interarrival_jitter_ms,
        }
    }

    fn test_remote_voice_packet(
        stream_id: u32,
        sequence: u32,
        payload: Vec<u8>,
    ) -> RemoteVoicePacket {
        RemoteVoicePacket {
            stream_id,
            sequence,
            timestamp: sequence.wrapping_mul(crate::audio::FRAME_SAMPLES as u32 * 2),
            flags: 0,
            payload: AudioVoicePayload::Opus(payload),
            received_at: Instant::now(),
        }
    }
}
