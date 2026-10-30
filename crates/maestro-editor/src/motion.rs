//! Pure motion and text-object functions. Each takes the text and a range and
//! returns a new range; commands decide whether to extend or replace.

use maestro_core::pos::{LineCol, char_to_line_col, line_col_to_char};
use maestro_core::text::{
    CharClass, char_class, is_line_ending_char, line_end_excluding_terminator,
    next_grapheme_boundary, prev_grapheme_boundary,
};
use maestro_core::{Range, RopeSlice};

/// Index of the last navigable line. A trailing newline does not create an
/// extra line, matching how Vim and Helix present files.
pub fn last_line(text: RopeSlice) -> usize {
    let n = text.len_lines();
    let len = text.len_chars();
    if n > 1 && len > 0 && is_line_ending_char(text.char(len - 1)) {
        n - 2
    } else {
        n - 1
    }
}

pub fn char_left(text: RopeSlice, pos: usize, count: usize) -> usize {
    let line_start = text.line_to_char(text.char_to_line(pos));
    let mut p = pos;
    for _ in 0..count {
        if p <= line_start {
            break;
        }
        p = prev_grapheme_boundary(text, p);
    }
    p
}

/// Moves right within the line. In normal mode the cursor may rest on the
/// line terminator (so empty lines are reachable) but not beyond it.
pub fn char_right(text: RopeSlice, pos: usize, count: usize) -> usize {
    let line = text.char_to_line(pos);
    let end = line_end_excluding_terminator(text, line);
    let mut p = pos;
    for _ in 0..count {
        if p >= end {
            break;
        }
        p = next_grapheme_boundary(text, p);
    }
    p
}

/// Moves vertically, keeping `col` (a display column) where possible.
pub fn line_vertical(
    text: RopeSlice,
    pos: usize,
    delta: isize,
    col: usize,
    tab_width: usize,
) -> usize {
    let line = text.char_to_line(pos) as isize;
    let target = (line + delta).clamp(0, last_line(text) as isize) as usize;
    line_col_to_char(text, LineCol { line: target, col }, tab_width)
}

pub fn display_col(text: RopeSlice, pos: usize, tab_width: usize) -> usize {
    char_to_line_col(text, pos, tab_width).col
}

pub fn line_start(text: RopeSlice, pos: usize) -> usize {
    text.line_to_char(text.char_to_line(pos))
}

/// The last grapheme on the line (the cursor position for `$`), or the line
/// start for an empty line.
pub fn line_end(text: RopeSlice, pos: usize) -> usize {
    let line = text.char_to_line(pos);
    let start = text.line_to_char(line);
    let end = line_end_excluding_terminator(text, line);
    if end > start {
        prev_grapheme_boundary(text, end)
    } else {
        start
    }
}

pub fn first_non_blank(text: RopeSlice, pos: usize) -> usize {
    let line = text.char_to_line(pos);
    let start = text.line_to_char(line);
    let end = line_end_excluding_terminator(text, line);
    (start..end)
        .find(|&i| !matches!(text.char(i), ' ' | '\t'))
        .unwrap_or(start)
}

pub fn goto_line(text: RopeSlice, line: usize) -> usize {
    let line = line.min(last_line(text));
    first_non_blank(text, text.line_to_char(line))
}

fn class_at(text: RopeSlice, i: usize, big: bool) -> CharClass {
    let c = char_class(text.char(i));
    if big && c == CharClass::Punctuation {
        CharClass::Word
    } else {
        c
    }
}

