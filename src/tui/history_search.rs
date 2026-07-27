use std::ops::Range;

use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    app::room::{HistoryDelta, Revision, RoomHistoryRef},
    chat_buffer::{ChatViewport, HistoryEntryId},
};

#[derive(Debug)]
struct SearchEntry {
    entry: HistoryEntryId,
    start: usize,
    end: usize,
    /// Only non-ASCII entries need a lowercase-byte to source-byte map. ASCII,
    /// overwhelmingly the common path, maps offsets directly with no allocation.
    original: Option<Vec<Range<u32>>>,
}

#[derive(Debug, Default)]
struct SearchIndex {
    /// All normalized bodies separated by a byte that cannot occur in UTF-8.
    /// Entry offsets are logical: `lower[0]` sits at [`Self::base`], so
    /// dropping evicted entries need not rewrite every offset.
    lower: Vec<u8>,
    /// Logical offset of `lower[0]`, advanced when an evicted prefix is
    /// dropped and reset whenever the buffer is physically compacted.
    base: usize,
    entries: Vec<SearchEntry>,
}

impl SearchIndex {
    /// Indexes what the view displays rather than what history retains: a
    /// `/clear` hides rows from the buffer, and offering them as matches would
    /// scroll the user to a row that is not there.
    fn build(chat: &ChatViewport, history: &RoomHistoryRef<'_>) -> Self {
        let mut index = Self::default();
        let ids = chat.entry_ids();
        index.entries.reserve(ids.len());
        for id in ids {
            index.append_entry(history, id);
        }
        index
    }

    /// Applies the canonical changes since `applied`, when every one of them
    /// leaves the already-normalized bytes of surviving entries valid.
    ///
    /// Only a tail append and a front eviction qualify: the index is one flat
    /// byte buffer addressed by per-entry offsets, so anything that rewrites a
    /// body or moves an interior entry invalidates those offsets. `None` asks
    /// the caller to rebuild.
    fn extend(&mut self, history: &RoomHistoryRef<'_>, applied: Revision) -> Option<()> {
        let deltas = history.deltas_since(applied)?;
        let mut evicted: Option<rpc::ids::MessageId> = None;
        let mut appended = Vec::new();
        for delta in deltas {
            match *delta {
                HistoryDelta::Appended(message_id) => {
                    appended.push(HistoryEntryId::Message(message_id));
                }
                HistoryDelta::NoticeAdded(key) => appended.push(HistoryEntryId::Notice(key)),
                HistoryDelta::EvictedThrough(watermark) => {
                    evicted = Some(evicted.map_or(watermark, |seen| seen.max(watermark)));
                }
                // Marks live in the heading; the indexed bodies are untouched.
                HistoryDelta::Relabelled => {}
                _ => return None,
            }
        }
        // Retention only ever reaches a prefix, so the watermark is below
        // everything appended in the same batch and the order is immaterial.
        if let Some(watermark) = evicted {
            self.drop_front(watermark);
        }
        self.entries.reserve(appended.len());
        for id in appended {
            self.append_entry(history, id);
        }
        Some(())
    }

    /// Drops the leading entries retention has evicted.
    ///
    /// The normalized bytes of the survivors are never rewritten: the dropped
    /// prefix is simply left unowned at the front of `lower`, and is discarded
    /// physically only once it outweighs the live remainder. That amortizes to
    /// a constant per evicted message.
    fn drop_front(&mut self, watermark: rpc::ids::MessageId) {
        let kept = self
            .entries
            .iter()
            .position(
                |entry| !matches!(entry.entry, HistoryEntryId::Message(id) if id <= watermark),
            )
            .unwrap_or(self.entries.len());
        if kept == 0 {
            return;
        }
        self.entries.drain(..kept);
        let Some(live) = self.entries.first().map(|entry| entry.start) else {
            self.lower.clear();
            self.base = 0;
            return;
        };
        let unowned = live - self.base;
        if unowned * 2 >= self.lower.len() {
            self.lower.drain(..unowned);
            self.base = live;
        }
    }

