use std::ops::Range;

use extui::{Style, vt::Modifier};
use rpc::ids::{FileTransferId, MessageId, RoomId};
use unicode_segmentation::UnicodeSegmentation;
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

impl HistoryEntryId {
    /// Whether this entry is a system notice rather than a user message.
    ///
    /// Notices start expanded: their bodies are diagnostics (`/audio`, device
    /// events, connection errors) whose useful part is usually past the
    /// [`COLLAPSE_SHOW`] cut, and a reader who does not know to expand them
    /// reports the truncation instead of the contents.
    fn is_notice(self) -> bool {
        matches!(self, Self::Notice(_) | Self::LocalNotice(_))
    }
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
        viewport.toggle_visual(&history.history(), VisualKind::Line, 6);
        assert_eq!(
            viewport.visual_text(&history.history(), 6).as_deref(),
            Some("alpha beta gamma")
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
    fn notices_start_expanded_and_messages_start_collapsed() {
        let mut history = TestHistory::new();
        let body = "line\n".repeat(14);
        history.push(1, "alice", &body);
        history.notice(&body, false);
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 40, 0);
        assert!(viewport.is_collapsed(0), "a long message still collapses");
        assert!(viewport.is_expanded(1), "a long notice opens on arrival");
    }

    #[test]
    fn an_expanded_notice_still_toggles_closed() {
        let mut history = TestHistory::new();
        history.notice(&"line\n".repeat(14), false);
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 40, 0);
        assert!(viewport.toggle_expand(&history.history(), 0, 40));
        viewport.visible_lines(&history.history(), 40, 40, 0);
        assert!(viewport.is_collapsed(0));
    }

    #[test]
    fn explicit_fold_state_is_idempotent_and_expand_all_only_affects_current_entries() {
        let mut history = TestHistory::new();
        let long = (0..20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        history.push(1, "alice", &long);
        history.push(2, "bob", "short");
        history.notice(&long, false);
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 1);

        let first = HistoryEntryId::Message(MessageId(1));
        assert_eq!(
            viewport.set_expand_entry(&history.history(), first, 40, true),
            Some(true)
        );
        assert_eq!(
            viewport.set_expand_entry(&history.history(), first, 40, true),
            Some(false)
        );
        assert_eq!(
            viewport.set_expand_entry(&history.history(), first, 40, false),
            Some(true)
        );
        assert_eq!(
            viewport.set_expand_entry(
                &history.history(),
                HistoryEntryId::Message(MessageId(2)),
                40,
                true,
            ),
            None
        );

        assert!(viewport.expand_all());
        viewport.visible_lines(&history.history(), 40, 8, 1);
        let first_index = viewport.entry_index(first).unwrap();
        let notice_index = viewport
            .entry_ids()
            .position(|entry| matches!(entry, HistoryEntryId::Notice(_)))
            .unwrap();
        assert!(viewport.entries[first_index].expanded);
        assert!(viewport.entries[notice_index].expanded);
        assert!(!viewport.expand_all());

        history.push(3, "carol", &long);
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 8, 1);
        let third = viewport
            .entry_index(HistoryEntryId::Message(MessageId(3)))
            .unwrap();
        assert!(
            !viewport.entries[third].expanded,
            "new messages keep the fold default"
        );
    }

    #[test]
    fn large_expanded_message_keeps_cursor_buffered_and_high_buffer_centers_it() {
        let mut history = TestHistory::new();
        let body = (0..60)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        history.push(1, "alice", &body);
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.ensure_cursor(&history.history(), 40);
        assert!(viewport.toggle_expand(&history.history(), 0, 40));
        viewport.move_cursor(&history.history(), ChatMotion::Down(20), 40);

        viewport
            .keep_cursor_visible(&history.history(), 40, 9, 3)
            .unwrap();
        let visible = viewport.visible_lines(&history.history(), 40, 9, 1);
        let cursor = viewport.cursor().unwrap();
        let row = viewport.cursor_row(&history.history(), 40).unwrap();
        let selected = visible
            .iter()
            .position(|line| {
                line.kind == LineKind::Body && line.entry == cursor.entry && line.line == row
            })
            .expect("cursor row is visible");
        assert!(selected >= 3, "only {selected} rows above cursor");
        assert!(visible.len() - selected - 1 >= 3);

        // The prior render installed a scroll anchor. Centering must discard
        // it instead of allowing the next visible-lines pass to restore the
        // old viewport.
        viewport.move_cursor(&history.history(), ChatMotion::Down(1), 40);
        viewport
            .keep_cursor_visible(&history.history(), 40, 9, 1_000)
            .unwrap();
        let visible = viewport.visible_lines(&history.history(), 40, 9, 1);
        let cursor = viewport.cursor().unwrap();
        let row = viewport.cursor_row(&history.history(), 40).unwrap();
        let selected = visible
            .iter()
            .position(|line| {
                line.kind == LineKind::Body && line.entry == cursor.entry && line.line == row
            })
            .expect("centered cursor row is visible");
        assert_eq!(selected, 4);
    }

    #[test]
    fn viewport_scrolling_moves_cursor_into_the_safe_band() {
        let mut history = TestHistory::new();
        for id in 1..=30 {
            history.push(id, &format!("user-{id}"), &format!("message {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.visible_lines(&history.history(), 40, 9, 1);

        viewport
            .scroll_with_cursor(&history.history(), -5, 40, 9, 3)
            .unwrap();
        let visible = viewport.visible_lines(&history.history(), 40, 9, 1);
        let cursor = viewport.cursor().unwrap();
        let row = viewport.cursor_row(&history.history(), 40).unwrap();
        let selected = visible
            .iter()
            .position(|line| {
                line.kind == LineKind::Body && line.entry == cursor.entry && line.line == row
            })
            .expect("scrolled cursor row is visible");
        assert!(selected >= 3);
        assert!(visible.len() - selected - 1 >= 3);
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

    fn viewport_with(bodies: &[&str]) -> (TestHistory, ChatViewport) {
        let mut history = TestHistory::new();
        for (index, body) in bodies.iter().enumerate() {
            history.push(index as u64 + 1, "alice", body);
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.cursor_to_first();
        (history, viewport)
    }

    fn step(
        viewport: &mut ChatViewport,
        history: &TestHistory,
        motion: ChatMotion,
        width: u16,
    ) -> u32 {
        viewport
            .move_cursor(&history.history(), motion, width)
            .unwrap()
            .offset
    }

    #[test]
    fn j_moves_by_logical_line_across_wrapped_rows() {
        let (history, mut viewport) = viewport_with(&["alpha beta gamma\nsecond"]);
        assert_eq!(step(&mut viewport, &history, ChatMotion::Down(1), 6), 17);
        assert_eq!(viewport.cursor_row(&history.history(), 6), Some(3));
        assert_eq!(step(&mut viewport, &history, ChatMotion::Up(1), 6), 0);
    }

    #[test]
    fn gj_moves_by_display_row() {
        let (history, mut viewport) = viewport_with(&["alpha beta gamma\nsecond"]);
        assert_eq!(step(&mut viewport, &history, ChatMotion::RowDown(1), 6), 6);
        assert_eq!(viewport.cursor_row(&history.history(), 6), Some(1));
        assert_eq!(step(&mut viewport, &history, ChatMotion::RowUp(1), 6), 0);
    }

    #[test]
    fn vertical_motion_keeps_virtual_column() {
        let (history, mut viewport) = viewport_with(&["abcdef\nab\nabcdef"]);
        for _ in 0..4 {
            step(&mut viewport, &history, ChatMotion::Right, 40);
        }
        assert_eq!(step(&mut viewport, &history, ChatMotion::Down(1), 40), 8);
        assert_eq!(step(&mut viewport, &history, ChatMotion::Down(1), 40), 14);
    }

    #[test]
    fn dollar_sticks_to_line_end_across_j() {
        let (history, mut viewport) = viewport_with(&["ab\nabcdef"]);
        assert_eq!(step(&mut viewport, &history, ChatMotion::LineEnd, 40), 1);
        assert_eq!(step(&mut viewport, &history, ChatMotion::Down(1), 40), 8);
    }

    #[test]
    fn h_and_l_do_not_cross_logical_lines() {
        let (history, mut viewport) = viewport_with(&["ab\ncd"]);
        assert_eq!(step(&mut viewport, &history, ChatMotion::Left, 40), 0);
        for _ in 0..3 {
            step(&mut viewport, &history, ChatMotion::Right, 40);
        }
        assert_eq!(viewport.cursor().unwrap().offset, 1);
    }

    #[test]
    fn line_start_and_first_non_blank_differ_on_indented_lines() {
        let (history, mut viewport) = viewport_with(&["```\n    code\n```"]);
        assert_eq!(step(&mut viewport, &history, ChatMotion::FirstNonBlank, 40), 8);
        assert_eq!(step(&mut viewport, &history, ChatMotion::LineStart, 40), 4);
    }

    #[test]
    fn word_motions_cross_lines_and_messages() {
        let (history, mut viewport) = viewport_with(&["foo.bar baz", "  qux"]);
        let forward = ChatMotion::WordForward(WordKind::Small);
        assert_eq!(step(&mut viewport, &history, forward, 40), 3);
        assert_eq!(step(&mut viewport, &history, forward, 40), 4);
        assert_eq!(step(&mut viewport, &history, forward, 40), 8);
        assert_eq!(step(&mut viewport, &history, forward, 40), 2);
        assert_eq!(
            viewport.cursor().unwrap().entry,
            HistoryEntryId::Message(MessageId(2))
        );
        let back = ChatMotion::WordBack(WordKind::Small);
        assert_eq!(step(&mut viewport, &history, back, 40), 8);
        assert_eq!(
            viewport.cursor().unwrap().entry,
            HistoryEntryId::Message(MessageId(1))
        );

        viewport.cursor_to_first();
        let end = ChatMotion::WordEnd(WordKind::Small);
        assert_eq!(step(&mut viewport, &history, end, 40), 2);
        assert_eq!(step(&mut viewport, &history, end, 40), 3);
        assert_eq!(step(&mut viewport, &history, end, 40), 6);
    }

    #[test]
    fn w_stops_on_empty_lines() {
        let (history, mut viewport) = viewport_with(&["a\n\nb"]);
        let forward = ChatMotion::WordForward(WordKind::Small);
        step(&mut viewport, &history, forward, 40);
        assert_eq!(viewport.cursor_row(&history.history(), 40), Some(1));
        assert_eq!(step(&mut viewport, &history, forward, 40), 3);
    }

    #[test]
    fn big_word_treats_punctuation_as_word() {
        let (history, mut viewport) = viewport_with(&["foo.bar baz"]);
        assert_eq!(
            step(&mut viewport, &history, ChatMotion::WordForward(WordKind::Big), 40),
            8
        );
        assert_eq!(
            step(&mut viewport, &history, ChatMotion::WordBack(WordKind::Big), 40),
            0
        );
        assert_eq!(
            step(&mut viewport, &history, ChatMotion::WordEnd(WordKind::Big), 40),
            6
        );
    }

    #[test]
    fn pill_is_a_single_cursor_stop() {
        let code = rpc::msgref::MessageRef {
            room_id: RoomId(1),
            message_id: MessageId(1),
        }
        .encode();
        let reference = format!("@@{code}");
        let body = format!("see {reference} ok");
        let (history, mut viewport) = viewport_with(&["target", &body]);
        viewport.set_cursor(HistoryEntryId::Message(MessageId(2)));
        let forward = ChatMotion::WordForward(WordKind::Small);
        assert_eq!(step(&mut viewport, &history, forward, 80), 4);
        let after = 4 + reference.len() as u32;
        assert_eq!(step(&mut viewport, &history, ChatMotion::Right, 80), after);
        assert_eq!(step(&mut viewport, &history, ChatMotion::Left, 80), 4);

        viewport.toggle_visual(&history.history(), VisualKind::Char, 80);
        assert_eq!(
            viewport.visual_text(&history.history(), 80).as_deref(),
            Some(reference.as_str())
        );
    }

    #[test]
    fn linewise_yank_keeps_list_marker_and_code_indent() {
        let (history, mut viewport) = viewport_with(&["- item one\n\n```\n  indented\n```"]);
        assert_eq!(
            viewport.cursor_line_text(&history.history(), 40).as_deref(),
            Some("- item one")
        );
        step(&mut viewport, &history, ChatMotion::Down(2), 40);
        assert_eq!(
            viewport.cursor_line_text(&history.history(), 40).as_deref(),
            Some("  indented")
        );
    }

    #[test]
    fn charwise_yank_slices_source_offsets() {
        let (history, mut viewport) = viewport_with(&["hello **bold** world"]);
        viewport.toggle_visual(&history.history(), VisualKind::Char, 40);
        assert_eq!(
            step(&mut viewport, &history, ChatMotion::WordForward(WordKind::Small), 40),
            8
        );
        assert_eq!(
            viewport.visual_text(&history.history(), 40).as_deref(),
            Some("hello **b")
        );
    }

    #[test]
    fn selection_survives_reflow() {
        let (history, mut viewport) = viewport_with(&["alpha beta gamma"]);
        viewport.toggle_visual(&history.history(), VisualKind::Char, 6);
        step(&mut viewport, &history, ChatMotion::Right, 6);
        step(&mut viewport, &history, ChatMotion::Right, 6);
        viewport.on_reflow(&history.history(), 40);
        assert!(viewport.has_visual());
        assert_eq!(
            viewport.visual_text(&history.history(), 40).as_deref(),
            Some("alp")
        );
    }

    #[test]
    fn collapse_clamps_cursor_into_preview() {
        let body = (0..30)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (history, mut viewport) = viewport_with(&[&body]);
        assert!(viewport.toggle_expand(&history.history(), 0, 40));
        step(&mut viewport, &history, ChatMotion::Down(25), 40);
        assert_eq!(viewport.cursor_row(&history.history(), 40), Some(25));
        assert!(viewport.toggle_expand(&history.history(), 0, 40));
        assert_eq!(
            viewport.cursor_row(&history.history(), 40),
            Some(COLLAPSE_SHOW - 1)
        );
    }

    #[test]
    fn yank_omits_text_hidden_by_collapse() {
        let body = (0..30)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (history, mut viewport) = viewport_with(&[&body, "after"]);
        viewport.toggle_visual(&history.history(), VisualKind::Line, 40);
        step(&mut viewport, &history, ChatMotion::Down(COLLAPSE_SHOW), 40);
        let text = viewport.visual_text(&history.history(), 40).unwrap();
        let shown = (0..COLLAPSE_SHOW)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text, format!("{shown}\nafter"));
    }

    #[test]
    fn generation_change_forgets_desired_column() {
        let (mut history, mut viewport) = viewport_with(&["abcdef\nab"]);
        step(&mut viewport, &history, ChatMotion::LineEnd, 40);
        history.advance_generation();
        viewport.reconcile(&history.history());
        viewport.ensure_cursor(&history.history(), 40);
        assert_eq!(step(&mut viewport, &history, ChatMotion::Up(1), 40), 0);
    }

    #[test]
    fn empty_line_has_a_cursor_stop() {
        let (history, mut viewport) = viewport_with(&["a\n\nb"]);
        step(&mut viewport, &history, ChatMotion::Down(1), 40);
        assert_eq!(viewport.cursor_row(&history.history(), 40), Some(1));
        assert_eq!(
            viewport.cursor_line_text(&history.history(), 40).as_deref(),
            Some("")
        );
        assert_eq!(step(&mut viewport, &history, ChatMotion::Down(1), 40), 3);
    }

    #[test]
    fn cursor_line_marks_every_wrapped_row_and_one_cursor_cell() {
        let (history, mut viewport) = viewport_with(&["alpha beta gamma\nsecond"]);
        viewport.visible_lines(&history.history(), 6, 10, 1);
        step(&mut viewport, &history, ChatMotion::Right, 6);
        let marks = viewport.marks(&history.history(), 6);
        for row in 0..3 {
            let row_marks = viewport.row_marks(&history.history(), &marks, 0, row);
            assert!(row_marks.cursor_line, "row {row} is in the logical line");
            assert_eq!(row_marks.cursor_cell, (row == 0).then_some((1, 1)));
        }
        let other = viewport.row_marks(&history.history(), &marks, 0, 3);
        assert!(!other.cursor_line);
    }

    #[test]
    fn charwise_marks_cover_partial_rows() {
        let (history, mut viewport) = viewport_with(&["alpha beta gamma"]);
        viewport.visible_lines(&history.history(), 6, 10, 1);
        step(&mut viewport, &history, ChatMotion::Right, 6);
        viewport.toggle_visual(&history.history(), VisualKind::Char, 6);
        step(&mut viewport, &history, ChatMotion::RowDown(1), 6);
        let marks = viewport.marks(&history.history(), 6);
        let first = viewport.row_marks(&history.history(), &marks, 0, 0);
        let second = viewport.row_marks(&history.history(), &marks, 0, 1);
        let third = viewport.row_marks(&history.history(), &marks, 0, 2);
        assert_eq!(first.selected_cols, Some(1..5));
        assert_eq!(second.selected_cols, Some(0..2));
        assert_eq!(third.selected_cols, None);
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

/// A chat-log cursor position: a body byte offset within a stable history
/// entry. Offsets are independent of wrap width, so a position survives
/// reflow; [`ChatViewport`] snaps them onto the nearest visible cursor stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewCursor {
    pub entry: HistoryEntryId,
    /// Body byte offset. `u32::MAX` means the first non-blank stop of the
    /// entry's last visible line.
    pub offset: u32,
}

/// Shape of the chat log's visual selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VisualKind {
    /// `v`: from the anchor's stop through the cursor's stop.
    #[default]
    Char,
    /// `V`: every logical line between the anchor and the cursor.
    Line,
}

/// Whether a word motion splits punctuation from keyword runs (`w`) or treats
/// any non-blank run as one word (`W`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordKind {
    Small,
    Big,
}

/// A vim cursor motion in the chat log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatMotion {
    /// `h`: previous stop on the logical line.
    Left,
    /// `l`: next stop on the logical line.
    Right,
    /// `0`
    LineStart,
    /// `^`
    FirstNonBlank,
    /// `$`
    LineEnd,
    /// `j`: logical lines down.
    Down(usize),
    /// `k`: logical lines up.
    Up(usize),
    /// `gj`: wrapped display rows down.
    RowDown(usize),
    /// `gk`: wrapped display rows up.
    RowUp(usize),
    /// `w` / `W`
    WordForward(WordKind),
    /// `b` / `B`
    WordBack(WordKind),
    /// `e` / `E`
    WordEnd(WordKind),
}

/// Vim's `curswant`: the column vertical motions aim for, kept across lines
/// that are too short to hold it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesiredColumn {
    /// Cells from the start of the logical line, for `j`/`k`.
    Logical(usize),
    /// Screen column within the row, for `gj`/`gk`.
    Screen(u16),
    /// The line end, set by `$`.
    End,
}

