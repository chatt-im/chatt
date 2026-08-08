//! Private, local RPC between the Chatt daemon and native renderers.
//!
//! This Unix-only protocol is versioned independently from the remote server
//! protocol. It contains presentation-safe projections, never application
//! implementation state or daemon filesystem paths.

pub mod appearance;
pub mod attachment_stream;
pub mod bulk;
pub mod frame;
pub mod identity;
pub mod ids {
    //! Resource identifiers visible to native renderers.

    pub use chatt_ids::{FileTransferId, MessageId, RoomId, StreamId, UserId};
}
pub mod model;
pub mod settings;
#[cfg(unix)]
pub mod unix;

pub use chatt_video::{bitstream, video};

mod framing;
mod recv_buffer;

/// This protocol is private to a daemon and the renderers it ships with, and
/// has no external users, so the number is not a compatibility record — a
/// daemon and a renderer either come from the same tree or refuse to speak.
/// Bump it only if something ever has to negotiate across versions for real.
pub const PROTOCOL_MIN_VERSION: u16 = 0;
pub const PROTOCOL_MAX_VERSION: u16 = 0;
pub const MAX_BOOTSTRAP_BYTES: usize = 64 * 1024;
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ROOM_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_STRING_BYTES: usize = 16 * 1024;
pub const MAX_MESSAGE_BODY_BYTES: usize = 16 * 1024;
pub const DEFAULT_UPLOAD_LIMIT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_HISTORY_REQUEST_MESSAGES: u16 = 500;
pub const MAX_ROOMS: usize = 4096;
pub const MAX_SERVERS: usize = 1024;
pub const MAX_MESSAGES: usize = 2000;
pub const MAX_PARTICIPANTS: usize = 4096;
/// The largest call this protocol can carry. Set to the server's own client cap
/// (`server::MAX_CLIENTS`), because nothing between the two admits fewer: the
/// voice relay is sized for the same number and there is no per-room voice
/// admission limit. A tighter bound here would be a limit renderers impose on
/// calls the rest of the system considers valid.
pub const MAX_VOICE_MEMBERS: usize = 1024;
/// The server's username bound (`rpc::username::MAX_USERNAME_BYTES`), restated
/// because this crate shares only resource IDs with the remote protocol.
pub const MAX_USERNAME_BYTES: usize = 64;
pub const MAX_TRANSFERS: usize = 32;
pub const MAX_LIVE_SHARES: usize = 64;
pub const MAX_COMMANDS: usize = 128;
pub const MAX_COMMAND_CANDIDATES: usize = 4096;
pub const MAX_COMMAND_OUTPUT_LINES: usize = 64;
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_FDS_PER_FRAME: usize = 4;
pub const MAX_RPC_CLIENTS: usize = 16;
pub const MAX_OUTSTANDING_REQUESTS: usize = 128;
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CONCURRENT_TRANSFERS: usize = 4;
pub const MAX_CONCURRENT_ATTACHMENT_STREAMS: usize = 8;
pub const MAX_ATTACHMENT_READ_BYTES: usize = 256 * 1024;
pub const MAX_OUTPUT_VOLUME_PERCENT: f32 = 130.0;
pub const MAX_SETTINGS_DIAGNOSTICS: usize = 64;
pub const MAX_SETTINGS_SECTIONS: usize = 32;
pub const MAX_SETTINGS_FIELDS: usize = 256;
pub const MAX_SETTINGS_CHOICES: usize = 64;
pub const MAX_SETTINGS_LIST_ITEMS: usize = 64;
pub const MAX_SETTINGS_CHANGES: usize = 256;
pub const MAX_APPEARANCE_BYTES: usize = 64 * 1024;
pub const MAX_IDENTITY_WORDS: usize = 32;
pub const MAX_IDENTITY_KEY_GROUPS: usize = 32;
