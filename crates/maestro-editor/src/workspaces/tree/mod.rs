//! The file tree: the current worktree's files in a column right of the
//! views, with every file that differs from the base branch marked.
//! `space t` (or `:tree`) opens and focuses it, focuses it again, or hides
//! it; keys while it has focus:
//!
//! | key | does |
//! |---|---|
//! | `j` `k` (arrows) | move |
//! | `g` `G` | first, last row |
//! | `C-d` `C-u` | half a page down, up |
//! | `ret` | directory: fold or unfold; file: open it in the focused view |
//! | `l` `h` | unfold, fold (on a file, `h` folds its directory) |
//! | `c` | show only changed files, or every file again |
//! | `d` | the selected file's diff against the base |
//! | `r` | list the files and read the changes again now |
//! | `q` | hide the tree |
//! | `esc` | back to the editor, the tree stays |
//! | `space ...` | the normal `space` keys (`space t` hides it) |
//!
//! The base is the merge-base of `HEAD` with the default branch
//! (`origin/HEAD`'s target, else `main`, else `master`), so the marks show
//! what the branch would bring: its commits, staged and unstaged edits and
//! untracked files. On the default branch itself, or without one, the
//! tree compares with `HEAD` (uncommitted changes only) and its header says
//! so. Each mark is a letter and a color (`M` modified, `A` added, `D`
//! deleted, `R` renamed, `?` untracked); the letter alone carries it.
//!
//! Directory listings and Git run on background threads ([`load`]):
//! directories are listed one at a time when unfolded, and a file's diff
//! (with its changed words) is built there too. What runs on the UI thread
//! is building the visible rows from that state, which grows with the
//! unfolded directories (at most [`load::MAX_ENTRIES`] each) or, in the
//! changed-only view, with the number of changed files. The tree follows the working directory when you switch
//! worktrees, reads the changes again after a save and every few seconds
//! while it is open. Rows are built from that state each time they are
//! drawn ([`rows`]); the selection is remembered by path.

pub mod diff;
pub(crate) mod load;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub use maestro_git::PathChangeKind;

use crate::editor::{Editor, MouseKind};
use crate::key::{KeyCode, KeyEvent};

/// Rows above the list: the title and what the files are compared with.
pub const HEADER_ROWS: u16 = 2;

/// The file tree's state, kept on [`super::Workspaces`].
#[derive(Debug, Default)]
pub struct FileTree {
    pub open: bool,
    /// Keys go to the tree.
    pub focused: bool,
    /// Set by the front end when the screen is too narrow to draw the
    /// tree: it then takes no keys or clicks.
    pub too_narrow: bool,
    /// The directory shown (canonical): the worktree the editor was in when
    /// the tree last loaded.
    pub root: Option<PathBuf>,
    /// Directory listings, by path relative to `root` (empty for the root).
    pub dirs: HashMap<PathBuf, Listing>,
    /// Directories shown unfolded, relative to `root`.
    pub expanded: HashSet<PathBuf>,
    /// Show only changed files, as a tree of the directories holding them.
    pub changed_only: bool,
    /// In the changed-only view, directories you folded (they start
    /// unfolded there).
    pub folded_changed: HashSet<PathBuf>,
    /// The selected row: its path relative to `root`, and whether it is a
    /// directory (a branch can turn a directory into a file of the same
    /// name, and the tree then lists both).
    pub selected: Option<(PathBuf, bool)>,
    /// The first list row shown.
    pub top: usize,
    pub changes: Changes,
    /// A file's diff, shown over the views (`d`).
    pub diff: Option<diff::DiffView>,
    pub(crate) jobs: load::Jobs,
}

impl FileTree {
    /// Open, shown and taking keys.
    pub fn has_focus(&self) -> bool {
        self.open && self.focused && !self.too_narrow
    }

    /// The diff covers the views and takes the keys.
    pub fn diff_shown(&self) -> bool {
        self.has_focus() && self.diff.is_some()
    }
}

/// One directory's entries, as the background lister found them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listing {
    Loading,
    Ready {
        /// Directories first, then files, each by name.
        entries: Vec<Entry>,
        /// Entries left out past [`load::MAX_ENTRIES`].
        more: usize,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name as the file system has it (not always UTF-8).
    pub name: OsString,
    pub is_dir: bool,
}

