//! Git in the editor: gutter signs, the branch for the status line, and the
//! `git_status` and `worktrees` pickers. The status screen is in
//! [`crate::git_screen`]; hunk keys, blame and the hunk popup in
//! [`crate::git_hunks`].
//!
//! All Git work runs on background threads through [`Editor::spawn_background`].
//! Each buffer has at most one worker thread at a time. Edits are debounced:
//! the worker waits until the buffer has been quiet for [`EDIT_DEBOUNCE`],
//! then diffs the newest text it was given. A result is applied only if it
//! answers the buffer's latest request and the buffer is still at the
//! version the result was computed for.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use maestro_core::{DocumentVersion, ReadOnlyReason, Rope};
pub use maestro_git::Git;
pub use maestro_git::{Base, LineHunk, Sign, SignMap};
use maestro_git::{Head, Repository, diff_lines};

use crate::editor::{DocumentId, Editor};
use crate::picker::{Picker, PickerAction, PickerItem};

crate::commands! {
    git_status => "Pick a changed file from git status",
    worktrees => "Pick a Git worktree and make it the working directory",
}

/// How long a buffer must be quiet after an edit before its signs are
/// recomputed.
pub const EDIT_DEBOUNCE: Duration = Duration::from_millis(150);

/// Buffers larger than this get no signs, to keep diffs cheap.
const MAX_DIFF_BYTES: usize = 8 << 20;

/// Git state the editor keeps between frames.
pub struct GitState {
    git: Git,
    /// What gutter signs compare against. Changing it takes effect on the
    /// next [`Editor::poll_background`].
    pub base: Base,
    docs: HashMap<DocumentId, DocGit>,
    repos: HashMap<PathBuf, RepoInfo>,
    /// Bumped each time a Git picker opens, so a slow result for a picker
    /// the user already closed is dropped.
    picker_seq: u64,
    /// The status screen (`space g`), when open.
    pub screen: Option<crate::git_screen::Screen>,
    /// A hunk diff or blame popup in a buffer; closes on the next key.
    pub popup: Option<crate::git_hunks::Popup>,
    /// Blame columns, by buffer.
    pub blame: HashMap<DocumentId, crate::git_hunks::BlameColumn>,
    /// A commit message being written (`c c` / `c a` on the status screen).
    pub commit_message: Option<crate::git_screen::CommitMessage>,
}

#[derive(Default)]
struct DocGit {
    requested: Option<RequestKey>,
    /// Number of the latest request; only its result is applied.
    seq: u64,
    signs: SignMap,
    /// The changes the signs show, and the buffer version and base text
    /// they were computed from.
    hunks: Vec<LineHunk>,
    hunks_version: Option<DocumentVersion>,
    base_text: Option<Arc<str>>,
    /// Root of the repository holding the file, once known.
    root: Option<PathBuf>,
    slot: Arc<Mutex<Slot>>,
}

/// Everything that, when it changes, calls for new signs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestKey {
    version: DocumentVersion,
    modified: bool,
    path: PathBuf,
    base: Base,
    /// Bumped when the repository's index or HEAD changes.
    epoch: u64,
}

struct RepoInfo {
    head: Option<Head>,
    stamp: Vec<Option<SystemTime>>,
    watch: Vec<PathBuf>,
    epoch: u64,
}

/// Shared between the editor and a buffer's worker thread.
#[derive(Default)]
struct Slot {
    pending: Option<Snapshot>,
    running: bool,
    cache: Option<BaseCache>,
}

struct Snapshot {
    doc: DocumentId,
    seq: u64,
    version: DocumentVersion,
    text: Rope,
    path: PathBuf,
    base: Base,
    epoch: u64,
    refetch: bool,
    due: Instant,
}

/// The base text for one path, kept across edits so typing only diffs.
struct BaseCache {
    path: PathBuf,
    base: Base,
    epoch: u64,
    repo: Option<Repository>,
    text: Option<Arc<str>>,
    untracked: bool,
    head: Option<Head>,
    /// The repository's watched files and their times, read before the
    /// base, so a change made while it was read still bumps the epoch.
    watch: Vec<PathBuf>,
    stamp: Vec<Option<SystemTime>>,
}

struct Outcome {
    doc: DocumentId,
    seq: u64,
    version: DocumentVersion,
    path: PathBuf,
    repo: Option<Repository>,
    head: Option<Head>,
    watch: Vec<PathBuf>,
    stamp: Vec<Option<SystemTime>>,
    signs: SignMap,
    hunks: Vec<LineHunk>,
    base_text: Option<Arc<str>>,
}

