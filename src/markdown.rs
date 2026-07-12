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
    ListItemStart { marker: Range<u32> },
    ListItemEnd,
    CodeBlockStart { lang: Option<Range<u32>> },
    CodeBlockLine,
    CodeBlockEnd,
    BlockQuoteStart,
    BlockQuoteEnd,
    Text,
    BoldStart,
    BoldEnd,
    ItalicStart,
    ItalicEnd,
    InlineCode,
    Url,
    MessageRef,
    HardBreak,
    BlankLine,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Fence {
    Bare { start: usize, standalone: bool },
    WithLanguage { start: usize, lang: Range<usize> },
}

impl Fence {
    fn start(&self) -> usize {
        match self {
            Self::Bare { start, .. } | Self::WithLanguage { start, .. } => *start,
        }
    }

    fn is_standalone_bare(&self) -> bool {
        matches!(
            self,
            Self::Bare {
                standalone: true,
                ..
            }
        )
    }
}

enum LineKind {
    Blank,
    Fence(Fence),
    Header,
    ListItem(ListKind, usize),
    Text,
}

struct SourceLine {
    start: usize,
    content_end: usize,
    next: usize,
    prefix_ends: Range<usize>,
    kind: LineKind,
    closing_fence: Option<usize>,
}

impl SourceLine {
    fn quote_depth(&self) -> usize {
        self.prefix_ends.len()
    }

    fn content_start(&self, prefix_ends: &[usize]) -> usize {
        self.content_at_depth(prefix_ends, self.quote_depth())
    }

