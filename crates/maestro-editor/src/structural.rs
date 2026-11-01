//! Structural pipelines: select syntax nodes with tree-sitter queries,
//! filter and expand the selections, then change the text at each one.
//!
//! Each step is a `:` command (`:select-nodes fn`), and steps chain with
//! `|` on one command line (`:select-nodes pub-async-fn | keep-nodes
//! returns-result | remove-nodes instrumented | insert-above
//! #[tracing::instrument]`). The whole line is checked before anything
//! runs (step names, queries, regexes, node kinds, quoting), the selection
//! steps are computed without touching the editor, and only then are the
//! selections replaced and the one text-changing step applied. So a
//! failing step changes nothing, and the edit is one transaction and one
//! undo step. `docs/editor-model.md` describes the commands for users.
//!
//! The same pipelines are intent edits for automation clients
//! ([`run_intent`]): `proposal.create` and `document.apply_pipeline` send a
//! pipeline instead of a text diff, and a proposal whose document moved on
//! is rebased by running it again on the current text
//! (`crate::proposals`).

use std::fmt;

use maestro_core::text::prev_grapheme_boundary;
use maestro_core::{
    Change, ChangeSet, DocumentVersion, Range, Rope, RopeSlice, Selection, Transaction,
};
use maestro_syntax::{Language, NodeQuery, Syntax, shortcuts};
use regex::Regex;

use crate::editor::{DocumentId, Editor};

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
    "insert-below",
    "replace-text",
    "delete",
    "wrap",
    "prepend",
    "append",
];

/// The steps that change text. A pipeline has at most one, as its last
/// step.
pub const TEXT_STEPS: &[&str] = &[
    "insert-above",
    "insert-below",
    "replace-text",
    "delete",
    "wrap",
    "prepend",
    "append",
];

/// True if `line` (a `:` command line without the colon) starts with a
/// pipeline step, so [`execute`] should run it.
pub fn handles(line: &str) -> bool {
    STEPS.contains(&first_word(line))
}

/// The step name at the start of `s`: the characters up to the first
/// space or `|`.
fn first_word(s: &str) -> &str {
    let s = s.trim_start();
    let end = s
        .find(|c: char| c.is_whitespace() || c == '|')
        .unwrap_or(s.len());
    &s[..end]
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
    Text(TextStep),
}

/// A step that changes the text at each selection.
enum TextStep {
    InsertAbove(String),
    InsertBelow(String),
    Replace { re: Regex, with: String },
    Delete,
    Wrap(String, String),
    Prepend(String),
    Append(String),
}

impl Step {
    fn needs_tree(&self) -> bool {
        matches!(
            self,
            Step::Select { .. } | Step::FilterNodes { .. } | Step::ExpandTo { .. }
        )
    }
}

/// One step of a pipeline line: its trimmed text and the byte offset in
/// the line where that text starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Part<'a> {
    text: &'a str,
    at: usize,
}

/// Splits a command line into steps at each `|` that does not belong to a
/// step's argument.
///
/// In a selection step, a `|` inside parentheses, inside a `[...]` bracket
/// (a regex character class or a query alternation), or inside a
/// double-quoted string within parentheses (a query predicate's argument)
/// does not split. Inside a bracket nothing counts until its closing `]`;
/// a `]` right after `[` or `[^` belongs to the class, as in a regex.
///
/// In a text step ([`TEXT_STEPS`]), only a double-quoted string keeps a
/// `|` from splitting; parentheses and brackets are just characters, so an
/// unbalanced `(` in a text cannot swallow the rest of the line.
///
/// In both, a backslash keeps the next character from counting, so `\|`
/// does not split.
pub fn split_steps(line: &str) -> Vec<&str> {
    split_parts(line).into_iter().map(|p| p.text).collect()
}

fn split_parts<'a>(line: &'a str) -> Vec<Part<'a>> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    // Some(n): inside a bracket, with n chars of it seen so far.
    let mut bracket: Option<usize> = None;
    let mut caret = false;
    let mut escaped = false;
    let mut start = 0;
    let mut text_mode = TEXT_STEPS.contains(&first_word(line));
    let push = |out: &mut Vec<Part<'a>>, from: usize, to: usize| {
        let raw = &line[from..to];
        let lead = raw.len() - raw.trim_start().len();
        out.push(Part {
            text: raw.trim(),
            at: from + lead,
        });
    };
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
        if text_mode {
            if quoted {
                if c == '"' {
                    quoted = false;
                }
                continue;
            }
            match c {
                '"' => quoted = true,
                '|' => {
                    push(&mut out, start, i);
                    start = i + 1;
                    text_mode = TEXT_STEPS.contains(&first_word(&line[start..]));
                }
                _ => {}
            }
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
                push(&mut out, start, i);
                start = i + 1;
                text_mode = TEXT_STEPS.contains(&first_word(&line[start..]));
            }
            _ => {}
        }
    }
    push(&mut out, start, line.len());
    out
}

