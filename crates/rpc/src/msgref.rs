//! Compact textual references to chat messages.
//!
//! The codec lives in `chatt-message-format` so every plaintext renderer uses
//! exactly the same reference identity and spelling.

pub use chatt_message_format::reference::{
    MAX_CODE_LEN, MIN_CODE_LEN, MessageRef, REF_PREFIX, is_ref_char,
};
