//! Commits built from chosen hunks: part of one worktree's work, or hunks
//! from several worktrees composed on their common base.
//!
//! The hunks come from review diffs ([`Repository::review_diff`]) against
//! one base commit. [`Repository::tree_from_hunks`] applies the chosen ones
//! (whole, or some of their changed lines) to the base's files in memory,
//! checks that every line they expect is there and that no two of them
//! change the same or neighboring lines, and writes the result as a tree
//! through a temporary index. Nothing here touches a working tree, the real
//! index or a ref, except [`Repository::create_branch_at`], which only ever
//! creates a new branch.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::repo::check_rev;
use crate::review::TempFile;
use crate::worktree::check_branch_name;
use crate::{
    Access, DiffFile, DiffLineKind, FileChange, GitError, Hunk, Repository, Result, bytes_to_path,
    split_nul,
};

/// One hunk to take, or some of its changed lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkPick {
    /// Where the hunk comes from, for messages (an agent's name).
    pub source: String,
    /// The file's path at the base; `None` for an added file.
    pub old_path: Option<PathBuf>,
    /// The file's path afterward; `None` for a deleted file.
    pub new_path: Option<PathBuf>,
    pub change: FileChange,
    /// The added file's mode (`100644`, `100755`, `120000`).
    pub new_mode: Option<String>,
    pub hunk: Hunk,
    /// Indices into `hunk.lines` of the added and removed lines to take.
    /// `None` takes every changed line. An added line left out is not
    /// added; a removed line left out stays.
    pub lines: Option<BTreeSet<usize>>,
}

impl HunkPick {
    /// Hunk `index` of `file`, with only `lines` of it when given.
    pub fn new(
        source: impl Into<String>,
        file: &DiffFile,
        index: usize,
        lines: Option<BTreeSet<usize>>,
    ) -> Option<Self> {
        Some(Self {
            source: source.into(),
            old_path: file.old_path.clone(),
            new_path: file.new_path.clone(),
            change: file.change.clone(),
            new_mode: file.new_mode.clone(),
            hunk: file.hunks.get(index)?.clone(),
            lines,
        })
    }

    /// The path the hunk changes.
    pub fn path(&self) -> Option<&Path> {
        self.new_path.as_deref().or(self.old_path.as_deref())
    }

    /// `src/a.rs -10,7 +10,8`, and `(2 of 5 changed lines)` when only some
    /// of its lines are taken.
    pub fn label(&self) -> String {
        let h = &self.hunk;
        let mut s = format!(
            "{} -{},{} +{},{}",
            self.path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            h.old_start,
            h.old_len,
            h.new_start,
            h.new_len
        );
        let changed = changed_lines(h);
        if let Some(sel) = &self.lines {
            let n = changed.iter().filter(|i| sel.contains(i)).count();
            if n < changed.len() {
                s.push_str(&format!(" ({n} of {} changed lines)", changed.len()));
            }
        }
        s
    }
}

/// Indices of the added and removed lines of `h`.
pub fn changed_lines(h: &Hunk) -> Vec<usize> {
    h.lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.kind != DiffLineKind::Context)
        .map(|(i, _)| i)
        .collect()
}

/// A hunk that could not be taken, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickFailure {
    /// Index into the picks.
    pub pick: usize,
    pub reason: String,
}

/// Why [`Repository::tree_from_hunks`] made no tree.
#[derive(Debug, thiserror::Error)]
pub enum HunksError {
    /// These picks do not apply to the base, alone or together. Nothing was
    /// written.
    #[error("{} hunk(s) do not apply", .0.len())]
    DoNotApply(Vec<PickFailure>),
    #[error(transparent)]
    Git(#[from] GitError),
}

/// An operation of a resolved pick, in old-file order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// The base line at this index stays.
    Keep,
    Remove,
    Insert,
}

/// A pick reduced to what it changes: operations from `start` (a 0-based
/// line index in the base file) with no unchanged lines at either end.
#[derive(Debug)]
struct Core<'a> {
    pick: usize,
    start: usize,
    /// Base lines it covers: `start..end`.
    end: usize,
    ops: Vec<(Op, &'a str, bool)>,
}

/// One base file entry: mode, type and object id.
#[derive(Debug, Clone)]
struct Entry {
    mode: String,
    kind: String,
    oid: String,
}

/// A path's new mode and contents, or `None` to remove it.
type NewFile = Option<(String, Vec<u8>)>;

