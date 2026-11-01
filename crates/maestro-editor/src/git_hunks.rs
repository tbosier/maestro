//! Git hunks in buffers: `]h`/`[h` move between changes, and under
//! `space h`: `s` stages the change under the cursor (or the selected
//! lines), `r` resets it to the staged text after asking, `p` shows its
//! diff, `b` blames the cursor line and `B` shows a blame column.
//!
//! The changes are the ones the gutter signs show (see [`crate::git`]).
//! Staging reads the file and the index on a background thread and applies
//! a patch to the index only; the file on disk is never written. Resetting
//! edits the buffer, so `u` undoes it.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use maestro_core::io::Encoding;
use maestro_core::{Change, ChangeSet, DocumentVersion, LineEnding, Selection, Transaction};
use maestro_git::{ApplyTo, DiffLineKind, LineHunk, LineHunkKind, Side, TextPatch, intraline};

use crate::editor::{DocumentId, Editor, Severity};

crate::commands! {
    goto_next_hunk => "Go to the next changed hunk in the buffer",
    goto_prev_hunk => "Go to the previous changed hunk in the buffer",
    stage_hunk => "Stage the change under the cursor, or the selected lines (file must be saved)",
    reset_hunk => "Reset the change under the cursor to the staged text, after asking",
    preview_hunk => "Show the diff of the change under the cursor",
    blame_line => "Show the commit, author, date and summary for the cursor line",
    toggle_blame => "Show or hide a blame column in the gutter",
}

/// Default keys, as (keys, group, command), bound by `keymap.rs`.
pub const DEFAULT_KEYS: &[(&str, &str, &str)] = &[
    ("] h", "next", "goto_next_hunk"),
    ("[ h", "previous", "goto_prev_hunk"),
    ("space h s", "hunk", "stage_hunk"),
    ("space h r", "hunk", "reset_hunk"),
    ("space h p", "hunk", "preview_hunk"),
    ("space h b", "hunk", "blame_line"),
    ("space h B", "hunk", "toggle_blame"),
];

/// Columns the blame column takes: `abc1234 author____ 2026-10-31 `.
pub const BLAME_WIDTH: u16 = 31;
const BLAME_AUTHOR: usize = 11;

/// A popup over the buffer: a hunk's diff or a line's blame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub doc: DocumentId,
    /// The zero-based line it belongs to (drawn below it).
    pub line: usize,
    pub title: String,
    pub lines: Vec<PopupLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupLine {
    /// `None` for plain text (blame details).
    pub kind: Option<DiffLineKind>,
    pub text: String,
    /// Changed byte ranges of `text` (intraline marks).
    pub marks: Vec<Range<usize>>,
}

/// One line's blame, as the column shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlameCell {
    pub short: String,
    pub author: String,
    pub date: String,
    pub uncommitted: bool,
}

impl BlameCell {
    /// The column's text, exactly [`BLAME_WIDTH`] columns wide when the
    /// author name is plain ASCII.
    pub fn label(&self) -> String {
        if self.uncommitted {
            return format!("{:<w$}", "not committed yet", w = BLAME_WIDTH as usize);
        }
        let author: String = self.author.chars().take(BLAME_AUTHOR).collect();
        format!(
            "{:<7} {:<a$} {:<10} ",
            self.short,
            author,
            self.date,
            a = BLAME_AUTHOR
        )
    }
}

/// A buffer's blame column.
#[derive(Debug, Clone, Default)]
pub struct BlameColumn {
    /// The buffer version `lines` was computed for.
    pub version: Option<DocumentVersion>,
    /// One entry per buffer line.
    pub lines: Vec<Option<BlameCell>>,
    pub error: Option<String>,
    running: bool,
}

/// Columns the blame column adds to a view's gutter.
pub fn blame_width(ed: &Editor, doc: DocumentId) -> u16 {
    if ed.git.blame.contains_key(&doc) {
        BLAME_WIDTH
    } else {
        0
    }
}

