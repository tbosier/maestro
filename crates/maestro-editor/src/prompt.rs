//! The single-line prompt used for `:` commands, search and regex selection.
//! Regex prompts preview their effect as you type and restore the original
//! selection on escape.

use maestro_core::{Range, Selection};

use crate::editor::{Editor, Severity};
use crate::key::{KeyCode, KeyEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Command,
    Search {
        backward: bool,
    },
    SelectRegex,
    SplitRegex,
    Keep {
        remove: bool,
    },
    /// Workspace search; Enter starts it.
    GlobalSearch,
    /// A new name for the symbol at the cursor (`crate::lsp::actions`).
    Rename,
    /// The path of a project to add (`crate::workspaces::manage`); `tab`
    /// completes directory names.
    ProjectAdd,
}

impl PromptKind {
    pub fn label(self) -> &'static str {
        match self {
            PromptKind::Command => ":",
            PromptKind::Search { backward: false } => "/",
            PromptKind::Search { backward: true } => "?",
            PromptKind::SelectRegex => "select: ",
            PromptKind::SplitRegex => "split: ",
            PromptKind::Keep { remove: false } => "keep: ",
            PromptKind::Keep { remove: true } => "remove: ",
            PromptKind::GlobalSearch => "search workspace: ",
            PromptKind::Rename => "rename to: ",
            PromptKind::ProjectAdd => "add project: ",
        }
    }

    fn history_key(self) -> char {
        match self {
            PromptKind::Command => ':',
            PromptKind::Rename => 'n',
            PromptKind::ProjectAdd => 'p',
            _ => '/',
        }
    }
}

#[derive(Debug, Clone)]
pub struct Prompt {
    pub kind: PromptKind,
    pub input: String,
    /// Byte index into `input`.
    pub cursor: usize,
    history_pos: Option<usize>,
    pub original: Selection,
    original_top: usize,
    pub completions: Vec<String>,
    completion_idx: Option<usize>,
    /// Live feedback, such as an invalid regex.
    pub feedback: Option<String>,
}

pub fn open(editor: &mut Editor, kind: PromptKind) {
    editor.prompt = Some(Prompt {
        kind,
        input: String::new(),
        cursor: 0,
        history_pos: None,
        original: editor.view().selection.clone(),
        original_top: editor.view().top_line,
        completions: Vec::new(),
        completion_idx: None,
        feedback: None,
    });
}

pub fn handle_key(editor: &mut Editor, key: KeyEvent) {
    let Some(mut p) = editor.prompt.take() else {
        return;
    };
    let ctrl = key.mods.ctrl;
    match key.code {
        KeyCode::Esc => {
            cancel(editor, &p);
            return;
        }
        KeyCode::Char('c') if ctrl => {
            cancel(editor, &p);
            return;
        }
        KeyCode::Enter => {
            finish(editor, p);
            return;
        }
        KeyCode::Backspace if p.input.is_empty() => {
            cancel(editor, &p);
            return;
        }
        KeyCode::Backspace => {
            if let Some((i, _)) = p.input[..p.cursor].char_indices().next_back() {
                p.input.replace_range(i..p.cursor, "");
                p.cursor = i;
            }
        }
        KeyCode::Delete => {
            if let Some(c) = p.input[p.cursor..].chars().next() {
                p.input.replace_range(p.cursor..p.cursor + c.len_utf8(), "");
            }
        }
        KeyCode::Left => {
            if let Some((i, _)) = p.input[..p.cursor].char_indices().next_back() {
                p.cursor = i;
            }
        }
        KeyCode::Right => {
            if let Some(c) = p.input[p.cursor..].chars().next() {
                p.cursor += c.len_utf8();
            }
        }
        KeyCode::Home => p.cursor = 0,
        KeyCode::Char('a') if ctrl => p.cursor = 0,
        KeyCode::End => p.cursor = p.input.len(),
        KeyCode::Char('e') if ctrl => p.cursor = p.input.len(),
        KeyCode::Char('u') if ctrl => {
            p.input.replace_range(..p.cursor, "");
            p.cursor = 0;
        }
        KeyCode::Char('w') if ctrl => {
            let before = &p.input[..p.cursor];
            let trimmed = before.trim_end();
            let start = trimmed
                .char_indices()
                .rfind(|&(_, c)| c.is_whitespace() || c == '/')
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(0);
            p.input.replace_range(start..p.cursor, "");
            p.cursor = start;
        }
        KeyCode::Up | KeyCode::Down => {
            let hist = editor
                .registers
                .history
                .get(&p.kind.history_key())
                .cloned()
                .unwrap_or_default();
            if !hist.is_empty() {
                let pos = match (p.history_pos, key.code) {
                    (None, KeyCode::Up) => Some(hist.len() - 1),
                    (Some(i), KeyCode::Up) => Some(i.saturating_sub(1)),
                    (Some(i), KeyCode::Down) if i + 1 < hist.len() => Some(i + 1),
                    _ => None,
                };
                p.history_pos = pos;
                p.input = pos.map(|i| hist[i].clone()).unwrap_or_default();
                p.cursor = p.input.len();
            }
        }
        KeyCode::Tab | KeyCode::BackTab
            if matches!(p.kind, PromptKind::Command | PromptKind::ProjectAdd) =>
        {
            complete(editor, &mut p, key.code == KeyCode::BackTab);
            editor.prompt = Some(p);
            return;
        }
        _ => {
            if let Some(c) = key.typed_char() {
                p.input.insert(p.cursor, c);
                p.cursor += c.len_utf8();
            } else {
                editor.prompt = Some(p);
                return;
            }
        }
    }
    p.completions.clear();
    p.completion_idx = None;
    preview(editor, &mut p);
    editor.prompt = Some(p);
}