impl Default for GitState {
    fn default() -> Self {
        Self::new(Git::new())
    }
}

impl GitState {
    pub fn new(git: Git) -> Self {
        Self {
            git,
            base: Base::Index,
            docs: HashMap::new(),
            repos: HashMap::new(),
            picker_seq: 0,
            screen: None,
            popup: None,
            blame: HashMap::new(),
            commit_message: None,
        }
    }

    /// True while the status screen is open; it takes every key.
    pub fn screen_open(&self) -> bool {
        self.screen.is_some()
    }

    /// The changes a buffer's signs show, with the buffer version and base
    /// text they were computed from. `None` until signs arrive.
    pub fn hunks(&self, doc: DocumentId) -> Option<(&[LineHunk], DocumentVersion, Arc<str>)> {
        let d = self.docs.get(&doc)?;
        Some((&d.hunks, d.hunks_version?, d.base_text.clone()?))
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    /// The gutter sign for a line of a buffer.
    pub fn sign_at(&self, doc: DocumentId, line: usize) -> Option<Sign> {
        self.docs.get(&doc)?.signs.get(line)
    }

    /// Every sign of a buffer.
    pub fn signs(&self, doc: DocumentId) -> Option<&SignMap> {
        self.docs.get(&doc).map(|d| &d.signs)
    }

    /// Replaces a buffer's signs until the next refresh computes new ones.
    /// For front-end tests that need signs without a repository.
    pub fn set_signs(&mut self, doc: DocumentId, signs: SignMap) {
        self.docs.entry(doc).or_default().signs = signs;
    }

    /// The branch (or detached commit) of the repository holding a buffer.
    pub fn branch_label(&self, doc: DocumentId) -> Option<String> {
        let root = self.docs.get(&doc)?.root.as_ref()?;
        self.repos.get(root)?.head.as_ref().map(Head::label)
    }

    /// How many times the repository holding a buffer has changed (index,
    /// `HEAD`), as far as the editor has noticed; 0 when unknown.
    pub(crate) fn epoch_of(&self, doc: DocumentId) -> u64 {
        self.root_of(doc)
            .and_then(|r| self.repos.get(r))
            .map_or(0, |i| i.epoch)
    }

    /// The repository root holding a buffer, once known.
    pub fn root_of(&self, doc: DocumentId) -> Option<&Path> {
        self.docs.get(&doc)?.root.as_deref()
    }
}

/// Marks a repository as changed (the editor staged, committed or
/// switched branches in it), so its buffers re-read their base and branch
/// on the next refresh instead of waiting for the next tick.
pub fn repository_changed(ed: &mut Editor, root: &Path) {
    if let Some(info) = ed.git.repos.get_mut(root) {
        info.epoch += 1;
        info.stamp = stamp(&info.watch);
    }
}

/// Starts sign updates for buffers whose text, save state, comparison base
/// or repository changed since the last request. Cheap when nothing
/// changed; called from [`Editor::poll_background`].
pub fn refresh(ed: &mut Editor) {
    crate::git_screen::check_commit_message(ed);
    crate::git_hunks::refresh_blame(ed);
    let live: Vec<DocumentId> = ed.buffers.keys().copied().collect();
    ed.git.docs.retain(|id, _| live.contains(id));
    ed.git.blame.retain(|id, _| live.contains(id));
    let now = Instant::now();
    let base = ed.git.base;
    for id in live {
        let buf = &ed.buffers[&id];
        let Some(path) = buf.doc.path() else {
            continue;
        };
        // Binary and undecodable files open read-only; diffing them is noise.
        let unreadable = matches!(
            buf.doc.read_only(),
            Some(ReadOnlyReason::Binary | ReadOnlyReason::InvalidEncoding)
        );
        if unreadable || buf.doc.text().len_bytes() > MAX_DIFF_BYTES {
            // Signs computed before the buffer crossed the limit would point
            // at lines that have since moved; show none instead.
            if let Some(entry) = ed.git.docs.get_mut(&id) {
                entry.signs = Default::default();
                entry.hunks.clear();
                entry.hunks_version = None;
            }
            continue;
        }
        let entry = ed.git.docs.entry(id).or_default();
        let epoch = entry
            .root
            .as_ref()
            .and_then(|r| ed.git.repos.get(r))
            .map_or(0, |r| r.epoch);
        let key = RequestKey {
            version: buf.doc.version(),
            modified: buf.doc.is_modified(),
            path: path.to_path_buf(),
            base,
            epoch,
        };
        if entry.requested.as_ref() == Some(&key) {
            continue;
        }
        // Re-read the base on open, save, reload, rename, a new comparison
        // base or a repository change. Plain edits reuse it and wait for
        // typing to pause.
        let refetch = match &entry.requested {
            None => true,
            Some(prev) => {
                prev.path != key.path
                    || prev.epoch != key.epoch
                    || prev.base != key.base
                    || !key.modified
            }
        };
        entry.seq += 1;
        let snapshot = Snapshot {
            doc: id,
            seq: entry.seq,
            version: key.version,
            text: buf.doc.text().clone(),
            path: key.path.clone(),
            base,
            epoch,
            refetch,
            due: if refetch { now } else { now + EDIT_DEBOUNCE },
        };
        entry.requested = Some(key);
        let slot = entry.slot.clone();
        let start = {
            let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(old) = &s.pending {
                // Keep a pending base refresh when an edit replaces it.
                let refetch = old.refetch || snapshot.refetch;
                s.pending = Some(Snapshot {
                    refetch,
                    ..snapshot
                });
            } else {
                s.pending = Some(snapshot);
            }
            !std::mem::replace(&mut s.running, true)
        };
        if start {
            let git = ed.git.git.clone();
            let spawned = ed.spawn_background("git-signs", move |tx| {
                worker(git, slot, tx);
            });
            if spawned.is_err()
                && let Some(d) = ed.git.docs.get_mut(&id)
            {
                d.slot.lock().unwrap_or_else(|e| e.into_inner()).running = false;
                d.requested = None;
            }
        }
    }
}

/// Watches each known repository's index, HEAD and HEAD reflog; a change
/// bumps its epoch so its buffers re-read their base and branch. Called
/// from [`Editor::tick`].
pub fn tick(ed: &mut Editor) {
    for info in ed.git.repos.values_mut() {
        let stamp = stamp(&info.watch);
        if stamp != info.stamp {
            info.stamp = stamp;
            info.epoch += 1;
        }
    }
}

fn stamp(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    paths
        .iter()
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect()
}

fn worker(git: Git, slot: Arc<Mutex<Slot>>, tx: crate::editor::JobSender) {
    loop {
        let (snap, cache) = {
            let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
            let Some(due) = s.pending.as_ref().map(|p| p.due) else {
                s.running = false;
                return;
            };
            let now = Instant::now();
            if due > now {
                drop(s);
                std::thread::sleep(due - now);
                continue;
            }
            let snap = s.pending.take().expect("checked above");
            (snap, s.cache.take())
        };
        let cache = match cache {
            Some(c)
                if !snap.refetch
                    && c.path == snap.path
                    && c.base == snap.base
                    && c.epoch == snap.epoch =>
            {
                c
            }
            _ => load_base(&git, snap.base, &snap.path, snap.epoch),
        };
        let text = snap.text.to_string();
        let hunks = match &cache.text {
            Some(b) => diff_lines(b, &text),
            None => Vec::new(),
        };
        let signs = match (&cache.text, cache.untracked) {
            (Some(_), _) => SignMap::from_hunks(&hunks),
            (None, true) => SignMap::all_added(&text),
            (None, false) => SignMap::default(),
        };
        let outcome = Outcome {
            doc: snap.doc,
            seq: snap.seq,
            version: snap.version,
            path: snap.path,
            repo: cache.repo.clone(),
            head: cache.head.clone(),
            watch: cache.watch.clone(),
            stamp: cache.stamp.clone(),
            signs,
            hunks,
            base_text: cache.text.clone(),
        };
        slot.lock().unwrap_or_else(|e| e.into_inner()).cache = Some(cache);
        if !tx.send(move |ed| apply(ed, outcome)) {
            return;
        }
    }
}

fn load_base(git: &Git, base: Base, path: &Path, epoch: u64) -> BaseCache {
    let mut cache = BaseCache {
        path: path.to_path_buf(),
        base,
        epoch,
        repo: None,
        text: None,
        untracked: false,
        head: None,
        watch: Vec::new(),
        stamp: Vec::new(),
    };
    // Errors leave the buffer without signs; they are not worth a message
    // on every keystroke. `:git-status` reports them.
    let Ok(Some(repo)) = git.discover(path) else {
        return cache;
    };
    cache.watch = repo.watch_files().to_vec();
    cache.stamp = stamp(&cache.watch);
    cache.head = repo.head().ok();
    match repo.base_text(path, base) {
        Ok(Some(bytes)) => {
            // Decode the way files are opened (BOM, UTF-16), so an
            // unchanged file compares equal to its buffer.
            let (rope, _, problem) = maestro_core::io::decode(&bytes);
            if problem.is_none() {
                cache.text = Some(Arc::from(rope.to_string()));
            }
        }
        Ok(None) => cache.untracked = repo.is_untracked(path).unwrap_or(false),
        Err(_) => {}
    }
    cache.repo = Some(repo);
    cache
}

fn apply(ed: &mut Editor, out: Outcome) {
    let Some(buf) = ed.buffers.get(&out.doc) else {
        return;
    };
    // Only the answer to the latest request, for the current text and
    // name, is shown.
    if buf.doc.version() != out.version || buf.doc.path() != Some(out.path.as_path()) {
        return;
    }
    if ed.git.docs.get(&out.doc).is_none_or(|d| d.seq != out.seq) {
        return;
    }
    let root = out.repo.as_ref().map(|r| r.root().to_path_buf());
    let mut fresh_repo = false;
    if let Some(root) = &root {
        let info = ed.git.repos.entry(root.clone()).or_insert_with(|| {
            fresh_repo = true;
            RepoInfo {
                head: None,
                // The times from before the base was read: if Git changed
                // since, the next tick sees it.
                stamp: out.stamp,
                watch: out.watch,
                epoch: 0,
            }
        });
        if out.head.is_some() {
            info.head = out.head;
        }
    }
    let epoch = root
        .as_ref()
        .and_then(|r| ed.git.repos.get(r))
        .map_or(0, |i| i.epoch);
    let Some(d) = ed.git.docs.get_mut(&out.doc) else {
        return;
    };
    d.signs = out.signs;
    d.hunks = out.hunks;
    d.hunks_version = Some(out.version);
    d.base_text = out.base_text;
    if d.root != root {
        d.root = root;
        // The request was keyed before the repository was known. If this
        // result created the repository's record, its stamp predates the
        // base, so the base is current for epoch 0. Otherwise the key keeps
        // its old epoch and the next refresh re-reads the base.
        if fresh_repo && let Some(k) = &mut d.requested {
            k.epoch = epoch;
        }
    }
}

// ----- pickers ----------------------------------------------------------

/// Where to look for a repository: the focused file's directory, else the
/// working directory.
pub(crate) fn start_dir(ed: &Editor) -> PathBuf {
    ed.doc()
        .path()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ed.cwd.clone())
}