/// A line of a file: its bytes without the newline, and whether it had one.
fn split_lines(bytes: &[u8]) -> Vec<(&[u8], bool)> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        match rest.iter().position(|b| *b == b'\n') {
            Some(i) => {
                out.push((&rest[..i], true));
                rest = &rest[i + 1..];
            }
            None => {
                out.push((rest, false));
                rest = &[];
            }
        }
    }
    out
}

/// A path inside the repository: relative, plain components only.
fn plain(p: &Path) -> bool {
    !p.as_os_str().is_empty() && p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Proper ancestors of `p`, nearest last: `a`, `a/b` for `a/b/c`.
fn ancestors(p: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = p
        .ancestors()
        .skip(1)
        .filter(|a| !a.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    out.reverse();
    out
}

impl Repository {
    /// The entries of `rev`'s tree at exactly these paths (a directory is
    /// listed as its tree entry, not its contents).
    fn entries_at(&self, rev: &str, paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, Entry>> {
        let mut out = BTreeMap::new();
        if paths.is_empty() {
            return Ok(out);
        }
        let mut args: Vec<OsString> = vec![
            "ls-tree".into(),
            "-z".into(),
            "--full-tree".into(),
            "--end-of-options".into(),
            rev.into(),
            "--".into(),
        ];
        args.extend(paths.iter().map(|p| p.clone().into_os_string()));
        let listing = self.run(&args, Access::ReadOnly)?;
        // "<mode> <type> <oid>\t<path>"
        for rec in split_nul(&listing) {
            let Some(tab) = rec.iter().position(|c| *c == b'\t') else {
                continue;
            };
            let meta = String::from_utf8_lossy(&rec[..tab]);
            let f: Vec<&str> = meta.split(' ').collect();
            if f.len() < 3 {
                continue;
            }
            out.insert(
                bytes_to_path(&rec[tab + 1..]),
                Entry {
                    mode: f[0].to_string(),
                    kind: f[1].to_string(),
                    oid: f[2].to_string(),
                },
            );
        }
        Ok(out)
    }

    /// Applies `picks` to the files of commit `base` and writes the result
    /// as a tree, returning its id. The tree differs from `base`'s only in
    /// the picked changes; the same picks on the same base always give the
    /// same tree.
    ///
    /// Refuses with [`HunksError::DoNotApply`], naming every pick that
    /// fails, when a pick's lines are not what the base has (it was made
    /// against another base, or needs a change that was not picked), when
    /// two picks change the same or neighboring lines of a file, when a
    /// pick adds a file where the base has one (or a directory, or a file
    /// in its way) that no pick removes, when lines would follow a last
    /// line that has no newline, and for renamed or copied files and
    /// submodules, which are only taken whole with their worktree. A line
    /// added from text that was not valid UTF-8 (shown with U+FFFD) is
    /// refused, since its bytes are not known. Mode changes of modified
    /// files are not carried: a file keeps its base mode.
    ///
    /// Writes blobs and trees to the object store (unreferenced until a
    /// commit and branch name them). The index, working tree and refs are
    /// not touched: the tree is built in a temporary index.
    pub fn tree_from_hunks(&self, base: &str, picks: &[HunkPick]) -> Result<String, HunksError> {
        check_rev(base)?;
        let base = self.resolve(base)?;
        let mut failures: Vec<PickFailure> = Vec::new();
        let fail = |failures: &mut Vec<PickFailure>, pick: usize, reason: String| {
            if !failures.iter().any(|f| f.pick == pick) {
                failures.push(PickFailure { pick, reason });
            }
        };
        // Picks by the path they change.
        let mut by_path: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for (i, p) in picks.iter().enumerate() {
            let renamed =
                matches!(
                    p.change,
                    FileChange::Renamed { .. } | FileChange::Copied { .. }
                ) || (p.old_path.is_some() && p.new_path.is_some() && p.old_path != p.new_path);
            if renamed {
                fail(
                    &mut failures,
                    i,
                    "the file was renamed or copied; Maestro takes such a file only with the \
                     whole work"
                        .into(),
                );
                continue;
            }
            match p.path() {
                Some(path) if plain(path) => by_path.entry(path.to_path_buf()).or_default().push(i),
                _ => fail(
                    &mut failures,
                    i,
                    "the hunk names no path inside the repository".into(),
                ),
            }
        }
        // What the base has at those paths and at their parent directories.
        let mut wanted: BTreeSet<PathBuf> = BTreeSet::new();
        for path in by_path.keys() {
            wanted.insert(path.clone());
            wanted.extend(ancestors(path));
        }
        let wanted: Vec<PathBuf> = wanted.into_iter().collect();
        let entries = self.entries_at(&base, &wanted)?;

        // New contents per path: `Some((mode, bytes))`, or `None` to remove.
        let mut results: BTreeMap<PathBuf, NewFile> = BTreeMap::new();
        for (path, ids) in &by_path {
            let shown = path.display().to_string();
            let entry = entries.get(path);
            let base_bytes = match entry {
                Some(e) if e.kind == "blob" && e.mode != "160000" => {
                    self.run(&["cat-file", "blob", &e.oid], Access::ReadOnly)?
                }
                Some(e) => {
                    for &i in ids {
                        fail(
                            &mut failures,
                            i,
                            format!(
                                "{shown} is a {} at the base, not a file",
                                if e.kind == "tree" {
                                    "directory"
                                } else {
                                    "submodule"
                                }
                            ),
                        );
                    }
                    continue;
                }
                None => Vec::new(),
            };
            let mut base_lines = split_lines(&base_bytes);
            let mut entry = entry;
            let mut ids: Vec<usize> = ids.clone();
            // Git shows a file whose type changed (a file turned into a
            // symlink, say) as a deletion and an addition of one path.
            // Taken together, the deletion empties it and the addition
            // fills it; the addition alone is refused below.
            let is_added =
                |i: usize| picks[i].change == FileChange::Added || picks[i].old_path.is_none();
            let deleting: Vec<usize> = ids
                .iter()
                .copied()
                .filter(|&i| whole_deletion(&picks[i]))
                .collect();
            if entry.is_some() && !deleting.is_empty() && ids.iter().any(|&i| is_added(i)) {
                let mut ok = true;
                for &i in &ids {
                    if deleting.contains(&i) {
                        if let Err(why) = resolve(i, &picks[i], &base_lines, &shown) {
                            fail(&mut failures, i, why);
                            ok = false;
                        }
                    } else if !is_added(i) {
                        fail(
                            &mut failures,
                            i,
                            format!("it changes {shown}, which another taken hunk deletes"),
                        );
                        ok = false;
                    }
                }
                if !ok {
                    continue;
                }
                ids.retain(|&i| is_added(i));
                base_lines = Vec::new();
                entry = None;
            }
            let mut cores: Vec<Core> = Vec::new();
            for &i in &ids {
                let p = &picks[i];
                let added = p.change == FileChange::Added || p.old_path.is_none();
                if added && entry.is_some() {
                    fail(
                        &mut failures,
                        i,
                        format!(
                            "it adds {shown}, which the base already has; it needs another \
                             change (the removal of the file there) that is not taken"
                        ),
                    );
                    continue;
                }
                if !added && entry.is_none() {
                    fail(
                        &mut failures,
                        i,
                        format!("it changes {shown}, which the base does not have"),
                    );
                    continue;
                }
                match resolve(i, p, &base_lines, &shown) {
                    Ok(c) => cores.push(c),
                    Err(why) => fail(&mut failures, i, why),
                }
            }
            if cores.len() != ids.len() {
                continue;
            }
            // No two picks may change the same or neighboring lines.
            cores.sort_by_key(|c| (c.start, c.end, c.pick));
            let mut clash = false;
            for w in cores.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                if b.start <= a.end {
                    clash = true;
                    let (pa, pb) = (&picks[a.pick], &picks[b.pick]);
                    fail(
                        &mut failures,
                        b.pick,
                        format!(
                            "it conflicts with {} from {}: both change the same or neighboring \
                             lines",
                            pa.label(),
                            pa.source
                        ),
                    );
                    fail(
                        &mut failures,
                        a.pick,
                        format!(
                            "it conflicts with {} from {}: both change the same or neighboring \
                             lines",
                            pb.label(),
                            pb.source
                        ),
                    );
                }
            }
            if clash {
                continue;
            }
            // The new contents.
            let mut out: Vec<u8> = Vec::with_capacity(base_bytes.len());
            let mut open_end = false;
            let mut emit = |line: &[u8], nl: bool, out: &mut Vec<u8>| -> bool {
                if open_end {
                    return false;
                }
                out.extend_from_slice(line);
                if nl {
                    out.push(b'\n');
                }
                open_end = !nl;
                true
            };
            let mut at = 0;
            let mut broken = None;
            'cores: for c in &cores {
                for &(line, nl) in &base_lines[at..c.start] {
                    if !emit(line, nl, &mut out) {
                        broken = Some(c.pick);
                        break 'cores;
                    }
                }
                let mut o = c.start;
                for &(op, text, no_nl) in &c.ops {
                    let ok = match op {
                        Op::Keep => {
                            let (line, nl) = base_lines[o];
                            o += 1;
                            emit(line, nl, &mut out)
                        }
                        Op::Remove => {
                            o += 1;
                            true
                        }
                        Op::Insert => emit(text.as_bytes(), !no_nl, &mut out),
                    };
                    if !ok {
                        broken = Some(c.pick);
                        break 'cores;
                    }
                }
                at = c.end;
            }
            if broken.is_none() {
                for &(line, nl) in &base_lines[at..] {
                    if !emit(line, nl, &mut out) {
                        broken = cores.last().map(|c| c.pick);
                        break;
                    }
                }
            }
            if let Some(i) = broken {
                fail(
                    &mut failures,
                    i,
                    format!(
                        "it puts lines after the last line of {shown}, which has no newline at \
                         its end; take the change that adds the newline too"
                    ),
                );
                continue;
            }
            // A deleted file whose every line is taken is removed; otherwise
            // the file stays with what is left.
            let deletes = ids.iter().any(|&i| whole_deletion(&picks[i]));
            let new = if deletes && out.is_empty() {
                None
            } else {
                let mode = match entry {
                    Some(e) => e.mode.clone(),
                    None => {
                        let m = picks[ids[0]].new_mode.clone().unwrap_or("100644".into());
                        if !["100644", "100755", "120000"].contains(&m.as_str()) {
                            for &i in &ids {
                                fail(&mut failures, i, format!("{shown} has mode {m}"));
                            }
                            continue;
                        }
                        m
                    }
                };
                Some((mode, out))
            };
            results.insert(path.clone(), new);
        }

        // Files and directories in each other's way.
        let created: Vec<&PathBuf> = results
            .iter()
            .filter(|(p, r)| r.is_some() && !entries.contains_key(*p))
            .map(|(p, _)| p)
            .collect();
        for p in &created {
            for a in ancestors(p) {
                let file_there = entries
                    .get(&a)
                    .is_some_and(|e| e.kind != "tree" && !matches!(results.get(&a), Some(None)));
                let created_there = created.contains(&&a);
                if file_there || created_there {
                    for &i in &by_path[*p] {
                        fail(
                            &mut failures,
                            i,
                            format!(
                                "it adds {}, but {} is a file{}; it needs another change (the \
                                 removal of {}) that is not taken",
                                p.display(),
                                a.display(),
                                if created_there {
                                    " another picked hunk adds"
                                } else {
                                    " at the base"
                                },
                                a.display()
                            ),
                        );
                    }
                }
            }
        }
        // A file added where the base has a directory: every file in it
        // must be removed by a taken hunk. Each path is listed on its own,
        // since ls-tree lists a directory's entry only when none of its
        // files is asked for too.
        for p in &created {
            let listed = self.entries_at(&base, std::slice::from_ref(*p))?;
            if !listed.get(*p).is_some_and(|e| e.kind == "tree") {
                continue;
            }
            let args: Vec<OsString> = vec![
                "ls-tree".into(),
                "-r".into(),
                "-z".into(),
                "--name-only".into(),
                "--full-tree".into(),
                "--end-of-options".into(),
                base.clone().into(),
                "--".into(),
                p.as_os_str().to_owned(),
            ];
            let out = self.run(&args, Access::ReadOnly)?;
            let left: Vec<String> = split_nul(&out)
                .into_iter()
                .map(bytes_to_path)
                .filter(|f| !matches!(results.get(f), Some(None)))
                .map(|f| f.display().to_string())
                .collect();
            if !left.is_empty() {
                for &i in &by_path[*p] {
                    fail(
                        &mut failures,
                        i,
                        format!(
                            "it adds {}, but the base has a directory there with files no taken \
                             hunk removes: {}",
                            p.display(),
                            left.join(", ")
                        ),
                    );
                }
            }
        }
        if !failures.is_empty() {
            failures.sort_by_key(|f| f.pick);
            return Err(HunksError::DoNotApply(failures));
        }

        // Write the tree through a temporary index: removals first, so a
        // file can take the place of a directory whose files were removed.
        let index = TempFile::create("hunks-index", &[]).map_err(GitError::Io)?;
        std::fs::remove_file(&index.0).map_err(GitError::Io)?;
        let shadow = self.with_git(
            self.git()
                .clone()
                .env("GIT_INDEX_FILE", &index.0)
                .config("core.splitIndex=false"),
        );
        shadow.run(&["read-tree", &base], Access::Write)?;
        let mut writes: Vec<(&PathBuf, &NewFile)> = results.iter().collect();
        writes.sort_by_key(|(p, new)| (new.is_some(), *p));
        for (path, new) in writes {
            match new {
                Some((mode, bytes)) => {
                    let blob = TempFile::create("hunks-blob", bytes).map_err(GitError::Io)?;
                    let args: Vec<OsString> = vec![
                        "hash-object".into(),
                        "-w".into(),
                        "--no-filters".into(),
                        "--".into(),
                        blob.0.clone().into_os_string(),
                    ];
                    let out = shadow.run(&args, Access::Write)?;
                    let oid = String::from_utf8_lossy(&out).trim().to_string();
                    check_rev(&oid)?;
                    let args: Vec<OsString> = vec![
                        "update-index".into(),
                        "--add".into(),
                        "--cacheinfo".into(),
                        mode.into(),
                        oid.into(),
                        path.clone().into_os_string(),
                    ];
                    shadow.run(&args, Access::Write)?;
                }
                None => {
                    let args: Vec<OsString> = vec![
                        "update-index".into(),
                        "--force-remove".into(),
                        "--".into(),
                        path.clone().into_os_string(),
                    ];
                    shadow.run(&args, Access::Write)?;
                }
            }
        }
        let out = shadow.run(&["write-tree"], Access::Write)?;
        let tree = String::from_utf8_lossy(&out).trim().to_string();
        check_rev(&tree)?;
        Ok(tree)
    }

    /// Writes a commit of `tree` with the single parent `parent` and
    /// `message`, authored and committed by the user's configured identity
    /// (refused with [`GitError::NoIdentity`] without one). No ref moves;
    /// [`Repository::create_branch_at`] names it.
    pub fn commit_on(&self, tree: &str, parent: &str, message: &str) -> Result<String> {
        check_rev(tree)?;
        check_rev(parent)?;
        self.check_identity()?;
        let args: Vec<OsString> = vec![
            "commit-tree".into(),
            tree.into(),
            "-p".into(),
            parent.into(),
            "-m".into(),
            message.into(),
        ];
        let out = self.run(&args, Access::Write)?;
        let commit = String::from_utf8_lossy(&out).trim().to_string();
        check_rev(&commit)?;
        Ok(commit)
    }

    /// True when the local branch `branch` exists.
    pub fn branch_exists(&self, branch: &str) -> Result<bool> {
        check_branch_name(self, branch)?;
        let name = format!("refs/heads/{branch}");
        let (out, shown) = self.run_status(
            &["show-ref", "--verify", "--quiet", &name],
            Access::ReadOnly,
        )?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(crate::command_error(shown, &out)),
        }
    }

    /// Creates the local branch `branch` at `commit`. Refuses, changing
    /// nothing, when the branch already exists: it never moves one.
    pub fn create_branch_at(&self, branch: &str, commit: &str) -> Result<()> {
        check_branch_name(self, branch)?;
        check_rev(commit)?;
        let name = format!("refs/heads/{branch}");
        let (out, shown) = self.run_status(
            &["update-ref", "-m", "maestro: hunks", &name, commit, ""],
            Access::Write,
        )?;
        if !out.status.success() {
            if self.branch_exists(branch)? {
                return Err(GitError::InvalidArgument(format!(
                    "branch {branch} already exists; it was not moved"
                )));
            }
            return Err(crate::command_error(shown, &out));
        }
        Ok(())
    }
}

