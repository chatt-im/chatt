use std::ops::Range;

use extui::{Style, vt::Modifier};
use rpc::control::ChatMessage;
use rpc::ids::FileTransferId;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::SyntaxTheme;
use chatt_message_format::{
    Token, TokenKind,
    highlight::{self, HlClass},
};

/// Wrapped body lines beyond this collapse a lone message behind an expander.
const COLLAPSE_LIMIT: usize = 12;
/// Body lines shown while a long message is collapsed.
const COLLAPSE_SHOW: usize = 10;
/// Maximum gap between adjacent same-sender messages that still groups them.
const GROUP_GAP_MS: u64 = 90_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoticeId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub col: u16,
    pub start: u32,
    pub end: u32,
    pub style: Style,
    /// Whether `start..end` indexes the layout's synthetic text (a resolved
    /// message-reference pill label) instead of the message body.
    pub synth: bool,
}

/// A message reference found in a body at push time.
///
/// `target` is `None` when the code failed to decode; `label` is `None` when
/// the referenced message is not in the local buffer (or in another room), in
/// which case the literal `@@code` renders dimmed instead of a pill.
pub struct MsgRefSpan {
    pub range: Range<u32>,
    pub target: Option<rpc::msgref::MessageRef>,
    pub label: Option<String>,
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn block_contains(self, message: usize) -> bool {
        self.block_first <= message && message <= self.block_last
    }
}

pub struct ChatEntry {
    pub id: u64,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub local: bool,
    /// The remote DM content is authenticated by a key that has not been
    /// independently verified.
    pub unverified: bool,
    /// The body was replaced by an edit; the heading renders an `(edited)`
    /// marker.
    pub edited: bool,
    /// The server file transfer this message announces, when it is a file. Keys
    /// the render-time progress overlay in [`crate::app::room::RoomSession`].
    pub file_transfer_id: Option<FileTransferId>,
    /// Byte ranges of `http`/`https` URLs in `body`, computed once at push time.
    pub links: Vec<Range<u32>>,
    /// Message references in `body`, decoded and resolved once at push time.
    pub refs: Vec<MsgRefSpan>,
    /// Whether a collapsible (over [`COLLAPSE_LIMIT`] lines) message is expanded.
    expanded: bool,
    notice_id: Option<NoticeId>,
    pub notice_kind: Option<NoticeKind>,
    layout: MessageLayout,
}

/// A run of one or more consecutive messages rendered under a single heading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Default)]
struct LayoutIndex {
    width: u16,
    valid: bool,
    line_counts: Vec<usize>,
    blocks: Vec<Block>,
    rows: FenwickRows,
    #[cfg(test)]
    full_rebuilds: usize,
}

impl LayoutIndex {
    fn invalidate(&mut self) {
        self.valid = false;
        self.rows.clear();
    }

    fn clear(&mut self) {
        self.valid = false;
        self.line_counts.clear();
        self.blocks.clear();
        self.rows.clear();
    }

    fn total_rows(&self) -> usize {
        self.rows.total().max(1)
    }

    fn block_containing_message(&self, message: usize) -> Option<usize> {
        let index = self.blocks.partition_point(|block| block.last < message);
        self.blocks
            .get(index)
            .is_some_and(|block| block.first <= message)
            .then_some(index)
    }
}

#[derive(Default)]
struct FenwickRows {
    tree: Vec<usize>,
    total: usize,
}

impl FenwickRows {
    fn clear(&mut self) {
        self.tree.clear();
        self.total = 0;
    }

    fn len(&self) -> usize {
        self.tree.len()
    }

    fn total(&self) -> usize {
        self.total
    }

    fn push(&mut self, value: usize) {
        let index = self.tree.len();
        let one_based = index + 1;
        let lowbit = one_based & one_based.wrapping_neg();
        let start = one_based - lowbit;
        let previous = self
            .prefix_sum(index)
            .saturating_sub(self.prefix_sum(start));
        self.tree.push(previous.saturating_add(value));
        self.total = self.total.saturating_add(value);
    }

    fn truncate(&mut self, len: usize) {
        self.tree.truncate(len);
        self.total = self.prefix_sum(len);
    }

    fn set(&mut self, index: usize, value: usize) {
        let current = self.range_sum(index, index + 1);
        if value >= current {
            self.add(index, value - current);
            self.total = self.total.saturating_add(value - current);
        } else {
            self.sub(index, current - value);
            self.total = self.total.saturating_sub(current - value);
        }
    }

    fn prefix_sum(&self, count: usize) -> usize {
        let mut index = count.min(self.tree.len());
        let mut sum = 0usize;
        while index > 0 {
            sum = sum.saturating_add(self.tree[index - 1]);
            index &= index - 1;
        }
        sum
    }

    fn range_sum(&self, start: usize, end: usize) -> usize {
        self.prefix_sum(end).saturating_sub(self.prefix_sum(start))
    }

    fn row_to_block(&self, row: usize) -> Option<usize> {
        if self.tree.is_empty() {
            return None;
        }
        let row = row.min(self.total.saturating_sub(1));
        let mut index = 0usize;
        let mut sum = 0usize;
        let mut bit = self.tree.len().next_power_of_two();
        while bit > 0 {
            let next = index + bit;
            if next <= self.tree.len() && sum.saturating_add(self.tree[next - 1]) <= row {
                sum = sum.saturating_add(self.tree[next - 1]);
                index = next;
            }
            bit >>= 1;
        }
        Some(index.min(self.tree.len() - 1))
    }

    fn add(&mut self, index: usize, delta: usize) {
        let mut one_based = index + 1;
        while one_based <= self.tree.len() {
            self.tree[one_based - 1] = self.tree[one_based - 1].saturating_add(delta);
            one_based += one_based & one_based.wrapping_neg();
        }
    }

    fn sub(&mut self, index: usize, delta: usize) {
        let mut one_based = index + 1;
        while one_based <= self.tree.len() {
            self.tree[one_based - 1] = self.tree[one_based - 1].saturating_sub(delta);
            one_based += one_based & one_based.wrapping_neg();
        }
    }
}

/// A body-line position: wrapped visible `line` within `message`. `Ord` is
/// `(message, line)` lexicographic, used to normalize visual ranges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cursor {
    pub message: usize,
    pub line: usize,
}

pub struct VirtualChatBuffer {
    messages: Vec<ChatEntry>,
    /// Advances whenever searchable message text or ordering changes.
    revision: u64,
    /// Advances only when existing index offsets may have changed. Search can
    /// append incrementally while this remains stable.
    reindex_revision: u64,
    max_messages: usize,
    scroll_offset: usize,
    /// Navigation cursor; `None` only while the buffer is empty. A stale
    /// `line` (after reflow or collapse) is clamped lazily by
    /// [`Self::ensure_cursor`].
    cursor: Option<Cursor>,
    /// `Some` means a line-wise visual selection spans `anchor..=cursor`,
    /// order-normalized. Keyboard visual mode and mouse drags share this.
    anchor: Option<Cursor>,
    /// A mouse drag is in progress; releasing with `anchor == cursor` (a
    /// click) clears the anchor, leaving a plain cursor move.
    dragging: bool,
    syntax: SyntaxTheme,
    room_id: Option<rpc::ids::RoomId>,
    next_notice_id: u64,
    layout_index: LayoutIndex,
    /// Advances on any mutation that can move existing rendered rows: edits,
    /// removals, prepends, eviction, collapse toggles, reflow. Pure tail
    /// appends leave it stable, so an unchanged epoch plus a viewport-top
    /// delta means the visible rows merely shifted and the renderer may
    /// scroll them instead of rewriting.
    layout_epoch: u64,
}

impl VirtualChatBuffer {
    pub fn new(max_messages: usize, syntax: SyntaxTheme) -> Self {
        Self {
            messages: Vec::new(),
            revision: 0,
            reindex_revision: 0,
            max_messages: max_messages.max(1),
            scroll_offset: 0,
            cursor: None,
            anchor: None,
            dragging: false,
            syntax,
            room_id: None,
            next_notice_id: 1,
            layout_index: LayoutIndex::default(),
            layout_epoch: 0,
        }
    }

    /// Sets the room this buffer displays, the scope against which message
    /// references resolve.
    pub fn set_room_id(&mut self, room_id: rpc::ids::RoomId) {
        self.room_id = Some(room_id);
    }

    pub fn room_id(&self) -> Option<rpc::ids::RoomId> {
        self.room_id
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
        self.layout_index.invalidate();
        self.bump_layout_epoch();
    }

    fn build_entry(&self, message: ChatMessage, local: bool, unverified: bool) -> ChatEntry {
        let inline = chatt_message_format::inline_ranges(&message.body);
        let refs = self.build_ref_spans(&message.body, inline.refs);
        ChatEntry {
            id: message.message_id.0,
            sender: message.sender_name,
            body: message.body,
            timestamp_ms: message.timestamp_ms,
            local,
            unverified: unverified && !local,
            edited: message.flags.edited(),
            file_transfer_id: message.file_transfer_id,
            links: inline.urls,
            refs,
            expanded: false,
            notice_id: None,
            notice_kind: None,
            layout: MessageLayout::new(),
        }
    }

    /// Replaces the message with `message_id` by the folded `message`,
    /// recomputing links, references, and layout while keeping the entry's
    /// local flag and expansion. Returns whether the message was resident.
    #[cfg(test)]
    pub fn edit_message(&mut self, message_id: u64, message: ChatMessage) -> bool {
        let unverified = self
            .find_message(message_id)
            .map(|index| self.messages[index].unverified)
            .unwrap_or(false);
        self.edit_authenticated_message(message_id, message, unverified)
    }

    pub fn edit_authenticated_message(
        &mut self,
        message_id: u64,
        message: ChatMessage,
        unverified: bool,
    ) -> bool {
        let Some(index) = self.find_message(message_id) else {
            return false;
        };
        let expanded = self.messages[index].expanded;
        let local = self.messages[index].local;
        let mut entry = self.build_entry(message, local, unverified);
        entry.expanded = expanded;
        self.messages[index] = entry;
        self.bump_reindex_revision();
        self.layout_index.invalidate();
        self.bump_layout_epoch();
        true
    }

    /// Removes the message with `message_id`, fixing up the cursor and anchor
    /// like a notice removal. Returns whether the message was resident.
    pub fn remove_message(&mut self, message_id: u64) -> bool {
        let Some(index) = self.find_message(message_id) else {
            return false;
        };
        self.remove_at(index);
        true
    }

    #[cfg(test)]
    pub fn push_chat(&mut self, message: ChatMessage, local: bool) {
        self.push_authenticated_chat(message, local, false);
    }

    pub fn push_authenticated_chat(&mut self, message: ChatMessage, local: bool, unverified: bool) {
        let old_len = self.messages.len();
        let entry = self.build_entry(message, local, unverified);
        self.messages.push(entry);
        self.bump_revision();
        self.repair_layout_index_after_append(old_len);
        self.follow_bottom_after_push(old_len);
        self.trim_front();
    }

    /// Advances the cursor onto a just-pushed newest message when the view is
    /// following the bottom: only with `scroll_offset == 0`, no visual anchor,
    /// and the cursor already on the previously-newest message. Scrolled-up or
    /// mid-selection, the cursor sticks to its message through index shifts.
    fn follow_bottom_after_push(&mut self, old_len: usize) {
        if self.scroll_offset != 0 || self.anchor.is_some() {
            return;
        }
        let Some(cursor) = &mut self.cursor else {
            return;
        };
        if old_len > 0 && cursor.message == old_len - 1 {
            // The line is clamped to the message's last visible line lazily.
            *cursor = Cursor {
                message: old_len,
                line: usize::MAX,
            };
        }
    }

    /// Inserts a batch of older messages before the first entry. `messages`
    /// must be sorted by `message_id` and older than every resident message.
    /// The bottom-relative scroll is untouched, so the view does not jump;
    /// cursor and anchor shift with the entries they name.
    #[cfg(test)]
    pub fn prepend_chat(&mut self, messages: Vec<(ChatMessage, bool)>) {
        self.prepend_authenticated_chat(
            messages
                .into_iter()
                .map(|(message, local)| (message, local, false))
                .collect(),
        );
    }

    pub fn prepend_authenticated_chat(&mut self, messages: Vec<(ChatMessage, bool, bool)>) {
        if messages.is_empty() {
            return;
        }
        let count = messages.len();
        let entries: Vec<ChatEntry> = messages
            .into_iter()
            .map(|(message, local, unverified)| self.build_entry(message, local, unverified))
            .collect();
        self.messages.splice(0..0, entries);
        self.bump_reindex_revision();
        if let Some(cursor) = &mut self.cursor {
            cursor.message += count;
        }
        if let Some(anchor) = &mut self.anchor {
            anchor.message += count;
        }
        self.layout_index.invalidate();
        self.bump_layout_epoch();
        self.trim_front();
    }

    /// Inserts one message at its sorted position among real messages,
    /// leaving notices pinned where they were pushed. For the rare history
    /// straggler that lands between resident messages.
    #[cfg(test)]
    pub fn insert_chat(&mut self, message: ChatMessage, local: bool) {
        self.insert_authenticated_chat(message, local, false);
    }

