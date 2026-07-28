use std::ops::Range;

use extui::{Style, vt::Modifier};
use rpc::ids::{FileTransferId, MessageId, RoomId};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    app::room::{HistoryDelta, Revision, RoomHistoryRef},
    theme::SyntaxTheme,
};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoticeId(u64);

/// Stable identity of one canonical room-history entry. Viewports retain these
/// ids and derived layout state, never the sender or body they identify.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HistoryEntryId {
    Message(MessageId),
    Notice(u64),
    LocalNotice(NoticeId),
}

#[cfg(test)]
mod viewport_tests {
    use super::*;
    use crate::app::room::RoomHistoryFixture as TestHistory;

    #[test]
    fn viewport_retains_only_ids_and_reads_current_body() {
        let mut history = TestHistory::new();
        history.push(1, "alice", "first");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        assert_eq!(
            viewport.record(&history.history(), 0).unwrap().body,
            "first"
        );

        history.edit(1, "edited");
        viewport.reconcile(&history.history());
        assert_eq!(
            viewport.record(&history.history(), 0).unwrap().body,
            "edited"
        );
    }

    #[test]
    fn stable_cursor_survives_prepend() {
        let mut history = TestHistory::new();
        history.push(10, "alice", "ten");
        history.push(11, "alice", "eleven");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.set_cursor(HistoryEntryId::Message(MessageId(11)));
        history.push(1, "alice", "one");
        viewport.reconcile(&history.history());
        assert_eq!(
            viewport.cursor().unwrap().entry,
            HistoryEntryId::Message(MessageId(11))
        );
        assert_eq!(
            viewport.record(&history.history(), 2).unwrap().entry_id,
            HistoryEntryId::Message(MessageId(11))
        );
    }

    #[test]
    fn deleted_cursor_chooses_newer_survivor_across_prepend() {
        let mut history = TestHistory::new();
        history.push(10, "alice", "ten");
        history.push(11, "alice", "eleven");
        history.push(12, "alice", "twelve");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.set_cursor(HistoryEntryId::Message(MessageId(11)));
        history.remove(11);
        history.push(1, "alice", "one");
        viewport.reconcile(&history.history());

        let cursor = viewport.cursor().unwrap();
        assert_eq!(cursor.entry, HistoryEntryId::Message(MessageId(12)));
    }

    #[test]
    fn clear_boundary_hides_old_history_but_keeps_new_messages() {
        let mut history = TestHistory::new();
        history.push(1, "alice", "old");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.clear_scrollback();
        assert!(viewport.is_empty());
        history.push(2, "alice", "new");
        viewport.reconcile(&history.history());
        assert_eq!(viewport.len(), 1);
        assert_eq!(viewport.record(&history.history(), 0).unwrap().body, "new");
    }

    #[test]
    fn clear_boundary_survives_deletion_of_its_last_message() {
        let mut history = TestHistory::new();
        history.push(1, "alice", "older");
        history.push(2, "alice", "boundary");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.clear_scrollback();

        history.remove(2);
        viewport.reconcile(&history.history());
        assert!(viewport.is_empty());

        history.push(3, "alice", "new");
        viewport.reconcile(&history.history());
        assert_eq!(viewport.len(), 1);
        assert_eq!(viewport.record(&history.history(), 0).unwrap().body, "new");
    }

    #[test]
    fn generation_reset_discards_clear_boundary() {
        let mut history = TestHistory::new();
        history.push(1, "alice", "old");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.clear_scrollback();
        assert!(viewport.is_empty());

        history.advance_generation();
        viewport.reconcile(&history.history());
        assert_eq!(viewport.record(&history.history(), 0).unwrap().body, "old");
    }

    #[test]
    fn wrapping_and_selection_read_borrowed_content() {
        let mut history = TestHistory::new();
        history.push(1, "alice", "alpha beta gamma");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.ensure_cursor(&history.history(), 6);
        viewport.toggle_visual_anchor(&history.history(), 6);
        assert_eq!(
            viewport.visual_text(&history.history(), 6).as_deref(),
            Some("gamma")
        );
    }

    #[test]
    fn grouping_and_collapse_layout_remain_view_local() {
        let mut history = TestHistory::new();
        history.push(1, "alice", &"line\n".repeat(14));
        let mut first = ChatViewport::new(SyntaxTheme::default());
        let mut second = ChatViewport::new(SyntaxTheme::default());
        first.reconcile(&history.history());
        second.reconcile(&history.history());
        assert!(first.toggle_expand(&history.history(), 0, 40));
        first.visible_lines(&history.history(), 40, 40, 0);
        second.visible_lines(&history.history(), 40, 40, 0);
        assert!(first.is_expanded(0));
        assert!(second.is_collapsed(0));
    }

    #[test]
    fn layout_cache_retains_only_the_visible_window() {
        let mut history = TestHistory::new();
        for id in 1..=200 {
            history.push(id, &format!("user-{id}"), &format!("message body {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        let visible = viewport.visible_lines(&history.history(), 40, 6, 2);

        let resident = viewport
            .entries
            .iter()
            .filter(|entry| entry.layout.is_some())
            .count();
        assert!(!visible.is_empty());
        assert!(resident <= 10, "{resident} layouts remained resident");
    }

    #[test]
    fn configured_overscan_above_256_is_honored() {
        let mut history = TestHistory::new();
        for id in 1..=400 {
            history.push(id, &format!("user-{id}"), &format!("message body {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 1, 300);

        let resident = viewport
            .entries
            .iter()
            .filter(|entry| entry.layout.is_some())
            .count();
        assert!(resident > 130, "{resident} layouts remained resident");
    }

    #[test]
    fn append_while_scrolled_preserves_the_visible_entry_anchor() {
        let mut history = TestHistory::new();
        for id in 1..=30 {
            history.push(id, &format!("user-{id}"), &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.scroll_up(&history.history(), 8, 40, 6);
        let before = viewport.visible_lines(&history.history(), 40, 6, 1);
        let before = viewport.entries[before[0].message].id;

        history.push(31, "new-user", "new tail");
        viewport.reconcile(&history.history());
        let after = viewport.visible_lines(&history.history(), 40, 6, 1);
        let after = viewport.entries[after[0].message].id;

        assert_eq!(after, before);
    }

    #[test]
    fn tail_append_repairs_the_existing_layout_index() {
        let mut history = TestHistory::new();
        for id in 1..=200 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);
        let rebuilds = viewport.layout_index.full_rebuilds;

        history.push(201, "alice", "new tail");
        history.push(202, "alice", "second batched tail");
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);

        assert_eq!(viewport.layout_index.full_rebuilds, rebuilds);
        assert_eq!(viewport.layout_index.line_counts.len(), viewport.len());
    }

    #[test]
    fn edit_remeasures_only_the_changed_non_reference_layout() {
        let mut history = TestHistory::new();
        for id in 1..=200 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);
        let measured = viewport.layout_index.measured_messages;

        history.edit(
            100,
            "changed body that wraps onto another line at this width",
        );
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);

        assert_eq!(viewport.layout_index.measured_messages, measured + 1);
        assert_eq!(viewport.layout_index.line_counts.len(), viewport.len());
    }

    #[test]
    fn front_eviction_drops_the_prefix_without_remeasuring() {
        let mut history = TestHistory::new();
        for id in 1..=200 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);
        let measured = viewport.layout_index.measured_messages;

        history.evict(50);
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);

        assert_eq!(viewport.len(), 150);
        assert_eq!(
            viewport.record(&history.history(), 0).unwrap().entry_id,
            HistoryEntryId::Message(MessageId(51))
        );
        // Retention removes a prefix and nothing else, so the survivors' rows
        // are regrouped from counts already taken rather than laid out again.
        assert_eq!(viewport.layout_index.measured_messages, measured);
        assert_eq!(viewport.layout_index.line_counts.len(), viewport.len());
    }

    #[test]
    fn tombstone_drops_one_entry_without_remeasuring_the_rest() {
        let mut history = TestHistory::new();
        for id in 1..=200 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);
        let measured = viewport.layout_index.measured_messages;

        history.tombstone(100);
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);

        assert_eq!(viewport.len(), 199);
        assert!(
            viewport
                .entry_index(HistoryEntryId::Message(MessageId(100)))
                .is_none()
        );
        // The delta named the id that left, so nothing else is laid out again.
        assert_eq!(viewport.layout_index.measured_messages, measured);
        assert_eq!(viewport.layout_index.line_counts.len(), viewport.len());
    }

    #[test]
    fn prepended_page_keeps_the_measurements_of_surviving_entries() {
        let mut history = TestHistory::new();
        for id in 100..=200 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);
        let measured = viewport.layout_index.measured_messages;

