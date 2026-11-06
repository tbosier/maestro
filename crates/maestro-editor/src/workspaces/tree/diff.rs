//! A changed file's diff against the tree's base (`d` in the file tree),
//! over the views. It is the review screen's diff ([`review::rows`]):
//! unified or side by side, with changed words marked. Keys:
//!
//! | key | does |
//! |---|---|
//! | `j` `k` (arrows) | move |
//! | `C-d` `C-u`, page keys | half a page, a page |
//! | `g` `G` | first, last line |
//! | `s` | unified or side by side |
//! | `ret` | open the file at this line |
//! | `q` `esc` | back to the tree |
//!
//! The diff is read on a background thread; it is not refreshed while it
//! is open (`d` again reads it again).

use std::path::PathBuf;

use maestro_git::{DiffFile, Git};

use super::{Comparison, PathChangeKind, Row, RowKind};
use crate::agents::review;
use crate::editor::{Editor, MouseKind};
use crate::key::{KeyCode, KeyEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffView {
    /// The file, relative to the tree's root.
    pub path: PathBuf,
    /// `src/a.rs  modified vs main (merge-base abc1234)`.
    pub title: String,
    pub state: DiffState,
    pub side_by_side: bool,
    pub cursor: usize,
    pub top: usize,
    seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffState {
    Loading,
    Failed(String),
    /// Both layouts are built on the background thread, so `s` only
    /// switches between them.
    Ready {
        file: Box<DiffFile>,
        unified: Vec<review::Row>,
        side_by_side: Vec<review::Row>,
    },
}

impl DiffView {
    pub fn rows(&self) -> &[review::Row] {
        match &self.state {
            DiffState::Ready {
                unified,
                side_by_side,
                ..
            } => {
                if self.side_by_side {
                    side_by_side
                } else {
                    unified
                }
            }
            _ => &[],
        }
    }
}

/// Rows of diff under the title line and above the key line.
pub fn content_height(ed: &Editor) -> usize {
    usize::from(ed.area.height.saturating_sub(2)).max(1)
}

/// Opens the diff of the file on `row` (`d`).
pub(crate) fn open(ed: &mut Editor, row: &Row) {
    let change = match &row.kind {
        RowKind::File { change } => change.clone(),
        RowKind::Dir { .. } => {
            ed.info("d shows a file's diff: choose a changed file");
            return;
        }
        RowKind::Note(_) => return,
    };
    let Some(cmp) = ed.workspaces.tree.changes.ready().cloned() else {
        let why = match &ed.workspaces.tree.changes {
            super::Changes::Unavailable(why) => format!("no diff: {why}"),
            _ => "no diff yet: still reading the changes".to_string(),
        };
        ed.info(why);
        return;
    };
    let Some(kind) = change else {
        ed.info(format!(
            "{} is the same as {}",
            row.path.display(),
            cmp.base_label()
        ));
        return;
    };
    let Some(root) = ed.workspaces.tree.root.clone() else {
        return;
    };
    let seq = {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    };
    ed.workspaces.tree.diff = Some(DiffView {
        path: row.path.clone(),
        title: format!(
            "{}  {} vs {}",
            row.path.display(),
            kind.word(),
            cmp.base_label()
        ),
        state: DiffState::Loading,
        side_by_side: false,
        cursor: 0,
        top: 0,
        seq,
    });
    let git = ed.git.git().clone();
    let abs = root.join(&row.path);
    let spawned = ed.spawn_background("file-tree-diff", move |tx| {
        let result = read(&git, &cmp, &abs, &kind).map(|file| {
            let unified = review::rows(&file, false, &[]);
            let side_by_side = review::rows(&file, true, &[]);
            DiffState::Ready {
                file: Box::new(file),
                unified,
                side_by_side,
            }
        });
        tx.send(move |ed| {
            let Some(d) = ed.workspaces.tree.diff.as_mut().filter(|d| d.seq == seq) else {
                return;
            };
            d.state = result.unwrap_or_else(DiffState::Failed);
        });
    });
    if let Err(e) = spawned
        && let Some(d) = &mut ed.workspaces.tree.diff
    {
        d.state = DiffState::Failed(format!("could not start Git: {e}"));
    }
}

/// Runs on a background thread.
fn read(
    git: &Git,
    cmp: &Comparison,
    abs: &std::path::Path,
    kind: &PathChangeKind,
) -> Result<DiffFile, String> {
    let e = |e: maestro_git::GitError| e.to_string();
    let repo = git
        .discover(&cmp.repo_root)
        .map_err(e)?
        .ok_or_else(|| "no longer a Git repository".to_string())?;
    let rel = abs
        .strip_prefix(&cmp.repo_root)
        .map_err(|_| format!("{} is outside the repository", abs.display()))?
        .to_path_buf();
    let rev = match (kind, cmp.rev()) {
        (PathChangeKind::Untracked, _) | (_, None) => {
            return repo.diff_untracked(&rel).map_err(e);
        }
        (_, Some(rev)) => rev,
    };
    let mut paths = vec![rel.clone()];
    if let PathChangeKind::Renamed { from } = kind {
        paths.push(from.clone());
    }
    let files = repo.diff_against(rev, &paths).map_err(e)?;
    files
        .into_iter()
        .find(|f| f.path() == Some(&rel))
        .ok_or_else(|| format!("{} no longer differs", rel.display()))
}

/// Keys while the diff shows.
pub(crate) fn handle_key(ed: &mut Editor, key: KeyEvent) {
    let height = content_height(ed);
    let Some(d) = &mut ed.workspaces.tree.diff else {
        return;
    };
    let len = d.rows().len();
    let half = (height / 2).max(1) as isize;
    let delta: isize = match (key.code, key.mods.ctrl) {
        (KeyCode::Esc, _) | (KeyCode::Char('q'), false) => {
            ed.workspaces.tree.diff = None;
            return;
        }
        (KeyCode::Char('j') | KeyCode::Down, false) => 1,
        (KeyCode::Char('k') | KeyCode::Up, false) => -1,
        (KeyCode::Char('d'), true) => half,
        (KeyCode::Char('u'), true) => -half,
        (KeyCode::PageDown, _) => height as isize,
        (KeyCode::PageUp, _) => -(height as isize),
        (KeyCode::Char('g') | KeyCode::Home, false) => isize::MIN / 2,
        (KeyCode::Char('G') | KeyCode::End, false) => isize::MAX / 2,
        (KeyCode::Char('s'), false) => {
            d.side_by_side = !d.side_by_side;
            d.cursor = d.cursor.min(d.rows().len().saturating_sub(1));
            scroll(d, height);
            return;
        }
        (KeyCode::Enter, _) => {
            let line = d.rows().get(d.cursor).and_then(review::Row::target_line);
            let path = d.path.clone();
            open_at(ed, path, line.unwrap_or(1));
            return;
        }
        _ => {
            ed.info("diff: j/k move, s side by side, ret open the file here, q back to the tree");
            return;
        }
    };
    if len == 0 {
        return;
    }
    d.cursor = (d.cursor as isize)
        .saturating_add(delta)
        .clamp(0, len as isize - 1) as usize;
    scroll(d, height);
}

fn scroll(d: &mut DiffView, height: usize) {
    if d.cursor < d.top {
        d.top = d.cursor;
    } else if d.cursor >= d.top + height {
        d.top = d.cursor + 1 - height;
    }
}

/// Wheel and clicks over the diff.
pub(crate) fn handle_mouse(ed: &mut Editor, kind: MouseKind, y: u16) {
    let height = content_height(ed);
    let area = ed.area;
    let Some(d) = &mut ed.workspaces.tree.diff else {
        return;
    };
    let len = d.rows().len();
    let max = len.saturating_sub(height);
    match kind {
        MouseKind::ScrollUp => d.top = d.top.saturating_sub(3),
        MouseKind::ScrollDown => d.top = (d.top + 3).min(max),
        MouseKind::Down => {
            // The title is the first row, the key line the last.
            let n = usize::from(y.saturating_sub(area.y + 1));
            let i = d.top + n;
            if y > area.y && n < height && i < len {
                d.cursor = i;
            }
        }
        MouseKind::Drag => {}
    }
}

/// Closes the diff and the tree's focus and opens the file at one-based
/// `line`.
fn open_at(ed: &mut Editor, rel: PathBuf, line: usize) {
    let Some(root) = ed.workspaces.tree.root.clone() else {
        return;
    };
    let path = root.join(&rel);
    if !path.exists() {
        ed.info(format!("{} is not in the working tree", rel.display()));
        return;
    }
    super::blur(ed);
    if let Err(e) = ed.open(&path) {
        ed.error(e);
        return;
    }
    super::say_if_covered(ed, &rel);
    let text = ed.doc().text().clone();
    let l = line
        .saturating_sub(1)
        .min(text.len_lines().saturating_sub(1));
    ed.push_jump();
    ed.set_selection(maestro_core::Selection::point(text.line_to_char(l)));
    crate::commands::align_view_center(ed);
}
