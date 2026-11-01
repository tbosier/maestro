//! The proposal review overlay: the queue on the left, the selected
//! proposal's facts and its diff on the right, drawn like the agent review
//! screen (the same rows, intraline marks and colors). The diff is computed
//! from the document text, not from Git. Keys go to the overlay while it is
//! open.

use std::path::Path;

use maestro_git::{
    DiffFile, DiffLine, DiffLineKind, FileChange, Hunk, LineHunk, LineHunkKind, diff_lines,
};

use super::{Decision, ProposalId, State};
use crate::agents::review::Row;
use crate::editor::{Editor, Mode};
use crate::key::{KeyCode, KeyEvent};

/// Lines of unchanged text around each change.
pub const CONTEXT: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewView {
    /// The proposal shown.
    pub selected: Option<ProposalId>,
    /// Row of [`current_rows`] under the cursor.
    pub cursor: usize,
    /// First row shown.
    pub top: usize,
    /// Columns of each line scrolled off to the left (`h`, `l`), so a long
    /// line can be read to its end.
    pub left: usize,
    pub side_by_side: bool,
    /// Show the verification output instead of the diff.
    pub output: bool,
    pub dialog: Option<Dialog>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// `y` approves: what will happen, in plain words.
    Approve {
        id: ProposalId,
        title: String,
        lines: Vec<String>,
        keys: Vec<(String, String)>,
    },
    /// A reason for rejecting, sent back to the client. Enter rejects.
    Reject { id: ProposalId, text: String },
}

/// Opens the overlay on the first pending proposal (or the newest one).
pub fn open(ed: &mut Editor) {
    if ed.proposals.items.is_empty() {
        ed.info("no edit proposals: automation clients make them with proposal.create");
        return;
    }
    if ed.mode == Mode::Insert {
        ed.run_command("normal_mode");
    }
    if ed.agents.panel_open() {
        crate::agents::panel::close(ed);
    }
    ed.proposals.review = Some(ReviewView {
        selected: default_selection(ed),
        cursor: 0,
        top: 0,
        left: 0,
        side_by_side: false,
        output: false,
        dialog: None,
    });
}

pub fn close(ed: &mut Editor) {
    ed.proposals.review = None;
}

fn default_selection(ed: &Editor) -> Option<ProposalId> {
    let items = &ed.proposals.items;
    items
        .iter()
        .find(|p| p.state == State::Pending)
        .or(items.last())
        .map(|p| p.id)
}

/// Keeps the overlay pointing at a proposal that exists, after one was
/// decided, withdrawn or dropped.
pub fn after_change(ed: &mut Editor) {
    let fallback = default_selection(ed);
    let Some(v) = &mut ed.proposals.review else {
        return;
    };
    let exists = v
        .selected
        .is_some_and(|id| ed.proposals.items.iter().any(|p| p.id == id));
    if !exists {
        v.selected = fallback;
        v.cursor = 0;
        v.top = 0;
    }
    // A dialog for a proposal that is no longer pending must not stay up:
    // accepting it would act on something else than it describes.
    let stale_dialog = match &v.dialog {
        Some(Dialog::Approve { id, .. } | Dialog::Reject { id, .. }) => !ed
            .proposals
            .items
            .iter()
            .any(|p| p.id == *id && p.state == State::Pending),
        None => false,
    };
    if stale_dialog {
        v.dialog = None;
    }
}

/// The proposal the overlay shows.
pub fn selected(ed: &Editor) -> Option<&super::Proposal> {
    let id = ed.proposals.review.as_ref()?.selected?;
    ed.proposals.get(id)
}