/// `w`: selects from the cursor to just before the start of the next word,
/// including trailing whitespace.
pub fn next_word_start(text: RopeSlice, range: Range, big: bool) -> Range {
    let len = text.len_chars();
    if len == 0 {
        return range;
    }
    let mut s = range.head.min(len - 1);
    if s + 1 < len && class_at(text, s, big) != class_at(text, s + 1, big) {
        s += 1;
    }
    while s + 1 < len && class_at(text, s, big) == CharClass::Eol {
        s += 1;
    }
    let mut i = s;
    let cls = class_at(text, i, big);
    if cls != CharClass::Whitespace && cls != CharClass::Eol {
        while i + 1 < len && class_at(text, i + 1, big) == cls {
            i += 1;
        }
    }
    while i + 1 < len && class_at(text, i + 1, big) == CharClass::Whitespace {
        i += 1;
    }
    Range::new(s, i)
}

/// `e`: selects from the cursor to the end of the current or next word.
pub fn next_word_end(text: RopeSlice, range: Range, big: bool) -> Range {
    let len = text.len_chars();
    if len == 0 {
        return range;
    }
    let mut s = range.head.min(len - 1);
    if s + 1 < len && class_at(text, s, big) != class_at(text, s + 1, big) {
        s += 1;
    }
    let anchor = s;
    let mut i = s;
    while i + 1 < len
        && matches!(
            class_at(text, i, big),
            CharClass::Whitespace | CharClass::Eol
        )
    {
        i += 1;
    }
    let cls = class_at(text, i, big);
    while i + 1 < len && class_at(text, i + 1, big) == cls {
        i += 1;
    }
    Range::new(anchor, i)
}

/// `b`: selects from the cursor back to the start of the current or
/// previous word.
pub fn prev_word_start(text: RopeSlice, range: Range, big: bool) -> Range {
    let len = text.len_chars();
    if len == 0 {
        return range;
    }
    let mut s = range.head.min(len - 1);
    if s > 0 && class_at(text, s, big) != class_at(text, s - 1, big) {
        s -= 1;
    }
    let anchor = s;
    let mut i = s;
    while i > 0
        && matches!(
            class_at(text, i, big),
            CharClass::Whitespace | CharClass::Eol
        )
    {
        i -= 1;
    }
    let cls = class_at(text, i, big);
    while i > 0 && class_at(text, i - 1, big) == cls {
        i -= 1;
    }
    Range::new(anchor, i)
}

/// `f`/`t`/`F`/`T`. Searches within the current line only. Returns `None`
/// when the character does not occur.
pub fn find_char(
    text: RopeSlice,
    pos: usize,
    ch: char,
    forward: bool,
    till: bool,
    count: usize,
) -> Option<usize> {
    let line = text.char_to_line(pos);
    let start = text.line_to_char(line);
    let end = line_end_excluding_terminator(text, line);
    let mut p = pos;
    let mut found = None;
    for _ in 0..count.max(1) {
        let hit = if forward {
            ((p + 1).min(end)..end).find(|&i| text.char(i) == ch)
        } else {
            (start..p.max(start)).rev().find(|&i| text.char(i) == ch)
        };
        match hit {
            Some(h) => {
                p = h;
                found = Some(h);
            }
            None => break,
        }
    }
    let hit = found?;
    Some(match (till, forward) {
        (false, _) => hit,
        (true, true) => hit - 1,
        (true, false) => hit + 1,
    })
}

/// The whole line(s) covered by `range`, including terminators.
pub fn line_range(text: RopeSlice, range: Range) -> Range {
    let first = text.char_to_line(range.from());
    let last = text.char_to_line(range.last());
    let start = text.line_to_char(first);
    let end = text.line_to_char((last + 1).min(text.len_lines()));
    let end = if end > start {
        prev_grapheme_boundary(text, end)
    } else {
        start
    };
    Range::new(start, end)
}

const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];

pub fn matching_pair(c: char) -> Option<(char, char)> {
    PAIRS
        .iter()
        .copied()
        .find(|(o, cl)| *o == c || *cl == c)
        .or(match c {
            '"' | '\'' | '`' => Some((c, c)),
            _ => None,
        })
}

