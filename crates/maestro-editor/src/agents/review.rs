//! The review screen: what an agent's worktree changed since its base,
//! file by file, in a unified or side-by-side layout with intraline marks
//! and line comments. Diffs are computed on background threads.

use std::ops::Range;
use std::path::PathBuf;
use std::time::Instant;

use maestro_git::{DiffFile, DiffLineKind, FileChange, intraline};

use super::panel::{InputPurpose, Overlay, layout};
use super::{AgentId, Comment};
use crate::editor::Editor;
use crate::key::{KeyCode, KeyEvent};

/// The review screen's place in an agent's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewView {
    pub agent: AgentId,
    /// Index into the agent's review files.
    pub file: usize,
    /// Row of [`rows`] under the cursor.
    pub cursor: usize,
    /// First row shown.
    pub top: usize,
    pub side_by_side: bool,
}

/// One line of a diff as the review screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLine {
    pub kind: DiffLineKind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub text: String,
    /// Changed byte ranges of `text` (intraline marks).
    pub marks: Vec<Range<usize>>,
    /// The new-file line this row sits at, for opening the file there.
    pub anchor: usize,
}

impl RowLine {
    /// The line a comment on this row attaches to: `(line, old side)`.
    fn comment_target(&self) -> (usize, bool) {
        match (self.new, self.old) {
            (Some(n), _) => (n, false),
            (None, Some(o)) => (o, true),
            (None, None) => (self.anchor, false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Hunk {
        index: usize,
        header: String,
    },
    /// A unified-layout line.
    Line(RowLine),
    /// A side-by-side line: old on the left, new on the right.
    Pair(Option<RowLine>, Option<RowLine>),
    /// A review comment under the line it belongs to.
    Comment {
        text: String,
        sent: bool,
    },
    Note(String),
}

impl Row {
    /// Where `Enter` opens the file: a one-based new-file line.
    pub fn target_line(&self) -> Option<usize> {
        match self {
            Row::Line(l) => Some(l.anchor),
            Row::Pair(l, r) => r.as_ref().or(l.as_ref()).map(|x| x.anchor),
            _ => None,
        }
    }

    fn comment_target(&self) -> Option<(usize, bool)> {
        match self {
            Row::Line(l) => Some(l.comment_target()),
            Row::Pair(l, r) => r.as_ref().or(l.as_ref()).map(RowLine::comment_target),
            _ => None,
        }
    }

    fn matches(&self, c: &Comment) -> bool {
        let hit = |l: &RowLine| {
            if c.old {
                l.new.is_none() && l.old == Some(c.line)
            } else {
                l.new == Some(c.line)
            }
        };
        match self {
            Row::Line(l) => hit(l),
            Row::Pair(l, r) => l.as_ref().is_some_and(hit) || r.as_ref().is_some_and(hit),
            _ => false,
        }
    }
}

/// `added`, `deleted`, `modified`, `renamed 90%`...
pub fn change_label(f: &DiffFile) -> String {
    let base = match &f.change {
        FileChange::Added => "added".to_string(),
        FileChange::Deleted => "deleted".to_string(),
        FileChange::Modified => "modified".to_string(),
        FileChange::Renamed { similarity } => format!("renamed {similarity}%"),
        FileChange::Copied { similarity } => format!("copied {similarity}%"),
    };
    if f.binary {
        format!("{base}, binary")
    } else if f.unmerged {
        format!("{base}, in conflict")
    } else {
        base
    }
}

/// The rows the review screen shows for `file`, with `comments` (on that
/// file) under their lines.
pub fn rows(file: &DiffFile, side_by_side: bool, comments: &[Comment]) -> Vec<Row> {
    let mut out = Vec::new();
    if let (Some(old), Some(new)) = (&file.old_path, &file.new_path)
        && old != new
    {
        out.push(Row::Note(format!(
            "{} from {}",
            change_label(file),
            old.display()
        )));
    }
    if file.binary {
        out.push(Row::Note("binary file; no text diff".into()));
    }
    if file.unmerged {
        out.push(Row::Note("in conflict; resolve it in the worktree".into()));
    }
    if file.hunks.is_empty() && !file.binary && !file.unmerged {
        out.push(Row::Note("no line changes (mode or empty file)".into()));
    }
    for (index, h) in file.hunks.iter().enumerate() {
        out.push(Row::Hunk {
            index,
            header: format!(
                "@@ -{},{} +{},{} @@ {}",
                h.old_start, h.old_len, h.new_start, h.new_len, h.section
            )
            .trim_end()
            .to_string(),
        });
        let lines = &h.lines;
        let mut next_new = h.new_start.max(1);
        let mut i = 0;
        let mk = |l: &maestro_git::DiffLine, marks, anchor| RowLine {
            kind: l.kind,
            old: l.old_line,
            new: l.new_line,
            text: l.text.clone(),
            marks,
            anchor,
        };
        while i < lines.len() {
            match lines[i].kind {
                DiffLineKind::Context => {
                    let l = &lines[i];
                    let anchor = l.new_line.unwrap_or(next_new);
                    next_new = anchor + 1;
                    if side_by_side {
                        out.push(Row::Pair(
                            Some(mk(l, Vec::new(), anchor)),
                            Some(mk(l, Vec::new(), anchor)),
                        ));
                    } else {
                        out.push(Row::Line(mk(l, Vec::new(), anchor)));
                    }
                    i += 1;
                }
                _ => {
                    let rs = i;
                    while i < lines.len() && lines[i].kind == DiffLineKind::Removed {
                        i += 1;
                    }
                    let added_start = i;
                    while i < lines.len() && lines[i].kind == DiffLineKind::Added {
                        i += 1;
                    }
                    let removed = &lines[rs..added_start];
                    let added = &lines[added_start..i];
                    let mut old_marks = vec![Vec::new(); removed.len()];
                    let mut new_marks = vec![Vec::new(); added.len()];
                    for k in 0..removed.len().min(added.len()) {
                        let (o, n) = intraline(&removed[k].text, &added[k].text);
                        old_marks[k] = o;
                        new_marks[k] = n;
                    }
                    let removed_anchor = next_new;
                    let left: Vec<RowLine> = removed
                        .iter()
                        .zip(old_marks)
                        .map(|(l, m)| mk(l, m, removed_anchor))
                        .collect();
                    let right: Vec<RowLine> = added
                        .iter()
                        .zip(new_marks)
                        .map(|(l, m)| {
                            let anchor = l.new_line.unwrap_or(next_new);
                            mk(l, m, anchor)
                        })
                        .collect();
                    if let Some(last) = right.last() {
                        next_new = last.anchor + 1;
                    }
                    if side_by_side {
                        let n = left.len().max(right.len());
                        let (mut l, mut r) = (left.into_iter(), right.into_iter());
                        for _ in 0..n {
                            out.push(Row::Pair(l.next(), r.next()));
                        }
                    } else {
                        out.extend(left.into_iter().map(Row::Line));
                        out.extend(right.into_iter().map(Row::Line));
                    }
                    if i == rs {
                        // Defensive: never loop on an unexpected kind.
                        i += 1;
                    }
                }
            }
        }
    }
    if comments.is_empty() {
        return out;
    }
    let mut with = Vec::with_capacity(out.len() + comments.len());
    for row in out {
        let attached: Vec<Row> = comments
            .iter()
            .filter(|c| row.matches(c))
            .map(|c| Row::Comment {
                text: c.text.clone(),
                sent: c.sent,
            })
            .collect();
        with.push(row);
        with.extend(attached);
    }
    with
}

/// The comments on `file`.
pub fn file_comments(comments: &[Comment], file: &DiffFile) -> Vec<Comment> {
    let Some(path) = file.path() else {
        return Vec::new();
    };
    comments
        .iter()
        .filter(|c| &c.path == path)
        .cloned()
        .collect::<Vec<_>>()
}

/// Starts a review diff for agent `id` on a background thread, or marks
/// one to run again if one is running.
pub fn request(ed: &mut Editor, id: AgentId) {
    let git = ed.git.git().clone();
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let Some(w) = a.worktree.clone() else {
        return;
    };
    if a.archived {
        return;
    }
    if a.review.running {
        a.review.again = true;
        return;
    }
    a.review.running = true;
    a.review.again = false;
    let spawned = ed.spawn_background("agent-review", move |tx| {
        // Discovery climbs to a parent repository when the worktree
        // directory is gone; only the worktree itself counts.
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.into());
        let result = match git.discover(&w.path) {
            Ok(Some(repo)) if canon(repo.root()) == canon(&w.path) => {
                repo.review_diff(&w.base).map_err(|e| e.to_string())
            }
            Ok(_) => Err(format!("{} is no longer a worktree", w.path.display())),
            Err(e) => Err(e.to_string()),
        };
        tx.send(move |ed| applied(ed, id, result));
    });
    if spawned.is_err()
        && let Some(a) = ed.agents.get_mut(id)
    {
        a.review.running = false;
    }
}

fn applied(ed: &mut Editor, id: AgentId, result: Result<Vec<DiffFile>, String>) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.review.running = false;
    match result {
        Ok(files) => {
            a.review.files = files;
            a.review.error = None;
            a.review.computed = Some(Instant::now());
        }
        Err(e) => a.review.error = Some(e),
    }
    let again = a.review.again && !a.archived;
    if again {
        request(ed, id);
    }
    let n = ed.agents.get(id).map(|a| a.review.files.len()).unwrap_or(0);
    if let Some(v) = &mut ed.agents.panel.review
        && v.agent == id
    {
        v.file = v.file.min(n.saturating_sub(1));
    }
}

/// Opens the review screen for agent `id` and refreshes its diff.
pub fn open(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if a.worktree.is_none() {
        ed.error("the agent has no worktree yet");
        return;
    }
    if a.archived {
        ed.error("the agent's worktree was archived; there is nothing to review");
        return;
    }
    ed.agents.panel.review = Some(ReviewView {
        agent: id,
        file: 0,
        cursor: 0,
        top: 0,
        side_by_side: false,
    });
    request(ed, id);
}

/// The rows for the review screen's current file.
pub fn current_rows(ed: &Editor) -> Vec<Row> {
    let Some(v) = &ed.agents.panel.review else {
        return Vec::new();
    };
    let Some(a) = ed.agents.get(v.agent) else {
        return Vec::new();
    };
    let Some(f) = a.review.files.get(v.file) else {
        return Vec::new();
    };
    rows(f, v.side_by_side, &file_comments(&a.comments, f))
}

pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    let Some(mut v) = ed.agents.panel.review.clone() else {
        return;
    };
    let id = v.agent;
    let nfiles = ed.agents.get(id).map(|a| a.review.files.len()).unwrap_or(0);
    let rows = current_rows(ed);
    let height = layout(ed.area).content.height.max(1) as usize;
    let last = rows.len().saturating_sub(1);
    let ctrl = key.mods.ctrl;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if !ctrl => {
            ed.agents.panel.review = None;
            return;
        }
        KeyCode::Char('j') | KeyCode::Down if !ctrl => v.cursor = (v.cursor + 1).min(last),
        KeyCode::Char('k') | KeyCode::Up if !ctrl => v.cursor = v.cursor.saturating_sub(1),
        KeyCode::PageDown => v.cursor = (v.cursor + height / 2).min(last),
        KeyCode::PageUp => v.cursor = v.cursor.saturating_sub(height / 2),
        KeyCode::Char('g') if !ctrl => v.cursor = 0,
        KeyCode::Char('G') if !ctrl => v.cursor = last,
        KeyCode::Tab | KeyCode::Char('}') if nfiles > 0 => {
            v.file = (v.file + 1) % nfiles;
            v.cursor = 0;
            v.top = 0;
        }
        KeyCode::BackTab | KeyCode::Char('{') if nfiles > 0 => {
            v.file = (v.file + nfiles - 1) % nfiles;
            v.cursor = 0;
            v.top = 0;
        }
        KeyCode::Char(']') if !ctrl => {
            if let Some(i) = rows
                .iter()
                .enumerate()
                .skip(v.cursor + 1)
                .find(|(_, r)| matches!(r, Row::Hunk { .. }))
                .map(|(i, _)| i)
            {
                v.cursor = i;
            }
        }
        KeyCode::Char('[') if !ctrl => {
            if let Some(i) = rows
                .iter()
                .enumerate()
                .take(v.cursor)
                .rev()
                .find(|(_, r)| matches!(r, Row::Hunk { .. }))
                .map(|(i, _)| i)
            {
                v.cursor = i;
            }
        }
        KeyCode::Char('s') if !ctrl => {
            // Keep the cursor on the same file line across layouts.
            let target = rows.get(v.cursor).and_then(Row::target_line);
            v.side_by_side = !v.side_by_side;
            ed.agents.panel.review = Some(v.clone());
            let new_rows = current_rows(ed);
            v.cursor = target
                .and_then(|t| new_rows.iter().position(|r| r.target_line() == Some(t)))
                .unwrap_or(0);
        }
        KeyCode::Char('r') if !ctrl => request(ed, id),
        KeyCode::Char('c') if !ctrl => {
            let file = ed
                .agents
                .get(id)
                .and_then(|a| a.review.files.get(v.file))
                .and_then(|f| f.path().cloned());
            match (file, rows.get(v.cursor).and_then(Row::comment_target)) {
                (Some(path), Some((line, old))) => {
                    let existing = ed
                        .agents
                        .get(id)
                        .and_then(|a| {
                            a.comments.iter().find(|c| {
                                c.path == path && c.line == line && c.old == old && !c.sent
                            })
                        })
                        .map(|c| c.text.clone())
                        .unwrap_or_default();
                    let side = if old { "old " } else { "" };
                    ed.agents.panel.overlay = Some(Overlay::Input {
                        purpose: InputPurpose::Comment {
                            agent: id,
                            path: path.clone(),
                            line,
                            old,
                        },
                        label: format!("comment on {}:{side}{line}", path.display()),
                        text: existing,
                    });
                }
                _ => ed.error("put the cursor on a diff line to comment on it"),
            }
        }
        KeyCode::Enter => {
            open_at_cursor(ed, &v, &rows);
            return;
        }
        KeyCode::Char('D') if !ctrl => {
            let file = ed
                .agents
                .get(id)
                .and_then(|a| a.review.files.get(v.file))
                .cloned();
            match file {
                Some(f) => super::actions::ask_drop_file(ed, id, &f),
                None => ed.error("no file to drop"),
            }
        }
        KeyCode::Char('t') if !ctrl => super::actions::run_tests(ed, id),
        KeyCode::Char('f') if !ctrl => super::actions::ask_feedback(ed, id),
        KeyCode::Char('I') if !ctrl => super::actions::prepare_integrate(ed, id),
        KeyCode::Char('X') if !ctrl => super::actions::confirm_reject(ed, id),
        KeyCode::Char(':') if !ctrl => crate::prompt::open(ed, crate::prompt::PromptKind::Command),
        _ => {}
    }
    // Keep the cursor on screen.
    if v.cursor < v.top {
        v.top = v.cursor;
    } else if v.cursor >= v.top + height {
        v.top = v.cursor + 1 - height;
    }
    if ed.agents.panel.review.is_some() {
        ed.agents.panel.review = Some(v);
    }
}