/// The cursor resolved against the current layout.
#[derive(Clone, Copy, Debug)]
struct CursorSpot {
    message: usize,
    /// Logical line within `message`.
    line: usize,
    /// Index of `stop` among the logical line's stops.
    index: usize,
    stop: Stop,
}

impl CursorSpot {
    fn layout(self) -> LayoutCursor {
        LayoutCursor {
            message: self.message,
            line: self.stop.row,
        }
    }

    fn position(self) -> (usize, u32) {
        (self.message, self.stop.offset)
    }
}

/// A cursor walking stops in reading order across logical lines and messages,
/// for word motions.
struct StopWalk {
    message: usize,
    line: usize,
    index: usize,
    stops: Vec<Stop>,
}

impl StopWalk {
    fn current(&self) -> Stop {
        self.stops[self.index]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    SameLine,
    NewLine,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Down,
    Up,
}

/// Index of the first non-blank stop, or the last stop of an all-blank line.
fn first_non_blank(stops: &[Stop]) -> usize {
    stops
        .iter()
        .position(|stop| stop.class != CharClass::Whitespace)
        .unwrap_or(stops.len().saturating_sub(1))
}

/// Cells before stop `index` on its logical line: vim's virtual column.
fn logical_col(stops: &[Stop], index: usize) -> usize {
    stops[..index]
        .iter()
        .map(|stop| stop.width as usize)
        .sum()
}

/// The last stop whose logical column does not pass `want`.
fn index_at_logical_col(stops: &[Stop], want: usize) -> usize {
    let mut col = 0usize;
    let mut found = 0;
    for (index, stop) in stops.iter().enumerate() {
        if col > want {
            break;
        }
        found = index;
        col = col.saturating_add(stop.width as usize);
    }
    found
}

/// The cursor and visual selection resolved once per frame, so each rendered
/// row can be classified without re-resolving them.
pub struct ChatMarks {
    cursor: Option<(CursorSpot, Range<usize>)>,
    selection: Option<(VisualKind, CursorSpot, CursorSpot)>,
    /// For a line selection, the first row of the oldest line and the last
    /// row of the newest, as `(message, row)` pairs.
    line_rows: Option<((usize, usize), (usize, usize))>,
}

/// How one body row draws against the chat cursor and visual selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowMarks {
    /// The row belongs to the cursor's logical line.
    pub cursor_line: bool,
    /// The row holds the cursor cell.
    pub cursor_row: bool,
    /// The whole row is inside a line-wise selection.
    pub line_selected: bool,
    /// Columns covered by a character-wise selection on this row.
    pub selected_cols: Option<Range<u16>>,
    /// Column and width of the cursor cell, when on this row.
    pub cursor_cell: Option<(u16, u16)>,
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
    /// Navigation cursor; `None` only while the buffer is empty. An offset
    /// that no longer names a visible stop (after an edit or collapse) is
    /// snapped lazily by [`Self::ensure_cursor`].
    cursor: Option<ViewCursor>,
    /// `Some` means a visual selection of kind [`Self::visual`] spans
    /// `anchor..=cursor`, order-normalized. Keyboard visual mode and mouse
    /// drags share this.
    anchor: Option<ViewCursor>,
    visual: VisualKind,
    /// Column vertical motions aim for. Belongs to the current cursor
    /// position: anything that places the cursor other than a vertical
    /// motion or a viewport scroll resets it.
    desired: Option<DesiredColumn>,
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
            visual: VisualKind::Char,
            desired: None,
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
                offset: 0,
            });
            self.anchor = None;
            self.desired = None;
        }
        if follow.following && follow.cursor_at_tail && self.anchor.is_none() {
            self.cursor = self.entries.last().map(|entry| ViewCursor {
                entry: entry.id,
                offset: u32::MAX,
            });
            self.desired = None;
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
            self.desired = None;
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
                        expanded: id.is_notice(),
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
                offset: cursor.offset,
            })
        });
        self.anchor = self.anchor.and_then(|anchor| {
            Some(ViewCursor {
                entry: successor(anchor.entry, anchor_position)?,
                offset: anchor.offset,
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
            expanded: id.is_notice(),
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
        if self.cursor.is_some_and(|current| current.entry == id) {
            self.desired = None;
        }
        for cursor in [&mut self.cursor, &mut self.anchor] {
            if cursor.is_some_and(|current| current.entry == id) {
                *cursor = replacement.map(|entry| ViewCursor { entry, offset: 0 });
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
        if let Some(current) = self.cursor
            && !self.entries.iter().any(|entry| entry.id == current.entry)
        {
            self.desired = None;
        }
        for cursor in [&mut self.cursor, &mut self.anchor] {
            if let Some(current) = cursor
                && !self.entries.iter().any(|entry| entry.id == current.entry)
            {
                *cursor = front.map(|entry| ViewCursor { entry, offset: 0 });
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
            expanded: true,
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
        self.desired = None;
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

    /// Sets the selected entry's fold state. `None` means the entry is absent
    /// or not collapsible at the current width; `Some(false)` is an idempotent
    /// request whose state already matched.
    pub fn set_expand_entry(
        &mut self,
        history: &RoomHistoryRef<'_>,
        id: HistoryEntryId,
        width: u16,
        expanded: bool,
    ) -> Option<bool> {
        let index = self.entry_index(id)?;
        self.set_expand(history, index, width, expanded)
    }

    /// Opens every entry currently retained by this viewport. Entries that do
    /// not wrap far enough to fold remember the preference in case a later
    /// reflow makes them collapsible; future arrivals keep their normal
    /// message/notice defaults.
    pub fn expand_all(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if !entry.expanded {
                entry.expanded = true;
                changed = true;
            }
        }
        if changed {
            self.layout_index.invalidate();
            self.scroll_anchor = None;
            self.bump_layout_epoch();
        }
        changed
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

    /// Fills `out` with the stops of logical line `line` of `message`, clipped
    /// to its collapse-visible rows.
    fn load_line_stops(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        line: usize,
        width: u16,
        out: &mut Vec<Stop>,
    ) {
        let visible = self.visible_body_lines(history, message, width);
        self.ensure_lines(history, message, width);
        out.clear();
        let view = &self.entries[message];
        let body = self
            .resolve_record(history, view.id)
            .map_or("", |record| record.body);
        view.layout().line_stops(line, visible, body, out);
    }

    /// Number of logical lines of `message` with a collapse-visible row.
    fn visible_lines_of(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        width: u16,
    ) -> usize {
        let visible = self.visible_body_lines(history, message, width);
        self.ensure_lines(history, message, width);
        self.entries[message]
            .layout()
            .visible_logical_lines(visible)
    }

    /// Resolves `cursor` onto the stop it names: the last visible stop at or
    /// before its offset, so offsets inside hidden markup or a collapsed tail
    /// snap to what is on screen. Leaves the stops of its line in `stops`.
    fn locate(
        &mut self,
        history: &RoomHistoryRef<'_>,
        cursor: ViewCursor,
        width: u16,
        stops: &mut Vec<Stop>,
    ) -> Option<CursorSpot> {
        let message = self.entry_index(cursor.entry)?;
        let visible = self.visible_body_lines(history, message, width);
        self.ensure_lines(history, message, width);
        let layout = self.entries[message].layout();
        let line = if cursor.offset == u32::MAX {
            layout.visible_logical_lines(visible) - 1
        } else {
            layout.logical_line_of_row(layout.row_at_offset(cursor.offset, visible))
        };
        self.load_line_stops(history, message, line, width, stops);
        let index = if cursor.offset == u32::MAX {
            first_non_blank(stops)
        } else {
            stops
                .iter()
                .rposition(|stop| stop.offset <= cursor.offset)
                .unwrap_or(0)
        };
        Some(CursorSpot {
            message,
            line,
            index,
            stop: stops[index],
        })
    }

    fn place(&mut self, message: usize, stop: Stop) -> Option<ViewCursor> {
        self.cursor = Some(ViewCursor {
            entry: self.entries[message].id,
            offset: stop.offset,
        });
        self.cursor
    }

    /// The stop on wrapped `row` of `message` covering screen column `col`,
    /// or the nearest one on that row.
    fn stop_on_row(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        row: usize,
        col: u16,
        width: u16,
    ) -> Option<Stop> {
        self.ensure_lines(history, message, width);
        let line = self.entries[message].layout().logical_line_of_row(row);
        let mut stops = Vec::new();
        self.load_line_stops(history, message, line, width, &mut stops);
        let mut on_row = stops.iter().filter(|stop| stop.row == row);
        let first = on_row.clone().next();
        on_row
            .rfind(|stop| stop.col <= col)
            .or(first)
            .or(stops.first())
            .copied()
    }

    /// The visible logical line after `line` of `message`, crossing into the
    /// next message.
    fn line_after(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        line: usize,
        width: u16,
    ) -> Option<(usize, usize)> {
        if line + 1 < self.visible_lines_of(history, message, width) {
            return Some((message, line + 1));
        }
        (message + 1 < self.entries.len()).then_some((message + 1, 0))
    }

    /// The visible logical line before `line` of `message`, crossing into the
    /// previous message's last visible line.
    fn line_before(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        line: usize,
        width: u16,
    ) -> Option<(usize, usize)> {
        if line > 0 {
            return Some((message, line - 1));
        }
        let message = message.checked_sub(1)?;
        Some((message, self.visible_lines_of(history, message, width) - 1))
    }

    /// Places the cursor and a character-wise anchor at the stop under
    /// `(row, col)` of `entry` and starts a mouse drag.
    pub fn begin_drag(
        &mut self,
        history: &RoomHistoryRef<'_>,
        entry: HistoryEntryId,
        row: usize,
        col: u16,
        width: u16,
    ) {
        let Some(message) = self.entry_index(entry) else {
            return;
        };
        let Some(stop) = self.stop_on_row(history, message, row, col, width) else {
            return;
        };
        self.place(message, stop);
        self.desired = None;
        self.anchor = self.cursor;
        self.visual = VisualKind::Char;
        self.dragging = true;
    }

    /// Moves the cursor of an in-progress drag to the stop under `(row, col)`
    /// of `entry`; the anchor stays.
    pub fn drag_to(
        &mut self,
        history: &RoomHistoryRef<'_>,
        entry: HistoryEntryId,
        row: usize,
        col: u16,
        width: u16,
    ) {
        if !self.dragging {
            return;
        }
        let Some(message) = self.entry_index(entry) else {
            return;
        };
        let Some(stop) = self.stop_on_row(history, message, row, col, width) else {
            return;
        };
        self.place(message, stop);
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

    /// The wrapped row within its message that the cursor sits on.
    #[cfg(test)]
    pub fn cursor_row(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> Option<usize> {
        Some(self.ensure_layout_cursor(history, width)?.stop.row)
    }

    /// Returns a valid cursor, defaulting to the start of the newest message's
    /// last line and snapping stale offsets onto a visible stop. `None` only
    /// when empty.
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
    ) -> Option<CursorSpot> {
        if self.entries.is_empty() {
            self.cursor = None;
            self.anchor = None;
            return None;
        }
        if self.cursor.is_none() {
            self.cursor = Some(ViewCursor {
                entry: self.entries.last()?.id,
                offset: u32::MAX,
            });
            self.desired = None;
        }
        self.clamp_positions(history, width)
    }

    /// Moves the cursor to the start of a stable history entry, clearing any
    /// visual anchor.
    pub fn set_cursor(&mut self, entry: HistoryEntryId) -> Option<ViewCursor> {
        if !self.entries.iter().any(|candidate| candidate.id == entry) {
            return None;
        }
        self.cursor = Some(ViewCursor { entry, offset: 0 });
        self.anchor = None;
        self.desired = None;
        self.cursor()
    }

    /// Applies a vim motion. Vertical motions skip lines hidden by collapse
    /// and clamp at the buffer edges; `h`/`l` never leave the logical line.
    pub fn move_cursor(
        &mut self,
        history: &RoomHistoryRef<'_>,
        motion: ChatMotion,
        width: u16,
    ) -> Option<ViewCursor> {
        let spot = self.ensure_layout_cursor(history, width)?;
        let mut stops = Vec::new();
        self.load_line_stops(history, spot.message, spot.line, width, &mut stops);
        match motion {
            ChatMotion::Left => {
                self.desired = None;
                self.place(spot.message, stops[spot.index.saturating_sub(1)])
            }
            ChatMotion::Right => {
                self.desired = None;
                let index = (spot.index + 1).min(stops.len() - 1);
                self.place(spot.message, stops[index])
            }
            ChatMotion::LineStart => {
                self.desired = None;
                self.place(spot.message, stops[0])
            }
            ChatMotion::FirstNonBlank => {
                self.desired = None;
                self.place(spot.message, stops[first_non_blank(&stops)])
            }
            ChatMotion::LineEnd => {
                self.desired = Some(DesiredColumn::End);
                self.place(spot.message, stops[stops.len() - 1])
            }
            ChatMotion::Down(count) => {
                self.move_lines(history, spot, &stops, count, Direction::Down, width)
            }
            ChatMotion::Up(count) => {
                self.move_lines(history, spot, &stops, count, Direction::Up, width)
            }
            ChatMotion::RowDown(count) => {
                self.move_rows(history, spot, count, Direction::Down, width)
            }
            ChatMotion::RowUp(count) => self.move_rows(history, spot, count, Direction::Up, width),
            ChatMotion::WordForward(kind) => {
                let mut walk = self.walk_from(spot, stops);
                self.word_forward(history, &mut walk, kind, width);
                self.desired = None;
                self.place(walk.message, walk.current())
            }
            ChatMotion::WordBack(kind) => {
                let mut walk = self.walk_from(spot, stops);
                self.word_back(history, &mut walk, kind, width);
                self.desired = None;
                self.place(walk.message, walk.current())
            }
            ChatMotion::WordEnd(kind) => {
                let mut walk = self.walk_from(spot, stops);
                self.word_end(history, &mut walk, kind, width);
                self.desired = None;
                self.place(walk.message, walk.current())
            }
        }
    }

    /// `j`/`k`: steps `count` logical lines, landing on the stop nearest the
    /// desired logical column.
    fn move_lines(
        &mut self,
        history: &RoomHistoryRef<'_>,
        spot: CursorSpot,
        stops: &[Stop],
        count: usize,
        direction: Direction,
        width: u16,
    ) -> Option<ViewCursor> {
        let want = match self.desired {
            Some(DesiredColumn::Logical(col)) => col,
            Some(DesiredColumn::End) => usize::MAX,
            Some(DesiredColumn::Screen(_)) | None => logical_col(stops, spot.index),
        };
        let (mut message, mut line) = (spot.message, spot.line);
        for _ in 0..count {
            let next = match direction {
                Direction::Down => self.line_after(history, message, line, width),
                Direction::Up => self.line_before(history, message, line, width),
            };
            let Some(next) = next else { break };
            (message, line) = next;
        }
        let mut stops = Vec::new();
        self.load_line_stops(history, message, line, width, &mut stops);
        self.desired = Some(match want {
            usize::MAX => DesiredColumn::End,
            col => DesiredColumn::Logical(col),
        });
        self.place(message, stops[index_at_logical_col(&stops, want)])
    }

    /// `gj`/`gk`: steps `count` wrapped rows, landing on the stop nearest the
    /// desired screen column.
    fn move_rows(
        &mut self,
        history: &RoomHistoryRef<'_>,
        spot: CursorSpot,
        count: usize,
        direction: Direction,
        width: u16,
    ) -> Option<ViewCursor> {
        let col = self.desired_screen_col(spot);
        let mut pos = spot.layout();
        for _ in 0..count {
            let next = match direction {
                Direction::Down => self.next_body_pos(history, pos, width),
                Direction::Up => self.prev_body_pos(history, pos, width),
            };
            let Some(next) = next else { break };
            pos = next;
        }
        let stop = self.stop_on_row(history, pos.message, pos.line, col, width)?;
        if col != u16::MAX {
            self.desired = Some(DesiredColumn::Screen(col));
        }
        self.place(pos.message, stop)
    }

    /// The screen column a row-wise move from `spot` aims for: `u16::MAX`
    /// for a `$` cursor, otherwise any remembered `gj`/`gk` column or the
    /// cursor's own.
    fn desired_screen_col(&self, spot: CursorSpot) -> u16 {
        match self.desired {
            Some(DesiredColumn::Screen(col)) => col,
            Some(DesiredColumn::End) => u16::MAX,
            Some(DesiredColumn::Logical(_)) | None => spot.stop.col,
        }
    }

    fn walk_from(&self, spot: CursorSpot, stops: Vec<Stop>) -> StopWalk {
        StopWalk {
            message: spot.message,
            line: spot.line,
            index: spot.index,
            stops,
        }
    }

    fn walk_next(
        &mut self,
        history: &RoomHistoryRef<'_>,
        walk: &mut StopWalk,
        width: u16,
    ) -> Option<Step> {
        if walk.index + 1 < walk.stops.len() {
            walk.index += 1;
            return Some(Step::SameLine);
        }
        let (message, line) = self.line_after(history, walk.message, walk.line, width)?;
        self.load_line_stops(history, message, line, width, &mut walk.stops);
        walk.message = message;
        walk.line = line;
        walk.index = 0;
        Some(Step::NewLine)
    }

    fn walk_prev(
        &mut self,
        history: &RoomHistoryRef<'_>,
        walk: &mut StopWalk,
        width: u16,
    ) -> Option<Step> {
        if walk.index > 0 {
            walk.index -= 1;
            return Some(Step::SameLine);
        }
        let (message, line) = self.line_before(history, walk.message, walk.line, width)?;
        self.load_line_stops(history, message, line, width, &mut walk.stops);
        walk.message = message;
        walk.line = line;
        walk.index = walk.stops.len() - 1;
        Some(Step::NewLine)
    }

    /// `w`: past the current word, then past blanks and line breaks to the
    /// next word or empty line.
    fn word_forward(
        &mut self,
        history: &RoomHistoryRef<'_>,
        walk: &mut StopWalk,
        kind: WordKind,
        width: u16,
    ) {
        let start = walk.current().class.widen(kind);
        if start.is_word() {
            loop {
                let Some(step) = self.walk_next(history, walk, width) else {
                    return;
                };
                if step == Step::NewLine || walk.current().class.widen(kind) != start {
                    break;
                }
            }
        } else if self.walk_next(history, walk, width).is_none() {
            return;
        }
        while walk.current().class == CharClass::Whitespace {
            if self.walk_next(history, walk, width).is_none() {
                return;
            }
        }
    }

    /// `e`: forward at least one stop, past blanks and empty lines, then to
    /// the last stop of that word.
    fn word_end(
        &mut self,
        history: &RoomHistoryRef<'_>,
        walk: &mut StopWalk,
        kind: WordKind,
        width: u16,
    ) {
        if self.walk_next(history, walk, width).is_none() {
            return;
        }
        while !walk.current().class.is_word() {
            if self.walk_next(history, walk, width).is_none() {
                return;
            }
        }
        let class = walk.current().class.widen(kind);
        while walk
            .stops
            .get(walk.index + 1)
            .is_some_and(|stop| stop.class.widen(kind) == class)
        {
            walk.index += 1;
        }
    }

    /// `b`: back at least one stop, past blanks and line breaks, then to the
    /// first stop of that word. An empty line is a word of its own.
    fn word_back(
        &mut self,
        history: &RoomHistoryRef<'_>,
        walk: &mut StopWalk,
        kind: WordKind,
        width: u16,
    ) {
        if self.walk_prev(history, walk, width).is_none() {
            return;
        }
        while walk.current().class == CharClass::Whitespace {
            if self.walk_prev(history, walk, width).is_none() {
                return;
            }
        }
        let class = walk.current().class.widen(kind);
        if !class.is_word() {
            return;
        }
        while walk.index > 0 && walk.stops[walk.index - 1].class.widen(kind) == class {
            walk.index -= 1;
        }
    }

    /// Moves the cursor to the first stop of the block `delta` blocks away (a
    /// sender-group heading boundary), clamping at the ends.
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
        self.desired = None;
        self.cursor = Some(ViewCursor {
            entry: self.entries[message].id,
            offset: 0,
        });
        self.cursor()
    }

    /// `gg`: the first stop of the oldest entry.
    pub fn cursor_to_first(&mut self) -> Option<ViewCursor> {
        let first = self.entries.first()?.id;
        self.desired = None;
        self.cursor = Some(ViewCursor {
            entry: first,
            offset: 0,
        });
        self.cursor()
    }

    /// `G`: the first non-blank stop of the newest entry's last visible line.
    pub fn cursor_to_last(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Option<ViewCursor> {
        let last = self.entries.last()?.id;
        self.desired = None;
        self.cursor = Some(ViewCursor {
            entry: last,
            offset: u32::MAX,
        });
        self.ensure_cursor(history, width)
    }

    /// Vim's `v`/`V`: starts a selection of `kind` at the cursor, switches an
    /// active selection of the other kind, or ends one of the same kind.
    /// Returns whether a selection is now active.
    pub fn toggle_visual(
        &mut self,
        history: &RoomHistoryRef<'_>,
        kind: VisualKind,
        width: u16,
    ) -> bool {
        if self.anchor.is_some() {
            if self.visual == kind {
                self.anchor = None;
                return false;
            }
            self.visual = kind;
            return true;
        }
        self.ensure_cursor(history, width);
        self.anchor = self.cursor;
        self.visual = kind;
        self.anchor.is_some()
    }

    /// Clears the visual selection, returning whether one existed.
    pub fn clear_visual_anchor(&mut self) -> bool {
        self.anchor.take().is_some()
    }

    pub fn has_visual(&self) -> bool {
        self.anchor.is_some()
    }

    /// The selection's kind and ordered `(oldest, newest)` endpoints.
    fn selection_bounds(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Option<(VisualKind, CursorSpot, CursorSpot)> {
        let cursor = self.ensure_layout_cursor(history, width)?;
        let mut stops = Vec::new();
        let anchor = self.locate(history, self.anchor?, width, &mut stops)?;
        if anchor.position() <= cursor.position() {
            Some((self.visual, anchor, cursor))
        } else {
            Some((self.visual, cursor, anchor))
        }
    }

    /// Stable entries targeted by an action on the current chat selection.
    /// A plain cursor targets its whole message; a visual range targets each
    /// message touched by either endpoint or anything between them.
    pub fn selected_entries(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Vec<HistoryEntryId> {
        let Some(cursor) = self.ensure_layout_cursor(history, width) else {
            return Vec::new();
        };
        let Some((_, lo, hi)) = self.selection_bounds(history, width) else {
            return vec![self.entries[cursor.message].id];
        };
        self.entries[lo.message..=hi.message]
            .iter()
            .map(|entry| entry.id)
            .collect()
    }

    /// Copies the original body text of the visual selection, content only (no
    /// sender column). A character-wise range slices source offsets through
    /// the newest endpoint's stop; a line-wise range covers whole source
    /// lines, so markup the renderer hides is kept. Text a collapse hides is
    /// never highlighted, so it is not copied either. Messages join with `\n`.
    pub fn visual_text(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> Option<String> {
        let (kind, lo, hi) = self.selection_bounds(history, width)?;
        let mut out = String::new();
        for message in lo.message..=hi.message {
            let visible_end = self.visible_source_end(history, message, width);
            let view = &self.entries[message];
            let body = self
                .resolve_record(history, view.id)
                .map_or("", |record| record.body);
            let layout = view.layout();
            let start = match (message == lo.message, kind) {
                (false, _) => 0,
                (true, VisualKind::Char) => lo.stop.offset as usize,
                (true, VisualKind::Line) => layout.logical_source_line(lo.line, body).start,
            };
            let end = match (message == hi.message, kind) {
                (false, _) => body.len(),
                (true, VisualKind::Char) => hi.stop.end as usize,
                (true, VisualKind::Line) => layout.logical_source_line(hi.line, body).end,
            };
            if message != lo.message {
                out.push('\n');
            }
            let end = end.min(visible_end);
            let start = start.min(end);
            out.push_str(&body[start..end]);
        }
        Some(out)
    }

    /// The original body text of the cursor's logical line, widened to its
    /// source line so hidden markup (list markers, `# `, `> `) is kept, and
    /// clipped where a collapse hides the rest of the line.
    pub fn cursor_line_text(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> Option<String> {
        let spot = self.ensure_layout_cursor(history, width)?;
        let visible_end = self.visible_source_end(history, spot.message, width);
        let view = &self.entries[spot.message];
        let body = self.resolve_record(history, view.id)?.body;
        let range = view.layout().logical_source_line(spot.line, body);
        let end = range.end.min(visible_end);
        Some(body[range.start.min(end)..end].to_string())
    }

    /// The body offset where `message`'s collapse-visible rows end: the body
    /// length when nothing is hidden.
    fn visible_source_end(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        width: u16,
    ) -> usize {
        let visible = self.visible_body_lines(history, message, width);
        self.ensure_lines(history, message, width);
        let view = &self.entries[message];
        let len = self
            .resolve_record(history, view.id)
            .map_or(0, |record| record.body.len());
        let layout = view.layout();
        if visible >= layout.lines() {
            return len;
        }
        layout.source_range(0, visible.saturating_sub(1), len).end
    }

    pub fn cursor_message_body<'a>(&'a self, history: &'a RoomHistoryRef<'a>) -> Option<&'a str> {
        let message = self.entry_index(self.cursor?.entry)?;
        Some(
            self.resolve_record(history, self.entries.get(message)?.id)?
                .body,
        )
    }

    /// The first decodable reference contained in the cursor's message, for
    /// keyboard-driven "open the reference in this message".
    pub fn cursor_ref(&self, history: &RoomHistoryRef<'_>) -> Option<rpc::msgref::MessageRef> {
        let message = self.entry_index(self.cursor?.entry)?;
        let entry = self.resolve_record(history, self.entries.get(message)?.id)?;
        let inline = chatt_message_format::inline_ranges(entry.body);
        for span in self.build_ref_spans(history, entry.body, inline.refs) {
            if let Some(target) = span.target {
                return Some(target);
            }
        }
        None
    }

    /// Resolves the cursor and selection for one frame of rendering. Call
    /// after the visible rows are laid out.
    pub fn marks(&mut self, history: &RoomHistoryRef<'_>, width: u16) -> ChatMarks {
        let cursor = self.ensure_layout_cursor(history, width).map(|spot| {
            let visible = self.visible_body_lines(history, spot.message, width);
            let rows = self.entries[spot.message]
                .layout()
                .logical_rows(spot.line, visible);
            (spot, rows)
        });
        let selection = self.selection_bounds(history, width);
        let line_rows = match selection {
            Some((VisualKind::Line, lo, hi)) => {
                let visible = self.visible_body_lines(history, hi.message, width);
                let first = self.entries[lo.message]
                    .layout()
                    .logical_rows(lo.line, usize::MAX)
                    .start;
                let last = self.entries[hi.message]
                    .layout()
                    .logical_rows(hi.line, visible)
                    .end
                    .saturating_sub(1);
                Some(((lo.message, first), (hi.message, last)))
            }
            _ => None,
        };
        ChatMarks {
            cursor,
            selection,
            line_rows,
        }
    }

    /// Classifies wrapped `row` of `message` against `marks`.
    pub fn row_marks(
        &self,
        history: &RoomHistoryRef<'_>,
        marks: &ChatMarks,
        message: usize,
        row: usize,
    ) -> RowMarks {
        let mut out = RowMarks::default();
        if let Some((spot, rows)) = &marks.cursor
            && spot.message == message
        {
            out.cursor_line = rows.contains(&row);
            if spot.stop.row == row {
                out.cursor_row = true;
                out.cursor_cell = Some((spot.stop.col, spot.stop.width.max(1)));
            }
        }
        let pos = (message, row);
        if let Some((lo, hi)) = marks.line_rows {
            out.line_selected = lo <= pos && pos <= hi;
            return out;
        }
        let Some((VisualKind::Char, lo, hi)) = marks.selection else {
            return out;
        };
        let lo_pos = (lo.message, lo.stop.row);
        let hi_pos = (hi.message, hi.stop.row);
        if pos < lo_pos || hi_pos < pos {
            return out;
        }
        let Some(view) = self.entries.get(message) else {
            return out;
        };
        let Some(layout) = view.layout.as_deref() else {
            return out;
        };
        let body = self
            .resolve_record(history, view.id)
            .map_or("", |record| record.body);
        let mut stops = Vec::new();
        layout.row_stops(row..row + 1, body, &mut stops);
        if stops.is_empty() {
            layout.line_stops(layout.logical_line_of_row(row), usize::MAX, body, &mut stops);
        }
        let mut on_row = stops.iter().filter(|stop| stop.row == row);
        let Some(first) = on_row.next() else {
            return out;
        };
        let last = on_row.last().unwrap_or(first);
        let start = if pos == lo_pos { lo.stop.col } else { first.col };
        let end = if pos == hi_pos {
            hi.stop.col.saturating_add(hi.stop.width.max(1))
        } else {
            last.col.saturating_add(last.width.max(1))
        };
        out.selected_cols = (start < end).then_some(start..end);
        out
    }

    /// Scrolls the minimum amount that brings the cursor's row into the safe
    /// viewport band. `scroll_buffer` is the desired rendered-row context on
    /// both sides; when it cannot fit, the safe band contracts to the center.
    pub fn keep_cursor_visible(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
        height: u16,
        scroll_buffer: usize,
    ) -> Option<()> {
        let height = height as usize;
        if height == 0 {
            return None;
        }
        let cursor = self.ensure_layout_cursor(history, width)?;
        let (row, total) = self.pos_row_and_total(history, cursor.layout(), width)?;
        let max_scroll = total.saturating_sub(height);
        self.scroll_offset = self.scroll_offset.min(max_scroll);
        let top = total.saturating_sub(self.scroll_offset.saturating_add(height));
        let bottom = top.saturating_add(height);
        let before = scroll_buffer.min(height / 2);
        let after = scroll_buffer.min(height.saturating_sub(1) / 2);
        let safe_top = top.saturating_add(before);
        let safe_bottom = bottom.saturating_sub(after);
        let previous = self.scroll_offset;
        if row < safe_top {
            let desired_top = row.saturating_sub(before).min(max_scroll);
            self.scroll_offset = total
                .saturating_sub(desired_top.saturating_add(height))
                .min(max_scroll);
        } else if row >= safe_bottom {
            let desired_bottom = row.saturating_add(after).saturating_add(1).min(total);
            let desired_top = desired_bottom.saturating_sub(height).min(max_scroll);
            self.scroll_offset = total
                .saturating_sub(desired_top.saturating_add(height))
                .min(max_scroll);
        }
        if self.scroll_offset != previous {
            // Explicit navigation owns the viewport now. A retained layout
            // anchor from the previous frame must not restore the old window.
            self.scroll_anchor = None;
        }
        Some(())
    }

    /// Moves the viewport by rendered rows while keeping its cursor in the
    /// configured safe band. If scrolling would push the cursor outside, the
    /// cursor moves to the nearest selectable body row in that band.
    pub fn scroll_with_cursor(
        &mut self,
        history: &RoomHistoryRef<'_>,
        rows: isize,
        width: u16,
        height: u16,
        scroll_buffer: usize,
    ) -> Option<()> {
        if rows < 0 {
            self.scroll_up(history, rows.unsigned_abs(), width, height);
        } else if rows > 0 {
            self.scroll_down(rows as usize);
        } else {
            return self.keep_cursor_visible(history, width, height, scroll_buffer);
        }

        let height = height as usize;
        if height == 0 {
            return None;
        }
        let cursor = self.ensure_layout_cursor(history, width)?;
        let (cursor_row, total) = self.pos_row_and_total(history, cursor.layout(), width)?;
        let max_top = total.saturating_sub(height);
        let top = total
            .saturating_sub(self.scroll_offset.saturating_add(height))
            .min(max_top);
        let bottom = top.saturating_add(height).min(total);
        let before = scroll_buffer.min(height / 2);
        let after = scroll_buffer.min(height.saturating_sub(1) / 2);
        let safe_top = top.saturating_add(before).min(bottom.saturating_sub(1));
        let safe_bottom = bottom.saturating_sub(after).max(safe_top.saturating_add(1));

        let replacement = if cursor_row < safe_top {
            self.body_cursor_at_or_after(safe_top, total)
                .or_else(|| self.body_cursor_at_or_before(safe_top))
        } else if cursor_row >= safe_bottom {
            self.body_cursor_at_or_before(safe_bottom.saturating_sub(1))
                .or_else(|| self.body_cursor_at_or_after(safe_bottom, total))
        } else {
            None
        };
        let col = self.desired_screen_col(cursor);
        if let Some(replacement) = replacement
            && let Some(stop) = self.stop_on_row(
                history,
                replacement.message,
                replacement.line,
                col,
                width,
            )
        {
            // Dragging the cursor along with the viewport is a row-wise move:
            // it lands on the remembered column and, like `gj`, remembers the
            // screen column so successive ticks do not drift on short rows.
            if self.desired.is_none() {
                self.desired = Some(DesiredColumn::Screen(col));
            }
            self.place(replacement.message, stop);
        }
        self.keep_cursor_visible(history, width, height as u16, scroll_buffer)
    }

    fn body_cursor_at_or_after(&self, start: usize, total: usize) -> Option<LayoutCursor> {
        for row in start..total {
            let line = self.cached_visible_line(row)?;
            if line.kind == LineKind::Body {
                return Some(LayoutCursor {
                    message: line.message,
                    line: line.line,
                });
            }
        }
        None
    }

    fn body_cursor_at_or_before(&self, start: usize) -> Option<LayoutCursor> {
        for row in (0..=start).rev() {
            let line = self.cached_visible_line(row)?;
            if line.kind == LineKind::Body {
                return Some(LayoutCursor {
                    message: line.message,
                    line: line.line,
                });
            }
        }
        None
    }

    /// Reflow at a new width invalidates wrapped-row coordinates only. Cursor
    /// and anchor are body offsets, so both survive and merely re-snap.
    pub fn on_reflow(&mut self, history: &RoomHistoryRef<'_>, width: u16) {
        self.dragging = false;
        if matches!(self.desired, Some(DesiredColumn::Screen(_))) {
            self.desired = None;
        }
        self.scroll_anchor = None;
        self.layout_index.invalidate();
        self.bump_layout_epoch();
        self.clamp_positions(history, width);
    }

    /// Snaps the cursor and anchor onto visible stops at `width`, pulling
    /// positions under a collapse into its preview. Returns the resolved
    /// cursor.
    fn clamp_positions(
        &mut self,
        history: &RoomHistoryRef<'_>,
        width: u16,
    ) -> Option<CursorSpot> {
        if self.entries.is_empty() {
            self.cursor = None;
            self.anchor = None;
            return None;
        }
        let mut stops = Vec::new();
        if let Some(anchor) = self.anchor
            && let Some(spot) = self.locate(history, anchor, width, &mut stops)
        {
            self.anchor = Some(ViewCursor {
                entry: anchor.entry,
                offset: spot.stop.offset,
            });
        }
        let cursor = self.cursor?;
        let spot = self.locate(history, cursor, width, &mut stops)?;
        self.cursor = Some(ViewCursor {
            entry: cursor.entry,
            offset: spot.stop.offset,
        });
        Some(spot)
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
        self.scroll_anchor = None;

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
        let Some(expanded) = self.entries.get(message).map(|entry| !entry.expanded) else {
            return false;
        };
        self.set_expand(history, message, width, expanded)
            .is_some_and(|changed| changed)
    }

    fn set_expand(
        &mut self,
        history: &RoomHistoryRef<'_>,
        message: usize,
        width: u16,
        expanded: bool,
    ) -> Option<bool> {
        if message >= self.entries.len()
            || self.ensure_lines(history, message, width) <= COLLAPSE_LIMIT
        {
            return None;
        }
        if self.entries[message].expanded == expanded {
            return Some(false);
        }
        self.entries[message].expanded = expanded;
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
        self.scroll_anchor = None;
        self.bump_layout_epoch();
        Some(true)
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
    /// First row of each logical line: a row the layout started fresh, as
    /// opposed to a wrap continuation. Ascending, and `[0]` is always row 0.
    logical_starts: Vec<u32>,
    /// Hidden `@@code` body range of each reference, indexed like the
    /// `ref_index` of [`Self::pill_spans`], so a pill maps back to its source.
    ref_ranges: Vec<Range<u32>>,
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
            logical_starts: Vec::new(),
            ref_ranges: Vec::new(),
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
            self.ref_ranges.clear();
            self.ref_ranges
                .extend(refs.iter().map(|span| span.range.clone()));
        }
        while !self.complete {
            self.layout_next_block(text, refs);
        }
        if self.line_starts.is_empty() {
            self.push_logical_line();
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
        self.logical_starts.clear();
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
                self.push_logical_line();
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
            self.push_logical_line();
            for source in invisible {
                self.note_source_range(source.source.start, source.source.end);
            }
            return;
        }

        let mut wrapped_any = false;
        for wrapped in bwrap::wrap_ranges_preserve_leading(display, widths.0, widths.1) {
            if wrapped_any {
                self.push_line();
            } else {
                self.push_logical_line();
            }
            let base_col = if wrapped_any { cols.1 } else { cols.0 };
            wrapped_any = true;
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
            self.push_logical_line();
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
            self.push_logical_line();
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

    fn push_logical_line(&mut self) {
        self.logical_starts.push(self.line_starts.len() as u32);
        self.push_line();
    }

    fn emit_plain_verbatim(
        &mut self,
        text: &str,
        start: usize,
        end: usize,
        avail: usize,
        style: Style,
    ) {
        self.push_logical_line();
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
        self.push_logical_line();
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

    fn logical_lines(&self) -> usize {
        self.logical_starts.len()
    }

    /// The logical line `row` belongs to.
    fn logical_line_of_row(&self, row: usize) -> usize {
        self.logical_starts
            .partition_point(|&start| start as usize <= row)
            .saturating_sub(1)
    }

    /// The rows logical line `line` spans, clipped to the first `visible` rows.
    fn logical_rows(&self, line: usize, visible: usize) -> Range<usize> {
        let start = self.logical_starts[line] as usize;
        let end = self
            .logical_starts
            .get(line + 1)
            .map_or(self.lines(), |&end| end as usize)
            .min(visible);
        start..end.max(start)
    }

    /// Number of logical lines with at least one row among the first `visible`.
    fn visible_logical_lines(&self, visible: usize) -> usize {
        self.logical_starts
            .partition_point(|&start| (start as usize) < visible)
            .max(1)
            .min(self.logical_lines())
    }

    /// The last visible row whose source starts at or before `offset`, or row 0.
    fn row_at_offset(&self, offset: u32, visible: usize) -> usize {
        let mut found = 0;
        for (row, &(start, _)) in self.line_sources.iter().take(visible).enumerate() {
            if start == u32::MAX {
                continue;
            }
            if start > offset {
                break;
            }
            found = row;
        }
        found
    }

    /// Appends the cursor stops of logical line `line` in display order,
    /// clipped to the first `visible` rows. A line with no navigable cell
    /// still yields one empty stop at its lead column, so every visible
    /// logical line can hold the cursor.
    fn line_stops(&self, line: usize, visible: usize, body: &str, out: &mut Vec<Stop>) {
        let rows = self.logical_rows(line, visible);
        let first = out.len();
        let lead = self.row_stops(rows.clone(), body, out);
        if out.len() == first {
            let offset = self
                .source_range(rows.start, rows.end.saturating_sub(1), body.len())
                .start as u32;
            out.push(Stop {
                offset,
                end: offset,
                row: rows.start,
                col: lead.unwrap_or(0),
                width: 0,
                class: CharClass::Empty,
            });
        }
    }

    /// Appends the stops drawn on `rows`, returning the column right of any
    /// synthetic lead (a quote marker) on the first row.
    fn row_stops(&self, rows: Range<usize>, body: &str, out: &mut Vec<Stop>) -> Option<u16> {
        let first = out.len();
        let mut lead = None;
        for row in rows.clone() {
            for seg in self.line(row) {
                if seg.synth {
                    let text = &self.synthetic[seg.start as usize..seg.end as usize];
                    let width = UnicodeWidthStr::width(text).min(u16::MAX as usize) as u16;
                    let Some(index) = self.pill_ref_at(seg) else {
                        if row == rows.start {
                            lead = Some(seg.col.saturating_add(width));
                        }
                        continue;
                    };
                    let Some(source) = self.ref_ranges.get(index) else {
                        continue;
                    };
                    // A pill wrapped across rows renders twice but is one stop.
                    if out[first..]
                        .last()
                        .is_some_and(|stop: &Stop| stop.offset == source.start)
                    {
                        continue;
                    }
                    out.push(Stop {
                        offset: source.start,
                        end: source.end,
                        row,
                        col: seg.col,
                        width,
                        class: CharClass::Keyword,
                    });
                    continue;
                }
                let text = &body[seg.start as usize..seg.end as usize];
                let mut col = seg.col;
                for (index, grapheme) in text.grapheme_indices(true) {
                    let width = UnicodeWidthStr::width(grapheme).min(u16::MAX as usize) as u16;
                    let offset = seg.start + index as u32;
                    out.push(Stop {
                        offset,
                        end: offset + grapheme.len() as u32,
                        row,
                        col,
                        width,
                        class: CharClass::of(grapheme),
                    });
                    col = col.saturating_add(width);
                }
            }
        }
        lead
    }

    /// The body range logical line `line` displays, widened to the enclosing
    /// source line breaks so markup the renderer hides (list markers, `# `,
    /// `> `, code indentation) is part of the yank.
    fn logical_source_line(&self, line: usize, body: &str) -> Range<usize> {
        let rows = self.logical_rows(line, usize::MAX);
        let range = self.source_range(rows.start, rows.end.saturating_sub(1), body.len());
        let start = body[..range.start].rfind('\n').map_or(0, |index| index + 1);
        let end = body[range.end..]
            .find('\n')
            .map_or(body.len(), |index| range.end + index);
        start..end.max(start)
    }
}

/// Word-motion class of one cursor stop, following vim's default
/// `iskeyword`. `Empty` marks the single stop of a line with no cells, which
/// vim's word motions treat as a word of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CharClass {
    Keyword,
    Whitespace,
    Other,
    Empty,
}

impl CharClass {
    fn of(grapheme: &str) -> Self {
        let Some(ch) = grapheme.chars().next() else {
            return Self::Whitespace;
        };
        if ch.is_whitespace() {
            Self::Whitespace
        } else if ch.is_alphanumeric() || ch == '_' {
            Self::Keyword
        } else {
            Self::Other
        }
    }

    /// The class under `W`/`B`/`E`, where any non-blank run is one word.
    fn widen(self, word: WordKind) -> Self {
        match (word, self) {
            (WordKind::Big, Self::Other) => Self::Keyword,
            _ => self,
        }
    }

    fn is_word(self) -> bool {
        matches!(self, Self::Keyword | Self::Other)
    }
}

/// One navigable cell group of a message: a grapheme, a whole reference pill,
/// or the placeholder of an empty line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stop {
    /// Body byte offset the stop starts at; what [`ViewCursor::offset`] holds.
    offset: u32,
    /// Body byte offset the stop's text ends at, for inclusive yanks.
    end: u32,
    /// Wrapped row within the message.
    row: usize,
    col: u16,
    width: u16,
    class: CharClass,
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
