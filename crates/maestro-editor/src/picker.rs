//! A filterable list overlay: command palette, buffer list, file finder.
//!
//! Items carry a [`PickerAction`] describing what accepting them does, so
//! pickers are plain data and can be filled from a background thread with
//! [`Picker::extend`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::editor::{DocumentId, Editor};
use crate::key::{KeyCode, KeyEvent};
use crate::layout::Direction;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerAction {
    OpenFile(PathBuf),
    Buffer(DocumentId),
    /// A registry or typed command name.
    Command(String),
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
    /// Unique per picker. Background producers remember it and drop their
    /// results once a different picker (or none) is open.
    pub id: u64,
    /// File preview for the selected item, kept current by
    /// [`crate::navigation::preview::refresh`].
    pub preview: Option<crate::navigation::preview::Preview>,
}

static NEXT_PICKER_ID: AtomicU64 = AtomicU64::new(1);

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
            id: NEXT_PICKER_ID.fetch_add(1, Ordering::Relaxed),
            preview: None,
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

    /// Appends items. Only the new items are matched against the query;
    /// their matches are sorted and merged into the sorted list, so a batch
    /// costs its own matching plus one linear merge.
    pub fn extend(&mut self, items: impl IntoIterator<Item = PickerItem>) {
        let start = self.items.len();
        self.items.extend(items);
        if start == self.items.len() {
            return;
        }
        let prev = self.selected_index();
        if self.query.is_empty() {
            self.matches
                .extend((start..self.items.len()).map(|index| Match {
                    index,
                    score: 0,
                    positions: Vec::new(),
                }));
        } else {
            let mut new = self.match_range(start..self.items.len());
            let items = &self.items;
            let key = |m: &Match| {
                (
                    std::cmp::Reverse(m.score),
                    items[m.index].label.len(),
                    m.index,
                )
            };
            new.sort_by_cached_key(key);
            let old = std::mem::take(&mut self.matches);
            let mut merged = Vec::with_capacity(old.len() + new.len());
            let (mut a, mut b) = (old.into_iter().peekable(), new.into_iter().peekable());
            loop {
                let take_old = match (a.peek(), b.peek()) {
                    (Some(x), Some(y)) => key(x) <= key(y),
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                merged.extend(if take_old { a.next() } else { b.next() });
            }
            self.matches = merged;
        }
        self.restore_selection(prev);
    }

    /// Index into [`Picker::items`] of the selected match.
    fn selected_index(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|m| m.index)
    }

    fn match_range(&self, range: std::ops::Range<usize>) -> Vec<Match> {
        let indices: Vec<usize> = range.collect();
        match_items(&self.query, &self.items, &indices)
    }

    /// Refilters after characters were appended to the query. A candidate
    /// that matches the longer query also matched the shorter one, so only
    /// the current matches need checking.
    fn narrow(&mut self) {
        let prev = self.selected_index();
        let indices: Vec<usize> = self.matches.iter().map(|m| m.index).collect();
        self.matches = match_items(&self.query, &self.items, &indices);
        self.sort_matches();
        self.restore_selection(prev);
    }

    fn sort_matches(&mut self) {
        let items = &self.items;
        self.matches.sort_by_cached_key(|m| {
            (
                std::cmp::Reverse(m.score),
                items[m.index].label.len(),
                m.index,
            )
        });
    }

    /// Keeps the selection on the same item when matches change.
    fn restore_selection(&mut self, prev: Option<usize>) {
        self.selected = prev
            .and_then(|p| self.matches.iter().position(|m| m.index == p))
            .unwrap_or(0)
            .min(self.matches.len().saturating_sub(1));
        self.fill_positions();
    }

    /// Computes highlight positions for the matches a list can show: the
    /// top of the list and the rows around the selection. Matching skips
    /// them for speed; a match with an empty `positions` and a non-empty
    /// query simply has none computed yet.
    fn fill_positions(&mut self) {
        const WINDOW: usize = 128;
        if self.query.is_empty() {
            return;
        }
        let matcher = Matcher::new(&self.query);
        let mut buf = Vec::new();
        let n = self.matches.len();
        let around = self.selected.saturating_sub(WINDOW)..(self.selected + WINDOW).min(n);
        for i in (0..WINDOW.min(n)).chain(around) {
            let m = &mut self.matches[i];
            if m.positions.is_empty()
                && let Some((_, p)) = matcher.score(&self.items[m.index].label, &mut buf, true)
            {
                m.positions = p;
            }
        }
    }

    pub fn selected_item(&self) -> Option<&PickerItem> {
        self.matches
            .get(self.selected)
            .map(|m| &self.items[m.index])
    }

    fn refilter(&mut self) {
        let prev = self.selected_index();
        self.matches = if self.query.is_empty() {
            (0..self.items.len())
                .map(|index| Match {
                    index,
                    score: 0,
                    positions: Vec::new(),
                })
                .collect()
        } else {
            self.match_range(0..self.items.len())
        };
        if !self.query.is_empty() {
            self.sort_matches();
        }
        self.restore_selection(prev);
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).rem_euclid(n as isize) as usize;
        self.fill_positions();
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
                    let was_empty = self.query.is_empty();
                    self.query.push(c);
                    if was_empty {
                        self.refilter();
                    } else {
                        self.narrow();
                    }
                }
            }
        }
        Outcome::Continue
    }
}

