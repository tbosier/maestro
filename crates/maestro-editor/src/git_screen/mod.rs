//! The Git status screen (`space g`, `:git`): a keyboard view of the
//! repository in the style of Magit, covering the editor area.
//!
//! A head line (branch, upstream, ahead and behind, any operation in
//! progress) sits above sections for untracked, unstaged and staged files,
//! stashes and recent commits. Tab opens a file's hunks inline with changed
//! words marked; `s`, `u` and `x` stage, unstage and discard a file, a hunk
//! or the lines selected with `v`. Every Git call runs on a background
//! thread; the screen shows what it is waiting for.
//!
//! Hunk and line actions build a patch from the diff the screen shows and
//! check, before applying it, that the file's diff is still byte for byte
//! what was drawn. Partial staging applies to the index only, so the file
//! on disk is never written.

mod actions;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use maestro_git::{
    CommitDetail, DiffLineKind, FileChange, FilePatch, LogCommit, LogEntry, Operation, Repository,
    Stash,
};

use crate::agents::review::{self, RowLine};
use crate::editor::{DocumentId, Editor};
use crate::key::{KeyCode, KeyEvent};

pub use actions::{SCISSORS, check_commit_message, clean_message};

crate::commands! {
    git_screen => "Open the Git status screen: stage, commit, branch, stash, fetch, pull, push, log",
}

/// Default keys, bound by `keymap.rs` under the space group.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[("space g", "git_screen")];

static NEXT_SCREEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Untracked,
    Unstaged,
    Staged,
    Stashes,
    Recent,
}

impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Section::Untracked => "Untracked",
            Section::Unstaged => "Unstaged",
            Section::Staged => "Staged",
            Section::Stashes => "Stashes",
            Section::Recent => "Recent commits",
        }
    }
}

/// The repository facts the screen shows, read on a background thread.
#[derive(Debug, Clone, Default)]
pub struct Data {
    /// The branch, or `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// The abbreviated `HEAD` commit, `None` before the first commit.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub upstream_gone: bool,
    pub operation: Option<Operation>,
    pub untracked: Vec<PathBuf>,
    pub unstaged: Vec<FilePatch>,
    pub staged: Vec<FilePatch>,
    pub stashes: Vec<Stash>,
    pub recent: Vec<LogCommit>,
}

impl Data {
    pub fn files(&self, section: Section) -> &[FilePatch] {
        match section {
            Section::Unstaged => &self.unstaged,
            Section::Staged => &self.staged,
            _ => &[],
        }
    }

    pub fn head_subject(&self) -> Option<&str> {
        self.recent.first().map(|c| c.subject.as_str())
    }
}

/// One row of the status page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A head line: label and value; `warn` marks an operation in progress.
    Head {
        label: &'static str,
        value: String,
        warn: bool,
    },
    Blank,
    Section {
        section: Section,
        count: usize,
    },
    Untracked(usize),
    File {
        section: Section,
        file: usize,
    },
    Note(String),
    Hunk {
        section: Section,
        file: usize,
        hunk: usize,
        header: String,
    },
    Line {
        section: Section,
        file: usize,
        hunk: usize,
        line: usize,
        row: RowLine,
    },
    Stash(usize),
    Commit(usize),
}

/// A page over the status page: the log, or one commit.
#[derive(Debug, Clone)]
pub enum Page {
    Log {
        entries: Vec<LogEntry>,
        cursor: usize,
        top: usize,
    },
    Commit {
        detail: Box<CommitDetail>,
        rows: Vec<DetailRow>,
        cursor: usize,
        top: usize,
    },
}

/// A row of the commit page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailRow {
    Text(String),
    File(String),
    Note(String),
    Hunk(String),
    Line(RowLine),
}

/// What a confirmation dialog does when accepted with `y`.
#[derive(Debug, Clone)]
pub enum Pending {
    Discard(actions::Discard),
    /// Amend after being told `HEAD` is already on a remote.
    Amend(Prepared),
    Remote(maestro_git::RemoteCommand),
}

#[derive(Debug, Clone)]
pub struct Dialog {
    pub title: String,
    pub lines: Vec<String>,
    pub keys: Vec<(String, String)>,
    pub pending: Pending,
}