/// What the files are compared with, and how they differ.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Changes {
    /// The first comparison has not finished.
    #[default]
    Loading,
    /// No marks, and why: not a Git repository, or Git failed.
    Unavailable(String),
    Ready(Box<Comparison>),
}

impl Changes {
    pub fn ready(&self) -> Option<&Comparison> {
        match self {
            Changes::Ready(c) => Some(c),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// `HEAD`'s branch, `(detached abc1234)` or `(no commits)`.
    pub head: String,
    pub base: Base,
    /// The repository's top directory (Git paths are relative to it).
    pub repo_root: PathBuf,
    /// Changed files by path relative to the tree's root. A rename's
    /// `from` is relative to `repo_root`.
    pub files: BTreeMap<PathBuf, PathChangeKind>,
    /// Changed files at any depth under each directory (relative to the
    /// tree's root; the root itself is the empty path).
    pub dir_counts: HashMap<PathBuf, usize>,
    /// The entries changed paths need in each directory, as (name, is a
    /// directory): deleted files and changed hidden files are not in the
    /// listings, so these are merged in.
    pub children: HashMap<PathBuf, BTreeSet<(OsString, bool)>>,
}

impl Comparison {
    /// Changed files in the tree.
    pub fn count(&self) -> usize {
        self.files.len()
    }

    /// The revision files are compared with, for `git diff`; `None` before
    /// the first commit.
    pub fn rev(&self) -> Option<&str> {
        match &self.base {
            Base::MergeBase { id, .. } => Some(id),
            Base::Head(_) => Some("HEAD"),
            Base::NoCommits => None,
        }
    }

    /// `main (merge-base abc1234)`, `HEAD`...
    pub fn base_label(&self) -> String {
        match &self.base {
            Base::MergeBase { branch, id } => {
                format!("{branch} (merge-base {})", &id[..id.len().min(7)])
            }
            Base::Head(_) => "HEAD".into(),
            Base::NoCommits => "an empty repository".into(),
        }
    }
}

/// The comparison base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// The merge-base of `HEAD` and the default branch.
    MergeBase { branch: String, id: String },
    /// `HEAD` itself: only uncommitted changes show.
    Head(HeadReason),
    /// No commits yet: every file is new.
    NoCommits,
}

/// Why the tree compares with `HEAD` instead of a merge-base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadReason {
    /// `HEAD` is the default branch (named).
    OnDefault(String),
    /// No `origin/HEAD`, `main` or `master`.
    NoDefault,
    /// `HEAD` shares no history with the default branch (named).
    Unrelated(String),
}

impl HeadReason {
    fn text(&self) -> String {
        match self {
            HeadReason::OnDefault(b) => format!("on {b}"),
            HeadReason::NoDefault => "no main or master".into(),
            HeadReason::Unrelated(b) => format!("no history shared with {b}"),
        }
    }
}

/// One row of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Relative to the tree's root.
    pub path: PathBuf,
    pub name: String,
    /// 0 for the root's entries.
    pub depth: usize,
    pub kind: RowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Dir {
        expanded: bool,
        /// Changed files under it.
        changed: usize,
    },
    File {
        change: Option<PathChangeKind>,
    },
    /// A line of information (loading, empty, an error); not selectable.
    Note(String),
}

impl Row {
    /// What the selection remembers: the path and whether it is a
    /// directory. `None` for notes.
    pub fn key(&self) -> Option<(PathBuf, bool)> {
        match self.kind {
            RowKind::Dir { .. } => Some((self.path.clone(), true)),
            RowKind::File { .. } => Some((self.path.clone(), false)),
            RowKind::Note(_) => None,
        }
    }

    pub fn selectable(&self) -> bool {
        !matches!(self.kind, RowKind::Note(_))
    }

    fn note(depth: usize, text: impl Into<String>) -> Row {
        Row {
            path: PathBuf::new(),
            name: String::new(),
            depth,
            kind: RowKind::Note(text.into()),
        }
    }
}

