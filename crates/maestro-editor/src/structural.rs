//! Structural pipelines: select syntax nodes with tree-sitter queries,
//! filter and expand the selections, and insert text above each one.
//!
//! Each step is a `:` command (`:select-nodes fn`), and steps chain with
//! `|` on one command line (`:select-nodes pub-async-fn | keep-nodes
//! returns-result | remove-nodes instrumented | insert-above
//! #[tracing::instrument]`). The whole line is checked before anything
//! runs (step names, queries, regexes, node kinds), the selection steps are
//! computed without touching the editor, and only then are the selections
//! replaced and the one text-changing step applied. So a failing step
//! changes nothing, and the edit is one transaction and one undo step.
//! `docs/editor-model.md` describes the commands for users.

use maestro_core::text::{line_indent, prev_grapheme_boundary};
use maestro_core::{Change, ChangeSet, Range, Rope, Selection, Transaction};
use maestro_syntax::{Language, NodeQuery, Syntax, shortcuts};
use regex::Regex;

use crate::editor::Editor;

/// The steps a pipeline can contain. Each is also a typed command in
/// `crate::typed`, which holds its description.
pub const STEPS: &[&str] = &[
    "select-nodes",
    "select-inside",
    "keep-nodes",
    "remove-nodes",
    "keep-containing",
    "remove-containing",
    "keep-matching",
    "remove-matching",
    "expand-to",
    "insert-above",
];

/// True if `line` (a `:` command line without the colon) starts with a
/// pipeline step, so [`execute`] should run it.
pub fn handles(line: &str) -> bool {
    let head = line.split_whitespace().next().unwrap_or_default();
    STEPS.contains(&head)
}

/// A step, checked and compiled.
enum Step {
    /// `inside`: within each selection rather than the whole buffer.
    Select {
        query: NodeQuery,
        inside: bool,
    },
    /// `containing`: the selection contains a match rather than being one.
    FilterNodes {
        query: NodeQuery,
        keep: bool,
        containing: bool,
    },
    FilterRegex {
        re: Regex,
        keep: bool,
    },
    ExpandTo {
        kinds: Vec<String>,
    },
    InsertAbove {
        text: String,
    },
}

impl Step {
    fn needs_tree(&self) -> bool {
        matches!(
            self,
            Step::Select { .. } | Step::FilterNodes { .. } | Step::ExpandTo { .. }
        )
    }
}