/// The branch picker (`b`).
#[derive(Debug, Clone, Default)]
pub struct BranchPicker {
    pub query: String,
    pub branches: Vec<maestro_git::Branch>,
    /// Indices into `branches` matching the query, best first.
    pub matches: Vec<usize>,
    /// Index into the shown items: the matches, then "create" when offered.
    pub selected: usize,
}

impl BranchPicker {
    fn refilter(&mut self) {
        let mut scored: Vec<(i64, usize)> = self
            .branches
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                if self.query.is_empty() {
                    Some((0, i))
                } else {
                    crate::picker::fuzzy_match(&self.query, &b.name).map(|(s, _)| (s, i))
                }
            })
            .collect();
        scored.sort_by_key(|(s, i)| (std::cmp::Reverse(*s), *i));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = self.selected.min(self.items().saturating_sub(1));
    }

    /// True when the last item creates a branch named by the query.
    pub fn offers_create(&self) -> bool {
        let q = self.query.trim();
        !q.is_empty() && !self.branches.iter().any(|b| b.name == q)
    }

    pub fn items(&self) -> usize {
        self.matches.len() + usize::from(self.offers_create())
    }
}

/// The status screen's state.
#[derive(Debug, Clone)]
pub struct Screen {
    pub id: u64,
    /// Where the repository is looked for.
    pub start: PathBuf,
    pub repo: Option<Repository>,
    pub data: Option<Data>,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub top: usize,
    /// The other end of a line selection (`v`), a row index.
    pub mark: Option<usize>,
    /// Files open to their hunks, by section and path.
    pub expanded: HashSet<(Section, PathBuf)>,
    pub collapsed: HashSet<Section>,
    pub loading: bool,
    load_seq: u64,
    /// The last load error, shown instead of the sections.
    pub error: Option<String>,
    /// The Git command running, shown in the header.
    pub busy: Option<String>,
    /// A key that waits for the next (`c` for commit).
    pub prefix: Option<char>,
    pub dialog: Option<Dialog>,
    pub branches: Option<BranchPicker>,
    pub pages: Vec<Page>,
    placed: bool,
}

/// A commit message being written in a buffer.
#[derive(Debug, Clone)]
pub struct CommitMessage {
    pub doc: DocumentId,
    /// A file of this message's own in the Git directory.
    pub path: PathBuf,
    pub repo: Repository,
    pub amend: bool,
    /// The file as written before the buffer opened: a commit needs a save
    /// after that.
    pub written: Option<maestro_core::io::DiskState>,
    /// Whether the buffer had unsaved edits when last seen, for a buffer
    /// closed by force.
    pub dirty: bool,
    /// `HEAD`, its branch and the operation in progress when the message
    /// was opened; the commit is refused if any of them changed.
    pub prepared: Prepared,
}

/// What a commit message is written against, read when it is opened.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    /// The amended commit's message (empty for a new commit).
    pub message: String,
    pub stat: String,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub operation: Option<Operation>,
}

fn git_screen(ed: &mut Editor) {
    let start = crate::git::start_dir(ed);
    open_at(ed, start);
}

/// Opens the status screen for the repository holding `start`.
pub fn open_at(ed: &mut Editor, start: PathBuf) {
    ed.git.screen = Some(Screen {
        id: NEXT_SCREEN.fetch_add(1, Ordering::Relaxed),
        start,
        repo: None,
        data: None,
        rows: Vec::new(),
        cursor: 0,
        top: 0,
        mark: None,
        expanded: HashSet::new(),
        collapsed: HashSet::new(),
        loading: true,
        load_seq: 0,
        error: None,
        busy: None,
        prefix: None,
        dialog: None,
        branches: None,
        pages: Vec::new(),
        placed: false,
    });
    reload(ed);
}

