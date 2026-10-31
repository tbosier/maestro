//! Changed regions inside a pair of lines, for highlighting a review diff.
//!
//! The lines are split into tokens (runs of word characters, runs of
//! whitespace, and single other characters) and the token sequences are
//! diffed with Myers. Token boundaries are char boundaries, so every range
//! can slice the line it belongs to.

// A one-element list of ranges ("the whole line changed") is the intent
// here, not a mistyped `(0..n).collect()`.
#![allow(clippy::single_range_in_vec_init)]

use std::ops::Range;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp};

/// Longer lines (in bytes, either side) are marked as wholly changed
/// without diffing. Minified files and generated data lines are the usual
/// cause; a highlight inside them would not help anyone read the change.
pub const INTRALINE_MAX_LINE: usize = 1024;

/// Below this share of matching non-whitespace text the pair is marked as
/// wholly changed: scattered fragments of a rewritten line read worse than
/// one solid mark.
const MIN_SIMILARITY: f32 = 0.4;

/// A safety net on top of the length cap.
const DEADLINE: Duration = Duration::from_millis(5);

/// Changed byte ranges in `old` (a removed line) and `new` (the added line
/// paired with it). Every range starts and ends on a char boundary, ranges
/// are sorted and do not touch each other. Identical lines give two empty
/// lists. Lines longer than [`INTRALINE_MAX_LINE`] bytes, or too different
/// to pair usefully, give one range covering each whole non-empty line.
pub fn intraline(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    if old == new {
        return (Vec::new(), Vec::new());
    }
    if old.is_empty()
        || new.is_empty()
        || old.len() > INTRALINE_MAX_LINE
        || new.len() > INTRALINE_MAX_LINE
    {
        return whole(old, new);
    }
    let a = tokens(old);
    let b = tokens(new);
    let ops = similar::capture_diff_slices_deadline(
        Algorithm::Myers,
        &a,
        &b,
        Some(Instant::now() + DEADLINE),
    );

    let span = |toks: &[&str], base: &[usize], i: usize, n: usize| -> Range<usize> {
        let start = base[i];
        let end = if n == 0 {
            start
        } else {
            base[i + n - 1] + toks[i + n - 1].len()
        };
        start..end
    };
    let offsets = |toks: &[&str]| -> Vec<usize> {
        let mut at = 0;
        toks.iter()
            .map(|t| {
                let s = at;
                at += t.len();
                s
            })
            .collect()
    };
    let (oa, ob) = (offsets(&a), offsets(&b));

    let mut same = 0usize;
    let mut old_ranges = Vec::new();
    let mut new_ranges = Vec::new();
    for op in ops {
        match op {
            DiffOp::Equal { old_index, len, .. } => {
                same += a[old_index..old_index + len]
                    .iter()
                    .map(|t| non_ws_len(t))
                    .sum::<usize>();
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => push(&mut old_ranges, span(&a, &oa, old_index, old_len)),
            DiffOp::Insert {
                new_index, new_len, ..
            } => push(&mut new_ranges, span(&b, &ob, new_index, new_len)),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                push(&mut old_ranges, span(&a, &oa, old_index, old_len));
                push(&mut new_ranges, span(&b, &ob, new_index, new_len));
            }
        }
    }
    let total = non_ws_len(old) + non_ws_len(new);
    if total > 0 && (2 * same) as f32 / (total as f32) < MIN_SIMILARITY {
        return whole(old, new);
    }
    (old_ranges, new_ranges)
}

fn whole(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let one = |s: &str| {
        if s.is_empty() {
            Vec::new()
        } else {
            vec![0..s.len()]
        }
    };
    (one(old), one(new))
}

/// Appends `r`, joining it to the previous range when they touch.
fn push(ranges: &mut Vec<Range<usize>>, r: Range<usize>) {
    if r.is_empty() {
        return;
    }
    if let Some(last) = ranges.last_mut()
        && last.end >= r.start
    {
        last.end = last.end.max(r.end);
        return;
    }
    ranges.push(r);
}

