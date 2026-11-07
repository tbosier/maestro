//! Moving a worktree's own commits onto another commit: the commits on its
//! branch since an old base, replayed onto a new one (`git rebase --onto`).

use std::path::PathBuf;

use crate::repo::check_rev;
use crate::worktree::check_branch_name;
use crate::{Access, GitError, Head, IntegrateError, Repository, Result, command_error};

/// One commit: its id, short id and subject line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitLine {
    pub id: String,
    pub short: String,
    pub subject: String,
}

/// Commits in a range: the first few, and how many there are in all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Commits {
    pub shown: Vec<CommitLine>,
    pub total: usize,
}

/// What [`Repository::rebase_check`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseCheck {
    /// Every commit applies; `kept` are the ones that still change
    /// something on the new base, oldest first, and `tree` is the tree the
    /// last of them leaves: what the rebased branch should hold.
    Clean { kept: Vec<String>, tree: String },
    /// Applying the commits stops on a conflict in these files.
    Conflicts(Vec<PathBuf>),
    /// The range holds merge commits, which a rebase would flatten.
    MergeCommits(Vec<String>),
}

/// What [`Repository::rebase_onto`] does.
#[derive(Debug, Clone, Copy)]
pub struct RebaseOnto<'a> {
    /// The branch checked out in this worktree, which is rewritten.
    pub branch: &'a str,
    /// The old base: commits after it on the branch are the ones moved.
    pub upstream: &'a str,
    /// The commit (an id) they are moved onto.
    pub onto: &'a str,
    /// The commit the branch must point at; anything else is refused.
    pub tip: &'a str,
    /// A new branch created at `tip` before the rebase, so the old history
    /// stays reachable. It must not exist yet.
    pub keep_old_tip_as: &'a str,
    /// When the caller committed on the branch just before (so `tip` is
    /// that commit and this is its parent), a failed rebase also moves the
    /// branch back here with `reset --soft`: the branch is then where it
    /// was before that commit, the files are unchanged, and what the commit
    /// held stays staged.
    pub undo_to: Option<&'a str>,
}

/// A completed rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseOutcome {
    /// The branch's new tip.
    pub new_tip: String,
    /// The branch's commits after `onto`, oldest first. Commits whose
    /// change `onto` already had were dropped and are not here.
    pub commits: Vec<String>,
}

/// Why [`Repository::rebase_onto`] did not rebase.
#[derive(Debug, thiserror::Error)]
pub enum RebaseError {
    /// Refused, or failed, before the old-tip branch was made: nothing
    /// this call does changed anything.
    #[error("{0}")]
    Refused(String),
    /// Git stopped on a conflict in these files. The rebase was aborted,
    /// the branch is back where it was (and at `undo_to`, when given), and
    /// the old-tip branch is deleted.
    #[error("the rebase stopped on a conflict in {} file(s)", .0.len())]
    Conflicts(Vec<PathBuf>),
    /// Git failed another way; put back as for `Conflicts`.
    #[error(transparent)]
    Git(GitError),
    /// Something failed after the old-tip branch was made and the
    /// branch could not be shown to be put back: the rebase may have run,
    /// or putting it back failed. The text says what was found; the
    /// old-tip branch is kept.
    #[error("{cause}; {detail}")]
    Unknown { cause: String, detail: String },
}

impl From<GitError> for RebaseError {
    /// A git error before anything changed.
    fn from(e: GitError) -> Self {
        RebaseError::Refused(e.to_string())
    }
}

