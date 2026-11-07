//! Hunks marked in an agent's review, and the parts of its work already
//! integrated on their own.
//!
//! A mark holds the hunk as the review showed it when it was marked, so
//! what is integrated is exactly what was marked. When a refreshed review
//! no longer has that hunk (the agent changed it), the mark is dropped and
//! the status line says so.

use std::collections::BTreeSet;
use std::path::PathBuf;

use maestro_git::{DiffFile, DiffLineKind, FileChange, Hunk, HunkPick, changed_lines};

use super::review::{Row, RowLine};
use super::{Agent, AgentId};
use crate::editor::Editor;

/// What identifies a hunk across review refreshes: the file's paths and
/// new mode, where it starts in the base file, and its lines. Where it
/// lands in the new file is left out, so a change elsewhere in the file
/// does not drop it. Two keys are equal only when everything a commit of
/// the hunk is built from is equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HunkKey {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    /// The added file's mode.
    pub new_mode: Option<String>,
    pub old_start: usize,
    pub old_len: usize,
    /// One line per diff line: its sign, `1` when it has no newline (else
    /// `0`), then its text. Diff lines hold no newline, so this cannot be
    /// read two ways.
    pub body: String,
}

impl HunkKey {
    pub fn of(f: &DiffFile, h: &Hunk) -> Self {
        Self::build(
            f.old_path.clone(),
            f.new_path.clone(),
            f.new_mode.clone(),
            h,
        )
    }

    /// The key of the hunk a pick takes.
    pub fn of_pick(p: &HunkPick) -> Self {
        Self::build(
            p.old_path.clone(),
            p.new_path.clone(),
            p.new_mode.clone(),
            &p.hunk,
        )
    }

    fn build(
        old_path: Option<PathBuf>,
        new_path: Option<PathBuf>,
        new_mode: Option<String>,
        h: &Hunk,
    ) -> Self {
        let mut body = String::new();
        for l in &h.lines {
            body.push(match l.kind {
                DiffLineKind::Context => ' ',
                DiffLineKind::Added => '+',
                DiffLineKind::Removed => '-',
            });
            body.push(if l.no_newline { '1' } else { '0' });
            body.push_str(&l.text);
            body.push('\n');
        }
        Self {
            old_path,
            new_path,
            new_mode,
            old_start: h.old_start,
            old_len: h.old_len,
            body,
        }
    }
}

/// A marked hunk, or some of its lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub key: HunkKey,
    /// The hunk as marked; `pick.lines` is `None` for the whole hunk.
    pub pick: HunkPick,
}

impl Mark {
    /// The changed lines it takes.
    pub fn taken(&self) -> BTreeSet<usize> {
        match &self.pick.lines {
            Some(s) => s.clone(),
            None => changed_lines(&self.pick.hunk).into_iter().collect(),
        }
    }
}

/// A part of an agent's work integrated on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// 1 for the first part.
    pub n: usize,
    /// `maestro/<agent>-part-<n>`.
    pub branch: String,
    pub commit: String,
    /// The hunks it held, with the changed lines taken (`None`: all).
    pub hunks: Vec<(HunkKey, Option<BTreeSet<usize>>)>,
}

/// How a hunk stands in the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkState {
    None,
    /// `taken` of its `of` changed lines are marked.
    Marked {
        taken: usize,
        of: usize,
    },
    /// It went in with part `n` (in whole or in part).
    Integrated {
        part: usize,
        whole: bool,
    },
}

/// How one diff line stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineState {
    None,
    Marked,
    Integrated,
}

/// Files whose hunks cannot be taken on their own, and why.
pub fn unmarkable(f: &DiffFile) -> Option<&'static str> {
    if f.binary {
        Some("a binary file has no hunks to mark")
    } else if f.unmerged {
        Some("a file in conflict cannot be marked")
    } else if matches!(
        f.change,
        FileChange::Renamed { .. } | FileChange::Copied { .. }
    ) || (f.old_path.is_some() && f.new_path.is_some() && f.old_path != f.new_path)
    {
        Some("hunks of a renamed or copied file are integrated only with the whole work")
    } else if f.hunks.is_empty() {
        Some("this file has no line changes to mark")
    } else {
        None
    }
}

pub fn hunk_state(a: &Agent, f: &DiffFile, index: usize) -> HunkState {
    let Some(h) = f.hunks.get(index) else {
        return HunkState::None;
    };
    let key = HunkKey::of(f, h);
    if let Some(m) = a.marks.iter().find(|m| m.key == key) {
        return HunkState::Marked {
            taken: m.taken().len(),
            of: changed_lines(h).len(),
        };
    }
    for p in &a.parts {
        if let Some((_, lines)) = p.hunks.iter().find(|(k, _)| *k == key) {
            return HunkState::Integrated {
                part: p.n,
                whole: lines.is_none(),
            };
        }
    }
    HunkState::None
}