pub(crate) fn find_repo(git: &Git, start: &Path) -> Result<Repository, String> {
    match git.discover(start) {
        Ok(Some(r)) => Ok(r),
        Ok(None) => Err(format!(
            "{} is not in a Git repository",
            crate::navigation::home_path(start)
        )),
        Err(e) => Err(e.to_string()),
    }
}

const STATUS_TITLE: &str = "git status";
const WORKTREES_TITLE: &str = "worktrees";

/// Opens a picker titled `title` that fills in when `work` finishes on a
/// background thread. If the user closed it or opened another picker in
/// the meantime, the result is dropped.
fn async_picker<F>(ed: &mut Editor, title: &'static str, empty: &'static str, work: F)
where
    F: FnOnce() -> Result<(String, Vec<PickerItem>), String> + Send + 'static,
{
    let mut picker = Picker::new(title, Vec::new());
    picker.loading = true;
    ed.picker = Some(picker);
    ed.git.picker_seq += 1;
    let seq = ed.git.picker_seq;
    let spawned = ed.spawn_background(title, move |tx| {
        let result = work();
        tx.send(move |ed| {
            let ours = ed.git.picker_seq == seq
                && ed
                    .picker
                    .as_ref()
                    .is_some_and(|p| p.loading && p.title == title);
            if !ours {
                return;
            }
            match result {
                Ok((_, items)) if items.is_empty() => {
                    ed.picker = None;
                    ed.info(empty);
                }
                Ok((new_title, items)) => {
                    if let Some(p) = &mut ed.picker {
                        p.title = new_title;
                        p.loading = false;
                        p.extend(items);
                    }
                }
                Err(e) => {
                    ed.picker = None;
                    ed.error(e);
                }
            }
        });
    });
    if let Err(e) = spawned {
        ed.picker = None;
        ed.error(format!("could not start {title}: {e}"));
    }
}

