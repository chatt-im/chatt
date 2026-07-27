//! Username validation and the case fold used for server-wide uniqueness.
//!
//! A username is the only thing a reader has to tell two accounts apart, so the
//! rules here are about visual distinctness rather than encoding hygiene: two
//! names that render alike must not be able to coexist. Every accepted code
//! point occupies at least one terminal column, the only permitted whitespace is
//! `U+0020`, and [`fold`] collapses the case and space differences that would
//! otherwise let a second account wear an existing name.
//!
//! Both the client and the server validate through this module so a name the
//! client accepts is one the server will register.

use unicode_width::UnicodeWidthChar;

/// Longest accepted username in bytes.
pub const MAX_USERNAME_BYTES: usize = 64;

/// Widest accepted username in terminal columns.
///
/// Bounded separately from [`MAX_USERNAME_BYTES`] because a name of wide CJK or
/// emoji code points stays well inside the byte cap while overrunning every
/// fixed-width column that displays it.
pub const MAX_USERNAME_COLUMNS: usize = 32;

/// Why a username was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsernameError {
    Empty,
    TooLong,
    TooWide,
    Control,
    Invisible,
    Whitespace,
}

impl UsernameError {
    /// Returns the operator-facing explanation for this rejection.
    pub const fn as_str(self) -> &'static str {
        match self {
            UsernameError::Empty => "username is empty",
            UsernameError::TooLong => "username exceeds 64 bytes",
            UsernameError::TooWide => "username exceeds 32 display columns",
            UsernameError::Control => "username must not contain control characters",
            UsernameError::Invisible => "username must not contain invisible characters",
            UsernameError::Whitespace => "username may only use the ordinary space character",
        }
    }
}

impl std::fmt::Display for UsernameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for UsernameError {}

/// Strips the leading and trailing spaces a username is stored without.
///
/// Deliberately narrower than [`str::trim`]: every other whitespace code point
/// is a rejection rather than something to silently strip, so trimming can
/// never turn an invalid name into a valid one.
///
/// # Examples
///
/// ```
/// assert_eq!(rpc::username::trim("  ada  "), "ada");
/// assert_eq!(rpc::username::trim("\u{00a0}ada"), "\u{00a0}ada");
/// ```
pub fn trim(name: &str) -> &str {
    name.trim_matches(' ')
}

/// Whether `name` is an acceptable username.
///
/// # Examples
///
/// ```
/// assert!(rpc::username::is_valid("Ada Lovelace"));
/// assert!(!rpc::username::is_valid("ada\u{200b}"));
/// ```
pub fn is_valid(name: &str) -> bool {
    validate(name).is_ok()
}

/// Validates `name` after [`trim`], rejecting anything that cannot be told
/// apart on screen from another name.
///
/// Every code point must occupy at least one column, which rules out the whole
/// invisible family in one rule: `Cf` format characters (bidi overrides, the
/// BOM, soft hyphen, tag characters), zero-width spaces and joiners, and
/// combining marks. Combining marks going with them is the deliberate cost of
/// the rule: `e` + `U+0301` is rejected while precomposed `é` is fine, so a
/// decomposed name can never shadow its precomposed twin.
///
/// # Errors
///
/// Returns the [`UsernameError`] naming the first violated rule.
///
/// # Examples
///
/// ```
/// use rpc::username::{UsernameError, validate};
///
/// assert!(validate(" Ada ").is_ok());
/// assert_eq!(validate("ada\u{202e}"), Err(UsernameError::Invisible));
/// assert_eq!(validate("ada\u{00a0}lovelace"), Err(UsernameError::Whitespace));
/// ```
pub fn validate(name: &str) -> Result<(), UsernameError> {
    let name = trim(name);
    if name.is_empty() {
        return Err(UsernameError::Empty);
    }
    if name.len() > MAX_USERNAME_BYTES {
        return Err(UsernameError::TooLong);
    }
    let mut columns = 0usize;
    for ch in name.chars() {
        if ch.is_control() {
            return Err(UsernameError::Control);
        }
        if ch != ' ' && ch.is_whitespace() {
            return Err(UsernameError::Whitespace);
        }
        let Some(width @ 1..) = ch.width() else {
            return Err(UsernameError::Invisible);
        };
        columns += width;
    }
    if columns > MAX_USERNAME_COLUMNS {
        return Err(UsernameError::TooWide);
    }
    Ok(())
}

/// Returns the uniqueness key for `name`: two names sharing a fold are the same
/// username.
///
/// Lowercases, collapses internal space runs to one, and repairs the two places
/// [`str::to_lowercase`] parts ways with case folding — `ß` folds to `ss` and
/// final sigma `ς` folds to `σ` — so those pairs cannot be registered twice.
/// Accepts any input so stored names can be re-keyed without revalidation.
///
/// # Examples
///
/// ```
/// use rpc::username::fold;
///
/// assert_eq!(fold("Ada  Lovelace"), "ada lovelace");
/// assert_eq!(fold("Straße"), fold("STRASSE"));
/// assert_eq!(fold("ΟΔΥΣΣΕΥΣ"), fold("Οδυσσευς"));
/// ```
pub fn fold(name: &str) -> String {
    let name = trim(name);
    let mut out = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.chars() {
        if ch == ' ' {
            pending_space = true;
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        for ch in ch.to_lowercase() {
            match ch {
                'ß' => out.push_str("ss"),
                'ς' => out.push('σ'),
                _ => out.push(ch),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        for name in ["ada", "Ada Lovelace", " ada ", "日本語", "user-1_2.3", "🙂"] {
            assert!(is_valid(name), "rejected {name:?}");
        }
    }

    #[test]
    fn rejects_invisible_code_points() {
        for name in [
            "ada\u{200b}",
            "ada\u{200d}",
            "\u{202e}ada",
            "\u{feff}ada",
            "ada\u{00ad}",
            "ada\u{e0041}",
            "e\u{0301}ada",
        ] {
            assert_eq!(validate(name), Err(UsernameError::Invisible), "{name:?}");
        }
    }

    #[test]
    fn rejects_whitespace_other_than_space() {
        for name in ["ada\u{00a0}lovelace", "ada\u{3000}lovelace", "ada\u{2009}l"] {
            assert_eq!(validate(name), Err(UsernameError::Whitespace), "{name:?}");
        }
        assert_eq!(validate("ada\tlovelace"), Err(UsernameError::Control));
    }

    #[test]
    fn rejects_empty_and_oversized_names() {
        assert_eq!(validate("   "), Err(UsernameError::Empty));
        assert_eq!(
            validate(&"a".repeat(MAX_USERNAME_BYTES + 1)),
            Err(UsernameError::TooLong)
        );
        assert_eq!(
            validate(&"a".repeat(MAX_USERNAME_COLUMNS + 1)),
            Err(UsernameError::TooWide)
        );
        assert_eq!(
            validate(&"日".repeat(MAX_USERNAME_COLUMNS / 2 + 1)),
            Err(UsernameError::TooWide)
        );
    }

    #[test]
    fn fold_collapses_case_space_and_sharp_s() {
        assert_eq!(fold("Ada  Lovelace"), "ada lovelace");
        assert_eq!(fold("  ADA  "), "ada");
        assert_eq!(fold("Straße"), fold("strasse"));
        assert_eq!(fold("ẞ"), "ss");
        assert_eq!(fold("ΟΔΥΣΣΕΥΣ"), fold("Οδυσσευς"));
    }

    #[test]
    fn fold_keeps_visually_distinct_names_apart() {
        assert_ne!(fold("ada"), fold("adam"));
        assert_ne!(fold("ada lovelace"), fold("adalovelace"));
    }
}
