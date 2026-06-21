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

use unicode_width::UnicodeWidthStr;

use crate::uwidth::{continues_width_unit, width};
use crate::Result;
use crate::WrapError;

struct OutBuf<'a> {
    before_len: usize,
    inner: &'a mut [u8],
    len: usize,
}

struct MayBrkLine {
    beg: usize,
    width: usize,
}

struct MayBrkUnit {
    beg: usize,
    end: usize,
    width: usize,
    has_more: bool,
}

impl<'a> OutBuf<'a> {
    fn new(before_len: usize, inner: &'a mut [u8]) -> Self {
        OutBuf {
            before_len,
            inner,
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn ensure(&self, additional: usize) -> Result<()> {
        let needed = self.len + additional;
        if self.inner.len() < needed {
            Err(WrapError::InsufficentBufferSize(
                self.before_len,
                self.inner.len(),
                needed,
            ))
        } else {
            Ok(())
        }
    }

    fn push(&mut self, byte: u8) -> Result<()> {
        self.ensure(1)?;
        self.inner[self.len] = byte;
        self.len += 1;
        Ok(())
    }

    fn push_many(&mut self, bytes: &[u8]) -> Result<()> {
        self.ensure(bytes.len())?;
        self.inner[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }

    fn push_byte_many(&mut self, byte: u8, count: usize) -> Result<()> {
        self.ensure(count)?;
        for dst in &mut self.inner[self.len..self.len + count] {
            *dst = byte;
        }
        self.len += count;
        Ok(())
    }
}

///
/// The wrapping style used by [`Wrapper`].
///
/// Bwrap categorizes the user input into two categories:
///
/// 1. space-sensitive
/// 2. space-insensitive
///
/// **"space-sensitive"** suits for the languages that depend on ASCII
/// SPACE to delimit words, such as English, Ukrainian, Greek, etc.
/// **"space-insensitive"** suits for otherwise languages, such as Chinese,
/// Japanese, Thai, etc.
pub enum WrapStyle<'a> {
    ///
    /// Wrapping text will **never** break the original semantics. This is true
    /// for those "space-sensitive" languages.
    ///
    /// If the first value is not `None`, it will be appended to all newly
    /// inserted newlines. The second value instructs the wrap how to deal
    /// with all existing newlines.
    NoBrk(Option<&'a str>, ExistNlPref),

    ///
    /// Wrapping text **may** break the original semantics. For example, the
    /// wrapping `We need an example` with 15-width limit results in
    ///
    /// ```ignored
    /// We need an exam
    /// ple
    /// ```
    ///
    /// If the first value is not `None`, it will be prepended to all newly
    /// inserted newlines. If the second value is not `None`, it will be
    /// appended to all newly inserted newlines.
    MayBrk(Option<&'a str>, Option<&'a str>),
}

///
/// Preference for existing newlines.
#[derive(PartialEq)]
pub enum ExistNlPref {
    ///
    /// Trim all ASCII SPACEs following each existing newline character.
    TrimTrailSpc,

    ///
    /// Keep all ASCII SPACEs following each existing newline character.
    KeepTrailSpc,
}

///
/// A type for the actual wrapping tasks.
///
/// Note that this requires manual memory management.
pub struct Wrapper<'a, 'b> {
    max_width: usize,
    before: &'a str,
    after: &'b mut [u8],
}

impl<'bf, 'af> Wrapper<'bf, 'af> {
    ///
    /// Initialize an Wrapper instance.
    ///
    /// # Errors
    /// If output buffer size is insufficient to hold the final output bytes.
    pub fn new(before: &'bf str, max_width: usize, after: &'af mut [u8]) -> Result<Self> {
        let bf_len = before.len();
        let af_len = after.len();

        if bf_len != 0 && af_len == 0 {
            return Err(WrapError::InsufficentBufferSize(bf_len, af_len, 0));
        }

        if af_len < bf_len {
            return Err(WrapError::InsufficentBufferSize(bf_len, af_len, bf_len));
        }

        if max_width == 0 {
            return Err(WrapError::InvalidWidth);
        }

        Ok(Wrapper {
            before,
            max_width,
            after,
        })
    }

    ///
    /// A convient alias for
    /// `wrap_use_style(WrapStyle::NoBrk(None, ExistNlPref::KeepTrailSpc)`.
    pub fn wrap(&mut self) -> Result<usize> {
        self.wrap_use_style(WrapStyle::NoBrk(None, ExistNlPref::KeepTrailSpc))
    }

    ///
    /// Perform wrapping tasks.
    ///
    /// Note that this method will mutate the output buffer.
    ///
    /// # Errors
    /// If output buffer size is insufficient to hold the final output bytes.
    pub fn wrap_use_style(&mut self, style: WrapStyle<'_>) -> Result<usize> {
        match style {
            WrapStyle::NoBrk(append_what, enl_pref) => {
                self.internal_wrap_nobrk(append_what, enl_pref)
            }
            WrapStyle::MayBrk(prepend_what, append_what) => {
                self.internal_wrap_maybrk(prepend_what, append_what)
            }
        }
    }

    pub(crate) fn internal_wrap_nobrk(
        &mut self,
        append_str: Option<&str>,
        enl_pref: ExistNlPref,
    ) -> Result<usize> {
        use ExistNlPref::TrimTrailSpc;

        let bf_bytes = self.before.as_bytes();
        let bf_len = bf_bytes.len();
        let max_width = self.max_width;

        let append_str = append_str.unwrap_or("");

        if max_width < UnicodeWidthStr::width(append_str) {
            return Err(WrapError::InvalidWidth);
        }

        if bf_len == 0 {
            return Ok(0);
        }

        let append_bytes = append_str.as_bytes();
        let mut buf_after = OutBuf::new(bf_len, self.after);
        let mut line_width = 0usize;
        let mut pending_spaces = 0usize;
        let mut trim_after_existing_nl = false;
        let mut idx = 0usize;

        while idx < bf_len {
            if trim_after_existing_nl && bf_bytes[idx] == b' ' {
                idx += 1;
                continue;
            }
            trim_after_existing_nl = false;

            if bf_bytes[idx] == b'\n' {
                buf_after.push_byte_many(b' ', pending_spaces)?;
                pending_spaces = 0;
                buf_after.push(b'\n')?;
                line_width = 0;
                trim_after_existing_nl = enl_pref == TrimTrailSpc;
                idx += 1;
                continue;
            }

            if bf_bytes[idx] == b' ' {
                let spc_beg = idx;
                while idx < bf_len && bf_bytes[idx] == b' ' {
                    idx += 1;
                }
                let space_len = idx - spc_beg;
                let available = max_width.saturating_sub(line_width);

                if space_len > available {
                    buf_after.push_byte_many(b' ', available)?;
                    buf_after.push(b'\n')?;
                    buf_after.push_many(append_bytes)?;
                    line_width = 0;
                    pending_spaces = 0;
                } else {
                    pending_spaces += space_len;
                }
                continue;
            }

            let run_beg = idx;
            while idx < bf_len && bf_bytes[idx] != b' ' && bf_bytes[idx] != b'\n' {
                idx += 1;
            }
            let run = &self.before[run_beg..idx];
            let run_width = UnicodeWidthStr::width(run);

            if pending_spaces > 0 && line_width + pending_spaces + run_width > max_width {
                buf_after.push_byte_many(b' ', pending_spaces - 1)?;
                buf_after.push(b'\n')?;
                buf_after.push_many(append_bytes)?;
                buf_after.push_many(run.as_bytes())?;
                line_width = run_width;
            } else {
                buf_after.push_byte_many(b' ', pending_spaces)?;
                buf_after.push_many(run.as_bytes())?;
                line_width += pending_spaces + run_width;
            }
            pending_spaces = 0;
        }

        buf_after.push_byte_many(b' ', pending_spaces)?;
        Ok(buf_after.len())
    }

    pub(crate) fn internal_wrap_maybrk(
        &mut self,
        prepend_str: Option<&str>,
        append_str: Option<&str>,
    ) -> Result<usize> {
        let bf_bytes = self.before.as_bytes();
        let bf_len = bf_bytes.len();

        let prepend_str = prepend_str.unwrap_or("");
        let append_str = append_str.unwrap_or("");

        if bf_len == 0 {
            return Ok(0);
        }

        let prepend_bytes = prepend_str.as_bytes();
        let append_bytes = append_str.as_bytes();
        let mut buf_after = OutBuf::new(bf_len, self.after);
        let mut seg_beg = 0usize;

        for (idx, byte) in bf_bytes.iter().enumerate() {
            if *byte == b'\n' {
                Self::wrap_maybrk_segment(
                    self.before,
                    self.max_width,
                    seg_beg,
                    idx,
                    prepend_bytes,
                    append_bytes,
                    &mut buf_after,
                )?;
                buf_after.push(b'\n')?;
                seg_beg = idx + 1;
            }
        }

        Self::wrap_maybrk_segment(
            self.before,
            self.max_width,
            seg_beg,
            bf_len,
            prepend_bytes,
            append_bytes,
            &mut buf_after,
        )?;

        Ok(buf_after.len())
    }

    fn push_maybrk_newline(
        prepend_bytes: &[u8],
        append_bytes: &[u8],
        buf_after: &mut OutBuf,
    ) -> Result<()> {
        buf_after.push_many(prepend_bytes)?;
        buf_after.push(b'\n')?;
        buf_after.push_many(append_bytes)
    }

    fn account_maybrk_unit(
        before: &str,
        line: &mut MayBrkLine,
        unit: MayBrkUnit,
        max_width: usize,
        prepend_bytes: &[u8],
        append_bytes: &[u8],
        buf_after: &mut OutBuf,
    ) -> Result<()> {
        if line.width + unit.width > max_width {
            buf_after.push_many(&before.as_bytes()[line.beg..unit.beg])?;
            Self::push_maybrk_newline(prepend_bytes, append_bytes, buf_after)?;
            line.beg = unit.beg;
            line.width = 0;
        }

        line.width += unit.width;

        if unit.has_more && line.width >= max_width {
            buf_after.push_many(&before.as_bytes()[line.beg..unit.end])?;
            Self::push_maybrk_newline(prepend_bytes, append_bytes, buf_after)?;
            line.beg = unit.end;
            line.width = 0;
        }

        Ok(())
    }

    fn wrap_maybrk_segment(
        before: &str,
        max_width: usize,
        mut line_beg: usize,
        seg_end: usize,
        prepend_bytes: &[u8],
        append_bytes: &[u8],
        buf_after: &mut OutBuf,
    ) -> Result<()> {
        if line_beg >= seg_end {
            return Ok(());
        }

        let segment = &before[line_beg..seg_end];
        if segment.is_ascii() {
            while seg_end - line_beg > max_width {
                let line_end = line_beg + max_width;
                buf_after.push_many(&before.as_bytes()[line_beg..line_end])?;
                Self::push_maybrk_newline(prepend_bytes, append_bytes, buf_after)?;
                line_beg = line_end;
            }
            buf_after.push_many(&before.as_bytes()[line_beg..seg_end])?;
            return Ok(());
        }

        if segment.len() <= max_width {
            buf_after.push_many(segment.as_bytes())?;
            return Ok(());
        }

        let mut line = MayBrkLine {
            beg: line_beg,
            width: 0,
        };
        let mut chars = before[line_beg..seg_end].char_indices();
        let (_, first) = chars.next().expect("non-empty segment");
        let mut unit_beg = line_beg;
        let mut unit_end = line_beg + first.len_utf8();
        let mut unit_width = width(first);
        let mut unit_last = first;
        let mut has_context = false;

        for (rel_idx, ch) in chars {
            let idx = line_beg + rel_idx;
            if continues_width_unit(unit_last, ch) {
                unit_end = idx + ch.len_utf8();
                unit_last = ch;
                has_context = true;
                continue;
            }

            if has_context {
                unit_width = UnicodeWidthStr::width(&before[unit_beg..unit_end]);
            }

            Self::account_maybrk_unit(
                before,
                &mut line,
                MayBrkUnit {
                    beg: unit_beg,
                    end: unit_end,
                    width: unit_width,
                    has_more: true,
                },
                max_width,
                prepend_bytes,
                append_bytes,
                buf_after,
            )?;

            unit_beg = idx;
            unit_end = idx + ch.len_utf8();
            unit_width = width(ch);
            unit_last = ch;
            has_context = false;
        }

        if has_context {
            unit_width = UnicodeWidthStr::width(&before[unit_beg..unit_end]);
        }

        Self::account_maybrk_unit(
            before,
            &mut line,
            MayBrkUnit {
                beg: unit_beg,
                end: unit_end,
                width: unit_width,
                has_more: false,
            },
            max_width,
            prepend_bytes,
            append_bytes,
            buf_after,
        )?;

        buf_after.push_many(&before.as_bytes()[line.beg..seg_end])?;

        Ok(())
    }
} // impl