/// What to draw in the blame column for a line: `None` while it loads or
/// when the line has no blame (it was added after the last blame).
pub fn blame_cell(ed: &Editor, doc: DocumentId, line: usize) -> Option<&BlameCell> {
    ed.git.blame.get(&doc)?.lines.get(line)?.as_ref()
}

/// Closes the popup at the start of each key, like the hover popup.
pub fn on_key(ed: &mut Editor) {
    ed.git.popup = None;
}

/// The zero-based line a change's gutter sign sits on.
pub fn sign_line(h: &LineHunk) -> usize {
    if h.kind == LineHunkKind::Removed {
        h.new.start.saturating_sub(1)
    } else {
        h.new.start
    }
}

/// The change shown at `line`, from a list of changes.
fn hunk_at(hunks: &[LineHunk], line: usize) -> Option<usize> {
    hunks
        .iter()
        .position(|h| h.new.contains(&line))
        .or_else(|| {
            hunks
                .iter()
                .position(|h| h.kind == LineHunkKind::Removed && sign_line(h) == line)
        })
}

fn cursor_line(ed: &Editor) -> usize {
    let text = ed.doc().text().slice(..);
    text.char_to_line(ed.primary().head.min(text.len_chars()))
}

/// The buffer's changes, when they are current for its text. Says why
/// not otherwise.
fn current_hunks(ed: &mut Editor) -> Option<(Vec<LineHunk>, Arc<str>, DocumentVersion)> {
    let doc = ed.view().doc;
    if ed.doc().path().is_none() {
        ed.error("the buffer has no file, so Git has nothing to compare it with");
        return None;
    }
    match ed.git.hunks(doc) {
        None => {
            ed.set_status(
                Severity::Warning,
                "no Git changes known for this buffer (not tracked, or not read yet)",
            );
            None
        }
        Some((_, v, _)) if v != ed.doc().version() => {
            ed.set_status(
                Severity::Warning,
                "the change list is being updated after an edit; try again in a moment",
            );
            None
        }
        Some((h, v, base)) => Some((h.to_vec(), base, v)),
    }
}

fn goto_hunk(ed: &mut Editor, forward: bool) {
    let Some((hunks, _, _)) = current_hunks(ed) else {
        return;
    };
    let line = cursor_line(ed);
    let mut starts: Vec<usize> = hunks.iter().map(sign_line).collect();
    starts.dedup();
    let n = ed.count_or(1);
    let target = if forward {
        starts.iter().filter(|s| **s > line).nth(n - 1)
    } else {
        starts.iter().rev().filter(|s| **s < line).nth(n - 1)
    };
    let Some(&target) = target else {
        ed.info(if hunks.is_empty() {
            "no changes in this buffer"
        } else if forward {
            "no next change"
        } else {
            "no previous change"
        });
        return;
    };
    ed.push_jump();
    let pos = ed.doc().text().line_to_char(target);
    ed.set_selection(Selection::point(pos));
    ed.info(format!(
        "change {} of {}",
        starts.iter().position(|s| *s == target).unwrap_or(0) + 1,
        starts.len()
    ));
}

fn goto_next_hunk(ed: &mut Editor) {
    goto_hunk(ed, true);
}

fn goto_prev_hunk(ed: &mut Editor) {
    goto_hunk(ed, false);
}

/// Lines of `text` without their terminators.
fn plain_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n')
        .map(|l| {
            let l = l.strip_suffix('\n').unwrap_or(l);
            l.strip_suffix('\r').unwrap_or(l)
        })
        .collect()
}

fn range_label(r: &Range<usize>) -> String {
    match r.len() {
        0 => format!("after line {}", r.start),
        1 => format!("line {}", r.start + 1),
        _ => format!("lines {}-{}", r.start + 1, r.end),
    }
}

