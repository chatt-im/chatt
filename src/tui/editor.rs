use extui::{Buffer, Rect};
use extui_editor::{Editor, Replacement, StyleRun, TextBuffer, TrackedChange};
use tinyhl::{Highlighter, Source};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Maps a byte offset in wrapped composer text to its visual row and column.
///
/// Keep this in lockstep with extui-editor's wrapping rules: hard tabs advance
/// to the next tab stop and every other grapheme contributes its terminal
/// display width.
pub(crate) fn composer_visual_position(
    text: &str,
    offset: usize,
    width: u16,
    tabstop: u16,
) -> (usize, u16) {
    let width = width.max(1);
    let tabstop = tabstop.max(1);
    let mut row = 0usize;
    let mut col = 0u16;
    for (start, grapheme) in text.grapheme_indices(true) {
        if start >= offset {
            break;
        }
        if grapheme == "\n" {
            row += 1;
            col = 0;
            continue;
        }
        let cells = if grapheme == "\t" {
            tabstop - col % tabstop
        } else {
            UnicodeWidthStr::width(grapheme).min(u16::MAX as usize) as u16
        };
        let visual = u32::from(col) + u32::from(cells);
        row += (visual / u32::from(width)) as usize;
        col = (visual % u32::from(width)) as u16;
    }
    (row, col)
}

/// Returns the first byte boundary on or below `target_row` in wrapped text.
pub(crate) fn composer_offset_at_visual_row(
    text: &str,
    target_row: usize,
    width: u16,
    tabstop: u16,
) -> usize {
    let width = width.max(1);
    let tabstop = tabstop.max(1);
    let mut row = 0usize;
    let mut col = 0u16;
    for (start, grapheme) in text.grapheme_indices(true) {
        if row >= target_row {
            return start;
        }
        if grapheme == "\n" {
            row += 1;
            col = 0;
            continue;
        }
        let cells = if grapheme == "\t" {
            tabstop - col % tabstop
        } else {
            UnicodeWidthStr::width(grapheme).min(u16::MAX as usize) as u16
        };
        let visual = u32::from(col) + u32::from(cells);
        row += (visual / u32::from(width)) as usize;
        col = (visual % u32::from(width)) as u16;
    }
    text.len()
}

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

    pub(crate) fn render(
        &mut self,
        editor: &mut Editor,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
    ) {
        self.sync(editor);
        self.runs.clear();
        if let Some(table) = self.hl.table() {
            let visible = editor.visible_byte_span(area);
            let span = tinyhl::Span::new(visible.offset, visible.len);
            for tok in table.query(span) {
                let mut style = theme.text;
                if let Some(render_span) = self
                    .hl
                    .render(tinyhl::Span::new(tok.span.offset, tok.span.len))
                    .next()
                {
                    style = style.patch(theme.syntax.style(&render_span));
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
