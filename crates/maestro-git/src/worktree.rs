//! Worktree lifecycle: list, add, remove with safety checks, prune.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::repo::check_rev;
use crate::{Access, GitError, GitVersion, Repository, Result, bytes_to_path, command_error};

/// Held while this process runs `git worktree` (add, list, remove, prune).
/// `git worktree add` makes the new entry under `.git/worktrees` before it
/// fills it in, and a `git worktree` command that reads the entries in that
/// moment fails ("failed to read .git/worktrees/<name>/commondir"). Two
/// agents started at once (`N compete`) did that. One lock for every
/// repository: these commands are short and rare. Git run by another
/// process, or other git commands that read the entries, are not covered.
static WORKTREE_ADMIN: Mutex<()> = Mutex::new(());

fn worktree_admin() -> MutexGuard<'static, ()> {
    // Nothing is guarded but the order of git runs, so a panic while it
    // was held leaves nothing to repair.
    WORKTREE_ADMIN.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// The checked-out commit; `None` before the first commit or for bare.
    pub head: Option<String>,
    /// The branch, without `refs/heads/`; `None` when detached.
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    /// `Some(reason)` when locked; the reason may be empty.
    pub locked: Option<String>,
    /// `Some(reason)` when its directory is gone and `prune` would drop it.
    pub prunable: Option<String>,
    /// The main working tree (always listed first by git).
    pub is_main: bool,
}

/// Why a worktree was not removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveRefusal {
    /// The path is not a worktree of this repository.
    NotAWorktree,
    /// The main working tree cannot be removed.
    MainWorktree,
    /// Locked with `git worktree lock`; unlock it first. Not overridden by
    /// `force`.
    Locked(String),
    /// Tracked files with staged or unstaged changes, or in conflict.
    Uncommitted(Vec<PathBuf>),
    /// Untracked files that are not ignored.
    Untracked(Vec<PathBuf>),
    /// Commits that exist only in this worktree: ahead of the upstream, or,
    /// without one (or when its ref is gone), on no other local branch or
    /// remote-tracking branch. A commit already on another local branch
    /// counts as safe even if it was never pushed.
    Unpushed {
        count: usize,
        upstream: Option<String>,
    },
}

impl std::fmt::Display for RemoveRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoveRefusal::NotAWorktree => write!(f, "not a worktree of this repository"),
            RemoveRefusal::MainWorktree => write!(f, "the main working tree cannot be removed"),
            RemoveRefusal::Locked(r) if r.is_empty() => write!(f, "locked"),
            RemoveRefusal::Locked(r) => write!(f, "locked: {r}"),
            RemoveRefusal::Uncommitted(p) => {
                write!(f, "{} file(s) with uncommitted changes", p.len())
            }
            RemoveRefusal::Untracked(p) => write!(f, "{} untracked file(s)", p.len()),
            RemoveRefusal::Unpushed {
                count,
                upstream: Some(u),
            } => {
                write!(f, "{count} commit(s) not on {u}")
            }
            RemoveRefusal::Unpushed {
                count,
                upstream: None,
            } => {
                write!(f, "{count} commit(s) on no other branch")
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    /// Nothing was removed. Pass `force` to override everything but
    /// `NotAWorktree`, `MainWorktree` and `Locked`.
    #[error("refused to remove the worktree: {}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Refused(Vec<RemoveRefusal>),
    #[error(transparent)]
    Git(#[from] GitError),
}

/// `git worktree list -z` arrived in 2.36.
const LIST_Z: GitVersion = GitVersion::new(2, 36, 0);

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Parses `git worktree list --porcelain` records separated by `sep`.
pub(crate) fn parse_worktrees(out: &[u8], sep: u8) -> Vec<Worktree> {
    let mut list: Vec<Worktree> = Vec::new();
    let mut cur: Option<Worktree> = None;
    for field in out.split(|c| *c == sep) {
        if field.is_empty() {
            list.extend(cur.take());
            continue;
        }
        let (key, value) = match field.iter().position(|c| *c == b' ') {
            Some(i) => (&field[..i], &field[i + 1..]),
            None => (field, &[][..]),
        };
        let text = || String::from_utf8_lossy(value).into_owned();
        if key == b"worktree" {
            list.extend(cur.take());
            cur = Some(Worktree {
                path: bytes_to_path(value),
                head: None,
                branch: None,
                detached: false,
                bare: false,
                locked: None,
                prunable: None,
                is_main: list.is_empty(),
            });
            continue;
        }
        let Some(w) = cur.as_mut() else {
            continue;
        };
        match key {
            b"HEAD" => w.head = Some(text()).filter(|h| h.bytes().any(|c| c != b'0')),
            b"branch" => {
                let b = text();
                w.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(&b).to_string());
            }
            b"detached" => w.detached = true,
            b"bare" => w.bare = true,
            b"locked" => w.locked = Some(text()),
            b"prunable" => w.prunable = Some(text()),
            _ => {}
        }
    }
    list.extend(cur);
    list
}