/// Reads everything the status page shows, off the UI thread.
pub fn load(repo: &Repository) -> Result<Data, String> {
    use maestro_git::DiffSpec;
    let e = |e: maestro_git::GitError| e.to_string();
    let status = repo.status().map_err(e)?;
    let has_head = repo.has_head().map_err(e)?;
    let head = if has_head {
        let id = repo.resolve("HEAD").map_err(e)?;
        Some(id[..id.len().min(7)].to_string())
    } else {
        None
    };
    Ok(Data {
        branch: status.branch.head.clone(),
        head,
        upstream: status.branch.upstream.clone(),
        ahead: status.branch.ahead,
        behind: status.branch.behind,
        upstream_gone: status.branch.upstream_gone,
        operation: repo.operation().map_err(e)?,
        untracked: status.untracked().map(|u| u.path.clone()).collect(),
        unstaged: repo.file_patches(&DiffSpec::Unstaged, &[]).map_err(e)?,
        staged: repo.file_patches(&DiffSpec::Staged, &[]).map_err(e)?,
        stashes: repo.stashes().map_err(e)?,
        recent: repo
            .log(10, false)
            .map_err(e)?
            .into_iter()
            .filter_map(|l| l.commit)
            .collect(),
    })
}

/// Reloads the screen's data in the background.
pub fn reload(ed: &mut Editor) {
    let git = ed.git.git().clone();
    let Some(sc) = &mut ed.git.screen else {
        return;
    };
    sc.load_seq += 1;
    sc.loading = true;
    let (id, seq, start) = (sc.id, sc.load_seq, sc.start.clone());
    let spawned = ed.spawn_background("git-screen", move |tx| {
        let result = crate::git::find_repo(&git, &start)
            .and_then(|repo| load(&repo).map(|data| (repo, data)));
        tx.send(move |ed| {
            let Some(sc) = &mut ed.git.screen else {
                return;
            };
            if sc.id != id || sc.load_seq != seq {
                return;
            }
            sc.loading = false;
            match result {
                Ok((repo, data)) => {
                    sc.repo = Some(repo);
                    sc.data = Some(data);
                    sc.error = None;
                }
                Err(e) => {
                    sc.error = Some(e);
                    sc.data = None;
                }
            }
            sc.mark = None;
            sc.rebuild();
        });
    });
    if let Err(e) = spawned {
        if let Some(sc) = &mut ed.git.screen {
            sc.loading = false;
        }
        ed.error(format!("could not start git: {e}"));
    }
}

fn change_word(f: &FilePatch) -> String {
    let base = match &f.file.change {
        FileChange::Added => "new file",
        FileChange::Deleted => "deleted",
        FileChange::Modified => "modified",
        FileChange::Renamed { .. } => "renamed",
        FileChange::Copied { .. } => "copied",
    };
    if f.file.unmerged {
        "conflict".into()
    } else if f.file.binary {
        format!("{base} (binary)")
    } else if f.file.old_mode.is_some()
        && f.file.new_mode.is_some()
        && f.file.old_mode != f.file.new_mode
        && f.file.hunks.is_empty()
    {
        "mode changed".into()
    } else {
        base.into()
    }
}

/// `modified   src/a.rs`, or `renamed    old -> new`.
pub fn file_label(f: &FilePatch) -> String {
    let path = match (&f.file.old_path, &f.file.new_path) {
        (Some(o), Some(n)) if o != n => format!("{} -> {}", o.display(), n.display()),
        _ => f
            .file
            .path()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    };
    format!("{:<11}{path}", change_word(f))
}