/// The one-based char column of byte offset `at` in `line`.
fn column(line: &str, at: usize) -> usize {
    line[..at.min(line.len())].chars().count() + 1
}

/// Why a pipeline line was refused before anything ran: which step, where
/// on the line, and what is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineError {
    /// One-based step number.
    pub step: usize,
    /// The step's name as written (empty for an empty step).
    pub name: String,
    /// One-based char column in the pipeline line, when the error is at a
    /// place in it. A missing syntax tree has none.
    pub column: Option<usize>,
    pub message: String,
    /// The line had more than one step, so messages say `step N (name)`.
    many: bool,
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match (self.name.is_empty(), self.many) {
            (true, _) => format!("step {}", self.step),
            (false, true) => format!("step {} ({})", self.step, self.name),
            (false, false) => self.name.clone(),
        };
        match self.column {
            Some(c) => write!(f, "{label}, column {c}: {}", self.message),
            None => write!(f, "{label}: {}", self.message),
        }
    }
}

impl std::error::Error for PipelineError {}

/// An argument error: the char offset in the argument where it is (when
/// it has a place), and the message.
type ArgError = (Option<usize>, String);

/// Compiles a query argument: a shortcut name for `lang`, or raw
/// tree-sitter query syntax.
fn compile_query(lang: Option<Language>, arg: &str) -> Result<NodeQuery, ArgError> {
    let lang = lang.ok_or((None, "no syntax tree: file type not recognized".into()))?;
    if arg.is_empty() {
        return Err((
            Some(0),
            format!("needs a query or one of: {}", shortcut_names(lang)),
        ));
    }
    let raw = arg.starts_with(['(', '[', '"', '_', ';']);
    if !raw {
        return NodeQuery::shortcut(lang, arg).ok_or_else(|| {
            (
                Some(0),
                format!(
                    "no {} shortcut named {arg:?} (shortcuts: {}; raw queries start with \"(\")",
                    lang.name(),
                    shortcut_names(lang)
                ),
            )
        });
    }
    NodeQuery::new(lang, arg).map_err(|e| {
        if e.row == 0 {
            (Some(e.column), format!("invalid query: {}", e.message))
        } else {
            (Some(0), e.to_string())
        }
    })
}

fn shortcut_names(lang: Language) -> String {
    let names: Vec<&str> = shortcuts(lang).iter().map(|(n, _, _)| *n).collect();
    if names.is_empty() {
        "none for this language".into()
    } else {
        names.join(", ")
    }
}

/// One argument of a text step: its text, its char offset in the step's
/// argument, and whether it was quoted.
struct Word {
    text: String,
    at: usize,
}

/// Splits a text step's argument into words. A word is either a
/// double-quoted string, where `\"`, `\\`, `\n` and `\t` stand for a quote,
/// a backslash, a line break and a tab (any other backslash is kept as
/// written, so regex escapes such as `\d` survive), or a run of non-space
/// characters taken literally.
fn words(arg: &str) -> Result<Vec<Word>, ArgError> {
    let chars: Vec<char> = arg.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let at = i;
        let mut text = String::new();
        if chars[i] == '"' {
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                match chars[i] {
                    '"' => {
                        closed = true;
                        i += 1;
                        break;
                    }
                    '\\' if i + 1 < chars.len() => {
                        match chars[i + 1] {
                            '"' => text.push('"'),
                            '\\' => text.push('\\'),
                            'n' => text.push('\n'),
                            't' => text.push('\t'),
                            other => {
                                text.push('\\');
                                text.push(other);
                            }
                        }
                        i += 2;
                    }
                    c => {
                        text.push(c);
                        i += 1;
                    }
                }
            }
            if !closed {
                return Err((Some(at), "this quoted text has no closing \"".into()));
            }
            if i < chars.len() && !chars[i].is_whitespace() {
                return Err((
                    Some(i),
                    "a quoted text must be followed by a space or the end of the step".into(),
                ));
            }
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                text.push(chars[i]);
                i += 1;
            }
        }
        out.push(Word { text, at });
    }
    Ok(out)
}

/// The one text argument of `insert-above`, `insert-below`, `prepend` and
/// `append`: one quoted string, or the whole argument taken literally.
fn one_text(arg: &str) -> Result<String, ArgError> {
    let text = if arg.starts_with('"') {
        let mut w = words(arg)?;
        if w.len() > 1 {
            return Err((
                Some(w[1].at),
                "takes one text; put the whole text inside the quotes".into(),
            ));
        }
        w.remove(0).text
    } else {
        arg.to_string()
    };
    if text.is_empty() {
        return Err((Some(0), "needs the text to insert".into()));
    }
    Ok(text)
}