    pub fn insert_authenticated_chat(
        &mut self,
        message: ChatMessage,
        local: bool,
        unverified: bool,
    ) {
        let key = message.message_id.0;
        let index = self
            .messages
            .iter()
            .rposition(|entry| entry.id != 0 && entry.id < key)
            .map_or(0, |newest_older| newest_older + 1);
        let entry = self.build_entry(message, local, unverified);
        self.messages.insert(index, entry);
        self.bump_reindex_revision();
        if let Some(cursor) = &mut self.cursor
            && cursor.message >= index
        {
            cursor.message += 1;
        }
        if let Some(anchor) = &mut self.anchor
            && anchor.message >= index
        {
            anchor.message += 1;
        }
        self.layout_index.invalidate();
        self.bump_layout_epoch();
        self.trim_front();
    }

    /// Re-marks which messages are locally sent, keyed by message id. Heading
    /// grouping reads `local`, so the row index is invalidated only when an
    /// entry actually changes.
    /// Notices and keys the callback does not know keep their flag.
    pub fn set_local_flags(&mut self, local_for: impl Fn(u64) -> Option<bool>) {
        let mut changed = false;
        for entry in &mut self.messages {
            if entry.id == 0 {
                continue;
            }
            if let Some(local) = local_for(entry.id) {
                changed |= entry.local != local;
                entry.local = local;
            }
        }
        if changed {
            self.layout_index.invalidate();
            self.bump_layout_epoch();
        }
    }

    #[cfg(test)]
    pub fn push_notice(&mut self, sender: impl Into<String>, body: impl Into<String>) -> NoticeId {
        self.push_notice_with_kind(sender, body, NoticeKind::Info)
    }

    pub fn push_notice_with_kind(
        &mut self,
        sender: impl Into<String>,
        body: impl Into<String>,
        kind: NoticeKind,
    ) -> NoticeId {
        let body = body.into();
        let inline = chatt_message_format::inline_ranges(&body);
        let refs = self.build_ref_spans(&body, inline.refs);
        let notice_id = NoticeId(self.next_notice_id);
        self.next_notice_id = self.next_notice_id.wrapping_add(1).max(1);
        let old_len = self.messages.len();
        self.messages.push(ChatEntry {
            id: 0,
            sender: sender.into(),
            body,
            timestamp_ms: 0,
            local: false,
            unverified: false,
            edited: false,
            file_transfer_id: None,
            links: inline.urls,
            refs,
            expanded: false,
            notice_id: Some(notice_id),
            notice_kind: Some(kind),
            layout: MessageLayout::new(),
        });
        self.bump_revision();
        self.repair_layout_index_after_append(old_len);
        self.follow_bottom_after_push(old_len);
        self.trim_front();
        notice_id
    }

    pub fn remove_notice(&mut self, notice_id: NoticeId) -> bool {
        let Some(index) = self
            .messages
            .iter()
            .position(|entry| entry.notice_id == Some(notice_id))
        else {
            return false;
        };
        self.remove_at(index);
        true
    }

    fn remove_at(&mut self, index: usize) {
        self.messages.remove(index);
        self.bump_reindex_revision();
        let anchor_removed = self.anchor.is_some_and(|anchor| anchor.message == index);
        let cursor_removed = self.cursor.is_some_and(|cursor| cursor.message == index);
        if anchor_removed || cursor_removed {
            self.anchor = None;
        } else if let Some(anchor) = &mut self.anchor
            && anchor.message > index
        {
            anchor.message -= 1;
        }
        if cursor_removed {
            // Land on the nearest surviving message's first line.
            if self.messages.is_empty() {
                self.cursor = None;
            } else {
                self.cursor = Some(Cursor {
                    message: index.min(self.messages.len() - 1),
                    line: 0,
                });
            }
        } else if let Some(cursor) = &mut self.cursor
            && cursor.message > index
        {
            cursor.message -= 1;
        }
        self.layout_index.invalidate();
        self.bump_layout_epoch();
    }

    fn build_ref_spans(&self, body: &str, ranges: Vec<Range<u32>>) -> Vec<MsgRefSpan> {
        let mut spans = Vec::with_capacity(ranges.len());
        for range in ranges {
            let code_start = range.start as usize + rpc::msgref::REF_PREFIX.len();
            let target = rpc::msgref::MessageRef::decode(&body[code_start..range.end as usize]);
            let label = target.and_then(|target| self.resolve_label(target));
            spans.push(MsgRefSpan {
                range,
                target,
                label,
            });
        }
        spans
    }

    /// Resolves a reference target to its pill label when the message is in
    /// this buffer and this room, the same lookup pushes use. The web feed
    /// resolves through this so both views label references identically.
    pub fn ref_label_for(&self, target: rpc::msgref::MessageRef) -> Option<String> {
        self.resolve_label(target)
    }

    /// The reference and pill label of the message at `index`, for the composer
    /// reference picker. `None` for notices, which have no durable key.
    pub fn ref_for_index(&self, index: usize) -> Option<(rpc::msgref::MessageRef, String)> {
        let room_id = self.room_id?;
        let entry = self.messages.get(index)?;
        if entry.id == 0 {
            return None;
        }
        let target = rpc::msgref::MessageRef {
            room_id,
            message_id: rpc::ids::MessageId(entry.id),
        };
        Some((target, ref_label(&entry.sender, &entry.body)))
    }

    fn resolve_label(&self, target: rpc::msgref::MessageRef) -> Option<String> {
        if self.room_id != Some(target.room_id) {
            return None;
        }
        let index = self.find_message(target.message_id.0)?;
        let entry = &self.messages[index];
        Some(ref_label(&entry.sender, &entry.body))
    }

    /// Returns the index of the message with the given durable key, preferring
    /// the newest on the (never expected) chance of a duplicate.
    pub fn find_message(&self, message_id: u64) -> Option<usize> {
        for (index, entry) in self.messages.iter().enumerate().rev() {
            if entry.id == message_id && entry.notice_id.is_none() {
                return Some(index);
            }
        }
        None
    }