pub fn line_state(a: &Agent, f: &DiffFile, hunk: usize, line: usize) -> LineState {
    let Some(h) = f.hunks.get(hunk) else {
        return LineState::None;
    };
    if h.lines
        .get(line)
        .is_none_or(|l| l.kind == DiffLineKind::Context)
    {
        return LineState::None;
    }
    let key = HunkKey::of(f, h);
    if let Some(m) = a.marks.iter().find(|m| m.key == key) {
        return if m.taken().contains(&line) {
            LineState::Marked
        } else {
            LineState::None
        };
    }
    for p in &a.parts {
        if let Some((_, lines)) = p.hunks.iter().find(|(k, _)| *k == key)
            && lines.as_ref().is_none_or(|s| s.contains(&line))
        {
            return LineState::Integrated;
        }
    }
    LineState::None
}

/// Marked hunks and marked changed lines, for the review's header.
pub fn counts(a: &Agent) -> (usize, usize) {
    (a.marks.len(), a.marks.iter().map(|m| m.taken().len()).sum())
}

/// `2 hunks (7 lines) marked`, or `None` without marks.
pub fn describe(a: &Agent) -> Option<String> {
    let (h, l) = counts(a);
    (h > 0).then(|| {
        format!(
            "{h} hunk{} ({l} line{}) marked: I integrates only them",
            if h == 1 { "" } else { "s" },
            if l == 1 { "" } else { "s" }
        )
    })
}

/// Drops marks whose hunk the agent's refreshed review no longer has, or
/// has only in a file the verification run made. Returns how many were
/// dropped.
pub fn prune(a: &mut Agent) -> usize {
    let keys: BTreeSet<HunkKey> = a
        .review
        .agent_files()
        .flat_map(|f| f.hunks.iter().map(move |h| HunkKey::of(f, h)))
        .collect();
    let before = a.marks.len();
    a.marks.retain(|m| keys.contains(&m.key));
    before - a.marks.len()
}

/// The hunk and line a review row stands for.
fn row_place(row: &Row) -> Option<(usize, Option<usize>)> {
    match row {
        Row::Hunk { index, .. } => Some((*index, None)),
        Row::Line(l) => Some((l.hunk, Some(l.line))),
        Row::Pair(l, r) => {
            let x: &RowLine = r.as_ref().or(l.as_ref())?;
            Some((x.hunk, Some(x.line)))
        }
        _ => None,
    }
}

/// The changed lines a review row shows (a side-by-side row has two).
fn row_changed_lines(row: &Row) -> Vec<(usize, usize)> {
    let one = |l: &RowLine| (l.kind != DiffLineKind::Context).then_some((l.hunk, l.line));
    match row {
        Row::Line(l) => one(l).into_iter().collect(),
        Row::Pair(l, r) => l.iter().chain(r.iter()).filter_map(one).collect::<Vec<_>>(),
        _ => Vec::new(),
    }
}

/// Why hunk `index` of `f` cannot be marked now, if it cannot.
fn refusal(a: &Agent, f: &DiffFile, index: usize) -> Option<String> {
    if let Some(why) = unmarkable(f) {
        return Some(why.to_string());
    }
    if a.review.is_test_made(f) {
        return Some(format!(
            "this file was {}, not by the agent, and is never integrated; K counts it as the \
             agent's",
            super::review::TEST_MADE
        ));
    }
    match hunk_state(a, f, index) {
        HunkState::Integrated { part, whole: true } => Some(format!(
            "this hunk was integrated in part {part}; it is already on the target branch"
        )),
        // Each part is built on the agent's starting commit, so the lines
        // left would change the same lines as the part already there.
        HunkState::Integrated { part, whole: false } => Some(format!(
            "some lines of this hunk were integrated in part {part}; the lines left cannot be \
             integrated on their own, only with the whole work (I with no marks), where Git \
             reports a conflict if they clash with the part"
        )),
        _ => None,
    }
}

/// `space` in the review: marks the hunk under the cursor, or unmarks it.
pub fn toggle_hunk(ed: &mut Editor, id: AgentId, file: usize, rows: &[Row], cursor: usize) {
    let Some((hunk, _)) = rows.get(cursor).and_then(row_place) else {
        ed.error("put the cursor on a hunk to mark it");
        return;
    };
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(f) = a.review.files.get(file).cloned() else {
        return;
    };
    let Some(h) = f.hunks.get(hunk) else {
        return;
    };
    let key = HunkKey::of(&f, h);
    if let Some(i) = a.marks.iter().position(|m| m.key == key) {
        if let Some(a) = ed.agents.get_mut(id) {
            a.marks.remove(i);
        }
        return;
    }
    if let Some(why) = refusal(a, &f, hunk) {
        ed.error(why);
        return;
    }
    let name = a.name.clone();
    let Some(pick) = HunkPick::new(name, &f, hunk, None) else {
        return;
    };
    if let Some(a) = ed.agents.get_mut(id) {
        a.marks.push(Mark { key, pick });
    }
}

