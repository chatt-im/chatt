use std::ops::Range;

use extui::{Style, vt::Modifier};
use rpc::control::ChatMessage;
use rpc::ids::FileTransferId;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::markdown::{Token, TokenKind};
use crate::theme::SyntaxTheme;

const REFLOW_TARGET: usize = 95;

/// Wrapped body lines beyond this collapse a lone message behind an expander.
const COLLAPSE_LIMIT: usize = 12;
/// Body lines shown while a long message is collapsed.
const COLLAPSE_SHOW: usize = 10;
/// Maximum gap between adjacent same-sender messages that still groups them.
const GROUP_GAP_MS: u64 = 90_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub col: u16,
    pub start: u32,
    pub end: u32,
    pub style: Style,
}

/// The role a rendered chat row plays within its block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Sender name plus age. Toggles collapse when it belongs to a long message.
    Heading,
    /// A wrapped body line. The only selectable kind.
    Body,
    /// The `...` truncation row of a collapsed message. Toggles collapse.
    Ellipsis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleLine {
    /// For `Body`/`Ellipsis` the owning message; for `Heading` the block's first
    /// (oldest) message.
    pub message: usize,
    /// Oldest message rendered under this line's heading.
    pub block_first: usize,
    /// Newest message rendered under this line's heading, inclusive.
    pub block_last: usize,
    /// Body line index within `message`; zero for `Heading`/`Ellipsis`.
    pub line: usize,
    pub kind: LineKind,
}

impl VisibleLine {
    pub fn block_contains(self, message: usize) -> bool {
        self.block_first <= message && message <= self.block_last
    }
}

pub struct ChatEntry {
    #[allow(dead_code)]
    pub id: u64,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub local: bool,
    /// The server file transfer this message announces, when it is a file. Keys
    /// the render-time progress overlay in [`crate::app::room::RoomSession`].
    pub file_transfer_id: Option<FileTransferId>,
    /// Byte ranges of `http`/`https` URLs in `body`, computed once at push time.
    pub links: Vec<Range<u32>>,
    /// Whether a collapsible (over [`COLLAPSE_LIMIT`] lines) message is expanded.
    expanded: bool,
    layout: MessageLayout,
}

/// A run of one or more consecutive messages rendered under a single heading.
struct Block {
    /// Oldest message index (heading anchor and age source).
    first: usize,
    /// Newest message index, inclusive.
    last: usize,
    /// Body lines actually rendered: the full wrapped count, or [`COLLAPSE_SHOW`]
    /// when collapsed.
    body_lines: usize,
    /// True only for a lone message over [`COLLAPSE_LIMIT`] lines that is not
    /// expanded.
    collapsed: bool,
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
    selected_message: Option<usize>,
    syntax: SyntaxTheme,
}

impl VirtualChatBuffer {
    pub fn new(max_messages: usize, syntax: SyntaxTheme) -> Self {
        Self {
            messages: Vec::new(),
            max_messages: max_messages.max(1),
            scroll_offset: 0,
            selection: None,
            selected_message: None,
            syntax,
        }
    }

    pub fn set_max_messages(&mut self, max_messages: usize) {
        self.max_messages = max_messages.max(1);
        self.trim_front();
    }

    /// Restyles syntax highlighting when the active theme changes. Cached
    /// message layouts are invalidated so already-rendered history recolors on
    /// the next layout pass.
    pub fn set_syntax(&mut self, syntax: SyntaxTheme) {
        if self.syntax == syntax {
            return;
        }
        self.syntax = syntax;
        for entry in &mut self.messages {
            entry.layout.invalidate();
        }
    }

    pub fn push_chat(&mut self, message: ChatMessage, local: bool) {
        let links = crate::markdown::link_ranges(&message.body);
        self.messages.push(ChatEntry {
            id: message.message_id.0,
            sender: message.sender_name,
            body: message.body,
            timestamp_ms: message.timestamp_ms,
            local,
            file_transfer_id: message.file_transfer_id,
            links,
            expanded: false,
            layout: MessageLayout::new(),
        });
        self.trim_front();
    }