impl Screen {
    /// Rows from the data, the expanded files and the folded sections.
    pub fn rebuild(&mut self) {
        let mut rows = Vec::new();
        if let Some(e) = &self.error {
            rows.push(Row::Note(format!("error: {e}")));
            self.rows = rows;
            self.cursor = 0;
            return;
        }
        let Some(d) = &self.data else {
            self.rows = rows;
            return;
        };
        let head = match (&d.branch, &d.head) {
            (Some(b), Some(_)) => format!("{b}  {}", d.head_subject().unwrap_or_default()),
            (Some(b), None) => format!("{b}  (no commits yet)"),
            (None, Some(h)) => format!(
                "(detached at {h})  {}",
                d.head_subject().unwrap_or_default()
            ),
            (None, None) => "(detached)".into(),
        };
        rows.push(Row::Head {
            label: "Head",
            value: head,
            warn: false,
        });
        let upstream = match &d.upstream {
            None => "none".to_string(),
            Some(u) if d.upstream_gone => format!("{u} (gone from the remote)"),
            Some(u) => format!("{u}  ahead {}, behind {}", d.ahead, d.behind),
        };
        rows.push(Row::Head {
            label: "Upstream",
            value: upstream,
            warn: false,
        });
        if let Some(op) = d.operation {
            rows.push(Row::Head {
                label: "In progress",
                value: format!("{}: finish with {}", op.label(), op.way_out()),
                warn: true,
            });
        }
        let clean = d.untracked.is_empty() && d.unstaged.is_empty() && d.staged.is_empty();
        if clean {
            rows.push(Row::Blank);
            rows.push(Row::Note("nothing to commit, working tree clean".into()));
        }
        let section = |rows: &mut Vec<Row>, s: Section, count: usize| -> bool {
            if count == 0 {
                return false;
            }
            rows.push(Row::Blank);
            rows.push(Row::Section { section: s, count });
            !self.collapsed.contains(&s)
        };
        if section(&mut rows, Section::Untracked, d.untracked.len()) {
            rows.extend((0..d.untracked.len()).map(Row::Untracked));
        }
        for s in [Section::Unstaged, Section::Staged] {
            let files = d.files(s);
            if !section(&mut rows, s, files.len()) {
                continue;
            }
            for (i, f) in files.iter().enumerate() {
                rows.push(Row::File {
                    section: s,
                    file: i,
                });
                let key = f.target_path().cloned().unwrap_or_default();
                if self.expanded.contains(&(s, key)) {
                    push_file_rows(&mut rows, s, i, f);
                }
            }
        }
        if section(&mut rows, Section::Stashes, d.stashes.len()) {
            rows.extend((0..d.stashes.len()).map(Row::Stash));
        }
        if d.recent.is_empty() {
            rows.push(Row::Blank);
            rows.push(Row::Note("no commits yet".into()));
        } else if section(&mut rows, Section::Recent, d.recent.len()) {
            rows.extend((0..d.recent.len()).map(Row::Commit));
        }
        self.rows = rows;
        if !self.placed {
            // Start on the first file, as Magit does.
            self.placed = true;
            self.cursor = self
                .rows
                .iter()
                .position(|r| matches!(r, Row::Untracked(_) | Row::File { .. }))
                .unwrap_or(0);
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        while self.cursor > 0 && self.rows.get(self.cursor) == Some(&Row::Blank) {
            self.cursor -= 1;
        }
    }

    /// The file a row belongs to, with its section.
    pub fn file_of(&self, row: usize) -> Option<(Section, usize)> {
        match self.rows.get(row)? {
            Row::File { section, file }
            | Row::Hunk { section, file, .. }
            | Row::Line { section, file, .. } => Some((*section, *file)),
            _ => None,
        }
    }

    pub fn file(&self, section: Section, file: usize) -> Option<&FilePatch> {
        self.data.as_ref()?.files(section).get(file)
    }

    /// The selected line rows, as row indices, when a selection is active.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let m = self.mark?;
        Some((m.min(self.cursor), m.max(self.cursor)))
    }
}

/// A file's hunks and lines as rows, using the review screen's layout and
/// intraline marks.
fn push_file_rows(rows: &mut Vec<Row>, section: Section, file: usize, f: &FilePatch) {
    let mut hunk = 0;
    let mut line = 0;
    let mut first = true;
    for r in review::rows(&f.file, false, &[]) {
        match r {
            review::Row::Hunk { index, header } => {
                hunk = index;
                line = 0;
                first = false;
                rows.push(Row::Hunk {
                    section,
                    file,
                    hunk,
                    header,
                });
            }
            review::Row::Line(row) if !first => {
                rows.push(Row::Line {
                    section,
                    file,
                    hunk,
                    line,
                    row,
                });
                line += 1;
            }
            review::Row::Note(t) => rows.push(Row::Note(t)),
            _ => {}
        }
    }
}

// ----- keys -----------------------------------------------------------

/// Rows of the page's content area for an editor area of `height` rows: a
/// header and a hint line take two.
pub fn content_height(height: u16) -> usize {
    height.saturating_sub(2).max(1) as usize
}

fn scroll_into_view(cursor: usize, top: &mut usize, height: usize) {
    if cursor < *top {
        *top = cursor;
    } else if cursor >= *top + height {
        *top = cursor + 1 - height;
    }
}

