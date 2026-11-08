//! Reviewing and landing a worktree's work: the review diff against a base,
//! committing everything, and integrating a branch with safety checks.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::diff::diff_args;
use crate::repo::check_rev;
use crate::worktree::check_branch_name;
use crate::{
    Access, DiffFile, DiffSpec, GitError, GitVersion, Head, LeftOut, MergeCheck, Repository,
    Result, bytes_to_path, command_error, split_nul,
};

/// How [`Repository::integrate`] brings a branch in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integration {
    /// Move the target branch forward to the branch; refuse when it has
    /// diverged.
    FastForwardOnly,
    /// Always create a merge commit (`git merge --no-ff`).
    Merge,
    /// Copy the branch's commits since the merge base onto the target,
    /// oldest first. Commits whose changes the target already has are
    /// skipped; merge commits are refused.
    CherryPick,
}

/// A completed integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrateOutcome {
    /// The target branch's new commit.
    pub new_head: String,
    /// Commits now on the target branch that were not before, oldest first:
    /// the branch's commits for a fast-forward, those plus the merge commit
    /// for a merge, the new copies for a cherry-pick.
    pub commits: Vec<String>,
}

/// Why [`Repository::integrate`] did not integrate. Every variant except
/// `Git` and `AbortFailed` is a refusal made before anything was changed.
#[derive(Debug, thiserror::Error)]
pub enum IntegrateError {
    /// This (target) worktree has staged, unstaged or conflicted files.
    #[error("this worktree has uncommitted changes in {} file(s)", .0.len())]
    Dirty(Vec<PathBuf>),
    /// The worktree that has the branch checked out has uncommitted or
    /// untracked files, which would be left out of the integration.
    #[error("the branch's worktree has {} uncommitted or untracked file(s)", .0.len())]
    SourceDirty(Vec<PathBuf>),
    /// The dry run found conflicts in these files.
    #[error("integrating would conflict in {} file(s)", .0.len())]
    Conflicts(Vec<PathBuf>),
    /// Fast-forward only, and the target has commits the branch lacks.
    #[error("the target branch has diverged; it cannot be fast-forwarded")]
    NotFastForward,
    /// The branch has no commits the target lacks (or, for a cherry-pick,
    /// none that would change anything).
    #[error("nothing to integrate")]
    NothingToIntegrate,
    /// This worktree's `HEAD` is not on a branch.
    #[error("HEAD is detached; check out the branch to integrate into")]
    DetachedHead,
    /// A merge, cherry-pick, revert or rebase is already in progress here.
    #[error("a {0} is in progress in this worktree")]
    InProgress(&'static str),
    /// A cherry-pick was asked for, but the branch has merge commits.
    #[error("the branch has {} merge commit(s); merge it instead of cherry-picking", .0.len())]
    MergeCommits(Vec<String>),
    /// Git failed; any merge or cherry-pick it started was aborted and
    /// `HEAD` is back where it was.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Git failed and the repository could not be put back: the abort
    /// failed, `HEAD` moved, files changed, or an operation someone else
    /// started was found and left alone. The user needs to look at it.
    #[error("{cause}; then restoring the worktree failed: {detail}")]
    AbortFailed { cause: GitError, detail: String },
    /// The target worktree's branch, `HEAD` or state changed while the
    /// checks ran. Nothing was integrated.
    #[error("the target worktree changed during the checks; nothing was integrated")]
    TargetChanged,
    /// Git reported success but the result is not what was asked (an
    /// operation left open, another branch checked out, `HEAD` not moved
    /// forward). The text says what was found and what was done about it.
    #[error("git reported success, but {0}")]
    Unexpected(String),
    /// [`Repository::integrate_reviewed`]: the branch no longer points at
    /// the reviewed commit (new commits, a reset, a rebase). Nothing was
    /// integrated.
    #[error("the branch is at {found}, not at the reviewed commit {expected}")]
    SourceMoved { expected: String, found: String },
    /// [`Repository::integrate_reviewed`]: this worktree has another branch
    /// checked out than the one the review was for. Nothing was integrated.
    #[error("{found} is checked out, not {expected}")]
    WrongTarget { expected: String, found: String },
    /// [`Repository::integrate_reviewed`]: the commit the reviewed work
    /// started from is not on the target branch, so integrating would
    /// bring commits the review did not show. Nothing was integrated.
    #[error("{base} is not on the target branch")]
    BaseNotOnTarget { base: String },
}

/// What [`Repository::integrate_reviewed`] holds the integration to.
#[derive(Debug, Clone, Copy)]
pub struct Reviewed<'a> {
    /// The source commit the review was about (a commit id or any revision
    /// naming one).
    pub commit: &'a str,
    /// The branch the work must go into: the one checked out here.
    pub target: &'a str,
    /// The commit the reviewed work started from. It must be on the target
    /// branch, so the range integrated holds only commits after it.
    pub base: &'a str,
    /// Untracked files (relative to the source worktree's root) that may
    /// stay behind in the worktree that has the branch checked out: the
    /// review left them out on purpose. Any other uncommitted or untracked
    /// file there still refuses the integration.
    pub leave_untracked: &'a [PathBuf],
    /// Tracked files whose unstaged change (and nothing staged) may stay
    /// behind there, for the same reason: the commit holds other contents
    /// for them ([`crate::LeftOut::instead`]).
    pub leave_modified: &'a [PathBuf],
}

/// [`Reviewed`] with the source resolved to a full id.
struct Pinned<'a> {
    commit: String,
    target: &'a str,
    base: &'a str,
    leave_untracked: &'a [PathBuf],
    leave_modified: &'a [PathBuf],
}