/// The two arguments of `wrap` and `replace-text`.
fn two_words(arg: &str, what: &str) -> Result<(Word, Word), ArgError> {
    let mut w = words(arg)?;
    match w.len() {
        2 => {
            let b = w.pop().expect("two");
            let a = w.pop().expect("two");
            Ok((a, b))
        }
        n if n < 2 => Err((
            Some(arg.chars().count()),
            format!("needs two arguments: {what}"),
        )),
        _ => Err((
            Some(w[2].at),
            format!("takes two arguments ({what}); quote an argument that has spaces"),
        )),
    }
}

/// Refuses a replacement that names a capture group the regex does not
/// have; the regex crate would silently put nothing there.
fn check_replacement(re: &Regex, with: &str) -> Result<(), String> {
    let chars: Vec<char> = with.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'$') {
            i += 2;
            continue;
        }
        let (name, next) = if chars.get(i + 1) == Some(&'{') {
            match chars[i + 2..].iter().position(|c| *c == '}') {
                Some(len) => (
                    chars[i + 2..i + 2 + len].iter().collect::<String>(),
                    i + 3 + len,
                ),
                None => (String::new(), chars.len()),
            }
        } else {
            let len = chars[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                .count();
            (
                chars[i + 1..i + 1 + len].iter().collect::<String>(),
                i + 1 + len,
            )
        };
        if !name.is_empty() {
            let known = match name.parse::<usize>() {
                Ok(n) => n < re.captures_len(),
                Err(_) => re.capture_names().flatten().any(|n| n == name),
            };
            if !known {
                return Err(format!(
                    "the replacement refers to group ${name}, which the regex does not have \
                     (write $$ for a literal $, and ${{1}}x when a group number is followed by \
                     a letter or digit)"
                ));
            }
        }
        i = next.max(i + 1);
    }
    Ok(())
}

fn parse_step(name: &str, arg: &str, lang: Option<Language>) -> Result<Step, ArgError> {
    let line_free = |t: &str| -> Result<(), ArgError> {
        if t.chars().any(maestro_core::text::is_line_ending_char) {
            Err((
                Some(0),
                "takes one line of text, without line breaks".into(),
            ))
        } else {
            Ok(())
        }
    };
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
                return Err((Some(0), "needs a regex".into()));
            }
            Step::FilterRegex {
                re: crate::search::compile(arg).map_err(|e| (Some(0), e))?,
                keep: name == "keep-matching",
            }
        }
        "expand-to" => {
            let lang = lang.ok_or((None, "no syntax tree: file type not recognized".into()))?;
            let kinds: Vec<String> = arg.split_whitespace().map(str::to_string).collect();
            if kinds.is_empty() {
                return Err((
                    Some(0),
                    "needs one or more node kinds, such as function_item".into(),
                ));
            }
            if let Some(bad) = kinds.iter().find(|k| !lang.has_node_kind(k)) {
                let at = arg.find(bad.as_str()).map(|b| arg[..b].chars().count());
                return Err((
                    at,
                    format!(
                        "{} has no node kind {bad:?} (kinds are grammar names, such as function_item)",
                        lang.name()
                    ),
                ));
            }
            Step::ExpandTo { kinds }
        }
        "insert-above" | "insert-below" => {
            let text = one_text(arg)?;
            line_free(&text)?;
            Step::Text(if name == "insert-above" {
                TextStep::InsertAbove(text)
            } else {
                TextStep::InsertBelow(text)
            })
        }
        "prepend" => Step::Text(TextStep::Prepend(one_text(arg)?)),
        "append" => Step::Text(TextStep::Append(one_text(arg)?)),
        "delete" => {
            if !arg.is_empty() {
                return Err((Some(0), "takes no argument".into()));
            }
            Step::Text(TextStep::Delete)
        }
        "wrap" => {
            let (a, b) = two_words(arg, "<before> <after>")?;
            if a.text.is_empty() && b.text.is_empty() {
                return Err((Some(0), "needs a text before or after".into()));
            }
            Step::Text(TextStep::Wrap(a.text, b.text))
        }
        "replace-text" => {
            let (pat, with) = two_words(arg, "<regex> <replacement>")?;
            if pat.text.is_empty() {
                return Err((Some(pat.at), "needs a regex".into()));
            }
            let re = crate::search::compile(&pat.text).map_err(|e| (Some(pat.at), e))?;
            check_replacement(&re, &with.text).map_err(|e| (Some(with.at), e))?;
            Step::Text(TextStep::Replace {
                re,
                with: with.text,
            })
        }
        other => {
            return Err((
                None,
                format!(
                    "{other} cannot be a pipeline step (steps: {})",
                    STEPS.join(", ")
                ),
            ));
        }
    })
}

/// A pipeline line, checked and compiled for one language.
pub struct Pipeline {
    steps: Vec<(String, Step)>,
}