pub fn handle_key(ed: &mut Editor, key: KeyEvent) {
    let height = content_height(ed.area.height);
    let Some(sc) = &mut ed.git.screen else {
        return;
    };
    if sc.dialog.is_some() {
        actions::dialog_key(ed, key);
        return;
    }
    if sc.branches.is_some() {
        actions::branch_key(ed, key);
        return;
    }
    if !sc.pages.is_empty() {
        page_key(ed, key, height);
        return;
    }
    let ch = key.typed_char();
    if let Some(p) = sc.prefix.take() {
        match (p, ch) {
            ('c', Some('c')) => actions::commit(ed, false),
            ('c', Some('a')) => actions::commit(ed, true),
            _ => ed.info("c c commits, c a amends"),
        }
        return;
    }
    let ctrl = key.mods.ctrl;
    match (key.code, ch) {
        (KeyCode::Esc, _) if sc.mark.is_some() => sc.mark = None,
        (KeyCode::Esc, _) | (_, Some('q')) => {
            ed.git.screen = None;
        }
        (KeyCode::Down, _) | (_, Some('j')) => move_cursor(sc, 1, height),
        (KeyCode::Up, _) | (_, Some('k')) => move_cursor(sc, -1, height),
        (KeyCode::Char('d'), _) if ctrl => move_cursor(sc, (height / 2) as isize, height),
        (KeyCode::Char('u'), _) if ctrl => move_cursor(sc, -((height / 2) as isize), height),
        (KeyCode::PageDown, _) => move_cursor(sc, height as isize, height),
        (KeyCode::PageUp, _) => move_cursor(sc, -(height as isize), height),
        (_, Some('g')) | (KeyCode::Home, _) => move_cursor(sc, isize::MIN / 2, height),
        (_, Some('G')) | (KeyCode::End, _) => move_cursor(sc, isize::MAX / 2, height),
        (KeyCode::Tab, _) => toggle(sc, height),
        (_, Some('v')) => {
            if sc.mark.is_some() {
                sc.mark = None;
            } else if matches!(sc.rows.get(sc.cursor), Some(Row::Line { .. })) {
                sc.mark = Some(sc.cursor);
                ed.info("selecting lines: j/k extend, s/u/x act on them, v or Escape stops");
            } else {
                ed.info("v selects lines inside an open hunk (Tab opens a file)");
            }
        }
        (KeyCode::Enter, _) => enter(ed),
        (_, Some('r')) => reload(ed),
        (_, Some('s')) => actions::stage(ed),
        (_, Some('u')) => actions::unstage(ed),
        (_, Some('x')) => actions::discard(ed),
        (_, Some('c')) => {
            sc.prefix = Some('c');
            ed.info("c: c commit, a amend");
        }
        (_, Some('b')) => actions::branch(ed),
        (_, Some('z')) => actions::stash(ed),
        (_, Some('Z')) => actions::pop(ed),
        (_, Some('f')) => actions::remote(ed, maestro_git::RemoteOp::Fetch),
        (_, Some('F')) => actions::remote(ed, maestro_git::RemoteOp::Pull),
        (_, Some('P')) => actions::remote(ed, maestro_git::RemoteOp::Push),
        (_, Some('l')) => actions::log(ed),
        _ => {}
    }
}

fn move_cursor(sc: &mut Screen, delta: isize, height: usize) {
    if sc.rows.is_empty() {
        return;
    }
    let last = sc.rows.len() - 1;
    let mut c = (sc.cursor as isize)
        .saturating_add(delta)
        .clamp(0, last as isize) as usize;
    // Never rest on a blank row.
    let step: isize = if delta < 0 { -1 } else { 1 };
    while sc.rows[c] == Row::Blank {
        let next = c as isize + step;
        if next < 0 || next > last as isize {
            break;
        }
        c = next as usize;
    }
    if sc.rows[c] == Row::Blank {
        return;
    }
    sc.cursor = c;
    scroll_into_view(sc.cursor, &mut sc.top, height);
}