fn preview_hunk(ed: &mut Editor) {
    let Some((hunks, base, _)) = current_hunks(ed) else {
        return;
    };
    let line = cursor_line(ed);
    let Some(i) = hunk_at(&hunks, line) else {
        ed.info("no change at the cursor line");
        return;
    };
    let h = &hunks[i];
    let buffer = ed.doc().text().to_string();
    let old = plain_lines(&base);
    let new = plain_lines(&buffer);
    let removed: Vec<&str> = h.old.clone().filter_map(|l| old.get(l).copied()).collect();
    let added: Vec<&str> = h.new.clone().filter_map(|l| new.get(l).copied()).collect();
    let mut lines = Vec::new();
    let mut old_marks = vec![Vec::new(); removed.len()];
    let mut new_marks = vec![Vec::new(); added.len()];
    for k in 0..removed.len().min(added.len()) {
        let (o, n) = intraline(removed[k], added[k]);
        old_marks[k] = o;
        new_marks[k] = n;
    }
    for (t, m) in removed.iter().zip(old_marks) {
        lines.push(PopupLine {
            kind: Some(DiffLineKind::Removed),
            text: t.to_string(),
            marks: m,
        });
    }
    for (t, m) in added.iter().zip(new_marks) {
        lines.push(PopupLine {
            kind: Some(DiffLineKind::Added),
            text: t.to_string(),
            marks: m,
        });
    }
    let title = format!(
        "change {} of {}: staged {} -> buffer {}",
        i + 1,
        hunks.len(),
        range_label(&h.old),
        range_label(&h.new)
    );
    ed.git.popup = Some(Popup {
        doc: ed.view().doc,
        line,
        title,
        lines,
    });
}

/// The text that replaces buffer lines `h.new` to bring back `h.old` of
/// `base`, in the buffer's line ending.
fn reset_text(base: &str, h: &LineHunk, crlf: bool) -> String {
    let mut out = String::new();
    for (i, l) in base.split_inclusive('\n').enumerate() {
        if h.old.contains(&i) {
            out.push_str(l);
        }
    }
    if crlf {
        let mut fixed = String::with_capacity(out.len());
        for l in out.split_inclusive('\n') {
            match l.strip_suffix('\n') {
                Some(body) => {
                    fixed.push_str(body.strip_suffix('\r').unwrap_or(body));
                    fixed.push_str("\r\n");
                }
                None => fixed.push_str(l),
            }
        }
        out = fixed;
    }
    out
}

fn reset_hunk(ed: &mut Editor) {
    let Some((hunks, base, version)) = current_hunks(ed) else {
        return;
    };
    let line = cursor_line(ed);
    let Some(i) = hunk_at(&hunks, line) else {
        ed.info("no change at the cursor line");
        return;
    };
    let h = hunks[i].clone();
    let doc = ed.view().doc;
    let what = match h.kind {
        LineHunkKind::Added => format!("delete the added {}", range_label(&h.new)),
        LineHunkKind::Removed => format!(
            "put back {} removed line(s) {}",
            h.old.len(),
            range_label(&h.new)
        ),
        LineHunkKind::Modified => format!(
            "replace {} with the {} staged line(s)",
            range_label(&h.new),
            h.old.len()
        ),
    };
    ed.on_next_char(
        &format!("reset this change: {what}? u undoes it. y/n"),
        move |ed, c| {
            if c != 'y' {
                ed.info("reset canceled; nothing changed");
                return;
            }
            apply_reset(ed, doc, version, &base, &h);
        },
    );
}