    fn content_at_depth(&self, prefix_ends: &[usize], depth: usize) -> usize {
        if depth == 0 {
            self.start
        } else {
            prefix_ends[self.prefix_ends.start + depth - 1]
        }
    }
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
///
/// Block parsing has three linear stages: [`scan_lines`] consumes the raw block
/// structure and quote markers once, [`match_fences`] pairs fences using only
/// line metadata, then this function emits block events from those indexed
/// lines. Only inline-capable line contents are subsequently read by the inline
/// tokenizer.
pub fn tokenize(source: &str, out: &mut Vec<Token>) {
    out.clear();
    let bytes = source.as_bytes();
    let (mut lines, prefix_ends) = scan_lines(bytes);
    match_fences(&mut lines);

    let mut quote_depth = 0usize;
    let mut paragraph_end = None;
    let mut suppress_blocks = false;
    let mut open_list = None;
    let mut line_index = 0usize;

    while line_index < lines.len() {
        let line = &lines[line_index];
        let depth = line.quote_depth();
        let content_start = line.content_start(&prefix_ends);

        if depth != quote_depth {
            close_paragraph(out, &mut paragraph_end, &mut suppress_blocks, line.start);
            close_list(out, &mut open_list, line.start);
            change_quote_depth(out, &mut quote_depth, depth, line.start);
        }

        if matches!(line.kind, LineKind::Blank) {
            close_paragraph(out, &mut paragraph_end, &mut suppress_blocks, line.start);
            close_list(out, &mut open_list, line.start);
            let blank_start = line.content_start(&prefix_ends);
            let mut next_index = line_index + 1;
            while lines
                .get(next_index)
                .is_some_and(|line| matches!(line.kind, LineKind::Blank))
            {
                next_index += 1;
            }
            if line_index > 0 && next_index < lines.len() {
                let blank_end = lines[next_index - 1].content_end;
                push(out, TokenKind::BlankLine, blank_start..blank_end);
            }
            for blank in &lines[line_index + 1..next_index] {
                change_quote_depth(out, &mut quote_depth, blank.quote_depth(), blank.start);
            }
            line_index = next_index;
            continue;
        }

        if !suppress_blocks && let Some(closing_index) = line.closing_fence {
            let fence = match &line.kind {
                LineKind::Fence(fence) => fence,
                _ => unreachable!("only fence openers have a closing fence"),
            };
            let fence_start = fence.start();
            close_list(out, &mut open_list, line.start);
            if content_start < fence_start {
                if let Some(previous_end) = paragraph_end {
                    push(out, TokenKind::HardBreak, previous_end..line.start);
                } else {
                    push_empty(out, TokenKind::ParagraphStart, line.start);
                }
                tokenize_inline(source, content_start..fence_start, out);
                paragraph_end = Some(fence_start);
            }
            close_paragraph(out, &mut paragraph_end, &mut suppress_blocks, fence_start);
            let lang = match fence {
                Fence::Bare { .. } => None,
                Fence::WithLanguage { lang, .. } => Some(to_u32_range(lang.clone())),
            };
            push(
                out,
                TokenKind::CodeBlockStart { lang },
                fence_start..line.content_end,
            );
            for code_line in &lines[line_index + 1..closing_index] {
                let start = code_line.content_at_depth(&prefix_ends, depth);
                push(out, TokenKind::CodeBlockLine, start..code_line.content_end);
            }
            let closing = &lines[closing_index];
            push(
                out,
                TokenKind::CodeBlockEnd,
                closing.content_start(&prefix_ends)..closing.content_end,
            );
            line_index = closing_index + 1;
            continue;
        }

        if !suppress_blocks && matches!(line.kind, LineKind::Header) {
            close_paragraph(out, &mut paragraph_end, &mut suppress_blocks, line.start);
            close_list(out, &mut open_list, line.start);
            push(
                out,
                TokenKind::HeaderStart,
                content_start..content_start + 2,
            );
            tokenize_inline(source, content_start + 2..line.content_end, out);
            push_empty(out, TokenKind::HeaderEnd, line.next);
            line_index += 1;
            continue;
        }

        let list_item = if suppress_blocks {
            None
        } else {
            match line.kind {
                LineKind::ListItem(kind, marker_len) => Some((kind, marker_len)),
                _ => None,
            }
        };
        if let Some((kind, marker_len)) = list_item {
            close_paragraph(out, &mut paragraph_end, &mut suppress_blocks, line.start);
            if open_list != Some(kind) {
                close_list(out, &mut open_list, line.start);
                push_empty(
                    out,
                    match kind {
                        ListKind::Unordered => TokenKind::UnorderedListStart,
                        ListKind::Ordered => TokenKind::OrderedListStart,
                    },
                    line.start,
                );
                open_list = Some(kind);
            }
            let marker = content_start..content_start + marker_len;
            push(
                out,
                TokenKind::ListItemStart {
                    marker: to_u32_range(marker.clone()),
                },
                marker,
            );
            tokenize_inline(source, content_start + marker_len..line.content_end, out);
            push_empty(out, TokenKind::ListItemEnd, line.next);
            line_index += 1;
            continue;
        }

        close_list(out, &mut open_list, line.start);
        if let Some(previous_end) = paragraph_end {
            push(out, TokenKind::HardBreak, previous_end..line.start);
        } else {
            push_empty(out, TokenKind::ParagraphStart, line.start);
        }
        tokenize_inline(source, content_start..line.content_end, out);
        paragraph_end = Some(line.content_end);
        suppress_blocks |= matches!(line.kind, LineKind::Fence(_));
        line_index += 1;
    }

    close_paragraph(out, &mut paragraph_end, &mut suppress_blocks, bytes.len());
    close_list(out, &mut open_list, bytes.len());
    change_quote_depth(out, &mut quote_depth, 0, bytes.len());
}

/// Scans physical lines and all quote-prefix boundaries exactly once. The
/// flattened prefix table lets later block parsers address content at any quote
/// depth without walking the markers again.
fn scan_lines(bytes: &[u8]) -> (Vec<SourceLine>, Vec<usize>) {
    let mut lines = Vec::new();
    let mut prefix_ends = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let prefix_start = prefix_ends.len();
        let mut content_start = pos;
        while bytes.get(content_start) == Some(&b'>') {
            content_start += 1;
            if bytes.get(content_start) == Some(&b' ') {
                content_start += 1;
            }
            prefix_ends.push(content_start);
        }
        let mut end = content_start;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let next = if end < bytes.len() { end + 1 } else { end };
        let content_end = if end > pos && bytes[end - 1] == b'\r' {
            end - 1
        } else {
            end
        };
        let kind = classify_line(bytes, content_start, content_end);
        lines.push(SourceLine {
            start: pos,
            content_end,
            next,
            prefix_ends: prefix_start..prefix_ends.len(),
            kind,
            closing_fence: None,
        });
        pos = next;
    }
    (lines, prefix_ends)
}