    fn append_entry(&mut self, history: &RoomHistoryRef<'_>, id: HistoryEntryId) {
        let Some(record) = history.record(id) else {
            return;
        };
        let start = self.base + self.lower.len();
        let original = if record.body.is_ascii() {
            self.lower
                .extend(record.body.bytes().map(|byte| byte.to_ascii_lowercase()));
            None
        } else {
            let mut map = Vec::with_capacity(record.body.len());
            for (source_start, ch) in record.body.char_indices() {
                let source_end = source_start + ch.len_utf8();
                for lowered in ch.to_lowercase() {
                    let mut bytes = [0; 4];
                    let lowered = lowered.encode_utf8(&mut bytes).as_bytes();
                    self.lower.extend_from_slice(lowered);
                    map.extend((0..lowered.len()).map(|_| source_start as u32..source_end as u32));
                }
            }
            Some(map)
        };
        let end = self.base + self.lower.len();
        self.entries.push(SearchEntry {
            entry: id,
            start,
            end,
            original,
        });
        self.lower.push(0xff);
    }

    fn search(&self, query: &str, out: &mut Vec<HistoryMatch>) {
        out.clear();
        let query = query.to_lowercase();
        let mut chunks = query.split_whitespace();
        let Some(first) = chunks.next() else {
            out.extend(
                self.entries
                    .iter()
                    .enumerate()
                    .map(|(ordinal, entry)| HistoryMatch {
                        entry: entry.entry,
                        ranges: Vec::new(),
                        ordinal,
                    }),
            );
            return;
        };
        let rest: Vec<_> = chunks
            .map(|chunk| memchr::memmem::Finder::new(chunk.as_bytes()))
            .collect();
        let finder = memchr::memmem::Finder::new(first.as_bytes());
        // `search_from` indexes `lower`; entry offsets are logical, so the two
        // are converted through `base` at each comparison.
        let mut search_from = self
            .entries
            .first()
            .map_or(0, |entry| entry.start - self.base);
        let mut entry_index = 0usize;
        while search_from < self.lower.len() {
            let Some(relative) = finder.find(&self.lower[search_from..]) else {
                break;
            };
            let found = self.base + search_from + relative;
            while entry_index < self.entries.len() && self.entries[entry_index].end <= found {
                entry_index += 1;
            }
            let Some(entry) = self.entries.get(entry_index) else {
                break;
            };
            let first_end = found + first.len();
            if found < entry.start || first_end > entry.end {
                search_from = found + 1 - self.base;
                continue;
            }

            let mut normalized_ranges = vec![found - entry.start..first_end - entry.start];
            let mut chunk_from = first_end;
            let mut matched = true;
            for chunk in &rest {
                let Some(relative) =
                    chunk.find(&self.lower[chunk_from - self.base..entry.end - self.base])
                else {
                    matched = false;
                    break;
                };
                let start = chunk_from + relative;
                let end = start + chunk.needle().len();
                normalized_ranges.push(start - entry.start..end - entry.start);
                chunk_from = end;
            }
            if matched {
                out.push(HistoryMatch {
                    entry: entry.entry,
                    ranges: normalized_ranges
                        .into_iter()
                        .filter_map(|range| entry.original_range(range))
                        .collect(),
                    ordinal: entry_index,
                });
                search_from = entry.end.saturating_add(1) - self.base;
            } else {
                search_from = found + 1 - self.base;
            }
        }
    }
}

