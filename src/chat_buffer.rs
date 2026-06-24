use std::ops::Range;

use extui::Style;
use rpc::control::ChatMessage;
use tinyhl::{Highlighter, Language, Source, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;

const REFLOW_TARGET: usize = 95;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub col: u16,
    pub start: u32,
    pub end: u32,
    pub style: Style,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleLine {
    pub message: usize,
    pub line: usize,
}

pub struct ChatEntry {
    #[allow(dead_code)]
    pub id: u64,
    pub sender: String,
    pub body: String,
    #[allow(dead_code)]
    pub timestamp_ms: u64,
    pub local: bool,
    layout: MessageLayout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: (usize, usize),
    head: (usize, usize),
    active: bool,
}

impl Selection {
    fn bounds(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

pub struct VirtualChatBuffer {
    messages: Vec<ChatEntry>,
    max_messages: usize,
    scroll_offset: usize,
    selection: Option<Selection>,
}

impl VirtualChatBuffer {
    pub fn new(max_messages: usize) -> Self {
        Self {
            messages: Vec::new(),
            max_messages: max_messages.max(1),
            scroll_offset: 0,
            selection: None,
        }
    }

    pub fn set_max_messages(&mut self, max_messages: usize) {
        self.max_messages = max_messages.max(1);
        self.trim_front();
    }

    pub fn push_chat(&mut self, message: ChatMessage, local: bool) {
        self.messages.push(ChatEntry {
            id: message.message_id.0,
            sender: message.sender_name,
            body: message.body,
            timestamp_ms: message.timestamp_ms,
            local,
            layout: MessageLayout::new(),
        });
        self.trim_front();
    }

    pub fn push_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        self.messages.push(ChatEntry {
            id: 0,
            sender: sender.into(),
            body: body.into(),
            timestamp_ms: 0,
            local: false,
            layout: MessageLayout::new(),
        });
        self.trim_front();
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.scroll_offset = 0;
        self.selection = None;
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn message(&self, index: usize) -> &ChatEntry {
        &self.messages[index]
    }

    pub fn line(&self, message: usize, line: usize) -> &[Segment] {
        self.messages[message].layout.line(line)
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn scroll_up(&mut self, rows: usize, width: u16, height: u16) {
        let max = self.max_scroll(width, height);
        self.scroll_offset = self.scroll_offset.saturating_add(rows.max(1)).min(max);
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows.max(1));
    }

    pub fn bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn top(&mut self, width: u16, height: u16) {
        self.scroll_offset = self.max_scroll(width, height);
    }

    /// Largest valid `scroll_offset`: the offset that places the oldest line at
    /// the top of the view. Zero when all content fits within `height`.
    fn max_scroll(&mut self, width: u16, height: u16) -> usize {
        self.total_lines_exact(width)
            .saturating_sub(height as usize)
    }

    /// Starts a selection anchored at `pos`, a `(message, line)` coordinate.
    pub fn begin_selection(&mut self, pos: (usize, usize)) {
        self.selection = Some(Selection {
            anchor: pos,
            head: pos,
            active: true,
        });
    }

    /// Moves the head of an in-progress selection to `pos`.
    pub fn extend_selection(&mut self, pos: (usize, usize)) {
        if let Some(selection) = &mut self.selection
            && selection.active
        {
            selection.head = pos;
        }
    }

    /// Marks the in-progress selection as finished; it remains visible.
    pub fn end_selection(&mut self) {
        if let Some(selection) = &mut self.selection {
            selection.active = false;
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn is_selecting(&self) -> bool {
        self.selection.is_some_and(|selection| selection.active)
    }

    /// Returns whether the given `(message, line)` falls within the selection.
    pub fn is_selected(&self, message: usize, line: usize) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let (lo, hi) = selection.bounds();
        let pos = (message, line);
        lo <= pos && pos <= hi
    }

    /// Concatenates the body text of every selected line, content only (no
    /// sender column), joining lines with `\n`. Returns `None` when nothing is
    /// selected.
    pub fn selected_text(&self) -> Option<String> {
        let (lo, hi) = self.selection?.bounds();
        let mut out = String::new();
        let mut first = true;
        for message in lo.0..=hi.0.min(self.messages.len().saturating_sub(1)) {
            let lines = self.messages[message].layout.lines().max(1);
            let start = if message == lo.0 { lo.1 } else { 0 };
            let end = if message == hi.0 { hi.1 } else { lines - 1 };
            for line in start..=end.min(lines - 1) {
                if !first {
                    out.push('\n');
                }
                first = false;
                for segment in self.line(message, line) {
                    out.push_str(
                        &self.messages[message].body[segment.start as usize..segment.end as usize],
                    );
                }
            }
        }
        Some(out)
    }

    pub fn visible_lines(&mut self, width: u16, height: u16, overscan: usize) -> Vec<VisibleLine> {
        let width = width.max(1);
        let target = height as usize;
        let mut need = target.saturating_add(overscan);
        let mut skip = self.scroll_offset;
        let mut reversed = Vec::with_capacity(target);

        for idx in (0..self.messages.len()).rev() {
            let msg = &mut self.messages[idx];
            msg.layout.ensure(width, &msg.body);
            let lines = msg.layout.lines().max(1);
            if skip >= lines {
                skip -= lines;
                continue;
            }

            let end = lines - skip;
            let take = end.min(need);
            let start = end - take;
            for line in (start..end).rev() {
                reversed.push(VisibleLine { message: idx, line });
            }
            need = need.saturating_sub(take);
            skip = 0;
            if need == 0 {
                break;
            }
        }

        if skip > 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub(skip);
        }

        reversed.reverse();
        if reversed.len() > target {
            reversed.split_off(reversed.len() - target)
        } else {
            reversed
        }
    }

    pub fn total_lines_estimate(&self) -> usize {
        self.messages
            .iter()
            .map(|message| message.layout.total_lines_estimate(&message.body))
            .sum::<usize>()
            .max(1)
    }

    fn total_lines_exact(&mut self, width: u16) -> usize {
        let width = width.max(1);
        let mut total = 0usize;
        for message in &mut self.messages {
            message.layout.ensure(width, &message.body);
            total = total.saturating_add(message.layout.lines().max(1));
        }
        total.max(1)
    }

    fn trim_front(&mut self) {
        let excess = self.messages.len().saturating_sub(self.max_messages);
        if excess > 0 {
            self.messages.drain(0..excess);
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
            // Message indices shifted; any selection now points at the wrong rows.
            self.selection = None;
        }
    }
}

struct MessageLayout {
    wrap_width: u16,
    hl: Highlighter,
    cursor: usize,
    line_starts: Vec<u32>,
    segments: Vec<Segment>,
    complete: bool,
    estimated_lines: usize,
}

enum BlockKind {
    Fence { fence: u8, count: usize },
    Heading { marker_len: usize },
    Blockquote { indent: usize },
    ListItem { indent: usize, marker_len: usize },
    Paragraph,
}

impl MessageLayout {
    fn new() -> Self {
        Self {
            wrap_width: 0,
            hl: Highlighter::new(Language::Markdown),
            cursor: 0,
            line_starts: Vec::new(),
            segments: Vec::new(),
            complete: false,
            estimated_lines: 1,
        }
    }

    fn ensure(&mut self, width: u16, text: &str) {
        if self.wrap_width != width {
            self.hl.rebuild(&text as &dyn Source);
            self.reset_layout(width, text);
        }
        while !self.complete {
            self.layout_next_block(text);
        }
        if self.line_starts.is_empty() {
            self.push_line();
            self.complete = true;
        }
    }

    fn lines(&self) -> usize {
        self.line_starts.len()
    }

    fn line(&self, i: usize) -> &[Segment] {
        let start = self.line_starts[i] as usize;
        let end = self
            .line_starts
            .get(i + 1)
            .map_or(self.segments.len(), |&end| end as usize);
        &self.segments[start..end]
    }

    fn total_lines_estimate(&self, text: &str) -> usize {
        if self.complete {
            self.lines()
        } else {
            self.estimated_lines
                .max(estimate_lines(text, self.wrap_width.max(1) as usize))
        }
    }

    fn reset_layout(&mut self, width: u16, text: &str) {
        self.wrap_width = width;
        self.cursor = 0;
        self.line_starts.clear();
        self.segments.clear();
        self.complete = false;
        self.estimated_lines = estimate_lines(text, width.max(1) as usize);
    }

    fn layout_next_block(&mut self, text: &str) {
        let bytes = text.as_bytes();
        let mut pos = self.cursor;
        let mut saw_blank = false;
        loop {
            if pos >= bytes.len() {
                self.cursor = pos;
                self.complete = true;
                return;
            }
            if !is_blank_line(bytes, pos) {
                break;
            }
            saw_blank = true;
            let (_, next) = line_bounds(bytes, pos);
            pos = next;
        }
        if saw_blank && !self.line_starts.is_empty() {
            self.push_line();
        }
        let avail = (self.wrap_width as usize).max(1);
        let target = avail.min(REFLOW_TARGET);
        self.cursor = match classify(bytes, pos) {
            BlockKind::Fence { fence, count } => self.layout_fence(text, pos, fence, count, avail),
            BlockKind::Heading { marker_len } => self.layout_heading(text, pos, marker_len, target),
            BlockKind::Blockquote { indent } => self.layout_quote(text, pos, indent, target),
            BlockKind::ListItem { indent, marker_len } => {
                self.layout_list_item(text, pos, indent, marker_len, target)
            }
            BlockKind::Paragraph => self.layout_paragraph(text, pos, target),
        };
    }

    fn layout_paragraph(&mut self, text: &str, pos: usize, target: usize) -> usize {
        let bytes = text.as_bytes();
        let mut unit_start = pos;
        let mut line_pos = pos;
        loop {
            let (content_end, next) = line_bounds(bytes, line_pos);
            let ends_block =
                next >= bytes.len() || is_blank_line(bytes, next) || starts_new_block(bytes, next);
            let hard_break = !ends_block && bytes[line_pos..content_end].ends_with(b"  ");
            if ends_block || hard_break {
                self.wrap_unit(
                    text,
                    unit_start..content_end,
                    (target, target),
                    (0, 0),
                    false,
                );
                unit_start = next;
            }
            if ends_block {
                return next;
            }
            line_pos = next;
        }
    }

    fn layout_heading(
        &mut self,
        text: &str,
        pos: usize,
        marker_len: usize,
        target: usize,
    ) -> usize {
        let bytes = text.as_bytes();
        let (content_end, next) = line_bounds(bytes, pos);
        let cont_col = (marker_len + 1).min(u16::MAX as usize) as u16;
        let cont_width = target.saturating_sub(marker_len + 1).max(1);
        self.wrap_unit(
            text,
            pos..content_end,
            (target, cont_width),
            (0, cont_col),
            false,
        );
        next
    }

    fn layout_list_item(
        &mut self,
        text: &str,
        pos: usize,
        indent: usize,
        marker_len: usize,
        target: usize,
    ) -> usize {
        let bytes = text.as_bytes();
        let mut line_pos = pos;
        let last_content_end = loop {
            let (content_end, next) = line_bounds(bytes, line_pos);
            if next >= bytes.len() || is_blank_line(bytes, next) || starts_new_block(bytes, next) {
                line_pos = next;
                break content_end;
            }
            line_pos = next;
        };
        let marker_start = pos + indent;
        let content_start = marker_start + marker_len;
        let content_col = (indent + marker_len).min(u16::MAX as usize) as u16;
        let width = target.saturating_sub(indent + marker_len).max(1);
        self.push_line();
        self.emit_text(text, marker_start, content_start, indent as u16);
        self.wrap_unit(
            text,
            content_start..last_content_end,
            (width, width),
            (content_col, content_col),
            true,
        );
        line_pos
    }

    fn layout_quote(&mut self, text: &str, pos: usize, indent: usize, target: usize) -> usize {
        let bytes = text.as_bytes();
        let content_col = (indent + 2).min(u16::MAX as usize) as u16;
        let width = target.saturating_sub(indent + 2).max(1);
        let mut line_pos = pos;
        loop {
            let (content_end, next) = line_bounds(bytes, line_pos);
            let line_indent = bytes[line_pos..content_end]
                .iter()
                .take_while(|b| matches!(b, b' ' | b'\t'))
                .count();
            let marker_start = line_pos + line_indent;
            let mut content_start = marker_start + 1;
            if bytes.get(content_start) == Some(&b' ') {
                content_start += 1;
            }
            let content_start = content_start.min(content_end);
            let mut wrapped = false;
            for range in bwrap::wrap_ranges(&text[content_start..content_end], width, width) {
                self.push_line();
                self.emit_text(text, marker_start, marker_start + 1, indent as u16);
                self.emit_text(
                    text,
                    content_start + range.start,
                    content_start + range.end,
                    content_col,
                );
                wrapped = true;
            }
            if !wrapped {
                self.push_line();
                self.emit_text(text, marker_start, marker_start + 1, indent as u16);
            }
            line_pos = next;
            if line_pos >= bytes.len()
                || !matches!(classify(bytes, line_pos), BlockKind::Blockquote { .. })
            {
                return line_pos;
            }
        }
    }

    fn layout_fence(
        &mut self,
        text: &str,
        pos: usize,
        fence: u8,
        count: usize,
        avail: usize,
    ) -> usize {
        let bytes = text.as_bytes();
        let mut line_pos = pos;
        let mut first = true;
        loop {
            if line_pos >= bytes.len() {
                return line_pos;
            }
            let (content_end, next) = line_bounds(bytes, line_pos);
            self.emit_verbatim(text, line_pos, content_end, avail);
            let closes = !first && is_fence_closer(&bytes[line_pos..content_end], fence, count);
            first = false;
            line_pos = next;
            if closes {
                return line_pos;
            }
        }
    }

    fn wrap_unit(
        &mut self,
        text: &str,
        range: Range<usize>,
        widths: (usize, usize),
        cols: (u16, u16),
        continue_line: bool,
    ) {
        let mut first = true;
        for wrapped in bwrap::wrap_ranges(&text[range.clone()], widths.0, widths.1) {
            if !(first && continue_line) {
                self.push_line();
            }
            let col = if first { cols.0 } else { cols.1 };
            self.emit_text(
                text,
                range.start + wrapped.start,
                range.start + wrapped.end,
                col,
            );
            first = false;
        }
        if first && !continue_line {
            self.push_line();
        }
    }

    fn push_line(&mut self) {
        self.line_starts.push(self.segments.len() as u32);
    }

    fn emit_verbatim(&mut self, text: &str, start: usize, end: usize, avail: usize) {
        self.push_line();
        let avail = avail.max(1);
        let mut chunk_start = start;
        let mut width = 0usize;
        for (i, ch) in text[start..end].char_indices() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if width + w > avail && width > 0 {
                self.emit_text(text, chunk_start, start + i, 0);
                self.push_line();
                chunk_start = start + i;
                width = 0;
            }
            width += w;
        }
        if chunk_start < end {
            self.emit_text(text, chunk_start, end, 0);
        }
    }

    fn emit_text(&mut self, text: &str, mut start: usize, end: usize, mut col: u16) {
        let bytes = text.as_bytes();
        while start < end {
            let mut brk = start;
            while brk < end && bytes[brk] != b'\n' && bytes[brk] != b'\r' {
                brk += 1;
            }
            let mut piece_end = brk;
            while piece_end > start && matches!(bytes[piece_end - 1], b' ' | b'\t') {
                piece_end -= 1;
            }
            if piece_end > start {
                col = self.emit_styled(text, start, piece_end, col);
            }
            if brk >= end {
                break;
            }
            let mut next = brk;
            while next < end && matches!(bytes[next], b' ' | b'\t' | b'\n' | b'\r') {
                next += 1;
            }
            col = col.saturating_add(1);
            start = next;
        }
    }

    fn emit_styled(&mut self, text: &str, start: usize, end: usize, mut col: u16) -> u16 {
        let Self { hl, segments, .. } = self;
        let mut push = |s: usize, e: usize, style: Style, col: u16| -> u16 {
            segments.push(Segment {
                col,
                start: s as u32,
                end: e as u32,
                style,
            });
            col.saturating_add(UnicodeWidthStr::width(&text[s..e]).min(u16::MAX as usize) as u16)
        };
        let mut cursor = start;
        for span in hl.render(Span::new(start as u32, (end - start) as u32)) {
            let s = (span.span.offset as usize).max(cursor);
            let e = (span.span.end() as usize).min(end);
            if e <= s {
                continue;
            }
            if s > cursor {
                col = push(cursor, s, Style::DEFAULT, col);
            }
            let style = if span.local_kind == tinyhl::kind::WHITESPACE {
                Style::DEFAULT
            } else {
                theme::syntax_style(&span)
            };
            col = push(s, e, style, col);
            cursor = e;
        }
        if cursor < end {
            col = push(cursor, end, Style::DEFAULT, col);
        }
        col
    }
}

fn line_bounds(bytes: &[u8], pos: usize) -> (usize, usize) {
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
    (content_end, next)
}

fn is_blank_line(bytes: &[u8], pos: usize) -> bool {
    let (content_end, _) = line_bounds(bytes, pos);
    bytes[pos..content_end]
        .iter()
        .all(|b| matches!(b, b' ' | b'\t'))
}

fn list_marker_len(rest: &[u8]) -> Option<usize> {
    match rest.first()? {
        b'-' | b'+' | b'*' => (rest.get(1) == Some(&b' ')).then_some(2),
        b'0'..=b'9' => {
            let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits > 9 {
                return None;
            }
            match (rest.get(digits), rest.get(digits + 1)) {
                (Some(b'.') | Some(b')'), Some(b' ')) => Some(digits + 2),
                _ => None,
            }
        }
        _ => None,
    }
}

fn classify(bytes: &[u8], pos: usize) -> BlockKind {
    let (content_end, _) = line_bounds(bytes, pos);
    let line = &bytes[pos..content_end];
    let indent = line
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t'))
        .count();
    let rest = &line[indent..];
    if indent <= 3 && (rest.starts_with(b"```") || rest.starts_with(b"~~~")) {
        let fence = rest[0];
        let count = rest.iter().take_while(|&&b| b == fence).count();
        return BlockKind::Fence { fence, count };
    }
    if indent == 0 && rest.first() == Some(&b'#') {
        let hashes = rest.iter().take_while(|&&b| b == b'#').count();
        if hashes <= 6 && matches!(rest.get(hashes), None | Some(b' ') | Some(b'\t')) {
            return BlockKind::Heading { marker_len: hashes };
        }
    }
    if rest.first() == Some(&b'>') {
        return BlockKind::Blockquote { indent };
    }
    if let Some(marker_len) = list_marker_len(rest) {
        return BlockKind::ListItem { indent, marker_len };
    }
    BlockKind::Paragraph
}

fn starts_new_block(bytes: &[u8], pos: usize) -> bool {
    !matches!(classify(bytes, pos), BlockKind::Paragraph)
}

fn estimate_lines(text: &str, avail: usize) -> usize {
    let target = avail.min(REFLOW_TARGET).max(1);
    let mut lines = 0usize;
    for line in text.lines() {
        lines = lines.saturating_add(UnicodeWidthStr::width(line).max(1).div_ceil(target));
    }
    lines.max(1)
}

fn is_fence_closer(line: &[u8], fence: u8, count: usize) -> bool {
    let indent = line
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t'))
        .count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let n = rest.iter().take_while(|&&b| b == fence).count();
    n >= count && rest[n..].iter().all(|b| matches!(b, b' ' | b'\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_with_notices(count: usize) -> VirtualChatBuffer {
        let mut buf = VirtualChatBuffer::new(1000);
        for i in 0..count {
            buf.push_notice("user", format!("message {i}"));
        }
        buf
    }

    #[test]
    fn scroll_up_clamps_at_the_top() {
        let mut buf = buffer_with_notices(20);
        let (width, height) = (40, 5);
        let total = buf.total_lines_exact(width);
        buf.scroll_up(1000, width, height);
        assert_eq!(buf.scroll_offset(), total - height as usize);
        // Already clamped; scrolling further changes nothing.
        buf.scroll_up(10, width, height);
        assert_eq!(buf.scroll_offset(), total - height as usize);
    }

    #[test]
    fn top_then_scroll_down_reveals_one_line() {
        let mut buf = buffer_with_notices(20);
        let (width, height) = (40, 5);
        buf.top(width, height);
        let top = buf.scroll_offset();
        assert!(top > 0);
        buf.scroll_down(1);
        assert_eq!(buf.scroll_offset(), top - 1);
    }

    #[test]
    fn max_scroll_is_zero_when_content_fits() {
        let mut buf = buffer_with_notices(3);
        assert_eq!(buf.max_scroll(40, 50), 0);
        buf.top(40, 50);
        assert_eq!(buf.scroll_offset(), 0);
    }

    #[test]
    fn bottom_resets_to_zero_offset() {
        let mut buf = buffer_with_notices(20);
        buf.scroll_up(5, 40, 5);
        assert!(buf.scroll_offset() > 0);
        buf.bottom();
        assert_eq!(buf.scroll_offset(), 0);
    }

    #[test]
    fn selected_text_is_body_content_joined_by_newlines() {
        let mut buf = buffer_with_notices(3);
        // Lay out every message so line segments exist.
        let _ = buf.total_lines_exact(40);
        buf.begin_selection((0, 0));
        buf.extend_selection((2, 0));
        assert_eq!(
            buf.selected_text().as_deref(),
            Some("message 0\nmessage 1\nmessage 2")
        );
    }
}
