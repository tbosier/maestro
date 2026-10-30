//! Change sets and transactions.
//!
//! A [`ChangeSet`] is a complete description of how to turn a document of
//! length `len` into one of length `len_after`: a sequence of retain, delete
//! and insert operations covering every char exactly once. Because the set
//! records the length it expects, applying it to a document of the wrong
//! length fails instead of corrupting text. This is the first line of defence
//! against stale edits; version checks in [`crate::document`] are the second.

use ropey::Rope;
use thiserror::Error;

use crate::selection::Selection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Retain(usize),
    Delete(usize),
    Insert(String),
}

/// One replacement in original-document coordinates: replace chars
/// `from..to` with `text` (or delete them when `text` is `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub from: usize,
    pub to: usize,
    pub text: Option<String>,
}

impl Change {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Self {
            from: at,
            to: at,
            text: Some(text.into()),
        }
    }

    pub fn delete(from: usize, to: usize) -> Self {
        Self {
            from,
            to,
            text: None,
        }
    }

    pub fn replace(from: usize, to: usize, text: impl Into<String>) -> Self {
        Self {
            from,
            to,
            text: Some(text.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChangeError {
    #[error("change {from}..{to} is out of bounds for a document of {len} chars")]
    OutOfBounds { from: usize, to: usize, len: usize },
    #[error("change {from}..{to} is reversed")]
    Reversed { from: usize, to: usize },
    #[error(
        "change starting at {from} overlaps or precedes the previous change ending at {prev_end}"
    )]
    Overlap { from: usize, prev_end: usize },
    #[error("change set expects a document of {expected} chars but it has {actual}")]
    LengthMismatch { expected: usize, actual: usize },
}

/// How a position sitting exactly at an insertion point moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assoc {
    /// Stay before inserted text.
    Before,
    /// Move past inserted text.
    After,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    ops: Vec<Op>,
    len: usize,
    len_after: usize,
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

impl ChangeSet {
    /// The identity change for a document of `len` chars.
    pub fn identity(len: usize) -> Self {
        Self {
            ops: Vec::new(),
            len,
            len_after: len,
        }
    }

    /// Builds a change set from changes sorted by position. Changes may touch
    /// (one ends where the next begins) but may not overlap. Two insertions
    /// at the same point are allowed and applied in order.
    pub fn from_changes(
        len: usize,
        changes: impl IntoIterator<Item = Change>,
    ) -> Result<Self, ChangeError> {
        let mut set = Self::identity(len);
        set.len_after = 0;
        let mut last = 0;
        for Change { from, to, text } in changes {
            if from > to {
                return Err(ChangeError::Reversed { from, to });
            }
            if to > len {
                return Err(ChangeError::OutOfBounds { from, to, len });
            }
            if from < last {
                return Err(ChangeError::Overlap {
                    from,
                    prev_end: last,
                });
            }
            set.retain(from - last);
            set.delete(to - from);
            if let Some(text) = text {
                set.insert(text);
            }
            last = to;
        }
        set.retain(len - last);
        Ok(set)
    }

    /// Like [`ChangeSet::from_changes`] but clips each change so it starts no
    /// earlier than the previous one ended. Used for multi-cursor commands
    /// where neighboring cursors may compute overlapping deletions.
    pub fn from_changes_clipped(len: usize, changes: impl IntoIterator<Item = Change>) -> Self {
        let mut last = 0;
        let clipped: Vec<Change> = changes
            .into_iter()
            .map(|mut c| {
                c.to = c.to.min(len);
                c.from = c.from.min(c.to).max(last);
                c.to = c.to.max(c.from);
                last = c.to;
                c
            })
            .collect();
        Self::from_changes(len, clipped).expect("clipped changes are valid by construction")
    }

    fn retain(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.len_after += n;
        if let Some(Op::Retain(m)) = self.ops.last_mut() {
            *m += n;
        } else {
            self.ops.push(Op::Retain(n));
        }
    }

    fn delete(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if let Some(Op::Delete(m)) = self.ops.last_mut() {
            *m += n;
        } else {
            self.ops.push(Op::Delete(n));
        }
    }

    fn insert(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.len_after += char_len(&text);
        if let Some(Op::Insert(s)) = self.ops.last_mut() {
            s.push_str(&text);
        } else {
            self.ops.push(Op::Insert(text));
        }
    }

    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// Length of the document this set applies to.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Length of the document after applying.
    pub fn len_after(&self) -> usize {
        self.len_after
    }

    /// True if applying this set changes nothing.
    pub fn is_empty(&self) -> bool {
        self.ops.iter().all(|op| matches!(op, Op::Retain(_)))
    }

    /// Applies the change set. Fails without modifying the rope if the rope
    /// length does not match the length this set was built for.
    pub fn apply(&self, text: &mut Rope) -> Result<(), ChangeError> {
        if text.len_chars() != self.len {
            return Err(ChangeError::LengthMismatch {
                expected: self.len,
                actual: text.len_chars(),
            });
        }
        let mut pos = 0;
        for op in &self.ops {
            match op {
                Op::Retain(n) => pos += n,
                Op::Delete(n) => text.remove(pos..pos + n),
                Op::Insert(s) => {
                    text.insert(pos, s);
                    pos += char_len(s);
                }
            }
        }
        Ok(())
    }

    /// Returns the change set that undoes `self`. `original` must be the
    /// document as it was before `self` was applied.
    pub fn invert(&self, original: &Rope) -> ChangeSet {
        debug_assert_eq!(original.len_chars(), self.len);
        let mut inv = Self::identity(self.len_after);
        inv.len_after = 0;
        let mut pos = 0;
        for op in &self.ops {
            match op {
                Op::Retain(n) => {
                    inv.retain(*n);
                    pos += n;
                }
                Op::Delete(n) => {
                    inv.insert(original.slice(pos..pos + n).to_string());
                    pos += n;
                }
                Op::Insert(s) => inv.delete(char_len(s)),
            }
        }
        inv
    }

    /// Maps a position in the original document to the changed document.
    /// Positions inside deleted text collapse to the deletion point.
    pub fn map_pos(&self, pos: usize, assoc: Assoc) -> usize {
        let mut old = 0;
        let mut new = 0;
        for op in &self.ops {
            match op {
                Op::Retain(n) => {
                    if pos < old + n {
                        return new + (pos - old);
                    }
                    old += n;
                    new += n;
                }
                Op::Delete(n) => {
                    if pos < old + n {
                        return new;
                    }
                    old += n;
                }
                Op::Insert(s) => {
                    if pos == old && assoc == Assoc::Before {
                        return new;
                    }
                    new += char_len(s);
                }
            }
        }
        new + pos.saturating_sub(old)
    }

    /// The replacements this set makes, in original-document coordinates.
    pub fn changes(&self) -> Vec<Change> {
        let mut out: Vec<Change> = Vec::new();
        let mut pos = 0;
        for op in &self.ops {
            match op {
                Op::Retain(n) => pos += n,
                Op::Delete(n) => {
                    out.push(Change::delete(pos, pos + n));
                    pos += n;
                }
                Op::Insert(s) => match out.last_mut() {
                    Some(c) if c.to == pos && c.text.is_none() => c.text = Some(s.clone()),
                    _ => out.push(Change::insert(pos, s.clone())),
                },
            }
        }
        out
    }
}

/// A change set plus the selection to adopt afterwards. Transactions are the
/// only way the editor modifies a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub changes: ChangeSet,
    pub selection: Option<Selection>,
}