/// `Enter`: opens the worktree's file in a normal buffer at the line under
/// the cursor and hides the panel.
fn open_at_cursor(ed: &mut Editor, v: &ReviewView, rows: &[Row]) {
    let Some(a) = ed.agents.get(v.agent) else {
        return;
    };
    let Some(w) = &a.worktree else {
        return;
    };
    let Some(f) = a.review.files.get(v.file) else {
        ed.error("no file to open");
        return;
    };
    let Some(rel) = f.new_path.clone() else {
        ed.error("the agent deleted this file; there is nothing to open");
        return;
    };
    let path: PathBuf = w.path.join(rel);
    // The agent controls this file. A FIFO or device (directly or through
    // a symlink) could block the editor thread or never end, so only a
    // regular file is opened. A swap in the instant before the open is
    // not caught.
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            ed.error(format!(
                "{} is not a regular file; not opened",
                path.display()
            ));
            return;
        }
        Err(e) => {
            ed.error(format!("{}: {e}", path.display()));
            return;
        }
    }
    let line = rows
        .get(v.cursor)
        .and_then(Row::target_line)
        .unwrap_or(1)
        .max(1);
    super::panel::close(ed);
    if let Err(e) = ed.open(&path) {
        ed.error(e);
        return;
    }
    let text = ed.doc().text().clone();
    let line = (line - 1).min(text.len_lines().saturating_sub(1));
    let pos = text.line_to_char(line);
    ed.push_jump();
    ed.set_selection(maestro_core::Selection::point(pos));
    ed.commit();
    crate::commands::align_view_center(ed);
}

