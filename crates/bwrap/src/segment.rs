// Copyright (C) 2023-2024 Michael Lee <micl2e2@proton.me>
//
// Licensed under the MIT License <LICENSE-MIT or
// https://opensource.org/license/mit> or the GNU General Public License,
// Version 3.0 or any later version <LICENSE-GPL or
// https://www.gnu.org/licenses/gpl-3.0.txt>, at your option.
//
// This file may not be copied, modified, or distributed except except in
// compliance with either of the licenses.
//

use core::ops::Range;

use unicode_width::UnicodeWidthStr;

use crate::uwidth::{continues_width_unit, width};

///
/// Wrap `text` at word boundaries, yielding each visual line as a byte range
/// into `text`. Nothing is copied; offsets into the original string survive
/// wrapping, which lets callers map ranges computed against `text` (such as
/// highlight spans) onto the wrapped lines.
///
/// The input is treated as one logical paragraph. Words are maximal runs of
/// bytes other than space, tab, `'\n'` and `'\r'`. A whitespace run between
/// two words *containing* `'\n'` or `'\r'` is a soft break and counts as
/// display width 1; a run of only spaces and tabs keeps its literal width.
/// A yielded range may therefore contain an interior newline: consumers must
/// render such a run as a single space to stay column-exact with the widths
/// accounted here.
///
/// The first yielded line is limited to `first_width` columns and every
/// subsequent line to `cont_width` (both treated as at least 1), which
/// supports hanging indents. Yielded ranges never start or end with
/// whitespace, and whitespace spanning a break is consumed. A single word
/// wider than its line limit is broken at width-unit boundaries (combining
/// sequences, ZWJ emoji and regional-indicator pairs are never split).
/// Empty or all-whitespace input yields nothing.
pub fn wrap_ranges(text: &str, first_width: usize, cont_width: usize) -> WrapRanges<'_> {
    WrapRanges {
        text,
        first_width: first_width.max(1),
        cont_width: cont_width.max(1),
        cursor: 0,
        started: false,
    }
}

///
/// Iterator over the visual lines of a soft-wrapped paragraph, as byte ranges
/// into the input. See [`wrap_ranges`].
pub struct WrapRanges<'a> {
    text: &'a str,
    first_width: usize,
    cont_width: usize,
    cursor: usize,
    started: bool,
}

#[inline]
fn is_wrap_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn separator_width(run: &str) -> usize {
    if run.as_bytes().iter().any(|&b| b == b'\n' || b == b'\r') {
        1
    } else {
        run.len()
    }
}

struct WidthUnits<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Iterator for WidthUnits<'a> {
    type Item = (Range<usize>, usize);

    fn next(&mut self) -> Option<(Range<usize>, usize)> {
        let mut chars = self.text[self.pos..].chars();
        let first = chars.next()?;
        let start = self.pos;
        let mut end = start + first.len_utf8();
        let mut last = first;
        let mut clustered = false;
        for ch in chars {
            if !continues_width_unit(last, ch) {
                break;
            }
            end += ch.len_utf8();
            last = ch;
            clustered = true;
        }
        let unit_width = if clustered {
            UnicodeWidthStr::width(&self.text[start..end])
        } else {
            width(first)
        };
        self.pos = end;
        Some((start..end, unit_width))
    }
}

fn word_width(word: &str) -> usize {
    if word.is_ascii() {
        return word.len();
    }
    WidthUnits { text: word, pos: 0 }.map(|(_, w)| w).sum()
}

fn fit_prefix(word: &str, max_width: usize) -> usize {
    if word.is_ascii() {
        return max_width.min(word.len());
    }
    let mut end = 0;
    let mut taken = 0;
    for (range, unit_width) in (WidthUnits { text: word, pos: 0 }) {
        if end > 0 && taken + unit_width > max_width {
            break;
        }
        taken += unit_width;
        end = range.end;
    }
    end
}

impl<'a> Iterator for WrapRanges<'a> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        let bytes = self.text.as_bytes();
        while self.cursor < bytes.len() && is_wrap_space(bytes[self.cursor]) {
            self.cursor += 1;
        }
        if self.cursor >= bytes.len() {
            return None;
        }
        let max_width = if self.started {
            self.cont_width
        } else {
            self.first_width
        };
        self.started = true;

        let line_start = self.cursor;
        let mut line_end = line_start;
        let mut line_width = 0usize;
        let mut pos = line_start;

        while pos < bytes.len() {
            let word_start = pos;
            while pos < bytes.len() && !is_wrap_space(bytes[pos]) {
                pos += 1;
            }
            let run_width = word_width(&self.text[word_start..pos]);
            let sep_width = if line_width == 0 {
                0
            } else {
                separator_width(&self.text[line_end..word_start])
            };

            if line_width + sep_width + run_width > max_width {
                if line_width == 0 {
                    let split = fit_prefix(&self.text[word_start..pos], max_width);
                    self.cursor = word_start + split;
                    return Some(word_start..self.cursor);
                }
                break;
            }

            line_width += sep_width + run_width;
            line_end = pos;
            while pos < bytes.len() && is_wrap_space(bytes[pos]) {
                pos += 1;
            }
        }

        self.cursor = line_end;
        Some(line_start..line_end)
    }
}
