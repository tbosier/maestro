//! The review screen: what an agent's worktree changed since its base,
//! file by file, in a unified or side-by-side layout with intraline marks
//! and line comments. Diffs are computed on background threads.

use std::ops::Range;
use std::path::PathBuf;
use std::time::Instant;

use maestro_git::{DiffFile, DiffLineKind, FileChange, intraline};

use super::panel::{InputPurpose, Overlay, agent_layout};
use super::{AgentId, Comment, Status};
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
    /// Where a line range started (`v`), while one is being chosen.
    pub anchor: Option<usize>,
}

impl ReviewView {
    /// The review of `agent`'s first file, at the top.
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            file: 0,
            cursor: 0,
            top: 0,
            side_by_side: false,
            anchor: None,
        }
    }
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
    /// The hunk it belongs to, and its index in the hunk's lines.
    pub hunk: usize,
    pub line: usize,
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
        let mk = |at: usize, marks, anchor| {
            let l: &maestro_git::DiffLine = &lines[at];
            RowLine {
                kind: l.kind,
                old: l.old_line,
                new: l.new_line,
                text: l.text.clone(),
                marks,
                anchor,
                hunk: index,
                line: at,
            }
        };
        while i < lines.len() {
            match lines[i].kind {
                DiffLineKind::Context => {
                    let l = &lines[i];
                    let anchor = l.new_line.unwrap_or(next_new);
                    next_new = anchor + 1;
                    if side_by_side {
                        out.push(Row::Pair(
                            Some(mk(i, Vec::new(), anchor)),
                            Some(mk(i, Vec::new(), anchor)),
                        ));
                    } else {
                        out.push(Row::Line(mk(i, Vec::new(), anchor)));
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
                    let left: Vec<RowLine> = (rs..added_start)
                        .zip(old_marks)
                        .map(|(at, m)| mk(at, m, removed_anchor))
                        .collect();
                    let right: Vec<RowLine> = (added_start..i)
                        .zip(new_marks)
                        .map(|(at, m)| {
                            let anchor = lines[at].new_line.unwrap_or(next_new);
                            mk(at, m, anchor)
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
    let outputs = a.test_outputs.clone();
    let spawned = ed.spawn_background("agent-review", move |tx| {
        // Discovery climbs to a parent repository when the worktree
        // directory is gone; only the worktree itself counts.
        let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.into());
        let result = match git.discover(&w.path) {
            // The diff is of the branch the record names; a worktree that
            // was switched to other work shows that instead of its diff.
            Ok(Some(repo)) if canon(repo.root()) == canon(&w.path) => match repo.head() {
                Ok(maestro_git::Head::Branch(b)) if b == w.branch => {
                    // The commit too: the same diff on another history
                    // (a merge resolved back to the same text) can merge
                    // differently with other agents' work.
                    let head = repo.resolve("HEAD").ok();
                    repo.review_diff(&w.base)
                        .map(|files| {
                            let made = test_made(&repo, &files, &outputs);
                            Reviewed { files, head, made }
                        })
                        .map_err(|e| e.to_string())
                }
                Ok(other) => Err(format!(
                    "the worktree is on {} now, not {}; no diff shown",
                    other.label(),
                    w.branch
                )),
                Err(e) => Err(e.to_string()),
            },
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

/// A review diff computed on a background thread.
struct Reviewed {
    files: Vec<DiffFile>,
    head: Option<String>,
    /// Files of `files` the verification run created, as it left them.
    made: Vec<PathBuf>,
}

/// Which added files of `files` are recorded test outputs still as their
/// run left them. When Git cannot tell, none are: every file then counts
/// as the agent's, as it did before Maestro recorded any.
fn test_made(
    repo: &maestro_git::Repository,
    files: &[DiffFile],
    outputs: &[super::TestOutput],
) -> Vec<PathBuf> {
    let added: Vec<super::TestOutput> = outputs
        .iter()
        .filter(|o| {
            files.iter().any(|f| {
                f.change == FileChange::Added
                    && f.old_path.is_none()
                    && f.new_path.as_ref() == Some(&o.path)
            })
        })
        .cloned()
        .collect();
    super::test_outputs::still_made_by_tests(repo, &added).unwrap_or_default()
}

fn applied(ed: &mut Editor, id: AgentId, result: Result<Reviewed, String>) {
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.review.running = false;
    // The agent's files first, then those the verification run made.
    let result = result.map(|mut r| {
        let made = |f: &DiffFile| {
            f.change == FileChange::Added
                && f.old_path.is_none()
                && f.new_path.as_ref().is_some_and(|p| r.made.contains(p))
        };
        let (mut files, theirs): (Vec<DiffFile>, Vec<DiffFile>) =
            r.files.drain(..).partition(|f| !made(f));
        files.extend(theirs);
        r.files = files;
        r
    });
    // Overlaps and conflicts with other agents follow every change to
    // what the diff says (files, an error, the first result) and to the
    // commit it is on. A binary file's diff does not show its contents,
    // so a diff with one always counts as changed.
    let changed = match &result {
        Ok(r) => {
            a.review.computed.is_none()
                || a.review.error.is_some()
                || r.files != a.review.files
                || r.head != a.review.head
                || r.made != a.review.test_made
                || r.files.iter().any(|f| f.binary)
        }
        Err(e) => a.review.error.as_ref() != Some(e),
    };
    match result {
        Ok(r) => {
            a.review.files = r.files;
            a.review.head = r.head;
            a.review.test_made = r.made;
            a.review.error = None;
            a.review.computed = Some(Instant::now());
            // A finished agent with nothing to review says so, judged
            // only on the newest diff: with a refresh queued this result
            // may predate the agent's last edits. Files the verification
            // run made are not the agent's.
            match (
                a.base_status(),
                a.review.agent_files().next().is_none() && !a.review.again,
            ) {
                (Status::ReadyForReview, true) => {
                    a.set_status(Status::Completed);
                    a.log("finished with no changes to review");
                }
                (Status::Completed, false) => a.set_status(Status::ReadyForReview),
                _ => {}
            }
        }
        Err(e) => a.review.error = Some(e),
    }
    // Marks follow the diff: a hunk the agent changed since is unmarked.
    let mut dropped = super::marks::prune(a);
    // The compose view's marks of this agent follow its diff the same way.
    let keys: std::collections::BTreeSet<super::marks::HunkKey> = a
        .review
        .agent_files()
        .flat_map(|f| f.hunks.iter().map(move |h| super::marks::HunkKey::of(f, h)))
        .collect();
    if let Some(c) = &mut ed.agents.panel.compose {
        let before = c.marks.len();
        c.marks
            .retain(|(agent, m)| *agent != id || keys.contains(&m.key));
        dropped += before - c.marks.len();
    }
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let again = a.review.again && !a.archived;
    if again {
        request(ed, id);
    }
    if changed {
        super::conflicts::refresh(ed);
    }
    if dropped > 0 {
        let name = ed
            .agents
            .get(id)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        ed.info(format!(
            "{dropped} marked hunk(s) of {name} changed in its worktree and were unmarked"
        ));
    }
    let n = ed.agents.get(id).map(|a| a.review.files.len()).unwrap_or(0);
    if let Some(v) = &mut ed.agents.panel.review
        && v.agent == id
    {
        v.file = v.file.min(n.saturating_sub(1));
    }
}

/// Why the review screen does not open for the agent. [`open`] refuses
/// with it and the panel's hints use it.
pub(crate) fn blocker(a: &super::Agent) -> Option<String> {
    if a.worktree.is_none() {
        return Some("the agent has no worktree yet".into());
    }
    if a.archived {
        return Some("the agent's worktree was archived; there is nothing to review".into());
    }
    None
}

/// Opens the review screen for agent `id` and refreshes its diff.
pub fn open(ed: &mut Editor, id: AgentId) {
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    if let Some(why) = blocker(a) {
        ed.error(why);
        return;
    }
    ed.agents.panel.review = Some(ReviewView::new(id));
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
    let mut out = Vec::new();
    if a.review.is_test_made(f) {
        out.extend(test_made_notes(a, f));
    } else if let Some(c) = &a.contract {
        for (_, why) in super::out_of_scope(c, std::slice::from_ref(f)) {
            out.push(Row::Note(format!(
                "{OUT_OF_SCOPE}: {why}; integrating it needs O, D drops it"
            )));
        }
    }
    out.extend(rows(f, v.side_by_side, &file_comments(&a.comments, f)));
    out
}

/// The words the review puts on a file the task contract does not allow.
pub const OUT_OF_SCOPE: &str = "outside the task contract";

/// The words the review puts on a file Maestro's verification run made.
pub const TEST_MADE: &str = "created by Maestro's test run";

/// The lines above the diff of a file the verification run made.
fn test_made_notes(a: &super::Agent, f: &DiffFile) -> Vec<Row> {
    let output = f
        .new_path
        .as_ref()
        .and_then(|p| a.test_outputs.iter().find(|o| &o.path == p));
    let command = output.map_or(String::new(), |o| format!(" (`{}`)", o.command));
    let mut out = vec![
        Row::Note(format!("T {TEST_MADE}{command}, not by the agent")),
        Row::Note(
            "not counted, integrated or composed; K counts it as the agent's, D drops it (deletes it)"
                .into(),
        ),
    ];
    if output.is_some_and(|o| o.agent_was_running) {
        out.push(Row::Note(
            "the agent was running during that run; if it wrote this file, press K".into(),
        ));
    }
    out
}

pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    let Some(mut v) = ed.agents.panel.review.clone() else {
        return;
    };
    let id = v.agent;
    let nfiles = ed.agents.get(id).map(|a| a.review.files.len()).unwrap_or(0);
    let rows = current_rows(ed);
    let height = agent_layout(ed.area).content.height.max(1) as usize;
    let last = rows.len().saturating_sub(1);
    let ctrl = key.mods.ctrl;
    match key.code {
        // A line range being chosen: esc drops it, space or v marks it.
        KeyCode::Esc if v.anchor.is_some() => v.anchor = None,
        KeyCode::Char(' ' | 'v') if !ctrl && v.anchor.is_some() => {
            let anchor = v.anchor.take().unwrap_or(v.cursor);
            super::marks::toggle_range(ed, id, v.file, &rows, anchor, v.cursor);
        }
        KeyCode::Char(' ') if !ctrl => {
            super::marks::toggle_hunk(ed, id, v.file, &rows, v.cursor);
        }
        KeyCode::Char('v') if !ctrl => {
            if matches!(rows.get(v.cursor), Some(Row::Line(_) | Row::Pair(..))) {
                v.anchor = Some(v.cursor);
                ed.info("choose the lines with j/k; space or v marks them, esc cancels");
            } else {
                ed.error("put the cursor on a diff line to start a range");
            }
        }
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
            v.anchor = None;
        }
        KeyCode::BackTab | KeyCode::Char('{') if nfiles > 0 => {
            v.file = (v.file + nfiles - 1) % nfiles;
            v.cursor = 0;
            v.top = 0;
            v.anchor = None;
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
            v.anchor = None;
            ed.agents.panel.review = Some(v.clone());
            let new_rows = current_rows(ed);
            v.cursor = target
                .and_then(|t| new_rows.iter().position(|r| r.target_line() == Some(t)))
                .unwrap_or(0);
        }
        KeyCode::Char('r') if !ctrl => request(ed, id),
        KeyCode::Char('c')
            if !ctrl
                && ed
                    .agents
                    .get(id)
                    .is_some_and(|a| a.origin == super::Origin::Found) =>
        {
            let why = ed.agents.get(id).and_then(super::actions::found_refusal);
            ed.error(why.unwrap_or_default());
        }
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
        KeyCode::Char('K') if !ctrl => {
            let file = ed
                .agents
                .get(id)
                .and_then(|a| a.review.files.get(v.file))
                .and_then(|f| f.new_path.clone());
            match file {
                Some(p) => super::test_outputs::keep_as_agents(ed, id, &p),
                None => ed.error("no file here was created by Maestro's test run"),
            }
        }
        KeyCode::Char('t') if !ctrl => super::actions::run_tests(ed, id),
        KeyCode::Char('m' | 'f') if !ctrl => super::actions::ask_message(ed, id),
        KeyCode::Char('?') => super::panel::show_keys(ed),
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
        ("esc", "back"),
        ("j/k", "line"),
        ("]/[", "hunk"),
        ("space", "mark hunk"),
        ("v", "mark lines"),
        ("tab", "file"),
        ("s", "side by side"),
        ("c", "comment"),
        ("D", "drop file"),
        ("K", "count as the agent's"),
        ("ret", "open file"),
        ("m", "message agent"),
        ("t", "test"),
        ("I", "integrate"),
        ("X", "reject"),
        ("r", "refresh"),
        ("g/G", "top, bottom"),
        ("pgup/pgdn", "page"),
        ("S-tab", "previous file"),
        (":", "command"),
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
