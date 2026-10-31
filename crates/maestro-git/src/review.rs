//! Reviewing and landing a worktree's work: the review diff against a base,
//! committing everything, and integrating a branch with safety checks.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::diff::diff_args;
use crate::repo::check_rev;
use crate::worktree::check_branch_name;
use crate::{
    Access, DiffFile, DiffSpec, GitError, GitVersion, Head, MergeCheck, Repository, Result,
    bytes_to_path, command_error, split_nul,
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
    /// failed or `HEAD` moved. The user needs to look at it.
    #[error("{cause}; then restoring the worktree failed: {detail}")]
    AbortFailed { cause: GitError, detail: String },
}

/// `git merge-tree` accepts trees, not only commits, with `--merge-base`
/// from 2.45. The cherry-pick dry run chains trees.
pub const MERGE_TREE_TREES: GitVersion = GitVersion::new(2, 45, 0);

/// A temporary file removed on drop, along with git's lock file for it.
struct TempFile(PathBuf);

impl TempFile {
    /// Creates a new file holding `contents` in the system temp directory.
    /// `create_new` (O_EXCL) refuses an existing path, so a file or symlink
    /// planted at the name in a shared temp directory is never written
    /// through.
    fn create(tag: &str, contents: &[u8]) -> std::io::Result<Self> {
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
        Ok(made)
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
    fn writer(&self) -> Repository {
        self.with_git(
            self.git()
                .clone()
                .env("GIT_EDITOR", "true")
                .env("GIT_SEQUENCE_EDITOR", "true")
                .env("GIT_MERGE_AUTOEDIT", "no"),
        )
    }

    fn rev_list(&self, args: &[&str]) -> Result<Vec<String>> {
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
    /// intent-to-add entry for each, in a temporary file; the real index is
    /// not touched and no objects are written.
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
        let index = match std::fs::read(self.git_dir().join("index")) {
            Ok(bytes) => TempFile::create("review-index", &bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No index yet: git must find no file (an empty one is an
                // error). It creates it through an exclusive lock file and
                // a rename, which never write through a planted symlink.
                let t = TempFile::create("review-index", &[])?;
                std::fs::remove_file(&t.0)?;
                t
            }
            Err(e) => return Err(e.into()),
        };
        let list = TempFile::create("review-paths", &untracked)?;
        let shadow = self.with_git(self.git().clone().env("GIT_INDEX_FILE", &index.0));
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

    fn parse_diff(&self, args: &[OsString]) -> Result<Vec<DiffFile>> {
        let out = self.run(args, Access::ReadOnly)?;
        crate::diff::parse_unified(&out).map_err(|detail| GitError::Parse {
            args: "diff".into(),
            detail,
        })
    }

    /// Fails with [`GitError::NoIdentity`] when git has no author or
    /// committer name and email (and would refuse to commit).
    fn check_identity(&self) -> Result<()> {
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
    /// conflict, so conflict markers are never committed, and with
    /// [`GitError::NoIdentity`] before staging anything when git has no user
    /// name and email. When the commit itself fails (a hook rejects it), the
    /// files stay staged and the working tree is unchanged.
    pub fn commit_all(&self, message: &str) -> Result<Option<String>> {
        let status = self.status()?;
        let conflicted: Vec<PathBuf> = status.conflicted().map(|e| e.path.clone()).collect();
        if !conflicted.is_empty() {
            return Err(GitError::Unmerged(conflicted));
        }
        let merging = self.git_dir().join("MERGE_HEAD").exists();
        if status.is_clean() && !merging {
            return Ok(None);
        }
        self.check_identity()?;
        self.run(&["add", "--all"], Access::Write)?;
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

    /// A merge, cherry-pick, revert or rebase in progress in this worktree.
    fn operation_in_progress(&self) -> Option<&'static str> {
        let d = self.git_dir();
        [
            ("MERGE_HEAD", "merge"),
            ("CHERRY_PICK_HEAD", "cherry-pick"),
            ("REVERT_HEAD", "revert"),
            ("sequencer", "cherry-pick or revert"),
            ("rebase-merge", "rebase"),
            ("rebase-apply", "rebase or am"),
        ]
        .into_iter()
        .find(|(f, _)| d.join(f).exists())
        .map(|(_, what)| what)
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
    /// the branch moves meanwhile. The user's hooks run. If git fails part
    /// way (a hook rejects the commit, an untracked file is in the way), the
    /// merge or cherry-pick is aborted so the repository is never left
    /// mid-operation, and the error is returned.
    ///
    /// Like `git merge`, it may overwrite ignored files that the branch
    /// adds as tracked files.
    pub fn integrate(
        &self,
        branch: &str,
        how: Integration,
    ) -> Result<IntegrateOutcome, IntegrateError> {
        check_branch_name(self, branch)?;
        let Head::Branch(target) = self.head()? else {
            return Err(IntegrateError::DetachedHead);
        };
        if let Some(op) = self.operation_in_progress() {
            return Err(IntegrateError::InProgress(op));
        }
        let source = self.resolve(&format!("refs/heads/{branch}"))?;
        let old_head = self.resolve("HEAD")?;

        let dirty: Vec<PathBuf> = self
            .status()?
            .uncommitted()
            .map(|e| e.path.clone())
            .collect();
        if !dirty.is_empty() {
            return Err(IntegrateError::Dirty(dirty));
        }
        self.check_source_worktree(branch)?;

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

        let args: Vec<String> = match how {
            Integration::FastForwardOnly => {
                vec!["merge".into(), "--ff-only".into(), source.clone()]
            }
            Integration::Merge => vec![
                "merge".into(),
                "--no-ff".into(),
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
        let (out, shown) = self.writer().run_status(&args, Access::Write)?;
        if !out.status.success() {
            let cause = command_error(shown, &out);
            return Err(match self.abort_operation(&old_head) {
                Ok(()) => IntegrateError::Git(cause),
                Err(detail) => IntegrateError::AbortFailed { cause, detail },
            });
        }
        let new_head = self.resolve("HEAD")?;
        let commits = self.rev_list(&["--reverse", &format!("{old_head}..{new_head}")])?;
        Ok(IntegrateOutcome { new_head, commits })
    }

    /// Refuses when the worktree that has `branch` checked out (if any,
    /// other than this one) has uncommitted or untracked files.
    fn check_source_worktree(&self, branch: &str) -> Result<(), IntegrateError> {
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
                .chain(status.untracked())
                .map(|e| e.path.clone())
                .collect();
            if !files.is_empty() {
                return Err(IntegrateError::SourceDirty(files));
            }
        }
        Ok(())
    }

    /// Applies `commits` one by one onto `onto` in memory, the way
    /// `cherry-pick` would, and returns the ones that change something.
    /// Writes tree objects only.
    fn cherry_pick_check(
        &self,
        onto: &str,
        commits: &[String],
    ) -> Result<Vec<String>, IntegrateError> {
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
        Ok(kept)
    }

    /// After a failed merge or cherry-pick: aborts whatever git left in
    /// progress and checks `HEAD` is back at `old_head`. The error text says
    /// what could not be restored.
    fn abort_operation(&self, old_head: &str) -> Result<(), String> {
        let d = self.git_dir().to_path_buf();
        let mut problems = Vec::new();
        if d.join("MERGE_HEAD").exists()
            && let Err(e) = self.run(&["merge", "--abort"], Access::Write)
        {
            problems.push(e.to_string());
        }
        if (d.join("CHERRY_PICK_HEAD").exists() || d.join("sequencer").exists())
            && let Err(e) = self.run(&["cherry-pick", "--abort"], Access::Write)
        {
            problems.push(e.to_string());
        }
        match self.resolve("HEAD") {
            Ok(now) if now == old_head => {}
            Ok(now) => problems.push(format!("HEAD is at {now}, it was {old_head}")),
            Err(e) => problems.push(e.to_string()),
        }
        if let Some(op) = self.operation_in_progress() {
            problems.push(format!("a {op} is still in progress"));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}
