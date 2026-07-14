use extui::{
    Buffer, Rect,
    event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind},
};
use extui_editor::{Editor, Mode, bindings as editor_bindings};
use unicode_width::UnicodeWidthChar;

use crate::{config::FormBindings, theme::Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormFieldKind {
    Text,
    AdjustableText,
    Toggle,
    Choice,
    Select,
    Action,
    Disabled,
    #[allow(dead_code)]
    Static,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormAction {
    None,
    Cancel,
    Activate,
    ActivateNextInsert,
    MoveFocus(isize),
    Adjust(isize),
    FocusMoved,
    TextChanged,
    Scrolled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormMouseIntent<F> {
    None,
    Activate(F),
    Adjust(F, isize),
    Text(F, Rect, u16),
    PickerItem(F, usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormEvent<F> {
    pub(crate) commit: Option<(F, String)>,
    pub(crate) action: FormAction,
}

impl<F> FormEvent<F> {
    fn action(action: FormAction) -> Self {
        Self {
            commit: None,
            action,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormMouseEvent<F> {
    pub(crate) commit: Option<(F, String)>,
    pub(crate) intent: FormMouseIntent<F>,
    pub(crate) action: FormAction,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FormRow {
    pub(crate) rect: Option<Rect>,
    pub(crate) virtual_y: u16,
    pub(crate) height: u16,
}

#[derive(Clone, Copy, Debug)]
struct FieldEntry<F> {
    field: F,
    virtual_y: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug)]
enum HitKind {
    Focus,
    Activate,
    Adjust(isize),
    Text,
    PickerItem(usize),
}

#[derive(Clone, Copy, Debug)]
struct HitEntry<F> {
    field: F,
    rect: Rect,
    kind: HitKind,
}

pub(crate) struct FormState<F> {
    focus: F,
    order: Vec<F>,
    frame_order: Vec<F>,
    fields: Vec<FieldEntry<F>>,
    hits: Vec<HitEntry<F>>,
    viewport: Rect,
    cursor_y: u16,
    scroll: u16,
    content_height: u16,
    bindings: FormBindings,
    editor: Editor,
    active_text: Option<F>,
    focused_kind: FormFieldKind,
}

impl<F: Copy + Eq> FormState<F> {
    pub(crate) fn new(focus: F, bindings: FormBindings) -> Self {
        Self {
            focus,
            order: Vec::new(),
            frame_order: Vec::new(),
            fields: Vec::new(),
            hits: Vec::new(),
            viewport: Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            cursor_y: 0,
            scroll: 0,
            content_height: 0,
            bindings,
            editor: new_form_editor(bindings),
            active_text: None,
            focused_kind: FormFieldKind::Action,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_order(
        focus: F,
        bindings: FormBindings,
        order: impl IntoIterator<Item = F>,
    ) -> Self {
        let mut state = Self::new(focus, bindings);
        state.order.extend(order);
        state
    }

    pub(crate) fn focus(&self) -> F {
        self.focus
    }

    /// Kind of the focused field as recorded by the most recent registration
    /// pass. Lets immediate-mode callers feed [`handle_key`](Self::handle_key)
    /// the focused kind without a separate per-field lookup table.
    pub(crate) fn focused_kind(&self) -> FormFieldKind {
        self.focused_kind
    }

    /// Viewport set by the most recent [`begin_frame`](Self::begin_frame). A
    /// headless logic pass replays the layout against the last rendered
    /// viewport so field ids and rects match what the user sees.
    pub(crate) fn viewport(&self) -> Rect {
        self.viewport
    }

    #[cfg(test)]
    pub(crate) fn scroll(&self) -> u16 {
        self.scroll
    }

    pub(crate) fn active_text(&self) -> Option<F> {
        self.active_text
    }

    /// Keeps Vim editing continuous after a commit-and-advance operation.
    pub(crate) fn enter_insert_mode(&mut self) {
        if self.active_text.is_some() {
            self.editor.enter_insert_mode();
        }
    }

    #[cfg(test)]
    pub(crate) fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub(crate) fn set_bindings(&mut self, bindings: FormBindings) -> Option<(F, String)> {
        if self.bindings == bindings {
            return None;
        }
        let commit = self.clear_text();
        self.bindings = bindings;
        self.editor = new_form_editor(bindings);
        commit
    }

    pub(crate) fn begin_frame(&mut self, viewport: Rect) {
        self.viewport = viewport;
        self.cursor_y = 0;
        self.frame_order.clear();
        self.fields.clear();
        self.hits.clear();
    }

    pub(crate) fn next_row(&mut self, height: u16) -> FormRow {
        let height = height.max(1);
        let row = FormRow {
            rect: visible_row(self.viewport, self.scroll, self.cursor_y, height),
            virtual_y: self.cursor_y,
            height,
        };
        self.cursor_y = self.cursor_y.saturating_add(height);
        row
    }

    pub(crate) fn spacer(&mut self, height: u16) {
        let _ = self.next_row(height);
    }

    pub(crate) fn register_field(
        &mut self,
        row: FormRow,
        field: F,
        kind: FormFieldKind,
    ) -> Option<Rect> {
        if !matches!(kind, FormFieldKind::Static | FormFieldKind::Disabled)
            && !self.frame_order.contains(&field)
        {
            self.frame_order.push(field);
        }
        if field == self.focus {
            self.focused_kind = kind;
        }
        self.fields.push(FieldEntry {
            field,
            virtual_y: row.virtual_y,
            height: row.height,
        });
        if let Some(rect) = row.rect {
            let hit = match kind {
                FormFieldKind::Text | FormFieldKind::AdjustableText => HitKind::Focus,
                FormFieldKind::Toggle
                | FormFieldKind::Choice
                | FormFieldKind::Select
                | FormFieldKind::Action => HitKind::Activate,
                FormFieldKind::Static => HitKind::Focus,
                FormFieldKind::Disabled => return row.rect,
            };
            self.register_hit(field, rect, hit);
        }
        row.rect
    }

    pub(crate) fn register_rect(
        &mut self,
        row: FormRow,
        rect: Rect,
        field: F,
        kind: FormFieldKind,
    ) {
        if !matches!(kind, FormFieldKind::Static | FormFieldKind::Disabled)
            && !self.frame_order.contains(&field)
        {
            self.frame_order.push(field);
        }
        if field == self.focus {
            self.focused_kind = kind;
        }
        self.fields.push(FieldEntry {
            field,
            virtual_y: row.virtual_y,
            height: row.height,
        });
        let hit = match kind {
            FormFieldKind::Text | FormFieldKind::AdjustableText => HitKind::Focus,
            FormFieldKind::Toggle
            | FormFieldKind::Choice
            | FormFieldKind::Select
            | FormFieldKind::Action => HitKind::Activate,
            FormFieldKind::Static => HitKind::Focus,
            FormFieldKind::Disabled => return,
        };
        self.register_hit(field, rect, hit);
    }

    pub(crate) fn register_text_area(&mut self, field: F, rect: Rect) {
        self.register_hit(field, rect, HitKind::Text);
    }

    pub(crate) fn register_adjust(&mut self, field: F, rect: Rect, delta: isize) {
        self.register_hit(field, rect, HitKind::Adjust(delta));
    }

    pub(crate) fn register_picker_item(&mut self, field: F, rect: Rect, item_index: usize) {
        self.register_hit(field, rect, HitKind::PickerItem(item_index));
    }

    pub(crate) fn finish_frame(&mut self) {
        self.content_height = self.cursor_y;
        if !self.frame_order.is_empty() {
            self.order.clear();
            self.order.extend(self.frame_order.iter().copied());
            if !self.order.contains(&self.focus) {
                self.focus = self.order[0];
            }
        }
        self.scroll = self.max_scroll().min(self.scroll);
        self.ensure_focus_visible();
    }

    pub(crate) fn set_focus(&mut self, field: F) -> Option<(F, String)> {
        if self.focus == field {
            return None;
        }
        let commit = self.clear_text();
        self.focus = field;
        self.ensure_focus_visible();
        commit
    }

    pub(crate) fn move_focus(&mut self, delta: isize) -> Option<(F, String)> {
        if self.order.is_empty() {
            return None;
        }
        let current = self
            .order
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(self.order.len() as isize) as usize;
        self.set_focus(self.order[next])
    }

    /// Returns the focusable field reached by moving `delta` positions
    /// horizontally within the focused field's row, or `None` when the row
    /// holds a single field. Fields registered on one [`FormRow`] share a
    /// `virtual_y`, and `order` preserves their left-to-right registration.
    fn horizontal_target(&self, delta: isize) -> Option<F> {
        let row_y = self
            .fields
            .iter()
            .find(|entry| entry.field == self.focus)?
            .virtual_y;
        let mut row = Vec::new();
        for field in &self.order {
            let on_row = self
                .fields
                .iter()
                .any(|entry| entry.field == *field && entry.virtual_y == row_y);
            if on_row {
                row.push(*field);
            }
        }
        if row.len() <= 1 {
            return None;
        }
        let position = row.iter().position(|field| *field == self.focus)?;
        let next = (position as isize + delta).rem_euclid(row.len() as isize) as usize;
        Some(row[next])
    }

    pub(crate) fn focus_text(
        &mut self,
        field: F,
        value: &str,
        enter_insert: bool,
    ) -> Option<(F, String)> {
        let commit = if self.active_text == Some(field) {
            None
        } else {
            let commit = self.clear_text();
            self.editor.set_lines(value);
            self.editor.set_cursor_offset(self.editor.text_len());
            if self.bindings == FormBindings::Standard || enter_insert {
                self.editor.enter_insert_mode();
            }
            self.active_text = Some(field);
            commit
        };
        self.focus = field;
        commit
    }

    pub(crate) fn focus_text_at(
        &mut self,
        field: F,
        value: &str,
        area: Rect,
        column: u16,
        enter_insert: bool,
    ) -> Option<(F, String)> {
        let commit = self.focus_text(field, value, enter_insert);
        let local_col = column.saturating_sub(area.x);
        let visible = self.editor.visible_byte_span(area);
        let text = self.editor.text();
        let start = visible.offset.min(text.len() as u32) as usize;
        let end = (visible.offset + visible.len).min(text.len() as u32) as usize;
        let relative = byte_offset_for_cell(&text[start..end], local_col);
        self.editor
            .set_cursor_offset(visible.offset.saturating_add(relative));
        if self.bindings == FormBindings::Standard || enter_insert {
            self.editor.enter_insert_mode();
        }
        commit
    }

    pub(crate) fn clear_text(&mut self) -> Option<(F, String)> {
        let active = self.active_text.take()?;
        Some((active, self.editor.text()))
    }

    /// Replaces the active editor contents after an out-of-band adjustment to
    /// the field value, keeping a following commit from restoring stale text.
    pub(crate) fn sync_active_text(&mut self, field: F, value: &str) {
        if self.active_text == Some(field) {
            self.replace_editor_text(value);
        }
    }

    /// The active editor's pending commit without disarming the editor. The
    /// in-place Enter commit uses this: clearing would let a render pass that
    /// interleaves before the core applies the commit re-seed the editor from
    /// the stale field value, and the following focus move would then commit
    /// that stale text back over the edit.
    fn text_commit(&self) -> Option<(F, String)> {
        let active = self.active_text?;
        Some((active, self.editor.text()))
    }

    pub(crate) fn text(&self) -> String {
        self.editor.text()
    }

    pub(crate) fn replace_active_text(&mut self, text: &str) -> Option<(F, String)> {
        let active = self.active_text?;
        self.replace_editor_text(text);
        Some((active, self.editor.text()))
    }

    fn replace_editor_text(&mut self, text: &str) {
        let restore_insert_mode = self.editor.mode() == Mode::Insert;
        self.editor.set_lines(text);
        self.editor.set_cursor_offset(self.editor.text_len());
        if restore_insert_mode {
            self.editor.enter_insert_mode();
        }
    }

    pub(crate) fn render_editor(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        self.editor.set_theme(theme.join_input_editor_theme());
        self.editor.resize(area.w.max(1));
        self.editor.render(area, buf);
    }

    pub(crate) fn handle_key(
        &mut self,
        key: KeyEvent,
        focused_kind: FormFieldKind,
    ) -> FormEvent<F> {
        if matches!(key.kind, KeyEventKind::Release) {
            return FormEvent::action(FormAction::None);
        }

        match self.bindings {
            FormBindings::Standard => self.handle_standard_key(key, focused_kind),
            FormBindings::Vim => self.handle_vim_key(key, focused_kind),
        }
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent) -> FormMouseEvent<F> {
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                if !rect_contains(self.viewport, mouse.column, mouse.row) {
                    return FormMouseEvent {
                        commit: None,
                        intent: FormMouseIntent::None,
                        action: FormAction::None,
                    };
                }
                self.scroll_by(3);
                return FormMouseEvent {
                    commit: None,
                    intent: FormMouseIntent::None,
                    action: FormAction::Scrolled,
                };
            }
            MouseEventKind::ScrollUp => {
                if !rect_contains(self.viewport, mouse.column, mouse.row) {
                    return FormMouseEvent {
                        commit: None,
                        intent: FormMouseIntent::None,
                        action: FormAction::None,
                    };
                }
                self.scroll_by(-3);
                return FormMouseEvent {
                    commit: None,
                    intent: FormMouseIntent::None,
                    action: FormAction::Scrolled,
                };
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => {
                return FormMouseEvent {
                    commit: None,
                    intent: FormMouseIntent::None,
                    action: FormAction::None,
                };
            }
        }

        let Some(hit) = self.hit(mouse.column, mouse.row) else {
            return FormMouseEvent {
                commit: None,
                intent: FormMouseIntent::None,
                action: FormAction::None,
            };
        };
        let commit = self.set_focus(hit.field);
        let intent = match hit.kind {
            HitKind::Focus => FormMouseIntent::None,
            HitKind::Activate => FormMouseIntent::Activate(hit.field),
            HitKind::Adjust(delta) => FormMouseIntent::Adjust(hit.field, delta),
            HitKind::Text => FormMouseIntent::Text(hit.field, hit.rect, mouse.column),
            HitKind::PickerItem(item_index) => FormMouseIntent::PickerItem(hit.field, item_index),
        };
        FormMouseEvent {
            commit,
            intent,
            action: FormAction::None,
        }
    }

    fn handle_standard_key(&mut self, key: KeyEvent, focused_kind: FormFieldKind) -> FormEvent<F> {
        match key.code {
            KeyCode::Esc => {
                let commit = self.clear_text();
                return FormEvent {
                    commit,
                    action: FormAction::Cancel,
                };
            }
            KeyCode::Tab | KeyCode::Down => {
                return FormEvent::action(FormAction::MoveFocus(1));
            }
            KeyCode::BackTab | KeyCode::Up => {
                return FormEvent::action(FormAction::MoveFocus(-1));
            }
            KeyCode::Enter => {
                return FormEvent {
                    commit: self.text_commit(),
                    action: FormAction::Activate,
                };
            }
            KeyCode::Left if focused_kind == FormFieldKind::AdjustableText => {
                return FormEvent {
                    commit: self.text_commit(),
                    action: FormAction::Adjust(-1),
                };
            }
            KeyCode::Right if focused_kind == FormFieldKind::AdjustableText => {
                return FormEvent {
                    commit: self.text_commit(),
                    action: FormAction::Adjust(1),
                };
            }
            KeyCode::Left if focused_kind != FormFieldKind::Text => {
                if let Some(target) = self.horizontal_target(-1) {
                    return FormEvent {
                        commit: self.set_focus(target),
                        action: FormAction::FocusMoved,
                    };
                }
                return FormEvent::action(FormAction::Adjust(-1));
            }
            KeyCode::Right if focused_kind != FormFieldKind::Text => {
                if let Some(target) = self.horizontal_target(1) {
                    return FormEvent {
                        commit: self.set_focus(target),
                        action: FormAction::FocusMoved,
                    };
                }
                return FormEvent::action(FormAction::Adjust(1));
            }
            _ => {}
        }

        if matches!(
            focused_kind,
            FormFieldKind::Text | FormFieldKind::AdjustableText
        ) && self.active_text.is_some()
        {
            let before = self.editor.text();
            if self.editor.send_key(&key) {
                let action = if self.editor.text() == before {
                    FormAction::None
                } else {
                    FormAction::TextChanged
                };
                return FormEvent {
                    commit: matches!(action, FormAction::TextChanged)
                        .then(|| self.text_commit())
                        .flatten(),
                    action,
                };
            }
        }

        FormEvent::action(FormAction::None)
    }

    fn handle_vim_key(&mut self, key: KeyEvent, focused_kind: FormFieldKind) -> FormEvent<F> {
        if matches!(
            focused_kind,
            FormFieldKind::Text | FormFieldKind::AdjustableText
        ) && self.active_text.is_some()
        {
            match self.editor.mode() {
                Mode::Normal => match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        return FormEvent::action(FormAction::MoveFocus(1));
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        return FormEvent::action(FormAction::MoveFocus(-1));
                    }
                    KeyCode::Enter => {
                        self.editor.enter_insert_mode();
                        return FormEvent::action(FormAction::None);
                    }
                    KeyCode::Esc => {
                        return FormEvent {
                            commit: self.clear_text(),
                            action: FormAction::Cancel,
                        };
                    }
                    _ => {
                        let before = self.editor.text();
                        if self.editor.send_key(&key) {
                            let action = if self.editor.text() == before {
                                FormAction::None
                            } else {
                                FormAction::TextChanged
                            };
                            return FormEvent {
                                commit: matches!(action, FormAction::TextChanged)
                                    .then(|| self.text_commit())
                                    .flatten(),
                                action,
                            };
                        }
                    }
                },
                Mode::Insert if key.code == KeyCode::Enter => {
                    return FormEvent {
                        commit: self.text_commit(),
                        action: FormAction::ActivateNextInsert,
                    };
                }
                Mode::Insert | Mode::Visual | Mode::VisualLine | Mode::VisualBlock => {
                    let before = self.editor.text();
                    if self.editor.send_key(&key) {
                        let action = if self.editor.text() == before {
                            FormAction::None
                        } else {
                            FormAction::TextChanged
                        };
                        return FormEvent {
                            commit: matches!(action, FormAction::TextChanged)
                                .then(|| self.text_commit())
                                .flatten(),
                            action,
                        };
                    }
                }
            }
            return FormEvent::action(FormAction::None);
        }

        match key.code {
            KeyCode::Esc => FormEvent::action(FormAction::Cancel),
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => {
                FormEvent::action(FormAction::MoveFocus(1))
            }
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => {
                FormEvent::action(FormAction::MoveFocus(-1))
            }
            KeyCode::Left | KeyCode::Char('h') => match self.horizontal_target(-1) {
                Some(target) => FormEvent {
                    commit: self.set_focus(target),
                    action: FormAction::FocusMoved,
                },
                None => FormEvent::action(FormAction::Adjust(-1)),
            },
            KeyCode::Right | KeyCode::Char('l') => match self.horizontal_target(1) {
                Some(target) => FormEvent {
                    commit: self.set_focus(target),
                    action: FormAction::FocusMoved,
                },
                None => FormEvent::action(FormAction::Adjust(1)),
            },
            KeyCode::Enter | KeyCode::Char('i') => FormEvent::action(FormAction::Activate),
            _ => FormEvent::action(FormAction::None),
        }
    }

    fn register_hit(&mut self, field: F, rect: Rect, kind: HitKind) {
        if rect.w > 0 && rect.h > 0 {
            self.hits.push(HitEntry { field, rect, kind });
        }
    }

    fn hit(&self, column: u16, row: u16) -> Option<HitEntry<F>> {
        self.hits
            .iter()
            .rev()
            .copied()
            .find(|hit| rect_contains(hit.rect, column, row))
    }

    fn scroll_by(&mut self, delta: isize) {
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, self.max_scroll() as isize) as u16;
    }

    fn max_scroll(&self) -> u16 {
        self.content_height.saturating_sub(self.viewport.h)
    }

    fn ensure_focus_visible(&mut self) {
        let Some(field) = self
            .fields
            .iter()
            .find(|entry| entry.field == self.focus)
            .copied()
        else {
            return;
        };
        if field.virtual_y < self.scroll {
            self.scroll = field.virtual_y;
        } else {
            let bottom = field.virtual_y.saturating_add(field.height);
            let visible_bottom = self.scroll.saturating_add(self.viewport.h);
            if bottom > visible_bottom {
                self.scroll = bottom.saturating_sub(self.viewport.h);
            }
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }
}

fn new_form_editor(bindings: FormBindings) -> Editor {
    let editor_bindings = match bindings {
        FormBindings::Standard => editor_bindings::nano(),
        FormBindings::Vim => editor_bindings::vim(editor_bindings::VimOptions::default()),
    };
    let mut editor = Editor::with_bindings(editor_bindings);
    editor.set_single_line(true);
    editor.set_wrap(false);
    editor.set_height_bounds(1, 1);
    editor
}

fn visible_row(viewport: Rect, scroll: u16, virtual_y: u16, height: u16) -> Option<Rect> {
    let row_bottom = virtual_y.saturating_add(height);
    let viewport_bottom = scroll.saturating_add(viewport.h);
    if row_bottom <= scroll || virtual_y >= viewport_bottom {
        return None;
    }
    if virtual_y < scroll || row_bottom > viewport_bottom {
        return None;
    }
    Some(Rect {
        x: viewport.x,
        y: viewport.y + virtual_y - scroll,
        w: viewport.w,
        h: height,
    })
}

pub(crate) fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && row >= rect.y
        && column < rect.x.saturating_add(rect.w)
        && row < rect.y.saturating_add(rect.h)
}

fn byte_offset_for_cell(text: &str, cell: u16) -> u32 {
    let mut width = 0usize;
    let target = cell as usize;
    for (index, ch) in text.char_indices() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if target <= width + ch_width / 2 {
            return index as u32;
        }
        width += ch_width;
    }
    text.len() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use extui::event::{KeyModifiers, MouseEvent};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Field {
        One,
        Two,
        Three,
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn enter_commits_text_without_disarming_the_editor() {
        // Enter's commit crosses a thread boundary before it is applied. If
        // Enter disarmed the editor, a render interleaved in that window would
        // re-seed it from the stale field value and the follow-up focus move
        // would commit the stale text back, reverting the edit.
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 5,
        });
        let row = form.next_row(1);
        form.register_field(row, Field::One, FormFieldKind::Text);
        form.finish_frame();
        form.focus_text(Field::One, "205", false);

        let event = form.handle_key(key(KeyCode::Enter), FormFieldKind::Text);

        assert_eq!(event.action, FormAction::Activate);
        assert_eq!(event.commit, Some((Field::One, "205".to_string())));
        assert_eq!(form.active_text(), Some(Field::One));
        // The eventual focus move still commits (the same text) and disarms.
        assert_eq!(form.clear_text(), Some((Field::One, "205".to_string())));
        assert_eq!(form.active_text(), None);
    }

    #[test]
    fn vertical_key_requests_focus_move_without_mutating_form() {
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 5,
        });
        for field in [Field::One, Field::Two, Field::Three] {
            let row = form.next_row(1);
            form.register_field(row, field, FormFieldKind::Choice);
        }
        form.finish_frame();

        let event = form.handle_key(key(KeyCode::Down), FormFieldKind::Choice);
        assert_eq!(event.action, FormAction::MoveFocus(1));
        assert_eq!(form.focus(), Field::One);
    }

    #[test]
    fn horizontal_moves_between_fields_sharing_a_row() {
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 1,
        });
        let row = form.next_row(1);
        form.register_rect(
            row,
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 1,
            },
            Field::One,
            FormFieldKind::Action,
        );
        form.register_rect(
            row,
            Rect {
                x: 10,
                y: 0,
                w: 10,
                h: 1,
            },
            Field::Two,
            FormFieldKind::Action,
        );
        form.finish_frame();

        let event = form.handle_key(key(KeyCode::Right), FormFieldKind::Action);
        assert_eq!(event.action, FormAction::FocusMoved);
        assert_eq!(form.focus(), Field::Two);

        let event = form.handle_key(key(KeyCode::Char('h')), FormFieldKind::Action);
        // Standard mode ignores 'h' as a movement key, leaving focus put.
        assert_eq!(event.action, FormAction::None);
        assert_eq!(form.focus(), Field::Two);
    }

    #[test]
    fn vim_horizontal_moves_between_buttons() {
        let mut form = FormState::new(Field::One, FormBindings::Vim);
        form.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 1,
        });
        let row = form.next_row(1);
        form.register_rect(
            row,
            Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 1,
            },
            Field::One,
            FormFieldKind::Action,
        );
        form.register_rect(
            row,
            Rect {
                x: 10,
                y: 0,
                w: 10,
                h: 1,
            },
            Field::Two,
            FormFieldKind::Action,
        );
        form.finish_frame();

        let event = form.handle_key(key(KeyCode::Char('l')), FormFieldKind::Action);
        assert_eq!(event.action, FormAction::FocusMoved);
        assert_eq!(form.focus(), Field::Two);
    }

    #[test]
    fn horizontal_adjusts_single_field_row() {
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 1,
        });
        let row = form.next_row(1);
        form.register_field(row, Field::One, FormFieldKind::Choice);
        form.finish_frame();

        let event = form.handle_key(key(KeyCode::Right), FormFieldKind::Choice);
        assert_eq!(event.action, FormAction::Adjust(1));
        assert_eq!(form.focus(), Field::One);
    }

    #[test]
    fn focused_field_scrolls_into_view() {
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 2,
        });
        for field in [Field::One, Field::Two, Field::Three] {
            let row = form.next_row(1);
            form.register_field(row, field, FormFieldKind::Choice);
        }
        form.finish_frame();

        form.set_focus(Field::Three);
        assert_eq!(form.scroll(), 1);
    }

    #[test]
    fn mouse_click_hits_button() {
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.begin_frame(Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 3,
        });
        let row = form.next_row(1);
        form.register_field(row, Field::Two, FormFieldKind::Action);
        form.finish_frame();

        let event = form.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(event.intent, FormMouseIntent::Activate(Field::Two));
        assert_eq!(form.focus(), Field::Two);
    }

    #[test]
    fn mouse_wheel_only_scrolls_inside_viewport() {
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.begin_frame(Rect {
            x: 2,
            y: 2,
            w: 20,
            h: 2,
        });
        for field in [Field::One, Field::Two, Field::Three] {
            let row = form.next_row(1);
            form.register_field(row, field, FormFieldKind::Choice);
        }
        form.finish_frame();

        let outside = form.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(outside.action, FormAction::None);
        assert_eq!(form.scroll(), 0);

        let inside = form.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 3,
            row: 2,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(inside.action, FormAction::Scrolled);
        assert_eq!(form.scroll(), 1);
    }

    #[test]
    fn standard_text_receives_left_right() {
        let mut form = FormState::new(Field::One, FormBindings::Standard);
        form.focus_text(Field::One, "abc", false);
        form.handle_key(key(KeyCode::Left), FormFieldKind::Text);
        assert!(form.editor_mut().cursor_offset() < 3);
    }

    #[test]
    fn vim_normal_j_requests_atomic_focus_move() {
        let mut form =
            FormState::with_order(Field::One, FormBindings::Vim, [Field::One, Field::Two]);
        form.focus_text(Field::One, "abc", false);
        let event = form.handle_key(key(KeyCode::Char('j')), FormFieldKind::Text);
        assert_eq!(event.action, FormAction::MoveFocus(1));
        assert_eq!(form.focus(), Field::One);
        assert_eq!(form.active_text(), Some(Field::One));
    }

    #[test]
    fn vim_text_allows_change_inner_word() {
        let mut form = FormState::with_order(Field::One, FormBindings::Vim, [Field::One]);
        form.focus_text(Field::One, "hello", false);
        form.editor_mut().set_cursor_offset(1);

        form.handle_key(key(KeyCode::Char('c')), FormFieldKind::Text);
        form.handle_key(key(KeyCode::Char('i')), FormFieldKind::Text);
        let event = form.handle_key(key(KeyCode::Char('w')), FormFieldKind::Text);

        assert_eq!(event.action, FormAction::TextChanged);
        assert_eq!(form.text(), "");
        assert_eq!(form.editor_mut().mode(), Mode::Insert);
    }

    #[test]
    fn vim_insert_enter_commits_and_activates_text_field() {
        let mut form = FormState::with_order(Field::One, FormBindings::Vim, [Field::One]);
        form.focus_text(Field::One, "value", true);

        let event = form.handle_key(key(KeyCode::Enter), FormFieldKind::Text);

        assert_eq!(event.action, FormAction::ActivateNextInsert);
        assert_eq!(event.commit, Some((Field::One, "value".to_string())));
        assert_eq!(form.active_text(), Some(Field::One));
    }

    #[test]
    fn replacing_active_text_preserves_vim_insert_mode() {
        let mut form = FormState::with_order(Field::One, FormBindings::Vim, [Field::One]);
        form.focus_text(Field::One, "", true);

        let commit = form.replace_active_text("photo-host-stadium-narrow-video-frost");

        assert_eq!(
            commit,
            Some((
                Field::One,
                "photo-host-stadium-narrow-video-frost".to_string()
            ))
        );
        assert_eq!(form.editor_mut().mode(), Mode::Insert);
    }

    #[test]
    fn cell_to_byte_offset_clamps_to_char_boundary() {
        assert_eq!(byte_offset_for_cell("abcd", 2), 2);
        assert_eq!(byte_offset_for_cell("aéz", 2), "aé".len() as u32);
    }
}