impl Transaction {
    pub fn new(changes: ChangeSet) -> Self {
        Self {
            changes,
            selection: None,
        }
    }

    pub fn with_selection(mut self, selection: Selection) -> Self {
        self.selection = Some(selection);
        self
    }

    /// Builds a transaction that applies one change per selection range.
    /// Ranges are visited in order; overlapping results are clipped.
    pub fn change_by_selection(
        text: &Rope,
        selection: &Selection,
        mut f: impl FnMut(&crate::selection::Range) -> Change,
    ) -> Self {
        let changes = selection.ranges().iter().map(&mut f);
        Self::new(ChangeSet::from_changes_clipped(text.len_chars(), changes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn apply_and_invert() {
        let original = Rope::from_str("hello world");
        let mut text = original.clone();
        let cs = ChangeSet::from_changes(
            11,
            [Change::replace(0, 5, "goodbye"), Change::insert(11, "!")],
        )
        .unwrap();
        cs.apply(&mut text).unwrap();
        assert_eq!(text.to_string(), "goodbye world!");
        assert_eq!(cs.len_after(), 14);
        let inv = cs.invert(&original);
        inv.apply(&mut text).unwrap();
        assert_eq!(text, original);
    }

    #[test]
    fn rejects_overlap_and_bounds() {
        assert!(matches!(
            ChangeSet::from_changes(5, [Change::delete(0, 3), Change::delete(2, 4)]),
            Err(ChangeError::Overlap { .. })
        ));
        assert!(matches!(
            ChangeSet::from_changes(5, [Change::delete(3, 9)]),
            Err(ChangeError::OutOfBounds { .. })
        ));
    }

    #[test]
    fn stale_changes_fail_to_apply() {
        let cs = ChangeSet::from_changes(5, [Change::delete(0, 1)]).unwrap();
        let mut text = Rope::from_str("longer text");
        assert!(matches!(
            cs.apply(&mut text),
            Err(ChangeError::LengthMismatch { .. })
        ));
        assert_eq!(text.to_string(), "longer text");
    }

    #[test]
    fn map_pos_assoc() {
        let cs =
            ChangeSet::from_changes(6, [Change::insert(3, "xy"), Change::delete(4, 6)]).unwrap();
        assert_eq!(cs.map_pos(2, Assoc::After), 2);
        assert_eq!(cs.map_pos(3, Assoc::Before), 3);
        assert_eq!(cs.map_pos(3, Assoc::After), 5);
        assert_eq!(cs.map_pos(5, Assoc::After), 6); // inside deletion
        assert_eq!(cs.map_pos(6, Assoc::After), 6);
    }

    #[test]
    fn clipped_changes_never_overlap() {
        let cs = ChangeSet::from_changes_clipped(10, [Change::delete(0, 4), Change::delete(2, 6)]);
        let mut text = Rope::from_str("0123456789");
        cs.apply(&mut text).unwrap();
        assert_eq!(text.to_string(), "6789");
    }

    fn arb_doc_and_changes() -> impl Strategy<Value = (String, Vec<Change>)> {
        "[a-zé漢\n]{0,40}"
            .prop_flat_map(|doc| {
                let len = doc.chars().count();
                let changes = proptest::collection::vec(
                    (0..=len, 0..=len, proptest::option::of("[x-zß\n]{0,4}")),
                    0..6,
                );
                (Just(doc), changes)
            })
            .prop_map(|(doc, raw)| {
                let mut points: Vec<(usize, usize, Option<String>)> = raw
                    .into_iter()
                    .map(|(a, b, t)| (a.min(b), a.max(b), t))
                    .collect();
                points.sort_by_key(|p| (p.0, p.1));
                let mut last = 0;
                let mut changes = Vec::new();
                for (from, to, text) in points {
                    if from < last {
                        continue;
                    }
                    changes.push(Change { from, to, text });
                    last = to;
                }
                (doc, changes)
            })
    }

    proptest! {
        #[test]
        fn invert_round_trips((doc, changes) in arb_doc_and_changes()) {
            let original = Rope::from_str(&doc);
            let cs = ChangeSet::from_changes(original.len_chars(), changes.clone()).unwrap();
            let mut text = original.clone();
            cs.apply(&mut text).unwrap();
            prop_assert_eq!(text.len_chars(), cs.len_after());

            // Same result as applying the changes back to front on a String.
            let mut expected: Vec<char> = doc.chars().collect();
            for c in changes.iter().rev() {
                let ins: Vec<char> = c.text.clone().unwrap_or_default().chars().collect();
                expected.splice(c.from..c.to, ins);
            }
            prop_assert_eq!(text.to_string(), expected.into_iter().collect::<String>());

            let inv = cs.invert(&original);
            inv.apply(&mut text).unwrap();
            prop_assert_eq!(text, original);
        }

        #[test]
        fn map_pos_stays_in_bounds((doc, changes) in arb_doc_and_changes(), p in 0usize..50) {
            let len = doc.chars().count();
            let cs = ChangeSet::from_changes(len, changes).unwrap();
            let p = p.min(len);
            prop_assert!(cs.map_pos(p, Assoc::Before) <= cs.len_after());
            prop_assert!(cs.map_pos(p, Assoc::After) <= cs.len_after());
            prop_assert!(cs.map_pos(p, Assoc::Before) <= cs.map_pos(p, Assoc::After));
        }
    }
}
