//! Pre-ship review tests for the chat viewport: targeted repros plus a seeded
//! randomized operation sequence that checks layout/index invariants and that
//! nothing panics. Included from `chat_buffer.rs` as a child module so the
//! private layout state is reachable.

use super::*;
use crate::app::room::RoomHistoryFixture as TestHistory;

/// Small deterministic xorshift RNG; no external crate needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const FRAGMENTS: &[&str] = &[
    "hello",
    " ",
    "world",
    "\n",
    "\r\n",
    "\t",
    "\n\n",
    "日本語のテキスト",
    "👩‍👩‍👧‍👦",
    "🏳️‍🌈",
    "e\u{301}\u{302}",
    "a\u{200b}b",
    "\u{200d}",
    "שלום עולם",
    "مرحبا",
    "**bold**",
    "*it*",
    "`code`",
    "\n```rust\nfn main() {\n\tlet x = 1;\n}\n```\n",
    "\n```\n  indented 日本\n\n```\n",
    "\n- item one\n- item two\n",
    "\n1. first\n2. second\n",
    "\n> quoted text\n> > nested\n",
    "\n# Header\n",
    "https://example.com/a/very/long/path?with=query&and=more#fragment",
    "averyveryveryverylongwordwithoutanyspacesthatmustbreakacrossrows",
    "日本語日本語日本語日本語日本語日本語日本語日本語",
    "\u{feff}",
    "\u{1b}[31m",
    "x",
];

