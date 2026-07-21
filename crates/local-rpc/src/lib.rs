//! Private, local RPC between the Chatt daemon and native renderers.
//!
//! This Unix-only protocol is versioned independently from the remote server
//! protocol. It contains presentation-safe projections, never application
//! implementation state or daemon filesystem paths.

pub mod bulk;
pub mod frame;
pub mod ids {
    //! Resource identifiers visible to native renderers.

    pub use chatt_ids::{FileTransferId, MessageId, RoomId, StreamId, UserId};
}
pub mod model;
#[cfg(unix)]
pub mod unix;

pub use chatt_video::{bitstream, video};

mod framing;
mod recv_buffer;

pub const PROTOCOL_MIN_VERSION: u16 = 4;
pub const PROTOCOL_MAX_VERSION: u16 = 4;
pub const MAX_BOOTSTRAP_BYTES: usize = 64 * 1024;
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ROOM_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_STRING_BYTES: usize = 16 * 1024;
pub const MAX_MESSAGE_BODY_BYTES: usize = 8 * 1024;
pub const DEFAULT_UPLOAD_LIMIT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_HISTORY_REQUEST_MESSAGES: u16 = 500;
pub const MAX_ROOMS: usize = 4096;
pub const MAX_MESSAGES: usize = 2000;
pub const MAX_PARTICIPANTS: usize = 4096;
pub const MAX_TRANSFERS: usize = 32;
pub const MAX_LIVE_SHARES: usize = 64;
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
pub const MAX_FDS_PER_FRAME: usize = 4;
pub const MAX_RPC_CLIENTS: usize = 16;
pub const MAX_OUTSTANDING_REQUESTS: usize = 128;
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CONCURRENT_TRANSFERS: usize = 4;
pub const MAX_OUTPUT_VOLUME_PERCENT: f32 = 130.0;
