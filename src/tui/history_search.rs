use std::ops::Range;

use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    app::room::RoomHistoryRef,
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
    lower: Vec<u8>,
    entries: Vec<SearchEntry>,
    tail: Option<rpc::ids::MessageId>,
}

impl SearchIndex {
    fn build(history: &RoomHistoryRef<'_>) -> Self {
        let mut index = Self::default();
        let ids = history.entry_ids();
        index.entries.reserve(ids.len());
        for id in ids {
            index.append_entry(history, id);
        }
        index.tail = history.tail_message_id();
        index
    }

    fn append_tail(&mut self, history: &RoomHistoryRef<'_>) -> bool {
        let Some(ids) = history.tail_entry_ids(self.tail) else {
            return false;
        };
        self.entries.reserve(ids.size_hint().1.unwrap_or(0));
        for id in ids {
            self.append_entry(history, id);
        }
        self.tail = history.tail_message_id();
        true
    }

    fn append_entry(&mut self, history: &RoomHistoryRef<'_>, id: HistoryEntryId) {
        let Some(record) = history.record(id) else {
            return;
        };
        let start = self.lower.len();
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
        let end = self.lower.len();
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
        let mut search_from = 0usize;
        let mut entry_index = 0usize;
        while search_from < self.lower.len() {
            let Some(relative) = finder.find(&self.lower[search_from..]) else {
                break;
            };
            let found = search_from + relative;
            while entry_index < self.entries.len() && self.entries[entry_index].end <= found {
                entry_index += 1;
            }
            let Some(entry) = self.entries.get(entry_index) else {
                break;
            };
            let first_end = found + first.len();
            if found < entry.start || first_end > entry.end {
                search_from = found + 1;
                continue;
            }

            let mut normalized_ranges = vec![found - entry.start..first_end - entry.start];
            let mut chunk_from = first_end;
            let mut matched = true;
            for chunk in &rest {
                let Some(relative) = chunk.find(&self.lower[chunk_from..entry.end]) else {
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
                search_from = entry.end.saturating_add(1);
            } else {
                search_from = found + 1;
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
    indexed_generation: u64,
    indexed_revision: u64,
    indexed_reindex_revision: u64,
    anchor: Option<HistoryEntryId>,
    #[cfg(test)]
    full_rebuilds: usize,
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
            indexed_generation: u64::MAX,
            indexed_revision: u64::MAX,
            indexed_reindex_revision: u64::MAX,
            anchor,
            #[cfg(test)]
            full_rebuilds: 0,
        };
        state.sync(history);
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

    pub(crate) fn sync(&mut self, history: &RoomHistoryRef<'_>) {
        if self.indexed_generation == history.generation()
            && self.indexed_revision == history.order_revision()
        {
            return;
        }
        let nearest = self.selected_entry().or(self.anchor);
        if self.indexed_generation == history.generation()
            && self.indexed_reindex_revision == history.reindex_revision()
            && self.index.append_tail(history)
        {
            // The normalized bytes for existing entries remain valid.
        } else {
            self.index = SearchIndex::build(history);
            #[cfg(test)]
            {
                self.full_rebuilds += 1;
            }
        }
        self.indexed_generation = history.generation();
        self.indexed_revision = history.order_revision();
        self.indexed_reindex_revision = history.reindex_revision();
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
        self.sync(history);
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
        self.sync(history);
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
        let nearest = nearest
            .and_then(|nearest| {
                self.index
                    .entries
                    .iter()
                    .position(|entry| entry.entry == nearest)
            })
            .unwrap_or(0);
        self.index.search(&self.query, &mut self.matches);
        self.selected = self
            .matches
            .partition_point(|found| found.ordinal <= nearest)
            .saturating_sub(1)
            .min(self.matches.len().saturating_sub(1));
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
        search.sync(&history.history());
        search.query = "needle".to_string();
        search.refresh(None);

        assert_eq!(search.full_rebuilds, rebuilds);
        assert_eq!(
            search.selected_entry(),
            Some(HistoryEntryId::Message(rpc::ids::MessageId(2)))
        );

        history.edit(2, "changed");
        search.sync(&history.history());
        assert_eq!(search.full_rebuilds, rebuilds + 1);
    }
}