/// Tab: open or close a file's hunks, or fold a section.
fn toggle(sc: &mut Screen, height: usize) {
    match sc.rows.get(sc.cursor).cloned() {
        Some(Row::Section { section, .. }) => {
            if !sc.collapsed.remove(&section) {
                sc.collapsed.insert(section);
            }
        }
        Some(Row::File { section, file }) => {
            let key = (
                section,
                sc.file(section, file)
                    .and_then(|f| f.target_path().cloned())
                    .unwrap_or_default(),
            );
            if !sc.expanded.remove(&key) {
                sc.expanded.insert(key);
            }
        }
        Some(Row::Hunk { section, file, .. } | Row::Line { section, file, .. }) => {
            let key = (
                section,
                sc.file(section, file)
                    .and_then(|f| f.target_path().cloned())
                    .unwrap_or_default(),
            );
            sc.expanded.remove(&key);
            // Back to the file's row.
            if let Some(i) = sc
                .rows
                .iter()
                .position(|r| *r == Row::File { section, file })
            {
                sc.cursor = i;
            }
        }
        _ => return,
    }
    sc.mark = None;
    sc.rebuild();
    scroll_into_view(sc.cursor, &mut sc.top, height);
}

/// Enter: open a file (at the line under the cursor), a stash or a commit.
fn enter(ed: &mut Editor) {
    let Some(sc) = &ed.git.screen else {
        return;
    };
    let Some(root) = sc.repo.as_ref().map(|r| r.root().to_path_buf()) else {
        return;
    };
    let row = sc.rows.get(sc.cursor).cloned();
    let (path, line): (Option<PathBuf>, usize) = match &row {
        Some(Row::Untracked(i)) => (
            sc.data.as_ref().and_then(|d| d.untracked.get(*i)).cloned(),
            1,
        ),
        Some(Row::File { section, file }) => (
            sc.file(*section, *file)
                .and_then(|f| f.target_path().cloned()),
            1,
        ),
        Some(Row::Hunk {
            section,
            file,
            hunk,
            ..
        }) => {
            let f = sc.file(*section, *file);
            (
                f.and_then(|f| f.target_path().cloned()),
                f.and_then(|f| f.file.hunks.get(*hunk))
                    .map_or(1, |h| h.new_start.max(1)),
            )
        }
        Some(Row::Line {
            section, file, row, ..
        }) => (
            sc.file(*section, *file)
                .and_then(|f| f.target_path().cloned()),
            row.anchor.max(1),
        ),
        Some(Row::Stash(i)) => {
            let id = sc
                .data
                .as_ref()
                .and_then(|d| d.stashes.get(*i))
                .map(|s| s.id.clone());
            if let Some(id) = id {
                actions::show_commit(ed, id);
            }
            return;
        }
        Some(Row::Commit(i)) => {
            let id = sc
                .data
                .as_ref()
                .and_then(|d| d.recent.get(*i))
                .map(|c| c.id.clone());
            if let Some(id) = id {
                actions::show_commit(ed, id);
            }
            return;
        }
        _ => (None, 1),
    };
    let Some(rel) = path else {
        return;
    };
    open_file(ed, &root.join(rel), line);
}

/// Closes the screen and opens `path` at one-based `line`.
fn open_file(ed: &mut Editor, path: &Path, line: usize) {
    ed.git.screen = None;
    if !path.exists() {
        ed.error(format!(
            "{} does not exist in the working tree",
            path.display()
        ));
        return;
    }
    if let Err(e) = ed.open(path) {
        ed.error(e);
        return;
    }
    let text = ed.doc().text().clone();
    let l = line
        .saturating_sub(1)
        .min(text.len_lines().saturating_sub(1));
    ed.push_jump();
    ed.set_selection(maestro_core::Selection::point(text.line_to_char(l)));
    crate::commands::align_view_center(ed);
}