/// Inserts pasted text at the prompt cursor. Line breaks become spaces,
/// since a prompt is one line.
pub fn insert_text(editor: &mut Editor, text: &str) {
    let Some(mut p) = editor.prompt.take() else {
        return;
    };
    let clean: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    p.input.insert_str(p.cursor, &clean);
    p.cursor += clean.len();
    p.completions.clear();
    p.completion_idx = None;
    preview(editor, &mut p);
    editor.prompt = Some(p);
}

fn cancel(editor: &mut Editor, p: &Prompt) {
    if p.kind != PromptKind::Command {
        // Saved selections follow edits (see `track_text_change`); clamping
        // again guards insert mode, where the dispatcher does not clamp.
        let text = editor.doc().text().clone();
        editor.view_mut().selection = p.original.clone().normalize(text.slice(..));
        editor.view_mut().top_line = p.original_top;
    }
}

fn finish(editor: &mut Editor, p: Prompt) {
    editor
        .registers
        .push_history(p.kind.history_key(), &p.input);
    match p.kind {
        PromptKind::Command => {
            let line = p.input.clone();
            crate::typed::execute_line(editor, &line);
        }
        PromptKind::Search { backward } => {
            if p.input.is_empty() {
                // Empty search repeats the last one, like Vim.
                editor.view_mut().selection = p.original.clone();
                if editor.last_search.is_some() {
                    crate::commands::search_next_impl(editor, backward);
                }
                return;
            }
            editor.last_search = Some(p.input.clone());
            editor.registers.set('/', vec![p.input.clone()]);
            editor.view_mut().selection = p.original.clone();
            editor.push_jump();
            crate::commands::search_next_impl(editor, backward);
        }
        PromptKind::GlobalSearch => {
            editor.view_mut().selection = p.original;
            if let Err(e) = crate::navigation::grep::start(editor, &p.input) {
                editor.set_status(Severity::Error, e);
            }
        }
        PromptKind::Rename => crate::lsp::actions::rename_prompt_done(editor, &p.input),
        PromptKind::ProjectAdd => crate::workspaces::manage::prompt_done(editor, &p.input),
        PromptKind::SelectRegex | PromptKind::SplitRegex | PromptKind::Keep { .. } => {
            if let Some(f) = p.feedback {
                editor.set_status(Severity::Error, f);
                editor.view_mut().selection = p.original;
            }
        }
    }
}

