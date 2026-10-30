//! Selections: the primary editing primitive.
//!
//! A [`Range`] has an `anchor` and a `head`, both char indices on grapheme
//! boundaries. In normal and select modes a range is *inclusive*: it covers
//! every grapheme from `min(anchor, head)` up to and including the grapheme
//! that starts at `max(anchor, head)`. A collapsed range (anchor == head)
//! therefore still covers one grapheme, the one under the block cursor. This
//! is the Kakoune model and it lets `d` on a collapsed cursor delete the
//! character under it without a special case.
//!
//! In insert mode ranges are collapsed insertion points; the char under the
//! point is not "selected". Commands that care use [`Range::cursor`].
//!
//! A [`Selection`] is a non-empty, sorted list of non-overlapping ranges with
//! one marked primary.

use ropey::RopeSlice;

use crate::change::{Assoc, ChangeSet};
use crate::text::{floor_grapheme_boundary, next_grapheme_boundary, prev_grapheme_boundary};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Range {
    pub anchor: usize,
    pub head: usize,
}

impl Range {
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    pub fn point(pos: usize) -> Self {
        Self::new(pos, pos)
    }

    /// The first covered char.
    pub fn from(&self) -> usize {
        self.anchor.min(self.head)
    }

    /// The start of the last covered grapheme.
    pub fn last(&self) -> usize {
        self.anchor.max(self.head)
    }

    /// Exclusive end of the covered text (one grapheme past [`Range::last`]).
    pub fn to(&self, text: RopeSlice) -> usize {
        next_grapheme_boundary(text, self.last())
    }

    pub fn cursor(&self) -> usize {
        self.head
    }

    pub fn is_collapsed(&self) -> bool {
        self.anchor == self.head
    }

    pub fn is_forward(&self) -> bool {
        self.head >= self.anchor
    }

    pub fn flip(self) -> Self {
        Self::new(self.head, self.anchor)
    }

    /// Collapses onto the head.
    pub fn collapse(self) -> Self {
        Self::point(self.head)
    }

    /// Moves the head, keeping the anchor (extend) or collapsing (move).
    pub fn put_head(self, head: usize, extend: bool) -> Self {
        if extend {
            Self::new(self.anchor, head)
        } else {
            Self::point(head)
        }
    }

    pub fn map(self, changes: &ChangeSet) -> Self {
        if self.is_collapsed() {
            let p = changes.map_pos(self.head, Assoc::After);
            return Self::point(p);
        }
        // Text inserted exactly at the start of a selection lands outside it;
        // text inserted exactly at its last grapheme start lands inside it.
        let (from_assoc, last_assoc) = (Assoc::After, Assoc::Before);
        if self.is_forward() {
            Self::new(
                changes.map_pos(self.anchor, from_assoc),
                changes.map_pos(self.head, last_assoc),
            )
        } else {
            Self::new(
                changes.map_pos(self.anchor, last_assoc),
                changes.map_pos(self.head, from_assoc),
            )
        }
    }

    /// Clamps both ends into the text and onto grapheme boundaries.
    pub fn clamp(self, text: RopeSlice) -> Self {
        Self::new(
            floor_grapheme_boundary(text, self.anchor),
            floor_grapheme_boundary(text, self.head),
        )
    }

    /// Like [`Range::clamp`], but also pulls an endpoint off the end-of-text
    /// position onto the last grapheme, which is where a block cursor can sit
    /// in normal mode.
    pub fn clamp_normal(self, text: RopeSlice) -> Self {
        let r = self.clamp(text);
        let len = text.len_chars();
        let fix = |p: usize| {
            if p >= len && len > 0 {
                prev_grapheme_boundary(text, len)
            } else {
                p
            }
        };
        Self::new(fix(r.anchor), fix(r.head))
    }

    pub fn fragment(&self, text: RopeSlice) -> String {
        text.slice(self.from()..self.to(text)).to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    ranges: Vec<Range>,
    primary: usize,
}

impl Selection {
    pub fn single(anchor: usize, head: usize) -> Self {
        Self {
            ranges: vec![Range::new(anchor, head)],
            primary: 0,
        }
    }

    pub fn point(pos: usize) -> Self {
        Self::single(pos, pos)
    }

    /// Builds a selection from ranges. Panics if `ranges` is empty; callers
    /// must normalize afterwards if ranges may be unsorted or overlapping.
    pub fn new(ranges: Vec<Range>, primary: usize) -> Self {
        assert!(!ranges.is_empty(), "a selection needs at least one range");
        let primary = primary.min(ranges.len() - 1);
        Self { ranges, primary }
    }