fn git_status(ed: &mut Editor) {
    let git = ed.git.git.clone();
    let start = start_dir(ed);
    async_picker(ed, STATUS_TITLE, "working tree clean", move || {
        let repo = find_repo(&git, &start)?;
        let status = repo.status().map_err(|e| e.to_string())?;
        let branch = status
            .branch
            .head
            .clone()
            .unwrap_or_else(|| "detached".into());
        let items: Vec<PickerItem> = status
            .entries
            .iter()
            .map(|e| PickerItem {
                label: e.path.display().to_string(),
                detail: e.describe(),
                action: PickerAction::OpenFile(repo.root().join(&e.path)),
            })
            .collect();
        let title = format!("{STATUS_TITLE} ({branch}, {} files)", items.len());
        Ok((title, items))
    });
}

fn worktrees(ed: &mut Editor) {
    let git = ed.git.git.clone();
    let start = start_dir(ed);
    async_picker(ed, WORKTREES_TITLE, "no worktrees", move || {
        let repo = find_repo(&git, &start)?;
        let list = repo.worktrees().map_err(|e| e.to_string())?;
        let items = list
            .iter()
            .filter(|w| !w.bare)
            .map(|w| PickerItem {
                label: w.path.display().to_string(),
                detail: describe_worktree(&git, w),
                action: PickerAction::ChangeDir(w.path.clone()),
            })
            .collect();
        Ok((WORKTREES_TITLE.to_string(), items))
    });
}