impl Pipeline {
    /// Checks and compiles every step of `line` for `lang` (the buffer's
    /// language, if it has one). Nothing runs.
    pub fn parse(line: &str, lang: Option<Language>) -> Result<Pipeline, PipelineError> {
        let parts = split_parts(line);
        let many = parts.len() > 1;
        let mut steps: Vec<(String, Step)> = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            let fail = |name: &str, at: Option<usize>, message: String| PipelineError {
                step: i + 1,
                name: name.to_string(),
                column: at.map(|b| column(line, b)),
                message,
                many,
            };
            if part.text.is_empty() {
                return Err(fail("", Some(part.at), "the step is empty".into()));
            }
            let name = part
                .text
                .split_whitespace()
                .next()
                .expect("a non-empty step has a word");
            let rest = &part.text[name.len()..];
            let arg = rest.trim();
            let arg_at = part.at + name.len() + (rest.len() - rest.trim_start().len());
            let step = parse_step(name, arg, lang).map_err(|(at, msg)| {
                // A misspelled step name points at the name.
                let at = if STEPS.contains(&name) {
                    at.map(|chars| {
                        arg_at + arg.char_indices().nth(chars).map_or(arg.len(), |(b, _)| b)
                    })
                } else {
                    Some(part.at)
                };
                fail(name, at, msg)
            })?;
            if matches!(step, Step::Text(_)) && i + 1 != parts.len() {
                return Err(fail(
                    name,
                    Some(part.at),
                    format!("{name} changes the text, so it must be the last step"),
                ));
            }
            steps.push((name.to_string(), step));
        }
        Ok(Pipeline { steps })
    }

    /// True if a step reads the syntax tree.
    pub fn needs_tree(&self) -> bool {
        self.steps.iter().any(|(_, s)| s.needs_tree())
    }

    /// True if the last step changes text.
    pub fn changes_text(&self) -> bool {
        matches!(self.steps.last(), Some((_, Step::Text(_))))
    }

    /// Runs the steps on `start` (the selections to begin with) without
    /// touching any editor state. `syntax` must be present and match
    /// `text` when [`Pipeline::needs_tree`]. `line_ending` is what
    /// `insert-above` and `insert-below` end their lines with. The
    /// selection that ends first after `cursor` becomes primary.
    pub fn run(
        &self,
        text: &Rope,
        syntax: Option<&Syntax>,
        line_ending: &str,
        start: Vec<Range>,
        cursor: usize,
    ) -> Result<Outcome, RunError> {
        let s = text.slice(..);
        let many = self.steps.len() > 1;
        // Run the selection steps on a list of candidate ranges that may
        // nest (a function inside a function). Merging them into a
        // selection waits until the end, so a filter that drops an outer
        // function keeps a matching one inside it.
        let mut cands = start;
        let mut counts: Vec<StepCount> = Vec::new();
        let mut text_step: Option<(usize, &str, &TextStep)> = None;
        for (i, (name, step)) in self.steps.iter().enumerate() {
            if let Step::Text(t) = step {
                if cands.is_empty() {
                    return Err(RunError::Empty {
                        step: i + 1,
                        name: name.clone(),
                        counts,
                        many,
                    });
                }
                text_step = Some((i, name, t));
                break;
            }
            if step.needs_tree() && syntax.is_none() {
                return Err(RunError::NoTree("no syntax tree for this text".into()));
            }
            cands = apply_step(step, syntax, text, cands);
            if cands.is_empty() {
                return Err(RunError::Empty {
                    step: i + 1,
                    name: name.clone(),
                    counts,
                    many,
                });
            }
            counts.push(StepCount {
                step: name.clone(),
                count: cands.len(),
            });
        }
        let selected = cands.len();
        // The first candidate that ends past the old cursor becomes
        // primary, so the view stays near where the user was (and a filter
        // that keeps the old primary keeps it primary).
        let primary = cands
            .iter()
            .position(|r| r.to(s) > cursor)
            .unwrap_or(selected.saturating_sub(1));
        let selection = Selection::new(cands.clone(), primary).normalize(s);
        let merged = selected - selection.len();
        let edit = match text_step {
            None => None,
            Some((i, name, step)) => {
                let (changes, summary) =
                    apply_text_step(step, text, &cands, &selection, line_ending).map_err(
                        |message| RunError::Refused {
                            step: i + 1,
                            name: name.to_string(),
                            message,
                            counts: counts.clone(),
                        },
                    )?;
                counts.push(StepCount {
                    step: name.to_string(),
                    count: selected,
                });
                let mut new_text = text.clone();
                changes
                    .apply(&mut new_text)
                    .expect("the change set was built for this text");
                Some(TextEdit {
                    step: i + 1,
                    name: name.to_string(),
                    changes,
                    summary,
                    new_text,
                })
            }
        };
        Ok(Outcome {
            counts,
            selected,
            selection,
            merged,
            edit,
        })
    }
}

