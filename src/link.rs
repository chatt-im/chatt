//! Detection of `http`/`https` URLs in plain message text.
//!
//! Chat bodies are rendered as Markdown, but bare URLs are not markup, so the
//! renderer and click handler need their own scan. [`find_urls`] returns the
//! byte ranges of every URL in a body, which the chat buffer caches per message
//! for underline styling and click hit-testing.

use std::ops::Range;

/// Finds every `http://` / `https://` URL in `body`, returning their byte ranges.
///
/// A URL runs from its scheme until the first byte that cannot belong to a URL
/// (ASCII whitespace, a control byte, or one of `<>"` `` ` ``). Trailing
/// sentence punctuation is trimmed, and a trailing `)` is dropped only when the
/// URL holds no matching `(`, so `(see https://x.com/a).` yields `https://x.com/a`.
pub fn find_urls(body: &str) -> Vec<Range<u32>> {
    let bytes = body.as_bytes();
    let mut ranges: Vec<Range<u32>> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let Some(scheme_len) = scheme_at(bytes, i) else {
            i += 1;
            continue;
        };
        let mut end = i + scheme_len;
        while end < bytes.len() && is_url_byte(bytes[end]) {
            end += 1;
        }
        end = trim_trailing(bytes, i, end);
        // A scheme with no host (e.g. a lone "https://") is not a usable URL.
        if end > i + scheme_len {
            ranges.push(i as u32..end as u32);
        }
        i = end.max(i + scheme_len);
    }
    ranges
}

/// Returns the scheme length (`7` for `http://`, `8` for `https://`) when one
/// starts at `pos`, matching the scheme case-insensitively.
fn scheme_at(bytes: &[u8], pos: usize) -> Option<usize> {
    for scheme in ["https://".as_bytes(), "http://".as_bytes()] {
        let end = pos + scheme.len();
        if end <= bytes.len() && bytes[pos..end].eq_ignore_ascii_case(scheme) {
            return Some(scheme.len());
        }
    }
    None
}

/// Whether `byte` can appear inside a URL. Stops at whitespace, controls, and
/// the delimiters commonly used to wrap a URL in prose.
fn is_url_byte(byte: u8) -> bool {
    !byte.is_ascii_whitespace() && !byte.is_ascii_control() && !matches!(byte, b'<' | b'>' | b'"' | b'`')
}

/// Trims trailing punctuation from `[start, end)`, dropping a closing paren only
/// when it is unbalanced within the URL.
fn trim_trailing(bytes: &[u8], start: usize, mut end: usize) -> usize {
    while end > start {
        let last = bytes[end - 1];
        let trim = match last {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' => true,
            b')' => bytes[start..end].iter().filter(|&&b| b == b'(').count()
                < bytes[start..end].iter().filter(|&&b| b == b')').count(),
            _ => false,
        };
        if !trim {
            break;
        }
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(body: &str) -> Vec<&str> {
        find_urls(body)
            .into_iter()
            .map(|r| &body[r.start as usize..r.end as usize])
            .collect()
    }

    #[test]
    fn bare_url() {
        assert_eq!(urls("https://example.com"), ["https://example.com"]);
        assert_eq!(urls("http://a.b/c?d=e#f"), ["http://a.b/c?d=e#f"]);
    }

    #[test]
    fn trailing_sentence_punctuation_trimmed() {
        assert_eq!(urls("see https://x.com/a."), ["https://x.com/a"]);
        assert_eq!(urls("go to https://x.com, now"), ["https://x.com"]);
    }

    #[test]
    fn parenthesized_url() {
        assert_eq!(urls("(see https://x.com/a)."), ["https://x.com/a"]);
        // A paren that is part of the URL is kept when balanced.
        assert_eq!(
            urls("https://en.wikipedia.org/wiki/Foo_(bar)"),
            ["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
    }

    #[test]
    fn markdown_link_url_detected() {
        assert_eq!(urls("[text](https://x.com/a)"), ["https://x.com/a"]);
    }

    #[test]
    fn multiple_and_none() {
        assert_eq!(
            urls("a https://one.com b http://two.com c"),
            ["https://one.com", "http://two.com"]
        );
        assert!(urls("no links here, just ftp://nope and text").is_empty());
    }

    #[test]
    fn scheme_without_host_ignored() {
        assert!(urls("https:// ").is_empty());
        assert!(urls("http://").is_empty());
    }

    #[test]
    fn case_insensitive_scheme() {
        assert_eq!(urls("HTTPS://Example.com"), ["HTTPS://Example.com"]);
    }
}