        history.prepend(&[1, 2, 3]);
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 2);

        assert_eq!(viewport.len(), 104);
        assert_eq!(
            viewport.record(&history.history(), 0).unwrap().entry_id,
            HistoryEntryId::Message(MessageId(1))
        );
        // A prepend rebuilds the id sequence but touches no resident body, so
        // only the three arrivals are measured.
        assert_eq!(viewport.layout_index.measured_messages, measured + 3);
    }

    #[test]
    fn page_that_prepends_and_splices_does_not_duplicate_the_spliced_entry() {
        let mut history = TestHistory::new();
        history.push(100, "alice", "hundred");
        history.push(200, "alice", "two hundred");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());

        // One page, two shapes: 1 lands under the resident front, 150 threads
        // between residents. The room journals a rebuild *and* a splice, and a
        // rebuild already reflects the splice.
        history.merge_page(&[1, 150]);
        viewport.reconcile(&history.history());

        assert_eq!(
            viewport.entry_ids().collect::<Vec<_>>(),
            history.history().entry_ids()
        );
    }

    #[test]
    fn falling_past_the_journal_rebuilds_instead_of_desynchronizing() {
        let mut history = TestHistory::new();
        history.push(1, "alice", "first");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        let stale = history.revision();

        // Well past the journal cap, so the span from `stale` is no longer
        // covered and replay must refuse it.
        for id in 2..=600 {
            history.push(id, "alice", &format!("message {id}"));
        }
        assert!(history.history().deltas_since(stale).is_none());
        viewport.reconcile(&history.history());

        assert_eq!(viewport.len(), 600);
        assert_eq!(
            viewport.record(&history.history(), 599).unwrap().body,
            "message 600"
        );
        assert_eq!(
            viewport.entry_ids().collect::<Vec<_>>(),
            history.history().entry_ids()
        );
    }

    #[test]
    fn replayed_appends_match_a_rebuild_entry_for_entry() {
        let mut history = TestHistory::new();
        history.push(1, "alice", "first");
        history.notice("connected", true);
        history.push(2, "bob", "second");
        let mut replayed = ChatViewport::new(SyntaxTheme::default());
        replayed.reconcile(&history.history());

        history.push(3, "alice", "third");
        history.notice("reconnected", true);
        history.tombstone(2);
        history.edit(1, "edited");
        replayed.reconcile(&history.history());

        // A viewport seeing every step must land where one seeing only the end
        // state does; the journal is an optimization, never a different answer.
        let mut rebuilt = ChatViewport::new(SyntaxTheme::default());
        rebuilt.reconcile(&history.history());
        assert_eq!(
            replayed.entry_ids().collect::<Vec<_>>(),
            rebuilt.entry_ids().collect::<Vec<_>>()
        );
        assert_eq!(
            replayed.entry_ids().collect::<Vec<_>>(),
            history.history().entry_ids()
        );
    }

    #[test]
    fn eviction_repoints_a_cursor_that_left_with_the_prefix() {
        let mut history = TestHistory::new();
        for id in 1..=10 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.set_cursor(HistoryEntryId::Message(MessageId(2)));

        history.evict(4);
        viewport.reconcile(&history.history());

        assert_eq!(
            viewport.cursor().unwrap().entry,
            HistoryEntryId::Message(MessageId(5))
        );
    }

    #[test]
    fn stale_view_id_lays_out_without_panicking() {
        let mut history = TestHistory::new();
        for id in 1..=3 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());

        // The core can tombstone between one reconcile and the next input
        // event, leaving this view holding an id canonical history no longer
        // resolves. Laying it out must degrade, not abort the render thread.
        history.remove(2);
        let lines = viewport.visible_lines(&history.history(), 40, 8, 2);

        assert!(!lines.is_empty());
    }

    #[test]
    fn notice_scroll_policy_survives_canonical_projection() {
        let mut history = TestHistory::new();
        for id in 1..=30 {
            history.push(id, "alice", &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.scroll_up(&history.history(), 8, 40, 6);
        assert!(viewport.scroll_offset() > 0);

        history.notice("gap", false);
        viewport.reconcile(&history.history());
        assert!(viewport.scroll_offset() > 0);

        history.notice("error", true);
        viewport.reconcile(&history.history());
        assert_eq!(viewport.scroll_offset(), 0);
    }
}

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
    pub entry: HistoryEntryId,
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

/// A borrowed canonical record presented to the viewport for one operation.
#[derive(Clone, Copy)]
pub struct ChatRecord<'a> {
    pub entry_id: HistoryEntryId,
    pub sender: &'a str,
    pub body: &'a str,
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
    pub notice_kind: Option<NoticeKind>,
}

struct ViewEntry {
    id: HistoryEntryId,
    /// Whether a collapsible (over [`COLLAPSE_LIMIT`] lines) message is expanded.
    expanded: bool,
    /// Whether the body carries a message reference. A reference pill resolves
    /// through *another* canonical record, so any change to a different id can
    /// restyle this one; the flag keeps that fan-out a bool scan instead of a
    /// substring search over every resident body.
    has_refs: bool,
    /// Present only inside the bounded visible/overscan cache window.
    layout: Option<Box<MessageLayout>>,
}

impl ViewEntry {
    fn layout(&self) -> &MessageLayout {
        self.layout
            .as_deref()
            .expect("message layout must be ensured before use")
    }

    fn layout_mut(&mut self) -> &mut MessageLayout {
        self.layout
            .get_or_insert_with(|| Box::new(MessageLayout::new()))
    }

    fn invalidate_layout(&mut self) {
        if let Some(layout) = self.layout.as_deref_mut() {
            layout.invalidate();
        }
    }
}

fn remap_missing_entry(
    previous: &[HistoryEntryId],
    old_position: usize,
    survives: impl Fn(HistoryEntryId) -> bool,
) -> Option<HistoryEntryId> {
    previous
        .get(old_position.saturating_add(1)..)
        .and_then(|newer| newer.iter().copied().find(|id| survives(*id)))
        .or_else(|| {
            previous[..old_position.min(previous.len())]
                .iter()
                .rev()
                .copied()
                .find(|id| survives(*id))
        })
}

struct LocalNotice {
    id: NoticeId,
    sender: String,
    body: String,
    kind: NoticeKind,
    after: Option<MessageId>,
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
    #[cfg(test)]
    measured_messages: usize,
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
struct LayoutCursor {
    message: usize,
    line: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewCursor {
    pub entry: HistoryEntryId,
    pub line: usize,
}

#[derive(Clone, Copy)]
struct ViewScrollAnchor {
    entry: HistoryEntryId,
    line: usize,
    kind: LineKind,
}

/// What a batch of applied deltas left for the end of the reconcile pass.
///
/// Deltas are applied one at a time but their consequences are not: repacking
/// blocks, re-resolving reference pills and bumping the render epoch are all
/// once-per-pass work, so each delta records that it is needed and the pass
/// pays for it once.
/// Where the reader sat before a reconcile pass, sampled before anything moves.
#[derive(Clone, Copy)]
struct FollowState {
    /// The viewport was pinned to the newest row.
    following: bool,
    /// The cursor sat on the newest entry.
    cursor_at_tail: bool,
}

#[derive(Default)]
struct ReplayEffects {
    /// Line counts moved, so blocks must be repacked from them.
    repack: bool,
    /// Rows already on screen moved, so the renderer cannot scroll its existing
    /// output into place.
    reflow: bool,
    /// A canonical id arrived, changed or left, so reference pill labels
    /// resolving through it may have changed.
    references_dirty: bool,
    /// A newly materialized notice asked every view to snap to the bottom.
    scroll_bottom: bool,
}

#[derive(Clone, Copy, Default)]
struct ClearBoundary {
    message: Option<MessageId>,
    notice: Option<u64>,
    local_notice: Option<NoticeId>,
}

impl ClearBoundary {
    fn observe(&mut self, id: HistoryEntryId) {
        match id {
            HistoryEntryId::Message(id) => {
                self.message = Some(self.message.map_or(id, |current| current.max(id)));
            }
            HistoryEntryId::Notice(id) => {
                self.notice = Some(self.notice.map_or(id, |current| current.max(id)));
            }
            HistoryEntryId::LocalNotice(id) => {
                self.local_notice = Some(self.local_notice.map_or(id, |current| current.max(id)));
            }
        }
    }

    fn hides(self, id: HistoryEntryId) -> bool {
        match id {
            HistoryEntryId::Message(id) => self.message.is_some_and(|boundary| id <= boundary),
            HistoryEntryId::Notice(id) => self.notice.is_some_and(|boundary| id <= boundary),
            HistoryEntryId::LocalNotice(id) => {
                self.local_notice.is_some_and(|boundary| id <= boundary)
            }
        }
    }
}

pub struct ChatViewport {
    entries: Vec<ViewEntry>,
    observed_generation: Option<u64>,
    /// The canonical revision these entries reflect. Catching up replays
    /// `deltas_since(observed_revision)`, or rebuilds when that span has fallen
    /// out of the journal.
    observed_revision: Revision,
    scroll_offset: usize,
    scroll_anchor: Option<ViewScrollAnchor>,
    /// Navigation cursor; `None` only while the buffer is empty. A stale
    /// `line` (after reflow or collapse) is clamped lazily by
    /// [`Self::ensure_cursor`].
    cursor: Option<ViewCursor>,
    /// `Some` means a line-wise visual selection spans `anchor..=cursor`,
    /// order-normalized. Keyboard visual mode and mouse drags share this.
    anchor: Option<ViewCursor>,
    /// A mouse drag is in progress; releasing with `anchor == cursor` (a
    /// click) clears the anchor, leaving a plain cursor move.
    dragging: bool,
    syntax: SyntaxTheme,
    room_id: Option<RoomId>,
    local_notices: Vec<LocalNotice>,
    max_local_notices: usize,
    next_notice_id: u64,
    clear_boundary: Option<ClearBoundary>,
    /// Advances on every `/clear`. Derived overlays key their index on it, so
    /// a clear invalidates them even though canonical history did not move.
    clear_generation: u64,
    layout_index: LayoutIndex,
    /// Advances on any mutation that can move existing rendered rows: edits,
    /// removals, prepends, eviction, collapse toggles, reflow. Pure tail
    /// appends leave it stable, so an unchanged epoch plus a viewport-top
    /// delta means the visible rows merely shifted and the renderer may
    /// scroll them instead of rewriting.
    layout_epoch: u64,
}

impl ChatViewport {
    pub fn new(syntax: SyntaxTheme) -> Self {
        Self {
            entries: Vec::new(),
            observed_generation: None,
            observed_revision: 0,
            scroll_offset: 0,
            scroll_anchor: None,
            cursor: None,
            anchor: None,
            dragging: false,
            syntax,
            room_id: None,
            local_notices: Vec::new(),
            max_local_notices: usize::MAX,
            next_notice_id: 1,
            clear_boundary: None,
            clear_generation: 0,
            layout_index: LayoutIndex::default(),
            layout_epoch: 0,
        }
    }

    /// Sets the room this buffer displays, the scope against which message
    /// references resolve.
    pub fn set_room_id(&mut self, room_id: RoomId) {
        self.room_id = Some(room_id);
    }