pub fn hints() -> Vec<(&'static str, &'static str)> {
    vec![
        ("j/k", "line"),
        ("]/[", "hunk"),
        ("tab", "file"),
        ("s", "side by side"),
        ("c", "comment"),
        ("D", "drop file"),
        ("ret", "open file"),
        ("f", "feedback"),
        ("t", "test"),
        ("I", "integrate"),
        ("X", "reject"),
        ("r", "refresh"),
        ("esc", "back"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_git::{DiffLine, Hunk};

    fn line(kind: DiffLineKind, old: Option<usize>, new: Option<usize>, text: &str) -> DiffLine {
        DiffLine {
            kind,
            old_line: old,
            new_line: new,
            text: text.into(),
            no_newline: false,
        }
    }

    fn file() -> DiffFile {
        use DiffLineKind::*;
        DiffFile {
            old_path: Some("a.rs".into()),
            new_path: Some("a.rs".into()),
            change: FileChange::Modified,
            old_mode: None,
            new_mode: None,
            binary: false,
            unmerged: false,
            hunks: vec![Hunk {
                old_start: 1,
                old_len: 3,
                new_start: 1,
                new_len: 4,
                section: String::new(),
                lines: vec![
                    line(Context, Some(1), Some(1), "fn a() {"),
                    line(Removed, Some(2), None, "    let x = 1;"),
                    line(Added, None, Some(2), "    let x = 2;"),
                    line(Added, None, Some(3), "    let y = 3;"),
                    line(Context, Some(3), Some(4), "}"),
                ],
            }],
        }
    }

    #[test]
    fn unified_rows_pair_intraline_marks() {
        let rows = rows(&file(), false, &[]);
        assert!(matches!(rows[0], Row::Hunk { .. }));
        let Row::Line(removed) = &rows[2] else {
            panic!("{rows:?}")
        };
        let Row::Line(added) = &rows[3] else { panic!() };
        assert_eq!(removed.kind, DiffLineKind::Removed);
        assert_eq!(&removed.text[removed.marks[0].clone()], "1");
        assert_eq!(&added.text[added.marks[0].clone()], "2");
        // The unpaired added line has no marks.
        let Row::Line(extra) = &rows[4] else { panic!() };
        assert!(extra.marks.is_empty());
        // A removed line opens the file where its replacement starts.
        assert_eq!(rows[2].target_line(), Some(2));
        assert_eq!(rows[5].target_line(), Some(4));
    }

    #[test]
    fn side_by_side_pairs_runs_and_shows_comments() {
        let c = Comment {
            path: "a.rs".into(),
            line: 3,
            old: false,
            text: "why y?".into(),
            sent: false,
        };
        let rows = rows(&file(), true, &[c]);
        // hunk, context, (removed|added), (none|added), comment, context
        assert_eq!(rows.len(), 6, "{rows:?}");
        let Row::Pair(Some(l), Some(r)) = &rows[2] else {
            panic!("{rows:?}")
        };
        assert_eq!(l.old, Some(2));
        assert_eq!(r.new, Some(2));
        assert!(matches!(&rows[3], Row::Pair(None, Some(_))));
        assert!(matches!(&rows[4], Row::Comment { text, .. } if text == "why y?"));
    }
}
