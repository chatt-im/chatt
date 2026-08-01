use std::borrow::Cow;

use chatt_message_format::{is_fence_line, quote_prefix};
use extui::{Buffer, Rect};
use extui_editor::{Editor, Replacement, Span, StyleRun, TextBuffer, TrackedChange};
use tinyhl::{Highlighter, Source};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme::Theme;

/// Inserts host-provided paste text at the editor cursor and leaves the editor
/// ready for continued typing. Single-line editors apply their own newline
/// filtering through [`Editor::replace_range`].
pub(crate) fn insert_editor_paste(editor: &mut Editor, text: &str) -> bool {
    let before = editor.text();
    editor.enter_insert_mode();
    editor.replace_range(Span::empty_at(editor.cursor_offset()), text);
    editor.text() != before
}

/// Normalizes paste text for controls backed by a plain [`String`] rather
/// than a single-line [`Editor`].
pub(crate) fn single_line_paste(text: &str) -> Cow<'_, str> {
    if !text.contains(['\n', '\r']) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .filter(|ch| !matches!(ch, '\n' | '\r'))
            .collect(),
    )
}

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

/// Rewrites a paste so it lands inside the Markdown block the cursor sits in.
///
/// Returns the exact text to insert at `offset` in `source`. Two rules apply,
/// both keyed to the grammar the message renderer uses
/// ([`chatt_message_format`]), so the composer and the sent message agree:
///
/// - A paste at the end of a fence line starts on the line below it, because
///   text appended to `` ``` `` becomes an info string instead of code. One
///   trailing newline is dropped so the closing fence stays tight.
/// - A paste on a block-quote line carries that line's `>` markers onto every
///   line it adds, so the whole paste stays quoted.
///
/// Line endings are normalized to LF first: bracketed paste carries whatever
/// the terminal sends, and the composer is a multi-line editor that would keep
/// a stray CR verbatim.
pub(crate) fn markdown_paste_insertion(source: &str, offset: usize, paste: &str) -> String {
    let offset = offset.min(source.len());
    let paste = normalize_line_endings(paste);
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[offset..]
        .find('\n')
        .map_or(source.len(), |index| offset + index);
    let line = &source[line_start..line_end];
    let prefix = quote_prefix(line);

    let mut paste = paste.as_ref();
    let mut insertion = String::with_capacity(paste.len() + prefix.len() + 1);
    if offset == line_end && is_fence_line(&line[prefix.len()..]) {
        if !paste.starts_with('\n') {
            insertion.push('\n');
        }
        paste = paste.strip_suffix('\n').unwrap_or(paste);
    }
    insertion.push_str(paste);

    if prefix.is_empty() || offset < line_start + prefix.len() {
        return insertion;
    }
    continue_quote(&insertion, prefix)
}

/// Repeats `prefix` on every line `text` starts after its first.
fn continue_quote(text: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(text.len() + prefix.len());
    let mut lines = text.split('\n').peekable();
    if let Some(first) = lines.next() {
        out.push_str(first);
    }
    while let Some(line) = lines.next() {
        out.push('\n');
        // An interior blank line only needs the bare marker to hold the quote
        // open; the final one is where the cursor lands, so it keeps the space
        // the user would otherwise have to type.
        if line.is_empty() && lines.peek().is_some() {
            out.push_str(prefix.trim_end());
        } else {
            out.push_str(prefix);
        }
        out.push_str(line);
    }
    out
}

/// Rewrites CRLF and lone CR line endings as LF.
fn normalize_line_endings(paste: &str) -> Cow<'_, str> {
    if !paste.contains('\r') {
        return Cow::Borrowed(paste);
    }
    let mut out = String::with_capacity(paste.len());
    let mut rest = paste;
    while let Some(index) = rest.find('\r') {
        out.push_str(&rest[..index]);
        out.push('\n');
        rest = &rest[index + 1..];
        if let Some(tail) = rest.strip_prefix('\n') {
            rest = tail;
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
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

#[cfg(test)]
mod tests {
    use super::markdown_paste_insertion;

    /// Pastes `paste` at the `|` in `source`, returning the resulting buffer.
    fn paste_at(source: &str, paste: &str) -> String {
        let offset = source.find('|').expect("cursor marker");
        let source = source.replace('|', "");
        let insertion = markdown_paste_insertion(&source, offset, paste);
        format!("{}{insertion}{}", &source[..offset], &source[offset..])
    }

    #[test]
    fn paste_on_fence_line_starts_a_new_line() {
        assert_eq!(paste_at("```|\n```", "code"), "```\ncode\n```");
    }

    #[test]
    fn paste_on_fence_line_with_language_starts_a_new_line() {
        assert_eq!(
            paste_at("```rust|\n```", "fn main() {}"),
            "```rust\nfn main() {}\n```"
        );
    }

    #[test]
    fn paste_on_fence_after_prose_starts_a_new_line() {
        assert_eq!(
            paste_at("Hello ```rust|\n```", "code"),
            "Hello ```rust\ncode\n```"
        );
    }

    #[test]
    fn paste_at_closing_fence_end_starts_a_new_line() {
        assert_eq!(
            paste_at("```\ncode\n```|", "after"),
            "```\ncode\n```\nafter"
        );
    }

    #[test]
    fn paste_before_fence_info_string_stays_inline() {
        assert_eq!(paste_at("```|rust\n```", "sh"), "```shrust\n```");
    }

    #[test]
    fn paste_inside_fence_body_is_unchanged() {
        assert_eq!(paste_at("```\n|\n```", "code"), "```\ncode\n```");
    }

    #[test]
    fn paste_into_fence_drops_one_trailing_newline() {
        assert_eq!(paste_at("```|\n```", "code\n"), "```\ncode\n```");
    }

    #[test]
    fn paste_starting_with_a_newline_is_not_doubled() {
        assert_eq!(paste_at("```|\n```", "\ncode"), "```\ncode\n```");
    }

    #[test]
    fn paste_on_quote_line_quotes_every_added_line() {
        assert_eq!(paste_at("> |", "first\nsecond"), "> first\n> second");
    }

    #[test]
    fn paste_keeps_nested_quote_prefix() {
        assert_eq!(paste_at(">> |", "first\nsecond"), ">> first\n>> second");
    }

    #[test]
    fn paste_before_quote_marker_is_unchanged() {
        assert_eq!(
            paste_at("|> quoted", "first\nsecond"),
            "first\nsecond> quoted"
        );
    }

    #[test]
    fn paste_blank_lines_keep_the_bare_quote_marker() {
        assert_eq!(paste_at("> |", "first\n\nsecond"), "> first\n>\n> second");
    }

    #[test]
    fn paste_ending_in_newline_keeps_the_quote_marker_for_typing() {
        assert_eq!(paste_at("> |", "first\n"), "> first\n> ");
    }

    #[test]
    fn paste_on_quoted_fence_line_quotes_the_inserted_break() {
        assert_eq!(paste_at("> ```|\n> ```", "code"), "> ```\n> code\n> ```");
    }

    #[test]
    fn paste_normalizes_crlf_line_endings() {
        assert_eq!(
            paste_at("|", "first\r\nsecond\rthird"),
            "first\nsecond\nthird"
        );
    }

    #[test]
    fn paste_outside_markdown_blocks_is_verbatim() {
        assert_eq!(paste_at("hello |world", "big "), "hello big world");
    }
}
