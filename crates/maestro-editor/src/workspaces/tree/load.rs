//! The file tree's background work: listing directories and comparing the
//! worktree with its base. Each runs on its own thread through
//! [`Editor::spawn_background`]; results carry the generation they were
//! started in, and results from before a reset (another worktree) are
//! dropped. One comparison runs at a time; a request while one runs queues
//! one more.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use maestro_git::{Git, PathChangeKind};

use super::{Base, Changes, Comparison, Entry, HeadReason, Listing};
use crate::editor::{DocumentId, Editor};

/// A directory lists at most this many entries; the rest are counted.
pub const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Default)]
pub(crate) struct Jobs {
    /// Bumped by [`reset`].
    generation: u64,
    /// The generation of the comparison running, if one is.
    comparing: Option<u64>,
    again: bool,
    pub(crate) last_refresh: Option<Instant>,
    /// The worktree root as the editor spelled it when the tree last
    /// followed it (before resolving symlinks).
    pub(crate) source: Option<PathBuf>,
    /// Bumped for each listing started; a listing older than the one shown
    /// is dropped.
    list_seq: u64,
    shown_seq: HashMap<PathBuf, u64>,
    /// Each named buffer's version and unsaved state when last looked at.
    buffers: HashMap<DocumentId, (u64, bool)>,
}

/// Starts over at `root`: everything folded, nothing selected, the root
/// listed and the changes read.
pub(crate) fn reset(ed: &mut Editor, root: PathBuf) {
    let t = &mut ed.workspaces.tree;
    t.root = Some(root);
    t.dirs.clear();
    t.expanded.clear();
    t.folded_changed.clear();
    t.selected = None;
    t.top = 0;
    t.changes = Changes::Loading;
    t.diff = None;
    t.jobs.generation += 1;
    t.jobs.shown_seq.clear();
    t.jobs.comparing = None;
    t.jobs.again = false;
    refresh(ed);
}

/// Lists the root and every unfolded directory again and reads the
/// changes. Listings already shown stay until the new ones arrive.
pub(crate) fn refresh(ed: &mut Editor) {
    ed.workspaces.tree.jobs.last_refresh = Some(Instant::now());
    let t = &ed.workspaces.tree;
    let mut dirs: Vec<PathBuf> = std::iter::once(PathBuf::new())
        .chain(t.expanded.iter().cloned())
        .collect();
    dirs.sort();
    // Folded directories are listed again when unfolded.
    let keep: Vec<PathBuf> = dirs.clone();
    ed.workspaces.tree.dirs.retain(|d, _| keep.contains(d));
    list(ed, dirs);
    compare(ed);
}

/// Lists `dirs` (relative to the root) on one background thread.
pub(crate) fn list(ed: &mut Editor, dirs: Vec<PathBuf>) {
    let Some(root) = ed.workspaces.tree.root.clone() else {
        return;
    };
    let generation = ed.workspaces.tree.jobs.generation;
    ed.workspaces.tree.jobs.list_seq += 1;
    let seq = ed.workspaces.tree.jobs.list_seq;
    for d in &dirs {
        ed.workspaces
            .tree
            .dirs
            .entry(d.clone())
            .or_insert(Listing::Loading);
    }
    let spawned = ed.spawn_background("file-tree-list", move |tx| {
        for dir in dirs {
            let listing = list_dir(&root, &dir);
            let sent = tx.send(move |ed| {
                let t = &mut ed.workspaces.tree;
                let newer = t.jobs.shown_seq.get(&dir).is_some_and(|s| *s > seq);
                if t.jobs.generation == generation && !newer {
                    t.jobs.shown_seq.insert(dir.clone(), seq);
                    t.dirs.insert(dir, listing);
                }
            });
            if !sent {
                return;
            }
        }
    });
    if let Err(e) = spawned {
        for listing in ed.workspaces.tree.dirs.values_mut() {
            if *listing == Listing::Loading {
                *listing = Listing::Failed(format!("could not list: {e}"));
            }
        }
    }
}