    pub fn room_id(&self) -> Option<RoomId> {
        self.room_id
    }

    /// Restyles syntax highlighting when the active theme changes. Cached
    /// message layouts are invalidated so already-rendered history recolors on
    /// the next layout pass.
    pub fn set_syntax(&mut self, syntax: SyntaxTheme) {
        if self.syntax == syntax {
            return;
        }
        self.syntax = syntax;
        for entry in &mut self.entries {
            entry.invalidate_layout();
        }
        self.layout_index.invalidate();
        self.bump_layout_epoch();
    }

    /// Reconciles stable view ids and disposable layout state with canonical
    /// history. Payload is always re-read through `history`.
    ///
    /// The canonical room names every change it makes, so the common path is a
    /// replay of the journal entries this view has not seen. A rebuild is the
    /// fallback for exactly two situations: a different room generation, and
    /// falling further behind than the journal reaches.
    pub fn reconcile(&mut self, history: &RoomHistoryRef<'_>) {
        self.max_local_notices = history.max_messages().max(1);
        let local_trimmed = self.local_notices.len() > self.max_local_notices;
        if local_trimmed {
            let excess = self.local_notices.len() - self.max_local_notices;
            self.local_notices.drain(..excess);
        }
        // Sampled before anything moves: whether the reader was pinned to the
        // tail decides where the cursor lands afterwards.
        let follow = self.follow_state();
        if self.observed_generation != Some(history.generation()) {
            self.rebuild(history, follow);
            return;
        }
        if self.observed_revision == history.revision() && !local_trimmed {
            return;
        }
        if local_trimmed {
            // Trimming this view's own notices drops ids the journal never
            // mentioned, so the sequence is rebuilt either way. No canonical
            // body moved, so the measurements survive.
            let mut effects = ReplayEffects::default();
            self.resync_entries(history, false, &mut effects);
            self.observe(history);
            self.settle_cursor(follow);
            self.finish(history, effects);
            return;
        }
        if !self.replay(history, follow) {
            self.rebuild(history, follow);
        }
    }

    /// Whether the reader is pinned to the newest entry, so a pass that appends
    /// past it should carry them along.
    fn follow_state(&self) -> FollowState {
        FollowState {
            following: self.scroll_offset == 0,
            cursor_at_tail: self.entries.is_empty()
                || self.cursor.is_some_and(|cursor| {
                    self.entries
                        .last()
                        .is_some_and(|entry| entry.id == cursor.entry)
                }),
        }
    }

    /// Seeds and re-pins the cursor after a pass moved entries under it.
    fn settle_cursor(&mut self, follow: FollowState) {
        if self.cursor.is_none() && !self.entries.is_empty() {
            self.cursor = self.entries.last().map(|entry| ViewCursor {
                entry: entry.id,
                line: 0,
            });
            self.anchor = None;
        }
        if follow.following && follow.cursor_at_tail && self.anchor.is_none() {
            self.cursor = self.entries.last().map(|entry| ViewCursor {
                entry: entry.id,
                line: usize::MAX,
            });
        }
    }

    /// Discards every derived id and rebuilds from the canonical sequence, for
    /// a change the journal could not describe.
    fn rebuild(&mut self, history: &RoomHistoryRef<'_>, follow: FollowState) {
        let generation_changed = self.observed_generation != Some(history.generation());
        if generation_changed {
            // Ids mean nothing outside their generation, so state derived from
            // the outgoing one cannot be carried across. A view that has not
            // observed any generation keeps its pre-connect notices: they were
            // never bound to a room in the first place.
            if self.observed_generation.is_some() {
                self.entries.clear();
                self.layout_index.clear();
                self.local_notices.clear();
            }
            self.clear_boundary = None;
            self.scroll_offset = 0;
            self.cursor = None;
            self.anchor = None;
            self.scroll_anchor = None;
            self.dragging = false;
        }
        let mut effects = ReplayEffects::default();
        // Nothing named which bodies changed, so no measurement can be trusted.
        self.resync_entries(history, true, &mut effects);
        self.observe(history);
        if generation_changed {
            // A fresh room seeds its cursor on the first navigation or render,
            // like a newly opened buffer, and snapping to notices it has never
            // shown would fight the scroll reset above.
            effects.scroll_bottom = false;
        } else {
            self.settle_cursor(follow);
        }
        self.finish(history, effects);
    }

    /// Replays the canonical changes this view has not applied. Returns false
    /// when the journal no longer covers the span, which demands a rebuild.
    fn replay(&mut self, history: &RoomHistoryRef<'_>, follow: FollowState) -> bool {
        let Some(deltas) = history.deltas_since(self.observed_revision) else {
            return false;
        };
        let mut effects = ReplayEffects::default();
        // A `Relaid` rebuilds from the sequence as it stands *now*, which
        // already reflects every other delta in the batch — including ones
        // journaled after it, as a history page that both prepends and splices
        // does. Applying those on top would insert them a second time, so the
        // rebuild subsumes the batch and only the bodies it names are re-read.
        if deltas
            .clone()
            .any(|delta| matches!(delta, HistoryDelta::Relaid))
        {
            self.resync_entries(history, false, &mut effects);
            for delta in deltas {
                match *delta {
                    HistoryDelta::Replaced(message_id) => {
                        self.replace_entry(
                            history,
                            HistoryEntryId::Message(message_id),
                            &mut effects,
                        );
                        effects.references_dirty = true;
                    }
                    HistoryDelta::Appended(_)
                    | HistoryDelta::Inserted(_)
                    | HistoryDelta::Tombstoned(_)
                    | HistoryDelta::EvictedThrough(_) => effects.references_dirty = true,
                    HistoryDelta::NoticeAdded(_)
                    | HistoryDelta::NoticeRemoved(_)
                    | HistoryDelta::Relaid
                    | HistoryDelta::Relabelled => {}
                }
            }
        } else {
            for delta in deltas {
                self.apply_delta(history, delta, &mut effects);
            }
        }
        self.observe(history);
        self.settle_cursor(follow);
        self.finish(history, effects);
        true
    }

    fn apply_delta(
        &mut self,
        history: &RoomHistoryRef<'_>,
        delta: &HistoryDelta,
        effects: &mut ReplayEffects,
    ) {
        match *delta {
            HistoryDelta::Appended(message_id) => {
                self.insert_entry(history, HistoryEntryId::Message(message_id), None, effects);
                effects.references_dirty = true;
            }
            HistoryDelta::Inserted(message_id) => {
                let at = self.message_insert_position(message_id);
                self.insert_entry(
                    history,
                    HistoryEntryId::Message(message_id),
                    Some(at),
                    effects,
                );
                effects.references_dirty = true;
            }
            HistoryDelta::Replaced(message_id) => {
                self.replace_entry(history, HistoryEntryId::Message(message_id), effects);
                effects.references_dirty = true;
            }
            HistoryDelta::Tombstoned(message_id) => {
                self.remove_entry(HistoryEntryId::Message(message_id), effects);
                effects.references_dirty = true;
            }
            HistoryDelta::EvictedThrough(watermark) => {
                self.drop_evicted_front(watermark, effects);
                effects.references_dirty = true;
            }
            HistoryDelta::NoticeAdded(key) => {
                self.insert_entry(history, HistoryEntryId::Notice(key), None, effects);
            }
            HistoryDelta::NoticeRemoved(key) => {
                self.remove_entry(HistoryEntryId::Notice(key), effects);
            }
            HistoryDelta::Relaid => {
                unreachable!("a batch carrying a rebuild is handled whole by replay")
            }
            HistoryDelta::Relabelled => {
                // Marks live in the heading, which the renderer draws from the
                // record every frame. No body, id or measurement moved.
                effects.reflow = true;
            }
        }
    }

    /// Rebuilds the id sequence from canonical history, carrying per-entry
    /// state across for every id that survives.
    ///
    /// `remeasure` drops the preserved line counts, for a caller that cannot
    /// rule out a body having changed unseen.
    fn resync_entries(
        &mut self,
        history: &RoomHistoryRef<'_>,
        remeasure: bool,
        effects: &mut ReplayEffects,
    ) {
        let cursor_position = self
            .cursor
            .and_then(|cursor| self.entry_index(cursor.entry));
        let anchor_position = self
            .anchor
            .and_then(|anchor| self.entry_index(anchor.entry));
        let scroll_anchor_position = self
            .scroll_anchor
            .and_then(|anchor| self.entry_index(anchor.entry));
        let previous_ids = self
            .entries
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();

        let counts_valid = !remeasure && self.counts_valid();
        let width = self.layout_index.width;
        let preserved_counts =
            counts_valid.then(|| std::mem::take(&mut self.layout_index.line_counts));
        let mut previous = self
            .entries
            .drain(..)
            .enumerate()
            .map(|(index, entry)| {
                let lines = preserved_counts.as_ref().map(|counts| counts[index]);
                (entry.id, (entry, lines))
            })
            .collect::<hashbrown::HashMap<_, _>>();

        let ids = self.sequence_ids(history);
        let mut entries = Vec::with_capacity(ids.len());
        let mut line_counts = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(has_refs) = self.body_has_references(history, id) else {
                continue;
            };
            match previous.remove(&id) {
                Some((mut entry, lines)) => {
                    entry.has_refs = has_refs;
                    entries.push(entry);
                    line_counts.push(lines);
                }
                None => {
                    if history.notice_scrolls_bottom(id) {
                        effects.scroll_bottom = true;
                    }
                    entries.push(ViewEntry {
                        id,
                        expanded: false,
                        has_refs,
                        layout: None,
                    });
                    line_counts.push(None);
                }
            }
        }
        self.entries = entries;
        self.room_id = history.room_id();