/// The two header lines: the title, and what the files are compared with.
pub fn header(ed: &Editor) -> [String; 2] {
    let t = &ed.workspaces.tree;
    let what = if t.changed_only {
        "changed files"
    } else {
        "files"
    };
    let place = match t.changes.ready() {
        Some(c) => c.head.clone(),
        None => t
            .root
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let title = format!("{what}: {place}");
    let detail = match &t.changes {
        Changes::Loading => "reading changes…".to_string(),
        Changes::Unavailable(why) => format!("no change marks: {why}"),
        Changes::Ready(c) => {
            let n = c.count();
            match &c.base {
                Base::MergeBase { .. } if n == 0 => format!("no changes vs {}", c.base_label()),
                Base::MergeBase { .. } => format!("{n} changed vs {}", c.base_label()),
                Base::Head(why) if n == 0 => {
                    format!("no uncommitted changes ({})", why.text())
                }
                Base::Head(why) => format!("{n} uncommitted vs HEAD ({})", why.text()),
                Base::NoCommits if n == 0 => "no commits and no files yet".into(),
                Base::NoCommits => format!("no commits yet: {n} new"),
            }
        }
    };
    [title, detail]
}

/// The rows to draw, top to bottom.
pub fn rows(ed: &Editor) -> Vec<Row> {
    let t = &ed.workspaces.tree;
    let mut out = Vec::new();
    if t.changed_only {
        match &t.changes {
            Changes::Loading => out.push(Row::note(0, "reading changes…")),
            Changes::Unavailable(why) => out.push(Row::note(0, why.clone())),
            Changes::Ready(c) if c.files.is_empty() => {
                out.push(Row::note(0, format!("no changes vs {}", c.base_label())));
            }
            Changes::Ready(c) => changed_rows(t, c, Path::new(""), 0, &mut out),
        }
        return out;
    }
    dir_rows(t, t.changes.ready(), Path::new(""), 0, &mut out);
    out
}

/// The entries of `dir`: its listing merged with the paths the changes
/// need there, directories first. A name that is a file on disk and a
/// directory in the changes (or the other way round) is listed as both.
fn entries_of(t: &FileTree, cmp: Option<&Comparison>, dir: &Path) -> (Vec<Entry>, Option<Row>) {
    let mut names: BTreeSet<(OsString, bool)> = BTreeSet::new();
    let mut note = None;
    match t.dirs.get(dir) {
        Some(Listing::Ready { entries, more }) => {
            for e in entries {
                names.insert((e.name.clone(), e.is_dir));
            }
            if *more > 0 {
                note = Some(format!("{more} more not shown (space f finds them)"));
            }
        }
        Some(Listing::Failed(e)) => note = Some(e.clone()),
        Some(Listing::Loading) | None => note = Some("loading…".into()),
    }
    if let Some(extra) = cmp.and_then(|c| c.children.get(dir)) {
        names.extend(extra.iter().cloned());
    }
    if names.is_empty() && note.is_none() {
        note = Some("empty".into());
    }
    let mut entries: Vec<Entry> = names
        .into_iter()
        .map(|(name, is_dir)| Entry { name, is_dir })
        .collect();
    sort_entries(&mut entries);
    (entries, note.map(|n| Row::note(0, n)))
}

/// Directories first, then files; by name ignoring case, then exactly.
pub(crate) fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by_cached_key(|e| {
        (
            !e.is_dir,
            e.name.to_string_lossy().to_lowercase(),
            e.name.clone(),
        )
    });
}

fn dir_rows(t: &FileTree, cmp: Option<&Comparison>, dir: &Path, depth: usize, out: &mut Vec<Row>) {
    let (entries, note) = entries_of(t, cmp, dir);
    if let Some(mut n) = note {
        n.depth = depth;
        out.push(n);
    }
    for e in entries {
        let path = dir.join(&e.name);
        let name = e.name.to_string_lossy().into_owned();
        if e.is_dir {
            let expanded = t.expanded.contains(&path);
            out.push(Row {
                name,
                depth,
                kind: RowKind::Dir {
                    expanded,
                    changed: cmp
                        .and_then(|c| c.dir_counts.get(&path))
                        .copied()
                        .unwrap_or(0),
                },
                path: path.clone(),
            });
            if expanded {
                dir_rows(t, cmp, &path, depth + 1, out);
            }
        } else {
            out.push(Row {
                name,
                depth,
                kind: RowKind::File {
                    change: cmp.and_then(|c| c.files.get(&path)).cloned(),
                },
                path,
            });
        }
    }
}