/// The rows the overlay shows for the selected proposal: its diff, or the
/// verification output.
pub fn current_rows(ed: &Editor) -> Vec<Row> {
    let Some(v) = &ed.proposals.review else {
        return Vec::new();
    };
    let Some(p) = selected(ed) else {
        return Vec::new();
    };
    if v.output {
        let mut out = Vec::new();
        match &p.result {
            None => out.push(Row::Note("no verification output yet".into())),
            Some(r) => {
                out.push(Row::Note(format!(
                    "verification {}{}",
                    r.summary(),
                    if r.dropped > 0 {
                        format!("; {} earlier line(s) not kept", r.dropped)
                    } else {
                        String::new()
                    }
                )));
                out.extend(r.output_tail.iter().map(|l| Row::Note(l.clone())));
            }
        }
        return out;
    }
    let mut rows = Vec::new();
    // A change to the final newline shows as two lines with the same text;
    // say what differs.
    let ends = |t: &maestro_core::Rope| t.len_chars() == 0 || t.char(t.len_chars() - 1) == '\n';
    match (ends(&p.old_text), ends(&p.new_text)) {
        (true, false) => rows.push(Row::Note(
            "this change removes the newline at the end of the file".into(),
        )),
        (false, true) => rows.push(Row::Note(
            "this change adds a newline at the end of the file".into(),
        )),
        _ => {}
    }
    rows.extend(crate::agents::review::rows(&p.diff, v.side_by_side, &[]));
    rows
}

/// Columns the unified diff has for a line's text in `area` (the line
/// numbers, sign and cursor column take the rest), as the front end draws
/// it.
pub fn text_columns(area: crate::layout::Rect) -> usize {
    (crate::agents::panel::layout(area).content.width as usize).saturating_sub(12)
}

/// Changed lines of proposal `p` wider than `cols` columns, with tabs
/// counted at their widest.
pub fn wide_changed_lines(p: &super::Proposal, cols: usize) -> usize {
    p.diff
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind != DiffLineKind::Context)
        .filter(|l| crate::agents::panel::text_width(&l.text.replace('\t', "    ")) > cols)
        .count()
}

/// Splits text into lines as `maestro_git::diff_lines` numbers them, but
/// keeps each line's bytes: the line without its `\n`, a `\r` before it
/// kept, and whether a `\n` ended it.
fn split_lines(text: &str) -> Vec<(&str, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(i) => {
                out.push((&rest[..i], true));
                rest = &rest[i + 1..];
            }
            None => {
                out.push((rest, false));
                rest = "";
            }
        }
    }
    out
}

/// `diff_lines` treats `\r\n` and `\n` as the same line ending, which is
/// right for gutter signs and wrong for a review: a `\r` can change what a
/// script does. Lines it calls equal whose bytes differ (in a `\r`, or a
/// final newline) become changes of their own, merged with their
/// neighbors.
fn exact_changes(ol: &[(&str, bool)], nl: &[(&str, bool)], loose: Vec<LineHunk>) -> Vec<LineHunk> {
    let mut out: Vec<LineHunk> = Vec::new();
    let mut push = |h: LineHunk| match out.last_mut() {
        Some(last) if last.old.end == h.old.start && last.new.end == h.new.start => {
            last.old.end = h.old.end;
            last.new.end = h.new.end;
            last.kind = LineHunkKind::Modified;
        }
        _ => out.push(h),
    };
    let (mut o, mut n) = (0, 0);
    let equal_run = |o: usize, n: usize, len: usize, push: &mut dyn FnMut(LineHunk)| {
        for k in 0..len {
            if ol[o + k] != nl[n + k] {
                push(LineHunk {
                    kind: LineHunkKind::Modified,
                    old: o + k..o + k + 1,
                    new: n + k..n + k + 1,
                });
            }
        }
    };
    for h in loose {
        equal_run(o, n, h.old.start - o, &mut push);
        o = h.old.end;
        n = h.new.end;
        push(h);
    }
    equal_run(
        o,
        n,
        ol.len().saturating_sub(o).min(nl.len().saturating_sub(n)),
        &mut push,
    );
    out
}

