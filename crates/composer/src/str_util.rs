//! UTF-8 ↔ UTF-16 offset conversion and grapheme-boundary helpers.
//!
//! `EntityInputHandler` speaks UTF-16 code units (the macOS NSTextInputClient
//! contract); the composer stores UTF-8 byte offsets internally. These free
//! functions bridge the two encodings. A BMP character is one UTF-16 unit,
//! an astral character (e.g. emoji) is two.

use std::ops::Range;

pub fn byte_to_utf16(text: &str, byte: usize) -> usize {
    let mut utf16 = 0;
    for (b, ch) in text.char_indices() {
        if b >= byte {
            break;
        }
        utf16 += ch.len_utf16();
    }
    utf16
}

pub fn utf16_to_byte(text: &str, utf16: usize) -> usize {
    let mut count = 0;
    for (b, ch) in text.char_indices() {
        if count >= utf16 {
            return b;
        }
        count += ch.len_utf16();
    }
    text.len()
}

pub fn byte_range_to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    byte_to_utf16(text, range.start)..byte_to_utf16(text, range.end)
}

pub fn utf16_range_to_byte(text: &str, range: Range<usize>) -> Range<usize> {
    utf16_to_byte(text, range.start)..utf16_to_byte(text, range.end)
}

/// Previous grapheme boundary strictly before `offset` (or 0).
pub fn previous_boundary(text: &str, offset: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    text.grapheme_indices(true)
        .rev()
        .find_map(|(idx, _)| (idx < offset).then_some(idx))
        .unwrap_or(0)
}

/// Next grapheme boundary strictly after `offset` (or `text.len()`).
pub fn next_boundary(text: &str, offset: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    text.grapheme_indices(true)
        .find_map(|(idx, _)| (idx > offset).then_some(idx))
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_round_trip() {
        let s = "hello";
        assert_eq!(byte_to_utf16(s, 3), 3);
        assert_eq!(utf16_to_byte(s, 3), 3);
    }

    #[test]
    fn cjk_one_utf16_unit_per_char() {
        let s = "你好";
        assert_eq!(byte_to_utf16(s, 6), 2); // two chars, 3 bytes each
        assert_eq!(utf16_to_byte(s, 1), 3);
        assert_eq!(utf16_to_byte(s, 2), 6);
    }

    #[test]
    fn astral_emoji_two_utf16_units() {
        let s = "a🎉b"; // 🎉 is U+1F389: 4 bytes UTF-8, 2 units UTF-16
        assert_eq!(byte_to_utf16(s, 1), 1); // 'a'
        assert_eq!(byte_to_utf16(s, 5), 3); // 'a' + emoji = 1 + 2 units
        assert_eq!(utf16_to_byte(s, 3), 5); // back to emoji end
        assert_eq!(utf16_range_to_byte(s, 1..3), 1..5);
    }
}