/// Splits a command line into steps at each `|` that is outside double
/// quotes and outside `()`, `[]` and `{}`. A backslash keeps the next
/// character from counting, so `\|` and `\(` in a regex do not split or
/// nest.
pub fn split_steps(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => quoted = !quoted,
            '(' | '[' | '{' if !quoted => depth += 1,
            ')' | ']' | '}' if !quoted => depth = depth.saturating_sub(1),
            '|' if !quoted && depth == 0 => {
                out.push(line[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(line[start..].trim());
    out
}

/// Compiles a query argument: a shortcut name for `lang`, or raw
/// tree-sitter query syntax.
fn compile_query(lang: Option<Language>, arg: &str) -> Result<NodeQuery, String> {
    let lang = lang.ok_or("no syntax tree: file type not recognized")?;
    if arg.is_empty() {
        return Err(format!("needs a query or one of: {}", shortcut_names(lang)));
    }
    let raw = arg.starts_with(['(', '[', '"', '_', ';']);
    if !raw {
        return NodeQuery::shortcut(lang, arg).ok_or_else(|| {
            format!(
                "no {} shortcut named {arg:?} (shortcuts: {}; raw queries start with \"(\")",
                lang.name(),
                shortcut_names(lang)
            )
        });
    }
    NodeQuery::new(lang, arg).map_err(|e| e.to_string())
}

fn shortcut_names(lang: Language) -> String {
    let names: Vec<&str> = shortcuts(lang).iter().map(|(n, _, _)| *n).collect();
    if names.is_empty() {
        "none for this language".into()
    } else {
        names.join(", ")
    }
}

fn parse_step(name: &str, arg: &str, lang: Option<Language>) -> Result<Step, String> {
    Ok(match name {
        "select-nodes" | "select-inside" => Step::Select {
            query: compile_query(lang, arg)?,
            inside: name == "select-inside",
        },
        "keep-nodes" | "remove-nodes" | "keep-containing" | "remove-containing" => {
            Step::FilterNodes {
                query: compile_query(lang, arg)?,
                keep: name.starts_with("keep"),
                containing: name.ends_with("containing"),
            }
        }
        "keep-matching" | "remove-matching" => {
            if arg.is_empty() {
                return Err("needs a regex".into());
            }
            Step::FilterRegex {
                re: crate::search::compile(arg)?,
                keep: name == "keep-matching",
            }
        }
        "expand-to" => {
            let lang = lang.ok_or("no syntax tree: file type not recognized")?;
            let kinds: Vec<String> = arg.split_whitespace().map(str::to_string).collect();
            if kinds.is_empty() {
                return Err("needs one or more node kinds, such as function_item".into());
            }
            if let Some(bad) = kinds.iter().find(|k| !lang.has_node_kind(k)) {
                return Err(format!(
                    "{} has no node kind {bad:?} (kinds are grammar names, such as function_item)",
                    lang.name()
                ));
            }
            Step::ExpandTo { kinds }
        }
        "insert-above" => {
            if arg.is_empty() {
                return Err("needs the text to insert".into());
            }
            Step::InsertAbove {
                text: arg.to_string(),
            }
        }
        other => {
            return Err(format!(
                "{other} cannot be a pipeline step (steps: {})",
                STEPS.join(", ")
            ));
        }
    })
}

/// Runs a `:` command line made of one or more steps, and reports the
/// selection count after each step in the status line. On any error the
/// status line says which step failed and nothing changes.
pub fn execute(ed: &mut Editor, line: &str) {
    if let Err(e) = run(ed, line) {
        ed.error(e);
    }
}

fn run(ed: &mut Editor, line: &str) -> Result<(), String> {
    let parts = split_steps(line);
    let many = parts.len() > 1;
    let label = |i: usize, name: &str| {
        if many {
            format!("step {} ({name})", i + 1)
        } else {
            name.to_string()
        }
    };

    // Check every step before running any.
    let lang = ed.buffer().syntax.language();
    let mut steps: Vec<(&str, Step)> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return Err(format!("step {} is empty", i + 1));
        }
        let name = part.split_whitespace().next().unwrap_or_default();
        let arg = part[name.len()..].trim();
        let step = parse_step(name, arg, lang).map_err(|e| format!("{}: {e}", label(i, name)))?;
        if matches!(step, Step::InsertAbove { .. }) && i + 1 != parts.len() {
            return Err(format!(
                "{}: insert-above changes the text, so it must be the last step",
                label(i, name)
            ));
        }
        steps.push((name, step));
    }

    let buf = ed.buffer();
    let text = buf.doc.text().clone();
    let syntax = if steps.iter().any(|(_, s)| s.needs_tree()) {
        match buf.syntax.syntax() {
            Some(s) => Some(s),
            None => return Err(buf.syntax.missing_reason()),
        }
    } else {
        None
    };

    // Compute the selections step by step without touching the editor.
    let mut sel = ed.view().selection.clone();
    let mut report: Vec<String> = Vec::new();
    let mut insert: Option<String> = None;
    for (i, (name, step)) in steps.into_iter().enumerate() {
        if let Step::InsertAbove { text } = step {
            insert = Some(text);
            break;
        }
        let (next, merged) = apply_step(&step, syntax, &text, &sel);
        let Some(next) = next else {
            let mut msg = format!("{} left no selections", label(i, name));
            if !report.is_empty() {
                msg.push_str(&format!(" (after {})", report.join(" | ")));
            }
            msg.push_str("; nothing changed");
            return Err(msg);
        };
        let mut entry = format!("{name} {}", next.len());
        if merged > 0 {
            entry.push_str(&format!(" ({merged} nested or overlapping merged)"));
        }
        report.push(entry);
        sel = next;
    }

    match insert {
        None => {
            ed.set_selection(sel);
            ed.info(report.join(" | "));
        }
        Some(line_text) => {
            let (cs, lines) = insert_above(ed, &text, &sel, &line_text)?;
            // The new selections ride on the transaction, so a refused edit
            // (a read-only buffer) leaves the old ones too. Text inserted
            // at a selection's start lands outside it (`Range::map`).
            let after = sel.map(&cs);
            if !ed.apply_or_report(Transaction::new(cs).with_selection(after)) {
                return Ok(());
            }
            let n = ed.view().selection.len();
            report.push(format!(
                "insert-above {n}: inserted {lines} line{} (u undoes)",
                if lines == 1 { "" } else { "s" }
            ));
            ed.info(report.join(" | "));
        }
    }
    Ok(())
}

/// Turns char ranges into inclusive selection ranges, sorted and merged.
/// Returns the selection (or `None` if there are no ranges) and how many
/// ranges merged into others.
fn to_selection(
    text: &Rope,
    ranges: Vec<std::ops::Range<usize>>,
    cursor: usize,
) -> (Option<Selection>, usize) {
    let s = text.slice(..);
    let ranges: Vec<Range> = ranges
        .into_iter()
        .filter(|r| r.start < r.end)
        .map(|r| Range::new(r.start, prev_grapheme_boundary(s, r.end)))
        .collect();
    if ranges.is_empty() {
        return (None, 0);
    }
    let n = ranges.len();
    // The first range that ends past the old cursor becomes primary, so the
    // view stays near where the user was.
    let primary = ranges
        .iter()
        .position(|r| r.to(s) > cursor)
        .unwrap_or(n - 1);
    let sel = Selection::new(ranges, primary).normalize(s);
    let merged = n - sel.len();
    (Some(sel), merged)
}

/// Runs one selection step. Returns the new selection (`None` when nothing
/// is left) and how many ranges merged. `syntax` is present whenever a
/// step needs it ([`Step::needs_tree`]).
fn apply_step(
    step: &Step,
    syntax: Option<&Syntax>,
    text: &Rope,
    sel: &Selection,
) -> (Option<Selection>, usize) {
    let s = text.slice(..);
    let tree = || syntax.expect("the caller checked for a tree");
    match step {
        Step::Select {
            query,
            inside: false,
        } => {
            let found = tree().query_nodes(text, query);
            to_selection(text, found, sel.primary().cursor())
        }
        Step::Select {
            query,
            inside: true,
        } => {
            let found = tree().query_nodes(text, query);
            let mut out = Vec::new();
            for r in sel.iter() {
                let (from, to) = (r.from(), r.to(s));
                out.extend(within(&found, from, to).cloned());
            }
            to_selection(text, out, sel.primary().cursor())
        }
        Step::FilterNodes {
            query,
            keep,
            containing,
        } => {
            let found = tree().query_nodes(text, query);
            let hit = |r: &Range| {
                let (from, to) = (r.from(), r.to(s));
                if *containing {
                    within(&found, from, to).next().is_some()
                } else {
                    found
                        .binary_search_by(|f| f.start.cmp(&from).then(to.cmp(&f.end)))
                        .is_ok()
                }
            };
            let next = sel.transform_many(s, |r| if hit(&r) == *keep { vec![r] } else { vec![] });
            (next, 0)
        }
        Step::FilterRegex { re, keep } => {
            let next = sel.transform_many(s, |r| {
                let hit = !crate::search::matches_in(text, re, r.from(), r.to(s)).is_empty();
                if hit == *keep { vec![r] } else { vec![] }
            });
            (next, 0)
        }
        Step::ExpandTo { kinds } => {
            let kinds: Vec<&str> = kinds.iter().map(String::as_str).collect();
            let found: Vec<std::ops::Range<usize>> = sel
                .iter()
                .filter_map(|r| tree().enclosing_kind(text, r.from(), r.to(s), &kinds))
                .collect();
            to_selection(text, found, sel.primary().cursor())
        }
        Step::InsertAbove { .. } => unreachable!("handled by the caller"),
    }
}

/// The ranges in `found` (sorted by start) that lie inside `from..to`.
fn within(
    found: &[std::ops::Range<usize>],
    from: usize,
    to: usize,
) -> impl Iterator<Item = &std::ops::Range<usize>> {
    let first = found.partition_point(|f| f.start < from);
    found[first..]
        .iter()
        .take_while(move |f| f.start < to)
        .filter(move |f| f.end <= to)
}

/// The change set that inserts `line_text` on a new line above the first
/// line of each selection, indented like that line. Refuses (changing
/// nothing) when a selection starts after the first non-blank character of
/// its line, where the new line would land above other code.
fn insert_above(
    ed: &Editor,
    text: &Rope,
    sel: &Selection,
    line_text: &str,
) -> Result<(ChangeSet, usize), String> {
    let s = text.slice(..);
    let le = ed.doc().line_ending().as_str();
    let mut changes = Vec::new();
    let mut last_line = None;
    for r in sel.iter() {
        let line = s.char_to_line(r.from());
        let start = s.line_to_char(line);
        let indent = line_indent(s, line);
        if r.from() > start + indent.chars().count() {
            return Err(format!(
                "insert-above: the selection on line {} starts after other text on that line, so a line above it would not belong to it; nothing changed",
                line + 1
            ));
        }
        if last_line == Some(line) {
            continue;
        }
        last_line = Some(line);
        changes.push(Change::insert(start, format!("{indent}{line_text}{le}")));
    }
    let lines = changes.len();
    let cs = ChangeSet::from_changes(text.len_chars(), changes).map_err(|e| e.to_string())?;
    Ok((cs, lines))
}

/// Typed-command entry points for [`crate::typed`]. A `:` line with
/// arguments reaches [`execute`] directly (see `typed::execute_line`); these
/// run when a step is invoked by name alone (the automation API's
/// `command.run`, a key binding), which reports the missing argument.
macro_rules! typed_steps {
    ($($fun:ident => $name:literal,)*) => {
        $(
            pub fn $fun(ed: &mut Editor, args: &[&str], _: bool) -> Result<(), String> {
                execute(ed, &format!("{} {}", $name, args.join(" ")));
                Ok(())
            }
        )*
    };
}

typed_steps! {
    select_nodes => "select-nodes",
    select_inside => "select-inside",
    keep_nodes => "keep-nodes",
    remove_nodes => "remove-nodes",
    keep_containing => "keep-containing",
    remove_containing => "remove-containing",
    keep_matching => "keep-matching",
    remove_matching => "remove-matching",
    expand_to => "expand-to",
    insert_above_cmd => "insert-above",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_split_at_top_level_bars_only() {
        assert_eq!(split_steps("a | b|c"), ["a", "b", "c"]);
        assert_eq!(
            split_steps(
                r#"keep-nodes ((x) @a (#match? @a "p|q")) | keep-matching (a|b) | k [|] {|}"#
            ),
            [
                r#"keep-nodes ((x) @a (#match? @a "p|q"))"#,
                "keep-matching (a|b)",
                "k [|] {|}"
            ]
        );
        assert_eq!(
            split_steps(r"keep-matching a\|b | keep-matching \(x | y"),
            [r"keep-matching a\|b", r"keep-matching \(x", "y"]
        );
        assert_eq!(split_steps("a |"), ["a", ""]);
    }
}
