use jsony::Jsony;

use crate::ids::{
    BugReportId, DeviceId, FileTransferId, LedgerHash, MessageId, RoomId, SessionId, StreamId,
    UserId,
};

pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 224 * 1024;
pub const MAX_CHAT_BODY_BYTES: usize = 8 * 1024;
pub const DEFAULT_FILE_SIZE_LIMIT_BYTES: u64 = 50 * 1024 * 1024; // default file size limit when unconfigured
/// Payload bytes carried by one `UploadFileChunk`/`BugReportChunk`. Sized so the
/// encoded control message plus transport header, AEAD tag, and length prefix
/// stays under [`crate::frame::MAX_FRAME_LEN`], keeping a 50 MB transfer to a few
/// hundred frames rather than thousands.
pub const MAX_FILE_CHUNK_BYTES: usize = 192 * 1024;
pub const MAX_FILE_NAME_BYTES: usize = 255;
pub const MAX_BUG_REPORT_DESC_BYTES: usize = 4 * 1024;
pub const MAX_BUG_REPORT_METADATA_BYTES: usize = 32 * 1024;
pub const MAX_BUG_REPORT_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_AUTH_FIELD_BYTES: usize = 512;
pub const MAX_VIDEO_CODEC_BYTES: usize = 64;
pub const MAX_VIDEO_EXTRADATA_BYTES: usize = 4 * 1024;
pub const MIN_PAIRING_SECRET_BYTES: usize = 16;
pub const MIN_PAIRED_TOKEN_BYTES: usize = 32;
pub const ERROR_AUTH_REJECTED: u16 = 401;
pub const ERROR_PAIRING_NOT_ACTIVE: u16 = 410;
pub const ERROR_PAIRING_CODE_MISMATCH: u16 = 409;
pub const ERROR_PAIRING_INVALID_REQUEST: u16 = 422;
/// The requested username is already registered to another user on this server.
/// The client should re-prompt for a different username.
pub const ERROR_USERNAME_TAKEN: u16 = 455;
/// Open pairing was attempted against a server that does not allow it.
pub const ERROR_PUBLIC_DISABLED: u16 = 403;
/// Open pairing requires a password and none was supplied.
pub const ERROR_PASSWORD_REQUIRED: u16 = 428;
/// Open pairing supplied the wrong password.
pub const ERROR_PASSWORD_MISMATCH: u16 = 423;
/// A dynamic token's password epoch is older than the server's current epoch;
/// the client should re-pair with the current password to refresh it.
pub const ERROR_TOKEN_STALE_EPOCH: u16 = 426;
/// Open pairing is being attempted too frequently.
pub const ERROR_OPEN_PAIR_RATE_LIMITED: u16 = 429;
pub const ERROR_BUG_REPORT_REJECTED: u16 = 400;
/// The room does not exist for the requesting user. Deliberately also the
/// answer for rooms the user cannot access, so private rooms are
/// indistinguishable from missing ones.
pub const ERROR_ROOM_NOT_FOUND: u16 = 404;
/// A room verb was rejected by current server state (not in the voice call,
/// share already active, peer gone); the connection stays usable.
pub const ERROR_REQUEST_REJECTED: u16 = 412;
/// The server failed internally while handling an otherwise valid request.
pub const ERROR_INTERNAL: u16 = 500;
pub const ERROR_DEVICE_LINK_UNAVAILABLE: u16 = 411;
/// Most messages one `FetchHistory` may request.
pub const MAX_HISTORY_FETCH_MESSAGES: u16 = 500;
/// A mutation is only valid while at most this many messages exist in the room
/// after its target; the server validates by scanning back this many records.
pub const MUTATION_WINDOW_MESSAGES: usize = 256;
pub const JOIN_STRING_PREFIX: &str = "tcj1_";
pub const DEVICE_LINK_STRING_PREFIX: &str = "tcd1_";
const MAX_JOIN_STRING_BYTES: usize = 4096;
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ClientHello {
    pub version: u16,
    /// Transport-mode wire ids the client supports; the server selects one it is
    /// configured for or rejects the connection. See [`crate::crypto::TransportMode`].
    pub modes: Vec<u8>,
    pub client_nonce: Vec<u8>,
    pub client_ephemeral: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ServerHello {
    pub version: u16,
    /// The transport-mode wire id the server selected. Covered by `signature`.
    pub mode: u8,
    pub server_nonce: Vec<u8>,
    pub server_ephemeral: Vec<u8>,
    pub server_public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ClientControl {
    Authenticate {
        username: String,
        token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    Pair {
        username: String,
        pairing_code: String,
        token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    /// Self-service join on a public server. The server allocates, or reuses when
    /// `existing_token` is a valid dynamic token or recovery secret, a user id
    /// and issues a fresh bearer token. `password` is empty when the server
    /// requires none.
    OpenPair {
        username: String,
        password: String,
        existing_token: String,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    /// Creates a short-lived device link for the authenticated, E2E-bound
    /// account. The client hashes the redemption secret before transmission;
    /// the enrollment bundle is opaque to the server.
    CreateDeviceLink {
        redemption_secret_hash: Vec<u8>,
        enrollment_bundle: Vec<u8>,
    },
    CancelDeviceLink {
        redemption_secret_hash: Vec<u8>,
    },
    /// Fetches an opaque enrollment bundle before authentication. Fetching is
    /// non-consuming so a mistyped transfer password can be retried.
    FetchDeviceLink {
        redemption_secret: String,
    },
    /// Atomically consumes a device link, appends the signed AddDevice, and
    /// creates a device-specific bearer credential.
    RedeemDeviceLink {
        redemption_secret: String,
        statement: crate::e2e::AccountKeyStatement,
        receive_files: bool,
        file_receive_limit_bytes: u64,
    },
    SendChat {
        room_id: RoomId,
        body: String,
        /// Encoded [`crate::e2e::DmEventEnvelope`] replacing `body` in DM rooms;
        /// exactly one of the two is set.
        envelope: Option<Vec<u8>>,
    },
    SetVoiceStatus {
        status: ParticipantVoiceStatus,
    },
    PublishP2p {
        room_id: RoomId,
        generation: u64,
        nat: P2pNatKind,
        tie_breaker: u64,
        candidates: Vec<P2pCandidate>,
    },
    UploadFileStart {
        room_id: RoomId,
        transfer_id: FileTransferId,
        name: String,
        /// Original/decompressed byte length. For
        /// [`FileContentEncoding::Sealed`] this is the Padmé-padded length, so
        /// the server never learns the true size.
        size: u64,
        encoding: FileContentEncoding,
        /// Encoded [`crate::e2e::DmEventEnvelope`] holding the transfer's real
        /// metadata and content key; required for
        /// [`FileContentEncoding::Sealed`], absent otherwise. The server copies
        /// it verbatim into the announcement message and [`FileMetadata`].
        sealed_meta: Option<Vec<u8>>,
    },
    UploadFileChunk {
        transfer_id: FileTransferId,
        /// Contiguous offset in the relayed representation. For identity this
        /// is also the logical offset; for zstd it is the encoded stream offset.
        offset: u64,
        data: Vec<u8>,
    },
    UploadFileComplete {
        transfer_id: FileTransferId,
    },
    UploadFileCancel {
        transfer_id: FileTransferId,
        reason: String,
    },
    StartShare {
        room_id: RoomId,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        annexb: bool,
        extradata: Vec<u8>,
    },
    StopShare {
        stream_id: StreamId,
    },
    Ping {
        nonce: u64,
    },
    BugReportStart {
        report_id: BugReportId,
        description: String,
        /// JSON metadata: app version, device/buffer config, `/audio` snapshot.
        metadata: String,
        /// Total length of the zstd-compressed log payload that follows.
        logs_size: u64,
    },
    BugReportChunk {
        report_id: BugReportId,
        offset: u64,
        data: Vec<u8>,
    },
    BugReportComplete {
        report_id: BugReportId,
    },
    /// Page a room's retained history backwards from `before` (or the newest
    /// retained message when `None`). Answered with raw history chunks from
    /// [`crate::history`].
    FetchHistory {
        room_id: RoomId,
        before: Option<MessageId>,
        limit: u16,
    },
    /// Join the room's voice call, leaving any current call first. Voice
    /// membership is independent of which room's chat the client is reading.
    JoinVoice {
        room_id: RoomId,
    },
    LeaveVoice,
    /// Open (or return) the DM room shared with `user_id`. Answered with
    /// [`ServerControl::DmOpened`]; both endpoints also receive
    /// [`ServerControl::RoomUpserted`] on first creation.
    OpenDm {
        user_id: UserId,
    },
    /// A recipient declines an in-flight file it is receiving. `transfer_id` is
    /// the server transfer id the recipient learned from
    /// [`FileMetadata::transfer_id`]. The server drops this session from the
    /// transfer's recipient set, stopping further chunk relay to it; the upload
    /// continues for everyone else.
    SkipFile {
        transfer_id: FileTransferId,
    },
    /// Replace the body of a recent text message the sender authored. Only
    /// valid while the target is within the last
    /// [`MUTATION_WINDOW_MESSAGES`] messages of the room.
    EditChat {
        room_id: RoomId,
        target: MessageId,
        body: String,
        /// Encoded [`crate::e2e::DmEventEnvelope`] replacing `body` in DM rooms;
        /// exactly one of the two is set.
        envelope: Option<Vec<u8>>,
    },
    /// Delete a recent message the sender authored, of any kind. Bounded like
    /// [`ClientControl::EditChat`].
    DeleteChat {
        room_id: RoomId,
        target: MessageId,
        /// Encoded [`crate::e2e::DmEventEnvelope`] authenticating the deletion and
        /// target in DM rooms; absent in other rooms.
        envelope: Option<Vec<u8>>,
    },
    /// Appends one authority-signed transition to the authenticated account's
    /// device ledger. The statement's `previous` hash is the compare-and-swap
    /// anchor, so concurrent device administration cannot silently overwrite.
    AppendAccountKeyStatement {
        statement: crate::e2e::AccountKeyStatement,
    },
    /// Fetches a user's signed account/device ledger. When `after` is known the
    /// server may return only the suffix following that checkpoint.
    FetchAccountKeyChain {
        user_id: UserId,
        after: Option<LedgerHash>,
    },
    /// Proves possession of an active device signing key for this authenticated
    /// transport session. DM sends are unavailable until this succeeds.
    BindE2eDevice {
        device_id: DeviceId,
        key_epoch: u64,
        ledger_head: LedgerHash,
        signature: Vec<u8>,
    },
    /// Stores the opaque recovery bundle named by the signed ledger head. The
    /// server never receives the recovery secret or account authority seed.
    PutE2eRecoveryBundle {
        ledger_head: LedgerHash,
        bundle: Vec<u8>,
    },
    FetchE2eRecoveryBundle {
        user_id: UserId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ServerControl {
    Authenticated {
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
        /// Server-wide user directory: every configured user plus online
        /// dynamic users.
        users: Vec<UserSummary>,
        /// The room clients drop into (voice and first view) on connect.
        default_room: RoomId,
    },
    /// Open pairing succeeded. Carries the issued bearer token the client must
    /// store, alongside the same session details as [`ServerControl::Authenticated`].
    OpenPaired {
        token: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
        users: Vec<UserSummary>,
        default_room: RoomId,
    },
    Chat {
        message: ChatMessage,
    },
    /// The server refused an edit/delete request before broadcasting a
    /// mutation record.
    ChatMutationRejected {
        room_id: RoomId,
        target: MessageId,
        kind: ChatMutationKind,
        message: String,
    },
    /// A session began publishing voice in the room. `session_id` is how a
    /// client recognizes its own stream: the same user may hold several
    /// sessions, so matching on `user_id` would adopt another device's stream.
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
    VoiceStatus {
        room_id: RoomId,
        user_id: UserId,
        status: ParticipantVoiceStatus,
    },
    /// Reply to a [`ClientControl::JoinVoice`] the server could not satisfy,
    /// attributable to the room so the client can roll back its pending join.
    VoiceJoinFailed {
        room_id: RoomId,
        message: String,
    },
    /// Periodic authoritative snapshot of each room member's measured
    /// client-to-server RTT. Clients combine their local RTT with a remote
    /// member's value to estimate the full relayed path.
    RoomRttSnapshot {
        room_id: RoomId,
        members: Vec<ParticipantServerRtt>,
    },
    UdpBound,
    UdpReflexive {
        addr: String,
    },
    P2pNatProbe {
        probe_id: u8,
        addr: String,
    },
    P2pPeer {
        peer: P2pPeerInfo,
    },
    P2pPeerGone {
        session_id: SessionId,
        user_id: UserId,
    },
    FileOffered {
        file: FileMetadata,
        contents: bool,
    },
    /// Maps an uploader's connection-local transfer id to the durable identity
    /// assigned by the server and used by the room's chat announcement.
    UploadFileAccepted {
        client_transfer_id: FileTransferId,
        file: FileMetadata,
    },
    FileChunk {
        transfer_id: FileTransferId,
        /// Contiguous offset in the relayed representation. For identity this
        /// is also the logical offset; for zstd it is the encoded stream offset.
        offset: u64,
        data: Vec<u8>,
    },
    FileComplete {
        transfer_id: FileTransferId,
    },
    FileCanceled {
        transfer_id: FileTransferId,
        reason: String,
    },
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
        user_id: UserId,
        sender_name: String,
        codec: String,
        coded_width: u32,
        coded_height: u32,
        annexb: bool,
        extradata: Vec<u8>,
        view_secret: Vec<u8>,
    },
    ShareEnded {
        room_id: RoomId,
        stream_id: StreamId,
    },
    Pong {
        nonce: u64,
    },
    Error {
        code: u16,
        message: String,
    },
    BugReportSaved {
        report_id: BugReportId,
    },
    /// A room appeared or changed shape (today: DM creation). Pushed to every
    /// connected session that can access the room.
    RoomUpserted {
        room: RoomInfo,
    },
    /// Reply to [`ClientControl::OpenDm`] naming the DM room for the pair.
    DmOpened {
        room_id: RoomId,
        peer: UserId,
    },
    /// Server-wide presence for one user.
    Presence {
        user: UserSummary,
        online: bool,
    },
    /// An in-flight upload lost its last recipient (all recipients skipped,
    /// disconnected, or none accepted it at offer time), so the server is
    /// relaying its chunks to nobody. The uploader responds with
    /// [`ClientControl::UploadFileCancel`] to tear the transfer down cleanly.
    UploadDeclined {
        client_transfer_id: FileTransferId,
        reason: String,
    },
    /// Full signed device ledger, or the requested suffix when `base` is set.
    AccountKeyChain {
        user_id: UserId,
        base: Option<LedgerHash>,
        statements: Vec<crate::e2e::AccountKeyStatement>,
    },
    /// Push notification that a user's signed ledger advanced. Recipients fetch
    /// and validate the chain before changing current sending authorization.
    AccountKeyHeadChanged {
        user_id: UserId,
        roster_epoch: u64,
        head: LedgerHash,
    },
    E2eDeviceBound {
        device_id: DeviceId,
        key_epoch: u64,
    },
    E2eRecoveryBundle {
        user_id: UserId,
        bundle: Option<Vec<u8>>,
    },
    DeviceLinkCreated {
        redemption_secret_hash: Vec<u8>,
        expires_at_ms: u64,
    },
    DeviceLinkBundle {
        enrollment_bundle: Vec<u8>,
        account_chain: Vec<crate::e2e::AccountKeyStatement>,
        user_id: UserId,
        username: String,
    },
    DeviceLinked {
        token: String,
        username: String,
        udp_addr: String,
        udp_probe_addr: Option<String>,
        session_id: SessionId,
        user_id: UserId,
        rooms: Vec<RoomInfo>,
        users: Vec<UserSummary>,
        default_room: RoomId,
    },
    DeviceLinkRedeemed {
        redemption_secret_hash: Vec<u8>,
        device_id: DeviceId,
        device_name: String,
    },
}

/// The operation named by [`ServerControl::ChatMutationRejected`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub enum ChatMutationKind {
    Edit,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct RoomInfo {
    pub room_id: RoomId,
    pub name: String,
    pub kind: RoomKind,
    /// Latest message id assigned in the room, `None` before the first
    /// message. Clients compare it against their local read watermark for
    /// unread markers.
    pub head: Option<MessageId>,
    /// Users currently in the room's voice call.
    pub voice_users: Vec<UserId>,
}

/// Who can access a room and how it should be labeled.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum RoomKind {
    /// Every user on the server.
    Public,
    /// The configured member subset; doubles as the room's roster.
    Private { members: Vec<UserId> },
    /// Runtime-created DM. Clients label the room after the other endpoint.
    Dm { user_a: UserId, user_b: UserId },
}

/// Server-wide user directory entry.
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct UserSummary {
    pub user_id: UserId,
    pub username: String,
    pub online: bool,
    /// Server wall-clock (UNIX ms) the user's current session connected; 0
    /// when offline.
    pub connected_at_ms: u64,
    pub voice_status: ParticipantVoiceStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ParticipantServerRtt {
    pub user_id: UserId,
    pub server_rtt_ms: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ParticipantVoiceStatus {
    pub muted: bool,
    pub deafened: bool,
}

impl ParticipantVoiceStatus {
    pub fn normalized(mut self) -> Self {
        if self.deafened {
            self.muted = true;
        }
        self
    }
}

/// Edit/delete bits carried by [`ChatMessage::flags`].
///
/// On a mutation record (`target` set) exactly one bit names the operation. On
/// a folded resident message the bits are display state: `EDITED` marks a body
/// replaced by an edit, `DELETED` marks a hidden tombstone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Jsony)]
#[jsony(Binary)]
pub struct MessageFlags(pub u8);

impl MessageFlags {
    pub const EDITED: u8 = 1;
    pub const DELETED: u8 = 2;

    pub fn edited(self) -> bool {
        self.0 & Self::EDITED != 0
    }

    pub fn deleted(self) -> bool {
        self.0 & Self::DELETED != 0
    }

    pub fn set_edited(&mut self) {
        self.0 |= Self::EDITED;
    }

    pub fn set_deleted(&mut self) {
        self.0 |= Self::DELETED;
    }

    /// Whether the bits are a coherent state: no unknown bits, and not both
    /// `EDITED` and `DELETED` at once.
    pub fn is_valid(self) -> bool {
        self.0 & !(Self::EDITED | Self::DELETED) == 0 && self.0 != Self::EDITED | Self::DELETED
    }
}

/// One chat message or mutation record.
///
/// `target.is_some()` marks a mutation record: `flags` then names the
/// operation applied to the `target` message id, `body` carries the
/// replacement text for an edit and is empty for a delete. On a plain message
/// `target` is `None` and `flags` is folded display state (see
/// [`MessageFlags`]).
#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ChatMessage {
    pub message_id: MessageId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub sender_name: String,
    pub timestamp_ms: u64,
    pub body: String,
    /// `Some` when this chat announces a file transfer, carrying that transfer's
    /// id. The web view correlates the announcement with the inline file by it.
    pub file_transfer_id: Option<FileTransferId>,
    pub flags: MessageFlags,
    /// `Some` marks this record as a mutation of that message id.
    pub target: Option<MessageId>,
    /// Encoded [`crate::e2e::DmEventEnvelope`] carrying the sealed content in DM
    /// rooms; `body` is empty then and receiving clients fill it after opening
    /// the envelope.
    pub envelope: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct FileMetadata {
    pub transfer_id: FileTransferId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub sender_name: String,
    pub file_name: String,
    pub original_name: String,
    /// Original/decompressed byte length. Padmé-padded for
    /// [`FileContentEncoding::Sealed`] transfers.
    pub size: u64,
    pub encoding: FileContentEncoding,
    pub timestamp_ms: u64,
    /// Encoded [`crate::e2e::DmEventEnvelope`] with the transfer's real metadata
    /// and content key, relayed verbatim from
    /// [`ClientControl::UploadFileStart`] for sealed transfers.
    pub sealed_meta: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum FileContentEncoding {
    Identity,
    Zstd,
    /// End-to-end encrypted DM transfer: chunks are AEAD frames sealed by the
    /// sender, the true name/size/inner-encoding travel only inside
    /// `sealed_meta`. The server sees a placeholder name and a padded size.
    Sealed,
}

/// Maximum relayed byte count allowed for the encoded representation of a file.
///
/// `FileMetadata::size` and `UploadFileStart::size` carry the original logical
/// size. Identity transfers must match it exactly, while zstd streams are
/// allowed bounded expansion for frame headers, blocks, and incompressible data.
pub fn max_file_wire_bytes(encoding: FileContentEncoding, original_size: u64) -> u64 {
    match encoding {
        FileContentEncoding::Identity => original_size,
        FileContentEncoding::Zstd => original_size
            .saturating_add(original_size / 128)
            .saturating_add(64 * 1024),
        FileContentEncoding::Sealed => {
            let stream = max_file_wire_bytes(FileContentEncoding::Zstd, original_size);
            // Padmé expands the sealed stream by at most 1/8th.
            let stream = stream.saturating_add(stream / 8);
            let chunks = stream / (MAX_FILE_CHUNK_BYTES - crate::e2e::DM_CHUNK_OVERHEAD) as u64 + 2;
            stream.saturating_add(chunks.saturating_mul(crate::e2e::DM_CHUNK_OVERHEAD as u64))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum P2pNatKind {
    Unknown,
    Cone,
    Symmetric,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum P2pRole {
    Controlling,
    Controlled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum P2pCandidateKind {
    Host,
    ServerReflexive,
    PeerReflexive,
    PortMapped,
    Relay,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct P2pCandidate {
    pub id: u32,
    pub socket_id: u32,
    pub generation: u64,
    pub kind: P2pCandidateKind,
    pub addr: String,
    pub priority: u32,
    pub foundation: String,
    pub verified: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct P2pKey {
    pub id: u32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct P2pPeerInfo {
    pub room_id: RoomId,
    pub session_id: SessionId,
    pub user_id: UserId,
    pub generation: u64,
    pub role: P2pRole,
    pub nat: P2pNatKind,
    pub tie_breaker: u64,
    pub candidates: Vec<P2pCandidate>,
    pub send_key: P2pKey,
    pub recv_key: P2pKey,
    pub stun_key: P2pKey,
    pub connection_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct InviteTicket {
    pub version: u16,
    pub pairing_code: String,
    pub tcp_addr: String,
    pub udp_addr: String,
    pub udp_probe_addr: Option<String>,
    pub server_public_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DeviceLinkTicket {
    pub version: u16,
    pub redemption_secret: String,
    pub tcp_addr: String,
    pub udp_addr: String,
    pub udp_probe_addr: Option<String>,
    pub server_public_key: String,
}

pub fn encode_client_hello(value: &ClientHello) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_client_hello(bytes: &[u8]) -> Result<ClientHello, String> {
    bounded_decode(bytes)
}

pub fn encode_server_hello(value: &ServerHello) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_server_hello(bytes: &[u8]) -> Result<ServerHello, String> {
    bounded_decode(bytes)
}

pub fn encode_client_control(value: &ClientControl) -> Result<Vec<u8>, String> {
    validate_client_control(value)?;
    Ok(jsony::to_binary(value))
}

pub fn decode_client_control(bytes: &[u8]) -> Result<ClientControl, String> {
    let value: ClientControl = bounded_decode(bytes)?;
    validate_client_control(&value)?;
    Ok(value)
}

pub fn encode_server_control(value: &ServerControl) -> Vec<u8> {
    jsony::to_binary(value)
}

pub fn decode_server_control(bytes: &[u8]) -> Result<ServerControl, String> {
    bounded_decode(bytes)
}

pub fn encode_invite_ticket(value: &InviteTicket) -> Result<String, String> {
    validate_invite_ticket(value)?;
    let payload = jsony::to_binary(value);
    let mut out =
        String::with_capacity(JOIN_STRING_PREFIX.len() + encoded_base64_len(payload.len()));
    out.push_str(JOIN_STRING_PREFIX);
    encode_base64url_no_pad(&payload, &mut out);
    Ok(out)
}

pub fn decode_invite_ticket(value: &str) -> Result<InviteTicket, String> {
    let value = value.trim();
    if value.len() > MAX_JOIN_STRING_BYTES {
        return Err("join string exceeds maximum length".to_string());
    }
    let payload = value
        .strip_prefix(JOIN_STRING_PREFIX)
        .ok_or_else(|| "join string has an unknown prefix".to_string())?;
    let bytes = decode_base64url_no_pad(payload)?;
    let ticket: InviteTicket = bounded_decode(&bytes)?;
    validate_invite_ticket(&ticket)?;
    Ok(ticket)
}

pub fn encode_device_link_ticket(value: &DeviceLinkTicket) -> Result<String, String> {
    validate_device_link_ticket(value)?;
    let payload = jsony::to_binary(value);
    let mut out = String::with_capacity(
        DEVICE_LINK_STRING_PREFIX.len() + encoded_base64_len(payload.len()),
    );
    out.push_str(DEVICE_LINK_STRING_PREFIX);
    encode_base64url_no_pad(&payload, &mut out);
    Ok(out)
}

pub fn decode_device_link_ticket(value: &str) -> Result<DeviceLinkTicket, String> {
    let value = value.trim();
    if value.len() > MAX_JOIN_STRING_BYTES {
        return Err("device pairing string exceeds maximum length".to_string());
    }
    let payload = value
        .strip_prefix(DEVICE_LINK_STRING_PREFIX)
        .ok_or_else(|| "device pairing string has an unknown prefix".to_string())?;
    let bytes = decode_base64url_no_pad(payload)?;
    let ticket: DeviceLinkTicket = bounded_decode(&bytes)?;
    validate_device_link_ticket(&ticket)?;
    Ok(ticket)
}

fn bounded_decode<T>(bytes: &[u8]) -> Result<T, String>
where
    T: for<'a> jsony::FromBinary<'a>,
{
    if bytes.len() > MAX_CONTROL_PAYLOAD_BYTES {
        return Err("control payload exceeds maximum length".to_string());
    }
    jsony::from_binary(bytes).map_err(|error| error.to_string())
}

/// Whether a published P2P candidate address is acceptable: either a literal
/// `ip:port` socket address or an `{token}.local:port` mDNS host name.
fn is_valid_candidate_addr(addr: &str) -> bool {
    if addr.parse::<std::net::SocketAddr>().is_ok() {
        return true;
    }
    let Some((host, port)) = addr.rsplit_once(':') else {
        return false;
    };
    port.parse::<u16>().is_ok() && is_valid_mdns_candidate_name(host)
}

/// Whether a host name is a single-label `.local` mDNS candidate name, matching
/// the WebRTC mDNS-ICE filter (one dot, `.local` suffix, alphanumeric label).
pub fn is_valid_mdns_candidate_name(name: &str) -> bool {
    let trimmed = name.trim_end_matches('.');
    if trimmed.len() > 80 || trimmed.matches('.').count() != 1 {
        return false;
    }
    let Some(label) = trimmed.strip_suffix(".local") else {
        return false;
    };
    !label.is_empty()
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn validate_client_control(value: &ClientControl) -> Result<(), String> {
    match value {
        ClientControl::Authenticate { token, .. } => {
            validate_auth_field("token", token)?;
        }
        ClientControl::Pair {
            username,
            pairing_code,
            token,
            ..
        } => {
            validate_auth_field("display name", username)?;
            validate_auth_field("pairing code", pairing_code)?;
            validate_auth_field("token", token)?;
            if pairing_code.len() < MIN_PAIRING_SECRET_BYTES {
                return Err("pairing code is too short".to_string());
            }
            if token.len() < MIN_PAIRED_TOKEN_BYTES {
                return Err("paired token is too short".to_string());
            }
        }
        ClientControl::OpenPair {
            username,
            password,
            existing_token,
            ..
        } => {
            validate_auth_field("display name", username)?;
            validate_optional_auth_field("password", password)?;
            validate_optional_auth_field("existing token", existing_token)?;
        }
        ClientControl::CreateDeviceLink {
            redemption_secret_hash,
            enrollment_bundle,
        } => {
            if redemption_secret_hash.len() != 32 {
                return Err("device-link secret hash has the wrong length".to_string());
            }
            if enrollment_bundle.is_empty() || enrollment_bundle.len() > 16 * 1024 {
                return Err("device enrollment bundle length is invalid".to_string());
            }
        }
        ClientControl::CancelDeviceLink {
            redemption_secret_hash,
        } => {
            if redemption_secret_hash.len() != 32 {
                return Err("device-link secret hash has the wrong length".to_string());
            }
        }
        ClientControl::FetchDeviceLink { redemption_secret }
        | ClientControl::RedeemDeviceLink {
            redemption_secret, ..
        } => {
            validate_auth_field("device-link redemption secret", redemption_secret)?;
            if redemption_secret.len() < MIN_PAIRING_SECRET_BYTES {
                return Err("device-link redemption secret is too short".to_string());
            }
        }
        ClientControl::SendChat { body, envelope, .. }
        | ClientControl::EditChat { body, envelope, .. } => match envelope {
            None => {
                if body.trim().is_empty() {
                    return Err("chat message is empty".to_string());
                }
                if body.len() > MAX_CHAT_BODY_BYTES {
                    return Err("chat message exceeds maximum length".to_string());
                }
            }
            Some(envelope) => {
                if !body.is_empty() {
                    return Err("chat message carries both text and an envelope".to_string());
                }
                validate_dm_envelope(envelope)?;
            }
        },
        ClientControl::DeleteChat { envelope, .. } => {
            if let Some(envelope) = envelope {
                validate_dm_envelope(envelope)?;
            }
        }
        ClientControl::PublishP2p { candidates, .. } => {
            if candidates.len() > 64 {
                return Err("too many P2P candidates".to_string());
            }
            for candidate in candidates {
                if !is_valid_candidate_addr(&candidate.addr) {
                    return Err("P2P candidate address is invalid".to_string());
                }
            }
        }
        ClientControl::UploadFileStart {
            name,
            encoding,
            sealed_meta,
            ..
        } => {
            validate_file_name(name)?;
            match sealed_meta {
                None => {
                    if *encoding == FileContentEncoding::Sealed {
                        return Err("sealed upload is missing its metadata envelope".to_string());
                    }
                }
                Some(sealed_meta) => {
                    if *encoding != FileContentEncoding::Sealed {
                        return Err("metadata envelope on an unsealed upload".to_string());
                    }
                    validate_dm_envelope(sealed_meta)?;
                }
            }
        }
        ClientControl::UploadFileChunk { data, .. } => {
            if data.is_empty() {
                return Err("file chunk is empty".to_string());
            }
            if data.len() > MAX_FILE_CHUNK_BYTES {
                return Err("file chunk exceeds maximum length".to_string());
            }
        }
        ClientControl::UploadFileCancel { reason, .. } => {
            if reason.len() > 512 {
                return Err("file cancel reason exceeds maximum length".to_string());
            }
        }
        ClientControl::BugReportStart {
            description,
            metadata,
            logs_size,
            ..
        } => {
            if description.len() > MAX_BUG_REPORT_DESC_BYTES {
                return Err("bug report description exceeds maximum length".to_string());
            }
            if metadata.len() > MAX_BUG_REPORT_METADATA_BYTES {
                return Err("bug report metadata exceeds maximum length".to_string());
            }
            if *logs_size > MAX_BUG_REPORT_BYTES {
                return Err("bug report logs exceed maximum length".to_string());
            }
        }
        ClientControl::BugReportChunk { data, .. } => {
            if data.is_empty() {
                return Err("bug report chunk is empty".to_string());
            }
            if data.len() > MAX_FILE_CHUNK_BYTES {
                return Err("bug report chunk exceeds maximum length".to_string());
            }
        }
        ClientControl::FetchHistory { limit, .. } => {
            if *limit == 0 {
                return Err("history fetch limit must be non-zero".to_string());
            }
            if *limit > MAX_HISTORY_FETCH_MESSAGES {
                return Err("history fetch limit exceeds maximum".to_string());
            }
        }
        ClientControl::StartShare {
            codec, extradata, ..
        } => {
            if codec.trim().is_empty() {
                return Err("share codec is empty".to_string());
            }
            if codec.len() > MAX_VIDEO_CODEC_BYTES {
                return Err("share codec exceeds maximum length".to_string());
            }
            if extradata.len() > MAX_VIDEO_EXTRADATA_BYTES {
                return Err("share extradata exceeds maximum length".to_string());
            }
        }
        ClientControl::AppendAccountKeyStatement { statement } => {
            if statement.authority_signature.len() != 64
                || statement
                    .co_signature
                    .as_ref()
                    .is_some_and(|signature| signature.len() != 64)
            {
                return Err("account key statement signature has the wrong length".to_string());
            }
        }
        ClientControl::BindE2eDevice {
            key_epoch,
            signature,
            ..
        } => {
            if *key_epoch == 0 {
                return Err("device key epoch is zero".to_string());
            }
            if signature.len() != 64 {
                return Err("device binding signature has the wrong length".to_string());
            }
        }
        ClientControl::PutE2eRecoveryBundle { bundle, .. } => {
            if bundle.is_empty() || bundle.len() > 16 * 1024 {
                return Err("recovery bundle length is invalid".to_string());
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_dm_envelope(envelope: &[u8]) -> Result<(), String> {
    if envelope.is_empty() {
        return Err("message envelope is empty".to_string());
    }
    if envelope.len() > crate::e2e::MAX_DM_ENVELOPE_BYTES {
        return Err("message envelope exceeds maximum length".to_string());
    }
    Ok(())
}

fn validate_invite_ticket(value: &InviteTicket) -> Result<(), String> {
    if value.version != crate::PROTOCOL_VERSION {
        return Err(format!("unsupported invite version {}", value.version));
    }
    validate_auth_field("pairing code", &value.pairing_code)?;
    if value.pairing_code.len() < MIN_PAIRING_SECRET_BYTES {
        return Err("pairing code is too short".to_string());
    }
    validate_auth_field("server public key", &value.server_public_key)?;
    validate_endpoint("invite TCP address", &value.tcp_addr)?;
    validate_endpoint("invite UDP address", &value.udp_addr)?;
    if let Some(addr) = &value.udp_probe_addr {
        validate_endpoint("invite UDP probe address", addr)?;
    }
    Ok(())
}

fn validate_device_link_ticket(value: &DeviceLinkTicket) -> Result<(), String> {
    if value.version != crate::PROTOCOL_VERSION {
        return Err(format!(
            "unsupported device-link version {}",
            value.version
        ));
    }
    validate_auth_field("device-link redemption secret", &value.redemption_secret)?;
    if value.redemption_secret.len() < MIN_PAIRING_SECRET_BYTES {
        return Err("device-link redemption secret is too short".to_string());
    }
    let secret = crate::crypto::decode_hex(&value.redemption_secret)
        .map_err(|_| "device-link redemption secret is not valid hex".to_string())?;
    if secret.len() != crate::crypto::KEY_LEN {
        return Err("device-link redemption secret has the wrong length".to_string());
    }
    validate_auth_field("server public key", &value.server_public_key)?;
    let key = crate::crypto::decode_hex(&value.server_public_key)
        .map_err(|_| "device-link server key is not valid hex".to_string())?;
    if key.len() != crate::crypto::ED25519_PUBLIC_KEY_LEN {
        return Err("device-link server key has the wrong length".to_string());
    }
    validate_endpoint("device-link TCP address", &value.tcp_addr)?;
    validate_endpoint("device-link UDP address", &value.udp_addr)?;
    if let Some(addr) = &value.udp_probe_addr {
        validate_endpoint("device-link UDP probe address", addr)?;
    }
    Ok(())
}

fn validate_endpoint(name: &str, value: &str) -> Result<(), String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{name} is empty"));
    }
    if value.len() > MAX_AUTH_FIELD_BYTES {
        return Err(format!("{name} exceeds maximum length"));
    }
    if value.parse::<std::net::SocketAddr>().is_ok() {
        return Ok(());
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return Err(format!("{name} must include a port"));
    };
    if host.trim().is_empty() || port.trim().is_empty() {
        return Err(format!("{name} must include a host and port"));
    }
    port.parse::<u16>()
        .map(|_| ())
        .map_err(|_| format!("{name} port is invalid"))
}

fn validate_auth_field(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} is empty"));
    }
    validate_optional_auth_field(name, value)
}

fn validate_optional_auth_field(name: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_AUTH_FIELD_BYTES {
        return Err(format!("{name} exceeds maximum length"));
    }
    Ok(())
}

fn encoded_base64_len(len: usize) -> usize {
    (len / 3) * 4
        + match len % 3 {
            0 => 0,
            1 => 2,
            _ => 3,
        }
}

fn encode_base64url_no_pad(bytes: &[u8], out: &mut String) {
    let mut chunks = bytes.chunks_exact(3);
    for chunk in &mut chunks {
        let value = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
        out.push(BASE64URL[((value >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 12) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 6) & 0x3f) as usize] as char);
        out.push(BASE64URL[(value & 0x3f) as usize] as char);
    }
    let remainder = chunks.remainder();
    if remainder.len() == 1 {
        let value = (remainder[0] as u32) << 16;
        out.push(BASE64URL[((value >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 12) & 0x3f) as usize] as char);
    } else if remainder.len() == 2 {
        let value = ((remainder[0] as u32) << 16) | ((remainder[1] as u32) << 8);
        out.push(BASE64URL[((value >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 12) & 0x3f) as usize] as char);
        out.push(BASE64URL[((value >> 6) & 0x3f) as usize] as char);
    }
}

fn decode_base64url_no_pad(value: &str) -> Result<Vec<u8>, String> {
    if value.is_empty() {
        return Err("join string payload is empty".to_string());
    }
    if value.as_bytes().contains(&b'=') {
        return Err("join string payload must not contain padding".to_string());
    }
    if value.len() % 4 == 1 {
        return Err("join string payload length is invalid".to_string());
    }

    let mut out = Vec::with_capacity((value.len() / 4) * 3 + 2);
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        let a = decode_base64url_byte(bytes[index])?;
        let b = decode_base64url_byte(bytes[index + 1])?;
        let c = decode_base64url_byte(bytes[index + 2])?;
        let d = decode_base64url_byte(bytes[index + 3])?;
        let packed = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | d as u32;
        out.push(((packed >> 16) & 0xff) as u8);
        out.push(((packed >> 8) & 0xff) as u8);
        out.push((packed & 0xff) as u8);
        index += 4;
    }

    match bytes.len() - index {
        0 => {}
        2 => {
            let a = decode_base64url_byte(bytes[index])?;
            let b = decode_base64url_byte(bytes[index + 1])?;
            let packed = ((a as u32) << 18) | ((b as u32) << 12);
            out.push(((packed >> 16) & 0xff) as u8);
        }
        3 => {
            let a = decode_base64url_byte(bytes[index])?;
            let b = decode_base64url_byte(bytes[index + 1])?;
            let c = decode_base64url_byte(bytes[index + 2])?;
            let packed = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6);
            out.push(((packed >> 16) & 0xff) as u8);
            out.push(((packed >> 8) & 0xff) as u8);
        }
        _ => return Err("join string payload length is invalid".to_string()),
    }

    Ok(out)
}

fn decode_base64url_byte(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err("join string payload contains invalid base64url data".to_string()),
    }
}

fn validate_file_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("file name is empty".to_string());
    }
    if name.len() > MAX_FILE_NAME_BYTES {
        return Err("file name exceeds maximum length".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("file name must not contain path separators".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_socket_addr_and_mdns_candidate_addrs() {
        assert!(is_valid_candidate_addr("192.168.1.2:5000"));
        assert!(is_valid_candidate_addr("[fe80::1]:5000"));
        assert!(is_valid_candidate_addr("abc123.local:5000"));
        // Rejects multi-label names, missing port, and non-.local hosts.
        assert!(!is_valid_candidate_addr("sub.token.local:5000"));
        assert!(!is_valid_candidate_addr("token.local"));
        assert!(!is_valid_candidate_addr("router.lan:5000"));
        assert!(!is_valid_candidate_addr("token.local:notaport"));
        assert!(!is_valid_mdns_candidate_name(".local"));
        assert!(!is_valid_mdns_candidate_name("under_score.local"));
    }

    #[test]
    fn client_control_round_trips() {
        let message = ClientControl::SendChat {
            envelope: None,
            room_id: RoomId(7),
            body: "hello".to_string(),
        };
        let encoded = encode_client_control(&message).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), message);
    }

    #[test]
    fn mutation_controls_round_trip() {
        let edit = ClientControl::EditChat {
            envelope: None,
            room_id: RoomId(7),
            target: MessageId(42),
            body: "revised".to_string(),
        };
        let encoded = encode_client_control(&edit).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), edit);

        let delete = ClientControl::DeleteChat {
            room_id: RoomId(7),
            target: MessageId(42),
            envelope: Some(vec![1u8; 160]),
        };
        let encoded = encode_client_control(&delete).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), delete);

        let invalid_delete = ClientControl::DeleteChat {
            room_id: RoomId(7),
            target: MessageId(42),
            envelope: Some(Vec::new()),
        };
        assert!(encode_client_control(&invalid_delete).is_err());
    }

    #[test]
    fn edit_chat_rejects_empty_and_oversize_body() {
        let empty = ClientControl::EditChat {
            envelope: None,
            room_id: RoomId(7),
            target: MessageId(42),
            body: "  \n".to_string(),
        };
        assert!(encode_client_control(&empty).is_err());

        let oversize = ClientControl::EditChat {
            envelope: None,
            room_id: RoomId(7),
            target: MessageId(42),
            body: "x".repeat(MAX_CHAT_BODY_BYTES + 1),
        };
        assert!(encode_client_control(&oversize).is_err());
    }

    #[test]
    fn rejects_body_and_envelope_together_or_neither() {
        let both = ClientControl::SendChat {
            room_id: RoomId(7),
            body: "hello".to_string(),
            envelope: Some(vec![1u8; 160]),
        };
        assert!(encode_client_control(&both).is_err());

        let neither = ClientControl::SendChat {
            room_id: RoomId(7),
            body: String::new(),
            envelope: None,
        };
        assert!(encode_client_control(&neither).is_err());

        let sealed = ClientControl::SendChat {
            room_id: RoomId(7),
            body: String::new(),
            envelope: Some(vec![1u8; 160]),
        };
        assert!(encode_client_control(&sealed).is_ok());

        let oversize = ClientControl::SendChat {
            room_id: RoomId(7),
            body: String::new(),
            envelope: Some(vec![1u8; crate::e2e::MAX_DM_ENVELOPE_BYTES + 1]),
        };
        assert!(encode_client_control(&oversize).is_err());

        let empty = ClientControl::SendChat {
            room_id: RoomId(7),
            body: String::new(),
            envelope: Some(Vec::new()),
        };
        assert!(encode_client_control(&empty).is_err());
    }

    #[test]
    fn sealed_upload_start_requires_sealed_meta() {
        let upload = |encoding, sealed_meta| ClientControl::UploadFileStart {
            room_id: RoomId(7),
            transfer_id: FileTransferId(1),
            name: "sealed.bin".to_string(),
            size: 4096,
            encoding,
            sealed_meta,
        };
        assert!(encode_client_control(&upload(FileContentEncoding::Sealed, None)).is_err());
        assert!(
            encode_client_control(&upload(FileContentEncoding::Zstd, Some(vec![1u8; 160])))
                .is_err()
        );
        assert!(
            encode_client_control(&upload(FileContentEncoding::Sealed, Some(vec![1u8; 160])))
                .is_ok()
        );
    }

    #[test]
    fn sealed_wire_bound_covers_chunk_overhead_and_padding() {
        for original in [0u64, 1, 1024, 200_000, 5_000_000] {
            let stream =
                crate::e2e::padme_len(max_file_wire_bytes(FileContentEncoding::Zstd, original));
            let chunk_payload = (MAX_FILE_CHUNK_BYTES - crate::e2e::DM_CHUNK_OVERHEAD) as u64;
            let chunks = stream.div_ceil(chunk_payload).max(1);
            let wire = stream + chunks * crate::e2e::DM_CHUNK_OVERHEAD as u64;
            assert!(
                wire <= max_file_wire_bytes(FileContentEncoding::Sealed, original),
                "bound too small for original {original}"
            );
        }
    }

    #[test]
    fn authenticate_control_round_trips_without_user() {
        let message = ClientControl::Authenticate {
            username: "Alice".to_string(),
            token: "client-generated-token-with-at-least-32-bytes".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        let encoded = encode_client_control(&message).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), message);
    }

    #[test]
    fn presence_round_trips_user_summary() {
        let message = ServerControl::Presence {
            user: UserSummary {
                user_id: UserId(5),
                username: "Alice".to_string(),
                online: true,
                connected_at_ms: 1_700_000_000_000,
                voice_status: ParticipantVoiceStatus {
                    muted: true,
                    deafened: false,
                },
            },
            online: true,
        };
        let encoded = encode_server_control(&message);
        assert_eq!(decode_server_control(&encoded).unwrap(), message);
    }

    #[test]
    fn p2p_control_round_trips() {
        let control = ClientControl::PublishP2p {
            room_id: RoomId(1),
            generation: 2,
            nat: P2pNatKind::Cone,
            tie_breaker: 99,
            candidates: vec![P2pCandidate {
                id: 1,
                socket_id: 1,
                generation: 2,
                kind: P2pCandidateKind::Host,
                addr: "192.168.1.2:5000".to_string(),
                priority: 1,
                foundation: "host-udp4".to_string(),
                verified: true,
            }],
        };
        let encoded = encode_client_control(&control).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), control);
    }

    #[test]
    fn p2p_peer_info_round_trips() {
        let control = ServerControl::P2pPeer {
            peer: P2pPeerInfo {
                room_id: RoomId(1),
                session_id: SessionId(2),
                user_id: UserId(3),
                generation: 4,
                role: P2pRole::Controlling,
                nat: P2pNatKind::Cone,
                tie_breaker: 99,
                candidates: vec![],
                send_key: P2pKey {
                    id: 1,
                    bytes: vec![1, 2, 3],
                },
                recv_key: P2pKey {
                    id: 2,
                    bytes: vec![4, 5, 6],
                },
                stun_key: P2pKey {
                    id: 3,
                    bytes: vec![7, 8, 9],
                },
                connection_id: 77,
            },
        };
        let encoded = encode_server_control(&control);
        assert_eq!(decode_server_control(&encoded).unwrap(), control);
    }

    #[test]
    fn voice_status_controls_round_trip() {
        let status = ParticipantVoiceStatus {
            muted: true,
            deafened: false,
        };
        let client = ClientControl::SetVoiceStatus { status };
        let encoded = encode_client_control(&client).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), client);

        let server = ServerControl::VoiceStatus {
            room_id: RoomId(1),
            user_id: UserId(2),
            status,
        };
        let encoded = encode_server_control(&server);
        assert_eq!(decode_server_control(&encoded).unwrap(), server);
    }

    #[test]
    fn voice_join_failed_round_trips() {
        let server = ServerControl::VoiceJoinFailed {
            room_id: RoomId(7),
            message: "room not found".to_string(),
        };
        let encoded = encode_server_control(&server);
        assert_eq!(decode_server_control(&encoded).unwrap(), server);
    }

    #[test]
    fn room_rtt_snapshot_round_trips() {
        let server = ServerControl::RoomRttSnapshot {
            room_id: RoomId(1),
            members: vec![
                ParticipantServerRtt {
                    user_id: UserId(2),
                    server_rtt_ms: Some(34),
                },
                ParticipantServerRtt {
                    user_id: UserId(3),
                    server_rtt_ms: None,
                },
            ],
        };
        let encoded = encode_server_control(&server);
        assert_eq!(decode_server_control(&encoded).unwrap(), server);
    }

    #[test]
    fn rejects_empty_chat() {
        let message = ClientControl::SendChat {
            envelope: None,
            room_id: RoomId(7),
            body: "  ".to_string(),
        };
        assert!(encode_client_control(&message).is_err());
    }

    #[test]
    fn pair_control_requires_strong_one_time_and_session_secrets() {
        let weak = ClientControl::Pair {
            username: "Alice".to_string(),
            pairing_code: "short".to_string(),
            token: "also-short".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&weak).is_err());

        let strong = ClientControl::Pair {
            username: "Alice".to_string(),
            pairing_code: "pair-alice-please-change".to_string(),
            token: "client-generated-token-with-at-least-32-bytes".to_string(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&strong).is_ok());
    }

    #[test]
    fn open_pair_allows_empty_optional_password_and_existing_token() {
        let control = ClientControl::OpenPair {
            username: "Alice".to_string(),
            password: String::new(),
            existing_token: String::new(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };

        let encoded = encode_client_control(&control).unwrap();

        assert_eq!(decode_client_control(&encoded).unwrap(), control);
    }

    #[test]
    fn open_pair_caps_optional_auth_fields() {
        let long = "x".repeat(MAX_AUTH_FIELD_BYTES + 1);
        let password = ClientControl::OpenPair {
            username: "Alice".to_string(),
            password: long.clone(),
            existing_token: String::new(),
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&password).is_err());

        let existing = ClientControl::OpenPair {
            username: "Alice".to_string(),
            password: String::new(),
            existing_token: long,
            receive_files: true,
            file_receive_limit_bytes: DEFAULT_FILE_SIZE_LIMIT_BYTES,
        };
        assert!(encode_client_control(&existing).is_err());
    }

    #[test]
    fn invite_ticket_round_trips_join_string() {
        let ticket = InviteTicket {
            version: crate::PROTOCOL_VERSION,
            pairing_code: "pair-alice-please-change".to_string(),
            tcp_addr: "127.0.0.1:41000".to_string(),
            udp_addr: "127.0.0.1:41000".to_string(),
            udp_probe_addr: Some("127.0.0.1:41002".to_string()),
            server_public_key: "de1235b52a8b96f16f91124a8b462d463f2af83756946effa70e842142a6d7cf"
                .to_string(),
        };

        let encoded = encode_invite_ticket(&ticket).unwrap();

        assert!(encoded.starts_with(JOIN_STRING_PREFIX));
        assert_eq!(decode_invite_ticket(&encoded).unwrap(), ticket);
    }

    #[test]
    fn open_paired_control_round_trips_endpoints() {
        let control = ServerControl::OpenPaired {
            token: "tct1_token".to_string(),
            udp_addr: "198.51.100.20:54100".to_string(),
            udp_probe_addr: Some("198.51.100.20:54101".to_string()),
            session_id: SessionId(7),
            user_id: UserId(4_294_967_296),
            rooms: vec![RoomInfo {
                room_id: RoomId(1),
                name: "lobby".to_string(),
                kind: RoomKind::Public,
                head: Some(MessageId(42)),
                voice_users: vec![UserId(4_294_967_296)],
            }],
            users: vec![UserSummary {
                user_id: UserId(4_294_967_296),
                username: "Zoe".to_string(),
                online: true,
                connected_at_ms: 1_700_000_000_000,
                voice_status: ParticipantVoiceStatus::default(),
            }],
            default_room: RoomId(1),
        };

        let encoded = encode_server_control(&control);

        assert_eq!(decode_server_control(&encoded).unwrap(), control);
    }

    #[test]
    fn chat_mutation_rejection_round_trips_target_and_kind() {
        let control = ServerControl::ChatMutationRejected {
            room_id: RoomId(3),
            target: MessageId(42),
            kind: ChatMutationKind::Delete,
            message: "message is too old".to_string(),
        };

        let encoded = encode_server_control(&control);

        assert_eq!(decode_server_control(&encoded).unwrap(), control);
    }

    #[test]
    fn room_info_round_trips_kind_head_and_voice() {
        for kind in [
            RoomKind::Public,
            RoomKind::Private {
                members: vec![UserId(1), UserId(2)],
            },
            RoomKind::Dm {
                user_a: UserId(3),
                user_b: UserId(4_294_967_296),
            },
        ] {
            let control = ServerControl::RoomUpserted {
                room: RoomInfo {
                    room_id: RoomId(9),
                    name: "dev".to_string(),
                    kind,
                    head: None,
                    voice_users: Vec::new(),
                },
            };
            let encoded = encode_server_control(&control);
            assert_eq!(decode_server_control(&encoded).unwrap(), control);
        }
    }

    #[test]
    fn fetch_history_request_round_trips() {
        let request = ClientControl::FetchHistory {
            room_id: RoomId(2),
            before: Some(MessageId(300)),
            limit: 100,
        };
        let encoded = encode_client_control(&request).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), request);
    }

    #[test]
    fn fetch_history_rejects_zero_and_oversize_limits() {
        let zero = ClientControl::FetchHistory {
            room_id: RoomId(2),
            before: None,
            limit: 0,
        };
        assert!(encode_client_control(&zero).is_err());

        let oversize = ClientControl::FetchHistory {
            room_id: RoomId(2),
            before: None,
            limit: MAX_HISTORY_FETCH_MESSAGES + 1,
        };
        assert!(encode_client_control(&oversize).is_err());
    }

    #[test]
    fn voice_membership_controls_round_trip() {
        let join = ClientControl::JoinVoice { room_id: RoomId(3) };
        let encoded = encode_client_control(&join).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), join);

        let leave = ClientControl::LeaveVoice;
        let encoded = encode_client_control(&leave).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), leave);
    }

    #[test]
    fn dm_controls_round_trip() {
        let open = ClientControl::OpenDm {
            user_id: UserId(4_294_967_296),
        };
        let encoded = encode_client_control(&open).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), open);

        let opened = ServerControl::DmOpened {
            room_id: RoomId(0x8000_0000),
            peer: UserId(4_294_967_296),
        };
        let encoded = encode_server_control(&opened);
        assert_eq!(decode_server_control(&encoded).unwrap(), opened);
    }

    #[test]
    fn file_upload_control_round_trips() {
        for encoding in [FileContentEncoding::Identity, FileContentEncoding::Zstd] {
            let start = ClientControl::UploadFileStart {
                sealed_meta: None,
                room_id: RoomId(2),
                transfer_id: FileTransferId(9),
                name: "report.bin".to_string(),
                size: 1234,
                encoding,
            };
            let encoded = encode_client_control(&start).unwrap();
            assert_eq!(decode_client_control(&encoded).unwrap(), start);
        }

        let control = ClientControl::UploadFileChunk {
            transfer_id: FileTransferId(9),
            offset: 32,
            data: vec![0x28, 0xb5, 0x2f, 0xfd, 0, 0xff],
        };
        let encoded = encode_client_control(&control).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), control);

        for encoding in [FileContentEncoding::Identity, FileContentEncoding::Zstd] {
            let accepted = ServerControl::UploadFileAccepted {
                client_transfer_id: FileTransferId(9),
                file: FileMetadata {
                    sealed_meta: None,
                    transfer_id: FileTransferId(17),
                    room_id: RoomId(2),
                    sender: UserId(3),
                    sender_name: "alice".to_string(),
                    file_name: "report.pdf".to_string(),
                    original_name: "report.pdf".to_string(),
                    size: 1234,
                    encoding,
                    timestamp_ms: 55,
                },
            };
            let encoded = encode_server_control(&accepted);
            assert_eq!(decode_server_control(&encoded).unwrap(), accepted);
        }
    }

    #[test]
    fn file_wire_bound_depends_on_content_encoding() {
        let original = 50 * 1024 * 1024;

        assert_eq!(
            max_file_wire_bytes(FileContentEncoding::Identity, original),
            original
        );
        assert_eq!(
            max_file_wire_bytes(FileContentEncoding::Zstd, original),
            original + original / 128 + 64 * 1024
        );
    }

    #[test]
    fn bug_report_control_round_trips() {
        let start = ClientControl::BugReportStart {
            report_id: BugReportId(4),
            description: "audio cut out after rejoin".to_string(),
            metadata: "{\"version\":\"0.1.0\"}".to_string(),
            logs_size: 2048,
        };
        let encoded = encode_client_control(&start).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), start);

        let chunk = ClientControl::BugReportChunk {
            report_id: BugReportId(4),
            offset: 32,
            data: vec![9, 8, 7],
        };
        let encoded = encode_client_control(&chunk).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), chunk);
    }

    #[test]
    fn bug_report_rejects_oversize_logs() {
        let control = ClientControl::BugReportStart {
            report_id: BugReportId(1),
            description: String::new(),
            metadata: String::new(),
            logs_size: MAX_BUG_REPORT_BYTES + 1,
        };
        assert!(encode_client_control(&control).is_err());
    }

    #[test]
    fn share_control_round_trips() {
        let start = ClientControl::StartShare {
            room_id: RoomId(3),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            annexb: true,
            extradata: vec![],
        };
        let encoded = encode_client_control(&start).unwrap();
        assert_eq!(decode_client_control(&encoded).unwrap(), start);

        let started = ServerControl::ShareStarted {
            room_id: RoomId(3),
            stream_id: StreamId(8),
            publish_secret: vec![7; 32],
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            extradata: vec![],
        };
        let encoded = encode_server_control(&started);
        assert_eq!(decode_server_control(&encoded).unwrap(), started);

        let available = ServerControl::ShareAvailable {
            room_id: RoomId(3),
            stream_id: StreamId(8),
            user_id: UserId(4),
            sender_name: "Alice".to_string(),
            codec: "avc1.42c01f".to_string(),
            coded_width: 1280,
            coded_height: 720,
            annexb: true,
            extradata: vec![],
            view_secret: vec![9; 32],
        };
        let encoded = encode_server_control(&available);
        assert_eq!(decode_server_control(&encoded).unwrap(), available);
    }

    #[test]
    fn rejects_empty_share_codec() {
        let start = ClientControl::StartShare {
            room_id: RoomId(1),
            codec: "  ".to_string(),
            coded_width: 0,
            coded_height: 0,
            annexb: true,
            extradata: vec![],
        };
        assert!(encode_client_control(&start).is_err());
    }

    #[test]
    fn rejects_oversized_file_chunk() {
        let control = ClientControl::UploadFileChunk {
            transfer_id: FileTransferId(9),
            offset: 0,
            data: vec![0; MAX_FILE_CHUNK_BYTES + 1],
        };
        assert!(encode_client_control(&control).is_err());
    }
}