/// Runs on a background thread: one directory's entries, with the file
/// finder's ignore rules (`.gitignore`, `.ignore`, hidden files).
pub(crate) fn list_dir(root: &Path, rel: &Path) -> Listing {
    let dir = root.join(rel);
    if !dir.exists() {
        // A directory deleted on the branch: its files come from the
        // changes.
        return Listing::Ready {
            entries: Vec::new(),
            more: 0,
        };
    }
    if !dir.is_dir() {
        return Listing::Failed(format!("{} is not a directory", rel.display()));
    }
    let mut walk = crate::navigation::finder::walker(&dir);
    walk.max_depth(Some(1));
    let mut entries = Vec::new();
    let mut more = 0;
    for entry in walk.build() {
        let Ok(entry) = entry else { continue };
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            more += 1;
            continue;
        }
        let Some(ft) = entry.file_type() else {
            continue;
        };
        let is_dir = ft.is_dir() || (ft.is_symlink() && entry.path().is_dir());
        entries.push(Entry {
            name: entry.file_name().to_os_string(),
            is_dir,
        });
    }
    super::sort_entries(&mut entries);
    Listing::Ready { entries, more }
}

/// Starts a comparison, or queues one if one is running.
pub(crate) fn compare(ed: &mut Editor) {
    let Some(root) = ed.workspaces.tree.root.clone() else {
        return;
    };
    let jobs = &mut ed.workspaces.tree.jobs;
    if jobs.comparing == Some(jobs.generation) {
        jobs.again = true;
        return;
    }
    let generation = jobs.generation;
    jobs.comparing = Some(generation);
    jobs.again = false;
    let git = ed.git.git().clone();
    let spawned = ed.spawn_background("file-tree-changes", move |tx| {
        let changes = run_compare(&git, &root);
        tx.send(move |ed| apply(ed, generation, changes));
    });
    if let Err(e) = spawned {
        let t = &mut ed.workspaces.tree;
        t.jobs.comparing = None;
        t.changes = Changes::Unavailable(format!("could not start Git: {e}"));
    }
}

fn apply(ed: &mut Editor, generation: u64, changes: Changes) {
    let t = &mut ed.workspaces.tree;
    if t.jobs.generation != generation {
        return;
    }
    t.jobs.comparing = None;
    t.changes = changes;
    if std::mem::take(&mut t.jobs.again) {
        compare(ed);
    }
}

/// Runs on a background thread: what the worktree at `root` changed
/// against its base.
pub(crate) fn run_compare(git: &Git, root: &Path) -> Changes {
    let repo = match git.discover(root) {
        Ok(Some(r)) => r,
        Ok(None) => return Changes::Unavailable("not a Git repository".into()),
        Err(e) => return Changes::Unavailable(format!("Git failed: {e}")),
    };
    match compare_in(&repo, root) {
        Ok(c) => Changes::Ready(Box::new(c)),
        Err(e) => Changes::Unavailable(format!("Git failed: {e}")),
    }
}

fn compare_in(repo: &maestro_git::Repository, root: &Path) -> maestro_git::Result<Comparison> {
    let has_head = repo.has_head()?;
    let branch = repo
        .head()
        .ok()
        .and_then(|h| h.branch().map(str::to_string));
    let head = if !has_head {
        "(no commits)".to_string()
    } else {
        repo.head()?.label()
    };
    let base = if !has_head {
        Base::NoCommits
    } else {
        match repo.default_branch()? {
            None => Base::Head(HeadReason::NoDefault),
            Some(full) => {
                let shown = maestro_git::short_ref(&full).to_string();
                // `origin/main` is the default: a local `main` is on it.
                let local = shown.split_once('/').map_or(shown.as_str(), |(_, b)| b);
                let on_default = branch.as_deref().is_some_and(|b| {
                    b == shown || (full.starts_with("refs/remotes/") && b == local)
                });
                if on_default {
                    Base::Head(HeadReason::OnDefault(shown))
                } else {
                    match repo.merge_base(&full, "HEAD")? {
                        Some(id) => Base::MergeBase { branch: shown, id },
                        None => Base::Head(HeadReason::Unrelated(shown)),
                    }
                }
            }
        }
    };
    let rev = match &base {
        Base::MergeBase { id, .. } => Some(id.as_str()),
        Base::Head(_) => Some("HEAD"),
        Base::NoCommits => None,
    };
    let list = repo.changes_since(rev)?;
    let repo_root = repo.root().to_path_buf();
    let mut files = BTreeMap::new();
    let mut ignores = DotIgnore::default();
    for c in list {
        let abs = repo_root.join(&c.path);
        // The tree may show a directory inside the repository; changes
        // outside it are left out.
        let Ok(rel) = abs.strip_prefix(root).map(Path::to_path_buf) else {
            continue;
        };
        // Git knows `.gitignore`; the tree also honors `.ignore`, as the
        // file finder does.
        if c.kind == PathChangeKind::Untracked && ignores.ignored(&repo_root, &abs) {
            continue;
        }
        files.insert(rel, c.kind);
    }
    let (dir_counts, children) = index(&files);
    Ok(Comparison {
        head,
        base,
        repo_root,
        files,
        dir_counts,
        children,
    })
}