/// How many selections a step left (for a text step: how many it was
/// given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepCount {
    pub step: String,
    pub count: usize,
}

/// `select-nodes 9 | keep-nodes 8`.
pub fn counts_label(counts: &[StepCount]) -> String {
    counts
        .iter()
        .map(|c| format!("{} {}", c.step, c.count))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// What a pipeline run produced.
pub struct Outcome {
    /// One entry per step that ran, the text step included.
    pub counts: Vec<StepCount>,
    /// Selections after the selection steps, nested ones counted
    /// separately.
    pub selected: usize,
    /// The same, sorted and merged.
    pub selection: Selection,
    /// How many were merged into others.
    pub merged: usize,
    pub edit: Option<TextEdit>,
}

/// The text step's change.
pub struct TextEdit {
    /// One-based step number and name.
    pub step: usize,
    pub name: String,
    pub changes: ChangeSet,
    /// What it did, for the status line: `inserted 5 lines`.
    pub summary: String,
    /// The text with the change applied.
    pub new_text: Rope,
}

/// Why a pipeline stopped while running. Nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// A step needs a syntax tree and there is none.
    NoTree(String),
    /// Step `step` left no selections (or a text step had none to work on).
    Empty {
        step: usize,
        name: String,
        /// The steps before it.
        counts: Vec<StepCount>,
        many: bool,
    },
    /// The text step refused (a line above a selection that starts after
    /// other text on its line).
    Refused {
        step: usize,
        name: String,
        message: String,
        counts: Vec<StepCount>,
    },
}

impl RunError {
    /// Counts of the steps that ran before it stopped.
    pub fn counts(&self) -> &[StepCount] {
        match self {
            RunError::NoTree(_) => &[],
            RunError::Empty { counts, .. } | RunError::Refused { counts, .. } => counts,
        }
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::NoTree(why) => write!(f, "{why}"),
            RunError::Empty {
                step,
                name,
                counts,
                many,
            } => {
                if *many {
                    write!(f, "step {step} ({name}) left no selections")?;
                } else {
                    write!(f, "{name} left no selections")?;
                }
                if !counts.is_empty() {
                    write!(f, " (after {})", counts_label(counts))?;
                }
                write!(f, "; nothing changed")
            }
            RunError::Refused { message, .. } => write!(f, "{message}"),
        }
    }
}

/// Runs a `:` command line made of one or more steps, and reports the
/// selection count after each step in the status line. On any error the
/// status line says which step failed and nothing changes.
pub fn execute(ed: &mut Editor, line: &str) {
    if let Err(e) = run_command_line(ed, line) {
        ed.error(e);
    }
}