/// The pick deletes its file and takes every line of the deletion.
fn whole_deletion(p: &HunkPick) -> bool {
    p.new_path.is_none()
        && p.change == FileChange::Deleted
        && p.lines
            .as_ref()
            .is_none_or(|s| changed_lines(&p.hunk).iter().all(|l| s.contains(l)))
}

/// Reduces pick `i` to the operations it takes, checks them against the
/// base's lines, and trims unchanged lines from both ends.
fn resolve<'a>(
    i: usize,
    p: &'a HunkPick,
    base: &[(&[u8], bool)],
    shown: &str,
) -> Result<Core<'a>, String> {
    let h = &p.hunk;
    let changed = changed_lines(h);
    if let Some(sel) = &p.lines {
        if let Some(bad) = sel.iter().find(|i| !changed.contains(i)) {
            return Err(format!(
                "line {bad} of the hunk is not an added or removed line"
            ));
        }
        if sel.is_empty() {
            return Err("no changed line of the hunk is taken".into());
        }
    }
    if changed.is_empty() {
        return Err("the hunk changes nothing".into());
    }
    let taken = |idx: usize| p.lines.as_ref().is_none_or(|s| s.contains(&idx));
    // Where the hunk starts in the base: the first old line, or after
    // `old_start` when it has none (an insertion).
    let start = if h.old_len == 0 {
        h.old_start
    } else if h.old_start == 0 {
        return Err("the hunk header is not valid".into());
    } else {
        h.old_start - 1
    };
    // Within a run of changes, the k-th removed line pairs with the k-th
    // added one, as the side-by-side review shows them: a kept removed
    // line stays where it is, before the additions paired with later
    // lines. (Taken in unified order, keeping `b` of `-a -b +A +B` while
    // taking `-a +A` would give `b A` instead of `A b`.)
    let mut order: Vec<usize> = Vec::with_capacity(h.lines.len());
    let mut at = 0;
    while at < h.lines.len() {
        if h.lines[at].kind == DiffLineKind::Context {
            order.push(at);
            at += 1;
            continue;
        }
        let run_end = (at..h.lines.len())
            .find(|&j| h.lines[j].kind == DiffLineKind::Context)
            .unwrap_or(h.lines.len());
        let removed: Vec<usize> = (at..run_end)
            .filter(|&j| h.lines[j].kind == DiffLineKind::Removed)
            .collect();
        let added: Vec<usize> = (at..run_end)
            .filter(|&j| h.lines[j].kind == DiffLineKind::Added)
            .collect();
        for k in 0..removed.len().max(added.len()) {
            order.extend(removed.get(k));
            order.extend(added.get(k));
        }
        at = run_end;
    }
    let mut ops: Vec<(Op, &str, bool)> = Vec::new();
    let mut o = start;
    for idx in order {
        let l = &h.lines[idx];
        let op = match l.kind {
            DiffLineKind::Context => Op::Keep,
            DiffLineKind::Removed if taken(idx) => Op::Remove,
            DiffLineKind::Removed => Op::Keep,
            DiffLineKind::Added if taken(idx) => Op::Insert,
            DiffLineKind::Added => continue,
        };
        if op == Op::Insert {
            if l.text.contains('\u{FFFD}') {
                return Err(format!(
                    "an added line of {shown} is not valid UTF-8 text (or holds U+FFFD), so its \
                     bytes are not known; take the whole work instead"
                ));
            }
        } else {
            let Some(&(line, nl)) = base.get(o) else {
                return Err(format!(
                    "{shown} at the base has {} lines; the hunk expects line {}",
                    base.len(),
                    o + 1
                ));
            };
            // An old line with no newline is the base's last line, and the
            // other way around.
            if line != l.text.as_bytes() || nl == l.no_newline {
                let hint = if std::str::from_utf8(line).is_err() {
                    " (it is not UTF-8 text, which Maestro cannot match)"
                } else {
                    ""
                };
                return Err(format!(
                    "line {} of {shown} at the base is not what the hunk expects{hint}; it needs \
                     a change that is not taken, or was made against another base",
                    o + 1
                ));
            }
            o += 1;
        }
        ops.push((op, l.text.as_str(), l.no_newline));
    }
    // Unchanged lines at either end are the base's own; drop them.
    let mut start = start;
    let lead = ops.iter().take_while(|(op, ..)| *op == Op::Keep).count();
    ops.drain(..lead);
    start += lead;
    let trail = ops
        .iter()
        .rev()
        .take_while(|(op, ..)| *op == Op::Keep)
        .count();
    ops.truncate(ops.len() - trail);
    let end = start
        + ops
            .iter()
            .filter(|(op, ..)| matches!(op, Op::Keep | Op::Remove))
            .count();
    Ok(Core {
        pick: i,
        start,
        end,
        ops,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_keep_whether_they_end_in_a_newline() {
        assert_eq!(split_lines(b""), Vec::<(&[u8], bool)>::new());
        assert_eq!(
            split_lines(b"a\nb"),
            vec![(&b"a"[..], true), (&b"b"[..], false)]
        );
        assert_eq!(
            split_lines(b"a\n\n"),
            vec![(&b"a"[..], true), (&b""[..], true)]
        );
        assert_eq!(
            ancestors(Path::new("a/b/c")),
            vec![PathBuf::from("a"), PathBuf::from("a/b")]
        );
        assert!(!plain(Path::new("../x")) && !plain(Path::new("/x")) && plain(Path::new("x/y")));
    }
}