/// Matches every possible fence opener to the next standalone bare fence at the
/// same quote depth, provided no intervening line leaves that quote. The reverse
/// pass is linear: lowering the depth truncates candidates from quote scopes
/// that ended.
fn match_fences(lines: &mut [SourceLine]) {
    let mut next_fence = vec![None];
    for index in (0..lines.len()).rev() {
        let depth = lines[index].quote_depth();
        if next_fence.len() > depth + 1 {
            next_fence.truncate(depth + 1);
        } else {
            next_fence.resize(depth + 1, None);
        }
        if matches!(lines[index].kind, LineKind::Fence(_)) {
            lines[index].closing_fence = next_fence[depth];
        }
        if matches!(
            &lines[index].kind,
            LineKind::Fence(fence) if fence.is_standalone_bare()
        ) {
            next_fence[depth] = Some(index);
        }
    }
}

fn change_quote_depth(out: &mut Vec<Token>, current: &mut usize, target: usize, pos: usize) {
    while *current > target {
        push_empty(out, TokenKind::BlockQuoteEnd, pos);
        *current -= 1;
    }
    while *current < target {
        push_empty(out, TokenKind::BlockQuoteStart, pos);
        *current += 1;
    }
}

/// The URL and message-reference ranges of a message body.
pub struct InlineRanges {
    pub urls: Vec<Range<u32>>,
    pub refs: Vec<Range<u32>>,
}

/// Returns the URL and message-reference ranges produced by the tokenizer.
pub fn inline_ranges(source: &str) -> InlineRanges {
    let mut tokens = Vec::new();
    tokenize(source, &mut tokens);
    let mut ranges = InlineRanges {
        urls: Vec::new(),
        refs: Vec::new(),
    };
    for token in tokens {
        match token.kind {
            TokenKind::Url => ranges.urls.push(token.range),
            TokenKind::MessageRef => ranges.refs.push(token.range),
            _ => {}
        }
    }
    ranges
}

fn close_list(out: &mut Vec<Token>, open_list: &mut Option<ListKind>, pos: usize) {
    if open_list.take().is_some() {
        push_empty(out, TokenKind::ListEnd, pos);
    }
}

fn close_paragraph(
    out: &mut Vec<Token>,
    paragraph_end: &mut Option<usize>,
    suppress_blocks: &mut bool,
    pos: usize,
) {
    if paragraph_end.take().is_some() {
        push_empty(out, TokenKind::ParagraphEnd, pos);
        *suppress_blocks = false;
    }
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
        emit_text_with_refs(source, cursor..start, out);
        push(out, TokenKind::Url, start..end);
        cursor = end;
    }
    emit_text_with_refs(source, cursor..range.end, out);
}

fn emit_text_with_refs(source: &str, range: Range<usize>, out: &mut Vec<Token>) {
    let bytes = source.as_bytes();
    let mut cursor = range.start;
    let mut pos = range.start;
    while pos + 1 < range.end {
        if bytes[pos] != b'@' || bytes[pos + 1] != b'@' || !ref_boundary(bytes, range.start, pos) {
            pos += 1;
            continue;
        }
        let code_start = pos + 2;
        let mut code_end = code_start;
        while code_end < range.end && rpc::msgref::is_ref_char(bytes[code_end]) {
            code_end += 1;
        }
        let len = code_end - code_start;
        let terminated = code_end >= range.end || !bytes[code_end].is_ascii_alphanumeric();
        let gated = (rpc::msgref::MIN_CODE_LEN..=rpc::msgref::MAX_CODE_LEN).contains(&len);
        if gated && terminated {
            if cursor < pos {
                push(out, TokenKind::Text, cursor..pos);
            }
            push(out, TokenKind::MessageRef, pos..code_end);
            cursor = code_end;
            pos = code_end;
        } else {
            pos = code_end.max(pos + 1);
        }
    }
    if cursor < range.end {
        push(out, TokenKind::Text, cursor..range.end);
    }
}

