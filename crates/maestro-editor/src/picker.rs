//! A filterable list overlay: command palette, buffer list, file finder.
//!
//! Items carry a [`PickerAction`] describing what accepting them does, so
//! pickers are plain data and can be filled from a background thread with
//! [`Picker::extend`].

use std::path::PathBuf;

use crate::editor::{DocumentId, Editor};
use crate::key::{KeyCode, KeyEvent};
use crate::layout::Direction;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerAction {
    OpenFile(PathBuf),
    Buffer(DocumentId),
    /// A registry or typed command name.
    Command(String),
    /// Make a directory the working directory (`:cd`).
    ChangeDir(PathBuf),
    /// Open a file at a zero-based line and char column.
    Goto {
        path: PathBuf,
        line: usize,
        column: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    pub label: String,
    /// Secondary text shown dimmed to the right (a key binding, a path).
    pub detail: String,
    pub action: PickerAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub index: usize,
    pub score: i64,
    /// Char positions in the label that matched, for highlighting.
    pub positions: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    items: Vec<PickerItem>,
    pub query: String,
    matches: Vec<Match>,
    pub selected: usize,
    pub scroll: usize,
    /// Set while a background producer is still adding items.
    pub loading: bool,
}

pub enum Outcome {
    Continue,
    Close,
    Accept(PickerAction, Option<Direction>),
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<PickerItem>) -> Self {
        let mut p = Self {
            title: title.into(),
            items,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            loading: false,
        };
        p.refilter();
        p
    }

    pub fn items(&self) -> &[PickerItem] {
        &self.items
    }

    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    pub fn total(&self) -> usize {
        self.items.len()
    }

    pub fn extend(&mut self, items: impl IntoIterator<Item = PickerItem>) {
        self.items.extend(items);
        self.refilter();
    }

    /// Appends pasted text to the query (line breaks dropped).
    pub fn push_query(&mut self, text: &str) {
        self.query
            .extend(text.chars().filter(|c| *c != '\n' && *c != '\r'));
        self.refilter();
    }

    pub fn selected_item(&self) -> Option<&PickerItem> {
        self.matches
            .get(self.selected)
            .map(|m| &self.items[m.index])
    }

    fn refilter(&mut self) {
        let prev = self.selected_item().cloned();
        self.matches = if self.query.is_empty() {
            (0..self.items.len())
                .map(|index| Match {
                    index,
                    score: 0,
                    positions: Vec::new(),
                })
                .collect()
        } else {
            let mut m: Vec<Match> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    fuzzy_match(&self.query, &item.label).map(|(score, positions)| Match {
                        index,
                        score,
                        positions,
                    })
                })
                .collect();
            m.sort_by(|a, b| {
                b.score.cmp(&a.score).then_with(|| {
                    self.items[a.index]
                        .label
                        .len()
                        .cmp(&self.items[b.index].label.len())
                })
            });
            m
        };
        // Keep the selection on the same item when new items stream in.
        self.selected = prev
            .and_then(|p| self.matches.iter().position(|m| self.items[m.index] == p))
            .unwrap_or(0)
            .min(self.matches.len().saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.mods.ctrl;
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('c') if ctrl => return Outcome::Close,
            KeyCode::Enter => {
                return match self.selected_item() {
                    Some(i) => Outcome::Accept(i.action.clone(), None),
                    None => Outcome::Close,
                };
            }
            KeyCode::Char('v') if ctrl => {
                return match self.selected_item() {
                    Some(i) => Outcome::Accept(i.action.clone(), Some(Direction::Vertical)),
                    None => Outcome::Continue,
                };
            }
            KeyCode::Char('s') if ctrl => {
                return match self.selected_item() {
                    Some(i) => Outcome::Accept(i.action.clone(), Some(Direction::Horizontal)),
                    None => Outcome::Continue,
                };
            }
            KeyCode::Up | KeyCode::BackTab => self.move_selection(-1),
            KeyCode::Char('p') | KeyCode::Char('k') if ctrl => self.move_selection(-1),
            KeyCode::Down | KeyCode::Tab => self.move_selection(1),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
            }
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.refilter();
            }
            _ => {
                if let Some(c) = key.typed_char() {
                    self.query.push(c);
                    self.refilter();
                }
            }
        }
        Outcome::Continue
    }
}