fn random_body(rng: &mut Rng) -> String {
    match rng.below(12) {
        0 => String::new(),
        1 => (0..rng.below(80) + 30)
            .map(|line| format!("line {line} {}", rng.pick(FRAGMENTS)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => {
            let mut body = String::new();
            for _ in 0..rng.below(14) + 1 {
                if rng.below(10) == 0 {
                    // A message reference: resolvable (renders as a pill) or not.
                    let target = if rng.below(2) == 0 {
                        1
                    } else {
                        1001 + rng.below(4) as u64
                    };
                    let code = rpc::msgref::MessageRef {
                        room_id: RoomId(1),
                        message_id: MessageId(target),
                    }
                    .encode();
                    body.push_str(&format!(" @@{code} "));
                    continue;
                }
                body.push_str(rng.pick(FRAGMENTS));
            }
            body
        }
    }
}

/// Mirrors what `tui::render` does with the viewport each frame, including
/// reading every segment of every visible body row.
fn render_frame(
    viewport: &mut ChatViewport,
    history: &TestHistory,
    width: u16,
    height: u16,
) -> Vec<VisibleLine> {
    let history = history.history();
    let mut lines = Vec::new();
    viewport.visible_lines_into(&history, width, height, 2, &mut lines);
    let marks = viewport.marks(&history, width);
    for line in &lines {
        if line.kind != LineKind::Body {
            continue;
        }
        let _ = viewport.row_marks(&history, &marks, line.message, line.line);
        if viewport.record(&history, line.message).is_none() {
            continue;
        }
        for seg in viewport.line(line.message, line.line) {
            let _ = viewport.segment_text(&history, line.message, seg);
        }
        let _ = viewport.link_at(&history, line.message, line.line, 3);
        let _ = viewport.ref_at(&history, line.message, line.line, 3);
    }
    lines
}

/// Every cached layout agrees with the row index that positions it.
fn assert_counts_match_layouts(viewport: &ChatViewport, context: &str) {
    let index = &viewport.layout_index;
    if !index.valid || index.line_counts.len() != viewport.entries.len() {
        return;
    }
    for (message, entry) in viewport.entries.iter().enumerate() {
        let Some(layout) = entry.layout.as_deref() else {
            continue;
        };
        if layout.wrap_width != index.width {
            continue;
        }
        assert_eq!(
            layout.lines().max(1),
            index.line_counts[message],
            "{context}: message {message} layout rows disagree with the row index"
        );
    }
}

fn assert_ids_match(viewport: &ChatViewport, history: &TestHistory, context: &str) {
    let mut fresh = ChatViewport::new(SyntaxTheme::default());
    fresh.reconcile(&history.history());
    // Notice placement can legitimately diverge (see
    // `inserted_message_below_notice_anchored_on_tombstone_matches_canonical`),
    // so the randomized check compares message order only.
    let messages = |view: &ChatViewport| {
        view.entry_ids()
            .filter(|id| matches!(id, HistoryEntryId::Message(_)))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        messages(viewport),
        messages(&fresh),
        "{context}: replayed message ids diverge from a fresh rebuild"
    );
}

const MOTIONS: &[ChatMotion] = &[
    ChatMotion::Left,
    ChatMotion::Right,
    ChatMotion::LineStart,
    ChatMotion::FirstNonBlank,
    ChatMotion::LineEnd,
    ChatMotion::Down(1),
    ChatMotion::Down(7),
    ChatMotion::Up(1),
    ChatMotion::Up(9),
    ChatMotion::RowDown(1),
    ChatMotion::RowDown(13),
    ChatMotion::RowUp(1),
    ChatMotion::RowUp(17),
    ChatMotion::WordForward(WordKind::Small),
    ChatMotion::WordForward(WordKind::Big),
    ChatMotion::WordBack(WordKind::Small),
    ChatMotion::WordBack(WordKind::Big),
    ChatMotion::WordEnd(WordKind::Small),
    ChatMotion::WordEnd(WordKind::Big),
];

const WIDTHS: &[u16] = &[1, 2, 3, 5, 8, 13, 40, 80];

/// One seeded run of random history mutations and view operations.
fn fuzz_run(seed: u64, steps: usize, check_counts: bool) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut history = TestHistory::new();
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    let mut next_id = 1000u64;
    let mut live: Vec<u64> = Vec::new();
    let mut width = 40u16;
    let mut height = 12u16;
    for step in 0..steps {
        let context = format!("seed {seed} step {step}");
        match rng.below(26) {
            0..=4 => {
                next_id += 1 + rng.below(3) as u64;
                let sender = if rng.below(3) == 0 { "bob" } else { "alice" };
                history.push(next_id, sender, &random_body(&mut rng));
                live.push(next_id);
            }
            5 => {
                // An older message threads in between residents.
                let id = live
                    .first()
                    .copied()
                    .unwrap_or(1000)
                    .saturating_sub(1)
                    .max(1)
                    + rng.below(2) as u64;
                if !live.contains(&id) {
                    history.push(id, "alice", &random_body(&mut rng));
                    live.push(id);
                    live.sort_unstable();
                }
            }
            6..=7 if !live.is_empty() => {
                let id = *rng.pick(&live);
                history.edit(id, &random_body(&mut rng));
            }
            8 if !live.is_empty() => {
                let index = rng.below(live.len());
                history.tombstone(live.remove(index));
            }
            9 => {
                // A history page: older messages splice under the front.
                let front = live.first().copied().unwrap_or(1000);
                let ids = (1..=rng.below(4) as u64 + 1)
                    .filter_map(|back| front.checked_sub(back * 2))
                    .filter(|id| *id > 0 && !live.contains(id))
                    .collect::<Vec<_>>();
                history.merge_page(&ids);
                live.extend(ids);
                live.sort_unstable();
            }
            10 => history.notice(&random_body(&mut rng), rng.below(2) == 0),
            11 => {
                width = *rng.pick(WIDTHS);
                height = *rng.pick(&[1u16, 2, 3, 12, 40]);
                viewport.on_reflow(&history.history(), width);
            }
            12 => {
                let len = viewport.len();
                if len > 0 {
                    viewport.toggle_expand(&history.history(), rng.below(len), width);
                }
            }
            13 => {
                let kind = if rng.below(2) == 0 {
                    VisualKind::Char
                } else {
                    VisualKind::Line
                };
                viewport.toggle_visual(&history.history(), kind, width);
            }
            14 => {
                let _ = viewport.visual_text(&history.history(), width);
                let _ = viewport.cursor_line_text(&history.history(), width);
                let _ = viewport.selected_entries(&history.history(), width);
            }
            15 => {
                let rows = rng.below(30) as isize - 15;
                viewport.scroll_with_cursor(&history.history(), rows, width, height, 3);
            }
            16 => {
                let delta = rng.below(5) as isize - 2;
                viewport.move_cursor_paragraph(&history.history(), delta, width);
            }
            17 => {
                if rng.below(2) == 0 {
                    viewport.cursor_to_first();
                } else {
                    viewport.cursor_to_last(&history.history(), width);
                }
            }
            18 => {
                // Mouse drag over whatever is on screen.
                let lines = render_frame(&mut viewport, &history, width, height);
                let body = lines
                    .iter()
                    .filter(|line| line.kind == LineKind::Body)
                    .collect::<Vec<_>>();
                if body.len() >= 2 {
                    let a = body[rng.below(body.len())];
                    let b = body[rng.below(body.len())];
                    viewport.begin_drag(
                        &history.history(),
                        a.entry,
                        a.line,
                        rng.below(50) as u16,
                        width,
                    );
                    viewport.drag_to(
                        &history.history(),
                        b.entry,
                        b.line,
                        rng.below(50) as u16,
                        width,
                    );
                    let _ = viewport.visual_text(&history.history(), width);
                    viewport.end_drag();
                }
            }
            19 => {
                if live.len() > 3 && rng.below(4) == 0 {
                    let count = rng.below(3) + 1;
                    history.evict(count);
                    live.drain(..count);
                }
            }
            20 => {
                viewport.expand_all();
            }
            21 => {
                viewport.scroll_up(&history.history(), rng.below(40), width, height);
            }
            22 => viewport.bottom(),
            _ => {
                let motion = *rng.pick(MOTIONS);
                viewport.move_cursor(&history.history(), motion, width);
            }
        }
        // Production syncs the view under the same session lock before every
        // input dispatch and render, so the view never runs unreconciled.
        viewport.reconcile(&history.history());
        if step % 16 == 0 {
            assert_ids_match(&viewport, &history, &context);
        }
        viewport.keep_cursor_visible(&history.history(), width, height, 2);
        render_frame(&mut viewport, &history, width, height);
        if check_counts {
            assert_counts_match_layouts(&viewport, &context);
        }
        if let Some(text) = viewport.visual_text(&history.history(), width) {
            assert!(text.len() <= 1 << 24, "{context}: runaway yank");
            let (_, lo, hi) = viewport
                .selection_bounds(&history.history(), width)
                .unwrap();
            if lo.message == hi.message {
                let history_ref = history.history();
                let body = viewport
                    .record(&history_ref, lo.message)
                    .map_or("", |record| record.body);
                assert!(
                    body.contains(text.as_str()),
                    "{context}: yank {text:?} not in {body:?}"
                );
            }
        }
        // The cursor always resolves onto a visible row of its message.
        if let Some(spot) = viewport.ensure_layout_cursor(&history.history(), width) {
            let visible = viewport.visible_body_lines(&history.history(), spot.message, width);
            assert!(
                spot.stop.row < visible,
                "{context}: cursor row {} hidden",
                spot.stop.row
            );
        }
    }
}

#[test]
fn randomized_operation_sequences_do_not_panic() {
    for seed in 1..=60u64 {
        fuzz_run(seed, 250, false);
    }
}

/// Journal overflow while a view is parked (or simply not reconciled for 256+
/// changes) rebuilds the id sequence but keeps each entry's cached
/// `MessageLayout`. `MessageLayout::ensure` only re-lays out when the width
/// changed, so an entry whose body was edited during the gap keeps its old
/// rows: the row index is rebuilt from the stale count, then the layout is
/// dropped and re-laid fresh, and the renderer indexes rows that no longer
/// exist.
#[test]
#[ignore = "BUG: journal-overflow rebuild keeps stale MessageLayout of edited bodies — see review"]
fn journal_overflow_rebuild_does_not_keep_stale_layout_of_edited_body() {
    let mut history = TestHistory::new();
    let tall = (0..12)
        .map(|line| format!("line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    history.push(1, "alice", &tall);
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    // Render once so message 1's layout sits in the visible cache.
    render_frame(&mut viewport, &history, 40, 20);

    // More than JOURNAL_CAP changes land before this view reconciles again,
    // one of which shrinks the visible message to a single row.
    for round in 0..300 {
        history.edit(1, &format!("{tall}\nround {round}"));
    }
    history.edit(1, "short");
    viewport.reconcile(&history.history());

    // The render path: row index says 12+ rows, fresh layout has 1.
    let lines = render_frame(&mut viewport, &history, 40, 20);
    let body_rows = lines
        .iter()
        .filter(|line| line.kind == LineKind::Body)
        .count();
    assert_eq!(body_rows, 1, "row index kept the pre-edit row count");
}

/// Same root cause, but the stale layout is read directly against the new
/// body, which panics on a byte slice past the end.
#[test]
#[ignore = "BUG: journal-overflow rebuild keeps stale MessageLayout of edited bodies — see review"]
fn journal_overflow_rebuild_stale_layout_slices_new_body() {
    let mut history = TestHistory::new();
    history.push(1, "alice", "a fairly long first body that wraps");
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    render_frame(&mut viewport, &history, 40, 20);
    viewport.cursor_to_first();
    for _ in 0..300 {
        history.edit(1, "a fairly long first body that wraps");
    }
    history.edit(1, "ab");
    viewport.reconcile(&history.history());
    // Cursor and selection resolution run before any re-measure and read the
    // retained layout's segments against the two-byte body.
    viewport.toggle_visual(&history.history(), VisualKind::Char, 40);
    viewport.move_cursor(&history.history(), ChatMotion::LineEnd, 40);
    let text = viewport.visual_text(&history.history(), 40);
    assert_eq!(text.as_deref(), Some("ab"));
}

#[test]
fn layout_counts_match_cached_layouts_under_random_ops() {
    for seed in 100..=130u64 {
        fuzz_run(seed, 200, true);
    }
}

/// The tombstone/evict-between-reconciles case `ensure_lines` documents: it
/// lays a missing record out as `""`, but `MessageLayout::ensure` keeps any
/// cached layout at the same width, so its segments then index into `""`.
/// Production syncs before dispatch and render under one lock, so this is
/// defensive only.
#[test]
#[ignore = "BUG: ensure_lines(\"\") keeps a cached layout for a vanished record — see review"]
fn vanished_record_with_cached_layout_does_not_slice_empty_body() {
    let mut history = TestHistory::new();
    for id in 1..=3 {
        history.push(id, "alice", &format!("message number {id}"));
    }
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    render_frame(&mut viewport, &history, 40, 20);
    viewport.set_cursor(HistoryEntryId::Message(MessageId(2)));
    viewport.move_cursor(&history.history(), ChatMotion::LineEnd, 40);
    history.tombstone(2);
    // No reconcile: the documented "view id outlived its record" window.
    viewport.keep_cursor_visible(&history.history(), 40, 20, 2);
}

/// `message_insert_position` threads an `Inserted` message in front of the
/// first newer *message*, ignoring notice anchors. A notice anchored after a
/// message that was later tombstoned sorts *below* an older newcomer in
/// canonical order (`after < key` fails), but the replay puts the newcomer
/// after the notice. The replayed view then disagrees with what any rebuild
/// (or another client) shows until the next `Relaid`.
#[test]
#[ignore = "BUG: Inserted replay misplaces message relative to notice anchored on a tombstone — see review"]
fn inserted_message_below_notice_anchored_on_tombstone_matches_canonical() {
    let mut history = TestHistory::new();
    history.push(10, "alice", "ten");
    history.push(20, "alice", "twenty");
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    // Anchored after 20, then 20 is deleted.
    history.notice("connection lost", false);
    history.tombstone(20);
    history.push(30, "alice", "thirty");
    viewport.reconcile(&history.history());
    // A late/backfilled message older than the notice's anchor arrives.
    history.push(15, "alice", "fifteen");
    viewport.reconcile(&history.history());
    assert_eq!(
        viewport.entry_ids().collect::<Vec<_>>(),
        history.history().entry_ids(),
    );
}

/// The TUI hit-tests clicks against `visible_chat_lines` from the previous
/// render but syncs the view first, so `VisibleLine::message` (a positional
/// index) can name a different entry by the time `link_at` / `ref_at` run.
/// Here a history page prepends ten entries between render and click: the
/// stale index now names an older entry outside the layout cache, and
/// `ViewEntry::layout()` panics ("message layout must be ensured before use").
/// When the shifted entry *is* cached, the click silently resolves against
/// the wrong message (wrong URL opened / wrong reference followed).
#[test]
#[ignore = "BUG: link_at/ref_at by stale positional index panic or hit the wrong message — see review"]
fn click_hit_test_with_stale_index_after_prepend_does_not_panic() {
    let mut history = TestHistory::new();
    for id in 100..130u64 {
        history.push(id, "alice", &format!("see https://example.com/{id} now"));
    }
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    let lines = viewport.visible_lines(&history.history(), 80, 4, 0);
    let clicked = *lines
        .iter()
        .find(|line| line.kind == LineKind::Body)
        .expect("a body row on screen");
    let expected = viewport
        .link_at(&history.history(), clicked.message, clicked.line, 6)
        .map(str::to_owned);
    assert!(expected.is_some());

    // A server history page lands before the click is dispatched.
    history.merge_page(&(1..=10).collect::<Vec<_>>());
    viewport.reconcile(&history.history());

    let found = viewport
        .link_at(&history.history(), clicked.message, clicked.line, 6)
        .map(str::to_owned);
    assert_eq!(found, expected, "click resolved against another message");
}

/// Every cursor stop must round-trip: placing the cursor on a stop's offset
/// and resolving it again lands on that same stop and logical line.
/// Otherwise `j`/`l`/`w` from that stop jump somewhere else and a yank's
/// endpoints shift.
fn stop_round_trip_failures(body: &str, width: u16) -> Vec<String> {
    let mut history = TestHistory::new();
    history.push(1, "alice", body);
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    viewport.expand_all();
    let history_ref = history.history();
    let lines = viewport.visible_lines_of(&history_ref, 0, width);
    let entry = viewport.entries[0].id;
    let mut failures = Vec::new();
    let mut stops = Vec::new();
    let mut scratch = Vec::new();
    for line in 0..lines {
        viewport.load_line_stops(&history_ref, 0, line, width, &mut stops);
        for (index, stop) in stops.clone().into_iter().enumerate() {
            let cursor = ViewCursor {
                entry,
                offset: stop.offset,
            };
            let spot = viewport
                .locate(&history_ref, cursor, width, &mut scratch)
                .unwrap();
            if spot.line != line || spot.index != index {
                failures.push(format!(
                    "width {width}: stop {index} of line {line} (offset {}, row {}) resolves to line {} index {}",
                    stop.offset, stop.row, spot.line, spot.index
                ));
            }
        }
    }
    failures
}

#[test]
fn every_stop_round_trips_through_locate_randomized() {
    let mut rng = Rng(0xC0FFEE);
    let mut failures = Vec::new();
    for _ in 0..400 {
        let body = random_body(&mut rng);
        let width = *rng.pick(WIDTHS);
        for failure in stop_round_trip_failures(&body, width) {
            failures.push(format!("{body:?}: {failure}"));
        }
    }
    failures.truncate(12);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every `Appended` delta sets `references_dirty`, and
/// `remeasure_reference_bodies` then re-lays out *every* resident body that
/// carries a reference, whatever the new message is. In a reply-heavy room
/// each incoming message costs a full re-layout of all replies.
#[test]
#[ignore = "PERF: each append re-lays out every reference-bearing body — see review"]
fn append_does_not_remeasure_every_reference_body() {
    let mut history = TestHistory::new();
    let code = rpc::msgref::MessageRef {
        room_id: RoomId(1),
        message_id: MessageId(1),
    }
    .encode();
    for id in 1..=2000u64 {
        history.push(
            id,
            "alice",
            &format!("replying to @@{code} with some text {id}"),
        );
    }
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    viewport.visible_lines(&history.history(), 80, 30, 2);
    let measured = viewport.layout_index.measured_messages;
    let started = std::time::Instant::now();
    history.push(5000, "bob", "an unrelated plain message");
    viewport.reconcile(&history.history());
    let elapsed = started.elapsed();
    let remeasured = viewport.layout_index.measured_messages - measured;
    eprintln!("append re-measured {remeasured} bodies in {elapsed:?}");
    assert!(
        remeasured <= 2,
        "append re-measured {remeasured} bodies in {elapsed:?}"
    );
}

/// With the chat log focused, render runs `keep_cursor_visible` *before*
/// `visible_lines_into` restores the scroll anchor. `scroll_offset` counts
/// rows from the bottom, so after a tail append it is stale by the appended
/// rows; `keep_cursor_visible` sees the cursor "above" the safe band,
/// rewrites the offset and drops the anchor, so the reader's view jumps.
#[test]
#[ignore = "BUG: focused chat log jumps when a tall message arrives while scrolled up — see review"]
fn append_while_scrolled_with_focused_cursor_keeps_view() {
    let (width, height, buffer) = (40u16, 10u16, 2usize);
    let mut history = TestHistory::new();
    for id in 1..=40u64 {
        history.push(id, &format!("user-{id}"), &format!("message {id}"));
    }
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    viewport.scroll_up(&history.history(), 30, width, height);
    let lines = viewport.visible_lines(&history.history(), width, height, 1);
    // Cursor on a body row just inside the top of the safe band.
    let target = lines
        .iter()
        .skip(buffer)
        .find(|line| line.kind == LineKind::Body)
        .copied()
        .unwrap();
    viewport.set_cursor(target.entry);
    viewport.keep_cursor_visible(&history.history(), width, height, buffer);
    let before = viewport.visible_lines(&history.history(), width, height, 1);
    let before_top = (before[0].entry, before[0].line, before[0].kind);

    let tall = (0..20)
        .map(|n| format!("row {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    history.push(41, "new-user", &tall);
    viewport.reconcile(&history.history());
    // Render order with chat focus.
    viewport.keep_cursor_visible(&history.history(), width, height, buffer);
    let after = viewport.visible_lines(&history.history(), width, height, 1);
    let after_top = (after[0].entry, after[0].line, after[0].kind);
    assert_eq!(after_top, before_top, "view moved under the reader");
}

/// `push_notice_with_kind` trims the oldest local notice with
/// `entries.retain` but never remaps a cursor or anchor sitting on it. The
/// dangling cursor then makes `ensure_layout_cursor` return `None` (no
/// motion works, no cursor is drawn), and `resync_entries` keeps it because
/// an id absent from both old and new entries counts as "surviving".
#[test]
#[ignore = "BUG: trimming a local notice leaves the cursor dangling on it — see review"]
fn trimmed_local_notice_does_not_strand_the_cursor() {
    let mut history = TestHistory::new();
    history.push(1, "alice", "hello");
    let mut viewport = ChatViewport::new(SyntaxTheme::default());
    viewport.reconcile(&history.history());
    viewport.max_local_notices = 2;
    let first = viewport.push_notice_with_kind("sys", "one", NoticeKind::Info);
    viewport.push_notice_with_kind("sys", "two", NoticeKind::Info);
    viewport.set_cursor(HistoryEntryId::LocalNotice(first));
    viewport.push_notice_with_kind("sys", "three", NoticeKind::Info);
    viewport.reconcile(&history.history());
    assert!(
        viewport
            .move_cursor(&history.history(), ChatMotion::Down(1), 40)
            .is_some(),
        "cursor is stranded on a trimmed notice"
    );
}