fn apply_reset(
    ed: &mut Editor,
    doc: DocumentId,
    version: DocumentVersion,
    base: &Arc<str>,
    h: &LineHunk,
) {
    // Refuse when the buffer or its base moved while the question was up.
    let same_base = ed
        .git
        .hunks(doc)
        .is_some_and(|(_, v, b)| v == version && Arc::ptr_eq(&b, base));
    if ed.view().doc != doc || ed.doc().version() != version || !same_base {
        ed.error("the buffer or the staged text changed since you asked; nothing was reset");
        return;
    }
    let crlf = ed.doc().line_ending() == LineEnding::Crlf;
    let text = ed.doc().text().clone();
    let from = text.line_to_char(h.new.start.min(text.len_lines()));
    let to = text.line_to_char(h.new.end.min(text.len_lines()));
    let replacement = reset_text(base, h, crlf);
    let cs =
        match ChangeSet::from_changes(text.len_chars(), [Change::replace(from, to, replacement)]) {
            Ok(cs) => cs,
            Err(e) => {
                ed.error(format!("could not reset: {e}"));
                return;
            }
        };
    let tx = Transaction::new(cs).with_selection(Selection::point(from));
    if ed.apply_or_report(tx) {
        ed.commit();
        ed.not_repeatable("resetting a Git change");
        ed.info("reset the change to the staged text (u undoes it)");
    }
}

/// The buffer's text as the bytes of the file it would save, when that is
/// the same as its text in UTF-8 (UTF-8, with or without a BOM).
fn buffer_bytes(ed: &Editor) -> Option<Vec<u8>> {
    let doc = ed.doc();
    let mut out = match doc.encoding() {
        Encoding::Utf8 => Vec::new(),
        Encoding::Utf8Bom => vec![0xEF, 0xBB, 0xBF],
        Encoding::Utf16Le | Encoding::Utf16Be => return None,
    };
    for chunk in doc.text().chunks() {
        out.extend_from_slice(chunk.as_bytes());
    }
    Some(out)
}

