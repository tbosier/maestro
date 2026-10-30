//! Structural selection over the syntax tree.

use maestro_core::text::prev_grapheme_boundary;
use maestro_core::{Range, Rope, Selection};
use maestro_syntax::Syntax;

use crate::editor::{Editor, Severity};

crate::commands! {
    expand_selection => "Grow each selection to the enclosing syntax node",
    shrink_selection => "Undo the last expand_selection, or shrink to the first child node",
    select_next_sibling => "Select the next syntax node at the same level",
    select_prev_sibling => "Select the previous syntax node at the same level",
}

/// Default normal and select mode keys, bound by `keymap.rs`.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[
    ("A-o", "expand_selection"),
    ("A-i", "shrink_selection"),
    ("A-n", "select_next_sibling"),
    ("A-p", "select_prev_sibling"),
];

type NodeFn = fn(&Syntax, &Rope, usize, usize) -> Option<std::ops::Range<usize>>;

/// Replaces each range with the node `f` finds for it, keeping ranges for
/// which it finds none. Returns false (after saying why) when the buffer has
/// no tree.
fn select_nodes(ed: &mut Editor, f: NodeFn) -> bool {
    let buf = ed.buffer();
    let Some(syntax) = buf.syntax.syntax() else {
        let msg = buf.syntax.missing_reason();
        ed.set_status(Severity::Warning, msg);
        return false;
    };
    let text = buf.doc.text().clone();
    let s = text.slice(..);
    let sel = &ed.view().selection;
    let ranges: Vec<Range> = sel
        .iter()
        .map(|r| match f(syntax, &text, r.from(), r.to(s)) {
            Some(n) if n.start < n.end => {
                let new = Range::new(n.start, prev_grapheme_boundary(s, n.end));
                if r.is_forward() { new } else { new.flip() }
            }
            _ => *r,
        })
        .collect();
    let new = Selection::new(ranges, sel.primary_index()).clamp_normal(s);
    ed.set_selection(new);
    true
}

fn expand_selection(ed: &mut Editor) {
    let before = ed.view().selection.clone();
    if !select_nodes(ed, |syn, text, from, to| syn.expand(text, from, to)) {
        return;
    }
    let after = ed.view().selection.clone();
    if after == before {
        return;
    }
    let view = ed.focus;
    let entry = ed
        .buffer_mut()
        .syntax
        .expansions
        .entry(view)
        .or_insert_with(|| (Vec::new(), before.clone()));
    if entry.1 != before {
        entry.0.clear();
    }
    entry.0.push(before);
    entry.1 = after;
}

fn shrink_selection(ed: &mut Editor) {
    let view = ed.focus;
    let current = ed.view().selection.clone();
    let previous = ed
        .buffer_mut()
        .syntax
        .expansions
        .get_mut(&view)
        .filter(|(_, last)| *last == current)
        .and_then(|(stack, last)| {
            let prev = stack.pop()?;
            *last = prev.clone();
            Some(prev)
        });
    match previous {
        Some(prev) => ed.set_selection(prev),
        None => {
            select_nodes(ed, |syn, text, from, to| syn.shrink(text, from, to));
        }
    }
}

fn select_next_sibling(ed: &mut Editor) {
    select_nodes(ed, |syn, text, from, to| syn.sibling(text, from, to, true));
}

fn select_prev_sibling(ed: &mut Editor) {
    select_nodes(ed, |syn, text, from, to| syn.sibling(text, from, to, false));
}
