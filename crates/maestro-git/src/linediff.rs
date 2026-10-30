//! Line-level diff of a buffer against a base text, for gutter signs.
//!
//! Computed in process with the `similar` crate (Myers), so it can run on
//! every edit without starting `git`. Only the base text comes from Git.

use std::ops::Range;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffOp};

/// What happened to a run of lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineHunkKind {
    /// Lines only in the new text.
    Added,
    /// Lines only in the old text.
    Removed,
    /// Old lines replaced by new ones.
    Modified,
}

/// One changed region, as zero-based line ranges. For `Removed`, `new` is
/// empty and starts at the line now following the removed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHunk {
    pub kind: LineHunkKind,
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// A gutter mark. Each has a symbol so it never relies on color alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Sign {
    /// Lines were removed just below this line (or above line 0).
    Removed,
    Added,
    Changed,
}

impl Sign {
    pub fn symbol(self) -> &'static str {
        match self {
            Sign::Added => "+",
            Sign::Changed => "~",
            Sign::Removed => "-",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Sign::Added => "added",
            Sign::Changed => "changed",
            Sign::Removed => "removed",
        }
    }
}

/// How long a single diff may search for a minimal result before settling
/// for a correct but larger one.
const DIFF_DEADLINE: Duration = Duration::from_millis(200);

/// Splits text into lines keyed by content without the terminator, plus
/// whether a terminator was present. `\r\n` and `\n` compare equal, so a
/// checkout with CRLF conversion does not mark every line.
fn lines(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(i) => {
                let line = &rest[..i];
                out.push((line.strip_suffix('\r').unwrap_or(line), true));
                rest = &rest[i + 1..];
            }
            None => {
                out.push((rest, false));
                rest = "";
            }
        }
    }
    out
}

/// Diffs `old` against `new` line by line. Line numbers match the editor's
/// (a line is text up to and including `\n`).
pub fn diff_lines(old: &str, new: &str) -> Vec<LineHunk> {
    let a = lines(old);
    let b = lines(new);
    let ops = similar::capture_diff_slices_deadline(
        Algorithm::Myers,
        &a,
        &b,
        Some(Instant::now() + DIFF_DEADLINE),
    );
    ops.into_iter()
        .filter_map(|op| match op {
            DiffOp::Equal { .. } => None,
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => Some(LineHunk {
                kind: LineHunkKind::Added,
                old: old_index..old_index,
                new: new_index..new_index + new_len,
            }),
            DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => Some(LineHunk {
                kind: LineHunkKind::Removed,
                old: old_index..old_index + old_len,
                new: new_index..new_index,
            }),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => Some(LineHunk {
                kind: LineHunkKind::Modified,
                old: old_index..old_index + old_len,
                new: new_index..new_index + new_len,
            }),
        })
        .collect()
}

/// Gutter signs by line, sorted for lookup while drawing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignMap {
    signs: Vec<(usize, Sign)>,
}

impl SignMap {
    /// Turns hunks into one sign per line. A modified hunk marks its first
    /// lines changed and any extra new lines added. Removed lines are marked
    /// on the line above them, or on line 0 when removed from the top.
    pub fn from_hunks(hunks: &[LineHunk]) -> Self {
        let mut map = std::collections::BTreeMap::new();
        let mut put = |line: usize, sign: Sign| {
            let e = map.entry(line).or_insert(sign);
            *e = (*e).max(sign);
        };
        for h in hunks {
            match h.kind {
                LineHunkKind::Added => h.new.clone().for_each(|l| put(l, Sign::Added)),
                LineHunkKind::Removed => put(h.new.start.saturating_sub(1), Sign::Removed),
                LineHunkKind::Modified => {
                    let changed = h.old.len().min(h.new.len());
                    for (i, l) in h.new.clone().enumerate() {
                        put(
                            l,
                            if i < changed {
                                Sign::Changed
                            } else {
                                Sign::Added
                            },
                        );
                    }
                }
            }
        }
        Self {
            signs: map.into_iter().collect(),
        }
    }