fn ref_boundary(bytes: &[u8], start: usize, pos: usize) -> bool {
    if pos == start {
        return true;
    }
    let prev = bytes[pos - 1];
    !prev.is_ascii_alphanumeric() && prev != b'@'
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

fn fence_opener(bytes: &[u8], pos: usize, content_end: usize) -> Option<Fence> {
    let mut start = pos;
    while start + 3 <= content_end {
        if bytes[start..].starts_with(b"```")
            && (start == pos || bytes[start - 1] != b'`')
            && (start + 3 == content_end || bytes[start + 3] != b'`')
        {
            let lang = &bytes[start + 3..content_end];
            if lang.is_empty() {
                return Some(Fence::Bare {
                    start,
                    standalone: start == pos,
                });
            }
            if lang.iter().all(|b| b.is_ascii_alphanumeric()) {
                return Some(Fence::WithLanguage {
                    start,
                    lang: start + 3..content_end,
                });
            }
        }
        start += 1;
    }
    None
}

fn classify_line(bytes: &[u8], start: usize, end: usize) -> LineKind {
    let line = &bytes[start..end];
    if line.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
        return LineKind::Blank;
    }
    if let Some(fence) = fence_opener(bytes, start, end) {
        return LineKind::Fence(fence);
    }
    if line.starts_with(b"# ") {
        return LineKind::Header;
    }
    if line.starts_with(b"- ") || line.starts_with(b"* ") {
        return LineKind::ListItem(ListKind::Unordered, 2);
    }
    if let Some(marker_len) = ordered_marker_len(line) {
        return LineKind::ListItem(ListKind::Ordered, marker_len);
    }
    LineKind::Text
}

