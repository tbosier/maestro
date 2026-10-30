//! Grapheme and character-class helpers over Ropey slices.
//!
//! All indices are char indices. Grapheme boundaries come from
//! `unicode-segmentation` (extended grapheme clusters), so a CRLF pair, an
//! emoji ZWJ sequence or a base letter plus combining marks each count as one
//! unit of cursor movement.

use ropey::RopeSlice;
use ropey::str_utils::byte_to_char_idx;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};
use unicode_width::UnicodeWidthStr;

/// Returns the char index of the next grapheme boundary after `char_idx`.
/// Returns `slice.len_chars()` at the end of the text.
pub fn next_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    let len = slice.len_chars();
    if char_idx >= len {
        return len;
    }
    let byte_idx = slice.char_to_byte(char_idx);
    let (mut chunk, mut chunk_byte_idx, mut chunk_char_idx, _) = slice.chunk_at_byte(byte_idx);
    let mut cursor = GraphemeCursor::new(byte_idx, slice.len_bytes(), true);
    loop {
        match cursor.next_boundary(chunk, chunk_byte_idx) {
            Ok(None) => return len,
            Ok(Some(n)) => return chunk_char_idx + byte_to_char_idx(chunk, n - chunk_byte_idx),
            Err(GraphemeIncomplete::NextChunk) => {
                chunk_byte_idx += chunk.len();
                let (c, _, ci, _) = slice.chunk_at_byte(chunk_byte_idx);
                chunk = c;
                chunk_char_idx = ci;
            }
            Err(GraphemeIncomplete::PreContext(n)) => {
                let ctx = slice.chunk_at_byte(n - 1).0;
                cursor.provide_context(ctx, n - ctx.len());
            }
            Err(_) => unreachable!("unexpected grapheme cursor state"),
        }
    }
}

/// Returns the char index of the previous grapheme boundary before `char_idx`.
/// Returns 0 at the start of the text.
pub fn prev_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    let char_idx = char_idx.min(slice.len_chars());
    let byte_idx = slice.char_to_byte(char_idx);
    let (mut chunk, mut chunk_byte_idx, mut chunk_char_idx, _) = slice.chunk_at_byte(byte_idx);
    let mut cursor = GraphemeCursor::new(byte_idx, slice.len_bytes(), true);
    loop {
        match cursor.prev_boundary(chunk, chunk_byte_idx) {
            Ok(None) => return 0,
            Ok(Some(n)) => return chunk_char_idx + byte_to_char_idx(chunk, n - chunk_byte_idx),
            Err(GraphemeIncomplete::PrevChunk) => {
                let (c, bi, ci, _) = slice.chunk_at_byte(chunk_byte_idx - 1);
                chunk = c;
                chunk_byte_idx = bi;
                chunk_char_idx = ci;
            }
            Err(GraphemeIncomplete::PreContext(n)) => {
                let ctx = slice.chunk_at_byte(n - 1).0;
                cursor.provide_context(ctx, n - ctx.len());
            }
            Err(_) => unreachable!("unexpected grapheme cursor state"),
        }
    }
}

/// True if `char_idx` sits on a grapheme boundary.
pub fn is_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> bool {
    if char_idx == 0 || char_idx >= slice.len_chars() {
        return true;
    }
    let byte_idx = slice.char_to_byte(char_idx);
    let (chunk, chunk_byte_idx, _, _) = slice.chunk_at_byte(byte_idx);
    let mut cursor = GraphemeCursor::new(byte_idx, slice.len_bytes(), true);
    loop {
        match cursor.is_boundary(chunk, chunk_byte_idx) {
            Ok(b) => return b,
            Err(GraphemeIncomplete::PreContext(n)) => {
                let ctx = slice.chunk_at_byte(n - 1).0;
                cursor.provide_context(ctx, n - ctx.len());
            }
            Err(_) => unreachable!("unexpected grapheme cursor state"),
        }
    }
}

/// Rounds `char_idx` down to the nearest grapheme boundary.
pub fn floor_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    let char_idx = char_idx.min(slice.len_chars());
    if is_grapheme_boundary(slice, char_idx) {
        char_idx
    } else {
        prev_grapheme_boundary(slice, char_idx)
    }
}