        // An id that survived keeps its cursor. One that did not hands the
        // cursor to the nearest survivor, preferring the newer side.
        let survives = |id: HistoryEntryId| !previous.contains_key(&id);
        let successor = |id: HistoryEntryId, position: Option<usize>| {
            survives(id)
                .then_some(id)
                .or_else(|| remap_missing_entry(&previous_ids, position?, survives))
        };
        self.cursor = self.cursor.and_then(|cursor| {
            Some(ViewCursor {
                entry: successor(cursor.entry, cursor_position)?,
                line: cursor.line,
            })
        });
        self.anchor = self.anchor.and_then(|anchor| {
            Some(ViewCursor {
                entry: successor(anchor.entry, anchor_position)?,
                line: anchor.line,
            })
        });
        self.scroll_anchor = self.scroll_anchor.and_then(|anchor| {
            Some(ViewScrollAnchor {
                entry: successor(anchor.entry, scroll_anchor_position)?,
                line: anchor.line,
                kind: anchor.kind,
            })
        });
        if counts_valid {
            self.rebuild_layout_index_from_counts(history, width, line_counts);
        } else {
            self.layout_index.invalidate();
        }
        effects.reflow = true;
        // The index was just installed from the counts carried across.
        effects.repack = false;
    }

    /// The ids this view should display: canonical history with this view's own
    /// notices threaded in and whatever a `/clear` hid removed.
    fn sequence_ids(&self, history: &RoomHistoryRef<'_>) -> Vec<HistoryEntryId> {
        let canonical_ids = history.entry_ids();
        let mut ids = Vec::with_capacity(canonical_ids.len() + self.local_notices.len());
        let mut local = self.local_notices.iter().peekable();
        for id in canonical_ids {
            if let HistoryEntryId::Message(message_id) = id {
                while local
                    .peek()
                    .is_some_and(|notice| notice.after.is_none_or(|after| after < message_id))
                {
                    ids.push(HistoryEntryId::LocalNotice(
                        local.next().expect("peeked local notice").id,
                    ));
                }
            }
            ids.push(id);
        }
        ids.extend(local.map(|notice| HistoryEntryId::LocalNotice(notice.id)));
        if let Some(boundary) = self.clear_boundary {
            ids.retain(|id| !boundary.hides(*id));
        }
        ids
    }

    /// Whether `id` resolves to a displayable record, and whether its body
    /// carries a message reference. `None` means the id has no visible record —
    /// a tombstone, or an entry this view's `/clear` hid.
    fn body_has_references(
        &self,
        history: &RoomHistoryRef<'_>,
        id: HistoryEntryId,
    ) -> Option<bool> {
        self.resolve_record(history, id).map(|record| {
            record
                .body
                .contains(chatt_message_format::reference::REF_PREFIX)
        })
    }

    /// Records the canonical position this view has caught up to.
    fn observe(&mut self, history: &RoomHistoryRef<'_>) {
        self.observed_generation = Some(history.generation());
        self.observed_revision = history.revision();
    }

    /// Whether the row index's per-entry line counts still line up with the
    /// entries they measure.
    fn counts_valid(&self) -> bool {
        self.layout_index.valid && self.layout_index.line_counts.len() == self.entries.len()
    }

    /// Applies whatever the replayed deltas left outstanding: reference pill
    /// labels that resolve through changed records, a block repack from the
    /// carried line counts, and the render-side epoch.
    fn finish(&mut self, history: &RoomHistoryRef<'_>, mut effects: ReplayEffects) {
        if effects.references_dirty {
            self.remeasure_reference_bodies(history, &mut effects);
        }
        if effects.repack {
            if self.counts_valid() {
                let line_counts = std::mem::take(&mut self.layout_index.line_counts);
                self.install_layout_index(history, line_counts);
            } else {
                self.layout_index.invalidate();
            }
        }
        if effects.reflow {
            self.bump_layout_epoch();
        }
        if effects.scroll_bottom {
            self.bottom();
        }
    }

    /// Re-lays the bodies whose reference pills resolve through canonical
    /// records that just changed.
    ///
    /// A pill's label comes from *another* message, so an append, edit,
    /// tombstone or eviction can change the rendered width of a body whose own
    /// text never moved. Only bodies flagged as carrying a reference are
    /// touched, so a room with none pays a bool scan.
    fn remeasure_reference_bodies(
        &mut self,
        history: &RoomHistoryRef<'_>,
        effects: &mut ReplayEffects,
    ) {
        if !self.entries.iter().any(|entry| entry.has_refs) {
            return;
        }
        let counts_valid = self.counts_valid();
        let width = self.layout_index.width;
        for index in 0..self.entries.len() {
            if !self.entries[index].has_refs {
                continue;
            }
            self.entries[index].invalidate_layout();
            if !counts_valid {
                continue;
            }
            let lines = self.ensure_lines(history, index, width);
            #[cfg(test)]
            {
                self.layout_index.measured_messages += 1;
            }
            self.layout_index.line_counts[index] = lines;
            // The visible pass repopulates its own bounded window; a layout
            // built only to recover a row count may be anywhere in history.
            self.entries[index].layout = None;
            effects.repack = true;
        }
        if !counts_valid {
            self.layout_index.invalidate();
        }
        effects.reflow = true;
    }

    /// Where a message threaded into canonical history lands among this view's
    /// entries: ahead of the first newer message.
    ///
    /// Notices anchored under an older message sort above the newcomer, which
    /// is exactly where leaving them put in place leaves them.
    fn message_insert_position(&self, message_id: MessageId) -> usize {
        self.entries
            .iter()
            .position(|entry| matches!(entry.id, HistoryEntryId::Message(id) if id > message_id))
            .unwrap_or(self.entries.len())
    }

    /// Materializes one canonical id. `at` is `None` for the tail, which is
    /// where appends and notices always land.
    fn insert_entry(
        &mut self,
        history: &RoomHistoryRef<'_>,
        id: HistoryEntryId,
        at: Option<usize>,
        effects: &mut ReplayEffects,
    ) {
        if self
            .clear_boundary
            .is_some_and(|boundary| boundary.hides(id))
        {
            return;
        }
        // A tombstone lands in the canonical log but has no visible record.
        let Some(has_refs) = self.body_has_references(history, id) else {
            return;
        };
        if history.notice_scrolls_bottom(id) {
            effects.scroll_bottom = true;
        }
        let entry = ViewEntry {
            id,
            expanded: false,
            has_refs,
            layout: None,
        };
        let Some(index) = at.filter(|index| *index < self.entries.len()) else {
            let append_index = self.entries.len();
            self.entries.push(entry);
            self.repair_layout_index_after_append(append_index, history);
            // Rows already on screen keep their positions, so the renderer may
            // scroll rather than rewrite: deliberately no reflow.
            return;
        };
        let counts_valid = self.counts_valid();
        self.entries.insert(index, entry);
        if counts_valid {
            let width = self.layout_index.width;
            let lines = self.ensure_lines(history, index, width);
            self.entries[index].layout = None;
            self.layout_index.line_counts.insert(index, lines);
            effects.repack = true;
        } else {
            self.layout_index.invalidate();
        }
        effects.reflow = true;
    }

    /// Re-reads one record whose body or labelling was replaced in place.
    fn replace_entry(
        &mut self,
        history: &RoomHistoryRef<'_>,
        id: HistoryEntryId,
        effects: &mut ReplayEffects,
    ) {
        let Some(index) = self.entry_index(id) else {
            return;
        };
        // An edit folding into an already-tombstoned target leaves nothing to
        // show; the entry goes with it.
        let Some(has_refs) = self.body_has_references(history, id) else {
            self.remove_entry_at(index, id, effects);
            return;
        };
        let counts_valid = self.counts_valid();
        let entry = &mut self.entries[index];
        entry.has_refs = has_refs;
        entry.invalidate_layout();
        if counts_valid {
            let width = self.layout_index.width;
            let lines = self.ensure_lines(history, index, width);
            #[cfg(test)]
            {
                self.layout_index.measured_messages += 1;
            }
            self.entries[index].layout = None;
            self.layout_index.line_counts[index] = lines;
            effects.repack = true;
        } else {
            self.layout_index.invalidate();
        }
        effects.reflow = true;
    }

    fn remove_entry(&mut self, id: HistoryEntryId, effects: &mut ReplayEffects) {
        let Some(index) = self.entry_index(id) else {
            return;
        };
        self.remove_entry_at(index, id, effects);
    }

    fn remove_entry_at(&mut self, index: usize, id: HistoryEntryId, effects: &mut ReplayEffects) {
        let counts_valid = self.counts_valid();
        self.entries.remove(index);
        if counts_valid {
            self.layout_index.line_counts.remove(index);
            effects.repack = true;
        } else {
            self.layout_index.invalidate();
        }
        // Prefer the newer survivor, matching what a rebuild would pick, so a
        // reader whose cursor sat on a deleted message keeps moving forward.
        let replacement = self
            .entries
            .get(index)
            .or_else(|| index.checked_sub(1).and_then(|prev| self.entries.get(prev)))
            .map(|entry| entry.id);
        for cursor in [&mut self.cursor, &mut self.anchor] {
            if cursor.is_some_and(|current| current.entry == id) {
                *cursor = replacement.map(|entry| ViewCursor { entry, line: 0 });
            }
        }
        if self.scroll_anchor.is_some_and(|anchor| anchor.entry == id) {
            self.scroll_anchor = None;
        }
        effects.reflow = true;
    }

