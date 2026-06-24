use extui::{Buffer, Rect};
use extui_editor::{
    Editor, Replacement, StyleRun, TextBuffer, TrackedChange, bindings as editor_bindings,
};
use tinyhl::{Highlighter, Source};

use crate::theme;

struct BufferSource<'a>(&'a TextBuffer);

impl<'a> Source for BufferSource<'a> {
    fn len(&self) -> u32 {
        self.0.len() as u32
    }

    fn page(&self, offset: u32) -> (u32, &[u8]) {
        self.0.page(offset)
    }
}

pub(crate) struct EditorHighlighter {
    hl: Highlighter,
    runs: Vec<StyleRun>,
}

impl EditorHighlighter {
    pub(crate) fn new(editor: &mut Editor) -> Self {
        editor.set_track_replacements(true);
        let mut hl = Highlighter::new(tinyhl::Language::Markdown);
        hl.rebuild(&BufferSource(editor.text_buffer()));
        Self {
            hl,
            runs: Vec::new(),
        }
    }

    fn sync(&mut self, editor: &mut Editor) {
        match editor.take_tracked_change() {
            TrackedChange::None => {}
            TrackedChange::Reset => self.hl.rebuild(&BufferSource(editor.text_buffer())),
            TrackedChange::Merged(Replacement {
                offset,
                old_len,
                new_len,
            }) => self.hl.apply_replacement(
                &BufferSource(editor.text_buffer()),
                tinyhl::Span::new(offset, old_len),
                new_len,
            ),
        }
    }

    pub(crate) fn render(&mut self, editor: &mut Editor, area: Rect, buf: &mut Buffer) {
        self.sync(editor);
        self.runs.clear();
        if let Some(table) = self.hl.table() {
            let visible = editor.visible_byte_span(area);
            let span = tinyhl::Span::new(visible.offset, visible.len);
            for tok in table.query(span) {
                let mut style = theme::TEXT;
                if let Some(render_span) = self
                    .hl
                    .render(tinyhl::Span::new(tok.span.offset, tok.span.len))
                    .next()
                {
                    style = style.patch(theme::syntax_style(&render_span));
                }
                self.runs.push(StyleRun {
                    offset: tok.span.offset,
                    len: tok.span.len,
                    style,
                });
            }
        }
        editor.render_with_styles(area, buf, &self.runs);
    }
}

pub(crate) fn focus_join_input_editor(editor: &mut Editor) {
    editor.enter_insert_mode();
    editor.set_cursor_offset(editor.text_len());
}

pub(crate) struct FormEditor<F> {
    editor: Editor,
    active: Option<F>,
}

impl<F> Default for FormEditor<F> {
    fn default() -> Self {
        Self {
            editor: new_form_editor(""),
            active: None,
        }
    }
}

impl<F: Copy + Eq> FormEditor<F> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn active(&self) -> Option<F> {
        self.active
    }

    pub(crate) fn focus(&mut self, field: F, value: &str) -> Option<(F, String)> {
        if self.active == Some(field) {
            return None;
        }
        let previous = self.active.map(|field| (field, self.editor.text()));
        self.editor.set_lines(value);
        focus_join_input_editor(&mut self.editor);
        self.active = Some(field);
        previous
    }

    pub(crate) fn clear_focus(&mut self) -> Option<(F, String)> {
        let previous = self.active.map(|field| (field, self.editor.text()));
        self.active = None;
        previous
    }

    pub(crate) fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub(crate) fn text(&self) -> String {
        self.editor.text()
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.editor.render(area, buf);
    }
}

fn new_form_editor(value: &str) -> Editor {
    let mut editor = Editor::with_bindings(editor_bindings::nano());
    editor.set_single_line(true);
    editor.set_wrap(false);
    editor.set_height_bounds(1, 1);
    editor.set_theme(theme::join_input_editor_theme());
    editor.set_lines(value);
    focus_join_input_editor(&mut editor);
    editor
}