/// Applies a picker outcome to the editor.
pub fn resolve(editor: &mut Editor, outcome: Outcome) {
    match outcome {
        Outcome::Continue => {}
        Outcome::Close => editor.picker = None,
        Outcome::Accept(action, split) => {
            editor.picker = None;
            if let Some(dir) = split {
                editor.split(dir);
            }
            match action {
                PickerAction::OpenFile(path) => {
                    if let Err(e) = editor.open(&path) {
                        editor.error(e);
                    }
                }
                PickerAction::Buffer(id) => {
                    if editor.buffers.contains_key(&id) {
                        editor.switch_to(id);
                    }
                }
                PickerAction::Command(name) => editor.run_command(&name),
                PickerAction::ChangeDir(path) => match std::fs::canonicalize(&path) {
                    Ok(dir) if dir.is_dir() => {
                        let msg = format!("cwd: {}", dir.display());
                        editor.cwd = dir;
                        editor.info(msg);
                    }
                    Ok(dir) => editor.error(format!("{} is not a directory", dir.display())),
                    Err(e) => editor.error(format!("{}: {e}", path.display())),
                },
                PickerAction::Goto { path, line, column } => {
                    if let Err(e) = editor.open(&path) {
                        editor.error(e);
                        return;
                    }
                    let text = editor.doc().text().clone();
                    let line = line.min(text.len_lines().saturating_sub(1));
                    let pos = (text.line_to_char(line) + column).min(text.len_chars());
                    editor.push_jump();
                    editor.set_selection(maestro_core::Selection::point(pos));
                    editor.commit();
                    crate::commands::align_view_center(editor);
                }
            }
        }
    }
}

/// Subsequence fuzzy match with smart case. Rewards consecutive runs and
/// matches at word starts or after path separators. Returns the score and
/// the matched char positions, or `None` when `query` is not a subsequence.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    let case_sensitive = query.chars().any(char::is_uppercase);
    let norm = |c: char| {
        if case_sensitive {
            c
        } else {
            c.to_ascii_lowercase()
        }
    };
    let cand: Vec<char> = candidate.chars().collect();
    let q: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(norm)
        .collect();
    if q.is_empty() {
        return Some((0, Vec::new()));
    }
    // Prefer matching inside the final path component when possible.
    let base_start = cand
        .iter()
        .rposition(|c| *c == '/' || *c == '\\')
        .map(|i| i + 1)
        .unwrap_or(0);
    let attempt = |start: usize| -> Option<(i64, Vec<usize>)> {
        let mut positions = Vec::with_capacity(q.len());
        let mut qi = 0;
        let mut score: i64 = 0;
        let mut prev: Option<usize> = None;
        for (i, &c) in cand.iter().enumerate().skip(start) {
            if qi < q.len() && norm(c) == q[qi] {
                score += 16;
                if prev.is_some_and(|p| p + 1 == i) {
                    score += 12;
                }
                let at_boundary = i == 0
                    || matches!(cand[i - 1], '/' | '\\' | '_' | '-' | '.' | ' ' | ':')
                    || (cand[i - 1].is_lowercase() && c.is_uppercase());
                if at_boundary {
                    score += 10;
                }
                if i >= base_start {
                    score += 4;
                }
                if let Some(p) = prev {
                    score -= (i - p - 1).min(8) as i64;
                }
                positions.push(i);
                prev = Some(i);
                qi += 1;
            }
        }
        (qi == q.len()).then_some((score, positions))
    };
    let whole = attempt(0)?;
    let base = if base_start > 0 {
        attempt(base_start)
    } else {
        None
    };
    Some(match base {
        Some(b) if b.0 >= whole.0 => b,
        _ => whole,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_tight_and_basename_matches() {
        assert!(fuzzy_match("xyz", "abc").is_none());
        let (a, _) = fuzzy_match("edit", "crates/maestro-editor/src/lib.rs").unwrap();
        let (b, _) = fuzzy_match("edit", "src/editor.rs").unwrap();
        assert!(b > a);
        let (_, pos) = fuzzy_match("mr", "main.rs").unwrap();
        assert_eq!(pos, vec![0, 5]);
        // Smart case: an uppercase letter makes the match case-sensitive.
        assert!(fuzzy_match("R", "main.rs").is_none());
    }

    #[test]
    fn picker_filters_and_accepts() {
        let items = ["write", "quit", "write-quit"]
            .iter()
            .map(|s| PickerItem {
                label: s.to_string(),
                detail: String::new(),
                action: PickerAction::Command(s.to_string()),
            })
            .collect();
        let mut p = Picker::new("t", items);
        assert_eq!(p.matches().len(), 3);
        for c in "wq".chars() {
            p.handle_key(KeyEvent::char(c));
        }
        assert_eq!(p.matches().len(), 1);
        match p.handle_key(KeyEvent::plain(KeyCode::Enter)) {
            Outcome::Accept(PickerAction::Command(c), None) => assert_eq!(c, "write-quit"),
            _ => panic!("expected accept"),
        }
    }
}