/// A worktree's files as an unreferenced commit, from
/// [`Repository::work_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkSnapshot {
    /// The commit: the tree below with the worktree's `HEAD` as parent.
    pub commit: String,
    /// The tree of every tracked and untracked (not ignored) file.
    pub tree: String,
    /// The worktree's `HEAD` the files were taken on, the commit's parent.
    pub head: String,
}

/// A commit's tree and parents, from [`Repository::commit_shape`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitShape {
    /// The commit's full id.
    pub commit: String,
    pub tree: String,
    /// Full ids, in order.
    pub parents: Vec<String>,
}

/// `git merge-tree` accepts trees, not only commits, with `--merge-base`
/// from 2.45. The cherry-pick dry run chains trees.
pub const MERGE_TREE_TREES: GitVersion = GitVersion::new(2, 45, 0);

/// A temporary file removed on drop, along with git's lock file for it.
pub(crate) struct TempFile(pub(crate) PathBuf);

impl TempFile {
    /// Creates a new file holding `contents` in the system temp directory.
    /// `create_new` (O_EXCL) refuses an existing path, so a file or symlink
    /// planted at the name in a shared temp directory is never written
    /// through.
    pub(crate) fn create(tag: &str, contents: &[u8]) -> std::io::Result<Self> {
        Self::create_at(tag, contents, None)
    }

