//! Explicit coordinate types and conversions.
//!
//! Char indices (`usize`) are the internal currency. These types exist so
//! that byte offsets, LSP UTF-16 positions and screen columns can never be
//! confused with them at a module boundary.

use ropey::{Rope, RopeSlice};

use crate::text::{grapheme_width, next_grapheme_boundary};

/// A byte offset into the UTF-8 encoding of a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteOffset(pub usize);

/// A Language Server Protocol position: zero-based line and UTF-16 code unit
/// offset within that line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

/// Zero-based line and terminal display column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineCol {
    pub line: usize,
    pub col: usize,
}

pub fn char_to_byte(text: &Rope, char_idx: usize) -> ByteOffset {
    ByteOffset(text.char_to_byte(char_idx.min(text.len_chars())))
}

/// Converts a byte offset to a char index. Returns `None` if the offset is
/// past the end or not on a char boundary.
pub fn byte_to_char(text: &Rope, byte: ByteOffset) -> Option<usize> {
    if byte.0 > text.len_bytes() {
        return None;
    }
    let c = text.byte_to_char(byte.0);
    (text.char_to_byte(c) == byte.0).then_some(c)
}

pub fn char_to_lsp(text: &Rope, char_idx: usize) -> LspPosition {
    let char_idx = char_idx.min(text.len_chars());
    let line = text.char_to_line(char_idx);
    let line_start = text.line_to_char(line);
    let character = text.char_to_utf16_cu(char_idx) - text.char_to_utf16_cu(line_start);
    LspPosition {
        line: line as u32,
        character: character as u32,
    }
}

/// Converts an LSP position to a char index. Following the LSP spec, a
/// `character` past the end of the line clamps to the line end (before its
/// terminator). A `line` past the end of the document returns `None`. A
/// position inside a surrogate pair rounds down to the char start.
pub fn lsp_to_char(text: &Rope, pos: LspPosition) -> Option<usize> {
    let line = pos.line as usize;
    if line >= text.len_lines() {
        return None;
    }
    let line_start = text.line_to_char(line);
    let line_end = crate::text::line_end_excluding_terminator(text.slice(..), line);
    let start_cu = text.char_to_utf16_cu(line_start);
    let end_cu = text.char_to_utf16_cu(line_end);
    let target = (start_cu + pos.character as usize).min(end_cu);
    Some(text.utf16_cu_to_char(target))
}

/// Display column of `char_idx` within its line, with tabs expanded.
pub fn char_to_line_col(text: RopeSlice, char_idx: usize, tab_width: usize) -> LineCol {
    let char_idx = char_idx.min(text.len_chars());
    let line = text.char_to_line(char_idx);
    let mut pos = text.line_to_char(line);
    let mut col = 0;
    while pos < char_idx {
        let next = next_grapheme_boundary(text, pos);
        let g = text.slice(pos..next);
        let g = g.to_string();
        col += grapheme_width(&g, col, tab_width);
        pos = next;
    }
    LineCol { line, col }
}

/// The char index of the grapheme that covers display column `col` on
/// `line`, or the line end if the line is shorter. Never lands inside a
/// grapheme or on the far side of the line terminator.
pub fn line_col_to_char(text: RopeSlice, line_col: LineCol, tab_width: usize) -> usize {
    let line = line_col.line.min(text.len_lines().saturating_sub(1));
    let mut pos = text.line_to_char(line);
    let end = crate::text::line_end_excluding_terminator(text, line);
    let mut col = 0;
    while pos < end {
        let next = next_grapheme_boundary(text, pos);
        let g = text.slice(pos..next).to_string();
        let w = grapheme_width(&g, col, tab_width);
        if col + w > line_col.col {
            return pos;
        }
        col += w;
        pos = next;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_round_trip_with_astral_chars() {
        // U+1F600 is two UTF-16 code units, one char.
        let rope = Rope::from_str("a😀b\nxyz");
        assert_eq!(
            char_to_lsp(&rope, 2),
            LspPosition {
                line: 0,
                character: 3
            }
        );
        assert_eq!(
            lsp_to_char(
                &rope,
                LspPosition {
                    line: 0,
                    character: 3
                }
            ),
            Some(2)
        );
        // Past end of line clamps before the newline.
        assert_eq!(
            lsp_to_char(
                &rope,
                LspPosition {
                    line: 0,
                    character: 99
                }
            ),
            Some(3)
        );
        assert_eq!(
            lsp_to_char(
                &rope,
                LspPosition {
                    line: 1,
                    character: 1
                }
            ),
            Some(5)
        );
        assert_eq!(
            lsp_to_char(
                &rope,
                LspPosition {
                    line: 7,
                    character: 0
                }
            ),
            None
        );
        for c in 0..=rope.len_chars() {
            assert_eq!(lsp_to_char(&rope, char_to_lsp(&rope, c)), Some(c));
        }
    }

    #[test]
    fn byte_conversion_rejects_mid_char_offsets() {
        let rope = Rope::from_str("héllo");
        assert_eq!(byte_to_char(&rope, ByteOffset(1)), Some(1));
        assert_eq!(byte_to_char(&rope, ByteOffset(2)), None); // inside 'é'
        assert_eq!(byte_to_char(&rope, ByteOffset(3)), Some(2));
        assert_eq!(byte_to_char(&rope, ByteOffset(99)), None);
        assert_eq!(char_to_byte(&rope, 2), ByteOffset(3));
    }

    #[test]
    fn display_columns_expand_tabs_and_wide_chars() {
        let rope = Rope::from_str("\tx漢y\n");
        let s = rope.slice(..);
        assert_eq!(char_to_line_col(s, 1, 4).col, 4);
        assert_eq!(char_to_line_col(s, 2, 4).col, 5);
        assert_eq!(char_to_line_col(s, 3, 4).col, 7);
        // Column 6 is the right half of the wide char; it maps to its start.
        assert_eq!(line_col_to_char(s, LineCol { line: 0, col: 6 }, 4), 2);
        assert_eq!(line_col_to_char(s, LineCol { line: 0, col: 2 }, 4), 0);
        assert_eq!(line_col_to_char(s, LineCol { line: 0, col: 99 }, 4), 4);
    }
}