/// `.ignore` files (the ripgrep convention the file finder follows), read
/// once per directory.
#[derive(Default)]
struct DotIgnore {
    by_dir: HashMap<PathBuf, Option<ignore::gitignore::Gitignore>>,
}

impl DotIgnore {
    /// True when the deepest `.ignore` rule between `top` and `file` that
    /// matches it (or a directory above it) ignores it.
    fn ignored(&mut self, top: &Path, file: &Path) -> bool {
        let mut dirs: Vec<PathBuf> = file
            .ancestors()
            .skip(1)
            .take_while(|d| d.starts_with(top))
            .map(Path::to_path_buf)
            .collect();
        // Deepest first: a nearer file overrides one further up.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for dir in dirs {
            let rules = self.by_dir.entry(dir.clone()).or_insert_with(|| {
                let path = dir.join(".ignore");
                path.is_file()
                    .then(|| ignore::gitignore::Gitignore::new(&path).0)
            });
            if let Some(rules) = rules {
                let m = rules.matched_path_or_any_parents(file, false);
                if m.is_ignore() {
                    return true;
                }
                if m.is_whitelist() {
                    return false;
                }
            }
        }
        false
    }
}

type Index = (
    HashMap<PathBuf, usize>,
    HashMap<PathBuf, BTreeSet<(OsString, bool)>>,
);

/// Per directory: how many changed files are under it, and the entries
/// the changed paths need in it.
fn index(files: &BTreeMap<PathBuf, PathChangeKind>) -> Index {
    let mut counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut children: HashMap<PathBuf, BTreeSet<(OsString, bool)>> = HashMap::new();
    for path in files.keys() {
        let mut child = path.as_path();
        let mut is_dir = false;
        while let Some(parent) = child.parent() {
            if let Some(name) = child.file_name() {
                children
                    .entry(parent.to_path_buf())
                    .or_default()
                    .insert((name.to_os_string(), is_dir));
            }
            *counts.entry(parent.to_path_buf()).or_default() += 1;
            child = parent;
            is_dir = true;
        }
    }
    (counts, children)
}

/// True when a named buffer was saved since the last look: it had unsaved
/// changes and has none now, or its text changed and it has no unsaved
/// changes (edited and saved between two looks, or reloaded from disk).
pub(crate) fn noticed_save(ed: &mut Editor) -> bool {
    let now: HashMap<DocumentId, (u64, bool)> = ed
        .buffers
        .values()
        .filter(|b| b.doc.path().is_some())
        .map(|b| (b.id, (b.doc.version().0, b.doc.is_modified())))
        .collect();
    let before = std::mem::replace(&mut ed.workspaces.tree.jobs.buffers, now);
    let now = &ed.workspaces.tree.jobs.buffers;
    now.iter().any(|(id, &(version, modified))| {
        // A buffer opened since the last look starts at version 0.
        let (was_version, was_modified) = before.get(id).copied().unwrap_or((0, false));
        !modified && (was_modified || version != was_version)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_counts_changes_per_directory_and_names_needed_entries() {
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("a.txt"), PathChangeKind::Modified);
        files.insert(PathBuf::from("src/x/gone.rs"), PathChangeKind::Deleted);
        files.insert(PathBuf::from("src/new.rs"), PathChangeKind::Added);
        let (counts, children) = index(&files);
        assert_eq!(counts[Path::new("")], 3);
        assert_eq!(counts[Path::new("src")], 2);
        assert_eq!(counts[Path::new("src/x")], 1);
        let names = |dir: &str| -> Vec<(String, bool)> {
            children[Path::new(dir)]
                .iter()
                .map(|(n, d)| (n.to_string_lossy().into_owned(), *d))
                .collect()
        };
        let root = names("");
        assert_eq!(root, [("a.txt".into(), false), ("src".into(), true)]);
        let src = names("src");
        assert_eq!(src, [("new.rs".into(), false), ("x".into(), true)]);
    }
}