    /// [`create`](TempFile::create), with the modification time set on the
    /// same handle (never through the path, which a shared temp directory
    /// lets others replace).
    fn create_at(
        tag: &str,
        contents: &[u8],
        modified: Option<std::time::SystemTime>,
    ) -> std::io::Result<Self> {
        use std::io::Write;
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let path =
            std::env::temp_dir().join(format!("maestro-{tag}-{}-{n}-{nanos}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let made = Self(path);
        file.write_all(contents)?;
        if let Some(t) = modified {
            file.set_modified(t)?;
        }
        Ok(made)
    }

    /// A copy of the index at `index` (or no file, when there is none)
    /// with the original's modification time. Git trusts an entry's cached
    /// file stats only when the file is older than the index (its "racy
    /// git" check), so a copy with a new time would hide a file rewritten
    /// with the same size in the same second as the checkout.
    fn index_copy(tag: &str, index: &Path) -> Result<Self> {
        // The time is read first: if git replaces the index in between,
        // the copy gets an older time than its contents, which only makes
        // git check more files.
        let modified = std::fs::metadata(index).and_then(|m| m.modified());
        match std::fs::read(index) {
            Ok(bytes) => Ok(Self::create_at(tag, &bytes, Some(modified?))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No index yet: git must find no file (an empty one is an
                // error). It creates it through an exclusive lock file and
                // a rename, which never write through a planted symlink.
                let t = Self::create(tag, &[])?;
                std::fs::remove_file(&t.0)?;
                Ok(t)
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

impl Repository {
    /// Runs commands that create commits: never opens an editor.
    pub(crate) fn writer(&self) -> Repository {
        self.with_git(
            self.git()
                .clone()
                .env("GIT_EDITOR", "true")
                .env("GIT_SEQUENCE_EDITOR", "true")
                .env("GIT_MERGE_AUTOEDIT", "no"),
        )
    }

    pub(crate) fn rev_list(&self, args: &[&str]) -> Result<Vec<String>> {
        let mut all = vec!["rev-list"];
        all.extend_from_slice(args);
        let out = self.run(&all, Access::ReadOnly)?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Everything this worktree changed since it forked from `base`:
    /// commits on its branch, staged and unstaged changes, and untracked
    /// files (as added; ignored files are skipped). The comparison is from
    /// the merge base of `base` and `HEAD` to the files on disk, with rename
    /// detection, so a file moved with plain `mv` shows as a rename.
    ///
    /// Untracked files are found by giving a copy of the index an
    /// intent-to-add entry for each, in a temporary file. The real index and
    /// the split index files it relies on are not touched. The only object
    /// this can write is the empty blob, which intent-to-add entries name.
    pub fn review_diff(&self, base: &str) -> Result<Vec<DiffFile>> {
        check_rev(base)?;
        let out = self.run(
            &["merge-base", "--end-of-options", base, "HEAD"],
            Access::ReadOnly,
        )?;
        let fork = String::from_utf8_lossy(&out).trim().to_string();
        check_rev(&fork)?;

        let untracked = self.run(
            &["ls-files", "-z", "--others", "--exclude-standard"],
            Access::ReadOnly,
        )?;
        // The working tree (through the index) against the fork point.
        let mut args = diff_args(&DiffSpec::Unstaged)?;
        args.push(fork.into());
        args.push("--".into());
        if untracked.is_empty() {
            return self.parse_diff(&args);
        }

        // A copy of the index keeps its cached file stats, so git does not
        // have to rehash every tracked file. Git replaces the index by
        // renaming, so this reads one whole version even if git is writing.
        let index = TempFile::index_copy("review-index", &self.git_dir().join("index"))?;
        let list = TempFile::create("review-paths", &untracked)?;
        // With a split index, the copy points at shared index files in the
        // real git directory, and writing it could make a new shared file
        // and expire the one the real index needs. Writing the copy as one
        // whole index never touches them.
        let shadow = self.with_git(
            self.git()
                .clone()
                .env("GIT_INDEX_FILE", &index.0)
                .config("core.splitIndex=false"),
        );
        let mut from_file = OsString::from("--pathspec-from-file=");
        from_file.push(&list.0);
        shadow.run(
            &[
                OsString::from("add"),
                "--intent-to-add".into(),
                "--pathspec-file-nul".into(),
                from_file,
            ],
            Access::Write,
        )?;
        shadow.parse_diff(&args)
    }

    /// Every path that any commit in `from..to` changes, each once, with
    /// renames as their two paths and merge commits compared with each of
    /// their parents. A net diff can hide a file changed and changed back;
    /// integrating by cherry-pick applies such commits one by one, so this
    /// is what a check of everything a branch could bring needs.
    pub fn paths_touched(&self, from: &str, to: &str) -> Result<Vec<PathBuf>> {
        check_rev(from)?;
        check_rev(to)?;
        let range = format!("{from}..{to}");
        let out = self.run(
            &[
                "log",
                "--no-renames",
                "-m",
                "--name-only",
                "-z",
                "--format=",
                "--end-of-options",
                &range,
            ],
            Access::ReadOnly,
        )?;
        let mut paths: Vec<PathBuf> = split_nul(&out)
            .into_iter()
            .map(|p| {
                // `--format=` leaves the newline that ends each commit's
                // (empty) header in front of its first path.
                let p = p.strip_prefix(b"\n").unwrap_or(p);
                bytes_to_path(p)
            })
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// A commit holding everything in this worktree as it is on disk:
    /// `HEAD` plus staged, unstaged and untracked files (ignored files left
    /// out), with `HEAD` as its parent. Nothing the user sees changes: it
    /// is built in a temporary copy of the index, no ref or reflog points
    /// at it, and `HEAD`, the real index and the working tree are not
    /// touched. It does write the worktree's file contents and the commit
    /// to the object store as unreferenced objects, which `git gc` drops.
    ///
    /// Author, committer, dates and message are fixed, so the same files on
    /// the same `HEAD` always give the same commit id; callers can cache
    /// on it. Used for merge dry runs between worktrees that hold
    /// uncommitted work ([`Repository::merge_check`]).
    pub fn work_snapshot(&self) -> Result<WorkSnapshot> {
        self.work_snapshot_leaving_out(&[])
    }

    /// [`Repository::work_snapshot`] with the files named in `leave_out`
    /// left out (relative to the root, matched by exact name): what
    /// [`Repository::commit_all_leaving_out`] with the same list would
    /// commit. An untracked path in it is not added (one already in the
    /// index stays); a tracked path given contents to commit instead has
    /// those.
    pub fn work_snapshot_leaving_out(&self, leave_out: &[LeftOut]) -> Result<WorkSnapshot> {
        let head = self.resolve("HEAD")?;
        let index = TempFile::index_copy("snapshot-index", &self.git_dir().join("index"))?;
        // As in `review_diff`: a whole index, never new shared index files.
        let shadow = self.with_git(
            self.git()
                .clone()
                .env("GIT_INDEX_FILE", &index.0)
                .config("core.splitIndex=false"),
        );
        shadow.add_all(leave_out)?;
        let out = shadow.run(&["write-tree"], Access::Write)?;
        let tree = String::from_utf8_lossy(&out).trim().to_string();
        check_rev(&tree)?;
        // The parent must be the HEAD the files were taken on; a commit made
        // in between would give the new files an old ancestry.
        if self.resolve("HEAD")? != head {
            return Err(GitError::InvalidArgument(
                "HEAD moved while the worktree was being read; try again".into(),
            ));
        }
        let commit = self.commit_tree(&tree, &[&head], "maestro work snapshot")?;
        Ok(WorkSnapshot { commit, tree, head })
    }

    /// The tree and parents of the commit `rev` names, read with
    /// `cat-file`, which no configuration changes.
    pub fn commit_shape(&self, rev: &str) -> Result<CommitShape> {
        let commit = self.resolve(rev)?;
        let out = self.run(&["cat-file", "commit", &commit], Access::ReadOnly)?;
        let text = String::from_utf8_lossy(&out);
        let mut tree = None;
        let mut parents = Vec::new();
        // The header ends at the first empty line.
        for line in text.lines().take_while(|l| !l.is_empty()) {
            if let Some(t) = line.strip_prefix("tree ") {
                tree = Some(t.trim().to_string());
            } else if let Some(p) = line.strip_prefix("parent ") {
                parents.push(p.trim().to_string());
            }
        }
        let tree = tree.ok_or_else(|| GitError::Parse {
            args: format!("cat-file commit {commit}"),
            detail: "no tree line".into(),
        })?;
        Ok(CommitShape {
            commit,
            tree,
            parents,
        })
    }

    /// Writes a commit object for `tree` with `parents` and returns its id,
    /// without moving any ref. Author and committer are fixed (`maestro`,
    /// the Unix epoch), so the id depends only on the tree, the parents and
    /// the message. For dry runs only: the commit is unreferenced.
    pub fn commit_tree(&self, tree: &str, parents: &[&str], message: &str) -> Result<String> {
        check_rev(tree)?;
        let mut args: Vec<OsString> =
            vec!["commit-tree".into(), "--no-gpg-sign".into(), tree.into()];
        for p in parents {
            check_rev(p)?;
            args.push("-p".into());
            args.push((*p).into());
        }
        args.push("-m".into());
        args.push(message.into());
        let fixed = self.with_git(
            self.git()
                .clone()
                .env("GIT_AUTHOR_NAME", "maestro")
                .env("GIT_AUTHOR_EMAIL", "maestro@localhost")
                .env("GIT_AUTHOR_DATE", "@0 +0000")
                .env("GIT_COMMITTER_NAME", "maestro")
                .env("GIT_COMMITTER_EMAIL", "maestro@localhost")
                .env("GIT_COMMITTER_DATE", "@0 +0000")
                .config("commit.gpgSign=false"),
        );
        let out = fixed.run(&args, Access::Write)?;
        let commit = String::from_utf8_lossy(&out).trim().to_string();
        check_rev(&commit)?;
        Ok(commit)
    }

    fn parse_diff(&self, args: &[OsString]) -> Result<Vec<DiffFile>> {
        let out = self.run(args, Access::ReadOnly)?;
        crate::diff::parse_unified(&out).map_err(|detail| GitError::Parse {
            args: "diff".into(),
            detail,
        })
    }

    /// Fails with [`GitError::NoIdentity`] when git has no author or
    /// committer name and email (and would refuse to commit).
    pub(crate) fn check_identity(&self) -> Result<()> {
        for var in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
            let (out, _) = self.run_status(&["var", var], Access::ReadOnly)?;
            if !out.status.success() {
                return Err(GitError::NoIdentity(
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Stages everything (untracked files included, ignored files not) and
    /// commits it with `message`. Returns the new commit, or `None` when
    /// there was nothing to commit. The user's hooks and configuration
    /// apply. Refuses with [`GitError::Unmerged`] while files are in
    /// conflict, so conflict markers are not committed, and with
    /// [`GitError::NoIdentity`] before staging anything when git has no user
    /// name and email. When the commit itself fails (a hook rejects it), the
    /// files stay staged and the working tree is unchanged.
    ///
    /// The conflict check runs before staging. If another program starts a
    /// merge in this worktree in between, staging has already resolved its
    /// conflicts with the markers in them; the merge state is checked again
    /// after staging and the commit is refused, but the conflict entries in
    /// the index are gone.
    pub fn commit_all(&self, message: &str) -> Result<Option<String>> {
        self.commit_all_leaving_out(message, &[])
    }

    /// `git add --all`, or with `leave_out`, every path but the untracked
    /// files named there (each matched by its exact name, relative to the
    /// root), and then the index entries of those given contents to commit
    /// instead set to them. They keep the mode `git add` just gave them
    /// from the file on disk, so a mode the agent gave the file is kept;
    /// one that is no longer a regular file refuses.
    fn add_all(&self, leave_out: &[LeftOut]) -> Result<()> {
        if leave_out.is_empty() {
            self.run(&["add", "--all", "--", "."], Access::Write)?;
            return Ok(());
        }
        // Checked before anything is staged.
        let instead: Vec<(&PathBuf, &str)> = leave_out
            .iter()
            .filter_map(|l| l.instead.as_deref().map(|b| (&l.path, b)))
            .collect();
        for (p, blob) in &instead {
            if !crate::untracked::is_object_id(blob) {
                return Err(GitError::InvalidArgument(format!(
                    "{blob:?} is not an object id (for {})",
                    p.display()
                )));
            }
        }
        // `update-index` takes an id it does not have, and the index would
        // then name a missing object.
        let ids: Vec<String> = instead.iter().map(|(_, b)| b.to_string()).collect();
        let present = self.objects_present(&ids)?;
        if let Some((p, blob)) = instead
            .iter()
            .find(|(_, b)| !present.iter().any(|q| q == b))
        {
            return Err(GitError::InvalidArgument(format!(
                "the contents to commit for {} ({blob}) are not in the object store",
                p.display()
            )));
        }
        let paths: Vec<PathBuf> = leave_out
            .iter()
            .filter(|l| l.instead.is_none())
            .map(|l| l.path.clone())
            .collect();
        let mut args: Vec<OsString> = vec!["add".into(), "--all".into(), "--".into()];
        args.extend(crate::untracked::all_but(&paths)?);
        self.with_magic_pathspecs().run(&args, Access::Write)?;
        if instead.is_empty() {
            return Ok(());
        }
        let wanted: Vec<PathBuf> = instead.iter().map(|(p, _)| (*p).clone()).collect();
        let modes = self.tracked_modes(&wanted)?;
        let mut input = Vec::new();
        for (p, blob) in &instead {
            let Some((_, mode)) = modes.iter().find(|(q, _)| q == *p) else {
                return Err(GitError::InvalidArgument(format!(
                    "{} is no longer a regular tracked file",
                    p.display()
                )));
            };
            input.extend_from_slice(format!("{mode} {blob}\t").as_bytes());
            input.extend_from_slice(p.as_os_str().as_encoded_bytes());
            input.push(0);
        }
        self.run_input(
            &["update-index", "-z", "--index-info"],
            &input,
            Access::Write,
        )?;
        Ok(())
    }

    /// The modes of the stage 0 index entries of `paths` that are regular
    /// files.
    fn tracked_modes(&self, paths: &[PathBuf]) -> Result<Vec<(PathBuf, String)>> {
        let mut args: Vec<OsString> = ["ls-files", "-z", "--stage", "--"]
            .into_iter()
            .map(OsString::from)
            .collect();
        args.extend(paths.iter().map(|p| p.as_os_str().to_os_string()));
        let out = self.run(&args, Access::ReadOnly)?;
        let mut modes = Vec::new();
        for entry in split_nul(&out) {
            let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
                continue;
            };
            let head = String::from_utf8_lossy(&entry[..tab]);
            let fields: Vec<&str> = head.split(' ').collect();
            if let [mode @ ("100644" | "100755"), _, "0"] = fields.as_slice() {
                modes.push((bytes_to_path(&entry[tab + 1..]), mode.to_string()));
            }
        }
        Ok(modes)
    }

    /// [`Repository::commit_all`], leaving out the files named in
    /// `leave_out` (relative to the root, matched by exact name). An
    /// untracked one stays untracked and out of the commit; one that is
    /// already staged is committed, as this only keeps `git add` from
    /// picking it up. A tracked one given contents to commit instead
    /// ([`LeftOut::instead`]) is committed with them, and the file on disk
    /// is left as it is (so it then differs from the commit). Returns
    /// `None` when nothing else was there to commit.
    pub fn commit_all_leaving_out(
        &self,
        message: &str,
        leave_out: &[LeftOut],
    ) -> Result<Option<String>> {
        let status = self.status()?;
        let conflicted: Vec<PathBuf> = status.conflicted().map(|e| e.path.clone()).collect();
        if !conflicted.is_empty() {
            return Err(GitError::Unmerged(conflicted));
        }
        let merging = self.pseudo_ref("MERGE_HEAD")?;
        // A clean worktree can still commit other contents for a file.
        let instead = leave_out.iter().any(|l| l.instead.is_some());
        if status.is_clean() && merging.is_none() && !instead {
            return Ok(None);
        }
        self.check_identity()?;
        self.add_all(leave_out)?;
        if self.pseudo_ref("MERGE_HEAD")? != merging {
            return Err(GitError::InvalidArgument(
                "a merge started in this worktree while committing; nothing was committed".into(),
            ));
        }
        let merging = merging.is_some();
        let (out, shown) = self.run_status(&["diff", "--cached", "--quiet"], Access::ReadOnly)?;
        match out.status.code() {
            Some(0) if !merging => return Ok(None),
            Some(0 | 1) => {}
            _ => return Err(command_error(shown, &out)),
        }
        let args: Vec<OsString> = vec![
            "commit".into(),
            "--quiet".into(),
            "--no-edit".into(),
            "-m".into(),
            message.into(),
        ];
        self.writer().run(&args, Access::Write)?;
        Ok(Some(self.resolve("HEAD")?))
    }

    /// Takes one file out of the work under review: afterward the file is
    /// as it was at the fork point of `base` and `HEAD`, the point
    /// [`Repository::review_diff`] compares against. `old_path` (the file at
    /// the fork point, if it existed) is restored there in the index and the
    /// working tree; `new_path` (the file now, if it exists and differs from
    /// `old_path`) is removed from both. For a modified file both are the
    /// same path; for an added file `old_path` is `None`; for a deleted
    /// file `new_path` is `None`; for a rename both are given. An "added"
    /// path that existed at the fork point (git shows a file replaced by a
    /// symlink as a deletion plus an addition) is restored, not just
    /// removed.
    ///
    /// This discards the worktree's change to that file. Paths are relative
    /// to this worktree's root; absolute paths and `..` are refused. A
    /// directory at either path, or a parent of either that is not a plain
    /// directory (a symlink, for one), is refused rather than touched. The
    /// removal target is checked again just before it is removed; another
    /// process that swaps a parent for a symlink in between can still win
    /// that race, since std has no `unlinkat`.
    pub fn drop_change(
        &self,
        base: &str,
        old_path: Option<&Path>,
        new_path: Option<&Path>,
    ) -> Result<()> {
        check_rev(base)?;
        for p in [old_path, new_path].into_iter().flatten() {
            let plain = p.components().all(|c| matches!(c, Component::Normal(_)));
            if p.as_os_str().is_empty() || !plain {
                return Err(GitError::InvalidArgument(format!(
                    "{} is not a path inside the worktree",
                    p.display()
                )));
            }
        }
        if old_path.is_none() && new_path.is_none() {
            return Err(GitError::InvalidArgument("no path to drop".into()));
        }
        let out = self.run(
            &["merge-base", "--end-of-options", base, "HEAD"],
            Access::ReadOnly,
        )?;
        let fork = String::from_utf8_lossy(&out).trim().to_string();
        check_rev(&fork)?;
        let old_path = match (old_path, new_path) {
            (None, Some(n)) if self.exists_at(&fork, n)? => Some(n),
            _ => old_path,
        };
        for p in [old_path, new_path].into_iter().flatten() {
            self.check_drop_target(p)?;
        }

        if let Some(new) = new_path.filter(|n| Some(*n) != old_path) {
            let args: Vec<OsString> = vec![
                "rm".into(),
                "-q".into(),
                "--cached".into(),
                "--ignore-unmatch".into(),
                "--".into(),
                new.into(),
            ];
            self.run(&args, Access::Write)?;
            self.check_drop_target(new)?;
            match std::fs::remove_file(self.root().join(new)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(GitError::Io(e)),
            }
        }
        if let Some(old) = old_path {
            let args: Vec<OsString> = vec![
                "restore".into(),
                format!("--source={fork}").into(),
                "--staged".into(),
                "--worktree".into(),
                "--".into(),
                old.into(),
            ];
            self.run(&args, Access::Write)?;
        }
        Ok(())
    }

    /// True when `path` names an entry in commit `rev`.
    fn exists_at(&self, rev: &str, path: &Path) -> Result<bool> {
        let args: Vec<OsString> = vec![
            "ls-tree".into(),
            "-z".into(),
            "--name-only".into(),
            "--end-of-options".into(),
            rev.into(),
            "--".into(),
            path.into(),
        ];
        Ok(!self.run(&args, Access::ReadOnly)?.is_empty())
    }

    /// Refuses a drop target that is a directory, or whose parents inside
    /// the worktree are not all plain directories. A path that does not
    /// exist (from some component on) is fine.
    fn check_drop_target(&self, path: &Path) -> Result<()> {
        let parts: Vec<_> = path.components().collect();
        let mut at = self.root().to_path_buf();
        for (i, part) in parts.iter().enumerate() {
            at.push(part);
            let meta = match std::fs::symlink_metadata(&at) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(GitError::Io(e)),
            };
            let last = i + 1 == parts.len();
            if last && meta.is_dir() {
                return Err(GitError::InvalidArgument(format!(
                    "{} is a directory; not touching it",
                    path.display()
                )));
            }
            if !last && !meta.is_dir() {
                return Err(GitError::InvalidArgument(format!(
                    "{} is not a plain directory (a symlink or a file); not touching {}",
                    at.strip_prefix(self.root()).unwrap_or(&at).display(),
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// The commit a pseudo-ref such as `MERGE_HEAD` names in this worktree,
    /// or `None` when it is not set. Asked through git, not by looking for a
    /// file, because the reftable backend does not keep most of them as
    /// files.
    fn pseudo_ref(&self, name: &str) -> Result<Option<String>> {
        let (out, shown) = self.run_status(
            &["rev-parse", "--verify", "--quiet", name],
            Access::ReadOnly,
        )?;
        match out.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(command_error(shown, &out)),
        }
    }

    /// A merge, cherry-pick, revert or rebase in progress in this worktree.
    fn operation_in_progress(&self) -> Result<Option<&'static str>> {
        for (name, what) in [
            ("MERGE_HEAD", "merge"),
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
        ] {
            if self.pseudo_ref(name)?.is_some() {
                return Ok(Some(what));
            }
        }
        // State directories, always plain directories in the git dir.
        let d = self.git_dir();
        Ok([
            ("sequencer", "cherry-pick or revert"),
            ("rebase-merge", "rebase"),
            ("rebase-apply", "rebase or am"),
        ]
        .into_iter()
        .find(|(f, _)| d.join(f).exists())
        .map(|(_, what)| what))
    }

    /// Integrates `branch` (a local branch) into the branch checked out in
    /// this (target) worktree.
    ///
    /// Refuses, changing nothing, when this worktree is detached or in the
    /// middle of another operation, has staged, unstaged or conflicted
    /// files, when the worktree that has `branch` checked out has
    /// uncommitted or untracked files, when there is nothing to integrate,
    /// when fast-forward only cannot fast-forward, and when the
    /// `merge-tree` dry run finds conflicts. Untracked files in this
    /// worktree do not block it: git refuses to overwrite them.
    ///
    /// The exact commit the checks ran on is what gets integrated, even if
    /// the branch moves meanwhile. The target is checked again just before
    /// git runs ([`IntegrateError::TargetChanged`]); a change in the last
    /// moment before that is not caught. The user's hooks run;
    /// `branch.<target>.mergeOptions` does not apply.
    ///
    /// If git fails part way (a hook rejects the commit, an untracked file
    /// is in the way), the merge or cherry-pick this call started is
    /// aborted, and [`IntegrateError::Git`] means `HEAD` is back, nothing is
    /// in progress and no tracked file differs from it. Anything else comes
    /// back as [`IntegrateError::AbortFailed`] with what was found.
    ///
    /// Like `git merge`, it may overwrite ignored files that the branch
    /// adds as tracked files.
    ///
    /// Whatever commit `branch` points at when the checks start is what
    /// gets integrated. To integrate only what someone reviewed, use
    /// [`Repository::integrate_reviewed`].
    pub fn integrate(
        &self,
        branch: &str,
        how: Integration,
    ) -> Result<IntegrateOutcome, IntegrateError> {
        self.integrate_inner(branch, None, how)
    }

    /// [`Repository::integrate`], held to what was reviewed. Refuses,
    /// changing nothing:
    ///
    /// - with [`IntegrateError::SourceMoved`] when `branch` does not point
    ///   at `reviewed.commit`, both before the checks and again just before
    ///   git runs;
    /// - with [`IntegrateError::WrongTarget`] when this worktree has another
    ///   branch than `reviewed.target` checked out;
    /// - with [`IntegrateError::BaseNotOnTarget`] when `reviewed.base` is
    ///   not an ancestor of the target's `HEAD`.
    ///
    /// The target's branch and `HEAD` are checked again just before git
    /// runs, as for `integrate`. Git is given the commit id, never the
    /// branch name, so a branch that moves after that last check still
    /// cannot bring in anything but the reviewed commit and its history
    /// since the target's `HEAD` (for a cherry-pick, the commits the checks
    /// listed).
    pub fn integrate_reviewed(
        &self,
        branch: &str,
        reviewed: Reviewed<'_>,
        how: Integration,
    ) -> Result<IntegrateOutcome, IntegrateError> {
        let pinned = Pinned {
            commit: self.resolve(reviewed.commit)?,
            target: reviewed.target,
            base: reviewed.base,
            leave_untracked: reviewed.leave_untracked,
            leave_modified: reviewed.leave_modified,
        };
        self.integrate_inner(branch, Some(&pinned), how)
    }

    fn integrate_inner(
        &self,
        branch: &str,
        pinned: Option<&Pinned<'_>>,
        how: Integration,
    ) -> Result<IntegrateOutcome, IntegrateError> {
        check_branch_name(self, branch)?;
        let Head::Branch(target) = self.head()? else {
            return Err(IntegrateError::DetachedHead);
        };
        if let Some(p) = pinned
            && p.target != target
        {
            return Err(IntegrateError::WrongTarget {
                expected: p.target.to_string(),
                found: target,
            });
        }
        if let Some(op) = self.operation_in_progress()? {
            return Err(IntegrateError::InProgress(op));
        }
        let branch_ref = format!("refs/heads/{branch}");
        let source = self.resolve(&branch_ref)?;
        let moved = |found: String| -> Result<(), IntegrateError> {
            match pinned {
                Some(p) if p.commit != found => Err(IntegrateError::SourceMoved {
                    expected: p.commit.clone(),
                    found,
                }),
                _ => Ok(()),
            }
        };
        moved(source.clone())?;
        let old_head = self.resolve("HEAD")?;
        // `old_head` is checked again before git runs, so this holds for
        // the `HEAD` git merges into.
        if let Some(p) = pinned
            && !self.can_fast_forward(p.base, &old_head)?
        {
            return Err(IntegrateError::BaseNotOnTarget {
                base: p.base.to_string(),
            });
        }

        let dirty: Vec<PathBuf> = self
            .status()?
            .uncommitted()
            .map(|e| e.path.clone())
            .collect();
        if !dirty.is_empty() {
            return Err(IntegrateError::Dirty(dirty));
        }
        self.check_source_worktree(
            branch,
            pinned.map_or(&[], |p| p.leave_untracked),
            pinned.map_or(&[], |p| p.leave_modified),
        )?;

        let range = format!("{old_head}..{source}");
        let mut commits = self.rev_list(&["--reverse", &range])?;
        if commits.is_empty() {
            return Err(IntegrateError::NothingToIntegrate);
        }
        match how {
            Integration::FastForwardOnly => {
                if !self.can_fast_forward(&old_head, &source)? {
                    return Err(IntegrateError::NotFastForward);
                }
            }
            Integration::Merge => {
                if let MergeCheck::Conflicts { files, .. } = self.merge_check(&old_head, &source)? {
                    return Err(IntegrateError::Conflicts(files));
                }
            }
            Integration::CherryPick => {
                let merges = self.rev_list(&["--merges", &range])?;
                if !merges.is_empty() {
                    return Err(IntegrateError::MergeCommits(merges));
                }
                commits = self.cherry_pick_check(&old_head, &commits)?;
                if commits.is_empty() {
                    return Err(IntegrateError::NothingToIntegrate);
                }
            }
        }

        // `--commit --no-squash` and an empty `branch.<target>.mergeOptions`
        // keep the user's merge defaults from turning this into a merge
        // left open for a commit, a squash, or another strategy than the
        // dry run used.
        let args: Vec<String> = match how {
            Integration::FastForwardOnly => vec![
                "merge".into(),
                "--ff-only".into(),
                "--commit".into(),
                "--no-squash".into(),
                source.clone(),
            ],
            Integration::Merge => vec![
                "merge".into(),
                "--no-ff".into(),
                "--commit".into(),
                "--no-squash".into(),
                "--no-edit".into(),
                "-m".into(),
                format!("Merge branch '{branch}' into {target}"),
                source.clone(),
            ],
            Integration::CherryPick => {
                let mut v = vec!["cherry-pick".to_string()];
                v.extend(commits.iter().cloned());
                v
            }
        };
        let writer = self.with_git(
            self.writer()
                .git()
                .clone()
                .config(format!("branch.{target}.mergeOptions=")),
        );

        // The checks took a moment; make sure the target is still where
        // they left it. This narrows the window, it cannot close it.
        if self.head()? != Head::Branch(target.clone())
            || self.resolve("HEAD")? != old_head
            || self.operation_in_progress()?.is_some()
        {
            return Err(IntegrateError::TargetChanged);
        }
        // And that the branch still points at the reviewed commit. A move
        // after this cannot change what is integrated: git gets `source`.
        moved(self.resolve(&branch_ref)?)?;

        let (out, shown) = writer.run_status(&args, Access::Write)?;
        if !out.status.success() {
            let cause = command_error(shown, &out);
            return Err(
                match self.restore_after_failure(&old_head, &source, &commits) {
                    Ok(()) => IntegrateError::Git(cause),
                    Err(detail) => IntegrateError::AbortFailed { cause, detail },
                },
            );
        }

        // Git said yes. Check it did what was asked: no operation left open,
        // the same branch checked out, and HEAD moved forward from where it
        // was.
        if let Some(op) = self.operation_in_progress()? {
            let detail = match self.restore_after_failure(&old_head, &source, &commits) {
                Ok(()) => format!("git left a {op} open; it was aborted"),
                Err(e) => format!("git left a {op} open, and restoring failed: {e}"),
            };
            return Err(IntegrateError::Unexpected(detail));
        }
        let new_head = self.resolve("HEAD")?;
        let mut wrong = Vec::new();
        if self.head()? != Head::Branch(target.clone()) {
            wrong.push(format!("{target} is no longer checked out"));
        }
        if new_head == old_head || !self.can_fast_forward(&old_head, &new_head)? {
            wrong.push(format!(
                "HEAD is at {new_head}, not a descendant of {old_head}"
            ));
        }
        if !wrong.is_empty() {
            return Err(IntegrateError::Unexpected(wrong.join("; ")));
        }
        let commits = self.rev_list(&["--reverse", &format!("{old_head}..{new_head}")])?;
        Ok(IntegrateOutcome { new_head, commits })
    }

    /// Refuses when the worktree that has `branch` checked out (if any,
    /// other than this one) has uncommitted or untracked files, other than
    /// the untracked files named in `leave_untracked` and the files named in
    /// `leave_modified` with only an unstaged modification.
    fn check_source_worktree(
        &self,
        branch: &str,
        leave_untracked: &[PathBuf],
        leave_modified: &[PathBuf],
    ) -> Result<(), IntegrateError> {
        let here = std::fs::canonicalize(self.root()).unwrap_or_else(|_| self.root().into());
        for wt in self.worktrees()? {
            if wt.branch.as_deref() != Some(branch) || wt.prunable.is_some() || wt.bare {
                continue;
            }
            let path = std::fs::canonicalize(&wt.path).unwrap_or(wt.path);
            if path == here {
                continue;
            }
            let Some(repo) = self.git().discover(&path)? else {
                continue;
            };
            let status = repo.status()?;
            let files: Vec<PathBuf> = status
                .uncommitted()
                .filter(|e| {
                    let only_unstaged = e.orig_path.is_none()
                        && e.state
                            == crate::EntryState::Changed {
                                staged: None,
                                unstaged: Some(crate::ChangeKind::Modified),
                            };
                    !(only_unstaged && leave_modified.contains(&e.path))
                })
                .map(|e| e.path.clone())
                .chain(
                    status
                        .untracked()
                        .map(|e| e.path.clone())
                        .filter(|p| !leave_untracked.contains(p)),
                )
                .collect();
            if !files.is_empty() {
                return Err(IntegrateError::SourceDirty(files));
            }
        }
        Ok(())
    }

    /// Applies `commits` one by one onto `onto` in memory, the way
    /// `cherry-pick` would, and returns the ones that change something.
    /// Writes objects (merged trees and blobs) to the object store, never
    /// refs, the index or files.
    fn cherry_pick_check(
        &self,
        onto: &str,
        commits: &[String],
    ) -> Result<Vec<String>, IntegrateError> {
        self.cherry_pick_trees(onto, commits).map(|(kept, _)| kept)
    }

    /// [`Repository::cherry_pick_check`], also returning the tree the last
    /// commit leaves.
    pub(crate) fn cherry_pick_trees(
        &self,
        onto: &str,
        commits: &[String],
    ) -> Result<(Vec<String>, String), IntegrateError> {
        let have = self.git().version()?;
        if have < MERGE_TREE_TREES {
            return Err(GitError::Unsupported {
                feature: "a cherry-pick dry run (merge-tree with trees)",
                need: MERGE_TREE_TREES,
                have,
            }
            .into());
        }
        let tree_of = |rev: &str| -> Result<String> {
            let out = self.run(
                &["rev-parse", "--verify", "--end-of-options", rev],
                Access::ReadOnly,
            )?;
            Ok(String::from_utf8_lossy(&out).trim().to_string())
        };
        let mut current = tree_of(&format!("{onto}^{{tree}}"))?;
        let mut kept = Vec::new();
        for c in commits {
            check_rev(c)?;
            let base = format!("--merge-base={c}^");
            let (out, shown) = self.run_status(
                &[
                    "merge-tree",
                    "--write-tree",
                    "-z",
                    "--name-only",
                    "--no-messages",
                    &base,
                    &current,
                    c,
                ],
                Access::ReadOnly,
            )?;
            let conflicted = match out.status.code() {
                Some(0) => false,
                Some(1) => true,
                _ => return Err(command_error(shown, &out).into()),
            };
            let fields = split_nul(&out.stdout);
            let Some((tree, files)) = fields.split_first() else {
                return Err(GitError::Parse {
                    args: shown,
                    detail: "no tree id".into(),
                }
                .into());
            };
            if conflicted {
                let mut files: Vec<PathBuf> = files.iter().map(|f| bytes_to_path(f)).collect();
                files.dedup();
                return Err(IntegrateError::Conflicts(files));
            }
            let tree = String::from_utf8_lossy(tree).trim().to_string();
            if tree != current {
                kept.push(c.clone());
                current = tree;
            }
        }
        Ok((kept, current))
    }

    /// After a failed merge or cherry-pick (of `source`, or of `commits`
    /// onto `old_head`): aborts the operation git left open, if it is the
    /// one this call started, then checks that `HEAD` is back at `old_head`,
    /// nothing is in progress, and no tracked file differs from `HEAD` (the
    /// target was clean before). An operation started by someone else in
    /// the meantime is left alone and reported. The error text says what
    /// could not be restored.
    fn restore_after_failure(
        &self,
        old_head: &str,
        source: &str,
        commits: &[String],
    ) -> Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        let abort = |args: &[&str]| self.run(args, Access::Write).err().map(|e| e.to_string());
        match self.pseudo_ref("MERGE_HEAD") {
            Ok(Some(m)) if m == source => problems.extend(abort(&["merge", "--abort"])),
            Ok(Some(m)) => problems.push(format!(
                "a merge of {m} that this integration did not start is in progress; left alone"
            )),
            Ok(None) => {}
            Err(e) => problems.push(e.to_string()),
        }
        let picking = match self.pseudo_ref("CHERRY_PICK_HEAD") {
            Ok(p) => p,
            Err(e) => {
                problems.push(e.to_string());
                None
            }
        };
        let sequencer = self.git_dir().join("sequencer");
        // `sequencer/head` holds the HEAD a multi-commit pick started from.
        let seq_head = std::fs::read_to_string(sequencer.join("head"))
            .ok()
            .map(|s| s.trim().to_string());
        if picking.is_some() || sequencer.exists() {
            let ours = picking.as_ref().is_some_and(|p| commits.contains(p))
                || seq_head.as_deref() == Some(old_head);
            if ours {
                problems.extend(abort(&["cherry-pick", "--abort"]));
            } else {
                problems.push(
                    "a cherry-pick that this integration did not start is in progress; left alone"
                        .into(),
                );
            }
        }
        match self.resolve("HEAD") {
            Ok(now) if now == old_head => {}
            Ok(now) => problems.push(format!("HEAD is at {now}, it was {old_head}")),
            Err(e) => problems.push(e.to_string()),
        }
        match self.operation_in_progress() {
            Ok(Some(op)) => problems.push(format!("a {op} is still in progress")),
            Ok(None) => {}
            Err(e) => problems.push(e.to_string()),
        }
        // Git can fail part way through updating files without leaving any
        // operation to abort (a failing filter, a full disk).
        match self.status() {
            Ok(s) => {
                let changed: Vec<String> = s
                    .uncommitted()
                    .map(|e| e.path.display().to_string())
                    .collect();
                if !changed.is_empty() {
                    problems.push(format!(
                        "these files now differ from HEAD: {}",
                        changed.join(", ")
                    ));
                }
            }
            Err(e) => problems.push(e.to_string()),
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}