fn run_command_line(ed: &mut Editor, line: &str) -> Result<(), String> {
    let lang = ed.buffer().syntax.language();
    let pipeline = Pipeline::parse(line, lang).map_err(|e| e.to_string())?;
    let buf = ed.buffer();
    let text = buf.doc.text().clone();
    let syntax = if pipeline.needs_tree() {
        match buf.syntax.syntax() {
            Some(s) => Some(s),
            None => return Err(buf.syntax.missing_reason()),
        }
    } else {
        None
    };
    let le = buf.doc.line_ending().as_str();
    let cursor = ed.view().selection.primary().cursor();
    let start = ed.view().selection.ranges().to_vec();
    let out = pipeline
        .run(&text, syntax, le, start, cursor)
        .map_err(|e| e.to_string())?;

    let merge_note = if out.merged > 0 {
        format!(
            "; {} selections after merging {} nested in others",
            out.selection.len(),
            out.merged
        )
    } else {
        String::new()
    };
    match out.edit {
        None => {
            ed.set_selection(out.selection);
            ed.info(format!("{}{merge_note}", counts_label(&out.counts)));
        }
        Some(edit) => {
            if edit.new_text == text {
                let label = if out.counts.len() > 1 {
                    format!("step {} ({})", edit.step, edit.name)
                } else {
                    edit.name.clone()
                };
                return Err(format!(
                    "{label} changed nothing ({}); nothing changed",
                    edit.summary
                ));
            }
            // The new selections ride on the transaction, so a refused edit
            // (a read-only buffer) leaves the old ones too. Text inserted
            // at a selection's start lands outside it (`Range::map`).
            let after = out.selection.map(&edit.changes);
            // Close any undo group the user has open (insert mode, through
            // the automation API), so one undo reverts exactly this edit.
            ed.commit();
            if !ed.apply_or_report(Transaction::new(edit.changes).with_selection(after)) {
                return Ok(());
            }
            ed.info(format!(
                "{}: {} (u undoes){merge_note}",
                counts_label(&out.counts),
                edit.summary
            ));
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
        Step::Text(_) => unreachable!("handled by the caller"),
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

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The change set a text step makes, and what it did in words. The line
/// steps work on every candidate (nested ones too, one line per line); the
/// others on the merged selections, which never overlap.
fn apply_text_step(
    step: &TextStep,
    text: &Rope,
    cands: &[Range],
    sel: &Selection,
    le: &str,
) -> Result<(ChangeSet, String), String> {
    let s = text.slice(..);
    let len = text.len_chars();
    let build =
        |changes: Vec<Change>| ChangeSet::from_changes(len, changes).map_err(|e| e.to_string());
    let spans: Vec<(usize, usize)> = sel.iter().map(|r| span(s, r)).collect();
    let n = spans.len();
    match step {
        TextStep::InsertAbove(line) => {
            let (cs, lines) = insert_above(text, cands, line, le)?;
            Ok((cs, format!("inserted {}", plural(lines, "line", "lines"))))
        }
        TextStep::InsertBelow(line) => {
            let (cs, lines) = insert_below(text, cands, line, le)?;
            Ok((cs, format!("inserted {}", plural(lines, "line", "lines"))))
        }
        TextStep::Delete => {
            let cs = build(spans.iter().map(|&(a, b)| Change::delete(a, b)).collect())?;
            Ok((
                cs,
                format!("deleted {}", plural(n, "selection", "selections")),
            ))
        }
        TextStep::Prepend(t) => {
            let cs = build(spans.iter().map(|&(a, _)| Change::insert(a, t)).collect())?;
            Ok((
                cs,
                format!("prepended to {}", plural(n, "selection", "selections")),
            ))
        }
        TextStep::Append(t) => {
            let cs = build(spans.iter().map(|&(_, b)| Change::insert(b, t)).collect())?;
            Ok((
                cs,
                format!("appended to {}", plural(n, "selection", "selections")),
            ))
        }
        TextStep::Wrap(before, after) => {
            let mut changes = Vec::new();
            for &(a, b) in &spans {
                if !before.is_empty() {
                    changes.push(Change::insert(a, before));
                }
                if !after.is_empty() {
                    changes.push(Change::insert(b, after));
                }
            }
            let cs = build(changes)?;
            Ok((
                cs,
                format!("wrapped {}", plural(n, "selection", "selections")),
            ))
        }
        TextStep::Replace { re, with } => {
            let mut changes = Vec::new();
            let mut matches = 0;
            let mut hit = 0;
            for &(a, b) in &spans {
                let part = s.slice(a..b).to_string();
                let before = matches;
                // Char offsets of byte positions in `part`, walked forward.
                let mut byte = 0;
                let mut chars = 0;
                let mut char_at = |target: usize| {
                    chars += part[byte..target].chars().count();
                    byte = target;
                    chars
                };
                for caps in re.captures_iter(&part) {
                    let m = caps.get(0).expect("group 0 is the match");
                    let mut out = String::new();
                    caps.expand(with, &mut out);
                    matches += 1;
                    let from = a + char_at(m.start());
                    let to = a + char_at(m.end());
                    if out != m.as_str() {
                        changes.push(Change::replace(from, to, out));
                    }
                }
                if matches > before {
                    hit += 1;
                }
            }
            let cs = build(changes)?;
            Ok((
                cs,
                format!(
                    "replaced {} in {}",
                    plural(matches, "match", "matches"),
                    plural(hit, "selection", "selections")
                ),
            ))
        }
    }
}

/// The indentation (spaces and tabs) at the start of the line beginning at
/// char `start`.
fn indent_at(s: RopeSlice, start: usize) -> String {
    s.slice(start..)
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// The start of the line holding char `at`. Lines end at `\n` here, as
/// they do for compilers: Ropey also breaks lines at a lone CR, form feed
/// or U+2028, and inserting after one of those inside a `//` comment would
/// end the comment early.
fn line_start(s: RopeSlice, at: usize) -> usize {
    let mut start = at;
    while start > 0 && s.char(start - 1) != '\n' {
        start -= 1;
    }
    start
}

/// The change set that inserts `line_text` on a new line above the line of
/// each candidate's start, indented like that line, and how many lines it
/// inserts. Refuses (changing nothing) when a candidate starts after other
/// text on its line, where the new line would sit above other code.
fn insert_above(
    text: &Rope,
    cands: &[Range],
    line_text: &str,
    le: &str,
) -> Result<(ChangeSet, usize), String> {
    let s = text.slice(..);
    let mut starts: Vec<usize> = Vec::new();
    for r in cands {
        let from = r.from();
        let start = line_start(s, from);
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
            let indent = indent_at(s, start);
            Change::insert(start, format!("{indent}{line_text}{le}"))
        })
        .collect();
    let lines = changes.len();
    let cs = ChangeSet::from_changes(text.len_chars(), changes).map_err(|e| e.to_string())?;
    Ok((cs, lines))
}

/// The change set that inserts `line_text` on a new line below the line
/// where each candidate ends, indented like the line where it starts, and
/// how many lines it inserts. Refuses (changing nothing) when a candidate
/// ends before other text on its line, where the new line would sit below
/// other code. At the end of a text without a final line break, the new
/// line gets one in front instead of after.
fn insert_below(
    text: &Rope,
    cands: &[Range],
    line_text: &str,
    le: &str,
) -> Result<(ChangeSet, usize), String> {
    let s = text.slice(..);
    let len = text.len_chars();
    // (insert position, the text there)
    let mut at: Vec<(usize, String)> = Vec::new();
    for r in cands {
        let (from, to) = span(s, r);
        let indent = indent_at(s, line_start(s, from));
        // A selection that ends with its line break (a whole line from
        // `x`) already ends where the next line starts.
        if to > 0 && s.char(to - 1) == '\n' {
            at.push((to, format!("{indent}{line_text}{le}")));
            continue;
        }
        let mut end = to;
        while end < len && s.char(end) != '\n' {
            let c = s.char(end);
            let line_break_cr = c == '\r' && end + 1 < len && s.char(end + 1) == '\n';
            if c != ' ' && c != '\t' && !line_break_cr {
                return Err(format!(
                    "insert-below: the selection on line {} ends before other text on that line, so a line below it would not belong to it; nothing changed",
                    s.char_to_line(to.saturating_sub(1)) + 1
                ));
            }
            end += 1;
        }
        if end < len {
            at.push((end + 1, format!("{indent}{line_text}{le}")));
        } else {
            at.push((len, format!("{le}{indent}{line_text}")));
        }
    }
    // Several selections ending on one line get one new line: the first.
    at.sort_by_key(|(p, _)| *p);
    at.dedup_by_key(|(p, _)| *p);
    let lines = at.len();
    let changes = at.into_iter().map(|(p, t)| Change::insert(p, t));
    let cs = ChangeSet::from_changes(len, changes).map_err(|e| e.to_string())?;
    Ok((cs, lines))
}

// ----- intent edits ----------------------------------------------------------

/// A pipeline run against a document as an intent edit.
#[derive(Debug, Clone)]
pub struct IntentRun {
    /// The change against the document at `version`.
    pub changes: ChangeSet,
    /// The version the pipeline ran against.
    pub version: DocumentVersion,
    /// One entry per step, the text step included.
    pub counts: Vec<StepCount>,
    /// Separate changes in the change set (insertions, deletions,
    /// replacements).
    pub edit_count: usize,
}

/// Why an intent edit ran but cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentFailure {
    /// A step needs a syntax tree and the document has none.
    NoTree,
    /// A step left no selections.
    NoSelections { step: usize },
    /// The text step refused.
    Refused { step: usize },
    /// The text step left the text as it was.
    NoChange,
    /// The selection count after the selection steps is not `expect`.
    Expect { expected: usize, actual: usize },
}

impl IntentFailure {
    /// The machine-readable name the protocol reports.
    pub fn code(&self) -> &'static str {
        match self {
            IntentFailure::NoTree => "no_tree",
            IntentFailure::NoSelections { .. } => "no_selections",
            IntentFailure::Refused { .. } => "refused",
            IntentFailure::NoChange => "no_change",
            IntentFailure::Expect { .. } => "expect_mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentError {
    /// The pipeline line is invalid; nothing ran.
    Invalid(PipelineError),
    /// It ran and failed.
    Failed {
        failure: IntentFailure,
        message: String,
        /// The steps that ran.
        counts: Vec<StepCount>,
    },
}

impl fmt::Display for IntentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IntentError::Invalid(e) => write!(f, "{e}"),
            IntentError::Failed { message, .. } => write!(f, "{message}"),
        }
    }
}

