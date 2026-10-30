//! Tree-sitter parsing, highlighting and structural selection for Maestro.
//!
//! This crate knows text (a Ropey [`Rope`]) and change sets from
//! `maestro-core`, and nothing about editors or terminals. Positions in and
//! out are char indices, like everywhere else in Maestro; tree-sitter's byte
//! offsets and byte-column [`Point`]s stay inside this crate.
//!
//! A [`Syntax`] holds a parser and the current tree for one document. After
//! the document changes, pass the old text and the applied change sets to
//! [`Syntax::update`] (or [`input_edits`] then [`Syntax::apply_edits`] and
//! [`Syntax::reparse`]), which reparses incrementally.

mod language;

use std::ops::Range;

use maestro_core::change::Op;
use maestro_core::{ChangeSet, Rope};
use tree_sitter::{InputEdit, Node, Parser, Point, QueryCursor, StreamingIterator, Tree};

pub use language::Language;
pub use tree_sitter::InputEdit as Edit;

/// A highlighted span: `start..end` in chars, and the theme scope (such as
/// `keyword` or `function.method`) that covers it.
pub type Highlight = (usize, usize, &'static str);

/// A parser and the current syntax tree for one document.
pub struct Syntax {
    language: Language,
    parser: Parser,
    tree: Tree,
}

impl std::fmt::Debug for Syntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Syntax")
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

/// Feeds a rope to tree-sitter chunk by chunk, without copying it.
fn read_rope<'a>(text: &'a Rope) -> impl FnMut(usize, Point) -> &'a [u8] + 'a {
    move |byte, _| {
        if byte >= text.len_bytes() {
            return &[][..];
        }
        let (chunk, start, _, _) = text.chunk_at_byte(byte);
        &chunk.as_bytes()[byte - start..]
    }
}