impl Repository {
    /// Every worktree of the repository, the main one first.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let z = self.git().version()? >= LIST_Z;
        let mut args = vec!["worktree", "list", "--porcelain"];
        if z {
            args.push("-z");
        }
        let out = {
            let _admin = worktree_admin();
            self.run(&args, Access::ReadOnly)?
        };
        Ok(parse_worktrees(&out, if z { 0 } else { b'\n' }))
    }

    /// Creates a worktree at `path` on a new branch `branch` started from
    /// `base` (a branch, tag or commit). A relative `path` is taken from the
    /// repository root. Fails if the branch exists or `path` is not empty.
    pub fn add_worktree(&self, path: &Path, branch: &str, base: &str) -> Result<Worktree> {
        check_rev(base)?;
        check_branch_name(self, branch)?;
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root().join(path)
        };
        let args: Vec<OsString> = vec![
            "worktree".into(),
            "add".into(),
            "-b".into(),
            branch.into(),
            "--".into(),
            path.clone().into_os_string(),
            base.into(),
        ];
        {
            let _admin = worktree_admin();
            self.run(&args, Access::Write)?;
        }
        let want = canon(&path);
        self.worktrees()?
            .into_iter()
            .find(|w| canon(&w.path) == want)
            .ok_or_else(|| GitError::Parse {
                args: "worktree list".into(),
                detail: format!("new worktree {} is not listed", path.display()),
            })
    }

    /// The listed worktree at `path`. A relative `path` is taken from the
    /// repository root, the same as [`Repository::add_worktree`].
    fn find_worktree(&self, path: &Path) -> Result<Option<Worktree>> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root().join(path)
        };
        let want = canon(&abs);
        Ok(self
            .worktrees()?
            .into_iter()
            .find(|w| canon(&w.path) == want))
    }

    /// Commits reachable from `head` and from no branch or remote-tracking
    /// branch other than `branch`.
    fn unique_commits(&self, head: &str, branch: Option<&str>) -> Result<usize> {
        check_rev(head)?;
        let mut args = vec![
            "rev-list".to_string(),
            "--count".into(),
            head.into(),
            "--not".into(),
        ];
        if let Some(b) = branch {
            args.push(format!("--exclude={b}"));
        }
        args.push("--branches".into());
        args.push("--remotes".into());
        let out = self.run(&args, Access::ReadOnly)?;
        let text = String::from_utf8_lossy(&out);
        // An unreadable count must not read as "nothing to lose".
        text.trim().parse().map_err(|_| GitError::Parse {
            args: args.join(" "),
            detail: text.trim().to_string(),
        })
    }

    /// Reasons removing the worktree at `path` would lose work or is not
    /// allowed. Empty when it is safe. A relative `path` is taken from the
    /// repository root.
    ///
    /// Not detected: changes to files marked `assume-unchanged` or
    /// `skip-worktree`, which `git status` does not look at.
    pub fn removal_check(&self, path: &Path) -> Result<Vec<RemoveRefusal>> {
        let Some(wt) = self.find_worktree(path)? else {
            return Ok(vec![RemoveRefusal::NotAWorktree]);
        };
        self.refusals_for(&wt)
    }

    fn refusals_for(&self, wt: &Worktree) -> Result<Vec<RemoveRefusal>> {
        let mut reasons = Vec::new();
        if wt.is_main {
            reasons.push(RemoveRefusal::MainWorktree);
        }
        if let Some(r) = &wt.locked {
            reasons.push(RemoveRefusal::Locked(r.clone()));
        }
        if !reasons.is_empty() {
            return Ok(reasons);
        }
        // A worktree whose directory is gone has no files to lose, but its
        // HEAD may be the last reference to detached commits.
        let live = if wt.prunable.is_some() {
            None
        } else {
            self.git().discover(&wt.path)?
        };
        let mut upstream_ahead = None;
        if let Some(repo) = &live {
            let status = repo.status()?;
            let uncommitted: Vec<PathBuf> = status.uncommitted().map(|e| e.path.clone()).collect();
            if !uncommitted.is_empty() {
                reasons.push(RemoveRefusal::Uncommitted(uncommitted));
            }
            let untracked: Vec<PathBuf> = status.untracked().map(|e| e.path.clone()).collect();
            if !untracked.is_empty() {
                reasons.push(RemoveRefusal::Untracked(untracked));
            }
            if let Some(u) = &status.branch.upstream
                && !status.branch.upstream_gone
            {
                upstream_ahead = Some((status.branch.ahead as usize, u.clone()));
            }
        }
        if let Some(head) = &wt.head {
            let (count, upstream) = match upstream_ahead {
                Some((ahead, u)) => (ahead, Some(u)),
                // No upstream, or its ref was deleted.
                None => (self.unique_commits(head, wt.branch.as_deref())?, None),
            };
            if count > 0 {
                reasons.push(RemoveRefusal::Unpushed { count, upstream });
            }
        }
        Ok(reasons)
    }

    /// Removes the worktree at `path` (its branch is kept). A relative
    /// `path` is taken from the repository root. Without `force` it refuses
    /// when [`Repository::removal_check`] finds anything; with `force` only
    /// a missing, main or locked worktree is refused. Ignored files (build
    /// output, `.env`) are not checked and are deleted with the directory,
    /// as `git worktree remove` does.
    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), RemoveError> {
        let Some(wt) = self.find_worktree(path)? else {
            return Err(RemoveError::Refused(vec![RemoveRefusal::NotAWorktree]));
        };
        let blocking: Vec<RemoveRefusal> = self
            .refusals_for(&wt)?
            .into_iter()
            .filter(|r| {
                !force
                    || matches!(
                        r,
                        RemoveRefusal::NotAWorktree
                            | RemoveRefusal::MainWorktree
                            | RemoveRefusal::Locked(_)
                    )
            })
            .collect();
        if !blocking.is_empty() {
            return Err(RemoveError::Refused(blocking));
        }
        let mut args: Vec<OsString> = vec!["worktree".into(), "remove".into()];
        if force {
            args.push("--force".into());
        }
        args.push("--".into());
        // The exact path git listed, so git removes the worktree that was
        // checked and not another one matching a relative name.
        args.push(wt.path.into_os_string());
        let _admin = worktree_admin();
        self.run(&args, Access::Write)?;
        Ok(())
    }

    /// Drops records of worktrees whose directories are gone. Returns git's
    /// messages, one per pruned worktree. Like `git worktree prune`, it does
    /// not check for commits that only a pruned worktree's HEAD referred to;
    /// call [`Repository::removal_check`] on it first to find them.
    pub fn prune_worktrees(&self) -> Result<Vec<String>> {
        let (out, shown) = {
            let _admin = worktree_admin();
            self.run_status(&["worktree", "prune", "--verbose"], Access::Write)?
        };
        if !out.status.success() {
            return Err(command_error(shown, &out));
        }
        let mut msgs: Vec<String> = Vec::new();
        for stream in [&out.stdout, &out.stderr] {
            msgs.extend(
                String::from_utf8_lossy(stream)
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string),
            );
        }
        Ok(msgs)
    }
}