/// Finds the innermost `open`/`close` pair enclosing `pos`. Returns the
/// positions of the opening and closing characters. Quotes (same open and
/// close) only search the current line.
pub fn surrounding_pair(
    text: RopeSlice,
    pos: usize,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let len = text.len_chars();
    if len == 0 {
        return None;
    }
    let pos = pos.min(len - 1);
    if open == close {
        let line = text.char_to_line(pos);
        let start = text.line_to_char(line);
        let end = line_end_excluding_terminator(text, line);
        let quotes: Vec<usize> = (start..end).filter(|&i| text.char(i) == open).collect();
        // Pair quotes left to right and pick the pair containing pos.
        return quotes
            .chunks_exact(2)
            .map(|w| (w[0], w[1]))
            .find(|(a, b)| *a <= pos && pos <= *b);
    }
    let c = text.char(pos);
    if c == close {
        return find_open_backward(text, pos, open, close).map(|o| (o, pos));
    }
    if c == open {
        return find_close_forward(text, pos, open, close).map(|cl| (pos, cl));
    }
    let o = find_open_backward(text, pos, open, close)?;
    let cl = find_close_forward(text, o, open, close)?;
    Some((o, cl))
}

/// The nearest unmatched `open` strictly before `pos`.
fn find_open_backward(text: RopeSlice, pos: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for i in (0..pos).rev() {
        let c = text.char(i);
        if c == close {
            depth += 1;
        } else if c == open {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

/// The `close` matching the `open` at `open_pos`.
fn find_close_forward(text: RopeSlice, open_pos: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for j in open_pos + 1..text.len_chars() {
        let c = text.char(j);
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some(j);
            }
            depth -= 1;
        }
    }
    None
}

/// Jumps to the bracket matching the one under the cursor (`mm`).
pub fn match_bracket(text: RopeSlice, pos: usize) -> Option<usize> {
    if pos >= text.len_chars() {
        return None;
    }
    let c = text.char(pos);
    let (open, close) = PAIRS.iter().copied().find(|(o, cl)| *o == c || *cl == c)?;
    let (a, b) = surrounding_pair(text, pos, open, close)?;
    Some(if a == pos { b } else { a })
}

/// Text object kinds for `mi` / `ma`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Inside,
    Around,
}

