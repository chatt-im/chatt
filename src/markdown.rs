//! Chatt's small Markdown subset tokenizer.
//!
//! The tokenizer is intentionally shallow: it emits balanced block and inline
//! delimiter events plus source byte ranges. Renderers keep using the original
//! message text for display, copying, and link hit-testing.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    ParagraphStart,
    ParagraphEnd,
    HeaderStart,
    HeaderEnd,
    UnorderedListStart,
    OrderedListStart,
    ListEnd,
    ListItemStart {
        marker: Range<u32>,
    },
    ListItemEnd,
    CodeBlock {
        lang: Option<Range<u32>>,
        content: Range<u32>,
    },
    Text,
    BoldStart,
    BoldEnd,
    ItalicStart,
    ItalicEnd,
    InlineCode,
    Url,
    HardBreak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Block {
    Code {
        lang: Option<Range<usize>>,
        content: Range<usize>,
        range: Range<usize>,
        next: usize,
    },
    UnclosedFence,
    Header {
        marker: Range<usize>,
        text: Range<usize>,
        next: usize,
    },
    ListItem {
        kind: ListKind,
        marker: Range<usize>,
        text: Range<usize>,
        next: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmphasisKind {
    Bold,
    Italic,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EmphasisSpan {
    kind: EmphasisKind,
    open: Range<usize>,
    content: Range<usize>,
    close: Range<usize>,
}

/// Tokenizes `source` into `out`, replacing any previous contents.
pub fn tokenize(source: &str, out: &mut Vec<Token>) {
    out.clear();
    let bytes = source.as_bytes();
    let mut pos = 0usize;
    let mut open_list = None;

    while pos < bytes.len() {
        if is_blank_line(bytes, pos) {
            close_list(out, &mut open_list, pos);
            pos = line_bounds(bytes, pos).2;
            continue;
        }

        match block_at(bytes, pos) {
            Some(Block::Code {
                lang,
                content,
                range,
                next,
            }) => {
                close_list(out, &mut open_list, pos);
                push(
                    out,
                    TokenKind::CodeBlock {
                        lang: lang.map(to_u32_range),
                        content: to_u32_range(content),
                    },
                    range,
                );
                pos = next;
            }
            Some(Block::Header { marker, text, next }) => {
                close_list(out, &mut open_list, pos);
                push(out, TokenKind::HeaderStart, marker);
                tokenize_inline(source, text, out);
                push_empty(out, TokenKind::HeaderEnd, next);
                pos = next;
            }
            Some(Block::ListItem {
                kind,
                marker,
                text,
                next,
            }) => {
                if open_list != Some(kind) {
                    close_list(out, &mut open_list, pos);
                    push_empty(
                        out,
                        match kind {
                            ListKind::Unordered => TokenKind::UnorderedListStart,
                            ListKind::Ordered => TokenKind::OrderedListStart,
                        },
                        pos,
                    );
                    open_list = Some(kind);
                }
                push(
                    out,
                    TokenKind::ListItemStart {
                        marker: to_u32_range(marker.clone()),
                    },
                    marker,
                );
                tokenize_inline(source, text, out);
                push_empty(out, TokenKind::ListItemEnd, next);
                pos = next;
            }
            Some(Block::UnclosedFence) | None => {
                close_list(out, &mut open_list, pos);
                pos = tokenize_paragraph(source, pos, out);
            }
        }
    }

    close_list(out, &mut open_list, pos);
}

/// Returns the URL ranges produced by the tokenizer.
pub fn link_ranges(source: &str) -> Vec<Range<u32>> {
    let mut tokens = Vec::new();
    tokenize(source, &mut tokens);
    tokens
        .into_iter()
        .filter_map(|token| (token.kind == TokenKind::Url).then_some(token.range))
        .collect()
}

fn close_list(out: &mut Vec<Token>, open_list: &mut Option<ListKind>, pos: usize) {
    if open_list.take().is_some() {
        push_empty(out, TokenKind::ListEnd, pos);
    }
}

fn tokenize_paragraph(source: &str, pos: usize, out: &mut Vec<Token>) -> usize {
    let bytes = source.as_bytes();
    let mut line_pos = pos;
    let mut suppress_blocks = false;

    push_empty(out, TokenKind::ParagraphStart, pos);
    loop {
        let (content_end, _, next) = line_bounds(bytes, line_pos);
        tokenize_inline(source, line_pos..content_end, out);
        if matches!(block_at(bytes, line_pos), Some(Block::UnclosedFence)) {
            suppress_blocks = true;
        }
        if next >= bytes.len() || is_blank_line(bytes, next) {
            line_pos = next;
            break;
        }
        if !suppress_blocks
            && matches!(block_at(bytes, next), Some(block) if block != Block::UnclosedFence)
        {
            line_pos = next;
            break;
        }
        push(out, TokenKind::HardBreak, content_end..next);
        line_pos = next;
    }
    push_empty(out, TokenKind::ParagraphEnd, line_pos);
    line_pos
}

fn tokenize_inline(source: &str, range: Range<usize>, out: &mut Vec<Token>) {
    let bytes = source.as_bytes();
    let mut cursor = range.start;
    let mut pos = range.start;

    while pos < range.end {
        if is_single_backtick(bytes, pos, range.clone())
            && let Some(close) = find_inline_code_close(source, pos, range.end)
        {
            parse_emphasis(source, cursor..pos, out, true, true);
            push(out, TokenKind::InlineCode, pos + 1..close);
            cursor = close + 1;
            pos = cursor;
            continue;
        }
        pos += 1;
    }

    parse_emphasis(source, cursor..range.end, out, true, true);
}

fn parse_emphasis(
    source: &str,
    range: Range<usize>,
    out: &mut Vec<Token>,
    allow_bold: bool,
    allow_italic: bool,
) {
    let mut cursor = range.start;
    while cursor < range.end {
        let Some(span) = find_next_emphasis(source, cursor..range.end, allow_bold, allow_italic)
        else {
            emit_text_with_urls(source, cursor..range.end, out);
            return;
        };
        emit_text_with_urls(source, cursor..span.open.start, out);
        match span.kind {
            EmphasisKind::Bold => {
                push(out, TokenKind::BoldStart, span.open.clone());
                parse_emphasis(source, span.content, out, false, allow_italic);
                push(out, TokenKind::BoldEnd, span.close.clone());
            }
            EmphasisKind::Italic => {
                push(out, TokenKind::ItalicStart, span.open.clone());
                parse_emphasis(source, span.content, out, allow_bold, false);
                push(out, TokenKind::ItalicEnd, span.close.clone());
            }
            EmphasisKind::Both => {
                push(
                    out,
                    TokenKind::ItalicStart,
                    span.open.start..span.open.start + 1,
                );
                push(
                    out,
                    TokenKind::BoldStart,
                    span.open.start + 1..span.open.end,
                );
                parse_emphasis(source, span.content, out, false, false);
                push(
                    out,
                    TokenKind::BoldEnd,
                    span.close.start..span.close.start + 2,
                );
                push(
                    out,
                    TokenKind::ItalicEnd,
                    span.close.start + 2..span.close.end,
                );
            }
        }
        cursor = span.close.end;
    }
}

fn emit_text_with_urls(source: &str, range: Range<usize>, out: &mut Vec<Token>) {
    if range.is_empty() {
        return;
    }
    let mut cursor = range.start;
    for url in crate::link::find_urls(&source[range.clone()]) {
        let start = range.start + url.start as usize;
        let end = range.start + url.end as usize;
        if cursor < start {
            push(out, TokenKind::Text, cursor..start);
        }
        push(out, TokenKind::Url, start..end);
        cursor = end;
    }
    if cursor < range.end {
        push(out, TokenKind::Text, cursor..range.end);
    }
}

fn find_inline_code_close(source: &str, open: usize, end: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut pos = open + 1;
    while pos < end {
        if is_single_backtick(bytes, pos, open + 1..end)
            && content_has_non_ws_edges(source, open + 1..pos)
        {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

fn find_next_emphasis(
    source: &str,
    range: Range<usize>,
    allow_bold: bool,
    allow_italic: bool,
) -> Option<EmphasisSpan> {
    let bytes = source.as_bytes();
    let mut pos = range.start;
    while pos < range.end {
        if allow_bold
            && allow_italic
            && exact_marker(bytes, pos, range.clone(), b'*', 3)
            && let Some(span) = find_emphasis_close(source, pos, range.end, EmphasisKind::Both)
        {
            return Some(span);
        }
        if allow_bold
            && exact_marker(bytes, pos, range.clone(), b'*', 2)
            && let Some(span) = find_emphasis_close(source, pos, range.end, EmphasisKind::Bold)
        {
            return Some(span);
        }
        if allow_italic
            && exact_marker(bytes, pos, range.clone(), b'*', 1)
            && let Some(span) = find_emphasis_close(source, pos, range.end, EmphasisKind::Italic)
        {
            return Some(span);
        }
        pos += 1;
    }
    None
}

fn find_emphasis_close(
    source: &str,
    open: usize,
    end: usize,
    kind: EmphasisKind,
) -> Option<EmphasisSpan> {
    let bytes = source.as_bytes();
    let len = match kind {
        EmphasisKind::Italic => 1,
        EmphasisKind::Bold => 2,
        EmphasisKind::Both => 3,
    };
    let mut pos = open + len;
    while pos + len <= end {
        if exact_marker(bytes, pos, open + len..end, b'*', len)
            && content_has_non_ws_edges(source, open + len..pos)
        {
            return Some(EmphasisSpan {
                kind,
                open: open..open + len,
                content: open + len..pos,
                close: pos..pos + len,
            });
        }
        pos += 1;
    }
    None
}

fn block_at(bytes: &[u8], pos: usize) -> Option<Block> {
    let (content_end, _, next) = line_bounds(bytes, pos);
    let line = &bytes[pos..content_end];

    if let Some(lang) = fence_opener(bytes, pos, content_end) {
        let content_start = next;
        let Some((closing_start, closing_next)) = find_closing_fence(bytes, next) else {
            return Some(Block::UnclosedFence);
        };
        let mut code_end = closing_start;
        if code_end > content_start && bytes[code_end - 1] == b'\n' {
            code_end -= 1;
            if code_end > content_start && bytes[code_end - 1] == b'\r' {
                code_end -= 1;
            }
        }
        return Some(Block::Code {
            lang,
            content: content_start..code_end,
            range: pos..closing_next,
            next: closing_next,
        });
    }

    if line.starts_with(b"# ") {
        return Some(Block::Header {
            marker: pos..pos + 2,
            text: pos + 2..content_end,
            next,
        });
    }

    if line.starts_with(b"- ") || line.starts_with(b"* ") {
        return Some(Block::ListItem {
            kind: ListKind::Unordered,
            marker: pos..pos + 2,
            text: pos + 2..content_end,
            next,
        });
    }

    if let Some(marker_len) = ordered_marker_len(line) {
        return Some(Block::ListItem {
            kind: ListKind::Ordered,
            marker: pos..pos + marker_len,
            text: pos + marker_len..content_end,
            next,
        });
    }

    None
}

fn fence_opener(bytes: &[u8], pos: usize, content_end: usize) -> Option<Option<Range<usize>>> {
    let line = &bytes[pos..content_end];
    if line == b"```" {
        return Some(None);
    }
    let lang = line.strip_prefix(b"```")?;
    (!lang.is_empty() && lang.iter().all(|b| b.is_ascii_alphanumeric()))
        .then_some(Some(pos + 3..content_end))
}

fn find_closing_fence(bytes: &[u8], mut pos: usize) -> Option<(usize, usize)> {
    while pos < bytes.len() {
        let (content_end, _, next) = line_bounds(bytes, pos);
        if &bytes[pos..content_end] == b"```" {
            return Some((pos, next));
        }
        pos = next;
    }
    None
}

fn ordered_marker_len(line: &[u8]) -> Option<usize> {
    let digits = line.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    (line.get(digits) == Some(&b'.') && line.get(digits + 1) == Some(&b' ')).then_some(digits + 2)
}

fn line_bounds(bytes: &[u8], pos: usize) -> (usize, usize, usize) {
    let mut end = pos;
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    let next = if end < bytes.len() { end + 1 } else { end };
    let content_end = if end > pos && bytes[end - 1] == b'\r' {
        end - 1
    } else {
        end
    };
    (content_end, end, next)
}

fn is_blank_line(bytes: &[u8], pos: usize) -> bool {
    let (content_end, _, _) = line_bounds(bytes, pos);
    bytes[pos..content_end]
        .iter()
        .all(|b| matches!(b, b' ' | b'\t'))
}

fn is_single_backtick(bytes: &[u8], pos: usize, bounds: Range<usize>) -> bool {
    bytes.get(pos) == Some(&b'`')
        && (pos == bounds.start || bytes.get(pos - 1) != Some(&b'`'))
        && (pos + 1 >= bounds.end || bytes.get(pos + 1) != Some(&b'`'))
}

fn exact_marker(bytes: &[u8], pos: usize, bounds: Range<usize>, marker: u8, len: usize) -> bool {
    pos + len <= bounds.end
        && bytes[pos..pos + len].iter().all(|&b| b == marker)
        && (pos == bounds.start || bytes.get(pos - 1) != Some(&marker))
        && (pos + len >= bounds.end || bytes.get(pos + len) != Some(&marker))
}

fn content_has_non_ws_edges(source: &str, range: Range<usize>) -> bool {
    let text = &source[range];
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    !first.is_whitespace() && !text.chars().next_back().is_some_and(char::is_whitespace)
}

fn push(out: &mut Vec<Token>, kind: TokenKind, range: Range<usize>) {
    out.push(Token {
        kind,
        range: to_u32_range(range),
    });
}

fn push_empty(out: &mut Vec<Token>, kind: TokenKind, pos: usize) {
    push(out, kind, pos..pos);
}

fn to_u32_range(range: Range<usize>) -> Range<u32> {
    range.start as u32..range.end as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(source: &str) -> Vec<(TokenKind, Range<u32>)> {
        let mut tokens = Vec::new();
        tokenize(source, &mut tokens);
        tokens
            .into_iter()
            .map(|token| (token.kind, token.range))
            .collect()
    }

    fn texts<'a>(source: &'a str, tokens: &'a [(TokenKind, Range<u32>)]) -> Vec<&'a str> {
        tokens
            .iter()
            .filter_map(|(kind, range)| {
                matches!(
                    kind,
                    TokenKind::Text | TokenKind::Url | TokenKind::InlineCode
                )
                .then(|| &source[range.start as usize..range.end as usize])
            })
            .collect()
    }

    #[test]
    fn recognizes_only_single_hash_header() {
        assert_eq!(
            pairs("# Hello"),
            vec![
                (TokenKind::HeaderStart, 0..2),
                (TokenKind::Text, 2..7),
                (TokenKind::HeaderEnd, 7..7),
            ]
        );
        assert_eq!(
            texts("## Nope", &pairs("## Nope")),
            vec!["## Nope"],
            "other heading levels are plain text"
        );
        assert_eq!(texts("#Nope", &pairs("#Nope")), vec!["#Nope"]);
    }

    #[test]
    fn recognizes_flat_lists_and_rejects_other_markers() {
        let source = "- one\n* two\n1. three\n2) nope\n + nope\n+ nope";
        let tokens = pairs(source);
        assert!(
            tokens
                .iter()
                .any(|(kind, _)| *kind == TokenKind::UnorderedListStart)
        );
        assert!(
            tokens
                .iter()
                .any(|(kind, _)| *kind == TokenKind::OrderedListStart)
        );
        assert_eq!(
            texts(source, &tokens),
            vec!["one", "two", "three", "2) nope", " + nope", "+ nope"]
        );
    }

    #[test]
    fn exact_fenced_code_block() {
        assert_eq!(
            pairs("```rust\nfn\n```"),
            vec![(
                TokenKind::CodeBlock {
                    lang: Some(3..7),
                    content: 8..10,
                },
                0..14,
            )]
        );
        assert_eq!(
            pairs("```\ncode\n```"),
            vec![(
                TokenKind::CodeBlock {
                    lang: None,
                    content: 4..8,
                },
                0..12,
            )]
        );
    }

    #[test]
    fn rejects_broad_fence_rules() {
        for source in [" ~~~\ncode\n~~~", "````\ncode\n````", "``` rust\ncode\n```"] {
            let tokens = pairs(source);
            assert!(
                tokens
                    .iter()
                    .all(|(kind, _)| !matches!(kind, TokenKind::CodeBlock { .. }))
            );
        }
    }

    #[test]
    fn unclosed_fence_is_plain_text() {
        let source = "```\n# still text";
        let tokens = pairs(source);
        assert!(tokens.iter().all(|(kind, _)| !matches!(
            kind,
            TokenKind::CodeBlock { .. } | TokenKind::HeaderStart
        )));
        assert_eq!(texts(source, &tokens), vec!["```", "# still text"]);
    }

    #[test]
    fn inline_code_wins_before_other_inline_syntax() {
        let source = "`**x**` **y**";
        assert_eq!(
            pairs(source),
            vec![
                (TokenKind::ParagraphStart, 0..0),
                (TokenKind::InlineCode, 1..6),
                (TokenKind::Text, 7..8),
                (TokenKind::BoldStart, 8..10),
                (TokenKind::Text, 10..11),
                (TokenKind::BoldEnd, 11..13),
                (TokenKind::ParagraphEnd, 13..13),
            ]
        );
    }

    #[test]
    fn bold_italic_and_triple_star() {
        let source = "**bold** *it* ***both***";
        let tokens = pairs(source);
        let kinds = tokens
            .iter()
            .map(|(kind, _)| kind.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![
                TokenKind::ParagraphStart,
                TokenKind::BoldStart,
                TokenKind::Text,
                TokenKind::BoldEnd,
                TokenKind::Text,
                TokenKind::ItalicStart,
                TokenKind::Text,
                TokenKind::ItalicEnd,
                TokenKind::Text,
                TokenKind::ItalicStart,
                TokenKind::BoldStart,
                TokenKind::Text,
                TokenKind::BoldEnd,
                TokenKind::ItalicEnd,
                TokenKind::ParagraphEnd,
            ]
        );
        assert_eq!(texts(source, &tokens), vec!["bold", " ", "it", " ", "both"]);
    }

    #[test]
    fn whitespace_inside_delimiters_rejects_inline_formatting() {
        for source in ["** bad**", "**bad **", "x * bad*", "` bad`"] {
            let tokens = pairs(source);
            assert!(tokens.iter().all(|(kind, _)| matches!(
                kind,
                TokenKind::ParagraphStart | TokenKind::ParagraphEnd | TokenKind::Text
            )));
        }
    }

    #[test]
    fn raw_urls_are_linkified_outside_code() {
        let source = "see https://example.com and `https://code.test`";
        let tokens = pairs(source);
        assert_eq!(
            tokens
                .iter()
                .filter(|(kind, _)| *kind == TokenKind::Url)
                .map(|(_, range)| &source[range.start as usize..range.end as usize])
                .collect::<Vec<_>>(),
            vec!["https://example.com"]
        );
    }

    #[test]
    fn markdown_links_images_and_html_stay_text() {
        let source = "[label](https://x.test) ![alt](https://i.test) <b>raw</b>";
        let tokens = pairs(source);
        assert!(
            tokens
                .iter()
                .all(|(kind, _)| !matches!(kind, TokenKind::BoldStart | TokenKind::ItalicStart))
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|(kind, _)| *kind == TokenKind::Url)
                .map(|(_, range)| &source[range.start as usize..range.end as usize])
                .collect::<Vec<_>>(),
            vec!["https://x.test", "https://i.test"]
        );
        assert!(texts(source, &tokens).join("").contains("<b>raw</b>"));
    }
}