impl SearchEntry {
    fn original_range(&self, range: Range<usize>) -> Option<Range<u32>> {
        match &self.original {
            Some(map) => Some(map.get(range.start)?.start..map.get(range.end.checked_sub(1)?)?.end),
            None => Some(range.start as u32..range.end as u32),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HistoryMatch {
    pub(crate) entry: HistoryEntryId,
    pub(crate) ranges: Vec<Range<u32>>,
    ordinal: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchAction {
    Continue,
    Close,
}

/// Per-view query and selection state. The index retains only normalized text,
/// source-offset maps, and stable canonical ids; message payload ownership
/// remains in room history.
#[derive(Debug)]
pub(crate) struct HistorySearch {
    query: String,
    index: SearchIndex,
    matches: Vec<HistoryMatch>,
    selected: usize,
    list_offset: usize,
    identity: Option<IndexIdentity>,
    anchor: Option<HistoryEntryId>,
    /// Ordinal of the last resolved selection, so a selection that vanishes
    /// falls back to its neighbourhood rather than to the oldest match.
    selected_ordinal: usize,
    #[cfg(test)]
    full_rebuilds: usize,
}

/// What the built index is a projection of. Message ids restart at one in every
/// room, so the room is part of the identity: without it an index built for one
/// room could alias same-numbered messages in another and hand render byte
/// ranges into a body they never described.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IndexIdentity {
    room_id: Option<rpc::ids::RoomId>,
    generation: u64,
    revision: Revision,
    clear_generation: u64,
}

impl IndexIdentity {
    fn of(chat: &ChatViewport, history: &RoomHistoryRef<'_>) -> Self {
        Self {
            room_id: history.room_id(),
            generation: history.generation(),
            revision: history.revision(),
            clear_generation: chat.clear_generation(),
        }
    }

    /// Whether the two identities describe the same indexed id space, so the
    /// difference between them is describable by canonical deltas alone. A
    /// different room, generation or `/clear` changes which ids the view shows
    /// at all, which no delta reports.
    fn same_space(&self, next: &Self) -> bool {
        self.room_id == next.room_id
            && self.generation == next.generation
            && self.clear_generation == next.clear_generation
    }
}

impl HistorySearch {
    pub(crate) fn new(chat: &ChatViewport, history: &RoomHistoryRef<'_>) -> Self {
        let anchor = chat
            .cursor()
            .map(|cursor| cursor.entry)
            .or_else(|| history.tail_message_id().map(HistoryEntryId::Message));
        let mut state = Self {
            query: String::new(),
            index: SearchIndex::default(),
            matches: Vec::new(),
            selected: 0,
            list_offset: 0,
            identity: None,
            anchor,
            selected_ordinal: 0,
            #[cfg(test)]
            full_rebuilds: 0,
        };
        state.sync(chat, history);
        state
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    pub(crate) fn matches(&self) -> &[HistoryMatch] {
        &self.matches
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected
    }

    pub(crate) fn list_offset(&self) -> usize {
        self.list_offset
    }

    pub(crate) fn selected_match(&self) -> Option<&HistoryMatch> {
        self.matches.get(self.selected)
    }

    pub(crate) fn selected_entry(&self) -> Option<HistoryEntryId> {
        self.selected_match().map(|found| found.entry)
    }

    pub(crate) fn set_visible_rows(&mut self, rows: usize) {
        if rows == 0 || self.matches.is_empty() {
            self.list_offset = 0;
            return;
        }
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if self.selected >= self.list_offset + rows {
            self.list_offset = self.selected + 1 - rows;
        }
        self.list_offset = self
            .list_offset
            .min(self.matches.len().saturating_sub(rows));
    }

    pub(crate) fn sync(&mut self, chat: &ChatViewport, history: &RoomHistoryRef<'_>) {
        let next = IndexIdentity::of(chat, history);
        if self.identity == Some(next) {
            return;
        }
        let nearest = self.selected_entry().or(self.anchor);
        let extended = self
            .identity
            .filter(|current| current.same_space(&next))
            .and_then(|current| self.index.extend(history, current.revision))
            .is_some();
        if !extended {
            self.index = SearchIndex::build(chat, history);
            #[cfg(test)]
            {
                self.full_rebuilds += 1;
            }
        }
        self.identity = Some(next);
        self.refresh(nearest);
    }

    pub(crate) fn process_key(
        &mut self,
        chat: &mut ChatViewport,
        history: &RoomHistoryRef<'_>,
        key: KeyEvent,
        width: u16,
        height: u16,
    ) -> SearchAction {
        if matches!(key.kind, KeyEventKind::Release) {
            return SearchAction::Continue;
        }
        self.sync(chat, history);
        let mut modifiers = key.modifiers;
        modifiers.remove(KeyModifiers::SHIFT);
        match (key.code, modifiers) {
            (KeyCode::Esc | KeyCode::Enter, KeyModifiers::NONE) => return SearchAction::Close,
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                self.move_selection(-1)
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.move_selection(1)
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                if self.query.pop().is_some() {
                    let nearest = self.selected_entry().or(self.anchor);
                    self.refresh(nearest);
                }
            }
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                if !self.query.is_empty() {
                    self.query.clear();
                    let nearest = self.selected_entry().or(self.anchor);
                    self.refresh(nearest);
                }
            }
            (KeyCode::Char(ch), KeyModifiers::NONE) if !ch.is_control() => {
                self.query.push(ch);
                let nearest = self.selected_entry().or(self.anchor);
                self.refresh(nearest);
            }
            _ => {}
        }
        self.follow_selection(chat, history, width, height);
        SearchAction::Continue
    }

    pub(crate) fn follow_selection(
        &self,
        chat: &mut ChatViewport,
        history: &RoomHistoryRef<'_>,
        width: u16,
        height: u16,
    ) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        chat.set_cursor(entry);
        chat.scroll_entry_into_view(history, entry, width.max(1), height.max(1));
    }

    pub(crate) fn repeat(
        &mut self,
        chat: &mut ChatViewport,
        history: &RoomHistoryRef<'_>,
        delta: isize,
        width: u16,
        height: u16,
    ) {
        self.sync(chat, history);
        if !self.matches.is_empty() {
            self.selected =
                (self.selected as isize + delta).rem_euclid(self.matches.len() as isize) as usize;
        }
        self.follow_selection(chat, history, width, height);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected =
            (self.selected as isize + delta).clamp(0, self.matches.len() as isize - 1) as usize;
    }

    fn refresh(&mut self, nearest: Option<HistoryEntryId>) {
        // An entry that no longer exists — evicted, tombstoned, or cleared —
        // leaves its last known ordinal as the best estimate of where the
        // reader was. Falling back to zero would throw them to the very oldest
        // match instead of the nearest surviving one.
        let nearest = nearest
            .and_then(|nearest| {
                self.index
                    .entries
                    .iter()
                    .position(|entry| entry.entry == nearest)
            })
            .unwrap_or_else(|| {
                self.selected_ordinal
                    .min(self.index.entries.len().saturating_sub(1))
            });
        self.index.search(&self.query, &mut self.matches);
        self.selected = self
            .matches
            .partition_point(|found| found.ordinal <= nearest)
            .saturating_sub(1)
            .min(self.matches.len().saturating_sub(1));
        self.selected_ordinal = self.selected_match().map_or(nearest, |found| found.ordinal);
        self.list_offset = 0;
    }
}

#[cfg(test)]
fn match_ranges(body: &str, query: &str) -> Option<Vec<Range<u32>>> {
    let lower_query = query.to_lowercase();
    let mut chunks = lower_query.split_whitespace().map(str::to_string);
    let Some(first) = chunks.next() else {
        return Some(Vec::new());
    };
    let (lower, map) = normalized(body);
    let mut from = 0usize;
    let mut ranges = Vec::new();
    for chunk in std::iter::once(first).chain(chunks) {
        let relative = memchr::memmem::find(&lower[from..], chunk.as_bytes())?;
        let start = from + relative;
        let end = start + chunk.len();
        ranges.push(match &map {
            Some(map) => map.get(start)?.start..map.get(end.checked_sub(1)?)?.end,
            None => start as u32..end as u32,
        });
        from = end;
    }
    Some(ranges)
}

#[cfg(test)]
fn normalized(body: &str) -> (Vec<u8>, Option<Vec<Range<u32>>>) {
    if body.is_ascii() {
        return (
            body.bytes().map(|byte| byte.to_ascii_lowercase()).collect(),
            None,
        );
    }
    let mut lower = Vec::with_capacity(body.len());
    let mut map = Vec::with_capacity(body.len());
    for (source_start, ch) in body.char_indices() {
        let source_end = source_start + ch.len_utf8();
        for lowered in ch.to_lowercase() {
            let mut bytes = [0; 4];
            let lowered = lowered.encode_utf8(&mut bytes).as_bytes();
            lower.extend_from_slice(lowered);
            map.extend((0..lowered.len()).map(|_| source_start as u32..source_end as u32));
        }
    }
    (lower, Some(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::room::RoomHistoryFixture, theme::SyntaxTheme};

    #[test]
    fn whitespace_is_an_ordered_wildcard() {
        assert_eq!(match_ranges("a Boat afloat", "b t"), Some(vec![2..3, 5..6]));
        assert_eq!(match_ranges("a Boat afloat", "t b"), None);
    }

    #[test]
    fn matching_is_case_insensitive_and_preserves_unicode_offsets() {
        assert_eq!(match_ranges("CAFÉ Straße", "café"), Some(vec![0..5]));
        assert_eq!(match_ranges("CAFÉ Straße", "straße"), Some(vec![6..13]));
    }

    #[test]
    fn tail_append_extends_the_normalized_index() {
        let mut history = RoomHistoryFixture::new();
        history.push(1, "alice", "first");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        let mut search = HistorySearch::new(&viewport, &history.history());
        let rebuilds = search.full_rebuilds;

        history.push(2, "alice", "needle");
        viewport.reconcile(&history.history());
        search.sync(&viewport, &history.history());
        search.query = "needle".to_string();
        search.refresh(None);

        assert_eq!(search.full_rebuilds, rebuilds);
        assert_eq!(
            search.selected_entry(),
            Some(HistoryEntryId::Message(rpc::ids::MessageId(2)))
        );

        history.edit(2, "changed");
        viewport.reconcile(&history.history());
        search.sync(&viewport, &history.history());
        assert_eq!(search.full_rebuilds, rebuilds + 1);
    }

    #[test]
    fn eviction_drops_the_index_prefix_without_rebuilding() {
        let mut history = RoomHistoryFixture::new();
        for id in 1..=40 {
            history.push(id, "alice", &format!("body {id} needle"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        let mut search = HistorySearch::new(&viewport, &history.history());
        search.query = "needle".to_string();
        search.refresh(None);
        let rebuilds = search.full_rebuilds;
        assert_eq!(search.matches().len(), 40);

        history.evict(30);
        viewport.reconcile(&history.history());
        search.sync(&viewport, &history.history());

        assert_eq!(search.full_rebuilds, rebuilds);
        assert_eq!(search.matches().len(), 10);
        assert_eq!(
            search.matches().first().map(|found| found.entry),
            Some(HistoryEntryId::Message(rpc::ids::MessageId(31)))
        );
    }

    #[test]
    fn index_excludes_cleared_scrollback() {
        let mut history = RoomHistoryFixture::new();
        history.push(1, "alice", "cleared needle");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        viewport.clear_scrollback();
        history.push(2, "alice", "kept needle");
        viewport.reconcile(&history.history());

        let mut search = HistorySearch::new(&viewport, &history.history());
        search.query = "needle".to_string();
        search.refresh(None);

        // Selecting a row the buffer no longer displays would be a no-op jump.
        assert_eq!(
            search
                .matches()
                .iter()
                .map(|found| found.entry)
                .collect::<Vec<_>>(),
            vec![HistoryEntryId::Message(rpc::ids::MessageId(2))]
        );
    }

    #[test]
    fn clearing_scrollback_reindexes_an_open_search() {
        let mut history = RoomHistoryFixture::new();
        history.push(1, "alice", "needle one");
        history.push(2, "alice", "needle two");
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        let mut search = HistorySearch::new(&viewport, &history.history());
        search.query = "needle".to_string();
        search.refresh(None);
        assert_eq!(search.matches().len(), 2);

        viewport.clear_scrollback();
        search.sync(&viewport, &history.history());

        assert!(search.matches().is_empty());
    }

    #[test]
    fn vanished_selection_falls_back_to_its_own_neighbourhood() {
        let mut history = RoomHistoryFixture::new();
        for id in 1..=6 {
            history.push(id, "alice", &format!("needle {id}"));
        }
        let mut viewport = ChatViewport::new(SyntaxTheme::default());
        viewport.reconcile(&history.history());
        let mut search = HistorySearch::new(&viewport, &history.history());
        search.query = "needle".to_string();
        search.refresh(None);
        search.selected = 4;
        search.selected_ordinal = 4;

        // The selected entry disappears while the rest of the index survives.
        history.remove(5);
        viewport.reconcile(&history.history());
        search.sync(&viewport, &history.history());

        assert_eq!(
            search.selected_entry(),
            Some(HistoryEntryId::Message(rpc::ids::MessageId(6))),
            "a vanished selection lands beside where it was, not at the oldest match"
        );
    }
}