/// `branch  state`, e.g. `agent/a  2 changed, 1 untracked`.
fn describe_worktree(git: &Git, w: &maestro_git::Worktree) -> String {
    let branch = match (&w.branch, &w.head) {
        (Some(b), _) => b.clone(),
        (None, Some(h)) => format!("(detached {})", &h[..h.len().min(7)]),
        (None, None) => "(no commits)".into(),
    };
    let mut state = if w.prunable.is_some() {
        "missing".to_string()
    } else {
        let status = git.discover(&w.path).and_then(|r| match r {
            Some(r) => r.status().map(Some),
            None => Ok(None),
        });
        match status {
            Ok(Some(s)) if s.is_clean() => "clean".to_string(),
            Ok(Some(s)) => {
                let changed = s.uncommitted().count();
                let untracked = s.untracked().count();
                match (changed, untracked) {
                    (c, 0) => format!("{c} changed"),
                    (0, u) => format!("{u} untracked"),
                    (c, u) => format!("{c} changed, {u} untracked"),
                }
            }
            Ok(None) => "not a working tree".into(),
            Err(e) => format!("status failed: {e}"),
        }
    };
    if w.locked.is_some() {
        state.push_str(", locked");
    }
    if w.is_main {
        state.push_str(", main worktree");
    }
    format!("{branch}  {state}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use maestro_config::Config;
    use maestro_core::Document;

    use crate::layout::Rect;

    #[test]
    fn results_for_another_request_version_or_path_are_dropped() {
        let mut ed = Editor::new(Config::default(), Rect::new(0, 0, 40, 10));
        let mut doc = Document::from_text("a\nb\n");
        let path = std::env::temp_dir().join("maestro-stale-check.txt");
        doc.set_path(path.clone());
        let id = ed.add_document(doc);
        ed.switch_to(id);
        ed.git.docs.insert(
            id,
            DocGit {
                seq: 2,
                ..DocGit::default()
            },
        );
        let current = ed.doc().version();
        let outcome = |version| Outcome {
            doc: id,
            seq: 2,
            version,
            path: path.clone(),
            repo: None,
            head: None,
            watch: Vec::new(),
            stamp: Vec::new(),
            signs: SignMap::all_added("a\nb\n"),
            hunks: Vec::new(),
            base_text: None,
        };
        apply(&mut ed, outcome(DocumentVersion(current.0 + 1)));
        assert!(ed.git.signs(id).unwrap().is_empty());
        // Same version and path, but an older request (say, from before
        // the index changed): dropped.
        let mut older = outcome(current);
        older.seq = 1;
        apply(&mut ed, older);
        assert!(ed.git.signs(id).unwrap().is_empty());
        apply(&mut ed, outcome(current));
        assert_eq!(ed.git.signs(id).unwrap().len(), 2);
        // A result for an old name (the buffer was renamed) is dropped too.
        let mut renamed = outcome(current);
        renamed.path = std::env::temp_dir().join("maestro-elsewhere.txt");
        renamed.signs = SignMap::default();
        apply(&mut ed, renamed);
        assert_eq!(ed.git.signs(id).unwrap().len(), 2);
    }
}