fn non_ws_len(s: &str) -> usize {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .map(char::len_utf8)
        .sum()
}

#[derive(PartialEq, Eq)]
enum Class {
    Word,
    Space,
    Other,
}

fn class(c: char) -> Class {
    if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else if c.is_whitespace() {
        Class::Space
    } else {
        Class::Other
    }
}

/// Runs of word characters, runs of whitespace, and single other chars.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<Class> = None;
    for (i, c) in s.char_indices() {
        let k = class(c);
        let joins = match &prev {
            Some(p) => *p == k && k != Class::Other,
            None => true,
        };
        if !joins {
            out.push(&s[start..i]);
            start = i;
        }
        prev = Some(k);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked<'a>(s: &'a str, rs: &[Range<usize>]) -> Vec<&'a str> {
        rs.iter().map(|r| &s[r.clone()]).collect()
    }

    #[test]
    fn ascii_word_change() {
        let old = "let total = price * count;";
        let new = "let total = price * amount;";
        let (o, n) = intraline(old, new);
        assert_eq!(marked(old, &o), vec!["count"]);
        assert_eq!(marked(new, &n), vec!["amount"]);
    }

    #[test]
    fn insertion_only_marks_the_new_side() {
        let old = "call(a, b)";
        let new = "call(a, b, c)";
        let (o, n) = intraline(old, new);
        assert!(o.is_empty(), "{o:?}");
        assert_eq!(marked(new, &n), vec![", c"]);
    }

    #[test]
    fn identical_and_empty_lines() {
        assert_eq!(intraline("same", "same"), (vec![], vec![]));
        assert_eq!(intraline("", ""), (vec![], vec![]));
        assert_eq!(intraline("", "new"), (vec![], vec![0..3]));
        assert_eq!(intraline("old", ""), (vec![0..3], vec![]));
    }

    #[test]
    fn multi_byte_ranges_sit_on_char_boundaries() {
        let old = "naïve café → résumé";
        let new = "naïve cafés → résumé ✓";
        let (o, n) = intraline(old, new);
        for r in &o {
            assert!(old.is_char_boundary(r.start) && old.is_char_boundary(r.end));
        }
        for r in &n {
            assert!(new.is_char_boundary(r.start) && new.is_char_boundary(r.end));
        }
        assert_eq!(marked(old, &o), vec!["café"]);
        assert_eq!(marked(new, &n), vec!["cafés", " ✓"]);

        // Emoji and CJK words.
        let (o, n) = intraline("日本語 😀 ok", "日本語 😃 ok");
        assert_eq!(marked("日本語 😀 ok", &o), vec!["😀"]);
        assert_eq!(marked("日本語 😃 ok", &n), vec!["😃"]);
    }

    #[test]
    fn whitespace_changes_are_marked() {
        let (o, n) = intraline("  x = 1", "    x = 1");
        assert_eq!(o, vec![0..2]);
        assert_eq!(n, vec![0..4]);
    }

    #[test]
    fn long_lines_fall_back_to_whole_line() {
        let old = "a ".repeat(INTRALINE_MAX_LINE);
        let new = format!("{old}b");
        let start = Instant::now();
        let (o, n) = intraline(&old, &new);
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(o, vec![0..old.len()]);
        assert_eq!(n, vec![0..new.len()]);

        // Just under the cap is diffed, and stays fast.
        let old = "word ".repeat(200);
        let new = old.replacen("word", "WORD", 1);
        let start = Instant::now();
        let (o, n) = intraline(&old, &new);
        assert!(start.elapsed() < Duration::from_millis(50));
        assert_eq!(o, vec![0..4]);
        assert_eq!(n, vec![0..4]);
    }

    #[test]
    fn rewritten_lines_are_marked_whole() {
        let old = "fn alpha(x: u32) -> bool {";
        let new = "    return gamma.delta();";
        assert_eq!(
            intraline(old, new),
            (vec![0..old.len()], vec![0..new.len()])
        );
    }
}