/// Applies a picker outcome to the editor.
pub fn resolve(editor: &mut Editor, outcome: Outcome) {
    match outcome {
        Outcome::Continue => crate::navigation::preview::refresh(editor),
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
                PickerAction::Goto { path, line, column } => {
                    if let Err(e) = editor.open(&path) {
                        editor.error(e);
                        return;
                    }
                    let text = editor.doc().text().clone();
                    let line = line.min(text.len_lines().saturating_sub(1));
                    // The location may come from the file on disk while the
                    // buffer has unsaved edits; never spill onto the next line.
                    let line_end =
                        maestro_core::text::line_end_excluding_terminator(text.slice(..), line);
                    let pos = (text.line_to_char(line) + column).min(line_end);
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
    Matcher::new(query).score(candidate, &mut Vec::new(), true)
}

/// A query prepared once for matching many candidates.
struct Matcher {
    q: Vec<char>,
    case_sensitive: bool,
}

impl Matcher {
    fn new(query: &str) -> Self {
        let case_sensitive = query.chars().any(char::is_uppercase);
        let q = query
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| {
                if case_sensitive {
                    c
                } else {
                    c.to_ascii_lowercase()
                }
            })
            .collect();
        Self { q, case_sensitive }
    }

    fn norm(&self, c: char) -> char {
        if self.case_sensitive {
            c
        } else {
            c.to_ascii_lowercase()
        }
    }

    /// Scores one candidate. Matched positions are returned only when
    /// `positions` is set; computing them for every match of a broad query
    /// over a large list is most of the cost. `buf` is scratch space reused
    /// between calls.
    fn score(
        &self,
        candidate: &str,
        buf: &mut Vec<char>,
        positions: bool,
    ) -> Option<(i64, Vec<usize>)> {
        if self.q.is_empty() {
            return Some((0, Vec::new()));
        }
        // ASCII labels (most paths) are scored on bytes without copying.
        if candidate.is_ascii() {
            return self.score_chars(candidate.as_bytes(), positions);
        }
        buf.clear();
        buf.extend(candidate.chars());
        self.score_chars(&buf[..], positions)
    }

    fn score_chars<C: Copy + Into<char>>(
        &self,
        cand: &[C],
        want_positions: bool,
    ) -> Option<(i64, Vec<usize>)> {
        let q = &self.q;
        let at = |i: usize| -> char { cand[i].into() };
        // Cheap rejection before any allocation.
        let mut qi = 0;
        for &c in cand {
            if qi < q.len() && self.norm(c.into()) == q[qi] {
                qi += 1;
            }
        }
        if qi < q.len() {
            return None;
        }
        // Prefer matching inside the final path component when possible.
        let base_start = (0..cand.len())
            .rposition(|i| matches!(at(i), '/' | '\\'))
            .map(|i| i + 1)
            .unwrap_or(0);
        let attempt = |start: usize, record: bool| -> Option<(i64, Vec<usize>)> {
            let mut positions = Vec::with_capacity(if record { q.len() } else { 0 });
            let mut matched = 0;
            let mut score: i64 = 0;
            let mut prev: Option<usize> = None;
            for i in start..cand.len() {
                let c = at(i);
                if self.norm(c) != q[matched] {
                    continue;
                }
                score += 16;
                if prev.is_some_and(|p| p + 1 == i) {
                    score += 12;
                }
                let at_boundary = i == 0
                    || matches!(at(i - 1), '/' | '\\' | '_' | '-' | '.' | ' ' | ':')
                    || (at(i - 1).is_lowercase() && c.is_uppercase());
                if at_boundary {
                    score += 10;
                }
                if i >= base_start {
                    score += 4;
                }
                if let Some(p) = prev {
                    score -= (i - p - 1).min(8) as i64;
                }
                if record {
                    positions.push(i);
                }
                matched += 1;
                prev = Some(i);
                if matched == q.len() {
                    return Some((score, positions));
                }
            }
            None
        };
        let whole = attempt(0, false)?;
        let base = if base_start > 0 {
            attempt(base_start, false)
        } else {
            None
        };
        let (start, score) = match base {
            Some(b) if b.0 >= whole.0 => (base_start, b.0),
            _ => (0, whole.0),
        };
        if want_positions {
            return attempt(start, true);
        }
        Some((score, Vec::new()))
    }
}

/// Matches the items at `indices` against `query`. Large lists are split
/// across threads so filtering a few hundred thousand files stays
/// interactive.
fn match_items(query: &str, items: &[PickerItem], indices: &[usize]) -> Vec<Match> {
    const PARALLEL_MIN: usize = 16_384;
    let matcher = Matcher::new(query);
    let run = |part: &[usize]| -> Vec<Match> {
        let mut buf = Vec::new();
        part.iter()
            .filter_map(|&index| {
                matcher
                    .score(&items[index].label, &mut buf, false)
                    .map(|(score, positions)| Match {
                        index,
                        score,
                        positions,
                    })
            })
            .collect()
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(16);
    if indices.len() < PARALLEL_MIN || threads < 2 {
        return run(indices);
    }
    let chunk = indices.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = indices
            .chunks(chunk)
            .map(|part| {
                let run = &run;
                s.spawn(move || run(part))
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
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

    fn items(labels: &[&str]) -> Vec<PickerItem> {
        labels
            .iter()
            .map(|s| PickerItem {
                label: s.to_string(),
                detail: String::new(),
                action: PickerAction::Command(s.to_string()),
            })
            .collect()
    }

    #[test]
    fn narrowing_and_streaming_match_a_full_refilter() {
        let labels: Vec<String> = (0..40_000)
            .map(|i| format!("dir{}/Sub_{}/file-{i}.rs", i % 7, i % 13))
            .chain(["src/ÉDIT.rs".to_string(), "x/edit_é.rs".to_string()])
            .collect();
        let all: Vec<&str> = labels.iter().map(String::as_str).collect();
        for query in ["f1", "sub3", "S", "file-39", "édit", "d i r"] {
            // Typed one key at a time, with items streaming in halfway.
            let mut typed = Picker::new("t", items(&all[..20_000]));
            for (n, c) in query.chars().enumerate() {
                typed.handle_key(KeyEvent::char(c));
                if n == 0 {
                    typed.extend(items(&all[20_000..]));
                }
            }
            let mut full = Picker::new("t", items(&all));
            full.query = query.to_string();
            full.refilter();
            let key = |p: &Picker| -> Vec<(usize, i64)> {
                p.matches().iter().map(|m| (m.index, m.score)).collect()
            };
            assert_eq!(key(&typed), key(&full), "query {query:?}");
            // Highlights are computed for the rows a list can show.
            for m in typed.matches().iter().take(5) {
                let want = fuzzy_match(query, &typed.items()[m.index].label).map(|x| x.1);
                assert_eq!(Some(m.positions.clone()), want, "query {query:?}");
            }
        }
    }
}