    pub fn ranges(&self) -> &[Range] {
        &self.ranges
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn primary(&self) -> Range {
        self.ranges[self.primary]
    }

    pub fn primary_index(&self) -> usize {
        self.primary
    }

    pub fn set_primary_index(&mut self, idx: usize) {
        self.primary = idx % self.ranges.len();
    }

    pub fn iter(&self) -> impl Iterator<Item = &Range> {
        self.ranges.iter()
    }

    /// Applies `f` to every range. The result is normalized against `text`.
    pub fn transform(&self, text: RopeSlice, mut f: impl FnMut(Range) -> Range) -> Self {
        let ranges = self.ranges.iter().map(|r| f(*r)).collect();
        Self::new(ranges, self.primary).normalize(text)
    }

    /// Replaces each range with zero or more ranges.
    pub fn transform_many(
        &self,
        text: RopeSlice,
        mut f: impl FnMut(Range) -> Vec<Range>,
    ) -> Option<Self> {
        let mut ranges = Vec::new();
        let mut primary = 0;
        for (i, r) in self.ranges.iter().enumerate() {
            if i == self.primary {
                primary = ranges.len();
            }
            ranges.extend(f(*r));
        }
        if ranges.is_empty() {
            return None;
        }
        Some(Self::new(ranges, primary).normalize(text))
    }

    pub fn map(&self, changes: &ChangeSet) -> Self {
        Self {
            ranges: self.ranges.iter().map(|r| r.map(changes)).collect(),
            primary: self.primary,
        }
    }

    pub fn push(mut self, range: Range, text: RopeSlice) -> Self {
        self.ranges.push(range);
        self.primary = self.ranges.len() - 1;
        self.normalize(text)
    }

    pub fn keep_primary(&self) -> Self {
        Self::new(vec![self.primary()], 0)
    }

    pub fn remove(&self, idx: usize) -> Option<Self> {
        if self.ranges.len() <= 1 || idx >= self.ranges.len() {
            return None;
        }
        let mut ranges = self.ranges.clone();
        ranges.remove(idx);
        let primary = if self.primary > idx {
            self.primary - 1
        } else {
            self.primary.min(ranges.len() - 1)
        };
        Some(Self { ranges, primary })
    }

    pub fn collapse(&self) -> Self {
        Self {
            ranges: self.ranges.iter().map(|r| r.collapse()).collect(),
            primary: self.primary,
        }
    }

    /// Clamps into the text, sorts and merges ranges whose covered text
    /// overlaps. Identical points merge too. The primary range survives
    /// merging (as the merged range).
    pub fn normalize(self, text: RopeSlice) -> Self {
        let mut indexed: Vec<(usize, Range)> = self
            .ranges
            .into_iter()
            .map(|r| r.clamp(text))
            .enumerate()
            .collect();
        indexed.sort_by_key(|(_, r)| (r.from(), r.last()));
        let mut out: Vec<Range> = Vec::with_capacity(indexed.len());
        let mut primary = 0;
        for (i, r) in indexed {
            let is_primary = i == self.primary;
            if let Some(prev) = out.last_mut()
                && (r.from() < prev.to(text) || r.from() == prev.from())
            {
                let from = prev.from().min(r.from());
                let last = prev.last().max(r.last());
                *prev = if prev.is_forward() {
                    Range::new(from, last)
                } else {
                    Range::new(last, from)
                };
                if is_primary {
                    primary = out.len() - 1;
                }
                continue;
            }
            out.push(r);
            if is_primary {
                primary = out.len() - 1;
            }
        }
        Self {
            ranges: out,
            primary,
        }
    }

    /// Normal-mode clamp: no endpoint past the last grapheme.
    pub fn clamp_normal(&self, text: RopeSlice) -> Self {
        Self::new(
            self.ranges.iter().map(|r| r.clamp_normal(text)).collect(),
            self.primary,
        )
        .normalize(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Change;
    use ropey::Rope;

    #[test]
    fn inclusive_ranges_cover_the_head_grapheme() {
        let rope = Rope::from_str("hello");
        let s = rope.slice(..);
        assert_eq!(Range::point(1).to(s), 2);
        assert_eq!(Range::new(3, 1).from(), 1);
        assert_eq!(Range::new(3, 1).to(s), 4);
        assert_eq!(Range::new(1, 3).fragment(s), "ell");
    }

    #[test]
    fn normalize_merges_overlaps_and_keeps_primary() {
        let rope = Rope::from_str("abcdefghij");
        let s = rope.slice(..);
        let sel = Selection::new(
            vec![
                Range::new(5, 6),
                Range::new(0, 2),
                Range::new(2, 3),
                Range::point(8),
            ],
            3,
        )
        .normalize(s);
        assert_eq!(
            sel.ranges(),
            &[Range::new(0, 3), Range::new(5, 6), Range::point(8)]
        );
        assert_eq!(sel.primary(), Range::point(8));
    }

    #[test]
    fn adjacent_points_do_not_merge() {
        let rope = Rope::from_str("abc");
        let sel =
            Selection::new(vec![Range::point(1), Range::point(2)], 0).normalize(rope.slice(..));
        assert_eq!(sel.len(), 2);
    }

    #[test]
    fn mapping_through_insertions() {
        let cs = ChangeSet::from_changes(10, [Change::insert(2, "xx")]).unwrap();
        assert_eq!(Range::point(2).map(&cs), Range::point(4));
        assert_eq!(Range::new(2, 5).map(&cs), Range::new(4, 7));
        assert_eq!(Range::new(0, 2).map(&cs), Range::new(0, 2));
    }

    #[test]
    fn clamp_normal_pulls_off_end() {
        let rope = Rope::from_str("ab");
        let s = rope.slice(..);
        assert_eq!(Range::point(2).clamp_normal(s), Range::point(1));
        let empty = Rope::new();
        assert_eq!(
            Range::point(5).clamp_normal(empty.slice(..)),
            Range::point(0)
        );
    }
}