/// A unified diff of `old` to `new` with [`CONTEXT`] lines around each
/// change, in the shape the agent review screen draws.
pub fn text_diff(old: &str, new: &str, path: &Path) -> DiffFile {
    let ol = split_lines(old);
    let nl = split_lines(new);
    let changes = exact_changes(&ol, &nl, diff_lines(old, new));
    // Changes closer than twice the context share a hunk.
    let mut groups: Vec<Vec<LineHunk>> = Vec::new();
    for h in changes {
        match groups.last_mut() {
            Some(g) if h.old.start <= g.last().expect("non-empty").old.end + 2 * CONTEXT => {
                g.push(h)
            }
            _ => groups.push(vec![h]),
        }
    }
    // A `\r` before the newline is dropped from what is shown, unless the
    // hunk's changed lines disagree about it: then every changed line in the
    // hunk keeps it, and the screen shows it as `?`.
    let line = |kind, old: Option<usize>, new: Option<usize>, l: (&str, bool), keep_cr: bool| {
        let (text, newline) = l;
        let shown = if keep_cr || !newline {
            text
        } else {
            text.strip_suffix('\r').unwrap_or(text)
        };
        DiffLine {
            kind,
            old_line: old.map(|i| i + 1),
            new_line: new.map(|i| i + 1),
            text: shown.to_string(),
            no_newline: !newline,
        }
    };
    let mut hunks = Vec::new();
    for g in groups {
        let first = &g[0];
        let before = first.old.start.min(first.new.start).min(CONTEXT);
        let (old0, new0) = (first.old.start - before, first.new.start - before);
        let (mut o, mut n) = (old0, new0);
        let mut lines = Vec::new();
        let ends_cr = |l: &(&str, bool)| l.1 && l.0.ends_with('\r');
        let changed: Vec<bool> = g
            .iter()
            .flat_map(|h| {
                h.old
                    .clone()
                    .map(|i| ends_cr(&ol[i]))
                    .chain(h.new.clone().map(|j| ends_cr(&nl[j])))
            })
            .collect();
        let keep_cr = changed.iter().any(|c| *c) && !changed.iter().all(|c| *c);
        for h in &g {
            while o < h.old.start && n < nl.len() {
                lines.push(line(DiffLineKind::Context, Some(o), Some(n), ol[o], false));
                o += 1;
                n += 1;
            }
            for i in h.old.clone() {
                lines.push(line(DiffLineKind::Removed, Some(i), None, ol[i], keep_cr));
            }
            for j in h.new.clone() {
                lines.push(line(DiffLineKind::Added, None, Some(j), nl[j], keep_cr));
            }
            o = h.old.end;
            n = h.new.end;
        }
        let after = (ol.len() - o).min(nl.len().saturating_sub(n)).min(CONTEXT);
        for _ in 0..after {
            lines.push(line(DiffLineKind::Context, Some(o), Some(n), ol[o], false));
            o += 1;
            n += 1;
        }
        // Git numbers an empty side by the line before it.
        let start = |s: usize, len: usize| if len == 0 { s } else { s + 1 };
        hunks.push(Hunk {
            old_start: start(old0, o - old0),
            old_len: o - old0,
            new_start: start(new0, n - new0),
            new_len: n - new0,
            section: String::new(),
            lines,
        });
    }
    DiffFile {
        old_path: Some(path.to_path_buf()),
        new_path: Some(path.to_path_buf()),
        change: FileChange::Modified,
        old_mode: None,
        new_mode: None,
        binary: false,
        unmerged: false,
        hunks,
    }
}

/// The approval dialog for proposal `id`: what will happen, in full,
/// including the exact verification command.
pub fn approve_dialog(ed: &Editor, id: ProposalId) -> Option<Dialog> {
    let p = ed.proposals.get(id)?;
    let buf = ed.buffers.get(&p.doc)?;
    let (add, rem) = p.counts();
    let file = p.file_label();
    let mut lines = vec![
        format!("From {}: {}", p.actor.label(), p.description),
        format!(
            "Applies {} edit(s) (+{add} -{rem} lines) to {file} as one undo step, \
             against version {} (the document is at that version now).",
            p.edit_count, p.base_version.0
        ),
    ];
    match &p.verify {
        Some(v) => {
            lines.push(format!(
                "Then saves {file} to disk and runs this command in {}, as you, \
                 with your environment:",
                v.cwd.display()
            ));
            lines.push(v.display());
            if buf.doc.is_modified() {
                lines.push(
                    "The buffer also has unsaved edits that are not part of this \
                     proposal. Saving writes them to disk too."
                        .into(),
                );
            }
            lines.push(
                "The client gets the decision, exit code and output tail; so does \
                 the document's history."
                    .into(),
            );
        }
        None => lines.push(
            "It has no verification command: nothing runs, and the buffer is not saved.".into(),
        ),
    }
    let wide = wide_changed_lines(p, text_columns(ed.area));
    if wide > 0 {
        lines.push(format!(
            "{wide} changed line(s) are wider than the review shows: a > at the right \
             edge marks them. l and h scroll sideways; read them before approving."
        ));
    }
    Some(Dialog::Approve {
        id,
        title: format!("approve proposal {}", id.0),
        lines,
        keys: vec![
            ("y".into(), "approve".into()),
            ("n/esc".into(), "cancel".into()),
        ],
    })
}