/// `v` then movement, then `space` or `v`: marks the changed lines between
/// `anchor` and `cursor` in the anchor's hunk, or unmarks them when all of
/// them are marked already.
pub fn toggle_range(
    ed: &mut Editor,
    id: AgentId,
    file: usize,
    rows: &[Row],
    anchor: usize,
    cursor: usize,
) {
    let Some((hunk, _)) = rows.get(anchor).and_then(row_place) else {
        ed.error("the range starts outside a hunk; v on a diff line starts one");
        return;
    };
    let (lo, hi) = (anchor.min(cursor), anchor.max(cursor));
    let mut lines: BTreeSet<usize> = BTreeSet::new();
    let mut clipped = false;
    for row in rows.iter().take(hi + 1).skip(lo) {
        for (h, l) in row_changed_lines(row) {
            if h == hunk {
                lines.insert(l);
            } else {
                clipped = true;
            }
        }
    }
    if lines.is_empty() {
        ed.error("the range has no added or removed lines");
        return;
    }
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    let Some(f) = a.review.files.get(file).cloned() else {
        return;
    };
    let Some(h) = f.hunks.get(hunk) else {
        return;
    };
    if let Some(why) = refusal(a, &f, hunk) {
        ed.error(why);
        return;
    }
    let key = HunkKey::of(&f, h);
    let all: BTreeSet<usize> = changed_lines(h).into_iter().collect();
    let name = a.name.clone();
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let current = a
        .marks
        .iter()
        .position(|m| m.key == key)
        .map(|i| (i, a.marks[i].taken()));
    let next: BTreeSet<usize> = match &current {
        Some((_, taken)) if lines.is_subset(taken) => taken.difference(&lines).copied().collect(),
        Some((_, taken)) => taken.union(&lines).copied().collect(),
        None => lines,
    };
    if let Some((i, _)) = current {
        a.marks.remove(i);
    }
    if !next.is_empty() {
        let sel = (next != all).then_some(next);
        if let Some(pick) = HunkPick::new(name, &f, hunk, sel) {
            a.marks.push(Mark { key, pick });
        }
    }
    if clipped {
        ed.info("a range marks lines of one hunk only: the lines in other hunks were left out");
    }
}

/// The picks of an agent's marks, in diff order.
pub fn picks(a: &Agent) -> Vec<HunkPick> {
    let mut out: Vec<(usize, usize, HunkPick)> = a
        .marks
        .iter()
        .map(|m| {
            let at = a
                .review
                .files
                .iter()
                .enumerate()
                .find_map(|(fi, f)| {
                    f.hunks
                        .iter()
                        .position(|h| HunkKey::of(f, h) == m.key)
                        .map(|hi| (fi, hi))
                })
                .unwrap_or((usize::MAX, usize::MAX));
            (at.0, at.1, m.pick.clone())
        })
        .collect();
    out.sort_by_key(|(f, h, _)| (*f, *h));
    out.into_iter().map(|(_, _, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_git::DiffLine;

    fn hunk(text: &str, no_newline: bool) -> Hunk {
        Hunk {
            old_start: 0,
            old_len: 0,
            new_start: 1,
            new_len: 1,
            section: String::new(),
            lines: vec![DiffLine {
                kind: DiffLineKind::Added,
                old_line: None,
                new_line: Some(1),
                text: text.into(),
                no_newline,
            }],
        }
    }

    #[test]
    fn keys_tell_apart_what_builds_different_files() {
        let file = |mode: &str| DiffFile {
            old_path: None,
            new_path: Some("a".into()),
            change: FileChange::Added,
            old_mode: None,
            new_mode: Some(mode.into()),
            binary: false,
            unmerged: false,
            hunks: vec![],
        };
        let f = file("100644");
        // `new` without a newline and `new\` with one.
        assert_ne!(
            HunkKey::of(&f, &hunk("new", true)),
            HunkKey::of(&f, &hunk("new\\", false))
        );
        // The same text as a file and as a symlink.
        assert_ne!(
            HunkKey::of(&f, &hunk("t", true)),
            HunkKey::of(&file("120000"), &hunk("t", true))
        );
        assert_eq!(
            HunkKey::of(&f, &hunk("t", true)),
            HunkKey::of(&f, &hunk("t", true))
        );
    }
}