    pub fn push_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) {
        let body = body.into();
        let links = crate::markdown::link_ranges(&body);
        self.messages.push(ChatEntry {
            id: 0,
            sender: sender.into(),
            body,
            timestamp_ms: 0,
            local: false,
            file_transfer_id: None,
            links,
            expanded: false,
            layout: MessageLayout::new(),
        });
        self.trim_front();
    }

    pub fn clear(&mut self) {
        self.messages.clear();
        self.scroll_offset = 0;
        self.selection = None;
        self.selected_message = None;
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

    /// Returns the URL at `col_in_line` on wrapped `line` of `message`, when a
    /// link segment covers that column. `col_in_line` is measured from the start
    /// of the message content, the same origin as [`Segment::col`].
    pub fn link_at(&self, message: usize, line: usize, col_in_line: u16) -> Option<&str> {
        let entry = self.messages.get(message)?;
        if entry.links.is_empty() {
            return None;
        }
        for seg in entry.layout.line(line) {
            let text = &entry.body[seg.start as usize..seg.end as usize];
            let width = UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16;
            if col_in_line < seg.col || col_in_line >= seg.col.saturating_add(width) {
                continue;
            }
            let range = entry
                .links
                .iter()
                .find(|r| r.start < seg.end && seg.start < r.end)?;
            return Some(&entry.body[range.start as usize..range.end as usize]);
        }
        None
    }

    /// Whether the current selection is a collapsed click (anchor equals head),
    /// i.e. the pointer was pressed and released without dragging.
    pub fn selection_is_click(&self) -> bool {
        self.selection.is_some_and(|s| s.anchor == s.head)
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

    #[cfg(test)]
    pub fn selected_message(&self) -> Option<usize> {
        self.selected_message
    }

    pub fn ensure_selected_message(&mut self) -> Option<usize> {
        if self.messages.is_empty() {
            self.selected_message = None;
            return None;
        }
        let selected = self
            .selected_message
            .filter(|message| *message < self.messages.len())
            .unwrap_or_else(|| self.messages.len() - 1);
        self.selected_message = Some(selected);
        self.selected_message
    }

    pub fn ensure_selected_header(&mut self, width: u16) -> Option<usize> {
        let selected = self.ensure_selected_message()?;
        let (first, _) = self.header_block_containing(selected, width)?;
        self.selected_message = Some(first);
        self.selected_message
    }

    pub fn select_message(&mut self, message: usize) -> Option<usize> {
        if message >= self.messages.len() {
            return None;
        }
        self.selected_message = Some(message);
        Some(message)
    }

    pub fn select_first_header(&mut self) -> Option<usize> {
        self.select_message(0)
    }

    pub fn select_last_header(&mut self, width: u16) -> Option<usize> {
        let (first, _) = self.header_blocks(width).last().copied()?;
        self.selected_message = Some(first);
        self.selected_message
    }

    pub fn select_header_containing(&mut self, message: usize, width: u16) -> Option<usize> {
        if message >= self.messages.len() {
            return None;
        }
        let (first, _) = self.header_block_containing(message, width)?;
        self.selected_message = Some(first);
        self.selected_message
    }

    pub fn move_selected_header(&mut self, delta: isize, width: u16) -> Option<usize> {
        if self.messages.is_empty() {
            self.selected_message = None;
            return None;
        }
        let blocks = self.header_blocks(width);
        let selected = self.ensure_selected_message()?;
        let current = blocks
            .iter()
            .position(|(first, last)| *first <= selected && selected <= *last)
            .unwrap_or_else(|| {
                blocks
                    .iter()
                    .position(|(first, _)| selected < *first)
                    .unwrap_or(blocks.len())
                    .saturating_sub(1)
            });
        let next = (current as isize + delta).clamp(0, blocks.len() as isize - 1) as usize;
        self.selected_message = Some(blocks[next].0);
        self.selected_message
    }

    pub fn is_header_selected(&self, line: VisibleLine) -> bool {
        line.kind == LineKind::Heading
            && self
                .selected_message
                .is_some_and(|message| line.block_contains(message))
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

    /// Copies original body text covered by the selected rendered rows, content
    /// only (no sender column). Wrapped rows from the same message are sliced
    /// as one source range so clipboard text keeps the message's whitespace
    /// instead of the display wrapper's trimmed fragments.
    pub fn selected_text(&self) -> Option<String> {
        let (lo, hi) = self.selection?.bounds();
        let mut out = String::new();
        let mut first = true;
        for message in lo.0..=hi.0.min(self.messages.len().saturating_sub(1)) {
            let entry = &self.messages[message];
            let lines = entry.layout.lines().max(1);
            let start = if message == lo.0 { lo.1 } else { 0 };
            if start >= lines {
                continue;
            }
            let end = if message == hi.0 { hi.1 } else { lines - 1 };
            let end = end.min(lines - 1);
            if !first {
                out.push('\n');
            }
            first = false;
            if start == 0 && end == lines - 1 {
                out.push_str(&entry.body);
            } else {
                let range = entry.layout.source_range(start, end, entry.body.len());
                out.push_str(&entry.body[range]);
            }
        }
        Some(out)
    }

    pub fn selected_header_text(&mut self, width: u16) -> Option<String> {
        let selected = self.selected_message?;
        let (first, last) = self.header_block_containing(selected, width)?;
        let mut out = String::new();
        for message in first..=last {
            if message > first {
                out.push('\n');
            }
            out.push_str(&self.messages[message].body);
        }
        Some(out)
    }

    pub fn keep_selected_header_visible(&mut self, width: u16, height: u16) -> Option<()> {
        let selected = self.ensure_selected_message()?;
        self.keep_header_visible(selected, width, height)
    }

    /// Toggles the expand/collapse state of `message` when it is collapsible
    /// (over [`COLLAPSE_LIMIT`] wrapped lines at `width`). Returns whether the
    /// state changed.
    pub fn toggle_expand(&mut self, message: usize, width: u16) -> bool {
        if message >= self.messages.len() || self.ensure_lines(message, width) <= COLLAPSE_LIMIT {
            return false;
        }
        self.messages[message].expanded = !self.messages[message].expanded;
        true
    }

    pub fn toggle_selected_expand(&mut self, width: u16) -> bool {
        let Some(selected) = self.selected_message else {
            return false;
        };
        let Some((first, last)) = self.header_block_containing(selected, width) else {
            return false;
        };
        if first != last {
            return false;
        }
        self.toggle_expand(first, width)
    }

    /// Whether `message`'s wrapped body exceeds [`COLLAPSE_LIMIT`] at `width`.
    #[cfg(test)]
    pub fn is_collapsible(&mut self, message: usize, width: u16) -> bool {
        message < self.messages.len() && self.ensure_lines(message, width) > COLLAPSE_LIMIT
    }

    /// Whether `message` is collapsible (over [`COLLAPSE_LIMIT`] lines) and
    /// currently collapsed. Assumes its layout was already laid out this frame
    /// (true for any message in a visible block).
    pub fn is_collapsed(&self, message: usize) -> bool {
        let entry = &self.messages[message];
        entry.layout.lines() > COLLAPSE_LIMIT && !entry.expanded
    }

    /// Whether `message` is collapsible and currently expanded. Counterpart to
    /// [`Self::is_collapsed`]; both are false for short messages.
    pub fn is_expanded(&self, message: usize) -> bool {
        let entry = &self.messages[message];
        entry.layout.lines() > COLLAPSE_LIMIT && entry.expanded
    }

    pub fn visible_lines(&mut self, width: u16, height: u16, overscan: usize) -> Vec<VisibleLine> {
        let width = width.max(1);
        let target = height as usize;
        let mut need = target.saturating_add(overscan);
        let mut skip = self.scroll_offset;
        let mut reversed = Vec::with_capacity(target);

        let mut cursor = self.messages.len();
        'runs: while cursor > 0 && need > 0 {
            let last = cursor - 1;
            let run_start = self.run_start(last, width);
            let blocks = self.pack_run(run_start, last, width);
            cursor = run_start;
            for block in blocks.iter().rev() {
                let rows = self.block_row_lines(block);
                let n = rows.len();
                if skip >= n {
                    skip -= n;
                    continue;
                }
                let end = n - skip;
                let take = end.min(need);
                let start = end - take;
                for i in (start..end).rev() {
                    reversed.push(rows[i]);
                }
                need = need.saturating_sub(take);
                skip = 0;
                if need == 0 {
                    break 'runs;
                }
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

    /// Lays out `idx` at `width` and returns its wrapped line count (at least 1).
    fn ensure_lines(&mut self, idx: usize, width: u16) -> usize {
        let width = width.max(1);
        let syntax = self.syntax;
        let msg = &mut self.messages[idx];
        msg.layout.ensure(width, &msg.body, syntax);
        msg.layout.lines().max(1)
    }

    /// Whether a block boundary is forced between adjacent messages `prev` and
    /// `cur` (`cur == prev + 1`): a sender or locality change, a notice
    /// (`timestamp_ms == 0`), a gap over [`GROUP_GAP_MS`], or either side being a
    /// lone collapsible message over [`COLLAPSE_LIMIT`] lines.
    fn boundary_before(&mut self, prev: usize, cur: usize, width: u16) -> bool {
        if self.messages[prev].timestamp_ms == 0 || self.messages[cur].timestamp_ms == 0 {
            return true;
        }
        if self.messages[prev].local != self.messages[cur].local
            || self.messages[prev].sender != self.messages[cur].sender
        {
            return true;
        }
        let gap = self.messages[cur]
            .timestamp_ms
            .saturating_sub(self.messages[prev].timestamp_ms);
        if gap > GROUP_GAP_MS {
            return true;
        }
        self.ensure_lines(prev, width) > COLLAPSE_LIMIT
            || self.ensure_lines(cur, width) > COLLAPSE_LIMIT
    }

    /// Oldest message in the same groupable run as `last`, walking back until a
    /// forced boundary. Headings anchor to a run's blocks from this end, so they
    /// stay fixed as newer messages arrive.
    fn run_start(&mut self, last: usize, width: u16) -> usize {
        if self.ensure_lines(last, width) > COLLAPSE_LIMIT {
            return last;
        }
        let mut start = last;
        while start > 0 && !self.boundary_before(start - 1, start, width) {
            start -= 1;
        }
        start
    }

    /// Newest message in the same groupable run as `start`, walking forward until
    /// a forced boundary.
    fn run_end(&mut self, start: usize, width: u16) -> usize {
        if self.ensure_lines(start, width) > COLLAPSE_LIMIT {
            return start;
        }
        let mut end = start;
        while end + 1 < self.messages.len() && !self.boundary_before(end, end + 1, width) {
            end += 1;
        }
        end
    }

    /// Packs the run `[run_start, run_end]` into blocks oldest-first, greedily
    /// filling each to [`COLLAPSE_LIMIT`] lines. A lone message over the limit
    /// becomes a single collapsible block.
    fn pack_run(&mut self, run_start: usize, run_end: usize, width: u16) -> Vec<Block> {
        let first_lines = self.ensure_lines(run_start, width);
        if run_start == run_end && first_lines > COLLAPSE_LIMIT {
            let expanded = self.messages[run_start].expanded;
            return vec![Block {
                first: run_start,
                last: run_start,
                body_lines: if expanded { first_lines } else { COLLAPSE_SHOW },
                collapsed: !expanded,
            }];
        }
        let mut blocks = Vec::new();
        let mut start = run_start;
        let mut total = 0usize;
        for message in run_start..=run_end {
            let lines = self.ensure_lines(message, width);
            if total > 0 && total + lines > COLLAPSE_LIMIT {
                blocks.push(Block {
                    first: start,
                    last: message - 1,
                    body_lines: total,
                    collapsed: false,
                });
                start = message;
                total = 0;
            }
            total += lines;
        }
        blocks.push(Block {
            first: start,
            last: run_end,
            body_lines: total,
            collapsed: false,
        });
        blocks
    }

    /// Rendered rows of `block`, top to bottom: heading, body lines, then an
    /// ellipsis row when collapsed. Layouts must already be ensured.
    fn block_row_lines(&self, block: &Block) -> Vec<VisibleLine> {
        let mut rows = Vec::with_capacity(Self::block_rows(block));
        rows.push(VisibleLine {
            message: block.first,
            block_first: block.first,
            block_last: block.last,
            line: 0,
            kind: LineKind::Heading,
        });
        if block.collapsed {
            for line in 0..block.body_lines {
                rows.push(VisibleLine {
                    message: block.last,
                    block_first: block.first,
                    block_last: block.last,
                    line,
                    kind: LineKind::Body,
                });
            }
            rows.push(VisibleLine {
                message: block.last,
                block_first: block.first,
                block_last: block.last,
                line: 0,
                kind: LineKind::Ellipsis,
            });
        } else {
            for message in block.first..=block.last {
                let lines = self.messages[message].layout.lines().max(1);
                for line in 0..lines {
                    rows.push(VisibleLine {
                        message,
                        block_first: block.first,
                        block_last: block.last,
                        line,
                        kind: LineKind::Body,
                    });
                }
            }
        }
        rows
    }

    /// Total rendered rows for a block: heading + body + optional ellipsis.
    fn block_rows(block: &Block) -> usize {
        1 + block.body_lines + usize::from(block.collapsed)
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
        let mut cursor = 0usize;
        while cursor < self.messages.len() {
            let run_end = self.run_end(cursor, width);
            for block in self.pack_run(cursor, run_end, width) {
                total = total.saturating_add(Self::block_rows(&block));
            }
            cursor = run_end + 1;
        }
        total.max(1)
    }

    fn header_blocks(&mut self, width: u16) -> Vec<(usize, usize)> {
        let width = width.max(1);
        let mut blocks = Vec::new();
        let mut cursor = 0usize;
        while cursor < self.messages.len() {
            let run_end = self.run_end(cursor, width);
            for block in self.pack_run(cursor, run_end, width) {
                blocks.push((block.first, block.last));
            }
            cursor = run_end + 1;
        }
        blocks
    }

    fn header_block_containing(&mut self, message: usize, width: u16) -> Option<(usize, usize)> {
        self.header_blocks(width)
            .into_iter()
            .find(|(first, last)| *first <= message && message <= *last)
    }

    fn keep_header_visible(&mut self, message: usize, width: u16, height: u16) -> Option<()> {
        let height = height as usize;
        if height == 0 {
            return None;
        }
        let (header_row, total_rows) = self.header_row_and_total(message, width)?;
        let max_scroll = total_rows.saturating_sub(height);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let top = total_rows.saturating_sub(self.scroll_offset.saturating_add(height));
        let bottom = top.saturating_add(height).min(total_rows);
        if header_row < top || header_row >= bottom {
            self.scroll_offset = total_rows
                .saturating_sub(header_row.saturating_add(height))
                .min(max_scroll);
        }
        Some(())
    }

    fn header_row_and_total(&mut self, message: usize, width: u16) -> Option<(usize, usize)> {
        let width = width.max(1);
        let mut header_row = None;
        let mut row = 0usize;
        let mut cursor = 0usize;
        while cursor < self.messages.len() {
            let run_end = self.run_end(cursor, width);
            for block in self.pack_run(cursor, run_end, width) {
                if block.first <= message && message <= block.last {
                    header_row = Some(row);
                }
                row = row.saturating_add(Self::block_rows(&block));
            }
            cursor = run_end + 1;
        }
        header_row.map(|header| (header, row.max(1)))
    }

    fn trim_front(&mut self) {
        let excess = self.messages.len().saturating_sub(self.max_messages);
        if excess > 0 {
            self.messages.drain(0..excess);
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
            // Message indices shifted; line selections now point at wrong rows.
            self.selection = None;
            self.selected_message = self
                .selected_message
                .and_then(|message| message.checked_sub(excess));
        }
    }
}

struct MessageLayout {
    wrap_width: u16,
    cursor: usize,
    tokens: Vec<Token>,
    line_starts: Vec<u32>,
    line_sources: Vec<(u32, u32)>,
    segments: Vec<Segment>,
    complete: bool,
    estimated_lines: usize,
    syntax: SyntaxTheme,
}

struct RenderPiece {
    source: Range<usize>,
    display: Range<usize>,
    style: Style,
}

struct InvisibleSource {
    source: Range<usize>,
    display_pos: usize,
}

#[derive(Default)]
struct LinePrefix {
    visible: Option<(Range<usize>, Style)>,
    invisible: Vec<Range<usize>>,
}

impl MessageLayout {
    fn new() -> Self {
        Self {
            wrap_width: 0,
            cursor: 0,
            tokens: Vec::new(),
            line_starts: Vec::new(),
            line_sources: Vec::new(),
            segments: Vec::new(),
            complete: false,
            estimated_lines: 1,
            syntax: SyntaxTheme::default(),
        }
    }

    /// Forces the next [`ensure`](Self::ensure) to rebuild the layout, picking
    /// up a new syntax theme. `0` is never a real wrap width (callers pass
    /// `width.max(1)`), so it reliably triggers a rebuild.
    fn invalidate(&mut self) {
        self.wrap_width = 0;
    }

    fn ensure(&mut self, width: u16, text: &str, syntax: SyntaxTheme) {
        self.syntax = syntax;
        if self.wrap_width != width {
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

    fn source_range(&self, start_line: usize, end_line: usize, text_len: usize) -> Range<usize> {
        if self.line_sources.is_empty() || text_len == 0 {
            return 0..0;
        }
        let last_line = self.line_sources.len() - 1;
        let start_line = start_line.min(last_line);
        let end_line = end_line.min(last_line).max(start_line);
        let start = (start_line..=end_line)
            .find_map(|line| Self::source_start(self.line_sources[line]))
            .or_else(|| {
                (0..start_line)
                    .rev()
                    .find_map(|line| Self::source_end(self.line_sources[line]))
            })
            .unwrap_or(0)
            .min(text_len);
        let end = (start_line..=end_line)
            .rev()
            .find_map(|line| Self::source_end(self.line_sources[line]))
            .or_else(|| {
                ((end_line + 1)..self.line_sources.len())
                    .find_map(|line| Self::source_start(self.line_sources[line]))
            })
            .unwrap_or(start)
            .min(text_len)
            .max(start);
        start..end
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
        crate::markdown::tokenize(text, &mut self.tokens);
        self.line_starts.clear();
        self.line_sources.clear();
        self.segments.clear();
        self.complete = false;
        self.estimated_lines = estimate_lines(text, width.max(1) as usize);
    }

    fn layout_next_block(&mut self, text: &str) {
        let avail = (self.wrap_width as usize).max(1);
        let target = avail.min(REFLOW_TARGET);

        if self.cursor >= self.tokens.len() {
            self.complete = true;
            return;
        }

        match &self.tokens[self.cursor].kind {
            TokenKind::ParagraphStart => {
                let end = self.find_token(self.cursor + 1, |kind| {
                    matches!(kind, TokenKind::ParagraphEnd)
                });
                self.layout_inline_lines(
                    text,
                    self.cursor + 1,
                    end,
                    Style::DEFAULT,
                    (target, target),
                    (0, 0),
                );
                self.cursor = end.saturating_add(1);
            }
            TokenKind::HeaderStart => {
                let marker = token_range(&self.tokens[self.cursor]);
                let end =
                    self.find_token(self.cursor + 1, |kind| matches!(kind, TokenKind::HeaderEnd));
                let prefix = LinePrefix {
                    visible: None,
                    invisible: vec![marker],
                };
                self.layout_inline_line(
                    text,
                    self.cursor + 1,
                    end,
                    Style::DEFAULT | Modifier::BOLD,
                    (target, target),
                    (0, 0),
                    prefix,
                );
                self.cursor = end.saturating_add(1);
            }
            TokenKind::UnorderedListStart | TokenKind::OrderedListStart => {
                self.cursor = self.layout_list(text, self.cursor + 1, target);
            }
            TokenKind::CodeBlock { content, .. } => {
                let content = content.start as usize..content.end as usize;
                let source = token_range(&self.tokens[self.cursor]);
                self.layout_code_block(text, content, source, avail);
                self.cursor += 1;
            }
            _ => self.cursor += 1,
        }
    }

    fn find_token(&self, start: usize, pred: impl Fn(&TokenKind) -> bool) -> usize {
        self.tokens[start..]
            .iter()
            .position(|token| pred(&token.kind))
            .map_or(self.tokens.len(), |offset| start + offset)
    }

    fn layout_inline_lines(
        &mut self,
        text: &str,
        start: usize,
        end: usize,
        base_style: Style,
        widths: (usize, usize),
        cols: (u16, u16),
    ) {
        let mut line_start = start;
        for i in start..end {
            if matches!(self.tokens[i].kind, TokenKind::HardBreak) {
                self.layout_inline_line(
                    text,
                    line_start,
                    i,
                    base_style,
                    widths,
                    cols,
                    LinePrefix::default(),
                );
                line_start = i + 1;
            }
        }
        self.layout_inline_line(
            text,
            line_start,
            end,
            base_style,
            widths,
            cols,
            LinePrefix::default(),
        );
    }

    fn layout_list(&mut self, text: &str, mut cursor: usize, target: usize) -> usize {
        while cursor < self.tokens.len() {
            match &self.tokens[cursor].kind {
                TokenKind::ListItemStart { marker } => {
                    let marker = marker.start as usize..marker.end as usize;
                    let end =
                        self.find_token(cursor + 1, |kind| matches!(kind, TokenKind::ListItemEnd));
                    let marker_width = UnicodeWidthStr::width(&text[marker.clone()]);
                    let content_col = marker_width.min(u16::MAX as usize) as u16;
                    let content_width = target.saturating_sub(marker_width).max(1);
                    let prefix = LinePrefix {
                        visible: Some((marker, self.syntax.keyword | Modifier::BOLD)),
                        invisible: Vec::new(),
                    };
                    self.layout_inline_line(
                        text,
                        cursor + 1,
                        end,
                        Style::DEFAULT,
                        (target, content_width),
                        (0, content_col),
                        prefix,
                    );
                    cursor = end.saturating_add(1);
                }
                TokenKind::ListEnd => return cursor + 1,
                _ => cursor += 1,
            }
        }
        cursor
    }

    fn layout_inline_line(
        &mut self,
        text: &str,
        start: usize,
        end: usize,
        base_style: Style,
        widths: (usize, usize),
        cols: (u16, u16),
        prefix: LinePrefix,
    ) {
        let mut display = String::new();
        let mut pieces = Vec::new();
        let mut invisible = Vec::new();

        if let Some((source, style)) = prefix.visible {
            append_piece(text, &mut display, &mut pieces, source, style);
        }
        for source in prefix.invisible {
            invisible.push(InvisibleSource {
                source,
                display_pos: display.len(),
            });
        }

        self.collect_inline_pieces(
            text,
            start,
            end,
            base_style,
            &mut display,
            &mut pieces,
            &mut invisible,
        );
        self.wrap_pieces(&display, &pieces, &invisible, widths, cols);
    }

    fn collect_inline_pieces(
        &self,
        text: &str,
        start: usize,
        end: usize,
        base_style: Style,
        display: &mut String,
        pieces: &mut Vec<RenderPiece>,
        invisible: &mut Vec<InvisibleSource>,
    ) {
        let mut bold = false;
        let mut italic = false;

        for token in &self.tokens[start..end] {
            match &token.kind {
                TokenKind::Text | TokenKind::Url => append_piece(
                    text,
                    display,
                    pieces,
                    token_range(token),
                    self.inline_style(base_style, bold, italic, false),
                ),
                TokenKind::InlineCode => append_piece(
                    text,
                    display,
                    pieces,
                    token_range(token),
                    self.inline_style(base_style, bold, italic, true),
                ),
                TokenKind::BoldStart => {
                    invisible.push(InvisibleSource {
                        source: token_range(token),
                        display_pos: display.len(),
                    });
                    bold = true;
                }
                TokenKind::BoldEnd => {
                    invisible.push(InvisibleSource {
                        source: token_range(token),
                        display_pos: display.len(),
                    });
                    bold = false;
                }
                TokenKind::ItalicStart => {
                    invisible.push(InvisibleSource {
                        source: token_range(token),
                        display_pos: display.len(),
                    });
                    italic = true;
                }
                TokenKind::ItalicEnd => {
                    invisible.push(InvisibleSource {
                        source: token_range(token),
                        display_pos: display.len(),
                    });
                    italic = false;
                }
                _ => {}
            }
        }
    }

    fn inline_style(&self, base: Style, bold: bool, italic: bool, code: bool) -> Style {
        let mut style = base;
        if code {
            style = style.patch(self.syntax.string);
        }
        if bold {
            style = style | Modifier::BOLD;
        }
        if italic {
            style = style | Modifier::ITALIC;
        }
        style
    }

    fn wrap_pieces(
        &mut self,
        display: &str,
        pieces: &[RenderPiece],
        invisible: &[InvisibleSource],
        widths: (usize, usize),
        cols: (u16, u16),
    ) {
        if display.is_empty() {
            self.push_line();
            for source in invisible {
                self.note_source_range(source.source.start, source.source.end);
            }
            return;
        }

        let mut wrapped_any = false;
        for wrapped in bwrap::wrap_ranges(display, widths.0, widths.1) {
            let base_col = if wrapped_any { cols.1 } else { cols.0 };
            wrapped_any = true;
            self.push_line();
            for source in invisible {
                if wrapped.start <= source.display_pos && source.display_pos <= wrapped.end {
                    self.note_source_range(source.source.start, source.source.end);
                }
            }
            for piece in pieces {
                let start = piece.display.start.max(wrapped.start);
                let end = piece.display.end.min(wrapped.end);
                if start >= end {
                    continue;
                }
                let source_start = piece.source.start + (start - piece.display.start);
                let source_end = piece.source.start + (end - piece.display.start);
                let prefix_width = UnicodeWidthStr::width(&display[wrapped.start..start]);
                let col = base_col.saturating_add(prefix_width.min(u16::MAX as usize) as u16);
                self.emit_segment(source_start..source_end, col, piece.style);
            }
        }

        if !wrapped_any {
            self.push_line();
            for source in invisible {
                self.note_source_range(source.source.start, source.source.end);
            }
            for piece in pieces {
                self.note_source_range(piece.source.start, piece.source.end);
            }
        }
    }

    fn layout_code_block(
        &mut self,
        text: &str,
        content: Range<usize>,
        source: Range<usize>,
        avail: usize,
    ) {
        if content.is_empty() {
            self.push_line();
            self.note_source_range(source.start, source.end);
            return;
        }

        let mut pos = content.start;
        while pos <= content.end {
            let (line_end, next) = line_bounds_limited(text.as_bytes(), pos, content.end);
            self.emit_verbatim(text, pos, line_end, avail, self.syntax.string);
            if next >= content.end {
                break;
            }
            pos = next;
        }
    }

    fn push_line(&mut self) {
        self.line_starts.push(self.segments.len() as u32);
        self.line_sources.push((u32::MAX, 0));
    }

    fn emit_verbatim(&mut self, text: &str, start: usize, end: usize, avail: usize, style: Style) {
        self.push_line();
        if start == end {
            self.note_source_range(start, end);
            return;
        }
        let avail = avail.max(1);
        let mut chunk_start = start;
        let mut width = 0usize;
        for (i, ch) in text[start..end].char_indices() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if width + w > avail && width > 0 {
                self.emit_segment(chunk_start..start + i, 0, style);
                self.push_line();
                chunk_start = start + i;
                width = 0;
            }
            width += w;
        }
        if chunk_start < end {
            self.emit_segment(chunk_start..end, 0, style);
        }
    }

    fn emit_segment(&mut self, range: Range<usize>, col: u16, style: Style) {
        self.note_source_range(range.start, range.end);
        if range.start < range.end {
            self.segments.push(Segment {
                col,
                start: range.start as u32,
                end: range.end as u32,
                style,
            });
        }
    }

    fn note_source_range(&mut self, start: usize, end: usize) {
        let Some((line_start, line_end)) = self.line_sources.last_mut() else {
            return;
        };
        let start = start.min(u32::MAX as usize) as u32;
        let end = end.min(u32::MAX as usize) as u32;
        if *line_start == u32::MAX {
            *line_start = start;
            *line_end = end;
        } else {
            *line_start = (*line_start).min(start);
            *line_end = (*line_end).max(end);
        }
    }

    fn source_start(range: (u32, u32)) -> Option<usize> {
        (range.0 != u32::MAX).then_some(range.0 as usize)
    }

    fn source_end(range: (u32, u32)) -> Option<usize> {
        (range.0 != u32::MAX).then_some(range.1 as usize)
    }
}

fn append_piece(
    text: &str,
    display: &mut String,
    pieces: &mut Vec<RenderPiece>,
    source: Range<usize>,
    style: Style,
) {
    if source.is_empty() {
        return;
    }
    let start = display.len();
    display.push_str(&text[source.clone()]);
    pieces.push(RenderPiece {
        source,
        display: start..display.len(),
        style,
    });
}

fn token_range(token: &Token) -> Range<usize> {
    token.range.start as usize..token.range.end as usize
}

fn line_bounds_limited(bytes: &[u8], pos: usize, limit: usize) -> (usize, usize) {
    let mut end = pos;
    while end < limit && bytes[end] != b'\n' {
        end += 1;
    }
    let next = if end < limit { end + 1 } else { end };
    let content_end = if end > pos && bytes[end - 1] == b'\r' {
        end - 1
    } else {
        end
    };
    (content_end, next)
}

fn estimate_lines(text: &str, avail: usize) -> usize {
    let target = avail.min(REFLOW_TARGET).max(1);
    let mut lines = 0usize;
    for line in text.lines() {
        lines = lines.saturating_add(UnicodeWidthStr::width(line).max(1).div_ceil(target));
    }
    lines.max(1)
}

/// Formats elapsed wall-clock milliseconds as a compact age label: minutes under
/// an hour (`40m`), tenths of an hour up to `9.9h`, whole hours through `48h`,
/// then whole days (`4d`).
pub fn format_age(elapsed_ms: u64) -> String {
    let minutes = elapsed_ms / 60_000;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    if elapsed_ms < 36_000_000 {
        let tenths = (elapsed_ms / 360_000).min(99);
        return format!("{}.{}h", tenths / 10, tenths % 10);
    }
    if elapsed_ms <= 172_800_000 {
        return format!("{}h", elapsed_ms / 3_600_000);
    }
    format!("{}d", elapsed_ms / 86_400_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_with_notices(count: usize) -> VirtualChatBuffer {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for i in 0..count {
            buf.push_notice("user", format!("message {i}"));
        }
        buf
    }

    impl VirtualChatBuffer {
        fn push_test(&mut self, sender: &str, body: &str, timestamp_ms: u64, local: bool) {
            let id = self.messages.len() as u64;
            self.messages.push(ChatEntry {
                id,
                sender: sender.to_string(),
                body: body.to_string(),
                timestamp_ms,
                local,
                file_transfer_id: None,
                links: crate::markdown::link_ranges(body),
                expanded: false,
                layout: MessageLayout::new(),
            });
            self.trim_front();
        }
    }

    fn heading_ids(buf: &mut VirtualChatBuffer, width: u16) -> Vec<u64> {
        buf.visible_lines(width, 10_000, 0)
            .into_iter()
            .filter(|row| row.kind == LineKind::Heading)
            .map(|row| buf.message(row.message).id)
            .collect()
    }

    /// A fenced code block laying out to exactly `n` rendered content lines.
    fn fenced(n: usize) -> String {
        let mut body = String::from("```\n");
        for i in 0..n {
            if i > 0 {
                body.push('\n');
            }
            body.push_str(&i.to_string());
        }
        body.push_str("\n```");
        body
    }

    fn kinds(rows: &[VisibleLine]) -> Vec<LineKind> {
        rows.iter().map(|row| row.kind).collect()
    }

    fn headings(rows: &[VisibleLine]) -> usize {
        rows.iter()
            .filter(|row| row.kind == LineKind::Heading)
            .count()
    }

    fn selected_heading_blocks(
        buf: &VirtualChatBuffer,
        rows: &[VisibleLine],
    ) -> Vec<(usize, usize)> {
        rows.iter()
            .copied()
            .filter(|row| buf.is_header_selected(*row))
            .map(|row| (row.block_first, row.block_last))
            .collect()
    }

    #[test]
    fn format_age_covers_each_unit_boundary() {
        let cases = [
            (0u64, "0m"),
            (59 * 60_000, "59m"),
            (3_600_000, "1.0h"),
            (5_400_000, "1.5h"),
            (35_640_000, "9.9h"),
            (36_000_000, "10h"),
            (115_200_000, "32h"),
            (172_800_000, "48h"),
            (176_400_000, "2d"),
            (345_600_000, "4d"),
        ];
        for (elapsed, expected) in cases {
            assert_eq!(format_age(elapsed), expected, "elapsed {elapsed}ms");
        }
    }

    #[test]
    fn same_sender_within_window_shares_one_heading() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "hello", 1_000_000, false);
        buf.push_test("alice", "world", 1_000_000 + GROUP_GAP_MS, false);
        let rows = buf.visible_lines(40, 50, 0);
        assert_eq!(headings(&rows), 1);
        assert_eq!(rows.len(), 3); // heading + two body lines
    }

    #[test]
    fn gap_over_window_breaks_the_block() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "hello", 1_000_000, false);
        buf.push_test("alice", "world", 1_000_000 + GROUP_GAP_MS + 1, false);
        let rows = buf.visible_lines(40, 50, 0);
        assert_eq!(headings(&rows), 2);
    }

    #[test]
    fn sender_change_breaks_the_block() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "hello", 1_000_000, false);
        buf.push_test("bob", "world", 1_000_000 + 1_000, false);
        assert_eq!(headings(&buf.visible_lines(40, 50, 0)), 2);
    }

    #[test]
    fn block_cap_groups_twelve_lines_and_splits_thirteen() {
        let group = |count: usize| {
            let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
            for i in 0..count {
                buf.push_test("alice", "x", 1_000_000 + i as u64 * 1_000, false);
            }
            headings(&buf.visible_lines(40, 100, 0))
        };
        assert_eq!(group(12), 1); // twelve single-line messages stay together
        assert_eq!(group(13), 2); // the thirteenth line forces a new heading
    }

    #[test]
    fn notices_never_group() {
        let mut buf = buffer_with_notices(3);
        assert_eq!(headings(&buf.visible_lines(40, 50, 0)), 3);
    }

    #[test]
    fn long_message_collapses_to_preview_plus_ellipsis() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", &fenced(13), 1_000_000, false);
        let rows = buf.visible_lines(40, 50, 0);
        assert_eq!(rows.len(), 1 + COLLAPSE_SHOW + 1);
        assert_eq!(rows.first().map(|r| r.kind), Some(LineKind::Heading));
        assert_eq!(rows.last().map(|r| r.kind), Some(LineKind::Ellipsis));
        assert_eq!(
            kinds(&rows[1..=COLLAPSE_SHOW]),
            vec![LineKind::Body; COLLAPSE_SHOW]
        );
        assert!(buf.is_collapsed(0));
        assert!(buf.is_collapsible(0, 40));
    }

    #[test]
    fn exactly_twelve_lines_renders_in_full() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", &fenced(12), 1_000_000, false);
        let rows = buf.visible_lines(40, 50, 0);
        assert_eq!(rows.len(), 1 + 12); // heading + twelve body lines
        assert!(rows.iter().all(|r| r.kind != LineKind::Ellipsis));
        assert!(!buf.is_collapsible(0, 40));
    }

    #[test]
    fn expanding_a_long_message_shows_every_line() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", &fenced(13), 1_000_000, false);
        let _ = buf.visible_lines(40, 50, 0);
        assert!(buf.is_collapsed(0) && !buf.is_expanded(0));
        assert!(buf.toggle_expand(0, 40));
        let rows = buf.visible_lines(40, 50, 0);
        assert_eq!(rows.len(), 1 + 13);
        assert!(rows.iter().all(|r| r.kind != LineKind::Ellipsis));
        assert!(buf.is_expanded(0) && !buf.is_collapsed(0));
    }

    #[test]
    fn block_first_line_is_a_heading() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "hi", 1_000_000, false);
        let rows = buf.visible_lines(40, 50, 0);
        assert_eq!(rows.first().map(|r| r.kind), Some(LineKind::Heading));
    }

    #[test]
    fn headings_stay_fixed_as_messages_arrive() {
        // Five-line messages pack two-to-a-block (10 lines) with a third forcing a
        // new heading. Forward packing keeps earlier headings anchored to the same
        // message as the run grows; backward packing would shuffle them.
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        let mut ts = 1_000_000;
        for _ in 0..3 {
            buf.push_test("alice", &fenced(5), ts, false);
            ts += 1_000;
        }
        let before = heading_ids(&mut buf, 40);
        buf.push_test("alice", &fenced(5), ts, false);
        let after = heading_ids(&mut buf, 40);
        for id in &before {
            assert!(
                after.contains(id),
                "heading on message {id} moved after a new message"
            );
        }
    }

    #[test]
    fn selected_header_follows_anchor_when_reflow_changes_blocks() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for i in 0..5 {
            buf.push_test(
                "alice",
                "aa bb cc dd ee ff",
                1_000_000 + i as u64 * 1_000,
                false,
            );
        }

        let narrow = buf.visible_lines(7, 100, 0);
        assert!(narrow.iter().any(|row| {
            row.kind == LineKind::Heading && row.block_first == 4 && row.block_last == 4
        }));
        buf.select_message(4);

        let wide = buf.visible_lines(40, 100, 0);
        assert_eq!(selected_heading_blocks(&buf, &wide), vec![(0, 4)]);
    }

    #[test]
    fn selected_header_text_copies_the_current_block_bodies() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "first", 1_000_000, false);
        buf.push_test("alice", "second", 1_001_000, false);
        buf.select_message(0);

        assert_eq!(
            buf.selected_header_text(40).as_deref(),
            Some("first\nsecond")
        );
    }

    #[test]
    fn total_lines_exact_matches_emitted_row_count() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "hello", 1_000_000, false);
        buf.push_test("alice", "world", 1_000_000 + 1_000, false);
        buf.push_test("bob", &fenced(13), 1_000_000 + 200_000, false);
        buf.push_notice("system", "joined");
        let total = buf.total_lines_exact(40);
        assert_eq!(total, buf.visible_lines(40, 10_000, 0).len());
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

    #[test]
    fn paragraph_newlines_render_as_separate_body_rows() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "Alpha.\nBeta.", 1_000_000, false);
        let _ = buf.total_lines_exact(80);

        assert_eq!(buf.messages[0].layout.lines(), 2);
        assert_eq!(
            kinds(&buf.visible_lines(80, 10, 0)),
            vec![LineKind::Heading, LineKind::Body, LineKind::Body]
        );
    }

    #[test]
    fn selected_text_preserves_whitespace_between_wrapped_rows() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "alpha beta", 1_000_000, false);
        let _ = buf.total_lines_exact(5);
        assert_eq!(buf.messages[0].layout.lines(), 2);

        buf.begin_selection((0, 0));
        buf.extend_selection((0, 1));

        assert_eq!(buf.selected_text().as_deref(), Some("alpha beta"));
    }

    #[test]
    fn selected_text_preserves_original_newlines_when_message_is_selected() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "alpha\nbeta", 1_000_000, false);
        let _ = buf.total_lines_exact(40);

        buf.begin_selection((0, 0));
        buf.extend_selection((0, buf.messages[0].layout.lines() - 1));

        assert_eq!(buf.selected_text().as_deref(), Some("alpha\nbeta"));
    }
}