fn stage_hunk(ed: &mut Editor) {
    let Some(path) = ed.doc().path().map(PathBuf::from) else {
        ed.error("the buffer has no file to stage");
        return;
    };
    if ed.doc().is_modified() {
        ed.error("save the buffer first: changes are staged from the file on disk");
        return;
    }
    let Some(bytes) = buffer_bytes(ed) else {
        ed.error("staging part of a UTF-16 file is not supported; stage the whole file with s in space g");
        return;
    };
    // A cursor (a one-character selection) stages its change; a longer
    // selection stages the changes to the lines it covers.
    let text = ed.doc().text().clone();
    let r = ed.primary();
    let lines = if r.is_collapsed() {
        None
    } else {
        let len = text.len_chars();
        let first = text.char_to_line(r.from().min(len));
        let last = text.char_to_line(r.last().min(len));
        Some((first, last))
    };
    let line = cursor_line(ed);
    let git = ed.git.git().clone();
    let doc = ed.view().doc;
    let spawned = ed.spawn_background("git-stage-hunk", move |tx| {
        let result = stage_in_background(&git, &path, &bytes, line, lines);
        tx.send(move |ed| match result {
            Ok((root, msg)) => {
                crate::git::repository_changed(ed, &root);
                if ed.buffers.contains_key(&doc) {
                    ed.info(msg);
                }
            }
            Err(e) => ed.error(e),
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start staging: {e}"));
    }
}

fn stage_in_background(
    git: &maestro_git::Git,
    path: &std::path::Path,
    buffer: &[u8],
    line: usize,
    lines: Option<(usize, usize)>,
) -> Result<(PathBuf, String), String> {
    let repo = crate::git::find_repo(git, path)?;
    let root = repo.root().to_path_buf();
    let rel = repo
        .relative(path)
        .ok_or_else(|| format!("{} is outside the repository", path.display()))?;
    let on_disk = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if on_disk != buffer {
        return Err(format!(
            "{} changed on disk since the buffer was loaded; reload it first",
            rel.display()
        ));
    }
    let base = repo
        .base_text(path, maestro_git::Base::Index)
        .map_err(|e| e.to_string())?;
    let Some(base) = base else {
        if repo.is_untracked(path).map_err(|e| e.to_string())? {
            repo.stage_paths(&[rel.as_path()])
                .map_err(|e| e.to_string())?;
            return Ok((
                root,
                format!("{} was untracked; staged the whole file", rel.display()),
            ));
        }
        return Err(format!(
            "{} is not in the index (ignored, or in conflict)",
            rel.display()
        ));
    };
    let new = repo.staged_form(path).map_err(|e| e.to_string())?;
    let tp = TextPatch::new(&rel, &base, &new)?;
    let (picks, what) = match lines {
        Some((a, b)) => (
            tp.pick_new_lines(a, b),
            format!("lines {}-{}", a + 1, b + 1),
        ),
        None => match tp.change_at(line) {
            Some(c) => (tp.pick_change(c), "the change".to_string()),
            None => return Err("no unstaged change at the cursor line".into()),
        },
    };
    if picks.is_empty() {
        return Err("no unstaged change in the selected lines".into());
    }
    let patch = tp
        .patch
        .build(&picks, Side::Forward)
        .map_err(|e| e.to_string())?;
    repo.apply_patch(&patch, ApplyTo::Index, false)
        .map_err(|e| e.to_string())?;
    Ok((root, format!("staged {what} in {}", rel.display())))
}

fn blame_line(ed: &mut Editor) {
    let Some(path) = ed.doc().path().map(PathBuf::from) else {
        ed.error("the buffer has no file to blame");
        return;
    };
    let line = cursor_line(ed);
    let contents = buffer_bytes(ed);
    let modified = ed.doc().is_modified();
    let doc = ed.view().doc;
    let version = ed.doc().version();
    let git = ed.git.git().clone();
    let spawned = ed.spawn_background("git-blame-line", move |tx| {
        let result = crate::git::find_repo(&git, &path).and_then(|repo| {
            // Unsaved UTF-16 text cannot be handed to git; blame the file.
            let contents = if modified { contents.as_deref() } else { None };
            repo.blame(&path, contents, Some((line + 1, line + 1)))
                .map_err(|e| e.to_string())
        });
        tx.send(move |ed| {
            if ed.view().doc != doc || ed.doc().version() != version {
                return;
            }
            match result {
                Ok(lines) => match lines.first() {
                    Some(l) => {
                        let c = &l.commit;
                        let text = |s: String| PopupLine {
                            kind: None,
                            text: s,
                            marks: Vec::new(),
                        };
                        let lines = if c.uncommitted() {
                            vec![text("not committed yet".into())]
                        } else {
                            vec![
                                text(format!("commit  {}", c.id)),
                                text(format!("author  {} {}", c.author, c.author_mail)),
                                text(format!("date    {}", c.date())),
                                text(format!("summary {}", c.summary)),
                            ]
                        };
                        ed.git.popup = Some(Popup {
                            doc,
                            line,
                            title: format!("blame line {}", line + 1),
                            lines,
                        });
                    }
                    None => ed.info("git blame returned nothing for this line"),
                },
                Err(e) => ed.error(format!("blame: {e}")),
            }
        });
    });
    if let Err(e) = spawned {
        ed.error(format!("could not start blame: {e}"));
    }
}

fn toggle_blame(ed: &mut Editor) {
    let doc = ed.view().doc;
    if ed.git.blame.remove(&doc).is_some() {
        ed.info("blame column hidden");
        return;
    }
    if ed.doc().path().is_none() {
        ed.error("the buffer has no file to blame");
        return;
    }
    ed.git.blame.insert(doc, BlameColumn::default());
    ed.info("blame column shown; space h B hides it");
    refresh_blame(ed);
}

/// How long a buffer must be quiet after an edit before its blame column
/// is computed again.
const BLAME_DEBOUNCE: Duration = Duration::from_millis(300);

/// Starts a blame for each shown column whose buffer changed since its
/// last blame. Called from [`crate::git::refresh`].
pub fn refresh_blame(ed: &mut Editor) {
    let docs: Vec<DocumentId> = ed.git.blame.keys().copied().collect();
    for doc in docs {
        let Some(buf) = ed.buffers.get(&doc) else {
            continue;
        };
        let col = &ed.git.blame[&doc];
        if col.running || col.version == Some(buf.doc.version()) {
            continue;
        }
        if buf.last_edit.is_some_and(|t| t.elapsed() < BLAME_DEBOUNCE) && col.version.is_some() {
            continue;
        }
        let Some(path) = buf.doc.path().map(PathBuf::from) else {
            continue;
        };
        let version = buf.doc.version();
        let modified = buf.doc.is_modified();
        let contents = {
            let mut out = match buf.doc.encoding() {
                Encoding::Utf8 => Some(Vec::new()),
                Encoding::Utf8Bom => Some(vec![0xEF, 0xBB, 0xBF]),
                _ => None,
            };
            if let Some(o) = &mut out {
                for chunk in buf.doc.text().chunks() {
                    o.extend_from_slice(chunk.as_bytes());
                }
            }
            out
        };
        let git = ed.git.git().clone();
        if let Some(c) = ed.git.blame.get_mut(&doc) {
            c.running = true;
        }
        let spawned = ed.spawn_background("git-blame", move |tx| {
            let result = crate::git::find_repo(&git, &path).and_then(|repo| {
                let contents = if modified { contents.as_deref() } else { None };
                repo.blame(&path, contents, None).map_err(|e| e.to_string())
            });
            tx.send(move |ed| {
                let current = ed.buffers.get(&doc).map(|b| b.doc.version());
                let Some(col) = ed.git.blame.get_mut(&doc) else {
                    return;
                };
                col.running = false;
                if current != Some(version) {
                    // The buffer changed meanwhile; the next refresh blames
                    // the new text.
                    return;
                }
                col.version = Some(version);
                match result {
                    Ok(lines) => {
                        let n = lines.iter().map(|l| l.final_line).max().unwrap_or(0);
                        let mut cells = vec![None; n];
                        for l in lines {
                            let c = &l.commit;
                            cells[l.final_line - 1] = Some(BlameCell {
                                short: c.short().to_string(),
                                author: c.author.clone(),
                                date: c.date(),
                                uncommitted: c.uncommitted(),
                            });
                        }
                        col.lines = cells;
                        col.error = None;
                    }
                    Err(e) => {
                        col.lines.clear();
                        col.error = Some(e.clone());
                        ed.error(format!("blame: {e}"));
                    }
                }
            });
        });
        if spawned.is_err()
            && let Some(c) = ed.git.blame.get_mut(&doc)
        {
            c.running = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_text_converts_to_crlf() {
        let h = LineHunk {
            kind: LineHunkKind::Modified,
            old: 1..3,
            new: 1..2,
        };
        assert_eq!(reset_text("a\nb\nc\nd", &h, false), "b\nc\n");
        assert_eq!(reset_text("a\nb\nc\nd", &h, true), "b\r\nc\r\n");
        assert_eq!(
            reset_text(
                "a\r\nb\r\nc",
                &LineHunk {
                    kind: LineHunkKind::Modified,
                    old: 1..3,
                    new: 0..0
                },
                true
            ),
            "b\r\nc"
        );
    }

    #[test]
    fn blame_labels_have_a_fixed_width() {
        let c = BlameCell {
            short: "abc1234".into(),
            author: "A Very Long Author Name".into(),
            date: "2026-10-31".into(),
            uncommitted: false,
        };
        assert_eq!(c.label(), "abc1234 A Very Long 2026-10-31 ");
        assert_eq!(c.label().len(), BLAME_WIDTH as usize);
        let u = BlameCell {
            uncommitted: true,
            ..c
        };
        assert_eq!(u.label().len(), BLAME_WIDTH as usize);
    }
}