    pub fn clear(&mut self) {
        if !self.messages.is_empty() {
            self.bump_reindex_revision();
        }
        self.messages.clear();
        self.scroll_offset = 0;
        self.cursor = None;
        self.anchor = None;
        self.dragging = false;
        self.room_id = None;
        self.next_notice_id = 1;
        self.layout_index.clear();
        self.bump_layout_epoch();
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

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn reindex_revision(&self) -> u64 {
        self.reindex_revision
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn bump_reindex_revision(&mut self) {
        self.bump_revision();
        self.reindex_revision = self.reindex_revision.wrapping_add(1);
    }

    pub(crate) fn layout_epoch(&self) -> u64 {
        self.layout_epoch
    }

    fn bump_layout_epoch(&mut self) {
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
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
        let seg = entry.layout.segment_at(&entry.body, line, col_in_line)?;
        if seg.synth {
            return None;
        }
        let range = entry
            .links
            .iter()
            .find(|r| r.start < seg.end && seg.start < r.end)?;
        Some(&entry.body[range.start as usize..range.end as usize])
    }

    /// Returns the decoded message reference at `col_in_line` on wrapped `line`
    /// of `message`, whether rendered as a pill or as a literal code.
    pub fn ref_at(
        &self,
        message: usize,
        line: usize,
        col_in_line: u16,
    ) -> Option<rpc::msgref::MessageRef> {
        let entry = self.messages.get(message)?;
        if entry.refs.is_empty() {
            return None;
        }
        let seg = entry.layout.segment_at(&entry.body, line, col_in_line)?;
        if seg.synth {
            let index = entry.layout.pill_ref_at(seg)?;
            return entry.refs.get(index)?.target;
        }
        let span = entry
            .refs
            .iter()
            .find(|span| span.range.start < seg.end && seg.start < span.range.end)?;
        span.target
    }

    /// Returns the text a segment displays: a body slice, or a slice of the
    /// layout's synthetic pill text.
    pub fn segment_text(&self, message: usize, seg: &Segment) -> &str {
        let entry = &self.messages[message];
        entry.layout.segment_str(&entry.body, seg)
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

    pub fn is_at_top(&mut self, width: u16, height: u16) -> bool {
        self.scroll_offset == self.max_scroll(width, height)
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

    /// Places the cursor and anchor at `pos` and starts a mouse drag.
    pub fn begin_drag(&mut self, pos: Cursor) {
        self.cursor = Some(pos);
        self.anchor = Some(pos);
        self.dragging = true;
    }

    /// Moves the cursor of an in-progress drag to `pos`; the anchor stays.
    pub fn drag_to(&mut self, pos: Cursor) {
        if self.dragging {
            self.cursor = Some(pos);
        }
    }

    /// Finishes a drag. A click (no movement since [`Self::begin_drag`])
    /// clears the anchor, leaving a plain cursor move.
    pub fn end_drag(&mut self) {
        if self.dragging && self.anchor == self.cursor {
            self.anchor = None;
        }
        self.dragging = false;
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Whether the current drag has not moved off its anchor, i.e. the pointer
    /// was pressed and is being released without dragging.
    pub fn drag_is_click(&self) -> bool {
        self.dragging && self.anchor.is_some() && self.anchor == self.cursor
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    /// Returns a valid cursor, defaulting to the last body line of the newest
    /// message and clamping stale coordinates. `None` only when empty.
    pub fn ensure_cursor(&mut self, width: u16) -> Option<Cursor> {
        if self.messages.is_empty() {
            self.cursor = None;
            self.anchor = None;
            return None;
        }
        if self.cursor.is_none() {
            self.cursor = Some(Cursor {
                message: self.messages.len() - 1,
                line: usize::MAX,
            });
        }
        self.clamp_positions(width);
        self.cursor
    }

    /// Moves the cursor to the first body line of `message`, clearing any
    /// visual anchor. Used by reference jumps.
    pub fn set_cursor_to_message(&mut self, message: usize) -> Option<Cursor> {
        if message >= self.messages.len() {
            return None;
        }
        self.cursor = Some(Cursor { message, line: 0 });
        self.anchor = None;
        self.cursor
    }

    /// Moves the cursor `delta` visible body lines, walking across messages,
    /// clamping at the buffer edges. Lines hidden by collapse are never
    /// visited.
    pub fn move_cursor_line(&mut self, delta: isize, width: u16) -> Option<Cursor> {
        let mut cursor = self.ensure_cursor(width)?;
        for _ in 0..delta.unsigned_abs() {
            let next = if delta > 0 {
                self.next_body_pos(cursor, width)
            } else {
                self.prev_body_pos(cursor, width)
            };
            let Some(next) = next else { break };
            cursor = next;
        }
        self.cursor = Some(cursor);
        self.cursor
    }

    /// Moves the cursor to the first body line of the block `delta` blocks
    /// away (a sender-group heading boundary), clamping at the ends.
    pub fn move_cursor_paragraph(&mut self, delta: isize, width: u16) -> Option<Cursor> {
        let cursor = self.ensure_cursor(width)?;
        self.ensure_layout_index(width);
        let current = self.layout_index.block_containing_message(cursor.message)?;
        let block_count = self.layout_index.blocks.len();
        let next = (current as isize + delta).clamp(0, block_count as isize - 1) as usize;
        self.cursor = Some(Cursor {
            message: self.layout_index.blocks[next].first,
            line: 0,
        });
        self.cursor
    }

    pub fn cursor_to_first(&mut self) -> Option<Cursor> {
        if self.messages.is_empty() {
            return None;
        }
        self.cursor = Some(Cursor {
            message: 0,
            line: 0,
        });
        self.cursor
    }

    pub fn cursor_to_last(&mut self, width: u16) -> Option<Cursor> {
        if self.messages.is_empty() {
            return None;
        }
        let last = self.messages.len() - 1;
        self.cursor = Some(Cursor {
            message: last,
            line: self.visible_body_lines(last, width) - 1,
        });
        self.cursor
    }

    /// Toggles visual-line mode: anchors a selection at the cursor, or clears
    /// an existing one. Returns whether a selection is now active.
    pub fn toggle_visual_anchor(&mut self, width: u16) -> bool {
        if self.anchor.take().is_some() {
            return false;
        }
        self.anchor = self.ensure_cursor(width);
        self.anchor.is_some()
    }

    /// Clears the visual selection, returning whether one existed.
    pub fn clear_visual_anchor(&mut self) -> bool {
        self.anchor.take().is_some()
    }

    pub fn has_visual(&self) -> bool {
        self.anchor.is_some()
    }

    pub fn is_cursor_line(&self, message: usize, line: usize) -> bool {
        self.cursor == Some(Cursor { message, line })
    }

    /// Returns whether the given `(message, line)` falls within the visual
    /// selection.
    pub fn is_visual(&self, message: usize, line: usize) -> bool {
        let Some((lo, hi)) = self.visual_bounds() else {
            return false;
        };
        let pos = Cursor { message, line };
        lo <= pos && pos <= hi
    }

    /// The visual range's ordered `(oldest, newest)` endpoints, when active.
    fn visual_bounds(&self) -> Option<(Cursor, Cursor)> {
        let anchor = self.anchor?;
        let cursor = self.cursor?;
        if anchor <= cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    /// Message indexes targeted by an action on the current chat selection.
    /// A plain cursor targets its whole message; a visual-line range targets
    /// each message touched by either endpoint or any line between them.
    pub fn selected_message_indices(&mut self, width: u16) -> Vec<usize> {
        let Some(cursor) = self.ensure_cursor(width) else {
            return Vec::new();
        };
        let Some((lo, hi)) = self.visual_bounds() else {
            return vec![cursor.message];
        };
        (lo.message..=hi.message).collect()
    }

    /// Copies original body text covered by the visually selected rendered
    /// rows, content only (no sender column). Wrapped rows from the same
    /// message are sliced as one source range so clipboard text keeps the
    /// message's whitespace instead of the display wrapper's trimmed fragments.
    pub fn visual_text(&mut self, width: u16) -> Option<String> {
        let width = width.max(1);
        let (lo, hi) = self.visual_bounds()?;
        let mut out = String::new();
        let mut first = true;
        for message in lo.message..=hi.message.min(self.messages.len().saturating_sub(1)) {
            let lines = self.visible_body_lines(message, width);
            let start = if message == lo.message { lo.line } else { 0 };
            if start >= lines {
                continue;
            }
            let end = if message == hi.message {
                hi.line
            } else {
                lines - 1
            };
            let end = end.min(lines - 1);
            if !first {
                out.push('\n');
            }
            first = false;
            let entry = &self.messages[message];
            if start == 0 && end == lines - 1 {
                if lines == entry.layout.lines().max(1) {
                    out.push_str(&entry.body);
                } else {
                    let range = entry.layout.source_range(start, end, entry.body.len());
                    out.push_str(&entry.body[range]);
                }
            } else {
                let range = entry.layout.source_range(start, end, entry.body.len());
                out.push_str(&entry.body[range]);
            }
        }
        Some(out)
    }

    /// The original body text the cursor's wrapped row displays.
    pub fn cursor_line_text(&self) -> Option<String> {
        let cursor = self.cursor?;
        let entry = self.messages.get(cursor.message)?;
        let lines = entry.layout.lines().max(1);
        if cursor.line >= lines {
            return None;
        }
        if lines == 1 {
            return Some(entry.body.clone());
        }
        let range = entry
            .layout
            .source_range(cursor.line, cursor.line, entry.body.len());
        Some(entry.body[range].to_string())
    }

    pub fn cursor_message_body(&self) -> Option<&str> {
        let cursor = self.cursor?;
        Some(self.messages.get(cursor.message)?.body.as_str())
    }

    /// The first decodable reference contained in the cursor's message, for
    /// keyboard-driven "open the reference in this message".
    pub fn cursor_ref(&self) -> Option<rpc::msgref::MessageRef> {
        let cursor = self.cursor?;
        let entry = self.messages.get(cursor.message)?;
        for span in &entry.refs {
            if let Some(target) = span.target {
                return Some(target);
            }
        }
        None
    }

    /// Scrolls the minimum amount that brings the cursor's row into view.
    pub fn keep_cursor_visible(&mut self, width: u16, height: u16) -> Option<()> {
        let height = height as usize;
        if height == 0 {
            return None;
        }
        let cursor = self.ensure_cursor(width)?;
        let (row, total) = self.pos_row_and_total(cursor, width)?;
        let max_scroll = total.saturating_sub(height);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let top = total.saturating_sub(self.scroll_offset.saturating_add(height));
        let bottom = top.saturating_add(height);
        if row < top {
            self.scroll_offset = total
                .saturating_sub(row.saturating_add(height))
                .min(max_scroll);
        } else if row >= bottom {
            self.scroll_offset = total.saturating_sub(row + 1).min(max_scroll);
        }
        Some(())
    }

    /// Reflow at a new width invalidates wrapped-line coordinates: the anchor
    /// is dropped (a line-wise range is ambiguous across rewrap) and the
    /// cursor's line is clamped, never lost.
    pub fn on_reflow(&mut self, width: u16) {
        self.anchor = None;
        self.dragging = false;
        self.layout_index.invalidate();
        self.bump_layout_epoch();
        self.clamp_positions(width);
    }

    /// Clamps the cursor and anchor into the collapse-aware visible line range
    /// at `width`.
    fn clamp_positions(&mut self, width: u16) {
        if self.messages.is_empty() {
            self.cursor = None;
            self.anchor = None;
            return;
        }
        if let Some(cursor) = self.cursor {
            let message = cursor.message.min(self.messages.len() - 1);
            let line = cursor.line.min(self.visible_body_lines(message, width) - 1);
            self.cursor = Some(Cursor { message, line });
        }
        if let Some(anchor) = self.anchor {
            let message = anchor.message.min(self.messages.len() - 1);
            let line = anchor.line.min(self.visible_body_lines(message, width) - 1);
            self.anchor = Some(Cursor { message, line });
        }
    }

    pub fn clamp_scroll(&mut self, width: u16, height: u16) {
        let max = self.max_scroll(width, height);
        self.scroll_offset = self.scroll_offset.min(max);
    }

    pub fn scroll_message_into_view(
        &mut self,
        message: usize,
        width: u16,
        height: u16,
    ) -> Option<()> {
        let height = height as usize;
        if height == 0 {
            return None;
        }
        let (message_row, total_rows) = self.message_row_and_total(message, width)?;
        let max_scroll = total_rows.saturating_sub(height);
        let max_top = total_rows.saturating_sub(height);
        let desired_top = message_row.saturating_sub(height / 2);
        let top = desired_top.min(max_top);
        self.scroll_offset = total_rows
            .saturating_sub(top.saturating_add(height))
            .min(max_scroll);

        // A reference jump is an explicit navigation action. When the target is
        // already in the tail viewport, move one row off the bottom if possible
        // so the bottom-follow rule cannot immediately reclaim the view.
        if self.scroll_offset == 0 && max_scroll > 0 && message_row + 1 < total_rows {
            self.scroll_offset = 1;
        }
        Some(())
    }

    /// Toggles the expand/collapse state of `message` when it is collapsible
    /// (over [`COLLAPSE_LIMIT`] wrapped lines at `width`). Returns whether the
    /// state changed.
    pub fn toggle_expand(&mut self, message: usize, width: u16) -> bool {
        if message >= self.messages.len() || self.ensure_lines(message, width) <= COLLAPSE_LIMIT {
            return false;
        }
        self.messages[message].expanded = !self.messages[message].expanded;
        if self.layout_index.valid
            && self.layout_index.width == width.max(1)
            && self.layout_index.line_counts.len() == self.messages.len()
        {
            if let Some(block_index) = self.layout_index.block_containing_message(message) {
                let rows = {
                    let block = &mut self.layout_index.blocks[block_index];
                    block.body_lines = if self.messages[message].expanded {
                        self.layout_index.line_counts[message]
                    } else {
                        COLLAPSE_SHOW
                    };
                    block.collapsed = !self.messages[message].expanded;
                    Self::block_rows(block)
                };
                self.layout_index.rows.set(block_index, rows);
            } else {
                self.layout_index.invalidate();
            }
        } else {
            self.layout_index.invalidate();
        }
        // Collapsing under the cursor or anchor pulls them into the preview.
        self.clamp_positions(width);
        self.bump_layout_epoch();
        true
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

    /// Returns the absolute row index of the viewport's top line at the given
    /// dimensions, applying the same scroll clamp as
    /// [`Self::visible_lines_into`] so a subsequent call computes the
    /// identical window.
    pub fn viewport_top(&mut self, width: u16, height: u16) -> usize {
        let target = height as usize;
        if self.messages.is_empty() || target == 0 {
            return 0;
        }
        self.ensure_layout_index(width.max(1));
        let total = self.layout_index.total_rows();
        self.scroll_offset = self.scroll_offset.min(total.saturating_sub(target));
        total.saturating_sub(self.scroll_offset.saturating_add(target))
    }

    pub fn visible_lines_into(
        &mut self,
        width: u16,
        height: u16,
        _overscan: usize,
        out: &mut Vec<VisibleLine>,
    ) {
        let width = width.max(1);
        let target = height as usize;
        out.clear();
        if self.messages.is_empty() || target == 0 {
            return;
        }
        self.ensure_layout_index(width);
        let total = self.layout_index.total_rows();
        let max_scroll = total.saturating_sub(target);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let top = total.saturating_sub(self.scroll_offset.saturating_add(target));
        let bottom = top.saturating_add(target).min(total);
        out.reserve(bottom.saturating_sub(top));
        for row in top..bottom {
            if let Some(line) = self.cached_visible_line(row) {
                out.push(line);
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn visible_lines(&mut self, width: u16, height: u16, overscan: usize) -> Vec<VisibleLine> {
        let mut lines = Vec::new();
        self.visible_lines_into(width, height, overscan, &mut lines);
        lines
    }

    /// Lays out `idx` at `width` and returns its wrapped line count (at least 1).
    fn ensure_lines(&mut self, idx: usize, width: u16) -> usize {
        let width = width.max(1);
        if self.layout_index.valid && self.layout_index.width != width {
            self.layout_index.invalidate();
        }
        let syntax = self.syntax;
        let msg = &mut self.messages[idx];
        msg.layout.ensure(width, &msg.body, &msg.refs, syntax);
        msg.layout.lines().max(1)
    }

    fn ensure_layout_index(&mut self, width: u16) {
        let width = width.max(1);
        if self.layout_index.valid
            && self.layout_index.width == width
            && self.layout_index.line_counts.len() == self.messages.len()
        {
            return;
        }
        self.rebuild_layout_index(width);
    }

    fn rebuild_layout_index(&mut self, width: u16) {
        let width = width.max(1);
        self.layout_index.valid = false;
        self.layout_index.width = width;

        let mut line_counts = std::mem::take(&mut self.layout_index.line_counts);
        line_counts.clear();
        line_counts.reserve(self.messages.len());
        for idx in 0..self.messages.len() {
            line_counts.push(self.ensure_lines(idx, width));
        }

        let mut blocks = std::mem::take(&mut self.layout_index.blocks);
        blocks.clear();
        let mut cursor = 0usize;
        while cursor < self.messages.len() {
            let run_end = self.run_end_cached(cursor, &line_counts);
            self.pack_run_cached(cursor, run_end, &line_counts, &mut blocks);
            cursor = run_end + 1;
        }

        self.layout_index.rows.clear();
        for block in &blocks {
            self.layout_index.rows.push(Self::block_rows(block));
        }
        self.layout_index.line_counts = line_counts;
        self.layout_index.blocks = blocks;
        self.layout_index.valid = true;
        #[cfg(test)]
        {
            self.layout_index.full_rebuilds += 1;
        }
    }

    fn repair_layout_index_after_append(&mut self, old_len: usize) {
        if !self.layout_index.valid {
            return;
        }
        if old_len + 1 != self.messages.len()
            || self.layout_index.line_counts.len() != old_len
            || self.layout_index.rows.len() != self.layout_index.blocks.len()
        {
            self.layout_index.invalidate();
            return;
        }
        let width = self.layout_index.width;
        let lines = self.ensure_lines(old_len, width);
        self.layout_index.line_counts.push(lines);
        let repair_start = if old_len == 0 {
            0
        } else if self.boundary_before_cached(old_len - 1, old_len, &self.layout_index.line_counts)
        {
            old_len
        } else if let Some(block_index) = self.layout_index.block_containing_message(old_len - 1) {
            self.layout_index.blocks[block_index].first
        } else {
            self.layout_index.invalidate();
            return;
        };
        self.rebuild_layout_index_tail_from(repair_start);
    }

    fn rebuild_layout_index_tail_from(&mut self, repair_start: usize) {
        if !self.layout_index.valid
            || self.layout_index.line_counts.len() != self.messages.len()
            || repair_start > self.messages.len()
        {
            self.layout_index.invalidate();
            return;
        }
        let keep_blocks = self
            .layout_index
            .blocks
            .partition_point(|block| block.last < repair_start);
        self.layout_index.blocks.truncate(keep_blocks);
        self.layout_index.rows.truncate(keep_blocks);

        let mut tail_blocks = Vec::new();
        let mut cursor = repair_start;
        while cursor < self.messages.len() {
            let run_end = self.run_end_cached(cursor, &self.layout_index.line_counts);
            self.pack_run_cached(
                cursor,
                run_end,
                &self.layout_index.line_counts,
                &mut tail_blocks,
            );
            cursor = run_end + 1;
        }
        for block in tail_blocks {
            self.layout_index.rows.push(Self::block_rows(&block));
            self.layout_index.blocks.push(block);
        }
    }

    fn rebuild_layout_index_blocks_from_counts(&mut self) {
        if !self.layout_index.valid || self.layout_index.line_counts.len() != self.messages.len() {
            self.layout_index.invalidate();
            return;
        }
        let mut blocks = std::mem::take(&mut self.layout_index.blocks);
        blocks.clear();
        let mut cursor = 0usize;
        while cursor < self.messages.len() {
            let run_end = self.run_end_cached(cursor, &self.layout_index.line_counts);
            self.pack_run_cached(cursor, run_end, &self.layout_index.line_counts, &mut blocks);
            cursor = run_end + 1;
        }
        self.layout_index.rows.clear();
        for block in &blocks {
            self.layout_index.rows.push(Self::block_rows(block));
        }
        self.layout_index.blocks = blocks;
    }

    fn run_end_cached(&self, start: usize, line_counts: &[usize]) -> usize {
        if line_counts[start] > COLLAPSE_LIMIT {
            return start;
        }
        let mut end = start;
        while end + 1 < self.messages.len()
            && !self.boundary_before_cached(end, end + 1, line_counts)
        {
            end += 1;
        }
        end
    }

    fn boundary_before_cached(&self, prev: usize, cur: usize, line_counts: &[usize]) -> bool {
        if self.messages[prev].timestamp_ms == 0 || self.messages[cur].timestamp_ms == 0 {
            return true;
        }
        if self.messages[prev].local != self.messages[cur].local
            || self.messages[prev].sender != self.messages[cur].sender
            || self.messages[prev].unverified != self.messages[cur].unverified
        {
            return true;
        }
        // An edited message anchors its own heading, where the `(edited)`
        // marker renders; grouped mid-block it would be invisible.
        if self.messages[prev].edited != self.messages[cur].edited {
            return true;
        }
        let gap = self.messages[cur]
            .timestamp_ms
            .saturating_sub(self.messages[prev].timestamp_ms);
        if gap > GROUP_GAP_MS {
            return true;
        }
        line_counts[prev] > COLLAPSE_LIMIT || line_counts[cur] > COLLAPSE_LIMIT
    }

    fn pack_run_cached(
        &self,
        run_start: usize,
        run_end: usize,
        line_counts: &[usize],
        blocks: &mut Vec<Block>,
    ) {
        let first_lines = line_counts[run_start];
        if run_start == run_end && first_lines > COLLAPSE_LIMIT {
            let expanded = self.messages[run_start].expanded;
            blocks.push(Block {
                first: run_start,
                last: run_start,
                body_lines: if expanded { first_lines } else { COLLAPSE_SHOW },
                collapsed: !expanded,
            });
            return;
        }
        let mut start = run_start;
        let mut total = 0usize;
        for message in run_start..=run_end {
            let lines = line_counts[message];
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
    }

    fn cached_visible_line(&self, row: usize) -> Option<VisibleLine> {
        let block_index = self.layout_index.rows.row_to_block(row)?;
        let block = self.layout_index.blocks.get(block_index)?;
        let row_in_block = row.saturating_sub(self.layout_index.rows.prefix_sum(block_index));
        if row_in_block == 0 {
            return Some(VisibleLine {
                message: block.first,
                block_first: block.first,
                block_last: block.last,
                line: 0,
                kind: LineKind::Heading,
            });
        }
        if block.collapsed {
            let body_row = row_in_block - 1;
            if body_row < block.body_lines {
                return Some(VisibleLine {
                    message: block.last,
                    block_first: block.first,
                    block_last: block.last,
                    line: body_row,
                    kind: LineKind::Body,
                });
            }
            return Some(VisibleLine {
                message: block.last,
                block_first: block.first,
                block_last: block.last,
                line: 0,
                kind: LineKind::Ellipsis,
            });
        }

        let mut body_row = row_in_block - 1;
        for message in block.first..=block.last {
            let lines = self.layout_index.line_counts[message];
            if body_row < lines {
                return Some(VisibleLine {
                    message,
                    block_first: block.first,
                    block_last: block.last,
                    line: body_row,
                    kind: LineKind::Body,
                });
            }
            body_row -= lines;
        }
        None
    }

    /// Total rendered rows for a block: heading + body + optional ellipsis.
    fn block_rows(block: &Block) -> usize {
        1 + block.body_lines + usize::from(block.collapsed)
    }

    fn total_lines_exact(&mut self, width: u16) -> usize {
        self.ensure_layout_index(width);
        self.layout_index.total_rows()
    }

    /// Visible body lines of `message` at `width`: the full wrapped count, or
    /// [`COLLAPSE_SHOW`] when the message is collapsed.
    fn visible_body_lines(&mut self, message: usize, width: u16) -> usize {
        let width = width.max(1);
        let lines = if self.layout_index.valid
            && self.layout_index.width == width
            && self.layout_index.line_counts.len() == self.messages.len()
        {
            self.layout_index.line_counts[message]
        } else {
            self.ensure_lines(message, width)
        };
        Self::visible_body_lines_for(&self.messages[message], lines)
    }

    fn visible_body_lines_for(entry: &ChatEntry, lines: usize) -> usize {
        if lines > COLLAPSE_LIMIT && !entry.expanded {
            COLLAPSE_SHOW
        } else {
            lines
        }
    }

    fn next_body_pos(&mut self, pos: Cursor, width: u16) -> Option<Cursor> {
        if pos.line + 1 < self.visible_body_lines(pos.message, width) {
            return Some(Cursor {
                message: pos.message,
                line: pos.line + 1,
            });
        }
        if pos.message + 1 < self.messages.len() {
            return Some(Cursor {
                message: pos.message + 1,
                line: 0,
            });
        }
        None
    }

    fn prev_body_pos(&mut self, pos: Cursor, width: u16) -> Option<Cursor> {
        if pos.line > 0 {
            return Some(Cursor {
                message: pos.message,
                line: pos.line - 1,
            });
        }
        let message = pos.message.checked_sub(1)?;
        Some(Cursor {
            message,
            line: self.visible_body_lines(message, width) - 1,
        })
    }

    /// The rendered row of `pos` from the top of the full layout plus the
    /// total row count, counting heading and ellipsis rows.
    fn pos_row_and_total(&mut self, pos: Cursor, width: u16) -> Option<(usize, usize)> {
        if pos.message >= self.messages.len() {
            return None;
        }
        self.ensure_layout_index(width);
        let block_index = self.layout_index.block_containing_message(pos.message)?;
        let block = self.layout_index.blocks[block_index];
        let mut row = self
            .layout_index
            .rows
            .prefix_sum(block_index)
            .saturating_add(1);
        if !block.collapsed {
            row = row.saturating_add(
                self.layout_index.line_counts[block.first..pos.message]
                    .iter()
                    .copied()
                    .sum::<usize>(),
            );
        }
        let visible_lines = Self::visible_body_lines_for(
            &self.messages[pos.message],
            self.layout_index.line_counts[pos.message],
        );
        row = row.saturating_add(pos.line.min(visible_lines.saturating_sub(1)));
        Some((row, self.layout_index.total_rows()))
    }

    fn message_row_and_total(&mut self, message: usize, width: u16) -> Option<(usize, usize)> {
        if message >= self.messages.len() {
            return None;
        }
        self.ensure_layout_index(width);
        let block_index = self.layout_index.block_containing_message(message)?;
        let block = self.layout_index.blocks[block_index];
        let mut row = self
            .layout_index
            .rows
            .prefix_sum(block_index)
            .saturating_add(1);
        if !block.collapsed {
            row = row.saturating_add(
                self.layout_index.line_counts[block.first..message]
                    .iter()
                    .copied()
                    .sum::<usize>(),
            );
        }
        Some((row, self.layout_index.total_rows()))
    }

    fn trim_front(&mut self) {
        let excess = self.messages.len().saturating_sub(self.max_messages);
        if excess > 0 {
            let index_repairable = self.layout_index.valid
                && self.layout_index.line_counts.len() == self.messages.len()
                && self.layout_index.rows.len() == self.layout_index.blocks.len();
            self.messages.drain(0..excess);
            self.bump_reindex_revision();
            self.bump_layout_epoch();
            self.scroll_offset = self.scroll_offset.saturating_sub(excess);
            let anchor_evicted = self.anchor.is_some_and(|anchor| anchor.message < excess);
            let cursor_evicted = self.cursor.is_some_and(|cursor| cursor.message < excess);
            if anchor_evicted || cursor_evicted {
                self.anchor = None;
            } else {
                self.anchor = self.anchor.map(|anchor| Cursor {
                    message: anchor.message - excess,
                    line: anchor.line,
                });
            }
            self.cursor = self.cursor.map(|cursor| {
                // Clamp an evicted cursor onto the oldest survivor.
                match cursor.message.checked_sub(excess) {
                    Some(message) => Cursor {
                        message,
                        line: cursor.line,
                    },
                    None => Cursor {
                        message: 0,
                        line: 0,
                    },
                }
            });
            if index_repairable {
                self.layout_index.line_counts.drain(0..excess);
                self.rebuild_layout_index_blocks_from_counts();
            } else {
                self.layout_index.invalidate();
            }
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
    /// Display-only text with no body counterpart: resolved reference pill
    /// labels. Synthetic segments index into this buffer.
    synthetic: String,
    /// Synthetic ranges of rendered pills paired with their `ChatEntry::refs`
    /// index, for hit-testing clicks on pill segments.
    pill_spans: Vec<(Range<u32>, u32)>,
    /// Current block-quote nesting while laying out; drives the grey `> ` prefix
    /// and dimmed text of quoted lines.
    quote_depth: usize,
    complete: bool,
    estimated_lines: usize,
    syntax: SyntaxTheme,
}

struct RenderPiece {
    source: Range<usize>,
    display: Range<usize>,
    style: Style,
    kind: PieceKind,
}

/// What a [`RenderPiece`]'s `source` range points at and how it maps to the
/// clipboard.
enum PieceKind {
    /// Real message-body text; contributes to clipboard source mapping.
    Body,
    /// A resolved reference pill label in the synthetic buffer, paired with its
    /// `ChatEntry::refs` index. Never contributes to clipboard mapping; the
    /// hidden literal `@@code` range does instead.
    Pill(u32),
    /// Display-only synthetic text such as a block-quote `> ` marker. Never
    /// contributes to clipboard mapping.
    Synthetic,
}

struct InvisibleSource {
    source: Range<usize>,
    display_pos: usize,
}

#[derive(Clone, Copy)]
struct CodeLine {
    source_start: usize,
    source_end: usize,
    logical_start: u32,
}

impl CodeLine {
    fn source_range(self) -> Range<usize> {
        self.source_start..self.source_end
    }

    fn len(self) -> u32 {
        self.source_end
            .saturating_sub(self.source_start)
            .min(u32::MAX as usize) as u32
    }

    fn logical_end(self) -> u32 {
        self.logical_start.saturating_add(self.len())
    }
}

struct CodeBlockSource<'a> {
    text: &'a str,
    lines: &'a [CodeLine],
    len: u32,
}

impl tinyhl::Source for CodeBlockSource<'_> {
    fn len(&self) -> u32 {
        self.len
    }

    fn page(&self, offset: u32) -> (u32, &[u8]) {
        if offset >= self.len {
            return (self.len, &[]);
        }

        let line_index = self
            .lines
            .partition_point(|line| line.logical_start <= offset)
            .saturating_sub(1);
        let line = self.lines[line_index];
        let line_end = line.logical_end();
        if offset < line_end {
            let source_start = line.source_start + (offset - line.logical_start) as usize;
            return (offset, &self.text.as_bytes()[source_start..line.source_end]);
        }

        (line_end, b"\n")
    }
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
            synthetic: String::new(),
            pill_spans: Vec::new(),
            quote_depth: 0,
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

    fn ensure(&mut self, width: u16, text: &str, refs: &[MsgRefSpan], syntax: SyntaxTheme) {
        self.syntax = syntax;
        if self.wrap_width != width {
            self.reset_layout(width, text);
        }
        while !self.complete {
            self.layout_next_block(text, refs);
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

    fn reset_layout(&mut self, width: u16, text: &str) {
        self.wrap_width = width;
        self.cursor = 0;
        chatt_message_format::tokenize(text, &mut self.tokens);
        self.line_starts.clear();
        self.line_sources.clear();
        self.segments.clear();
        self.synthetic.clear();
        self.pill_spans.clear();
        self.quote_depth = 0;
        self.complete = false;
        self.estimated_lines = estimate_lines(text, width.max(1) as usize);
    }

    fn layout_next_block(&mut self, text: &str, refs: &[MsgRefSpan]) {
        let avail = (self.wrap_width as usize).max(1);

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
                    refs,
                    self.cursor + 1,
                    end,
                    Style::DEFAULT,
                    (avail, avail),
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
                    refs,
                    self.cursor + 1,
                    end,
                    Style::DEFAULT | Modifier::BOLD,
                    (avail, avail),
                    (0, 0),
                    prefix,
                );
                self.cursor = end.saturating_add(1);
            }
            TokenKind::UnorderedListStart | TokenKind::OrderedListStart => {
                self.cursor = self.layout_list(text, refs, self.cursor + 1, avail);
            }
            TokenKind::CodeBlockStart { .. } => {
                self.cursor = self.layout_code_block(text, self.cursor, avail);
            }
            TokenKind::BlockQuoteStart => {
                self.quote_depth = self.quote_depth.saturating_add(1);
                self.cursor += 1;
            }
            TokenKind::BlockQuoteEnd => {
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.cursor += 1;
            }
            TokenKind::BlankLine => {
                let range = token_range(&self.tokens[self.cursor]);
                self.push_line();
                self.emit_quote_marker();
                self.note_source_range(range.start, range.end);
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
        refs: &[MsgRefSpan],
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
                    refs,
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
            refs,
            line_start,
            end,
            base_style,
            widths,
            cols,
            LinePrefix::default(),
        );
    }

    fn layout_list(
        &mut self,
        text: &str,
        refs: &[MsgRefSpan],
        mut cursor: usize,
        target: usize,
    ) -> usize {
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
                        refs,
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
        refs: &[MsgRefSpan],
        start: usize,
        end: usize,
        mut base_style: Style,
        mut widths: (usize, usize),
        mut cols: (u16, u16),
        prefix: LinePrefix,
    ) {
        let mut display = String::new();
        let mut pieces = Vec::new();
        let mut invisible = Vec::new();
        let mut synthetic = String::new();

        // Inside a block quote every line leads with a grey `> ` per nesting
        // level and its text is dimmed. The marker is synthetic (display-only)
        // so it stays out of the clipboard; wrapped continuation rows hang under
        // the content, mirroring list layout.
        if self.quote_depth > 0 {
            base_style = base_style.patch(self.syntax.comment);
            let marker = "> ".repeat(self.quote_depth);
            let marker_width = UnicodeWidthStr::width(marker.as_str());
            append_synth_prefix(
                &marker,
                self.synthetic.len(),
                &mut synthetic,
                &mut display,
                &mut pieces,
                self.syntax.comment,
            );
            let marker_col = marker_width.min(u16::MAX as usize) as u16;
            cols.1 = cols.1.saturating_add(marker_col);
            widths.1 = widths.1.saturating_sub(marker_width).max(1);
        }

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
            refs,
            start,
            end,
            base_style,
            &mut display,
            &mut pieces,
            &mut invisible,
            &mut synthetic,
        );
        self.synthetic.push_str(&synthetic);
        self.wrap_pieces(&display, &pieces, &invisible, widths, cols);
    }

    fn collect_inline_pieces(
        &self,
        text: &str,
        refs: &[MsgRefSpan],
        start: usize,
        end: usize,
        base_style: Style,
        display: &mut String,
        pieces: &mut Vec<RenderPiece>,
        invisible: &mut Vec<InvisibleSource>,
        synthetic: &mut String,
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
                TokenKind::MessageRef => {
                    let span = refs
                        .iter()
                        .enumerate()
                        .find(|(_, span)| span.range == token.range);
                    let pill = span.and_then(|(index, span)| {
                        let label = span.label.as_deref()?;
                        Some((index, label))
                    });
                    match pill {
                        Some((index, label)) => {
                            invisible.push(InvisibleSource {
                                source: token_range(token),
                                display_pos: display.len(),
                            });
                            append_pill_piece(
                                label,
                                self.synthetic.len(),
                                synthetic,
                                display,
                                pieces,
                                self.syntax.namespace | Modifier::UNDERLINED,
                                index as u32,
                            );
                        }
                        None => append_piece(
                            text,
                            display,
                            pieces,
                            token_range(token),
                            self.syntax.comment,
                        ),
                    }
                }
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
        for wrapped in bwrap::wrap_ranges_preserve_leading(display, widths.0, widths.1) {
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
                match piece.kind {
                    PieceKind::Pill(ref_index) => self.emit_pill_segment(
                        source_start..source_end,
                        col,
                        piece.style,
                        ref_index,
                    ),
                    PieceKind::Synthetic => {
                        self.emit_synth_segment(source_start..source_end, col, piece.style)
                    }
                    PieceKind::Body => {
                        self.emit_segment(source_start..source_end, col, piece.style)
                    }
                }
            }
        }

        if !wrapped_any {
            self.push_line();
            for source in invisible {
                self.note_source_range(source.source.start, source.source.end);
            }
            for piece in pieces {
                if matches!(piece.kind, PieceKind::Body) {
                    self.note_source_range(piece.source.start, piece.source.end);
                }
            }
        }
    }

    fn layout_code_block(&mut self, text: &str, start: usize, avail: usize) -> usize {
        let lang = match &self.tokens[start].kind {
            TokenKind::CodeBlockStart { lang } => lang
                .as_ref()
                .map(|range| &text[range.start as usize..range.end as usize]),
            _ => None,
        };
        let lines_start = start + 1;
        let mut cursor = lines_start;
        while self
            .tokens
            .get(cursor)
            .is_some_and(|token| matches!(token.kind, TokenKind::CodeBlockLine))
        {
            cursor += 1;
        }

        if cursor == lines_start {
            let source_pos = token_range(&self.tokens[start]).end;
            debug_assert!(
                self.tokens
                    .get(cursor)
                    .is_some_and(|token| matches!(token.kind, TokenKind::CodeBlockEnd))
            );
            self.push_line();
            self.emit_quote_marker();
            self.note_source_range(source_pos, source_pos);
            return cursor.saturating_add(1);
        }

        match lang.and_then(highlight::language_for_tag) {
            Some(language) => {
                let lines = self.code_lines(lines_start, cursor);
                let source = CodeBlockSource {
                    text,
                    len: lines.last().map_or(0, |line| line.logical_end()),
                    lines: &lines,
                };
                let runs = highlight::source_runs(&source, Some(language));
                let mut run_index = 0usize;
                for line in lines {
                    self.emit_highlighted_verbatim(text, line, avail, &runs, &mut run_index);
                }
            }
            None => {
                for index in lines_start..cursor {
                    let range = token_range(&self.tokens[index]);
                    self.emit_plain_verbatim(
                        text,
                        range.start,
                        range.end,
                        avail,
                        self.syntax.string,
                    );
                }
            }
        }

        debug_assert!(
            self.tokens
                .get(cursor)
                .is_some_and(|token| matches!(token.kind, TokenKind::CodeBlockEnd))
        );
        cursor.saturating_add(1)
    }

    fn code_lines(&self, start: usize, end: usize) -> Vec<CodeLine> {
        let mut lines = Vec::with_capacity(end.saturating_sub(start));
        let mut logical_start = 0u32;
        for index in start..end {
            let range = token_range(&self.tokens[index]);
            let line = CodeLine {
                source_start: range.start,
                source_end: range.end,
                logical_start,
            };
            logical_start = line.logical_end();
            if index + 1 < end {
                logical_start = logical_start.saturating_add(1);
            }
            lines.push(line);
        }
        lines
    }

    /// Emits the grey `> ` marker run for the current quote depth on the current
    /// line and returns its rendered width (0 when not in a quote).
    fn emit_quote_marker(&mut self) -> usize {
        if self.quote_depth == 0 {
            return 0;
        }
        let marker = "> ".repeat(self.quote_depth);
        let start = self.synthetic.len();
        self.synthetic.push_str(&marker);
        let width = UnicodeWidthStr::width(marker.as_str());
        self.segments.push(Segment {
            col: 0,
            start: start as u32,
            end: self.synthetic.len() as u32,
            style: self.syntax.comment,
            synth: true,
        });
        width
    }

    fn push_line(&mut self) {
        self.line_starts.push(self.segments.len() as u32);
        self.line_sources.push((u32::MAX, 0));
    }

    fn emit_plain_verbatim(
        &mut self,
        text: &str,
        start: usize,
        end: usize,
        avail: usize,
        style: Style,
    ) {
        self.push_line();
        let lead = self.emit_quote_marker();
        if start == end {
            self.note_source_range(start, end);
            return;
        }
        let avail = avail.saturating_sub(lead).max(1);
        let base = lead.min(u16::MAX as usize) as u16;
        let mut chunk_start = start;
        let mut width = 0usize;
        for (i, ch) in text[start..end].char_indices() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if width + w > avail && width > 0 {
                self.emit_segment(chunk_start..start + i, base, style);
                self.push_line();
                chunk_start = start + i;
                width = 0;
            }
            width += w;
        }
        if chunk_start < end {
            self.emit_segment(chunk_start..end, base, style);
        }
    }

    fn emit_highlighted_verbatim(
        &mut self,
        text: &str,
        line: CodeLine,
        avail: usize,
        runs: &[(u32, u32, HlClass)],
        run_index: &mut usize,
    ) {
        self.push_line();
        let lead = self.emit_quote_marker();
        if line.source_start == line.source_end {
            self.note_source_range(line.source_start, line.source_end);
            return;
        }

        let avail = avail.saturating_sub(lead).max(1);
        let base = lead.min(u16::MAX as usize) as u16;
        let fallback = self.syntax.string;
        let mut chunk_start = line.source_start;
        let mut chunk_logical_start = line.logical_start;
        let mut width = 0usize;

        for (i, ch) in text[line.source_range()].char_indices() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(1);
            if width + w > avail && width > 0 {
                let chunk_end = line.source_start + i;
                self.emit_highlighted_chunk(
                    text,
                    chunk_start,
                    chunk_end,
                    chunk_logical_start,
                    base,
                    runs,
                    run_index,
                    fallback,
                );
                self.push_line();
                chunk_start = chunk_end;
                chunk_logical_start = line.logical_start.saturating_add(i as u32);
                width = 0;
            }
            width += w;
        }

        if chunk_start < line.source_end {
            self.emit_highlighted_chunk(
                text,
                chunk_start,
                line.source_end,
                chunk_logical_start,
                base,
                runs,
                run_index,
                fallback,
            );
        }
    }

    fn emit_highlighted_chunk(
        &mut self,
        text: &str,
        source_start: usize,
        source_end: usize,
        logical_start: u32,
        base: u16,
        runs: &[(u32, u32, HlClass)],
        run_index: &mut usize,
        fallback: Style,
    ) {
        let logical_end =
            logical_start.saturating_add((source_end - source_start).min(u32::MAX as usize) as u32);
        while *run_index < runs.len() && runs[*run_index].1 <= logical_start {
            *run_index += 1;
        }

        let mut cursor = source_start;
        let mut width = 0usize;
        let mut index = *run_index;
        while index < runs.len() && runs[index].0 < logical_end {
            let (run_start, run_end, class) = runs[index];
            let start = run_start.max(logical_start);
            let end = run_end.min(logical_end);
            if end > start {
                let styled_start = source_start + (start - logical_start) as usize;
                let styled_end = source_start + (end - logical_start) as usize;
                if cursor < styled_start {
                    let col = base.saturating_add(width.min(u16::MAX as usize) as u16);
                    self.emit_segment(cursor..styled_start, col, fallback);
                    width =
                        width.saturating_add(UnicodeWidthStr::width(&text[cursor..styled_start]));
                }

                let col = base.saturating_add(width.min(u16::MAX as usize) as u16);
                self.emit_segment(styled_start..styled_end, col, self.syntax.style_for(class));
                width =
                    width.saturating_add(UnicodeWidthStr::width(&text[styled_start..styled_end]));
                cursor = styled_end;
            }

            if run_end > logical_end {
                break;
            }
            index += 1;
        }

        if cursor < source_end {
            let col = base.saturating_add(width.min(u16::MAX as usize) as u16);
            self.emit_segment(cursor..source_end, col, fallback);
        }

        while *run_index < runs.len() && runs[*run_index].1 <= logical_end {
            *run_index += 1;
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
                synth: false,
            });
        }
    }

    /// Emits a display-only synthetic segment (a block-quote `> ` marker). Like
    /// [`Self::emit_pill_segment`] it notes no source range, so the markers stay
    /// out of the clipboard, but it registers no pill span.
    fn emit_synth_segment(&mut self, range: Range<usize>, col: u16, style: Style) {
        if range.start < range.end {
            self.segments.push(Segment {
                col,
                start: range.start as u32,
                end: range.end as u32,
                style,
                synth: true,
            });
        }
    }

    /// Emits a segment of synthetic pill text. Unlike [`Self::emit_segment`]
    /// this never notes a source range: the pill's clipboard text is the hidden
    /// literal `@@code`, already noted through its `InvisibleSource`.
    fn emit_pill_segment(&mut self, range: Range<usize>, col: u16, style: Style, ref_index: u32) {
        if range.start < range.end {
            let range = range.start as u32..range.end as u32;
            self.pill_spans.push((range.clone(), ref_index));
            self.segments.push(Segment {
                col,
                start: range.start,
                end: range.end,
                style,
                synth: true,
            });
        }
    }

    /// Returns the first segment of wrapped `line` whose rendered text covers
    /// `col_in_line`.
    fn segment_at(&self, body: &str, line: usize, col_in_line: u16) -> Option<&Segment> {
        if line >= self.lines() {
            return None;
        }
        for seg in self.line(line) {
            let text = self.segment_str(body, seg);
            let width = UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16;
            if col_in_line >= seg.col && col_in_line < seg.col.saturating_add(width) {
                return Some(seg);
            }
        }
        None
    }

    fn segment_str<'a>(&'a self, body: &'a str, seg: &Segment) -> &'a str {
        if seg.synth {
            &self.synthetic[seg.start as usize..seg.end as usize]
        } else {
            &body[seg.start as usize..seg.end as usize]
        }
    }

    fn pill_ref_at(&self, seg: &Segment) -> Option<usize> {
        for (range, index) in &self.pill_spans {
            if range.start < seg.end && seg.start < range.end {
                return Some(*index as usize);
            }
        }
        None
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
        kind: PieceKind::Body,
    });
}

/// Appends a resolved reference's pill label: the label text joins the display
/// string for wrapping like any piece, but its source range indexes the
/// layout's synthetic buffer (`base` is the buffer length before this line's
/// local `synthetic` additions).
fn append_pill_piece(
    label: &str,
    base: usize,
    synthetic: &mut String,
    display: &mut String,
    pieces: &mut Vec<RenderPiece>,
    style: Style,
    ref_index: u32,
) {
    if label.is_empty() {
        return;
    }
    let start = display.len();
    display.push_str(label);
    let source_start = base + synthetic.len();
    synthetic.push_str(label);
    pieces.push(RenderPiece {
        source: source_start..source_start + label.len(),
        display: start..display.len(),
        style,
        kind: PieceKind::Pill(ref_index),
    });
}

/// Appends a display-only synthetic prefix (a block-quote `> ` marker run) as a
/// leading piece. Like a pill its `source` indexes the synthetic buffer, but it
/// carries no reference and never enters clipboard mapping. `base` is the
/// committed synthetic length before this line's local additions.
fn append_synth_prefix(
    marker: &str,
    base: usize,
    synthetic: &mut String,
    display: &mut String,
    pieces: &mut Vec<RenderPiece>,
    style: Style,
) {
    if marker.is_empty() {
        return;
    }
    let start = display.len();
    display.push_str(marker);
    let source_start = base + synthetic.len();
    synthetic.push_str(marker);
    pieces.push(RenderPiece {
        source: source_start..source_start + marker.len(),
        display: start..display.len(),
        style,
        kind: PieceKind::Synthetic,
    });
}

/// Builds the display label of a resolved reference pill from its target's
/// sender and body.
fn ref_label(sender: &str, body: &str) -> String {
    const SNIPPET_CHARS: usize = 40;
    let mut label = format!("@@ {sender}: ");
    let snippet = body.lines().next().unwrap_or("");
    let mut truncated = body.lines().nth(1).is_some();
    for (count, ch) in snippet.chars().enumerate() {
        if count == SNIPPET_CHARS {
            truncated = true;
            break;
        }
        label.push(ch);
    }
    if truncated {
        label.push('…');
    }
    label
}

fn token_range(token: &Token) -> Range<usize> {
    token.range.start as usize..token.range.end as usize
}

fn estimate_lines(text: &str, avail: usize) -> usize {
    let target = avail.max(1);
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
            let id = self.messages.len() as u64 + 1;
            let inline = chatt_message_format::inline_ranges(body);
            let refs = self.build_ref_spans(body, inline.refs);
            let old_len = self.messages.len();
            self.messages.push(ChatEntry {
                id,
                sender: sender.to_string(),
                body: body.to_string(),
                timestamp_ms,
                local,
                edited: false,
                unverified: false,
                file_transfer_id: None,
                links: inline.urls,
                refs,
                expanded: false,
                notice_id: None,
                notice_kind: None,
                layout: MessageLayout::new(),
            });
            self.repair_layout_index_after_append(old_len);
            self.trim_front();
        }
    }

    fn chat_message(id: u64, timestamp_ms: u64, body: &str) -> ChatMessage {
        ChatMessage {
            message_id: rpc::ids::MessageId(id),
            room_id: rpc::ids::RoomId(1),
            sender: rpc::ids::UserId(2),
            sender_name: "alice".to_string(),
            timestamp_ms,
            body: body.to_string(),
            file_transfer_id: None,
            flags: rpc::control::MessageFlags::default(),
            target: None,
        }
    }

    #[test]
    fn prepend_shifts_cursor_and_keeps_bottom_relative_scroll() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 10..15 {
            buf.push_chat(chat_message(id, id * 1_000, "resident"), false);
        }
        buf.scroll_up(3, 40, 2);
        let offset = buf.scroll_offset();
        buf.set_cursor_to_message(2);

        let older = (1..4)
            .map(|id| (chat_message(id, id * 1_000, "older"), false))
            .collect();
        buf.prepend_chat(older);

        assert_eq!(buf.len(), 8);
        assert_eq!(buf.message(0).id, 1);
        assert_eq!(buf.scroll_offset(), offset);
        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 5,
                line: 0
            })
        );
        assert_eq!(buf.message(5).id, 12);
    }

    #[test]
    fn prepend_respects_message_cap() {
        let mut buf = VirtualChatBuffer::new(4, SyntaxTheme::default());
        for id in 10..13 {
            buf.push_chat(chat_message(id, id * 1_000, "resident"), false);
        }

        let older = (1..=3)
            .map(|id| (chat_message(id, id * 1_000, "older"), false))
            .collect();
        buf.prepend_chat(older);

        assert_eq!(buf.len(), 4);
        assert_eq!(buf.message(0).id, 3);
        assert_eq!(buf.message(3).id, 12);
    }

    #[test]
    fn insert_chat_orders_between_messages_and_skips_notices() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_chat(chat_message(1, 1_000, "first"), false);
        buf.push_notice("net", "notice");
        buf.push_chat(chat_message(4, 4_000, "fourth"), false);

        buf.insert_chat(chat_message(2, 2_000, "second"), false);
        buf.insert_chat(chat_message(0, 500, "oldest"), false);

        let ids: Vec<u64> = (0..buf.len()).map(|index| buf.message(index).id).collect();
        assert_eq!(ids, vec![0, 1, 2, 0, 4]);
        assert_eq!(buf.message(3).timestamp_ms, 0, "notice stays pinned");
        assert_eq!(buf.message(0).body, "oldest");
    }

    #[test]
    fn set_local_flags_updates_entries_by_key() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_chat(chat_message(1, 1_000, "mine"), false);
        buf.push_notice("net", "notice");
        buf.push_chat(chat_message(2, 2_000, "theirs"), false);

        buf.set_local_flags(|id| (id == 1).then_some(true));

        assert!(buf.message(0).local);
        assert!(!buf.message(1).local);
        assert!(!buf.message(2).local);
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
    fn reflow_clamps_cursor_and_drops_anchor() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "aa bb cc dd ee ff", 1_000_000, false);
        // At width 3 the body wraps to six lines; park the cursor on the last.
        let cursor = buf.cursor_to_last(3).expect("non-empty");
        assert!(cursor.line >= 5);
        assert!(buf.toggle_visual_anchor(3));

        // At width 40 the body is a single line: the anchor is gone and the
        // cursor keeps its message with the line clamped into range.
        buf.on_reflow(40);
        assert!(!buf.has_visual());
        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
    }

    #[test]
    fn reflow_first_render_clamps_stale_scroll_offset() {
        let mut buf = buffer_with_notices(20);
        let height = 5;
        buf.top(3, height);
        assert!(buf.scroll_offset() > 0);

        buf.on_reflow(80);
        let rows = buf.visible_lines(80, height, 0);

        assert!(!rows.is_empty(), "the first post-reflow render draws rows");
        assert_eq!(buf.scroll_offset(), buf.max_scroll(80, height));
    }

    #[test]
    fn cursor_message_body_returns_the_full_body() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "first", 1_000_000, false);
        buf.push_test("alice", "second\nlines", 1_001_000, false);
        buf.set_cursor_to_message(1);

        assert_eq!(buf.cursor_message_body(), Some("second\nlines"));
    }

    #[test]
    fn selected_message_indices_cover_each_visual_message_once() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "first wraps across rows", 1_000_000, false);
        buf.push_test("bob", "second also wraps across rows", 1_001_000, false);
        buf.push_test("carol", "third wraps as well", 1_002_000, false);
        buf.set_cursor_to_message(0);
        assert!(buf.toggle_visual_anchor(8));
        buf.move_cursor_line(20, 8);

        assert_eq!(buf.selected_message_indices(8), vec![0, 1, 2]);
    }

    #[test]
    fn selected_message_indices_plain_cursor_targets_whole_message() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test(
            "alice",
            "many wrapped body lines live here",
            1_000_000,
            false,
        );
        buf.cursor_to_last(6);

        assert_eq!(buf.selected_message_indices(6), vec![0]);
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
    fn ref_scroll_detaches_when_target_is_visible_in_tail() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for i in 0..8 {
            let sender = if i % 2 == 0 { "alice" } else { "bob" };
            buf.push_test(sender, &format!("message {i}"), 1_000_000 + i as u64, false);
        }

        let (width, height) = (40, 5);
        buf.bottom();
        assert_eq!(buf.scroll_offset(), 0);

        let target = buf.messages.len() - 2;
        buf.scroll_message_into_view(target, width, height)
            .expect("target message is present");

        assert_eq!(buf.scroll_offset(), 1);
        let rows = buf.visible_lines(width, height, 0);
        assert!(
            rows.iter()
                .any(|row| row.kind == LineKind::Body && row.message == target),
            "target body should remain visible after detaching from bottom"
        );
    }

    #[test]
    fn visual_text_is_body_content_joined_by_newlines() {
        let mut buf = buffer_with_notices(3);
        // Lay out every message so line segments exist.
        let _ = buf.total_lines_exact(40);
        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 2,
            line: 0,
        });
        assert_eq!(
            buf.visual_text(40).as_deref(),
            Some("message 0\nmessage 1\nmessage 2")
        );
    }

    #[test]
    fn visual_text_does_not_copy_hidden_collapsed_rows() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "before", 1_000_000, false);
        buf.push_test("bob", &fenced(13), 1_200_000, false);
        buf.push_test("carol", "after", 1_400_000, false);
        let _ = buf.total_lines_exact(40);

        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 2,
            line: 0,
        });

        let copied = buf.visual_text(40).expect("selection copies");
        assert!(copied.contains("before\n0\n1"));
        assert!(copied.contains("\n9\nafter"));
        assert!(!copied.contains("\n10"));
        assert!(!copied.contains("\n12"));
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
    fn visual_text_preserves_whitespace_between_wrapped_rows() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "alpha beta", 1_000_000, false);
        let _ = buf.total_lines_exact(5);
        assert_eq!(buf.messages[0].layout.lines(), 2);

        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 0,
            line: 1,
        });

        assert_eq!(buf.visual_text(5).as_deref(), Some("alpha beta"));
    }

    fn ref_code(message_id: u64) -> String {
        rpc::msgref::MessageRef {
            room_id: rpc::ids::RoomId(1),
            message_id: rpc::ids::MessageId(message_id),
        }
        .encode()
    }

    fn pill_segments(buf: &VirtualChatBuffer, message: usize) -> Vec<(Segment, String)> {
        let entry = &buf.messages[message];
        let mut found = Vec::new();
        for line in 0..entry.layout.lines() {
            for seg in entry.layout.line(line) {
                if seg.synth {
                    found.push((*seg, buf.segment_text(message, seg).to_string()));
                }
            }
        }
        found
    }

    #[test]
    fn resolved_ref_lays_out_a_pill_with_the_target_label() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.set_room_id(rpc::ids::RoomId(1));
        buf.push_test("alice", "the delay manager change is in", 1_000_000, false);
        let code = ref_code(1);
        buf.push_test(
            "bob",
            &format!("see @@{code} for context"),
            1_060_000,
            false,
        );
        let _ = buf.total_lines_exact(95);

        let entry = &buf.messages[1];
        assert!(entry.refs[0].target.is_some());
        assert!(entry.refs[0].label.is_some());
        let pills = pill_segments(&buf, 1);
        assert!(!pills.is_empty(), "no synthetic pill segment emitted");
        assert!(
            pills[0].1.starts_with("@@ alice: the delay manager"),
            "unexpected pill text {:?}",
            pills[0].1
        );
    }

    #[test]
    fn ref_at_resolves_a_click_on_the_pill() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.set_room_id(rpc::ids::RoomId(1));
        buf.push_test("alice", "target", 1_000_000, false);
        let code = ref_code(1);
        buf.push_test("bob", &format!("see @@{code}"), 1_060_000, false);
        let _ = buf.total_lines_exact(95);

        let (pill, _) = pill_segments(&buf, 1)[0];
        let target = buf.ref_at(1, 0, pill.col).expect("pill click resolves");
        assert_eq!(target.message_id.0, 1);
        assert_eq!(buf.find_message(1), Some(0));
    }

    #[test]
    fn unresolved_ref_renders_the_literal_code() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.set_room_id(rpc::ids::RoomId(1));
        let code = ref_code(42);
        buf.push_test("bob", &format!("see @@{code}"), 1_060_000, false);
        let _ = buf.total_lines_exact(95);

        let entry = &buf.messages[0];
        assert!(entry.refs[0].target.is_some());
        assert!(entry.refs[0].label.is_none());
        assert!(pill_segments(&buf, 0).is_empty());
        let target = buf.ref_at(0, 0, 5).expect("literal ref is clickable");
        assert_eq!(target.message_id.0, 42);
    }

    #[test]
    fn undecodable_ref_is_not_clickable() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.set_room_id(rpc::ids::RoomId(1));
        let mut code = ref_code(42);
        let flipped = if code.ends_with('0') { '1' } else { '0' };
        code.pop();
        code.push(flipped);
        buf.push_test("bob", &format!("see @@{code}"), 1_060_000, false);
        let _ = buf.total_lines_exact(95);

        let entry = &buf.messages[0];
        assert_eq!(entry.refs.len(), 1);
        assert!(entry.refs[0].target.is_none());
        assert!(buf.ref_at(0, 0, 5).is_none());
    }

    #[test]
    fn selection_over_a_pill_line_copies_the_literal_code() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.set_room_id(rpc::ids::RoomId(1));
        buf.push_test("alice", "target message", 1_000_000, false);
        let code = ref_code(1);
        let body = format!("intro\nsee @@{code} tail");
        buf.push_test("bob", &body, 1_060_000, false);
        let _ = buf.total_lines_exact(95);
        assert_eq!(buf.messages[1].layout.lines(), 2);

        buf.begin_drag(Cursor {
            message: 1,
            line: 1,
        });
        buf.drag_to(Cursor {
            message: 1,
            line: 1,
        });
        assert_eq!(
            buf.visual_text(95).as_deref(),
            Some(format!("see @@{code} tail").as_str())
        );
    }

    #[test]
    fn visual_text_preserves_original_newlines_when_message_is_selected() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "alpha\nbeta", 1_000_000, false);
        let _ = buf.total_lines_exact(40);

        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 0,
            line: buf.messages[0].layout.lines() - 1,
        });

        assert_eq!(buf.visual_text(40).as_deref(), Some("alpha\nbeta"));
    }

    /// A theme whose `comment` slot is a distinct grey, so quote dimming is
    /// observable (the derived default leaves every slot blank).
    fn grey_theme() -> SyntaxTheme {
        let mut syntax = SyntaxTheme::default();
        syntax.comment = Style::DEFAULT.with_fg_rgb(0x8a, 0x8c, 0x8a);
        syntax
    }

    fn syntax_probe_theme() -> SyntaxTheme {
        let mut syntax = SyntaxTheme::default();
        syntax.fg = Style::DEFAULT.with_fg_rgb(0x01, 0x01, 0x01);
        syntax.keyword = Style::DEFAULT.with_fg_rgb(0x02, 0x02, 0x02);
        syntax.function = Style::DEFAULT.with_fg_rgb(0x03, 0x03, 0x03);
        syntax.string = Style::DEFAULT.with_fg_rgb(0x04, 0x04, 0x04);
        syntax
    }

    /// Renders `body` into a standalone layout and returns its segments paired
    /// with their rendered text.
    fn quote_segments(body: &str) -> Vec<(Segment, String)> {
        let mut layout = MessageLayout::new();
        layout.ensure(40, body, &[], grey_theme());
        (0..layout.lines())
            .flat_map(|line| layout.line(line).to_vec())
            .map(|seg| {
                let text = layout.segment_str(body, &seg).to_string();
                (seg, text)
            })
            .collect()
    }

    fn styled_segments(body: &str, syntax: SyntaxTheme) -> Vec<(String, Style)> {
        let mut layout = MessageLayout::new();
        layout.ensure(80, body, &[], syntax);
        (0..layout.lines())
            .flat_map(|line| layout.line(line).to_vec())
            .filter(|seg| !seg.synth)
            .map(|seg| (layout.segment_str(body, &seg).to_string(), seg.style))
            .collect()
    }

    fn rendered_lines(body: &str, width: u16) -> Vec<String> {
        let mut layout = MessageLayout::new();
        layout.ensure(width, body, &[], grey_theme());
        (0..layout.lines())
            .map(|line| {
                layout
                    .line(line)
                    .iter()
                    .map(|segment| layout.segment_str(body, segment))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn prose_wraps_at_the_available_terminal_width() {
        let body = ["word"; 21].join(" ");
        assert_eq!(rendered_lines(&body, 120), vec![body]);
    }

    #[test]
    fn ordinary_lines_preserve_leading_whitespace() {
        let body = "sh ./script.sh /\n    arg1\n    arg2";
        assert_eq!(
            rendered_lines(body, 80),
            vec!["sh ./script.sh /", "    arg1", "    arg2"]
        );
    }

    #[test]
    fn internal_blank_lines_render_once_and_edge_blanks_are_omitted() {
        let body = "\n\n> Quote 1\n\n \n\n> Quote 2\n\n";
        assert_eq!(rendered_lines(body, 80), vec!["> Quote 1", "", "> Quote 2"]);
    }

    #[test]
    fn fenced_code_block_uses_declared_language_highlighting() {
        let syntax = syntax_probe_theme();
        let segments = styled_segments("```rust\nfn main() {\n    let value = 1;\n}\n```", syntax);

        let fn_style = segments
            .iter()
            .find_map(|(text, style)| (text == "fn").then_some(*style))
            .expect("rust keyword segment");
        assert_eq!(fn_style, syntax.keyword);

        let main_style = segments
            .iter()
            .find_map(|(text, style)| (text == "main").then_some(*style))
            .expect("rust function segment");
        assert_eq!(main_style, syntax.function);
    }

    #[test]
    fn quoted_fenced_code_block_uses_declared_language_highlighting() {
        let syntax = syntax_probe_theme();
        let segments = styled_segments("> ```rust\n> fn main() {}\n> ```", syntax);

        let fn_style = segments
            .iter()
            .find_map(|(text, style)| (text == "fn").then_some(*style))
            .expect("rust keyword segment");
        assert_eq!(fn_style, syntax.keyword);

        let main_style = segments
            .iter()
            .find_map(|(text, style)| (text == "main").then_some(*style))
            .expect("rust function segment");
        assert_eq!(main_style, syntax.function);
    }

    #[test]
    fn unknown_code_block_language_keeps_code_style() {
        let syntax = syntax_probe_theme();
        let segments = styled_segments("```madeup\nfn main\n```", syntax);

        assert!(
            segments
                .iter()
                .any(|(text, style)| text == "fn main" && *style == syntax.string),
            "unrecognized fences fall back to the code string color"
        );
    }

    #[test]
    fn block_quote_prefixes_grey_marker_and_dims_text() {
        let grey = grey_theme().comment;
        let segs = quote_segments("> quoted");

        let (marker, _) = segs
            .iter()
            .find(|(seg, text)| seg.synth && text == "> ")
            .expect("synthetic grey marker preserving the `>`");
        assert_eq!(marker.style, grey);

        let (body, _) = segs
            .iter()
            .find(|(seg, text)| !seg.synth && text == "quoted")
            .expect("dimmed quote text");
        assert_eq!(
            body.style,
            Style::DEFAULT.patch(grey),
            "quote text is dimmed"
        );
    }

    #[test]
    fn nested_block_quote_marker_repeats_per_level() {
        let segs = quote_segments(">> deep");
        assert!(
            segs.iter().any(|(seg, text)| seg.synth && text == "> > "),
            "two nesting levels render two grey markers"
        );
    }

    #[test]
    fn quoted_marker_stays_out_of_line_selection() {
        let mut buf = VirtualChatBuffer::new(1000, grey_theme());
        buf.push_test("alice", "> a\n> b", 1_000_000, false);
        let _ = buf.total_lines_exact(40);

        // Selecting only the first rendered line copies its content, not the
        // synthetic `> ` marker.
        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 0,
            line: 0,
        });
        assert_eq!(buf.visual_text(40).as_deref(), Some("a"));
    }

    #[test]
    fn quoted_code_selection_uses_logical_line_ranges() {
        let mut buf = VirtualChatBuffer::new(1000, grey_theme());
        buf.push_test(
            "alice",
            "intro\n> ```\n>> literal marker\n> ```\noutro",
            1_000_000,
            false,
        );
        let _ = buf.total_lines_exact(40);
        assert_eq!(buf.messages[0].layout.lines(), 3);

        buf.begin_drag(Cursor {
            message: 0,
            line: 1,
        });
        buf.drag_to(Cursor {
            message: 0,
            line: 1,
        });
        assert_eq!(
            buf.visual_text(40).as_deref(),
            Some("> literal marker"),
            "the container prefix is absent while the deeper literal marker remains"
        );
    }

    #[test]
    fn empty_quoted_code_has_no_synthetic_contiguous_source_range() {
        let mut buf = VirtualChatBuffer::new(1000, grey_theme());
        buf.push_test("alice", "intro\n> ```\n> ```\noutro", 1_000_000, false);
        let _ = buf.total_lines_exact(40);
        assert_eq!(buf.messages[0].layout.lines(), 3);

        buf.begin_drag(Cursor {
            message: 0,
            line: 1,
        });
        buf.drag_to(Cursor {
            message: 0,
            line: 1,
        });
        assert_eq!(buf.visual_text(40).as_deref(), Some(""));
    }

    #[test]
    fn cursor_defaults_to_last_body_line_of_newest_message() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "one", 1_000_000, false);
        buf.push_test("alice", "alpha\nbeta", 1_001_000, false);

        assert_eq!(
            buf.ensure_cursor(40),
            Some(Cursor {
                message: 1,
                line: 1
            })
        );
    }

    #[test]
    fn ensure_cursor_is_none_on_an_empty_buffer() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        assert_eq!(buf.ensure_cursor(40), None);
    }

    #[test]
    fn move_cursor_line_crosses_block_boundaries() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "alpha\nbeta", 1_000_000, false);
        buf.push_test("bob", "gamma", 1_001_000, false);

        buf.set_cursor_to_message(0);
        assert_eq!(
            buf.move_cursor_line(1, 40),
            Some(Cursor {
                message: 0,
                line: 1
            })
        );
        assert_eq!(
            buf.move_cursor_line(1, 40),
            Some(Cursor {
                message: 1,
                line: 0
            }),
            "crossing into bob's block"
        );
        assert_eq!(
            buf.move_cursor_line(-2, 40),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
        assert_eq!(
            buf.move_cursor_line(-1, 40),
            Some(Cursor {
                message: 0,
                line: 0
            }),
            "clamped at the oldest line"
        );
    }

    #[test]
    fn move_cursor_line_skips_collapsed_hidden_lines() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", &fenced(13), 1_000_000, false);
        buf.push_test("bob", "after", 1_200_000, false);

        buf.set_cursor_to_message(0);
        let mut cursor = buf.cursor().expect("cursor set");
        while cursor.message == 0 {
            cursor = buf.move_cursor_line(1, 40).expect("non-empty");
        }
        // The collapsed preview shows COLLAPSE_SHOW lines; the walk never
        // visits the hidden tail before crossing to the next message.
        assert_eq!(
            cursor,
            Cursor {
                message: 1,
                line: 0
            }
        );
        let steps_taken = COLLAPSE_SHOW; // preview walk plus the crossing step
        assert_eq!(
            buf.move_cursor_line(-(steps_taken as isize), 40),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
    }

    #[test]
    fn paragraph_motion_lands_on_first_body_line_of_adjacent_block() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "one\ntwo", 1_000_000, false);
        buf.push_test("alice", "three", 1_001_000, false);
        buf.push_test("bob", "four", 1_002_000, false);

        // alice's two messages share a block; bob starts the next.
        buf.cursor_to_last(40);
        assert_eq!(
            buf.move_cursor_paragraph(-1, 40),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
        assert_eq!(
            buf.move_cursor_paragraph(1, 40),
            Some(Cursor {
                message: 2,
                line: 0
            })
        );
    }

    #[test]
    fn paragraph_motion_clamps_at_buffer_edges() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "one", 1_000_000, false);
        buf.push_test("bob", "two", 1_001_000, false);

        buf.set_cursor_to_message(0);
        assert_eq!(
            buf.move_cursor_paragraph(-1, 40),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
        assert_eq!(
            buf.move_cursor_paragraph(5, 40),
            Some(Cursor {
                message: 1,
                line: 0
            })
        );
    }

    #[test]
    fn visual_range_normalizes_anchor_after_cursor() {
        let mut buf = buffer_with_notices(3);
        let _ = buf.total_lines_exact(40);
        buf.set_cursor_to_message(2);
        assert!(buf.toggle_visual_anchor(40));
        buf.move_cursor_line(-2, 40);

        assert!(buf.is_visual(0, 0));
        assert!(buf.is_visual(2, 0));
        assert_eq!(
            buf.visual_text(40).as_deref(),
            Some("message 0\nmessage 1\nmessage 2")
        );
    }

    #[test]
    fn toggling_visual_anchor_twice_clears_it() {
        let mut buf = buffer_with_notices(1);
        assert!(buf.toggle_visual_anchor(40));
        assert!(buf.has_visual());
        assert!(!buf.toggle_visual_anchor(40));
        assert!(!buf.has_visual());
    }

    #[test]
    fn click_release_without_drag_clears_the_anchor() {
        let mut buf = buffer_with_notices(2);
        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        assert!(buf.drag_is_click());
        buf.end_drag();

        assert!(!buf.has_visual());
        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
        assert!(!buf.is_dragging());
    }

    #[test]
    fn drag_release_keeps_the_visual_range() {
        let mut buf = buffer_with_notices(2);
        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 1,
            line: 0,
        });
        assert!(!buf.drag_is_click());
        buf.end_drag();

        assert!(buf.has_visual());
        assert!(buf.is_visual(0, 0) && buf.is_visual(1, 0));
    }

    #[test]
    fn trim_clears_visual_when_either_endpoint_is_evicted() {
        let mut buf = VirtualChatBuffer::new(3, SyntaxTheme::default());
        for id in 0..3 {
            buf.push_chat(chat_message(id, 1_000 + id, "m"), false);
        }
        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 2,
            line: 0,
        });

        buf.push_chat(chat_message(3, 2_000, "new"), false);

        assert!(!buf.has_visual());
        assert_eq!(
            buf.cursor().map(|cursor| buf.message(cursor.message).id),
            Some(2)
        );
    }

    #[test]
    fn remove_notice_clears_visual_when_cursor_endpoint_is_removed() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_notice("system", "one");
        let notice = buf.push_notice("system", "two");
        buf.begin_drag(Cursor {
            message: 0,
            line: 0,
        });
        buf.drag_to(Cursor {
            message: 1,
            line: 0,
        });

        assert!(buf.remove_notice(notice));

        assert!(!buf.has_visual());
        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
    }

    fn edited_message(id: u64, timestamp_ms: u64, body: &str) -> ChatMessage {
        let mut message = chat_message(id, timestamp_ms, body);
        message.flags.set_edited();
        message
    }

    #[test]
    fn edit_message_relayouts_preserving_cursor() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 1..=3 {
            buf.push_chat(chat_message(id, id * 1_000, "short"), false);
        }
        let _ = buf.total_lines_exact(40);
        buf.set_cursor_to_message(1);
        let lines_before = buf.total_lines_exact(40);

        assert!(buf.edit_message(
            2,
            edited_message(2, 2_000, "a much longer body\nwith a second line")
        ));

        assert_eq!(
            buf.cursor().map(|cursor| buf.message(cursor.message).id),
            Some(2)
        );
        let entry = buf.message(1);
        assert!(entry.edited);
        assert_eq!(entry.body, "a much longer body\nwith a second line");
        assert!(buf.total_lines_exact(40) > lines_before);
        assert!(!buf.edit_message(99, edited_message(99, 9_000, "absent")));
    }

    #[test]
    fn edit_message_preserves_expansion_and_local_flag() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_chat(chat_message(1, 1_000, &fenced(20)), true);
        let _ = buf.total_lines_exact(40);
        assert!(buf.toggle_expand(0, 40));

        assert!(buf.edit_message(1, edited_message(1, 1_000, &fenced(21))));
        let _ = buf.total_lines_exact(40);

        assert!(buf.message(0).local);
        assert!(buf.is_expanded(0));
    }

    #[test]
    fn remove_message_fixes_cursor_and_anchor() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 1..=3 {
            buf.push_chat(chat_message(id, id * 1_000, "m"), false);
        }
        let _ = buf.total_lines_exact(40);
        buf.set_cursor_to_message(2);

        assert!(buf.remove_message(1));

        assert_eq!(buf.len(), 2);
        assert_eq!(
            buf.cursor().map(|cursor| buf.message(cursor.message).id),
            Some(3)
        );
        assert!(buf.find_message(1).is_none());
        assert!(!buf.remove_message(1));
    }

    #[test]
    fn remove_message_under_cursor_lands_on_neighbor() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 1..=2 {
            buf.push_chat(chat_message(id, id * 1_000, "m"), false);
        }
        let _ = buf.total_lines_exact(40);
        buf.set_cursor_to_message(1);
        buf.toggle_visual_anchor(40);

        assert!(buf.remove_message(2));

        assert!(!buf.has_visual());
        assert_eq!(
            buf.cursor().map(|cursor| buf.message(cursor.message).id),
            Some(1)
        );
    }

    #[test]
    fn edited_message_breaks_block_grouping() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 1..=3 {
            buf.push_chat(chat_message(id, 1_000 + id, "grouped"), false);
        }
        assert_eq!(heading_ids(&mut buf, 40), vec![1]);

        assert!(buf.edit_message(2, edited_message(2, 1_002, "revised")));

        assert_eq!(heading_ids(&mut buf, 40), vec![1, 2, 3]);
    }

    #[test]
    fn insert_before_cursor_shifts_it() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_chat(chat_message(1, 1_000, "first"), false);
        buf.push_chat(chat_message(4, 4_000, "fourth"), false);
        buf.set_cursor_to_message(1);

        buf.insert_chat(chat_message(2, 2_000, "second"), false);

        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 2,
                line: 0
            })
        );
        assert_eq!(buf.message(2).id, 4);
    }

    #[test]
    fn eviction_clamps_cursor_to_oldest_resident() {
        let mut buf = VirtualChatBuffer::new(3, SyntaxTheme::default());
        for id in 0..3 {
            buf.push_chat(chat_message(id, 1_000 + id, "m"), false);
        }
        buf.set_cursor_to_message(0);
        buf.scroll_up(1, 40, 2);

        buf.push_chat(chat_message(3, 2_000, "new"), false);

        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
        assert_eq!(buf.message(0).id, 1);
    }

    #[test]
    fn follow_bottom_advances_cursor_on_new_message() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_chat(chat_message(1, 1_000, "old"), false);
        buf.ensure_cursor(40);

        buf.push_chat(chat_message(2, 2_000, "alpha\nbeta"), false);

        assert_eq!(
            buf.ensure_cursor(40),
            Some(Cursor {
                message: 1,
                line: 1
            }),
            "cursor follows to the new newest message's last line"
        );
    }

    #[test]
    fn notice_append_follows_bottom_cursor() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_chat(chat_message(1, 1_000, "old"), false);
        buf.ensure_cursor(40);

        let notice = buf.push_notice("system", "connected");

        assert_eq!(
            buf.ensure_cursor(40),
            Some(Cursor {
                message: 1,
                line: 0
            }),
            "cursor follows the newest notice while at bottom"
        );
        assert_eq!(buf.message(1).notice_id, Some(notice));
    }

    #[test]
    fn scrolled_up_cursor_sticks_to_its_message() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 0..6 {
            buf.push_chat(chat_message(id, 1_000 + id, "m"), false);
        }
        buf.set_cursor_to_message(5);
        buf.scroll_up(2, 40, 3);

        buf.push_chat(chat_message(6, 2_000, "new"), false);

        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 5,
                line: 0
            })
        );
    }

    #[test]
    fn visual_anchor_suppresses_follow_bottom() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_chat(chat_message(1, 1_000, "old"), false);
        buf.ensure_cursor(40);
        assert!(buf.toggle_visual_anchor(40));

        buf.push_chat(chat_message(2, 2_000, "new"), false);

        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 0,
                line: 0
            })
        );
    }

    #[test]
    fn collapse_clamps_cursor_into_preview() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", &fenced(13), 1_000_000, false);
        assert!(buf.toggle_expand(0, 40), "expand");
        buf.cursor_to_last(40);
        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 0,
                line: 12
            })
        );

        assert!(buf.toggle_expand(0, 40), "collapse");

        assert_eq!(
            buf.cursor(),
            Some(Cursor {
                message: 0,
                line: COLLAPSE_SHOW - 1,
            })
        );
    }

    #[test]
    fn cursor_line_text_slices_one_wrapped_row() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.push_test("alice", "alpha\nbeta", 1_000_000, false);
        let _ = buf.total_lines_exact(40);

        buf.set_cursor_to_message(0);
        buf.move_cursor_line(1, 40);

        assert_eq!(buf.cursor_line_text().as_deref(), Some("beta"));
    }

    #[test]
    fn cursor_ref_scans_only_the_cursor_message() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        buf.set_room_id(rpc::ids::RoomId(1));
        buf.push_test("alice", "target", 1_000_000, false);
        let code = ref_code(1);
        buf.push_test("alice", &format!("see @@{code}"), 1_001_000, false);
        buf.push_test("alice", "no reference here", 1_002_000, false);
        let _ = buf.total_lines_exact(95);

        buf.set_cursor_to_message(2);
        assert_eq!(
            buf.cursor_ref(),
            None,
            "sibling block members are not scanned"
        );
        buf.set_cursor_to_message(1);
        assert_eq!(buf.cursor_ref().map(|target| target.message_id.0), Some(1));
    }

    #[test]
    fn visual_text_requires_an_anchor() {
        let mut buf = buffer_with_notices(2);
        let _ = buf.total_lines_exact(40);
        buf.ensure_cursor(40);
        assert_eq!(buf.visual_text(40), None);
    }

    #[test]
    fn keep_cursor_visible_scrolls_offscreen_cursor_into_view() {
        let mut buf = buffer_with_notices(20);
        let (width, height) = (40, 5);
        buf.ensure_cursor(width);
        buf.cursor_to_first();
        buf.keep_cursor_visible(width, height);

        let rows = buf.visible_lines(width, height, 0);
        assert!(
            rows.iter()
                .any(|row| row.kind == LineKind::Body && row.message == 0),
            "oldest message's body is on screen"
        );

        buf.cursor_to_last(width);
        buf.keep_cursor_visible(width, height);
        assert_eq!(buf.scroll_offset(), 0, "newest line sits at the bottom");
    }

    #[test]
    fn cached_navigation_reuses_full_layout_index() {
        let mut buf = buffer_with_notices(30);
        let (width, height) = (40, 6);
        let _ = buf.total_lines_exact(width);
        let rebuilds = buf.layout_index.full_rebuilds;

        buf.cursor_to_first();
        for _ in 0..20 {
            buf.move_cursor_line(1, width);
            buf.keep_cursor_visible(width, height);
        }

        assert_eq!(buf.layout_index.full_rebuilds, rebuilds);
    }

    #[test]
    fn rendering_builds_layout_index_on_cache_miss() {
        let mut buf = buffer_with_notices(30);
        let (width, height) = (40, 6);
        assert!(!buf.layout_index.valid);

        let rows = buf.visible_lines(width, height, 0);

        assert_eq!(rows.len(), height as usize);
        assert!(buf.layout_index.valid);
        assert_eq!(buf.layout_index.width, width);
        let rebuilds = buf.layout_index.full_rebuilds;

        let rows = buf.visible_lines(width, height, 0);

        assert_eq!(rows.len(), height as usize);
        assert_eq!(buf.layout_index.full_rebuilds, rebuilds);
    }

    #[test]
    fn tail_append_repairs_cache_without_full_rebuild() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for i in 0..5 {
            buf.push_chat(chat_message(i, 1_000 + i, "m"), false);
        }
        let _ = buf.total_lines_exact(40);
        let rebuilds = buf.layout_index.full_rebuilds;

        buf.push_chat(chat_message(5, 2_000, "new"), false);
        let total = buf.total_lines_exact(40);

        assert_eq!(buf.layout_index.full_rebuilds, rebuilds);
        assert_eq!(total, buf.visible_lines(40, 10_000, 0).len());
    }

    #[test]
    fn capped_tail_append_repairs_cache_without_full_rebuild() {
        let mut buf = VirtualChatBuffer::new(5, SyntaxTheme::default());
        for i in 0..5 {
            buf.push_chat(chat_message(i, 1_000 + i, "m"), false);
        }
        let _ = buf.total_lines_exact(40);
        let rebuilds = buf.layout_index.full_rebuilds;

        buf.push_chat(chat_message(5, 2_000, "new"), false);
        let total = buf.total_lines_exact(40);

        assert_eq!(buf.len(), 5);
        assert_eq!(buf.message(0).id, 1);
        assert!(buf.layout_index.valid);
        assert_eq!(buf.layout_index.full_rebuilds, rebuilds);
        assert_eq!(total, buf.visible_lines(40, 10_000, 0).len());
    }

    #[test]
    fn layout_epoch_stable_across_tail_appends() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        let epoch = buf.layout_epoch();
        for id in 1..6 {
            buf.push_chat(chat_message(id, id * 1_000, "appended"), false);
        }
        buf.push_notice("net", "joined");
        assert_eq!(buf.layout_epoch(), epoch);
    }

    #[test]
    fn layout_epoch_bumps_when_existing_rows_shift() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 10..15 {
            buf.push_chat(chat_message(id, id * 1_000, "resident"), false);
        }

        let epoch = buf.layout_epoch();
        buf.prepend_chat(vec![(chat_message(1, 1_000, "older"), false)]);
        assert_ne!(buf.layout_epoch(), epoch, "prepend");

        let epoch = buf.layout_epoch();
        assert!(buf.edit_message(12, chat_message(12, 12_000, "edited")));
        assert_ne!(buf.layout_epoch(), epoch, "edit");

        let epoch = buf.layout_epoch();
        buf.insert_chat(chat_message(2, 2_000, "straggler"), false);
        assert_ne!(buf.layout_epoch(), epoch, "insert");

        let epoch = buf.layout_epoch();
        assert!(buf.remove_message(13));
        assert_ne!(buf.layout_epoch(), epoch, "remove");

        let epoch = buf.layout_epoch();
        buf.on_reflow(30);
        assert_ne!(buf.layout_epoch(), epoch, "reflow");

        let epoch = buf.layout_epoch();
        buf.clear();
        assert_ne!(buf.layout_epoch(), epoch, "clear");
    }

    #[test]
    fn layout_epoch_bumps_when_capacity_eviction_shifts_rows() {
        let mut buf = VirtualChatBuffer::new(3, SyntaxTheme::default());
        for id in 1..=3 {
            buf.push_chat(chat_message(id, id * 1_000, "resident"), false);
        }
        let epoch = buf.layout_epoch();
        buf.push_chat(chat_message(4, 4_000, "evicts the oldest"), false);
        assert_ne!(buf.layout_epoch(), epoch);
    }

    #[test]
    fn layout_epoch_bumps_only_on_effective_expand_toggle() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        let long_body = (0..COLLAPSE_LIMIT + 2)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        buf.push_chat(chat_message(1, 1_000, &long_body), false);
        buf.push_chat(chat_message(2, 2_000, "short"), false);

        let epoch = buf.layout_epoch();
        assert!(buf.toggle_expand(0, 40));
        assert_ne!(buf.layout_epoch(), epoch);

        let epoch = buf.layout_epoch();
        assert!(!buf.toggle_expand(1, 40));
        assert_eq!(buf.layout_epoch(), epoch);
    }

    #[test]
    fn viewport_top_matches_visible_window_and_clamps() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        assert_eq!(buf.viewport_top(40, 5), 0, "empty buffer");
        for id in 1..=10 {
            buf.push_chat(chat_message(id, id * 1_000, "m"), false);
        }
        let top = buf.viewport_top(40, 5);
        assert_eq!(top, buf.max_scroll(40, 5), "pinned to the bottom");

        buf.scroll_up(2, 40, 5);
        assert_eq!(buf.viewport_top(40, 5), top - 2);

        buf.scroll_up(1_000, 40, 5);
        assert_eq!(buf.viewport_top(40, 5), 0, "clamped at the oldest line");

        assert_eq!(
            buf.viewport_top(40, 10_000),
            0,
            "window taller than content"
        );
        assert_eq!(buf.viewport_top(40, 0), 0);
    }

    #[test]
    fn viewport_top_advances_by_appended_rows_while_pinned() {
        let mut buf = VirtualChatBuffer::new(1000, SyntaxTheme::default());
        for id in 1..=10 {
            buf.push_chat(chat_message(id, id * 1_000, "m"), false);
        }
        buf.bottom();
        let top = buf.viewport_top(40, 5);

        buf.push_chat(chat_message(11, 11_000, "new"), false);

        let new_top = buf.viewport_top(40, 5);
        assert!(new_top > top, "append while pinned shifts the window down");
        assert_eq!(new_top, buf.max_scroll(40, 5));
    }
}
