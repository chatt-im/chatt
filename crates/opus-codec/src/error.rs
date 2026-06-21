//! Error types for Opus codec operations

use crate::bindings::{
    OPUS_ALLOC_FAIL, OPUS_BAD_ARG, OPUS_BUFFER_TOO_SMALL, OPUS_INTERNAL_ERROR, OPUS_INVALID_PACKET,
    OPUS_INVALID_STATE, OPUS_UNIMPLEMENTED,
};
use std::fmt;

/// Convenient result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Opus error variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Bad argument passed to a function.
    BadArg,
    /// Provided buffer was too small.
    BufferTooSmall,
    /// Internal libopus error.
    InternalError,
    /// Packet is invalid or unsupported.
    InvalidPacket,
    /// Feature not implemented.
    Unimplemented,
    /// Invalid state.
    InvalidState,
    /// Memory allocation failure.
    AllocFail,
    /// Unknown error code.
    Unknown(i32),
}

impl Error {
    /// Map a libopus error code to [`Error`].
    #[must_use]
    pub fn from_code(code: i32) -> Self {
        match code {
            OPUS_BAD_ARG => Self::BadArg,
            OPUS_BUFFER_TOO_SMALL => Self::BufferTooSmall,
            OPUS_INTERNAL_ERROR => Self::InternalError,
            OPUS_INVALID_PACKET => Self::InvalidPacket,
            OPUS_UNIMPLEMENTED => Self::Unimplemented,
            OPUS_INVALID_STATE => Self::InvalidState,
            OPUS_ALLOC_FAIL => Self::AllocFail,
            _ => Self::Unknown(code),
        }
    }

    /// Convert [`Error`] back to libopus code.
    #[must_use]
    pub const fn to_code(self) -> i32 {
        match self {
            Self::BadArg => OPUS_BAD_ARG,
            Self::BufferTooSmall => OPUS_BUFFER_TOO_SMALL,
            Self::InternalError => OPUS_INTERNAL_ERROR,
            Self::InvalidPacket => OPUS_INVALID_PACKET,
            Self::Unimplemented => OPUS_UNIMPLEMENTED,
            Self::InvalidState => OPUS_INVALID_STATE,
            Self::AllocFail => OPUS_ALLOC_FAIL,
            Self::Unknown(code) => code,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadArg => write!(f, "Bad arguments passed to Opus function"),
            Self::BufferTooSmall => write!(f, "Buffer too small"),
            Self::InternalError => write!(f, "Internal Opus error"),
            Self::InvalidPacket => write!(f, "Invalid packet"),
            Self::Unimplemented => write!(f, "Unimplemented feature"),
            Self::InvalidState => write!(f, "Invalid state"),
            Self::AllocFail => write!(f, "Memory allocation failed"),
            Self::Unknown(code) => write!(f, "Unknown Opus error code: {code}"),
        }
    }
}

impl std::error::Error for Error {}