    /// Applies the canonical retention watermark to this view's leading
    /// entries.
    ///
    /// Retention removes an ordered prefix and nothing else, so every survivor
    /// keeps its relative position and the row index regroups from line counts
    /// already measured — no body is laid out again. Notices anchored among the
    /// evicted messages stay: they are retained independently of the message
    /// log.
    fn drop_evicted_front(&mut self, watermark: MessageId, effects: &mut ReplayEffects) {
        let counts_valid = self.counts_valid();
        let mut kept = 0usize;
        for index in 0..self.entries.len() {
            if matches!(self.entries[index].id, HistoryEntryId::Message(id) if id <= watermark) {
                continue;
            }
            if kept != index {
                self.entries.swap(kept, index);
                if counts_valid {
                    self.layout_index.line_counts.swap(kept, index);
                }
            }
            kept += 1;
        }
        if kept == self.entries.len() {
            return;
        }
        self.entries.truncate(kept);
        let front = self.entries.first().map(|entry| entry.id);
        for cursor in [&mut self.cursor, &mut self.anchor] {
            if let Some(current) = cursor
                && !self.entries.iter().any(|entry| entry.id == current.entry)
            {
                *cursor = front.map(|entry| ViewCursor { entry, line: 0 });
            }
        }
        if let Some(anchor) = self.scroll_anchor
            && !self.entries.iter().any(|entry| entry.id == anchor.entry)
        {
            self.scroll_anchor = None;
        }
        if counts_valid {
            self.layout_index.line_counts.truncate(kept);
            effects.repack = true;
        } else {
            self.layout_index.invalidate();
        }
        effects.reflow = true;
    }