fn changed_rows(t: &FileTree, c: &Comparison, dir: &Path, depth: usize, out: &mut Vec<Row>) {
    let Some(children) = c.children.get(dir) else {
        return;
    };
    let mut entries: Vec<Entry> = children
        .iter()
        .map(|(name, is_dir)| Entry {
            name: name.clone(),
            is_dir: *is_dir,
        })
        .collect();
    sort_entries(&mut entries);
    for e in entries {
        let path = dir.join(&e.name);
        let name = e.name.to_string_lossy().into_owned();
        if e.is_dir {
            let expanded = !t.folded_changed.contains(&path);
            out.push(Row {
                name,
                depth,
                kind: RowKind::Dir {
                    expanded,
                    changed: c.dir_counts.get(&path).copied().unwrap_or(0),
                },
                path: path.clone(),
            });
            if expanded {
                changed_rows(t, c, &path, depth + 1, out);
            }
        } else if let Some(kind) = c.files.get(&path) {
            out.push(Row {
                name,
                depth,
                kind: RowKind::File {
                    change: Some(kind.clone()),
                },
                path,
            });
        }
    }
}

/// The index of the selected row: the remembered one, else the first
/// selectable row.
pub fn selected_index(ed: &Editor, rows: &[Row]) -> Option<usize> {
    let want = ed.workspaces.tree.selected.as_ref();
    want.and_then(|k| rows.iter().position(|r| r.key().as_ref() == Some(k)))
        .or_else(|| rows.iter().position(Row::selectable))
}

/// List rows that fit under the header (and the key line, when there is
/// room for one).
pub fn body_height(ed: &Editor) -> usize {
    let h = ed.region().height;
    usize::from(h.saturating_sub(HEADER_ROWS + u16::from(has_footer(h))))
}

/// A key line under the list when the column has room for it.
pub fn has_footer(height: u16) -> bool {
    height > 6
}

/// The first row to draw: `top`, kept inside the list.
pub fn first_shown(ed: &Editor, rows: usize) -> usize {
    let h = body_height(ed).max(1);
    ed.workspaces.tree.top.min(rows.saturating_sub(h))
}

/// Moves `top` so the selected row is on screen.
fn keep_visible(ed: &mut Editor) {
    let rows = rows(ed);
    let Some(at) = selected_index(ed, &rows) else {
        return;
    };
    let h = body_height(ed).max(1);
    let top = &mut ed.workspaces.tree.top;
    if at < *top {
        *top = at;
    } else if at >= *top + h {
        *top = at + 1 - h;
    }
}

/// Opens and focuses the tree, focuses it, or hides it (`space t`).
pub fn toggle(ed: &mut Editor) {
    let t = &mut ed.workspaces.tree;
    if !t.open {
        t.open = true;
        t.focused = true;
        ed.workspaces.sidebar.focused = false;
        if ed.workspaces.tree.too_narrow {
            ed.set_status(
                crate::editor::Severity::Warning,
                "the window is too narrow to show the file tree ([tree] width, or widen it)",
            );
        }
        if !follow_root(ed) {
            load::refresh(ed);
        }
    } else if !t.focused {
        t.focused = true;
        ed.workspaces.sidebar.focused = false;
    } else {
        hide(ed);
    }
}

fn hide(ed: &mut Editor) {
    let t = &mut ed.workspaces.tree;
    t.open = false;
    t.focused = false;
    t.diff = None;
}

/// Gives the keys back to the editor; the tree stays shown.
pub(crate) fn blur(ed: &mut Editor) {
    let t = &mut ed.workspaces.tree;
    t.focused = false;
    t.diff = None;
}

/// Starts over in the current worktree when the editor moved to another
/// one (a switch, `:cd`). Returns true when it did.
pub(crate) fn follow_root(ed: &mut Editor) -> bool {
    if !ed.workspaces.tree.open {
        return false;
    }
    // Spelled as the editor has it; only a new spelling is resolved
    // through the file system, so a tick touches no disk.
    let source = super::current_root(ed);
    if ed.workspaces.tree.jobs.source.as_ref() == Some(&source) {
        return false;
    }
    let root = crate::editor::normalize_path(&source);
    ed.workspaces.tree.jobs.source = Some(source);
    if ed.workspaces.tree.root.as_deref() == Some(root.as_path()) {
        return false;
    }
    load::reset(ed, root);
    true
}