fn ordered_marker_len(line: &[u8]) -> Option<usize> {
    let digits = line.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    (line.get(digits) == Some(&b'.') && line.get(digits + 1) == Some(&b' ')).then_some(digits + 2)
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
            vec![
                (TokenKind::CodeBlockStart { lang: Some(3..7) }, 0..7,),
                (TokenKind::CodeBlockLine, 8..10),
                (TokenKind::CodeBlockEnd, 11..14),
            ]
        );
        assert_eq!(
            pairs("```\ncode\n```"),
            vec![
                (TokenKind::CodeBlockStart { lang: None }, 0..3),
                (TokenKind::CodeBlockLine, 4..8),
                (TokenKind::CodeBlockEnd, 9..12),
            ]
        );
    }

    #[test]
    fn fenced_code_block_can_follow_prose_on_the_same_line() {
        let source = "Hello ```rust\nlet x = 4;\n```";
        assert_eq!(
            pairs(source),
            vec![
                (TokenKind::ParagraphStart, 0..0),
                (TokenKind::Text, 0..6),
                (TokenKind::ParagraphEnd, 6..6),
                (TokenKind::CodeBlockStart { lang: Some(9..13) }, 6..13),
                (TokenKind::CodeBlockLine, 14..24),
                (TokenKind::CodeBlockEnd, 25..28),
            ]
        );
    }

    #[test]
    fn rejects_broad_fence_rules() {
        for source in [" ~~~\ncode\n~~~", "````\ncode\n````", "``` rust\ncode\n```"] {
            let tokens = pairs(source);
            assert!(
                tokens
                    .iter()
                    .all(|(kind, _)| !matches!(kind, TokenKind::CodeBlockStart { .. }))
            );
        }
    }

    #[test]
    fn unclosed_fence_is_plain_text() {
        let source = "```\n# still text";
        let tokens = pairs(source);
        assert!(tokens.iter().all(|(kind, _)| !matches!(
            kind,
            TokenKind::CodeBlockStart { .. } | TokenKind::HeaderStart
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
    fn message_refs_are_tokenized_at_word_boundaries() {
        let source = "see @@1c9k3m5n7p2tq for context";
        let tokens = pairs(source);
        assert_eq!(
            tokens
                .iter()
                .filter(|(kind, _)| *kind == TokenKind::MessageRef)
                .map(|(_, range)| &source[range.start as usize..range.end as usize])
                .collect::<Vec<_>>(),
            vec!["@@1c9k3m5n7p2tq"]
        );
        let ranges = inline_ranges(source);
        assert_eq!(ranges.refs.len(), 1);
        assert!(ranges.urls.is_empty());
    }

    #[test]
    fn message_ref_rejections_stay_text() {
        for source in [
            "hi@@1c9k3m5n7p2tq",
            "@@@1c9k3m5n7p2tq",
            "bare @@ marks",
            "@@abc",
            "@@1c9k3m5n7p2tqu2",
            "email@@example",
        ] {
            let tokens = pairs(source);
            assert!(
                tokens
                    .iter()
                    .all(|(kind, _)| *kind != TokenKind::MessageRef),
                "false positive in {source:?}"
            );
        }
    }

    #[test]
    fn message_refs_inert_inside_code() {
        let source = "`@@1c9k3m5n7p2tq` and\n```\n@@1c9k3m5n7p2tq\n```";
        let tokens = pairs(source);
        assert!(
            tokens
                .iter()
                .all(|(kind, _)| *kind != TokenKind::MessageRef)
        );
    }

    #[test]
    fn message_refs_coexist_with_urls() {
        let source = "@@1c9k3m5n7p2tq https://example.com @@0abcdefgh2";
        let ranges = inline_ranges(source);
        assert_eq!(ranges.urls.len(), 1);
        assert_eq!(ranges.refs.len(), 2);
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

    fn kinds(source: &str) -> Vec<TokenKind> {
        pairs(source).into_iter().map(|(kind, _)| kind).collect()
    }

    #[test]
    fn recognizes_block_quotes() {
        let source = "> a\n> b";
        assert_eq!(
            kinds(source),
            vec![
                TokenKind::BlockQuoteStart,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::HardBreak,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlockQuoteEnd,
            ]
        );
        assert_eq!(texts(source, &pairs(source)), vec!["a", "b"]);
    }

    #[test]
    fn internal_blank_lines_collapse_to_one_token() {
        let source = "\n  \nalpha\n\n \n\nbeta\n\n";
        assert_eq!(
            kinds(source),
            vec![
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlankLine,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
            ]
        );
    }

    #[test]
    fn blank_line_between_quotes_is_preserved() {
        assert_eq!(
            kinds("> Quote 1\n\n> Quote 2"),
            vec![
                TokenKind::BlockQuoteStart,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlockQuoteEnd,
                TokenKind::BlankLine,
                TokenKind::BlockQuoteStart,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlockQuoteEnd,
            ]
        );
    }

    #[test]
    fn block_quote_without_space_and_bare_gt_is_plain() {
        assert_eq!(texts(">quote", &pairs(">quote")), vec!["quote"]);
        assert_eq!(
            kinds(">quote").first(),
            Some(&TokenKind::BlockQuoteStart),
            "a bare `>` opens a quote even without a following space"
        );
        // A `>` with nothing after it is an empty quote line, still a quote.
        assert_eq!(kinds(">").first(), Some(&TokenKind::BlockQuoteStart));
    }

    #[test]
    fn nested_block_quotes() {
        let source = "> outer\n>> inner\n> outer again";
        assert_eq!(
            kinds(source),
            vec![
                TokenKind::BlockQuoteStart,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlockQuoteStart,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlockQuoteEnd,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlockQuoteEnd,
            ]
        );
        assert_eq!(
            texts(source, &pairs(source)),
            vec!["outer", "inner", "outer again"]
        );
    }

    #[test]
    fn code_block_inside_quote() {
        let source = "> ```rust\n> fn f() {}\n> ```";
        let tokens = pairs(source);
        assert_eq!(
            tokens.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            vec![
                TokenKind::BlockQuoteStart,
                TokenKind::CodeBlockStart { lang: Some(5..9) },
                TokenKind::CodeBlockLine,
                TokenKind::CodeBlockEnd,
                TokenKind::BlockQuoteEnd,
            ]
        );
        let (_, range) = tokens
            .iter()
            .find(|(k, _)| matches!(k, TokenKind::CodeBlockLine))
            .map(|(k, r)| (k.clone(), r.clone()))
            .unwrap();
        assert_eq!(range, 12..21, "code line excludes the quote prefix");
    }

    #[test]
    fn quote_ends_at_unprefixed_line() {
        let source = "> quoted\nplain";
        assert_eq!(
            kinds(source),
            vec![
                TokenKind::BlockQuoteStart,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
                TokenKind::BlockQuoteEnd,
                TokenKind::ParagraphStart,
                TokenKind::Text,
                TokenKind::ParagraphEnd,
            ]
        );
    }

    #[test]
    fn maximum_message_quote_depth_is_iterative() {
        let source = format!(
            "{}x",
            ">".repeat(rpc::control::MAX_CHAT_BODY_BYTES.saturating_sub(1))
        );
        let tokens = pairs(&source);
        assert_eq!(
            tokens
                .iter()
                .filter(|(kind, _)| matches!(kind, TokenKind::BlockQuoteStart))
                .count(),
            rpc::control::MAX_CHAT_BODY_BYTES - 1
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|(kind, _)| matches!(kind, TokenKind::BlockQuoteEnd))
                .count(),
            rpc::control::MAX_CHAT_BODY_BYTES - 1
        );
    }
}