impl Repository {
    /// Commits reachable from `to` and not from `from`, newest first: at
    /// most `limit` of them, and how many there are in all.
    pub fn commits_between(&self, from: &str, to: &str, limit: usize) -> Result<Commits> {
        check_rev(from)?;
        check_rev(to)?;
        let range = format!("{from}..{to}");
        let count = self.run(&["rev-list", "--count", &range], Access::ReadOnly)?;
        let total = String::from_utf8_lossy(&count)
            .trim()
            .parse()
            .map_err(|_| GitError::Parse {
                args: format!("rev-list --count {range}"),
                detail: "not a number".into(),
            })?;
        let max = format!("--max-count={limit}");
        let out = self.run(
            &[
                "log",
                "--no-color",
                "--no-show-signature",
                "--format=%H%x1f%h%x1f%s",
                &max,
                &range,
                "--",
            ],
            Access::ReadOnly,
        )?;
        let shown = String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\u{1f}');
                Some(CommitLine {
                    id: f.next()?.to_string(),
                    short: f.next()?.to_string(),
                    subject: f.next().unwrap_or_default().to_string(),
                })
            })
            .collect();
        Ok(Commits { shown, total })
    }

    /// A dry run of moving the commits of `upstream..tip`, and then
    /// `extra` (a commit whose parent is `tip`, such as a work snapshot),
    /// onto `onto`, in memory: each is applied as a cherry-pick would. No
    /// ref, index or file changes; merged trees are written to the object
    /// store unreferenced. Needs git 2.45 or newer.
    pub fn rebase_check(
        &self,
        onto: &str,
        upstream: &str,
        tip: &str,
        extra: Option<&str>,
    ) -> Result<RebaseCheck> {
        check_rev(onto)?;
        let range = format!("{upstream}..{tip}");
        check_rev(upstream)?;
        check_rev(tip)?;
        let merges = self.rev_list(&["--merges", &range])?;
        if !merges.is_empty() {
            return Ok(RebaseCheck::MergeCommits(merges));
        }
        let mut commits = self.rev_list(&["--reverse", &range])?;
        if let Some(e) = extra {
            check_rev(e)?;
            commits.push(e.to_string());
        }
        match self.cherry_pick_trees(onto, &commits) {
            Ok((kept, tree)) => Ok(RebaseCheck::Clean { kept, tree }),
            Err(IntegrateError::Conflicts(files)) => Ok(RebaseCheck::Conflicts(files)),
            Err(IntegrateError::Git(e)) => Err(e),
            Err(e) => Err(GitError::InvalidArgument(e.to_string())),
        }
    }

    /// Rebases the branch checked out in this worktree: the commits of
    /// `upstream..tip` are replayed onto `onto`, as
    /// `git rebase --onto <onto> <upstream> <branch>` does, and the branch
    /// then points at the last of them. First a branch named
    /// `keep_old_tip_as` is created at `tip`, so the old history stays
    /// reachable under a name.
    ///
    /// Refuses, changing nothing, when this worktree is not on `branch`,
    /// `branch` does not point at `tip`, an operation (merge, rebase,
    /// cherry-pick) is in progress here, tracked files have uncommitted
    /// changes or conflicts, the range has merge commits, the old-tip
    /// branch already exists, or a file `onto` tracks and `tip` does not
    /// is already on disk (untracked or ignored), since git would refuse
    /// or overwrite it. The branch, `HEAD` and the operation state are
    /// checked again just before git runs; a change in the moment after
    /// that is not caught.
    ///
    /// The user's configuration that would change what a rebase does is
    /// overridden: the `ort` strategy, no autostash, no moving other
    /// branches (`--no-update-refs`), no merges kept, no autosquash, the
    /// merge backend, and the default todo line format. Hooks run.
    /// Commits whose change `onto` already has are dropped, as
    /// `git rebase` does.
    ///
    /// When git fails (a conflict, a hook), a rebase it left open is
    /// aborted if it is this one (its state names `onto` and `tip`), the
    /// branch is checked to be back at `tip` with no tracked file changed,
    /// the branch is moved from `tip` to `undo_to` with a compare-and-swap
    /// on the branch itself (so no other branch moves), and the old-tip
    /// branch is deleted.
    pub fn rebase_onto(&self, r: RebaseOnto<'_>) -> Result<RebaseOutcome, RebaseError> {
        check_branch_name(self, r.branch)?;
        check_branch_name(self, r.keep_old_tip_as)?;
        for rev in [r.upstream, r.onto, r.tip] {
            check_rev(rev)?;
        }
        if let Some(u) = r.undo_to {
            check_rev(u)?;
        }
        let branch_ref = format!("refs/heads/{}", r.branch);
        let tip = self.resolve(r.tip)?;
        self.ready_to_rebase(&r, &branch_ref, &tip)?;
        let dirty: Vec<PathBuf> = self
            .status()?
            .uncommitted()
            .map(|e| e.path.clone())
            .collect();
        if !dirty.is_empty() {
            return Err(RebaseError::Refused(format!(
                "the worktree has uncommitted changes in {} file(s)",
                dirty.len()
            )));
        }
        let range = format!("{}..{tip}", r.upstream);
        let merges = self.rev_list(&["--merges", &range])?;
        if !merges.is_empty() {
            return Err(RebaseError::Refused(format!(
                "{} has {} merge commit(s) since {}; a rebase would flatten them",
                r.branch,
                merges.len(),
                short(r.upstream)
            )));
        }
        let onto = self.resolve(r.onto)?;
        // Files the checkout of `onto` would create. One already on disk
        // is untracked or ignored here: git refuses the first and silently
        // overwrites the second, and an abort would then delete it.
        let out = self.run(
            &[
                "diff",
                "--name-only",
                "--no-renames",
                "--diff-filter=A",
                "-z",
                &tip,
                &onto,
                "--",
            ],
            Access::ReadOnly,
        )?;
        let in_the_way: Vec<String> = crate::split_nul(&out)
            .into_iter()
            .map(crate::bytes_to_path)
            .filter(|p| std::fs::symlink_metadata(self.root().join(p)).is_ok())
            .map(|p| p.display().to_string())
            .collect();
        if !in_the_way.is_empty() {
            return Err(RebaseError::Refused(format!(
                "these files are in the worktree but not in its branch, and {} has them: {}; \
                 move them out of the way first",
                short(&onto),
                in_the_way.join(", ")
            )));
        }
        if let Err(e) = self.create_branch_at(r.keep_old_tip_as, &tip) {
            return Err(RebaseError::Refused(e.to_string()));
        }
        let unknown = |cause: String, detail: &str| RebaseError::Unknown {
            cause,
            detail: format!(
                "{detail}; the old tip is kept as branch {}; look at the worktree with git status",
                r.keep_old_tip_as
            ),
        };
        // The checks took a moment; the branch, HEAD and state must still
        // be what they found. This narrows the window, it cannot close it.
        if let Err(e) = self.ready_to_rebase(&r, &branch_ref, &tip) {
            return Err(match self.drop_keep(&r, &tip) {
                Ok(()) => e,
                Err(d) => unknown(e.to_string(), &d),
            });
        }

        let args = [
            "rebase",
            "--merge",
            "--strategy=ort",
            "--no-autostash",
            "--no-update-refs",
            "--no-rebase-merges",
            "--no-autosquash",
            "--no-fork-point",
            "--empty=drop",
            "--onto",
            &onto,
            r.upstream,
            r.branch,
        ];
        let writer = self.with_git(
            self.writer()
                .git()
                .clone()
                .config("rebase.instructionFormat=%s"),
        );
        let (out, shown) = match writer.run_status(&args, Access::Write) {
            Ok(o) => o,
            Err(e) => return Err(unknown(e.to_string(), "git may not have run")),
        };
        if !out.status.success() {
            let cause = command_error(shown, &out);
            let conflicts: Vec<PathBuf> = self
                .status()
                .map(|s| s.conflicted().map(|e| e.path.clone()).collect())
                .unwrap_or_default();
            return Err(match self.put_back(&r, &onto, &tip) {
                Ok(()) if !conflicts.is_empty() => RebaseError::Conflicts(conflicts),
                Ok(()) => RebaseError::Git(cause),
                Err(detail) => {
                    let cause = if conflicts.is_empty() {
                        cause.to_string()
                    } else {
                        format!(
                            "the rebase stopped on a conflict in {}",
                            conflicts
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    unknown(cause, &format!("putting the branch back failed: {detail}"))
                }
            });
        }

        // Git said yes. Check that it did what was asked.
        let checked = (|| -> Result<(String, Vec<String>)> {
            let mut wrong = Vec::new();
            if let Some(op) = self.operation()? {
                wrong.push(format!("a {} was left open", op.label()));
            }
            if self.head()? != Head::Branch(r.branch.to_string()) {
                wrong.push(format!("{} is no longer checked out", r.branch));
            }
            let new_tip = self.resolve(&branch_ref)?;
            if !self.can_fast_forward(&onto, &new_tip)? {
                wrong.push(format!(
                    "{} is at {}, which is not on top of {}",
                    r.branch,
                    short(&new_tip),
                    short(&onto)
                ));
            }
            let commits = self.rev_list(&["--reverse", &format!("{onto}..{new_tip}")])?;
            Ok((new_tip, commits)).and_then(|v| {
                if wrong.is_empty() {
                    Ok(v)
                } else {
                    Err(GitError::InvalidArgument(wrong.join("; ")))
                }
            })
        })();
        match checked {
            Ok((new_tip, commits)) => Ok(RebaseOutcome { new_tip, commits }),
            Err(e) => Err(unknown("git reported success".into(), &format!("but {e}"))),
        }
    }

    /// On `branch` at `tip` with no operation in progress.
    fn ready_to_rebase(
        &self,
        r: &RebaseOnto<'_>,
        branch_ref: &str,
        tip: &str,
    ) -> Result<(), RebaseError> {
        match self.head()? {
            Head::Branch(b) if b == r.branch => {}
            other => {
                return Err(RebaseError::Refused(format!(
                    "the worktree is on {}, not {}",
                    other.label(),
                    r.branch
                )));
            }
        }
        if let Some(op) = self.operation()? {
            return Err(RebaseError::Refused(format!(
                "a {} is in progress in the worktree; finish it first ({})",
                op.label(),
                op.way_out()
            )));
        }
        let at = self.resolve(branch_ref)?;
        if at != tip || self.resolve("HEAD")? != tip {
            return Err(RebaseError::Refused(format!(
                "{} is at {}, not at {}",
                r.branch,
                short(&at),
                short(tip)
            )));
        }
        Ok(())
    }

    /// Deletes the old-tip branch, if it still points at `tip`.
    fn drop_keep(&self, r: &RebaseOnto<'_>, tip: &str) -> std::result::Result<(), String> {
        let keep = format!("refs/heads/{}", r.keep_old_tip_as);
        self.run(&["update-ref", "-d", &keep, tip], Access::Write)
            .map(|_| ())
            .map_err(|e| format!("deleting branch {} failed: {e}", r.keep_old_tip_as))
    }

    /// The rebase state git left in this worktree is this call's: the
    /// merge backend's, rebasing `tip` onto `onto`.
    fn our_rebase(&self, onto: &str, tip: &str) -> bool {
        let dir = self.git_dir().join("rebase-merge");
        let read = |f: &str| {
            std::fs::read_to_string(dir.join(f))
                .ok()
                .map(|s| s.trim().to_string())
        };
        read("onto").as_deref() == Some(onto) && read("orig-head").as_deref() == Some(tip)
    }

    /// After a failed rebase: aborts it if git left this one open (another
    /// one is left alone and reported), checks the branch is checked out
    /// at `tip` with no tracked file changed, moves it to `undo_to`, and
    /// deletes the old-tip branch. Returns what could not be put back.
    fn put_back(
        &self,
        r: &RebaseOnto<'_>,
        onto: &str,
        tip: &str,
    ) -> std::result::Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        match self.operation() {
            Ok(Some(crate::Operation::Rebase)) if self.our_rebase(onto, tip) => {
                if let Err(e) = self.run(&["rebase", "--abort"], Access::Write) {
                    problems.push(e.to_string());
                }
            }
            Ok(Some(op)) => problems.push(format!(
                "a {} that this rebase did not start is in progress; left alone",
                op.label()
            )),
            Ok(None) => {}
            Err(e) => problems.push(e.to_string()),
        }
        match self.head() {
            Ok(Head::Branch(b)) if b == r.branch => {}
            Ok(other) => problems.push(format!("the worktree is on {}", other.label())),
            Err(e) => problems.push(e.to_string()),
        }
        match self.resolve("HEAD") {
            Ok(now) if now == tip => {}
            Ok(now) => problems.push(format!("HEAD is at {now}, it was {tip}")),
            Err(e) => problems.push(e.to_string()),
        }
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
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        // The branch itself, from `tip` only: if the worktree switched to
        // another branch meanwhile, nothing moves. The index is left as it
        // is, as `reset --soft` would leave it.
        if let Some(to) = r.undo_to {
            let branch_ref = format!("refs/heads/{}", r.branch);
            if let Err(e) = self.run(
                &[
                    "update-ref",
                    "-m",
                    "maestro: rebase put back",
                    &branch_ref,
                    to,
                    tip,
                ],
                Access::Write,
            ) {
                return Err(format!(
                    "moving {} back to {} failed: {e}; it is at {}",
                    r.branch,
                    short(to),
                    short(tip)
                ));
            }
        }
        self.drop_keep(r, tip)
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}
