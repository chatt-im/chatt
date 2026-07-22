//! Chatt's renderer-independent message formatting grammar and syntax highlighting.

mod link;
pub mod reference;
pub mod tokenizer;

#[cfg(feature = "syntax-highlighting")]
pub mod highlight;

pub use tokenizer::{InlineRanges, Token, TokenKind, inline_ranges, tokenize};
