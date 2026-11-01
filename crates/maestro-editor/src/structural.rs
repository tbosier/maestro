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

use maestro_core::text::prev_grapheme_boundary;
use maestro_core::{Change, ChangeSet, Range, Rope, RopeSlice, Selection, Transaction};
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

/// Splits a command line into steps at each `|` that is outside
/// parentheses, outside a `[...]` bracket (a regex character class or a
/// query alternation), and outside a double-quoted string inside
/// parentheses (a query predicate's argument). A backslash keeps the next
/// character from counting, so `\|` and `\(` do not split or nest. Inside a
/// bracket nothing counts until its closing `]`; a `]` right after `[` or
/// `[^` belongs to the class, as in a regex.
pub fn split_steps(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    // Some(n): inside a bracket, with n chars of it seen so far.
    let mut bracket: Option<usize> = None;
    let mut caret = false;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            if let Some(n) = &mut bracket {
                *n += 1;
            }
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if let Some(n) = bracket {
            let literal_close = n == 0 || (n == 1 && caret);
            if c == ']' && !literal_close {
                bracket = None;
            } else {
                caret = n == 0 && c == '^';
                bracket = Some(n + 1);
            }
            continue;
        }
        if quoted {
            if c == '"' {
                quoted = false;
            }
            continue;
        }
        match c {
            '"' if depth > 0 => quoted = true,
            '[' => {
                bracket = Some(0);
                caret = false;
            }
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
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
            if arg.chars().any(maestro_core::text::is_line_ending_char) {
                return Err("takes one line of text, without line breaks".into());
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

    // Run the selection steps on a list of candidate ranges that may nest
    // (a function inside a function), without touching the editor. Merging
    // them into a selection waits until the end, so a filter that drops an
    // outer function keeps a matching one inside it.
    let s = text.slice(..);
    let cursor = ed.view().selection.primary().cursor();
    let mut cands: Vec<Range> = ed.view().selection.ranges().to_vec();
    let mut report: Vec<String> = Vec::new();
    let mut insert: Option<String> = None;
    for (i, (name, step)) in steps.into_iter().enumerate() {
        if let Step::InsertAbove { text } = step {
            insert = Some(text);
            break;
        }
        cands = apply_step(&step, syntax, &text, cands);
        if cands.is_empty() {
            let mut msg = format!("{} left no selections", label(i, name));
            if !report.is_empty() {
                msg.push_str(&format!(" (after {})", report.join(" | ")));
            }
            msg.push_str("; nothing changed");
            return Err(msg);
        }
        report.push(format!("{name} {}", cands.len()));
    }

    // The first candidate that ends past the old cursor becomes primary, so
    // the view stays near where the user was (and a filter that keeps the
    // old primary keeps it primary).
    let primary = cands
        .iter()
        .position(|r| r.to(s) > cursor)
        .unwrap_or(cands.len() - 1);
    let sel = Selection::new(cands.clone(), primary).normalize(s);
    let merged = cands.len() - sel.len();
    let merge_note = if merged > 0 {
        format!(
            "; {} selections after merging {merged} nested in others",
            sel.len()
        )
    } else {
        String::new()
    };

    match insert {
        None => {
            ed.set_selection(sel);
            ed.info(format!("{}{merge_note}", report.join(" | ")));
        }
        Some(line_text) => {
            let (cs, lines) = insert_above(ed, &text, &cands, &line_text)?;
            // The new selections ride on the transaction, so a refused edit
            // (a read-only buffer) leaves the old ones too. Text inserted
            // at a selection's start lands outside it (`Range::map`).
            let after = sel.map(&cs);
            // Close any undo group the user has open (insert mode, through
            // the automation API), so one undo reverts exactly this edit.
            ed.commit();
            if !ed.apply_or_report(Transaction::new(cs).with_selection(after)) {
                return Ok(());
            }
            report.push(format!(
                "insert-above {}: inserted {lines} line{} (u undoes)",
                cands.len(),
                if lines == 1 { "" } else { "s" }
            ));
            ed.info(format!("{}{merge_note}", report.join(" | ")));
        }
    }
    Ok(())
}

/// The covered char span `from..to` of an inclusive range.
fn span(s: RopeSlice, r: &Range) -> (usize, usize) {
    (r.from(), r.to(s))
}

/// Inclusive ranges for char spans, sorted by start (longest first) with
/// duplicates removed. Nested spans stay.
fn candidates(s: RopeSlice, spans: impl IntoIterator<Item = std::ops::Range<usize>>) -> Vec<Range> {
    let mut spans: Vec<(usize, usize)> = spans
        .into_iter()
        .filter(|r| r.start < r.end)
        .map(|r| (r.start, r.end))
        .collect();
    spans.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    spans.dedup();
    spans
        .into_iter()
        .map(|(a, b)| Range::new(a, prev_grapheme_boundary(s, b)))
        .collect()
}

/// Runs one selection step on the candidates and returns the new ones
/// (empty when nothing is left). `syntax` is present whenever a step needs
/// it ([`Step::needs_tree`]).
fn apply_step(step: &Step, syntax: Option<&Syntax>, text: &Rope, cands: Vec<Range>) -> Vec<Range> {
    let s = text.slice(..);
    let tree = || syntax.expect("the caller checked for a tree");
    match step {
        Step::Select {
            query,
            inside: false,
        } => candidates(s, tree().query_nodes(text, query)),
        Step::Select {
            query,
            inside: true,
        } => {
            let found = tree().query_nodes(text, query);
            let inner = cands.iter().flat_map(|r| {
                let (from, to) = span(s, r);
                within(&found, from, to).cloned().collect::<Vec<_>>()
            });
            candidates(s, inner)
        }
        Step::FilterNodes {
            query,
            keep,
            containing,
        } => {
            let found = tree().query_nodes(text, query);
            let hit = |r: &Range| {
                let (from, to) = span(s, r);
                if *containing {
                    within(&found, from, to).next().is_some()
                } else {
                    found
                        .binary_search_by(|f| f.start.cmp(&from).then(to.cmp(&f.end)))
                        .is_ok()
                }
            };
            cands.into_iter().filter(|r| hit(r) == *keep).collect()
        }
        Step::FilterRegex { re, keep } => cands
            .into_iter()
            .filter(|r| {
                let (from, to) = span(s, r);
                re.is_match(&s.slice(from..to).to_string()) == *keep
            })
            .collect(),
        Step::ExpandTo { kinds } => {
            let kinds: Vec<&str> = kinds.iter().map(String::as_str).collect();
            let grown = cands.iter().filter_map(|r| {
                let (from, to) = span(s, r);
                tree().enclosing_kind(text, from, to, &kinds)
            });
            candidates(s, grown)
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

/// The change set that inserts `line_text` on a new line above the line of
/// each candidate's start, indented like that line, and how many lines it
/// inserts. Lines end at `\n` here, as they do for compilers: Ropey also
/// breaks lines at a lone CR, form feed or U+2028, and inserting after one
/// of those inside a `//` comment would end the comment early. Refuses
/// (changing nothing) when a candidate starts after other text on its
/// line, where the new line would sit above other code.
fn insert_above(
    ed: &Editor,
    text: &Rope,
    cands: &[Range],
    line_text: &str,
) -> Result<(ChangeSet, usize), String> {
    let s = text.slice(..);
    let le = ed.doc().line_ending().as_str();
    let mut starts: Vec<usize> = Vec::new();
    for r in cands {
        let from = r.from();
        let mut start = from;
        while start > 0 && s.char(start - 1) != '\n' {
            start -= 1;
        }
        let blank = s.slice(start..from).chars().all(|c| c == ' ' || c == '\t');
        if !blank {
            return Err(format!(
                "insert-above: the selection on line {} starts after other text on that line, so a line above it would not belong to it; nothing changed",
                s.char_to_line(from) + 1
            ));
        }
        starts.push(start);
    }
    starts.sort_unstable();
    starts.dedup();
    let changes: Vec<Change> = starts
        .iter()
        .map(|&start| {
            let indent: String = s
                .slice(start..)
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            Change::insert(start, format!("{indent}{line_text}{le}"))
        })
        .collect();
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
                r#"keep-nodes ((x) @a (#match? @a "p|q")) | keep-matching (a|b) | k [|] [)|] [^]|] []|]"#
            ),
            [
                r#"keep-nodes ((x) @a (#match? @a "p|q"))"#,
                "keep-matching (a|b)",
                "k [|] [)|] [^]|] []|]"
            ]
        );
        // A regex class with a paren does not swallow the next step.
        assert_eq!(
            split_steps("remove-matching [(] | insert-above #[allow(dead_code)]"),
            ["remove-matching [(]", "insert-above #[allow(dead_code)]"]
        );
        assert_eq!(
            split_steps(r"keep-matching a\|b | keep-matching \(x | y"),
            [r"keep-matching a\|b", r"keep-matching \(x", "y"]
        );
        // A quote outside parentheses is just a character.
        assert_eq!(
            split_steps(r#"keep-matching " | insert-above x"#),
            [r#"keep-matching ""#, "insert-above x"]
        );
        assert_eq!(split_steps("a |"), ["a", ""]);
    }
}
