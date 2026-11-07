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
    /// Every commit applies; these are the ones that still change
    /// something on the new base, oldest first.
    Clean { kept: Vec<String> },
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
    /// Refused before anything changed.
    #[error("{0}")]
    Refused(String),
    /// Git stopped on a conflict in these files. The rebase was aborted,
    /// the branch is back where it was, and the old-tip branch is deleted.
    #[error("the rebase stopped on a conflict in {} file(s)", .0.len())]
    Conflicts(Vec<PathBuf>),
    /// Git failed another way; put back as for `Conflicts`.
    #[error(transparent)]
    Git(#[from] GitError),
    /// Git failed and putting things back failed too. The text says what
    /// was found.
    #[error("{cause}; then putting the branch back failed: {detail}")]
    AbortFailed { cause: String, detail: String },
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
        match self.cherry_pick_check(onto, &commits) {
            Ok(kept) => Ok(RebaseCheck::Clean { kept }),
            Err(IntegrateError::Conflicts(files)) => Ok(RebaseCheck::Conflicts(files)),
            Err(IntegrateError::Git(e)) => Err(e),
            Err(e) => Err(GitError::InvalidArgument(e.to_string())),
        }
    }

    /// Rebases the branch checked out in this worktree: the commits of
    /// `upstream..tip` are replayed onto `onto`, as
    /// `git rebase --onto <onto> <upstream>` does, and the branch then
    /// points at the last of them. First a branch named
    /// `keep_old_tip_as` is created at `tip`, so the old history stays
    /// reachable under a name.
    ///
    /// Refuses, changing nothing, when this worktree is not on `branch`,
    /// `branch` does not point at `tip`, an operation (merge, rebase,
    /// cherry-pick) is in progress here, tracked files have uncommitted
    /// changes or conflicts, the range has merge commits, or the old-tip
    /// branch already exists. Untracked files may stay; git refuses to
    /// overwrite them.
    ///
    /// The user's configuration that would change what a rebase does is
    /// overridden: no autostash, no moving other branches
    /// (`--no-update-refs`), no merges kept, no autosquash, and the merge
    /// backend. Hooks run. Commits whose change `onto` already has are
    /// dropped, as `git rebase` does.
    ///
    /// When git fails (a conflict, a hook), the rebase is aborted, the
    /// branch is checked to be back at `tip` with no tracked file changed,
    /// `undo_to` is applied, and the old-tip branch is deleted.
    pub fn rebase_onto(&self, r: RebaseOnto<'_>) -> Result<RebaseOutcome, RebaseError> {
        check_branch_name(self, r.branch)?;
        check_branch_name(self, r.keep_old_tip_as)?;
        for rev in [r.upstream, r.onto, r.tip] {
            check_rev(rev)?;
        }
        if let Some(u) = r.undo_to {
            check_rev(u)?;
        }
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
        let branch_ref = format!("refs/heads/{}", r.branch);
        let at = self.resolve(&branch_ref)?;
        if at != self.resolve(r.tip)? {
            return Err(RebaseError::Refused(format!(
                "{} is at {}, not at {}",
                r.branch,
                short(&at),
                short(r.tip)
            )));
        }
        let tip = at;
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
        if let Err(e) = self.create_branch_at(r.keep_old_tip_as, &tip) {
            return Err(RebaseError::Refused(e.to_string()));
        }

        let args = [
            "rebase",
            "--merge",
            "--no-autostash",
            "--no-update-refs",
            "--no-rebase-merges",
            "--no-autosquash",
            "--no-fork-point",
            "--empty=drop",
            "--onto",
            &onto,
            r.upstream,
        ];
        let (out, shown) = self.writer().run_status(&args, Access::Write)?;
        if !out.status.success() {
            let cause = command_error(shown, &out);
            let conflicts: Vec<PathBuf> = self
                .status()
                .map(|s| s.conflicted().map(|e| e.path.clone()).collect())
                .unwrap_or_default();
            return Err(match self.put_back(&r, &tip) {
                Ok(()) if !conflicts.is_empty() => RebaseError::Conflicts(conflicts),
                Ok(()) => RebaseError::Git(cause),
                Err(detail) => RebaseError::AbortFailed {
                    cause: if conflicts.is_empty() {
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
                    },
                    detail,
                },
            });
        }

        // Git said yes. Check that it did what was asked.
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
        if !wrong.is_empty() {
            return Err(RebaseError::AbortFailed {
                cause: "git reported success".into(),
                detail: format!(
                    "{}; the old tip is kept as branch {}",
                    wrong.join("; "),
                    r.keep_old_tip_as
                ),
            });
        }
        let commits = self.rev_list(&["--reverse", &format!("{onto}..{new_tip}")])?;
        Ok(RebaseOutcome { new_tip, commits })
    }

    /// After a failed rebase: aborts it if git left one open, checks the
    /// branch is checked out at `tip` with no tracked file changed, applies
    /// `undo_to`, and deletes the old-tip branch. Returns what could not be
    /// put back.
    fn put_back(&self, r: &RebaseOnto<'_>, tip: &str) -> std::result::Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        match self.operation() {
            Ok(Some(crate::Operation::Rebase)) => {
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
            problems.push(format!(
                "the old tip is kept as branch {}",
                r.keep_old_tip_as
            ));
            return Err(problems.join("; "));
        }
        if let Some(to) = r.undo_to
            && let Err(e) = self.run(&["reset", "--soft", "--quiet", to], Access::Write)
        {
            return Err(format!(
                "moving {} back to {} failed: {e}; it is at {}",
                r.branch,
                short(to),
                short(tip)
            ));
        }
        let keep = format!("refs/heads/{}", r.keep_old_tip_as);
        if let Err(e) = self.run(&["update-ref", "-d", &keep, tip], Access::Write) {
            return Err(format!("deleting branch {} failed: {e}", r.keep_old_tip_as));
        }
        Ok(())
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}