/// Iterates over the graphemes of a slice as owned-or-borrowed strings.
pub fn graphemes(slice: RopeSlice<'_>) -> impl Iterator<Item = std::borrow::Cow<'_, str>> {
    let mut pos = 0;
    let len = slice.len_chars();
    std::iter::from_fn(move || {
        if pos >= len {
            return None;
        }
        let next = next_grapheme_boundary(slice, pos);
        let g = slice.slice(pos..next);
        pos = next;
        Some(match g.as_str() {
            Some(s) => std::borrow::Cow::Borrowed(s),
            None => std::borrow::Cow::Owned(g.to_string()),
        })
    })
}

/// Display width of a single grapheme in terminal cells, given the column it
/// starts at (needed for tab expansion). Control characters render as a
/// caret pair (`^X`), so they occupy two cells.
pub fn grapheme_width(g: &str, col: usize, tab_width: usize) -> usize {
    match g {
        "\t" => {
            let tab_width = tab_width.max(1);
            tab_width - (col % tab_width)
        }
        _ if is_line_ending_str(g) => 0,
        _ if g.chars().next().is_some_and(char::is_control) => 2,
        _ => g.width().max(if g.is_empty() { 0 } else { 1 }),
    }
}

/// True for every line terminator Ropey recognizes.
pub fn is_line_ending_str(s: &str) -> bool {
    matches!(
        s,
        "\n" | "\r\n" | "\r" | "\u{000B}" | "\u{000C}" | "\u{0085}" | "\u{2028}" | "\u{2029}"
    )
}

pub fn is_line_ending_char(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{000B}' | '\u{000C}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

/// Character classes used by word motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    Whitespace,
    Eol,
    Word,
    Punctuation,
}

pub fn char_class(c: char) -> CharClass {
    if is_line_ending_char(c) {
        CharClass::Eol
    } else if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

/// Char index of the end of line `line`, excluding its line terminator.
pub fn line_end_excluding_terminator(slice: RopeSlice, line: usize) -> usize {
    let start = slice.line_to_char(line);
    let line_slice = slice.line(line);
    let mut len = line_slice.len_chars();
    // Strip CRLF or a single terminator.
    if len > 0 && is_line_ending_char(line_slice.char(len - 1)) {
        len -= 1;
        if len > 0 && line_slice.char(len) == '\n' && line_slice.char(len - 1) == '\r' {
            len -= 1;
        }
    }
    start + len
}

/// The leading whitespace of a line, as a string.
pub fn line_indent(slice: RopeSlice, line: usize) -> String {
    slice
        .line(line)
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn grapheme_boundaries_handle_combining_marks_and_crlf() {
        // "e" + combining acute, CRLF, family emoji (ZWJ sequence), "x".
        let rope = Rope::from_str("e\u{301}\r\n👨\u{200d}👩\u{200d}👧x");
        let s = rope.slice(..);
        assert_eq!(next_grapheme_boundary(s, 0), 2);
        assert_eq!(next_grapheme_boundary(s, 2), 4); // CRLF is one grapheme
        assert_eq!(next_grapheme_boundary(s, 4), 9); // 5-char ZWJ family
        assert_eq!(next_grapheme_boundary(s, 9), 10);
        assert_eq!(next_grapheme_boundary(s, 10), 10);
        assert_eq!(prev_grapheme_boundary(s, 10), 9);
        assert_eq!(prev_grapheme_boundary(s, 9), 4);
        assert_eq!(prev_grapheme_boundary(s, 4), 2);
        assert_eq!(prev_grapheme_boundary(s, 2), 0);
        assert!(!is_grapheme_boundary(s, 1));
        assert!(!is_grapheme_boundary(s, 3));
        assert_eq!(floor_grapheme_boundary(s, 6), 4);
    }

    #[test]
    fn widths() {
        assert_eq!(grapheme_width("a", 0, 4), 1);
        assert_eq!(grapheme_width("漢", 0, 4), 2);
        assert_eq!(grapheme_width("\t", 0, 4), 4);
        assert_eq!(grapheme_width("\t", 3, 4), 1);
        assert_eq!(grapheme_width("\t", 4, 4), 4);
        assert_eq!(grapheme_width("\n", 0, 4), 0);
        assert_eq!(grapheme_width("\u{7}", 0, 4), 2);
    }

    #[test]
    fn line_end_strips_terminators() {
        let rope = Rope::from_str("ab\r\ncd\nef");
        let s = rope.slice(..);
        assert_eq!(line_end_excluding_terminator(s, 0), 2);
        assert_eq!(line_end_excluding_terminator(s, 1), 6);
        assert_eq!(line_end_excluding_terminator(s, 2), 9);
    }

    #[test]
    fn boundaries_across_chunk_edges() {
        // Force many chunks and check every combining sequence is respected.
        let unit = "a\u{301}b";
        let text = unit.repeat(5000);
        let rope = Rope::from_str(&text);
        let s = rope.slice(..);
        let mut pos = 0;
        let mut count = 0;
        while pos < s.len_chars() {
            pos = next_grapheme_boundary(s, pos);
            count += 1;
        }
        assert_eq!(count, 10_000);
        let mut back = 0;
        while pos > 0 {
            pos = prev_grapheme_boundary(s, pos);
            back += 1;
        }
        assert_eq!(back, 10_000);
    }
}
