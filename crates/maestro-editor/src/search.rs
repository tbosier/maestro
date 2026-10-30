//! Regex search over documents.
//!
//! The regex engine needs contiguous text, so searches materialize the rope
//! as a `String` (one copy, O(n)). That is fast enough for ordinary files and
//! is the documented cost for very large ones; a rope-native engine is a
//! known follow-up (see docs/performance.md).

use maestro_core::Rope;
use regex::{Regex, RegexBuilder};

/// Compiles a pattern with smart case: case-insensitive unless the pattern
/// contains an uppercase letter.
pub fn compile(pattern: &str) -> Result<Regex, String> {
    let insensitive = !pattern.chars().any(char::is_uppercase);
    RegexBuilder::new(pattern)
        .case_insensitive(insensitive)
        .multi_line(true)
        .build()
        .map_err(|e| format!("invalid regex: {e}"))
}

/// All matches in `text[from..to]` (char indices), as char ranges
/// `(start, end_exclusive)`. Empty matches are skipped.
pub fn matches_in(text: &Rope, re: &Regex, from: usize, to: usize) -> Vec<(usize, usize)> {
    let slice = text.slice(from..to);
    let s = slice.to_string();
    let base_byte = text.char_to_byte(from);
    re.find_iter(&s)
        .filter(|m| !m.is_empty())
        .map(|m| {
            (
                text.byte_to_char(base_byte + m.start()),
                text.byte_to_char(base_byte + m.end()),
            )
        })
        .collect()
}

/// The next match after `pos` (forward) or before it (backward), wrapping
/// around the document. Returns the match and whether it wrapped.
pub fn find(text: &Rope, re: &Regex, pos: usize, backward: bool) -> Option<((usize, usize), bool)> {
    let all = matches_in(text, re, 0, text.len_chars());
    if all.is_empty() {
        return None;
    }
    if backward {
        match all.iter().rev().find(|(s, _)| *s < pos) {
            Some(m) => Some((*m, false)),
            None => Some((*all.last().expect("non-empty"), true)),
        }
    } else {
        match all.iter().find(|(s, _)| *s > pos) {
            Some(m) => Some((*m, false)),
            None => Some((all[0], true)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_and_char_offsets() {
        let text = Rope::from_str("héllo Hello hello");
        let re = compile("hello").unwrap();
        assert_eq!(
            matches_in(&text, &re, 0, text.len_chars()),
            vec![(6, 11), (12, 17)]
        );
        let re = compile("Hello").unwrap();
        assert_eq!(matches_in(&text, &re, 0, text.len_chars()), vec![(6, 11)]);
        let re = compile("h.llo").unwrap();
        assert_eq!(find(&text, &re, 0, false), Some(((6, 11), false)));
        assert_eq!(find(&text, &re, 12, false), Some(((0, 5), true)));
        assert_eq!(find(&text, &re, 6, true), Some(((0, 5), false)));
    }
}