/// Keys on the log and commit pages.
fn page_key(ed: &mut Editor, key: KeyEvent, height: usize) {
    let Some(sc) = &mut ed.git.screen else {
        return;
    };
    let ch = key.typed_char();
    let ctrl = key.mods.ctrl;
    if key.code == KeyCode::Esc || ch == Some('q') {
        sc.pages.pop();
        return;
    }
    let Some(page) = sc.pages.last_mut() else {
        return;
    };
    let (cursor, top, len) = match page {
        Page::Log {
            entries,
            cursor,
            top,
        } => (cursor, top, entries.len()),
        Page::Commit {
            rows, cursor, top, ..
        } => (cursor, top, rows.len()),
    };
    let delta: isize = match (key.code, ch) {
        (KeyCode::Down, _) | (_, Some('j')) => 1,
        (KeyCode::Up, _) | (_, Some('k')) => -1,
        (KeyCode::Char('d'), _) if ctrl => (height / 2) as isize,
        (KeyCode::Char('u'), _) if ctrl => -((height / 2) as isize),
        (KeyCode::PageDown, _) => height as isize,
        (KeyCode::PageUp, _) => -(height as isize),
        (_, Some('g')) | (KeyCode::Home, _) => isize::MIN / 2,
        (_, Some('G')) | (KeyCode::End, _) => isize::MAX / 2,
        (KeyCode::Enter, _) => {
            if let Page::Log {
                entries, cursor, ..
            } = page
            {
                // Enter on a graph-only row opens the commit above it.
                let id = entries[..=(*cursor).min(entries.len().saturating_sub(1))]
                    .iter()
                    .rev()
                    .find_map(|e| e.commit.as_ref().map(|c| c.id.clone()));
                if let Some(id) = id {
                    actions::show_commit(ed, id);
                }
            }
            return;
        }
        _ => return,
    };
    if len == 0 {
        return;
    }
    *cursor = (*cursor as isize)
        .saturating_add(delta)
        .clamp(0, len as isize - 1) as usize;
    scroll_into_view(*cursor, top, height);
}

/// Key hints for the hint line, for the page that is showing.
pub fn hints(sc: &Screen) -> Vec<(&'static str, &'static str)> {
    if sc.dialog.is_some() {
        return vec![("y", "confirm"), ("n", "cancel")];
    }
    if sc.branches.is_some() {
        return vec![
            ("type", "filter or name"),
            ("ret", "switch or create"),
            ("esc", "cancel"),
        ];
    }
    match sc.pages.last() {
        Some(Page::Log { .. }) => vec![("j/k", "move"), ("ret", "show commit"), ("q", "back")],
        Some(Page::Commit { .. }) => vec![("j/k", "move"), ("q", "back")],
        None if sc.mark.is_some() => vec![
            ("j/k", "extend"),
            ("s", "stage lines"),
            ("u", "unstage lines"),
            ("x", "discard lines"),
            ("esc", "stop selecting"),
        ],
        // Most used first: a narrow window drops the end of the list.
        None => vec![
            ("tab", "hunks"),
            ("s", "stage"),
            ("u", "unstage"),
            ("x", "discard"),
            ("v", "lines"),
            ("cc", "commit"),
            ("ca", "amend"),
            ("f/F/P", "fetch/pull/push"),
            ("l", "log"),
            ("q", "close"),
            ("b", "branch"),
            ("z/Z", "stash/pop"),
            ("r", "refresh"),
        ],
    }
}

/// The rows of a commit page.
pub fn detail_rows(d: &CommitDetail) -> Vec<DetailRow> {
    let mut rows = vec![
        DetailRow::Text(format!("commit {}", d.id)),
        DetailRow::Text(format!("Author: {}", d.author)),
        DetailRow::Text(format!("Date:   {}", d.date)),
    ];
    if d.parents.len() > 1 {
        rows.push(DetailRow::Text(format!(
            "Merge:  {} (diff against the first parent)",
            d.parents
                .iter()
                .map(|p| &p[..p.len().min(7)])
                .collect::<Vec<_>>()
                .join(" ")
        )));
    }
    rows.push(DetailRow::Text(String::new()));
    rows.extend(
        d.message
            .lines()
            .map(|l| DetailRow::Text(format!("    {l}"))),
    );
    rows.push(DetailRow::Text(String::new()));
    if d.files.is_empty() {
        rows.push(DetailRow::Note("no file changes".into()));
    }
    for f in &d.files {
        rows.push(DetailRow::File(file_label(f)));
        for r in review::rows(&f.file, false, &[]) {
            match r {
                review::Row::Hunk { header, .. } => rows.push(DetailRow::Hunk(header)),
                review::Row::Line(l) => rows.push(DetailRow::Line(l)),
                review::Row::Note(t) => rows.push(DetailRow::Note(t)),
                _ => {}
            }
        }
    }
    rows
}

/// True when line `l` of a hunk is a change (not context).
pub(crate) fn is_change(f: &FilePatch, hunk: usize, l: usize) -> bool {
    f.file
        .hunks
        .get(hunk)
        .and_then(|h| h.lines.get(l))
        .is_some_and(|l| l.kind != DiffLineKind::Context)
}
