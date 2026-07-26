use std::ops::Range;

use extui::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::{
    app::room::RoomHistoryRef,
    chat_buffer::{ChatViewport, HistoryEntryId},
};

#[derive(Clone, Debug)]
pub(crate) struct HistoryMatch {
    pub(crate) entry: HistoryEntryId,
    pub(crate) ranges: Vec<Range<u32>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchAction {
    Continue,
    Close,
}

/// Per-view query and selection state. Search text is derived directly from
/// canonical records and is never retained as another room-history model.
#[derive(Debug)]
pub(crate) struct HistorySearch {
    query: String,
    matches: Vec<HistoryMatch>,
    selected: usize,
    list_offset: usize,
    indexed_generation: u64,
    indexed_revision: u64,
    anchor: Option<HistoryEntryId>,
}

impl HistorySearch {
    pub(crate) fn new(chat: &ChatViewport, history: &RoomHistoryRef<'_>) -> Self {
        let anchor = chat
            .cursor()
            .map(|cursor| cursor.entry)
            .or_else(|| history.entry_ids().last().copied());
        let mut state = Self {
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            list_offset: 0,
            indexed_generation: u64::MAX,
            indexed_revision: u64::MAX,
            anchor,
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
        self.indexed_generation = history.generation();
        self.indexed_revision = history.order_revision();
        self.refresh(history, self.selected_entry().or(self.anchor));
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
                    self.refresh(history, self.selected_entry().or(self.anchor));
                }
            }
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.refresh(history, self.selected_entry().or(self.anchor));
                }
            }
            (KeyCode::Char(ch), KeyModifiers::NONE) if !ch.is_control() => {
                self.query.push(ch);
                self.refresh(history, self.selected_entry().or(self.anchor));
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

    fn refresh(&mut self, history: &RoomHistoryRef<'_>, nearest: Option<HistoryEntryId>) {
        self.matches.clear();
        let entries = history.entry_ids();
        let nearest = nearest
            .and_then(|nearest| entries.iter().position(|id| *id == nearest))
            .unwrap_or(0);
        self.selected = 0;
        for (index, id) in entries.into_iter().enumerate() {
            let Some(record) = history.record(id) else {
                continue;
            };
            if let Some(ranges) = match_ranges(record.body, &self.query) {
                if index <= nearest {
                    self.selected = self.matches.len();
                }
                self.matches.push(HistoryMatch { entry: id, ranges });
            }
        }
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        self.list_offset = 0;
    }
}

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
}
