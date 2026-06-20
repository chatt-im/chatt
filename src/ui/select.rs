use extui::{
    Buffer, Rect,
    event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
};

use crate::fuzzy::fuzzy_score;

pub trait SelectableItem {
    fn search_text(&self) -> &str;

    fn rank(&self) -> i32 {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectEntry {
    pub item_index: usize,
    pub score: i32,
}

#[derive(Clone, Debug, Default)]
pub struct FuzzySelect {
    query: String,
    entries: Vec<SelectEntry>,
    cursor: usize,
    offset: usize,
    selected_item: Option<usize>,
}

impl FuzzySelect {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn filtered_len(&self) -> usize {
        self.entries.len()
    }

    pub fn current_item_index(&self) -> Option<usize> {
        self.entries.get(self.cursor).map(|entry| entry.item_index)
    }

    pub fn clear_query(&mut self) -> bool {
        if self.query.is_empty() {
            return false;
        }
        self.query.clear();
        true
    }

    pub fn set_selected_item(&mut self, item_index: Option<usize>) {
        self.selected_item = item_index;
        if let Some(item_index) = item_index {
            if let Some(cursor) = self
                .entries
                .iter()
                .position(|entry| entry.item_index == item_index)
            {
                self.cursor = cursor;
            }
        } else {
            self.cursor = 0;
        }
    }

    pub fn refresh<T: SelectableItem>(&mut self, items: &[T]) {
        self.entries.clear();

        if self.query.is_empty() {
            self.entries.extend(
                items
                    .iter()
                    .enumerate()
                    .map(|(item_index, item)| SelectEntry {
                        item_index,
                        score: item.rank(),
                    }),
            );
        } else {
            self.entries
                .extend(items.iter().enumerate().filter_map(|(item_index, item)| {
                    fuzzy_score(&self.query, item.search_text()).map(|score| SelectEntry {
                        item_index,
                        score: score + item.rank(),
                    })
                }));
        }

        self.entries.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.item_index.cmp(&b.item_index))
        });
        self.reconcile_cursor();
    }

    pub fn edit_query(&mut self, key: KeyEvent) -> bool {
        if matches!(key.kind, KeyEventKind::Release) {
            return false;
        }

        let mut modifiers = key.modifiers;
        modifiers.remove(KeyModifiers::SHIFT);
        if !modifiers.is_empty() {
            return false;
        }

        match key.code {
            KeyCode::Char(ch) if !ch.is_control() => {
                self.query.push(ch);
                true
            }
            KeyCode::Backspace => self.query.pop().is_some(),
            KeyCode::Delete => self.clear_query(),
            _ => false,
        }
    }

    pub fn move_selection(&mut self, delta: isize) -> Option<usize> {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.selected_item = None;
            return None;
        }
        self.cursor =
            (self.cursor as isize + delta).rem_euclid(self.entries.len() as isize) as usize;
        let item_index = self.entries[self.cursor].item_index;
        self.selected_item = Some(item_index);
        Some(item_index)
    }

    pub fn render<F>(&mut self, area: Rect, item_height: u16, buf: &mut Buffer, mut render_item: F)
    where
        F: FnMut(usize, usize, bool, Rect, &mut Buffer),
    {
        let item_height = item_height.max(1);
        let visible_rows = area.h as usize / item_height as usize;
        self.ensure_visible(visible_rows);

        if visible_rows == 0 {
            return;
        }

        let end = (self.offset + visible_rows).min(self.entries.len());
        let mut rows = area;
        for (visible_index, entry_index) in (self.offset..end).enumerate() {
            let row = rows.take_top(item_height as i32);
            let entry = self.entries[entry_index];
            render_item(
                visible_index,
                entry.item_index,
                entry_index == self.cursor,
                row,
                buf,
            );
        }
    }

    fn reconcile_cursor(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.offset = 0;
            self.selected_item = None;
            return;
        }

        if let Some(selected_item) = self.selected_item
            && let Some(cursor) = self
                .entries
                .iter()
                .position(|entry| entry.item_index == selected_item)
        {
            self.cursor = cursor;
            return;
        }

        self.cursor = self.cursor.min(self.entries.len() - 1);
        self.selected_item = self.current_item_index();
    }

    fn ensure_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 || self.entries.is_empty() {
            self.offset = 0;
            return;
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + visible_rows {
            self.offset = self.cursor + 1 - visible_rows;
        }
        self.offset = self
            .offset
            .min(self.entries.len().saturating_sub(visible_rows));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Item {
        text: &'static str,
        rank: i32,
    }

    impl SelectableItem for Item {
        fn search_text(&self) -> &str {
            self.text
        }

        fn rank(&self) -> i32 {
            self.rank
        }
    }

    #[test]
    fn rank_orders_empty_query() {
        let items = [
            Item {
                text: "monitor",
                rank: -100,
            },
            Item {
                text: "microphone",
                rank: 100,
            },
        ];
        let mut select = FuzzySelect::default();
        select.refresh(&items);

        assert_eq!(select.current_item_index(), Some(1));
    }

    #[test]
    fn search_filters_items() {
        let items = [
            Item {
                text: "USB Microphone",
                rank: 0,
            },
            Item {
                text: "Monitor of Output",
                rank: 0,
            },
        ];
        let mut select = FuzzySelect::default();
        select.query = "mic".to_string();
        select.refresh(&items);

        assert_eq!(select.filtered_len(), 1);
        assert_eq!(select.current_item_index(), Some(0));
    }
}