/// Runs pipeline `source` on document `doc` as an intent edit, without
/// changing anything. It starts from one selection over the whole
/// document, so it means the same whatever the user has selected. The
/// pipeline must be one line and end in a text step. With `expect`, the
/// selection count after the selection steps (nested matches counted
/// separately, as the status line counts them) must equal it. A run that
/// selects nothing or changes nothing is an error too.
///
/// The buffer's syntax tree is used when it matches the text; while a
/// background parse is pending, the text is parsed here.
pub fn run_intent(
    ed: &Editor,
    doc: DocumentId,
    source: &str,
    expect: Option<usize>,
) -> Result<IntentRun, IntentError> {
    let buf = &ed.buffers[&doc];
    let lang = buf.syntax.language();
    if let Some(b) = source.find(maestro_core::text::is_line_ending_char) {
        let parts = split_parts(source);
        let i = parts.iter().rposition(|p| p.at <= b).unwrap_or(0);
        return Err(IntentError::Invalid(PipelineError {
            step: i + 1,
            name: first_word(parts[i].text).to_string(),
            column: Some(column(source, b)),
            message: "a pipeline is one line; write \\n inside a quoted text for a line break"
                .into(),
            many: parts.len() > 1,
        }));
    }
    let pipeline = Pipeline::parse(source, lang).map_err(IntentError::Invalid)?;
    if !pipeline.changes_text() {
        let parts = split_parts(source);
        let last = parts.last().expect("a pipeline has a step");
        return Err(IntentError::Invalid(PipelineError {
            step: parts.len(),
            name: first_word(last.text).to_string(),
            column: Some(column(source, last.at)),
            message: format!(
                "the pipeline changes no text; end it with one of {}",
                TEXT_STEPS.join(", ")
            ),
            many: parts.len() > 1,
        }));
    }
    let text = buf.doc.text();
    let parsed;
    let syntax = if pipeline.needs_tree() {
        match buf.syntax.syntax() {
            Some(s) => Some(s),
            None => {
                parsed = lang.and_then(|l| Syntax::parse(l, text));
                match &parsed {
                    Some(s) => Some(s),
                    None => {
                        return Err(IntentError::Failed {
                            failure: IntentFailure::NoTree,
                            message: buf.syntax.missing_reason(),
                            counts: Vec::new(),
                        });
                    }
                }
            }
        }
    } else {
        None
    };
    let s = text.slice(..);
    let whole = candidates(s, std::iter::once(0..text.len_chars()));
    let le = buf.doc.line_ending().as_str();
    let out = pipeline.run(text, syntax, le, whole, 0).map_err(|e| {
        let failure = match &e {
            RunError::NoTree(_) => IntentFailure::NoTree,
            RunError::Empty { step, .. } => IntentFailure::NoSelections { step: *step },
            RunError::Refused { step, .. } => IntentFailure::Refused { step: *step },
        };
        IntentError::Failed {
            failure,
            message: e.to_string(),
            counts: e.counts().to_vec(),
        }
    })?;
    let edit = out.edit.expect("the pipeline ends in a text step");
    if let Some(expected) = expect
        && expected != out.selected
    {
        let selection_counts = &out.counts[..out.counts.len() - 1];
        return Err(IntentError::Failed {
            failure: IntentFailure::Expect {
                expected,
                actual: out.selected,
            },
            message: format!(
                "the pipeline selected {} where {expected} {} expected ({}); nothing changed",
                out.selected,
                if expected == 1 { "was" } else { "were" },
                if selection_counts.is_empty() {
                    "the whole document".to_string()
                } else {
                    counts_label(selection_counts)
                }
            ),
            counts: out.counts,
        });
    }
    if edit.new_text == *text {
        return Err(IntentError::Failed {
            failure: IntentFailure::NoChange,
            message: format!(
                "the pipeline changed nothing: {} {} ({}); nothing changed",
                edit.name, out.selected, edit.summary
            ),
            counts: out.counts,
        });
    }
    Ok(IntentRun {
        edit_count: edit.changes.changes().len(),
        changes: edit.changes,
        version: buf.doc.version(),
        counts: out.counts,
    })
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
    insert_below_cmd => "insert-below",
    replace_text_cmd => "replace-text",
    delete_cmd => "delete",
    wrap_cmd => "wrap",
    prepend_cmd => "prepend",
    append_cmd => "append",
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

    #[test]
    fn text_steps_split_only_outside_quotes() {
        // Quotes protect a bar; parentheses and brackets do not.
        assert_eq!(
            split_steps(r#"select-nodes fn | append " | x" | delete"#),
            ["select-nodes fn", r#"append " | x""#, "delete"]
        );
        assert_eq!(
            split_steps("select-nodes fn | prepend ( | append )"),
            ["select-nodes fn", "prepend (", "append )"]
        );
        assert_eq!(
            split_steps(r#"wrap "a\"|" b | x"#),
            [r#"wrap "a\"|" b"#, "x"]
        );
        assert_eq!(split_steps(r"append a\|b"), [r"append a\|b"]);
    }

    #[test]
    fn words_decode_four_escapes_and_keep_the_rest() {
        let w = words(r#"  "a\"b\\c\nd\te\d" bare\n"#).unwrap();
        assert_eq!(w[0].text, "a\"b\\c\nd\te\\d");
        assert_eq!(w[0].at, 2);
        assert_eq!(w[1].text, r"bare\n");
        assert_eq!(words(r#""open"#).err().unwrap().0, Some(0));
        assert_eq!(words(r#""a"b"#).err().unwrap().0, Some(3));
    }

    #[test]
    fn replacement_groups_are_checked() {
        let re = Regex::new(r"(?P<n>\w+)\((\d)\)").unwrap();
        assert!(check_replacement(&re, "$n ${2} $$3 $1").is_ok());
        assert!(check_replacement(&re, "$3").is_err());
        assert!(check_replacement(&re, "$1x").is_err());
        assert!(check_replacement(&re, "${name}").is_err());
    }
}