/// A text object selected around `range.head`: `w` word, `W` WORD, `p`
/// paragraph, or a bracket/quote character.
pub fn text_object(text: RopeSlice, range: Range, kind: ObjectKind, obj: char) -> Option<Range> {
    let len = text.len_chars();
    if len == 0 {
        return None;
    }
    let pos = range.head.min(len - 1);
    match obj {
        'w' | 'W' => {
            let big = obj == 'W';
            let cls = class_at(text, pos, big);
            let mut a = pos;
            while a > 0 && class_at(text, a - 1, big) == cls {
                a -= 1;
            }
            let mut b = pos;
            while b + 1 < len && class_at(text, b + 1, big) == cls {
                b += 1;
            }
            if kind == ObjectKind::Around {
                while b + 1 < len && class_at(text, b + 1, big) == CharClass::Whitespace {
                    b += 1;
                }
            }
            Some(Range::new(a, b))
        }
        'p' => {
            let blank = |l: usize| text.line(l).chars().all(char::is_whitespace);
            let mut first = text.char_to_line(pos);
            let mut last = first;
            let last_line = last_line(text);
            let on_blank = blank(first);
            while first > 0 && blank(first - 1) == on_blank {
                first -= 1;
            }
            while last < last_line && blank(last + 1) == on_blank {
                last += 1;
            }
            if kind == ObjectKind::Around {
                while last < last_line && blank(last + 1) {
                    last += 1;
                }
            }
            let start = text.line_to_char(first);
            let end = text.line_to_char((last + 1).min(text.len_lines()));
            Some(Range::new(
                start,
                prev_grapheme_boundary(text, end.max(start + 1)),
            ))
        }
        c => {
            let (open, close) = matching_pair(c)?;
            let (a, b) = surrounding_pair(text, pos, open, close)?;
            match kind {
                ObjectKind::Around => Some(Range::new(a, b)),
                ObjectKind::Inside if b > a + 1 => Some(Range::new(a + 1, b - 1)),
                ObjectKind::Inside => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_core::Rope;

    fn sel(text: &str, r: Range) -> String {
        let rope = Rope::from_str(text);
        r.fragment(rope.slice(..))
    }

    #[test]
    fn word_motions() {
        let t = "foo bar.baz  qux";
        let rope = Rope::from_str(t);
        let s = rope.slice(..);
        let r = next_word_start(s, Range::point(0), false);
        assert_eq!(sel(t, r), "foo ");
        let r = next_word_start(s, r, false);
        assert_eq!(sel(t, r), "bar");
        let r = next_word_start(s, r, false);
        assert_eq!(sel(t, r), ".");
        let r = next_word_start(s, Range::point(4), true);
        assert_eq!(sel(t, r), "bar.baz  ");
        let r = next_word_end(s, Range::point(0), false);
        assert_eq!(sel(t, r), "foo");
        let r = next_word_end(s, r, false);
        assert_eq!(sel(t, r), " bar");
        let r = prev_word_start(s, Range::point(15), false);
        assert_eq!(r.head, 13);
        assert_eq!(sel(t, r), "qux");
    }

    #[test]
    fn vertical_motion_keeps_column_and_clamps() {
        let rope = Rope::from_str("abcdef\nab\nabcdef\n");
        let s = rope.slice(..);
        assert_eq!(last_line(s), 2);
        let p = line_vertical(s, 4, 1, 4, 4);
        assert_eq!(p, 9); // clamped to end of "ab" (the newline)
        let p = line_vertical(s, p, 1, 4, 4);
        assert_eq!(p, 14);
        assert_eq!(line_vertical(s, 14, 5, 4, 4), 14);
    }

    #[test]
    fn find_and_till() {
        let rope = Rope::from_str("a,b,c,d\n");
        let s = rope.slice(..);
        assert_eq!(find_char(s, 0, ',', true, false, 1), Some(1));
        assert_eq!(find_char(s, 0, ',', true, false, 2), Some(3));
        assert_eq!(find_char(s, 0, ',', true, true, 1), Some(0));
        assert_eq!(find_char(s, 0, 'z', true, false, 1), None);
        assert_eq!(find_char(s, 6, ',', false, false, 1), Some(5));
        assert_eq!(find_char(s, 6, ',', false, true, 1), Some(6));
    }

    #[test]
    fn objects_and_pairs() {
        let t = "fn f(a, (b), c) { \"q s\" }";
        let rope = Rope::from_str(t);
        let s = rope.slice(..);
        let r = text_object(s, Range::point(6), ObjectKind::Inside, '(').unwrap();
        assert_eq!(sel(t, r), "a, (b), c");
        let r = text_object(s, Range::point(9), ObjectKind::Around, '(').unwrap();
        assert_eq!(sel(t, r), "(b)");
        let r = text_object(s, Range::point(20), ObjectKind::Inside, '"').unwrap();
        assert_eq!(sel(t, r), "q s");
        let r = text_object(s, Range::point(1), ObjectKind::Inside, 'w').unwrap();
        assert_eq!(sel(t, r), "fn");
        assert_eq!(match_bracket(s, 4), Some(14));
        assert_eq!(match_bracket(s, 14), Some(4));
        assert_eq!(match_bracket(s, 16), Some(24));
    }

    #[test]
    fn line_ranges() {
        let t = "one\ntwo\nthree";
        let rope = Rope::from_str(t);
        let s = rope.slice(..);
        assert_eq!(sel(t, line_range(s, Range::point(5))), "two\n");
        assert_eq!(sel(t, line_range(s, Range::new(1, 9))), "one\ntwo\nthree");
    }
}