impl Syntax {
    /// Parses `text` from scratch. Returns `None` only if the grammar cannot
    /// be loaded (an ABI mismatch), which the tests rule out per language.
    pub fn parse(language: Language, text: &Rope) -> Option<Syntax> {
        let mut parser = Parser::new();
        parser.set_language(&language.grammar()).ok()?;
        let tree = parser.parse_with_options(&mut read_rope(text), None, None)?;
        Some(Syntax {
            language,
            parser,
            tree,
        })
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Tells the tree about edits, in order. Call [`Syntax::reparse`] after.
    pub fn apply_edits(&mut self, edits: &[InputEdit]) {
        for e in edits {
            self.tree.edit(e);
        }
    }

    /// Reparses `text`, reusing the unchanged parts of the edited tree.
    pub fn reparse(&mut self, text: &Rope) {
        let new = self
            .parser
            .parse_with_options(&mut read_rope(text), Some(&self.tree), None);
        match new {
            Some(tree) => self.tree = tree,
            None => {
                // Only happens if parsing was cancelled; fall back to a fresh
                // parse so the tree never stays out of date.
                self.parser.reset();
                if let Some(tree) = self
                    .parser
                    .parse_with_options(&mut read_rope(text), None, None)
                {
                    self.tree = tree;
                }
            }
        }
    }

    /// Updates the tree after `changes` (applied in order) turned `old` into
    /// `new`.
    pub fn update(&mut self, old: &Rope, changes: &[ChangeSet], new: &Rope) {
        self.apply_edits(&input_edits(old, changes));
        self.reparse(new);
    }

    /// Highlights inside the char range `range`: sorted, non-overlapping
    /// spans clipped to the range. Where captures nest, the innermost wins;
    /// where two patterns capture the same node, the earlier pattern in the
    /// query wins (tree-sitter-highlight's rule, which the bundled queries
    /// are written for).
    pub fn highlights(&self, text: &Rope, range: Range<usize>) -> Vec<Highlight> {
        let hq = self.language.highlights();
        let Some(query) = &hq.query else {
            return Vec::new();
        };
        let end = range.end.min(text.len_chars());
        let start = range.start.min(end);
        let (start_byte, end_byte) = (text.char_to_byte(start), text.char_to_byte(end));
        if start_byte == end_byte {
            return Vec::new();
        }
        let len_bytes = text.len_bytes();

        // (start byte, end byte, pattern, scope) for every capture.
        let mut caps: Vec<(usize, usize, usize, &'static str)> = Vec::new();
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(start_byte..end_byte);
        let provider = |node: Node| {
            let r = node.byte_range();
            text.byte_slice(r.start.min(len_bytes)..r.end.min(len_bytes))
                .chunks()
                .map(str::as_bytes)
        };
        let mut matches = cursor.matches(query, self.tree.root_node(), provider);
        while let Some(m) = matches.next() {
            for c in m.captures {
                let Some(Some(scope)) = hq.scopes.get(c.index as usize) else {
                    continue;
                };
                let r = c.node.byte_range();
                let (a, b) = (r.start.max(start_byte), r.end.min(end_byte));
                if a < b {
                    caps.push((r.start, r.end, m.pattern_index, scope));
                }
            }
        }

        // Paint outer captures first and inner ones over them; for the same
        // node paint later patterns first so the earliest pattern ends on top.
        caps.sort_by(|x, y| (y.1 - y.0).cmp(&(x.1 - x.0)).then_with(|| y.2.cmp(&x.2)));
        let mut owner: Vec<Option<&'static str>> = vec![None; end_byte - start_byte];
        for &(a, b, _, scope) in &caps {
            let (a, b) = (a.max(start_byte), b.min(end_byte));
            owner[a - start_byte..b - start_byte].fill(Some(scope));
        }

        let mut out = Vec::new();
        let mut i = 0;
        while i < owner.len() {
            let Some(scope) = owner[i] else {
                i += 1;
                continue;
            };
            let mut j = i + 1;
            while j < owner.len() && owner[j] == Some(scope) {
                j += 1;
            }
            let from = text.byte_to_char(start_byte + i);
            let to = text.byte_to_char(start_byte + j);
            if from < to {
                out.push((from, to, scope));
            }
            i = j;
        }
        out
    }

    /// The char range of the smallest named node that strictly contains
    /// `from..to` (the range grows by at least one char), for "expand
    /// selection".
    pub fn expand(&self, text: &Rope, from: usize, to: usize) -> Option<Range<usize>> {
        let (fb, tb) = self.bytes(text, from, to);
        let mut node = self
            .tree
            .root_node()
            .named_descendant_for_byte_range(fb, tb)?;
        loop {
            let r = node.byte_range();
            if r.start <= fb && r.end >= tb && (r.start < fb || r.end > tb) {
                return Some(self.chars(text, r));
            }
            node = node.parent()?;
        }
    }

    /// The char range of the first named child inside `from..to` that is
    /// smaller than it, for "shrink selection" without history.
    pub fn shrink(&self, text: &Rope, from: usize, to: usize) -> Option<Range<usize>> {
        let (fb, tb) = self.bytes(text, from, to);
        let mut node = self
            .tree
            .root_node()
            .named_descendant_for_byte_range(fb, tb)?;
        loop {
            let mut cursor = node.walk();
            let child = node
                .named_children(&mut cursor)
                .find(|c| c.start_byte() >= fb && c.end_byte() <= tb);
            let child = child?;
            let r = child.byte_range();
            if r.start > fb || r.end < tb {
                return Some(self.chars(text, r));
            }
            node = child;
        }
    }

    /// The next (or previous) named sibling of the node at `from..to`,
    /// climbing to the parent when a node has none.
    pub fn sibling(
        &self,
        text: &Rope,
        from: usize,
        to: usize,
        forward: bool,
    ) -> Option<Range<usize>> {
        let (fb, tb) = self.bytes(text, from, to);
        let mut node = self
            .tree
            .root_node()
            .named_descendant_for_byte_range(fb, tb)?;
        loop {
            let next = if forward {
                node.next_named_sibling()
            } else {
                node.prev_named_sibling()
            };
            if let Some(n) = next {
                return Some(self.chars(text, n.byte_range()));
            }
            node = node.parent()?;
        }
    }

    /// The kinds of the named nodes containing char `pos`, innermost first.
    /// Useful for debugging and tests.
    pub fn node_kinds_at(&self, text: &Rope, pos: usize) -> Vec<&'static str> {
        let (b, e) = self.bytes(text, pos, (pos + 1).min(text.len_chars()));
        let mut out = Vec::new();
        let mut node = self.tree.root_node().named_descendant_for_byte_range(b, e);
        while let Some(n) = node {
            out.push(n.kind());
            node = n.parent();
        }
        out
    }

    fn bytes(&self, text: &Rope, from: usize, to: usize) -> (usize, usize) {
        let len = text.len_chars();
        let (from, to) = (from.min(len), to.min(len));
        (text.char_to_byte(from), text.char_to_byte(to.max(from)))
    }

    fn chars(&self, text: &Rope, r: Range<usize>) -> Range<usize> {
        let len = text.len_bytes();
        text.byte_to_char(r.start.min(len))..text.byte_to_char(r.end.min(len))
    }
}

/// The tree-sitter edits for `changes`, applied in order to `old`. Each
/// edit's positions are relative to the text after the edits before it,
/// which is what [`Tree::edit`] expects.
pub fn input_edits(old: &Rope, changes: &[ChangeSet]) -> Vec<InputEdit> {
    let mut text = old.clone();
    let mut out = Vec::new();
    for cs in changes {
        let mut pos = 0;
        for op in cs.ops() {
            match op {
                Op::Retain(n) => pos += n,
                Op::Delete(n) => out.push(edit(&mut text, pos, *n, "")),
                Op::Insert(s) => {
                    out.push(edit(&mut text, pos, 0, s));
                    pos += s.chars().count();
                }
            }
        }
    }
    out
}

/// Replaces chars `pos..pos + delete` of `text` with `insert` and describes
/// the change for tree-sitter.
fn edit(text: &mut Rope, pos: usize, delete: usize, insert: &str) -> InputEdit {
    let start_byte = text.char_to_byte(pos);
    let old_end_byte = text.char_to_byte(pos + delete);
    let start_position = point(text, start_byte);
    let old_end_position = point(text, old_end_byte);
    if delete > 0 {
        text.remove(pos..pos + delete);
    }
    if !insert.is_empty() {
        text.insert(pos, insert);
    }
    let new_end_byte = start_byte + insert.len();
    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position,
        old_end_position,
        new_end_position: point(text, new_end_byte),
    }
}

/// The tree-sitter point (row, byte column) of `byte`.
///
/// Tree-sitter counts rows at `\n` only, while Ropey also breaks lines at
/// lone CR and Unicode separators. The column matters (external scanners for
/// indentation read it), so it is measured from the last `\n`. The row can
/// over-count in files with those rare separators; nothing here reads rows.
fn point(text: &Rope, byte: usize) -> Point {
    let row = text.byte_to_line(byte);
    let mut line = row;
    let mut start = text.line_to_byte(line);
    let mut steps = 0;
    while line > 0 && text.byte(start - 1) != b'\n' && steps < 64 {
        line -= 1;
        start = text.line_to_byte(line);
        steps += 1;
    }
    Point {
        row,
        column: byte - start,
    }
}

#[cfg(test)]
mod tests;