pub(crate) fn check_branch_name(repo: &Repository, branch: &str) -> Result<()> {
    if branch.starts_with('-') {
        return Err(GitError::InvalidArgument(format!(
            "bad branch name {branch:?}"
        )));
    }
    let (out, _) = repo.run_status(&["check-ref-format", "--branch", branch], Access::ReadOnly)?;
    if !out.status.success() {
        return Err(GitError::InvalidArgument(format!(
            "bad branch name {branch:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_with_both_separators() {
        let nl = b"worktree /repo\nHEAD 1111\nbranch refs/heads/main\n\nworktree /repo-wt\nHEAD 2222\ndetached\nlocked moving disks\n\nworktree /gone\nHEAD 3333\nbranch refs/heads/agent/a\nprunable gitdir file points to non-existent location\n\n";
        let z: Vec<u8> = nl
            .iter()
            .map(|c| if *c == b'\n' { 0 } else { *c })
            .collect();
        for list in [parse_worktrees(nl, b'\n'), parse_worktrees(&z, 0)] {
            assert_eq!(list.len(), 3);
            assert!(list[0].is_main);
            assert_eq!(list[0].branch.as_deref(), Some("main"));
            assert!(list[1].detached && !list[1].is_main);
            assert_eq!(list[1].locked.as_deref(), Some("moving disks"));
            assert_eq!(list[2].branch.as_deref(), Some("agent/a"));
            assert!(list[2].prunable.is_some());
        }
    }
}
