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

use unicode_width::UnicodeWidthChar;

#[inline]
pub(crate) fn is_width_continuation(ch: char) -> bool {
    if ch < '\u{0300}' || matches!(ch, '\u{FF00}'..='\u{FFFF}') {
        return false;
    }

    matches!(
        ch,
        '\u{0300}'..='\u{036F}'
            | '\u{0483}'..='\u{0489}'
            | '\u{0591}'..='\u{05BD}'
            | '\u{05BF}'
            | '\u{05C1}'..='\u{05C2}'
            | '\u{05C4}'..='\u{05C5}'
            | '\u{05C7}'
            | '\u{0610}'..='\u{061A}'
            | '\u{064B}'..='\u{065F}'
            | '\u{0670}'
            | '\u{06D6}'..='\u{06DC}'
            | '\u{06DF}'..='\u{06E4}'
            | '\u{06E7}'..='\u{06E8}'
            | '\u{06EA}'..='\u{06ED}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{200C}'..='\u{200D}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{2D7F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

#[inline]
pub(crate) fn is_regional_indicator(ch: char) -> bool {
    matches!(ch, '\u{1F1E6}'..='\u{1F1FF}')
}

#[inline]
pub(crate) fn extends_next(ch: char) -> bool {
    matches!(ch, '\u{200D}' | '\u{17D2}' | '\u{2D7F}')
}

#[inline]
pub(crate) fn continues_width_unit(prev: char, next: char) -> bool {
    extends_next(prev)
        || (is_regional_indicator(prev) && is_regional_indicator(next))
        || is_width_continuation(next)
}

#[inline]
pub(crate) fn width(ch: char) -> usize {
    if ch < '\u{A0}' {
        1
    } else if matches!(ch, '\u{FF01}'..='\u{FF60}' | '\u{FFE0}'..='\u{FFE6}') {
        2
    } else {
        ch.width().unwrap_or(1)
    }
}