/// Periodic work while the tree is open: follow a worktree switch, read
/// the changes again after a save and every few seconds. Called from
/// [`super::tick`].
pub(crate) fn tick(ed: &mut Editor, now: Instant) {
    if !ed.workspaces.tree.open {
        return;
    }
    if follow_root(ed) {
        return;
    }
    let saved = load::noticed_save(ed);
    let due = ed
        .workspaces
        .tree
        .jobs
        .last_refresh
        .is_none_or(|t| now.saturating_duration_since(t) >= super::scan::REFRESH_EVERY);
    if saved || due {
        load::refresh(ed);
    }
}

/// Handles a key while the tree has focus. Returns false for keys that
/// belong to the normal keymap (`space`).
pub fn handle_key(ed: &mut Editor, key: KeyEvent) -> bool {
    if key == KeyEvent::char(' ') {
        return false;
    }
    if ed.workspaces.tree.diff.is_some() {
        diff::handle_key(ed, key);
        return true;
    }
    let rows = rows(ed);
    let at = selected_index(ed, &rows);
    let selectable: Vec<usize> = (0..rows.len()).filter(|i| rows[*i].selectable()).collect();
    let pos = at.and_then(|i| selectable.iter().position(|s| *s == i));
    let select = |ed: &mut Editor, i: Option<usize>| {
        if let Some(i) = i {
            ed.workspaces.tree.selected = rows[i].key();
        }
    };
    let step = |delta: isize| -> Option<usize> {
        let last = selectable.len().checked_sub(1)?;
        let p = pos
            .map_or(0, |p| p as isize + delta)
            .clamp(0, last as isize) as usize;
        selectable.get(p).copied()
    };
    let half = (body_height(ed) / 2).max(1) as isize;
    let row = at.map(|i| rows[i].clone());
    match (key.code, key.mods.ctrl) {
        (KeyCode::Esc, _) => blur(ed),
        (KeyCode::Char('q'), false) => hide(ed),
        (KeyCode::Char('j') | KeyCode::Down, false) => select(ed, step(1)),
        (KeyCode::Char('k') | KeyCode::Up, false) => select(ed, step(-1)),
        (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => select(ed, step(half)),
        (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => select(ed, step(-half)),
        (KeyCode::Char('g') | KeyCode::Home, false) => select(ed, selectable.first().copied()),
        (KeyCode::Char('G') | KeyCode::End, false) => select(ed, selectable.last().copied()),
        (KeyCode::Char('r'), false) => {
            load::refresh(ed);
            ed.info("reading the file tree and its changes again");
        }
        (KeyCode::Char('c'), false) => {
            let t = &mut ed.workspaces.tree;
            t.changed_only = !t.changed_only;
            let on = t.changed_only;
            ed.info(if on {
                "showing changed files only (c shows every file)"
            } else {
                "showing every file (c shows changed files only)"
            });
        }
        (KeyCode::Char('d'), false) => match &row {
            Some(r) => diff::open(ed, r),
            None => ed.info("nothing selected"),
        },
        (KeyCode::Enter, _) => {
            if let Some(r) = &row {
                activate(ed, r);
            }
        }
        (KeyCode::Char('l') | KeyCode::Right, false) => {
            if let Some(Row {
                path,
                kind: RowKind::Dir { expanded, .. },
                ..
            }) = &row
            {
                if *expanded {
                    select(ed, step(1));
                } else {
                    set_expanded(ed, path, true);
                }
            }
        }
        (KeyCode::Char('h') | KeyCode::Left, false) => match &row {
            Some(Row {
                path,
                kind: RowKind::Dir { expanded: true, .. },
                ..
            }) => set_expanded(ed, path, false),
            Some(r) => {
                if let Some(parent) = r.path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    let parent = parent.to_path_buf();
                    set_expanded(ed, &parent, false);
                    ed.workspaces.tree.selected = Some((parent, true));
                }
            }
            None => {}
        },
        _ => ed.info(
            "file tree: j/k move, ret open, h/l fold, c changed only, d diff, r refresh, esc editor, q hide",
        ),
    }
    keep_visible(ed);
    true
}

/// `ret` or a click on a row: fold or unfold a directory, open a file.
fn activate(ed: &mut Editor, row: &Row) {
    match &row.kind {
        RowKind::Dir { expanded, .. } => {
            let path = row.path.clone();
            set_expanded(ed, &path, !expanded);
        }
        RowKind::File { change } => {
            if *change == Some(PathChangeKind::Deleted) {
                ed.info(format!(
                    "{} is deleted on this branch; d shows what it had",
                    row.path.display()
                ));
                return;
            }
            let Some(root) = ed.workspaces.tree.root.clone() else {
                return;
            };
            match ed.open(&root.join(&row.path)) {
                Ok(_) => {
                    blur(ed);
                    say_if_covered(ed, &row.path);
                }
                Err(e) => ed.error(e),
            }
        }
        RowKind::Note(_) => {}
    }
}

/// After opening `rel` from the tree: when a full screen covers the views,
/// the file opened behind it, so say so. The screen is left as it is (it
/// may hold a dialog or a half-written prompt).
pub(crate) fn say_if_covered(ed: &mut Editor, rel: &Path) {
    let screen = if ed.forge.screen_open() {
        "GitHub inbox"
    } else if ed.git.screen_open() {
        "Git screen"
    } else if ed.proposals.review_open() {
        "proposal review"
    } else if ed.agents.panel_open() {
        "agent panel"
    } else {
        return;
    };
    ed.set_status(
        crate::editor::Severity::Warning,
        format!(
            "opened {} behind the {screen}; close the {screen} to see it",
            rel.display()
        ),
    );
}

/// Folds or unfolds `dir`, listing it in the background the first time.
fn set_expanded(ed: &mut Editor, dir: &Path, on: bool) {
    let t = &mut ed.workspaces.tree;
    if t.changed_only {
        if on {
            t.folded_changed.remove(dir);
        } else {
            t.folded_changed.insert(dir.to_path_buf());
        }
        return;
    }
    if on {
        t.expanded.insert(dir.to_path_buf());
        if !t.dirs.contains_key(dir) {
            load::list(ed, vec![dir.to_path_buf()]);
        }
    } else {
        t.expanded.remove(dir);
    }
}

/// A mouse event at screen cell (x, y). Returns true when the tree (or its
/// diff over the views) took it. A click elsewhere gives the keys back to
/// the editor and returns false, so the views handle it.
pub fn handle_mouse(ed: &mut Editor, kind: MouseKind, x: u16, y: u16) -> bool {
    let t = &ed.workspaces.tree;
    if !t.open || t.too_narrow {
        return false;
    }
    if t.diff_shown() && ed.area.contains(x, y) {
        diff::handle_mouse(ed, kind, y);
        return true;
    }
    // The tree is the column right of the views; its divider is the first
    // column past them. It is as tall as the views and the shell pane.
    let area = ed.region();
    let inside = x >= area.right() && y >= area.y && y < area.bottom();
    if !inside {
        if kind == MouseKind::Down && t.focused {
            blur(ed);
        }
        return false;
    }
    let rows = rows(ed);
    match kind {
        MouseKind::ScrollUp | MouseKind::ScrollDown => {
            let max = rows.len().saturating_sub(body_height(ed).max(1));
            let top = &mut ed.workspaces.tree.top;
            *top = if kind == MouseKind::ScrollUp {
                top.saturating_sub(3)
            } else {
                (*top + 3).min(max)
            }
            .min(max);
        }
        MouseKind::Drag => {}
        MouseKind::Down => {
            let t = &mut ed.workspaces.tree;
            t.focused = true;
            t.diff = None;
            ed.workspaces.sidebar.focused = false;
            let body = y - area.y;
            if body < HEADER_ROWS {
                return true;
            }
            let n = usize::from(body - HEADER_ROWS);
            if n >= body_height(ed) {
                return true;
            }
            let i = first_shown(ed, rows.len()) + n;
            if let Some(row) = rows.get(i).filter(|r| r.selectable()) {
                ed.workspaces.tree.selected = row.key();
                activate(ed, row);
            }
        }
    }
    true
}