fn move_selection(ed: &mut Editor, delta: isize) {
    let ids: Vec<ProposalId> = ed.proposals.items.iter().map(|p| p.id).collect();
    let Some(v) = &mut ed.proposals.review else {
        return;
    };
    if ids.is_empty() {
        return;
    }
    let n = ids.len() as isize;
    let at = v
        .selected
        .and_then(|s| ids.iter().position(|i| *i == s))
        .unwrap_or(0) as isize;
    v.selected = Some(ids[(at + delta).rem_euclid(n) as usize]);
    v.cursor = 0;
    v.top = 0;
    v.left = 0;
}

/// Handles a key while the overlay is open.
pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    let Some(mut v) = ed.proposals.review.clone() else {
        return;
    };
    // A macro or `.` replays keys the user typed for something else; it
    // must not approve or reject a proposal they have not seen.
    if ed.is_replaying() {
        ed.error("the proposal review takes keys typed by hand; replayed keys are ignored");
        return;
    }
    let ctrl = key.mods.ctrl;
    match v.dialog.take() {
        Some(Dialog::Approve {
            id,
            title,
            lines,
            keys,
        }) => {
            let ch = key.typed_char();
            if key.code == KeyCode::Esc || ch == Some('n') || (ctrl && ch.is_none()) {
                ed.info(format!("{title}: canceled, nothing changed"));
            } else if ch == Some('y') {
                if crate::agents::panel::dialog_fits(ed.area, &lines, &keys) {
                    if let Some(r) = &mut ed.proposals.review {
                        r.dialog = None;
                    }
                    super::approve(ed, id);
                    return;
                }
                ed.error(
                    "the window is too small to show this whole dialog; make it larger, or press n",
                );
                v.dialog = Some(Dialog::Approve {
                    id,
                    title,
                    lines,
                    keys,
                });
            } else {
                v.dialog = Some(Dialog::Approve {
                    id,
                    title,
                    lines,
                    keys,
                });
            }
            ed.proposals.review = Some(v);
            return;
        }
        Some(Dialog::Reject { id, mut text }) => {
            match key.code {
                KeyCode::Esc => {
                    ed.info(format!("proposal {}: not rejected", id.0));
                }
                KeyCode::Char('c') if ctrl => {
                    ed.info(format!("proposal {}: not rejected", id.0));
                }
                KeyCode::Enter => {
                    ed.proposals.review = Some(v);
                    let reason = text.trim().to_string();
                    super::reject(ed, id, (!reason.is_empty()).then_some(reason));
                    return;
                }
                KeyCode::Backspace => {
                    text.pop();
                    v.dialog = Some(Dialog::Reject { id, text });
                }
                KeyCode::Char('u') if ctrl => {
                    v.dialog = Some(Dialog::Reject {
                        id,
                        text: String::new(),
                    })
                }
                _ => {
                    if let Some(c) = key.typed_char() {
                        text.push(c);
                    }
                    v.dialog = Some(Dialog::Reject { id, text });
                }
            }
            ed.proposals.review = Some(v);
            return;
        }
        None => {}
    }
    let rows = current_rows(ed);
    let height = crate::agents::panel::layout(ed.area).content.height.max(1) as usize;
    let last = rows.len().saturating_sub(1);
    let sel = v.selected;
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') if !ctrl => {
            close(ed);
            return;
        }
        KeyCode::Char('c') if ctrl => {
            close(ed);
            return;
        }
        KeyCode::Char('n') | KeyCode::Tab if !ctrl => {
            ed.proposals.review = Some(v);
            move_selection(ed, 1);
            return;
        }
        KeyCode::Char('p') | KeyCode::BackTab if !ctrl => {
            ed.proposals.review = Some(v);
            move_selection(ed, -1);
            return;
        }
        KeyCode::Char('j') | KeyCode::Down if !ctrl => v.cursor = (v.cursor + 1).min(last),
        KeyCode::Char('k') | KeyCode::Up if !ctrl => v.cursor = v.cursor.saturating_sub(1),
        KeyCode::PageDown => v.cursor = (v.cursor + height / 2).min(last),
        KeyCode::PageUp => v.cursor = v.cursor.saturating_sub(height / 2),
        KeyCode::Char('g') if !ctrl => v.cursor = 0,
        KeyCode::Char('G') if !ctrl => v.cursor = last,
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
            v.side_by_side = !v.side_by_side;
            v.cursor = 0;
            v.top = 0;
        }
        KeyCode::Char('l') | KeyCode::Right if !ctrl => {
            v.left += (text_columns(ed.area) / 2).max(1);
        }
        KeyCode::Char('h') | KeyCode::Left if !ctrl => {
            v.left = v.left.saturating_sub((text_columns(ed.area) / 2).max(1));
        }
        KeyCode::Char('o') if !ctrl => {
            v.output = !v.output;
            v.cursor = 0;
            v.top = 0;
        }
        KeyCode::Char('a') if !ctrl => {
            let Some(id) = sel else {
                ed.error("no proposal selected");
                return;
            };
            if let Some(why) = super::approval_blocker(ed, id) {
                ed.error(why);
            } else if ed
                .proposals
                .get(id)
                .and_then(|p| super::stale_reason(ed, p))
                .is_some()
            {
                // Nothing to confirm: approving refuses it as stale.
                ed.proposals.review = Some(v);
                super::approve(ed, id);
                return;
            } else {
                v.dialog = approve_dialog(ed, id);
            }
        }
        KeyCode::Char('x' | 'X') if !ctrl => match sel.and_then(|id| ed.proposals.get(id)) {
            Some(p) if p.state == State::Pending => {
                v.dialog = Some(Dialog::Reject {
                    id: p.id,
                    text: String::new(),
                })
            }
            Some(p) => ed.error(format!(
                "proposal {} is {}; only a pending proposal can be rejected",
                p.id.0,
                p.decision.map(Decision::label).unwrap_or(p.state.label())
            )),
            None => ed.error("no proposal selected"),
        },
        KeyCode::Char('c') if !ctrl => {
            if let Some(id) = sel {
                ed.proposals.review = Some(v);
                super::cancel_verification(ed, id);
                return;
            }
        }
        KeyCode::Char(':') if !ctrl => {
            ed.proposals.review = Some(v);
            crate::prompt::open(ed, crate::prompt::PromptKind::Command);
            return;
        }
        _ => {}
    }
    if v.cursor < v.top {
        v.top = v.cursor;
    } else if v.cursor >= v.top + height {
        v.top = v.cursor + 1 - height;
    }
    if ed.proposals.review.is_some() {
        ed.proposals.review = Some(v);
    }
}

