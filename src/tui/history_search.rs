use std::ops::Range;

use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::chat_buffer::VirtualChatBuffer;

#[derive(Clone, Debug)]
struct SearchEntry {
    message: usize,
    start: usize,
    end: usize,
    /// Only non-ASCII entries need a lowercase-byte to source-byte map. ASCII,
    /// overwhelmingly the common path, maps offsets directly with no allocation.
    original: Option<Vec<Range<u32>>>,
}

#[derive(Debug, Default)]
struct SearchIndex {
    /// All normalized bodies separated by a byte that cannot occur in UTF-8.
    /// This is the same cache-coherent layout used by devsm's log search.
    lower: Vec<u8>,
    entries: Vec<SearchEntry>,
}

impl SearchIndex {
    fn build(chat: &VirtualChatBuffer) -> Self {
        let mut index = Self {
            lower: Vec::with_capacity(chat.len().saturating_mul(128)),
            entries: Vec::with_capacity(chat.len()),
        };
        index.append(chat, 0);
        index
    }

    fn append(&mut self, chat: &VirtualChatBuffer, from: usize) {
        self.entries.reserve(chat.len().saturating_sub(from));
        for message in from..chat.len() {
            let body = &chat.message(message).body;
            let start = self.lower.len();
            let original = if body.is_ascii() {
                self.lower
                    .extend(body.bytes().map(|byte| byte.to_ascii_lowercase()));
                None
            } else {
                let mut map = Vec::with_capacity(body.len());
                for (source_start, ch) in body.char_indices() {
                    let source_end = source_start + ch.len_utf8();
                    for lowered in ch.to_lowercase() {
                        let mut bytes = [0; 4];
                        let lowered = lowered.encode_utf8(&mut bytes).as_bytes();
                        self.lower.extend_from_slice(lowered);
                        map.extend(
                            (0..lowered.len()).map(|_| source_start as u32..source_end as u32),
                        );
                    }
                }
                Some(map)
            };
            let end = self.lower.len();
            self.entries.push(SearchEntry {
                message,
                start,
                end,
                original,
            });
            self.lower.push(0xff);
        }
    }

    fn search(&self, query: &str, out: &mut Vec<HistoryMatch>) {
        out.clear();
        let query = query.to_lowercase();
        let mut chunks = query.split_whitespace();
        let Some(first) = chunks.next() else {
            out.extend(self.entries.iter().map(|entry| HistoryMatch {
                message: entry.message,
                ranges: Vec::new(),
            }));
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
                    message: entry.message,
                    ranges: normalized_ranges
                        .into_iter()
                        .filter_map(|range| entry.original_range(range))
                        .collect(),
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
    pub(crate) message: usize,
    pub(crate) ranges: Vec<Range<u32>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchAction {
    Continue,
    Close,
}

/// Cache-coherent, per-view history search. Lowercasing is paid only when the
/// chat changes; editing the query scans already-normalized contiguous strings.
#[derive(Debug)]
pub(crate) struct HistorySearch {
    query: String,
    index: SearchIndex,
    matches: Vec<HistoryMatch>,
    selected: usize,
    list_offset: usize,
    indexed_revision: u64,
    indexed_reindex_revision: u64,
    anchor_message: usize,
}

impl HistorySearch {
    pub(crate) fn new(chat: &VirtualChatBuffer) -> Self {
        let anchor_message = chat
            .cursor()
            .map(|cursor| cursor.message)
            .unwrap_or_else(|| chat.len().saturating_sub(1));
        let mut state = Self {
            query: String::new(),
            index: SearchIndex::default(),
            matches: Vec::new(),
            selected: 0,
            list_offset: 0,
            indexed_revision: u64::MAX,
            indexed_reindex_revision: u64::MAX,
            anchor_message,
        };
        state.sync(chat);
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

    pub(crate) fn selected_message(&self) -> Option<usize> {
        self.selected_match().map(|found| found.message)
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

    pub(crate) fn sync(&mut self, chat: &VirtualChatBuffer) {
        if self.indexed_revision == chat.revision() {
            return;
        }
        let selected_message = self.selected_message().unwrap_or(self.anchor_message);
        if self.indexed_reindex_revision == chat.reindex_revision()
            && self.index.entries.len() <= chat.len()
        {
            self.index.append(chat, self.index.entries.len());
        } else {
            self.index = SearchIndex::build(chat);
        }
        self.indexed_revision = chat.revision();
        self.indexed_reindex_revision = chat.reindex_revision();
        self.refresh(selected_message);
    }

    pub(crate) fn process_key(
        &mut self,
        chat: &mut VirtualChatBuffer,
        key: KeyEvent,
        width: u16,
        height: u16,
    ) -> SearchAction {
        if matches!(key.kind, KeyEventKind::Release) {
            return SearchAction::Continue;
        }
        self.sync(chat);
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
                    self.refresh_selected();
                }
            }
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.refresh_selected();
                }
            }
            (KeyCode::Char(ch), KeyModifiers::NONE) if !ch.is_control() => {
                self.query.push(ch);
                self.refresh_selected();
            }
            _ => {}
        }
        self.follow_selection(chat, width, height);
        SearchAction::Continue
    }

    pub(crate) fn follow_selection(&self, chat: &mut VirtualChatBuffer, width: u16, height: u16) {
        let Some(message) = self.selected_message() else {
            return;
        };
        chat.set_cursor_to_message(message);
        chat.scroll_message_into_view(message, width.max(1), height.max(1));
    }

    pub(crate) fn repeat(
        &mut self,
        chat: &mut VirtualChatBuffer,
        delta: isize,
        width: u16,
        height: u16,
    ) {
        self.sync(chat);
        if !self.matches.is_empty() {
            self.selected =
                (self.selected as isize + delta).rem_euclid(self.matches.len() as isize) as usize;
        }
        self.follow_selection(chat, width, height);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected =
            (self.selected as isize + delta).clamp(0, self.matches.len() as isize - 1) as usize;
    }

    fn refresh_selected(&mut self) {
        let selected = self.selected_message().unwrap_or(self.anchor_message);
        self.refresh(selected);
    }

    fn refresh(&mut self, nearest: usize) {
        self.index.search(&self.query, &mut self.matches);
        self.selected = self
            .matches
            .partition_point(|found| found.message <= nearest)
            .saturating_sub(1)
            .min(self.matches.len().saturating_sub(1));
        self.list_offset = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_is_an_ordered_wildcard() {
        let mut chat = VirtualChatBuffer::new(10, Default::default());
        chat.push_notice("test", "a Boat afloat");
        let index = SearchIndex::build(&chat);
        let mut matches = Vec::new();
        index.search("b t", &mut matches);
        assert_eq!(matches[0].ranges, vec![2..3, 5..6]);
        index.search("t b", &mut matches);
        assert!(matches.is_empty());
    }

    #[test]
    fn matching_is_case_insensitive_and_preserves_unicode_offsets() {
        let mut chat = VirtualChatBuffer::new(10, Default::default());
        chat.push_notice("test", "CAFÉ Straße");
        let index = SearchIndex::build(&chat);
        let mut matches = Vec::new();
        index.search("café", &mut matches);
        assert_eq!(matches[0].ranges, vec![0..5]);
        index.search("straße", &mut matches);
        assert_eq!(matches[0].ranges, vec![6..13]);
    }
}
