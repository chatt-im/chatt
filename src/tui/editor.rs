use extui::{Buffer, Rect};
use extui_editor::{Editor, Replacement, StyleRun, TextBuffer, TrackedChange};
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