/// Pasted text goes into the rejection reason when one is being typed, and
/// nowhere otherwise.
pub fn paste(ed: &mut Editor, text: &str) {
    if let Some(ReviewView {
        dialog: Some(Dialog::Reject { text: t, .. }),
        ..
    }) = &mut ed.proposals.review
    {
        t.push_str(&text.replace(['\r', '\n'], " "));
    }
}

/// Key hints for the overlay's last line.
pub fn hints(ed: &Editor) -> Vec<(&'static str, &'static str)> {
    let Some(v) = &ed.proposals.review else {
        return Vec::new();
    };
    match &v.dialog {
        Some(Dialog::Approve { .. }) => return vec![("y", "approve"), ("esc/n", "cancel")],
        Some(Dialog::Reject { .. }) => {
            return vec![("ret", "reject"), ("C-u", "clear"), ("esc", "cancel")];
        }
        None => {}
    }
    let mut out = Vec::new();
    match selected(ed).map(|p| p.state) {
        Some(State::Pending) => out.extend([("a", "approve"), ("x", "reject")]),
        Some(State::Verifying) => out.push(("c", "cancel verification")),
        _ => {}
    }
    out.extend([
        ("n/p", "next/previous"),
        ("j/k", "line"),
        ("]/[", "hunk"),
        ("h/l", "scroll sideways"),
        ("s", "side by side"),
        ("o", if v.output { "diff" } else { "output" }),
        ("esc", "close"),
    ]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(f: &DiffFile) -> Vec<(DiffLineKind, Option<usize>, Option<usize>, String)> {
        f.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| (l.kind, l.old_line, l.new_line, l.text.clone()))
            .collect()
    }

    #[test]
    fn diff_has_context_and_one_based_numbers() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nD\ne\nf\ng\nh\n";
        let f = text_diff(old, new, Path::new("x"));
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 7, 1, 7)
        );
        let k = kinds(&f);
        assert_eq!(k[3], (DiffLineKind::Removed, Some(4), None, "d".into()));
        assert_eq!(k[4], (DiffLineKind::Added, None, Some(4), "D".into()));
        assert_eq!(k.len(), 8);
        assert_eq!(f.counts(), (1, 1));
    }

    #[test]
    fn far_apart_changes_get_their_own_hunks_and_lengths_stay_right() {
        let old: String = (0..30).map(|i| format!("l{i}\n")).collect();
        let new = old.replace("l2\n", "").replace("l25\n", "l25\nnew\n");
        let f = text_diff(&old, &new, Path::new("x"));
        assert_eq!(f.hunks.len(), 2, "{:?}", f.hunks);
        let h = &f.hunks[1];
        assert_eq!(h.old_len + 1, h.new_len);
        assert!(
            h.lines
                .iter()
                .any(|l| l.kind == DiffLineKind::Added && l.text == "new")
        );
        // Every context line pairs the same text on both sides.
        for l in f.hunks.iter().flat_map(|h| &h.lines) {
            if l.kind == DiffLineKind::Context {
                let o: Vec<&str> = old.lines().collect();
                let n: Vec<&str> = new.lines().collect();
                assert_eq!(o[l.old_line.unwrap() - 1], n[l.new_line.unwrap() - 1]);
            }
        }
    }

    #[test]
    fn a_carriage_return_change_is_shown() {
        // `\r` before the newline breaks a shell line continuation; the
        // gutter diff ignores it, the review must not.
        let old = "echo safe \\\nprintf HIDDEN\n";
        let new = "echo safe \\\r\nprintf HIDDEN\n";
        let f = text_diff(old, new, Path::new("x"));
        let k = kinds(&f);
        assert!(
            k.contains(&(DiffLineKind::Removed, Some(1), None, "echo safe \\".into())),
            "{k:?}"
        );
        assert!(
            k.contains(&(DiffLineKind::Added, None, Some(1), "echo safe \\\r".into())),
            "{k:?}"
        );
        // A whole CRLF document with an ordinary change shows no `\r`.
        let f = text_diff("a\r\nb\r\n", "a\r\nB\r\n", Path::new("x"));
        let k = kinds(&f);
        assert!(k.iter().all(|l| !l.3.contains('\r')), "{k:?}");
        assert_eq!(f.counts(), (1, 1));
        // A missing final newline is a change too.
        let f = text_diff("a\nb\n", "a\nb", Path::new("x"));
        assert_eq!(f.counts(), (1, 1));
        assert!(f.hunks[0].lines.iter().any(|l| l.no_newline));
    }

    #[test]
    fn insert_into_empty_text() {
        let f = text_diff("", "x\n", Path::new("x"));
        assert_eq!(f.hunks.len(), 1);
        assert_eq!((f.hunks[0].old_start, f.hunks[0].old_len), (0, 0));
        assert_eq!((f.hunks[0].new_start, f.hunks[0].new_len), (1, 1));
    }
}