    /// Diffs and builds the map in one step.
    pub fn diff(old: &str, new: &str) -> Self {
        Self::from_hunks(&diff_lines(old, new))
    }

    /// Every line of `text` marked added (a new, untracked file).
    pub fn all_added(text: &str) -> Self {
        Self {
            signs: (0..lines(text).len()).map(|l| (l, Sign::Added)).collect(),
        }
    }

    pub fn get(&self, line: usize) -> Option<Sign> {
        self.signs
            .binary_search_by_key(&line, |(l, _)| *l)
            .ok()
            .map(|i| self.signs[i].1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, Sign)> + '_ {
        self.signs.iter().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.signs.is_empty()
    }

    pub fn len(&self) -> usize {
        self.signs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signs(old: &str, new: &str) -> Vec<(usize, Sign)> {
        SignMap::diff(old, new).iter().collect()
    }

    #[test]
    fn identical_texts_have_no_signs() {
        assert!(signs("a\nb\n", "a\nb\n").is_empty());
        assert!(signs("", "").is_empty());
        assert!(signs("a\r\nb\r\n", "a\nb\n").is_empty());
    }

    #[test]
    fn added_changed_removed() {
        use Sign::*;
        assert_eq!(signs("a\nc\n", "a\nb\nc\n"), vec![(1, Added)]);
        assert_eq!(signs("a\nb\nc\n", "a\nB\nc\n"), vec![(1, Changed)]);
        assert_eq!(signs("a\nb\nc\n", "a\nc\n"), vec![(0, Removed)]);
        // Two lines replaced by three: two changed, one added.
        assert_eq!(
            signs("a\nb\nc\nd\n", "a\nB\nC\nX\nd\n"),
            vec![(1, Changed), (2, Changed), (3, Added)]
        );
    }

    #[test]
    fn edits_at_start_and_end() {
        use Sign::*;
        assert_eq!(signs("a\nb\n", "z\na\nb\n"), vec![(0, Added)]);
        assert_eq!(signs("a\nb\n", "A\nb\n"), vec![(0, Changed)]);
        assert_eq!(signs("a\nb\nc\n", "b\nc\n"), vec![(0, Removed)]);
        assert_eq!(signs("a\nb\n", "a\nb\nz\n"), vec![(2, Added)]);
        assert_eq!(signs("a\nb\n", "a\nB\n"), vec![(1, Changed)]);
        assert_eq!(signs("a\nb\nc\n", "a\nb\n"), vec![(1, Removed)]);
        assert_eq!(signs("a\n", ""), vec![(0, Removed)]);
    }

    #[test]
    fn missing_trailing_newline() {
        use Sign::*;
        assert!(signs("a\nb", "a\nb").is_empty());
        // Adding a line after a last line without a newline changes that
        // line (it gains a terminator), as `git diff` reports it.
        assert_eq!(signs("a\nb", "a\nb\nc"), vec![(1, Changed), (2, Added)]);
        assert_eq!(signs("a\nb\n", "a\nb"), vec![(1, Changed)]);
    }

    #[test]
    fn hunks_carry_line_ranges() {
        let h = diff_lines("a\nb\nc\n", "a\nc\nd\n");
        assert_eq!(
            h,
            vec![
                LineHunk {
                    kind: LineHunkKind::Removed,
                    old: 1..2,
                    new: 1..1
                },
                LineHunk {
                    kind: LineHunkKind::Added,
                    old: 3..3,
                    new: 2..3
                },
            ]
        );
    }

    #[test]
    fn all_added_marks_each_line() {
        assert_eq!(SignMap::all_added("a\nb").len(), 2);
        assert_eq!(SignMap::all_added("").len(), 0);
        assert_eq!(SignMap::all_added("a\n").get(0), Some(Sign::Added));
    }
}