/// Recomputes the live preview for regex prompts.
fn preview(editor: &mut Editor, p: &mut Prompt) {
    p.feedback = None;
    if matches!(
        p.kind,
        PromptKind::Command | PromptKind::Rename | PromptKind::ProjectAdd
    ) {
        return;
    }
    // The document may have changed since the prompt opened (an automatic
    // reload, a background edit), so bring the saved selection into range.
    let text = editor.doc().text().clone();
    p.original = p.original.clone().normalize(text.slice(..));
    editor.view_mut().selection = p.original.clone();
    if p.input.is_empty() {
        return;
    }
    if p.kind == PromptKind::GlobalSearch {
        // Only validate here; the search itself starts on Enter.
        if let Err(e) = crate::navigation::grep::compile(&p.input) {
            p.feedback = Some(e);
        }
        return;
    }
    let re = match crate::search::compile(&p.input) {
        Ok(re) => re,
        Err(e) => {
            p.feedback = Some(e);
            return;
        }
    };
    let slice = text.slice(..);
    match p.kind {
        PromptKind::Search { backward } => {
            let r = p.original.primary();
            let head = if backward { r.from() } else { r.last() };
            if let Some(((s, e), _)) = crate::search::find(&text, &re, head, backward) {
                editor.view_mut().selection = Selection::single(s, e.saturating_sub(1).max(s));
                editor.ensure_cursor_visible();
            } else {
                p.feedback = Some("no matches".into());
            }
        }
        PromptKind::SelectRegex | PromptKind::SplitRegex => {
            let split = p.kind == PromptKind::SplitRegex;
            let sel = p.original.transform_many(slice, |r| {
                let (from, to) = (r.from(), r.to(slice));
                let found = crate::search::matches_in(&text, &re, from, to);
                if !split {
                    return found
                        .into_iter()
                        .map(|(s, e)| Range::new(s, e - 1))
                        .collect();
                }
                let mut out = Vec::new();
                let mut start = from;
                for (s, e) in found {
                    if s > start {
                        out.push(Range::new(start, s - 1));
                    }
                    start = e;
                }
                if start < to {
                    out.push(Range::new(start, to - 1));
                }
                out
            });
            match sel {
                Some(sel) => editor.view_mut().selection = sel,
                None => p.feedback = Some("no matches".into()),
            }
        }
        PromptKind::Keep { remove } => {
            let sel = p.original.transform_many(slice, |r| {
                let hit = !crate::search::matches_in(&text, &re, r.from(), r.to(slice)).is_empty();
                if hit != remove { vec![r] } else { vec![] }
            });
            match sel {
                Some(sel) => editor.view_mut().selection = sel,
                None => p.feedback = Some("no selections would remain".into()),
            }
        }
        PromptKind::Command
        | PromptKind::GlobalSearch
        | PromptKind::Rename
        | PromptKind::ProjectAdd => {}
    }
}

fn complete(editor: &Editor, p: &mut Prompt, backward: bool) {
    // The project prompt is one path, spaces and all.
    let whole = p.kind == PromptKind::ProjectAdd;
    if p.completions.is_empty() {
        p.completions = if whole {
            path_completions(editor, &p.input, true)
        } else {
            completions_for(editor, &p.input)
        };
        p.completion_idx = None;
        if p.completions.is_empty() {
            return;
        }
    }
    let n = p.completions.len();
    let next = match (p.completion_idx, backward) {
        (None, false) => 0,
        (None, true) => n - 1,
        (Some(i), false) => (i + 1) % n,
        (Some(i), true) => (i + n - 1) % n,
    };
    p.completion_idx = Some(next);
    let (head, _) = if whole {
        ("", "")
    } else {
        split_last_word(&p.input)
    };
    p.input = format!("{head}{}", p.completions[next]);
    p.cursor = p.input.len();
}

fn split_last_word(input: &str) -> (&str, &str) {
    match input.rfind(' ') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    }
}

fn completions_for(editor: &Editor, input: &str) -> Vec<String> {
    let (head, word) = split_last_word(input);
    if head.is_empty() {
        let mut names: Vec<String> = crate::typed::all()
            .iter()
            .flat_map(|c| std::iter::once(c.name).chain(c.aliases.iter().copied()))
            .filter(|n| n.starts_with(word))
            .map(str::to_string)
            .collect();
        names.sort();
        names.dedup();
        return names;
    }
    // Path completion for every other argument.
    path_completions(editor, word, false)
}

/// Completions of the path `word`: the entries of the directory it names
/// that start with its last part, directories with a trailing `/`. Hidden
/// entries only when the last part starts with `.`.
fn path_completions(editor: &Editor, word: &str, dirs_only: bool) -> Vec<String> {
    let (dir_part, file_part) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let dir = editor.resolve_path(std::path::Path::new(if dir_part.is_empty() {
        "."
    } else {
        dir_part
    }));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(file_part)
                || (name.starts_with('.') && !file_part.starts_with('.'))
            {
                return None;
            }
            // For a project, a link to a directory completes as one.
            let is_dir = e
                .file_type()
                .is_ok_and(|t| t.is_dir() || (dirs_only && t.is_symlink() && e.path().is_dir()));
            if dirs_only && !is_dir {
                return None;
            }
            let slash = if is_dir { "/" } else { "" };
            Some(format!("{dir_part}{name}{slash}"))
        })
        .collect();
    out.sort();
    out
}