    fn resolve_record<'a>(
        &'a self,
        history: &'a RoomHistoryRef<'a>,
        id: HistoryEntryId,
    ) -> Option<ChatRecord<'a>> {
        if let HistoryEntryId::LocalNotice(id) = id {
            let notice = self.local_notices.iter().find(|notice| notice.id == id)?;
            return Some(ChatRecord {
                entry_id: HistoryEntryId::LocalNotice(id),
                sender: &notice.sender,
                body: &notice.body,
                timestamp_ms: 0,
                local: false,
                unverified: false,
                edited: false,
                file_transfer_id: None,
                notice_kind: Some(notice.kind),
            });
        }
        history.record(id)
    }

    fn build_ref_spans(
        &self,
        history: &RoomHistoryRef<'_>,
        body: &str,
        ranges: Vec<Range<u32>>,
    ) -> Vec<MsgRefSpan> {
        let mut spans = Vec::with_capacity(ranges.len());
        for range in ranges {
            let code_start = range.start as usize + rpc::msgref::REF_PREFIX.len();
            let target = rpc::msgref::MessageRef::decode(&body[code_start..range.end as usize]);
            let label = target.and_then(|target| self.resolve_label(history, target));
            spans.push(MsgRefSpan {
                range,
                target,
                label,
            });
        }
        spans
    }

    /// Resolves a reference target to its pill label when the message is in
    /// this buffer and this room.
    pub fn ref_label_for(
        &self,
        history: &RoomHistoryRef<'_>,
        target: rpc::msgref::MessageRef,
    ) -> Option<String> {
        self.resolve_label(history, target)
    }

    /// The reference and pill label of the message at `index`, for the composer
    /// reference picker. `None` for notices, which have no durable key.
    pub fn ref_for_index(
        &self,
        history: &RoomHistoryRef<'_>,
        index: usize,
    ) -> Option<(rpc::msgref::MessageRef, String)> {
        let room_id = history.room_id()?;
        let entry = self.resolve_record(history, self.entries.get(index)?.id)?;
        let HistoryEntryId::Message(message_id) = entry.entry_id else {
            return None;
        };
        let target = rpc::msgref::MessageRef {
            room_id,
            message_id,
        };
        Some((target, message_ref_label(entry.sender, entry.body)))
    }

    fn resolve_label(
        &self,
        history: &RoomHistoryRef<'_>,
        target: rpc::msgref::MessageRef,
    ) -> Option<String> {
        if history.room_id() != Some(target.room_id) {
            return None;
        }
        let entry = history.record(HistoryEntryId::Message(target.message_id))?;
        Some(message_ref_label(entry.sender, entry.body))
    }

    pub fn find_message(&self, message_id: u64) -> Option<HistoryEntryId> {
        self.entries
            .iter()
            .find(|entry| entry.id == HistoryEntryId::Message(MessageId(message_id)))
            .map(|entry| entry.id)
    }

    pub fn push_notice_with_kind(
        &mut self,
        sender: impl Into<String>,
        body: impl Into<String>,
        kind: NoticeKind,
    ) -> NoticeId {
        let id = NoticeId(self.next_notice_id);
        self.next_notice_id = self.next_notice_id.wrapping_add(1).max(1);
        let after = self.entries.iter().rev().find_map(|entry| match entry.id {
            HistoryEntryId::Message(id) => Some(id),
            HistoryEntryId::Notice(_) | HistoryEntryId::LocalNotice(_) => None,
        });
        let body = body.into();
        let has_refs = body.contains(chatt_message_format::reference::REF_PREFIX);
        self.local_notices.push(LocalNotice {
            id,
            sender: sender.into(),
            body,
            kind,
            after,
        });
        if self.local_notices.len() > self.max_local_notices {
            let removed = self.local_notices.remove(0).id;
            self.entries
                .retain(|entry| entry.id != HistoryEntryId::LocalNotice(removed));
        }
        self.entries.push(ViewEntry {
            id: HistoryEntryId::LocalNotice(id),
            expanded: false,
            has_refs,
            layout: None,
        });
        self.layout_index.invalidate();
        self.bump_layout_epoch();
        id
    }

    pub fn clear_scrollback(&mut self) {
        let mut boundary = ClearBoundary::default();
        for entry in &self.entries {
            boundary.observe(entry.id);
        }
        self.clear_generation = self.clear_generation.wrapping_add(1);
        self.clear_boundary = (!self.entries.is_empty()).then_some(boundary);
        self.local_notices.clear();
        self.entries.clear();
        self.scroll_offset = 0;
        self.scroll_anchor = None;
        self.cursor = None;
        self.anchor = None;
        self.dragging = false;
        self.layout_index.clear();
        self.bump_layout_epoch();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn record<'a>(
        &'a self,
        history: &'a RoomHistoryRef<'a>,
        index: usize,
    ) -> Option<ChatRecord<'a>> {
        let id = self.entries.get(index)?.id;
        self.resolve_record(history, id)
    }

    pub fn record_entry<'a>(
        &'a self,
        history: &'a RoomHistoryRef<'a>,
        id: HistoryEntryId,
    ) -> Option<ChatRecord<'a>> {
        self.resolve_record(history, id)
    }

    pub(crate) fn entry_index(&self, id: HistoryEntryId) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }

    /// The ids this view actually displays, in order. Unlike the canonical id
    /// list this already excludes whatever a `/clear` hid, so derived overlays
    /// built from it cannot offer the user rows the view will not show.
    pub(crate) fn entry_ids(&self) -> impl ExactSizeIterator<Item = HistoryEntryId> + '_ {
        self.entries.iter().map(|entry| entry.id)
    }

    /// Advances on every `/clear`, so an overlay can tell that the displayed id
    /// set shrank even when canonical history did not move.
    pub(crate) fn clear_generation(&self) -> u64 {
        self.clear_generation
    }

    pub fn toggle_expand_entry(
        &mut self,
        history: &RoomHistoryRef<'_>,
        id: HistoryEntryId,
        width: u16,
    ) -> bool {
        let Some(index) = self.entry_index(id) else {
            return false;
        };
        self.toggle_expand(history, index, width)
    }

    pub fn scroll_entry_into_view(
        &mut self,
        history: &RoomHistoryRef<'_>,
        id: HistoryEntryId,
        width: u16,
        height: u16,
    ) {
        if let Some(index) = self.entry_index(id) {
            self.scroll_message_into_view(history, index, width, height);
        }
    }

    #[cfg(test)]
    pub(crate) fn local_record(&self, index: usize) -> Option<ChatRecord<'_>> {
        let id = self.entries.get(index)?.id;
        let HistoryEntryId::LocalNotice(id) = id else {
            return None;
        };
        let notice = self.local_notices.iter().find(|notice| notice.id == id)?;
        Some(ChatRecord {
            entry_id: HistoryEntryId::LocalNotice(id),
            sender: &notice.sender,
            body: &notice.body,
            timestamp_ms: 0,
            local: false,
            unverified: false,
            edited: false,
            file_transfer_id: None,
            notice_kind: Some(notice.kind),
        })
    }

    pub(crate) fn layout_epoch(&self) -> u64 {
        self.layout_epoch
    }

    fn bump_layout_epoch(&mut self) {
        self.layout_epoch = self.layout_epoch.wrapping_add(1);
    }

    pub fn line(&self, message: usize, line: usize) -> &[Segment] {
        self.entries[message].layout().line(line)
    }

    /// Returns the URL at `col_in_line` on wrapped `line` of `message`, when a
    /// link segment covers that column. `col_in_line` is measured from the start
    /// of the message content, the same origin as [`Segment::col`].
    pub fn link_at<'a>(
        &'a self,
        history: &'a RoomHistoryRef<'a>,
        message: usize,
        line: usize,
        col_in_line: u16,
    ) -> Option<&'a str> {
        let view = self.entries.get(message)?;
        let entry = self.resolve_record(history, self.entries.get(message)?.id)?;
        let links = chatt_message_format::inline_ranges(entry.body).urls;
        if links.is_empty() {
            return None;
        }
        let seg = view.layout().segment_at(entry.body, line, col_in_line)?;
        if seg.synth {
            return None;
        }
        let range = links
            .iter()
            .find(|r| r.start < seg.end && seg.start < r.end)?;
        Some(&entry.body[range.start as usize..range.end as usize])
    }

    /// Returns the decoded message reference at `col_in_line` on wrapped `line`
    /// of `message`, whether rendered as a pill or as a literal code.
    pub fn ref_at(
        &self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        line: usize,
        col_in_line: u16,
    ) -> Option<rpc::msgref::MessageRef> {
        let view = self.entries.get(message)?;
        let entry = self.resolve_record(history, self.entries.get(message)?.id)?;
        let inline = chatt_message_format::inline_ranges(entry.body);
        if inline.refs.is_empty() {
            return None;
        }
        let refs = self.build_ref_spans(history, entry.body, inline.refs);
        let seg = view.layout().segment_at(entry.body, line, col_in_line)?;
        if seg.synth {
            let index = view.layout().pill_ref_at(seg)?;
            return refs.get(index)?.target;
        }
        let span = refs
            .iter()
            .find(|span| span.range.start < seg.end && seg.start < span.range.end)?;
        span.target
    }

    /// Returns the text a segment displays: a body slice, or a slice of the
    /// layout's synthetic pill text.
    pub fn segment_text<'a>(
        &'a self,
        history: &'a RoomHistoryRef<'a>,
        message: usize,
        seg: &Segment,
    ) -> Option<&'a str> {
        let view = self.entries.get(message)?;
        let entry = self.resolve_record(history, self.entries.get(message)?.id)?;
        Some(view.layout().segment_str(entry.body, seg))
    }

    #[cfg(test)]
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub fn scroll_up(
        &mut self,
        history: &RoomHistoryRef<'_>,
        rows: usize,
        width: u16,
        height: u16,
    ) {
        let max = self.max_scroll(history, width, height);
        self.scroll_offset = self.scroll_offset.saturating_add(rows.max(1)).min(max);
        self.scroll_anchor = None;
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows.max(1));
        self.scroll_anchor = None;
    }

    pub fn bottom(&mut self) {
        self.scroll_offset = 0;
        self.scroll_anchor = None;
    }

    pub fn is_at_top(&mut self, history: &RoomHistoryRef<'_>, width: u16, height: u16) -> bool {
        self.scroll_offset == self.max_scroll(history, width, height)
    }

    pub fn top(&mut self, history: &RoomHistoryRef<'_>, width: u16, height: u16) {
        self.scroll_offset = self.max_scroll(history, width, height);
        self.scroll_anchor = None;
    }

    /// Largest valid `scroll_offset`: the offset that places the oldest line at
    /// the top of the view. Zero when all content fits within `height`.
    fn max_scroll(&mut self, history: &RoomHistoryRef<'_>, width: u16, height: u16) -> usize {
        self.total_lines_exact(history, width)
            .saturating_sub(height as usize)
    }

    fn layout_cursor(&self, cursor: ViewCursor) -> Option<LayoutCursor> {
        Some(LayoutCursor {
            message: self
                .entries
                .iter()
                .position(|entry| entry.id == cursor.entry)?,
            line: cursor.line,
        })
    }

    /// Places the cursor and anchor at `pos` and starts a mouse drag.
    pub fn begin_drag(&mut self, pos: ViewCursor) {
        self.cursor = self
            .entries
            .iter()
            .any(|entry| entry.id == pos.entry)
            .then_some(pos);
        self.anchor = self.cursor;
        self.dragging = true;
    }

    /// Moves the cursor of an in-progress drag to `pos`; the anchor stays.
    pub fn drag_to(&mut self, pos: ViewCursor) {
        if self.dragging && self.entries.iter().any(|entry| entry.id == pos.entry) {
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
    pub fn cursor(&self) -> Option<ViewCursor> {
        self.cursor
    }

    /// Returns a valid cursor, defaulting to the last body line of the newest
    /// message and clamping stale coordinates. `None` only when empty.
    pub fn ensure_cursor(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Option<ViewCursor> {
        self.ensure_layout_cursor(history, width)?;
        self.cursor
    }

    fn ensure_layout_cursor(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Option<LayoutCursor> {
        if self.entries.is_empty() {
            self.cursor = None;
            self.anchor = None;
            return None;
        }
        if self.cursor.is_none() {
            self.cursor = Some(ViewCursor {
                entry: self.entries.last()?.id,
                line: usize::MAX,
            });
        }
        self.clamp_positions(history, width);
        self.layout_cursor(self.cursor?)
    }

    /// Moves the cursor to a stable history entry, clearing any visual anchor.
    pub fn set_cursor(&mut self, entry: HistoryEntryId) -> Option<ViewCursor> {
        if !self.entries.iter().any(|candidate| candidate.id == entry) {
            return None;
        }
        self.cursor = Some(ViewCursor { entry, line: 0 });
        self.anchor = None;
        self.cursor()
    }

    /// Moves the cursor `delta` visible body lines, walking across messages,
    /// clamping at the buffer edges. Lines hidden by collapse are never
    /// visited.
    pub fn move_cursor_line(
        &mut self,
        history: &RoomHistoryRef<'_>,
        delta: isize,
        width: u16,
    ) -> Option<ViewCursor> {
        let mut cursor = self.ensure_layout_cursor(history, width)?;
        for _ in 0..delta.unsigned_abs() {
            let next = if delta > 0 {
                self.next_body_pos(history, cursor, width)
            } else {
                self.prev_body_pos(history, cursor, width)
            };
            let Some(next) = next else { break };
            cursor = next;
        }
        self.cursor = Some(ViewCursor {
            entry: self.entries[cursor.message].id,
            line: cursor.line,
        });
        self.cursor()
    }

    /// Moves the cursor to the first body line of the block `delta` blocks
    /// away (a sender-group heading boundary), clamping at the ends.
    pub fn move_cursor_paragraph(
        &mut self,
        history: &RoomHistoryRef<'_>,
        delta: isize,
        width: u16,
    ) -> Option<ViewCursor> {
        let cursor = self.ensure_layout_cursor(history, width)?;
        self.ensure_layout_index(history, width);
        let current = self.layout_index.block_containing_message(cursor.message)?;
        let block_count = self.layout_index.blocks.len();
        let next = (current as isize + delta).clamp(0, block_count as isize - 1) as usize;
        let message = self.layout_index.blocks[next].first;
        self.cursor = Some(ViewCursor {
            entry: self.entries[message].id,
            line: 0,
        });
        self.cursor()
    }

    pub fn cursor_to_first(&mut self) -> Option<ViewCursor> {
        if self.entries.is_empty() {
            return None;
        }
        self.cursor = Some(ViewCursor {
            entry: self.entries[0].id,
            line: 0,
        });
        self.cursor()
    }

    pub fn cursor_to_last(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Option<ViewCursor> {
        if self.entries.is_empty() {
            return None;
        }
        let last = self.entries.len() - 1;
        self.cursor = Some(ViewCursor {
            entry: self.entries[last].id,
            line: self.visible_body_lines(history, last, width) - 1,
        });
        self.cursor()
    }

    /// Toggles visual-line mode: anchors a selection at the cursor, or clears
    /// an existing one. Returns whether a selection is now active.
    pub fn toggle_visual_anchor(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> bool {
        if self.anchor.take().is_some() {
            return false;
        }
        self.ensure_cursor(history, width);
        self.anchor = self.cursor;
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
        self.entries.get(message).is_some_and(|entry| {
            self.cursor
                == Some(ViewCursor {
                    entry: entry.id,
                    line,
                })
        })
    }

    /// Returns whether the given `(message, line)` falls within the visual
    /// selection.
    pub fn is_visual(&self, message: usize, line: usize) -> bool {
        let Some((lo, hi)) = self.visual_bounds() else {
            return false;
        };
        let pos = LayoutCursor { message, line };
        lo <= pos && pos <= hi
    }

    /// The visual range's ordered `(oldest, newest)` endpoints, when active.
    fn visual_bounds(&self) -> Option<(LayoutCursor, LayoutCursor)> {
        let anchor = self.layout_cursor(self.anchor?)?;
        let cursor = self.layout_cursor(self.cursor?)?;
        if anchor <= cursor {
            Some((anchor, cursor))
        } else {
            Some((cursor, anchor))
        }
    }

    /// Stable entries targeted by an action on the current chat selection.
    /// A plain cursor targets its whole message; a visual-line range targets
    /// each message touched by either endpoint or any line between them.
    pub fn selected_entries(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Vec<HistoryEntryId> {
        let Some(cursor) = self.ensure_layout_cursor(history, width) else {
            return Vec::new();
        };
        let Some((lo, hi)) = self.visual_bounds() else {
            return vec![self.entries[cursor.message].id];
        };
        self.entries[lo.message..=hi.message]
            .iter()
            .map(|entry| entry.id)
            .collect()
    }

    /// Copies original body text covered by the visually selected rendered
    /// rows, content only (no sender column). Wrapped rows from the same
    /// message are sliced as one source range so clipboard text keeps the
    /// message's whitespace instead of the display wrapper's trimmed fragments.
    pub fn visual_text(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> Option<String> {
        let width = width.max(1);
        let (lo, hi) = self.visual_bounds()?;
        let mut out = String::new();
        let mut first = true;
        for message in lo.message..=hi.message.min(self.entries.len().saturating_sub(1)) {
            let lines = self.visible_body_lines(history, message, width);
            self.ensure_lines(history, message, width);
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
            let view = &self.entries[message];
            let entry = self.resolve_record(history, self.entries.get(message)?.id)?;
            if start == 0 && end == lines - 1 {
                if lines == view.layout().lines().max(1) {
                    out.push_str(entry.body);
                } else {
                    let range = view.layout().source_range(start, end, entry.body.len());
                    out.push_str(&entry.body[range]);
                }
            } else {
                let range = view.layout().source_range(start, end, entry.body.len());
                out.push_str(&entry.body[range]);
            }
        }
        Some(out)
    }

    /// The original body text the cursor's wrapped row displays.
    pub fn cursor_line_text(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> Option<String> {
        let cursor = self.layout_cursor(self.cursor?)?;
        self.ensure_lines(history, cursor.message, width);
        let view = self.entries.get(cursor.message)?;
        let entry = self.resolve_record(history, self.entries.get(cursor.message)?.id)?;
        let lines = view.layout().lines().max(1);
        if cursor.line >= lines {
            return None;
        }
        if lines == 1 {
            return Some(entry.body.to_string());
        }
        let range = view
            .layout()
            .source_range(cursor.line, cursor.line, entry.body.len());
        Some(entry.body[range].to_string())
    }

    pub fn cursor_message_body<'a>(&'a self, history: &'a RoomHistoryRef<'a>) -> Option<&'a str> {
        let cursor = self.layout_cursor(self.cursor?)?;
        Some(
            self.resolve_record(history, self.entries.get(cursor.message)?.id)?
                .body,
        )
    }

    /// The first decodable reference contained in the cursor's message, for
    /// keyboard-driven "open the reference in this message".
    pub fn cursor_ref(&self, history: &RoomHistoryRef<'_>) -> Option<rpc::msgref::MessageRef> {
        let cursor = self.layout_cursor(self.cursor?)?;
        let entry = self.resolve_record(history, self.entries.get(cursor.message)?.id)?;
        let inline = chatt_message_format::inline_ranges(entry.body);
        for span in self.build_ref_spans(history, entry.body, inline.refs) {
            if let Some(target) = span.target {
                return Some(target);
            }
        }
        None
    }

    /// Scrolls the minimum amount that brings the cursor's row into view.
    pub fn keep_cursor_visible(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
        height: u16,
    ) -> Option<()> {
        let height = height as usize;
        if height == 0 {
            return None;
        }
        let cursor = self.ensure_layout_cursor(history, width)?;
        let (row, total) = self.pos_row_and_total(history, cursor, width)?;
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
    pub fn on_reflow(&mut self, history: &RoomHistoryRef<'_>, width: u16) {
        self.anchor = None;
        self.dragging = false;
        self.layout_index.invalidate();
        self.bump_layout_epoch();
        self.clamp_positions(history, width);
    }

    /// Clamps the cursor and anchor into the collapse-aware visible line range
    /// at `width`.
    fn clamp_positions(&mut self, history: &RoomHistoryRef<'_>, width: u16) {
        if self.entries.is_empty() {
            self.cursor = None;
            self.anchor = None;
            return;
        }
        if let Some(cursor) = self.cursor.and_then(|cursor| self.layout_cursor(cursor)) {
            let message = cursor.message;
            let line = cursor
                .line
                .min(self.visible_body_lines(history, message, width) - 1);
            self.cursor = Some(ViewCursor {
                entry: self.entries[message].id,
                line,
            });
        }
        if let Some(anchor) = self.anchor.and_then(|anchor| self.layout_cursor(anchor)) {
            let message = anchor.message;
            let line = anchor
                .line
                .min(self.visible_body_lines(history, message, width) - 1);
            self.anchor = Some(ViewCursor {
                entry: self.entries[message].id,
                line,
            });
        }
    }

    pub fn clamp_scroll(&mut self, history: &RoomHistoryRef<'_>, width: u16, height: u16) {
        let max = self.max_scroll(history, width, height);
        self.scroll_offset = self.scroll_offset.min(max);
    }

    pub fn scroll_message_into_view(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        width: u16,
        height: u16,
    ) -> Option<()> {
        let height = height as usize;
        if height == 0 {
            return None;
        }
        let (message_row, total_rows) = self.message_row_and_total(history, message, width)?;
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
    pub fn toggle_expand(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        width: u16,
    ) -> bool {
        if message >= self.entries.len()
            || self.ensure_lines(history, message, width) <= COLLAPSE_LIMIT
        {
            return false;
        }
        self.entries[message].expanded = !self.entries[message].expanded;
        if self.layout_index.valid
            && self.layout_index.width == width.max(1)
            && self.layout_index.line_counts.len() == self.entries.len()
        {
            if let Some(block_index) = self.layout_index.block_containing_message(message) {
                let rows = {
                    let block = &mut self.layout_index.blocks[block_index];
                    block.body_lines = if self.entries[message].expanded {
                        self.layout_index.line_counts[message]
                    } else {
                        COLLAPSE_SHOW
                    };
                    block.collapsed = !self.entries[message].expanded;
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
        self.clamp_positions(history, width);
        self.bump_layout_epoch();
        true
    }

    /// Whether `message` is collapsible (over [`COLLAPSE_LIMIT`] lines) and
    /// currently collapsed. Assumes its layout was already laid out this frame
    /// (true for any message in a visible block).
    pub fn is_collapsed(&self, message: usize) -> bool {
        let entry = &self.entries[message];
        entry.layout().lines() > COLLAPSE_LIMIT && !entry.expanded
    }

    /// Whether `message` is collapsible and currently expanded. Counterpart to
    /// [`Self::is_collapsed`]; both are false for short messages.
    pub fn is_expanded(&self, message: usize) -> bool {
        let entry = &self.entries[message];
        entry.layout().lines() > COLLAPSE_LIMIT && entry.expanded
    }

    /// Returns the absolute row index of the viewport's top line at the given
    /// dimensions, applying the same scroll clamp as
    /// [`Self::visible_lines_into`] so a subsequent call computes the
    /// identical window.
    pub fn viewport_top(&mut self, history: &RoomHistoryRef<'_>, width: u16, height: u16) -> usize {
        let target = height as usize;
        if self.entries.is_empty() || target == 0 {
            return 0;
        }
        self.ensure_layout_index(history, width.max(1));
        let total = self.layout_index.total_rows();
        self.scroll_offset = self.scroll_offset.min(total.saturating_sub(target));
        total.saturating_sub(self.scroll_offset.saturating_add(target))
    }

    pub fn visible_lines_into(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
        height: u16,
        overscan: usize,
        out: &mut Vec<VisibleLine>,
    ) {
        let width = width.max(1);
        let target = height as usize;
        out.clear();
        if self.entries.is_empty() || target == 0 {
            return;
        }
        self.ensure_layout_index(history, width);
        let total = self.layout_index.total_rows();
        if self.scroll_offset > 0
            && let Some(anchor) = self.scroll_anchor.take()
            && let Some(message) = self
                .entries
                .iter()
                .position(|entry| entry.id == anchor.entry)
            && let Some(top) = self.visible_line_row(message, anchor.line, anchor.kind)
        {
            self.scroll_offset = total
                .saturating_sub(top.saturating_add(target))
                .min(total.saturating_sub(target));
        }
        let max_scroll = total.saturating_sub(target);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let top = total.saturating_sub(self.scroll_offset.saturating_add(target));
        let bottom = top.saturating_add(target).min(total);
        let cache_top = top.saturating_sub(overscan);
        let cache_bottom = bottom.saturating_add(overscan).min(total);
        let cache_range = self.message_range_for_rows(cache_top, cache_bottom);
        if let Some((first, last)) = cache_range {
            for message in first..=last {
                self.ensure_lines(history, message, width);
            }
        }
        for (message, entry) in self.entries.iter_mut().enumerate() {
            if !cache_range.is_some_and(|(first, last)| first <= message && message <= last) {
                entry.layout = None;
            }
        }
        out.reserve(bottom.saturating_sub(top));
        for row in top..bottom {
            if let Some(line) = self.cached_visible_line(row) {
                out.push(line);
            }
        }
        self.scroll_anchor = (self.scroll_offset > 0)
            .then(|| out.first().copied())
            .flatten()
            .and_then(|line| {
                Some(ViewScrollAnchor {
                    entry: self.entries.get(line.message)?.id,
                    line: line.line,
                    kind: line.kind,
                })
            });
    }

    fn message_range_for_rows(&self, top: usize, bottom: usize) -> Option<(usize, usize)> {
        if top >= bottom {
            return None;
        }
        let first = self.cached_visible_line(top)?;
        let last = self.cached_visible_line(bottom - 1)?;
        Some((first.block_first, last.block_last))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn visible_lines(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
        height: u16,
        overscan: usize,
    ) -> Vec<VisibleLine> {
        let mut lines = Vec::new();
        self.visible_lines_into(history, width, height, overscan, &mut lines);
        lines
    }

    /// Lays out `idx` at `width` and returns its wrapped line count (at least 1).
    fn ensure_lines(&mut self, history: &RoomHistoryRef<'_>, idx: usize, width: u16) -> usize {
        let width = width.max(1);
        if self.layout_index.valid && self.layout_index.width != width {
            self.layout_index.invalidate();
        }
        let syntax = self.syntax;
        let id = self.entries[idx].id;
        if let HistoryEntryId::LocalNotice(id) = id {
            let body = self
                .local_notices
                .iter()
                .find(|notice| notice.id == id)
                .expect("local notice id must resolve")
                .body
                .clone();
            let inline = chatt_message_format::inline_ranges(&body);
            let refs = self.build_ref_spans(history, &body, inline.refs);
            let msg = &mut self.entries[idx];
            let layout = msg.layout_mut();
            layout.ensure(width, &body, &refs, syntax);
            return layout.lines().max(1);
        }
        // A view id can outlive its record: the core may tombstone or evict
        // between one reconcile and the next input event. Lay the row out as a
        // single blank line and let the following reconcile drop it.
        let Some(record) = history.record(id) else {
            let msg = &mut self.entries[idx];
            let layout = msg.layout_mut();
            layout.ensure(width, "", &[], syntax);
            return layout.lines().max(1);
        };
        let inline = chatt_message_format::inline_ranges(record.body);
        let refs = self.build_ref_spans(history, record.body, inline.refs);
        let msg = &mut self.entries[idx];
        let layout = msg.layout_mut();
        layout.ensure(width, record.body, &refs, syntax);
        layout.lines().max(1)
    }

    fn ensure_layout_index(&mut self, history: &RoomHistoryRef<'_>, width: u16) {
        let width = width.max(1);
        if self.layout_index.valid
            && self.layout_index.width == width
            && self.layout_index.line_counts.len() == self.entries.len()
        {
            return;
        }
        self.rebuild_layout_index(history, width);
    }

    fn rebuild_layout_index(&mut self, history: &RoomHistoryRef<'_>, width: u16) {
        let width = width.max(1);
        self.layout_index.valid = false;
        self.layout_index.width = width;

        let mut line_counts = std::mem::take(&mut self.layout_index.line_counts);
        line_counts.clear();
        line_counts.reserve(self.entries.len());
        for idx in 0..self.entries.len() {
            let lines = self.ensure_lines(history, idx, width);
            #[cfg(test)]
            {
                self.layout_index.measured_messages += 1;
            }
            line_counts.push(lines);
            // A full row-index rebuild measures every record, but those
            // layouts are not the cache. Release each immediately; the
            // visible/overscan pass below repopulates only its bounded window.
            self.entries[idx].layout = None;
        }
        self.install_layout_index(history, line_counts);
    }

    fn rebuild_layout_index_from_counts(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
        preserved: Vec<Option<usize>>,
    ) {
        debug_assert_eq!(preserved.len(), self.entries.len());
        self.layout_index.valid = false;
        self.layout_index.width = width;

        let mut line_counts = std::mem::take(&mut self.layout_index.line_counts);
        line_counts.clear();
        line_counts.reserve(self.entries.len());
        for (idx, preserved) in preserved.into_iter().enumerate() {
            if let Some(lines) = preserved {
                line_counts.push(lines);
                continue;
            }
            let lines = self.ensure_lines(history, idx, width);
            #[cfg(test)]
            {
                self.layout_index.measured_messages += 1;
            }
            line_counts.push(lines);
            // Preserve only the already-bounded visible cache. Layouts built
            // solely to recover a changed row count may be anywhere in the
            // retained history and are released immediately.
            self.entries[idx].layout = None;
        }
        self.install_layout_index(history, line_counts);
    }

    fn install_layout_index(&mut self, history: &RoomHistoryRef<'_>, line_counts: Vec<usize>) {
        let mut blocks = std::mem::take(&mut self.layout_index.blocks);
        blocks.clear();
        let mut cursor = 0usize;
        while cursor < self.entries.len() {
            let run_end = self.run_end_cached(history, cursor, &line_counts);
            self.pack_run_cached(history, cursor, run_end, &line_counts, &mut blocks);
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

    fn repair_layout_index_after_append(&mut self, old_len: usize, history: &RoomHistoryRef<'_>) {
        if !self.layout_index.valid {
            return;
        }
        if old_len + 1 != self.entries.len()
            || self.layout_index.line_counts.len() != old_len
            || self.layout_index.rows.len() != self.layout_index.blocks.len()
        {
            self.layout_index.invalidate();
            return;
        }
        let width = self.layout_index.width;
        let lines = self.ensure_lines(history, old_len, width);
        self.layout_index.line_counts.push(lines);
        let repair_start = if old_len == 0 {
            0
        } else if self.boundary_before_cached(
            history,
            old_len - 1,
            old_len,
            &self.layout_index.line_counts,
        ) {
            old_len
        } else if let Some(block_index) = self.layout_index.block_containing_message(old_len - 1) {
            self.layout_index.blocks[block_index].first
        } else {
            self.layout_index.invalidate();
            return;
        };
        self.rebuild_layout_index_tail_from(history, repair_start);
    }

    fn rebuild_layout_index_tail_from(
        &mut self,
        history: &RoomHistoryRef<'_>,
        repair_start: usize,
    ) {
        if !self.layout_index.valid
            || self.layout_index.line_counts.len() != self.entries.len()
            || repair_start > self.entries.len()
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
        while cursor < self.entries.len() {
            let run_end = self.run_end_cached(history, cursor, &self.layout_index.line_counts);
            self.pack_run_cached(
                history,
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

    fn run_end_cached(
        &self,
        history: &RoomHistoryRef<'_>,
        start: usize,
        line_counts: &[usize],
    ) -> usize {
        if line_counts[start] > COLLAPSE_LIMIT {
            return start;
        }
        let mut end = start;
        while end + 1 < self.entries.len()
            && !self.boundary_before_cached(history, end, end + 1, line_counts)
        {
            end += 1;
        }
        end
    }

    fn boundary_before_cached(
        &self,
        history: &RoomHistoryRef<'_>,
        prev: usize,
        cur: usize,
        line_counts: &[usize],
    ) -> bool {
        // An id whose record has already gone stands alone rather than joining
        // a group: grouping it would style the survivors against a body this
        // view can no longer read.
        let (Some(prev_record), Some(cur_record)) = (
            self.resolve_record(history, self.entries[prev].id),
            self.resolve_record(history, self.entries[cur].id),
        ) else {
            return true;
        };
        if prev_record.timestamp_ms == 0 || cur_record.timestamp_ms == 0 {
            return true;
        }
        if prev_record.local != cur_record.local
            || prev_record.sender != cur_record.sender
            || prev_record.unverified != cur_record.unverified
        {
            return true;
        }
        // An edited message anchors its own heading, where the `(edited)`
        // marker renders; grouped mid-block it would be invisible.
        if prev_record.edited != cur_record.edited {
            return true;
        }
        let gap = cur_record
            .timestamp_ms
            .saturating_sub(prev_record.timestamp_ms);
        if gap > GROUP_GAP_MS {
            return true;
        }
        line_counts[prev] > COLLAPSE_LIMIT || line_counts[cur] > COLLAPSE_LIMIT
    }

    fn pack_run_cached(
        &self,
        _history: &RoomHistoryRef<'_>,
        run_start: usize,
        run_end: usize,
        line_counts: &[usize],
        blocks: &mut Vec<Block>,
    ) {
        let first_lines = line_counts[run_start];
        if run_start == run_end && first_lines > COLLAPSE_LIMIT {
            let expanded = self.entries[run_start].expanded;
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
                entry: self.entries[block.first].id,
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
                    entry: self.entries[block.last].id,
                    message: block.last,
                    block_first: block.first,
                    block_last: block.last,
                    line: body_row,
                    kind: LineKind::Body,
                });
            }
            return Some(VisibleLine {
                entry: self.entries[block.last].id,
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
                    entry: self.entries[message].id,
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

    fn visible_line_row(&self, message: usize, line: usize, kind: LineKind) -> Option<usize> {
        let block_index = self.layout_index.block_containing_message(message)?;
        let block = self.layout_index.blocks.get(block_index)?;
        let block_row = self.layout_index.rows.prefix_sum(block_index);
        match kind {
            LineKind::Heading => (message == block.first).then_some(block_row),
            LineKind::Ellipsis => (block.collapsed && message == block.last)
                .then_some(block_row + 1 + block.body_lines),
            LineKind::Body => {
                let preceding = if block.collapsed {
                    0
                } else {
                    self.layout_index.line_counts[block.first..message]
                        .iter()
                        .copied()
                        .sum()
                };
                Some(block_row + 1 + preceding + line.min(block.body_lines.saturating_sub(1)))
            }
        }
    }

    /// Total rendered rows for a block: heading + body + optional ellipsis.
    fn block_rows(block: &Block) -> usize {
        1 + block.body_lines + usize::from(block.collapsed)
    }

    fn total_lines_exact(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> usize {
        self.ensure_layout_index(history, width);
        self.layout_index.total_rows()
    }

    /// Visible body lines of `message` at `width`: the full wrapped count, or
    /// [`COLLAPSE_SHOW`] when the message is collapsed.
    fn visible_body_lines(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        width: u16,
    ) -> usize {
        let width = width.max(1);
        let lines = if self.layout_index.valid
            && self.layout_index.width == width
            && self.layout_index.line_counts.len() == self.entries.len()
        {
            self.layout_index.line_counts[message]
        } else {
            self.ensure_lines(history, message, width)
        };
        Self::visible_body_lines_for(&self.entries[message], lines)
    }

    fn visible_body_lines_for(entry: &ViewEntry, lines: usize) -> usize {
        if lines > COLLAPSE_LIMIT && !entry.expanded {
            COLLAPSE_SHOW
        } else {
            lines
        }
    }

    fn next_body_pos(
        &mut self,
        history: &RoomHistoryRef<'_>,
        pos: LayoutCursor,
        width: u16,
    ) -> Option<LayoutCursor> {
        if pos.line + 1 < self.visible_body_lines(history, pos.message, width) {
            return Some(LayoutCursor {
                message: pos.message,
                line: pos.line + 1,
            });
        }
        if pos.message + 1 < self.entries.len() {
            return Some(LayoutCursor {
                message: pos.message + 1,
                line: 0,
            });
        }
        None
    }

    fn prev_body_pos(
        &mut self,
        history: &RoomHistoryRef<'_>,
        pos: LayoutCursor,
        width: u16,
    ) -> Option<LayoutCursor> {
        if pos.line > 0 {
            return Some(LayoutCursor {
                message: pos.message,
                line: pos.line - 1,
            });
        }
        let message = pos.message.checked_sub(1)?;
        Some(LayoutCursor {
            message,
            line: self.visible_body_lines(history, message, width) - 1,
        })
    }

    /// The rendered row of `pos` from the top of the full layout plus the
    /// total row count, counting heading and ellipsis rows.
    fn pos_row_and_total(
        &mut self,
        history: &RoomHistoryRef<'_>,
        pos: LayoutCursor,
        width: u16,
    ) -> Option<(usize, usize)> {
        if pos.message >= self.entries.len() {
            return None;
        }
        self.ensure_layout_index(history, width);
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
            &self.entries[pos.message],
            self.layout_index.line_counts[pos.message],
        );
        row = row.saturating_add(pos.line.min(visible_lines.saturating_sub(1)));
        Some((row, self.layout_index.total_rows()))
    }

    fn message_row_and_total(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        width: u16,
    ) -> Option<(usize, usize)> {
        if message >= self.entries.len() {
            return None;
        }
        self.ensure_layout_index(history, width);
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
    /// Synthetic ranges of rendered pills paired with their decoded-reference
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
    /// decoded-reference index. Never contributes to clipboard mapping; the
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
                TokenKind::Text => append_piece(
                    text,
                    display,
                    pieces,
                    token_range(token),
                    self.inline_style(base_style, bold, italic, false),
                ),
                TokenKind::Url => append_piece(
                    text,
                    display,
                    pieces,
                    token_range(token),
                    self.inline_style(base_style, bold, italic, false)
                        .patch(self.syntax.namespace)
                        | Modifier::UNDERLINED,
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
            style |= Modifier::BOLD;
        }
        if italic {
            style |= Modifier::ITALIC;
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
pub(crate) fn message_ref_label(sender: &str, body: &str) -> String {
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
