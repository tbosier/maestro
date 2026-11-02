//! Branches, and fetch, fast-forward pull and push as exact commands the
//! user sees before they run. Nothing here can force-push.

use crate::worktree::check_branch_name;
use crate::{Access, GitError, Repository, Result, command_error};

/// A local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    /// Checked out in this working tree.
    pub current: bool,
    /// Its upstream, such as `origin/main`.
    pub upstream: Option<String>,
    /// Subject of its newest commit.
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteOp {
    Fetch,
    /// `git pull --ff-only`: never a merge commit, never a rebase.
    Pull,
    Push,
}

impl RemoteOp {
    pub fn label(self) -> &'static str {
        match self {
            RemoteOp::Fetch => "fetch",
            RemoteOp::Pull => "pull",
            RemoteOp::Push => "push",
        }
    }
}

/// A network command, worked out in full before it runs so the user can
/// see exactly what will happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCommand {
    pub op: RemoteOp,
    pub remote: String,
    /// The URL git will use: the push URL for a push.
    pub url: String,
    /// The branch checked out when the command was worked out. It runs
    /// only while that branch is still checked out.
    pub branch: Option<String>,
    /// The arguments after `git`.
    pub args: Vec<String>,
}

impl RemoteCommand {
    /// `git push origin refs/heads/main:refs/heads/main`.
    pub fn display(&self) -> String {
        format!("git {}", self.args.join(" "))
    }
}

/// Arguments that would force or delete; a built command never has them,
/// and [`Repository::run_remote`] refuses one that does.
fn forbidden(arg: &str) -> bool {
    matches!(
        arg,
        "-f" | "--force" | "--mirror" | "-d" | "--delete" | "--prune" | "--all" | "--tags"
    ) || arg.starts_with("--force")
        || arg.starts_with('+')
        || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('f'))
}

pub(crate) fn parse_branches(out: &[u8]) -> Vec<Branch> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\0');
            let head = f.next()?;
            let name = f.next()?.to_string();
            let upstream = f.next().filter(|u| !u.is_empty()).map(str::to_string);
            let subject = f.next().unwrap_or_default().to_string();
            Some(Branch {
                current: head == "*",
                name,
                upstream,
                subject,
            })
        })
        .collect()
}

impl Repository {
    /// `git config --get key`, or `None` when it is not set.
    fn config_get(&self, key: &str) -> Result<Option<String>> {
        let (out, shown) = self.run_status(&["config", "--get", key], Access::ReadOnly)?;
        match out.status.code() {
            Some(0) => Ok(Some(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            )),
            Some(1) => Ok(None),
            _ => Err(command_error(shown, &out)),
        }
    }

    fn remotes(&self) -> Result<Vec<String>> {
        let out = self.run(&["remote"], Access::ReadOnly)?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Local branches, sorted by name.
    pub fn branches(&self) -> Result<Vec<Branch>> {
        let out = self.run(
            &[
                "for-each-ref",
                "--format=%(HEAD)%00%(refname:short)%00%(upstream:short)%00%(contents:subject)",
                "refs/heads",
            ],
            Access::ReadOnly,
        )?;
        Ok(parse_branches(&out))
    }

    /// Checks out an existing local branch. Git refuses when uncommitted
    /// changes would be overwritten.
    pub fn switch_branch(&self, name: &str) -> Result<()> {
        check_branch_name(self, name)?;
        let full = format!("refs/heads/{name}");
        if !self.has_pseudo_ref(&full)? {
            return Err(GitError::InvalidArgument(format!("no branch named {name}")));
        }
        self.run(&["switch", "--no-guess", "--quiet", name], Access::Write)?;
        Ok(())
    }

    /// Creates a branch at `HEAD` and checks it out.
    pub fn create_branch(&self, name: &str) -> Result<()> {
        check_branch_name(self, name)?;
        self.run(&["switch", "--quiet", "--create", name], Access::Write)?;
        Ok(())
    }

    /// Works out the exact command for `op` on the current branch:
    ///
    /// - fetch: `git fetch <remote>`, the branch's upstream remote, or the
    ///   only remote, or `origin`;
    /// - pull: `git pull --no-rebase --ff-only <remote> <upstream ref>`,
    ///   refused without an upstream;
    /// - push: `git push <remote> refs/heads/<branch>:<destination>` to the
    ///   push remote (`branch.<name>.pushRemote`, `remote.pushDefault`, the
    ///   upstream's remote, or the only remote), with `--set-upstream` when
    ///   the branch has no upstream yet. The refspec names both sides in
    ///   full, so neither `push.default` nor a configured push refspec can
    ///   change it, and it never forces.
    pub fn remote_command(&self, op: RemoteOp) -> Result<RemoteCommand> {
        let branch = self.head()?.branch().map(str::to_string);
        let remotes = self.remotes()?;
        let only = (remotes.len() == 1).then(|| remotes[0].clone());
        let upstream_remote = match &branch {
            Some(b) => self
                .config_get(&format!("branch.{b}.remote"))?
                .filter(|r| r != "."),
            None => None,
        };
        let upstream_ref = match (&branch, &upstream_remote) {
            (Some(b), Some(_)) => self.config_get(&format!("branch.{b}.merge"))?,
            _ => None,
        };
        let need_branch = || {
            branch.clone().ok_or_else(|| {
                GitError::InvalidArgument(format!(
                    "HEAD is detached; check out a branch to {}",
                    op.label()
                ))
            })
        };
        let (remote, args) = match op {
            RemoteOp::Fetch => {
                let remote = upstream_remote
                    .clone()
                    .or(only.clone())
                    .or_else(|| remotes.iter().find(|r| *r == "origin").cloned())
                    .ok_or_else(|| GitError::InvalidArgument("no remote to fetch from".into()))?;
                (remote.clone(), vec!["fetch".to_string(), remote])
            }
            RemoteOp::Pull => {
                let b = need_branch()?;
                let (Some(remote), Some(merge)) = (upstream_remote.clone(), upstream_ref.clone())
                else {
                    return Err(GitError::InvalidArgument(format!(
                        "{b} has no upstream to pull from; push it first (P sets one)"
                    )));
                };
                (
                    remote.clone(),
                    vec![
                        "pull".to_string(),
                        "--no-rebase".into(),
                        "--ff-only".into(),
                        remote,
                        merge,
                    ],
                )
            }
            RemoteOp::Push => {
                let b = need_branch()?;
                let push_remote = match self.config_get(&format!("branch.{b}.pushRemote"))? {
                    Some(r) => Some(r),
                    None => self.config_get("remote.pushDefault")?,
                };
                let remote = push_remote
                    .clone()
                    .or(upstream_remote.clone())
                    .or(only.clone())
                    .ok_or_else(|| {
                        GitError::InvalidArgument(format!(
                            "no remote to push {b} to (the repository has {} remotes)",
                            remotes.len()
                        ))
                    })?;
                let dest = match (&upstream_remote, &upstream_ref) {
                    (Some(r), Some(m)) if *r == remote && m.starts_with("refs/heads/") => m.clone(),
                    _ => format!("refs/heads/{b}"),
                };
                let mut args = vec!["push".to_string()];
                if upstream_remote.is_none() {
                    args.push("--set-upstream".into());
                }
                args.push(remote.clone());
                args.push(format!("refs/heads/{b}:{dest}"));
                (remote, args)
            }
        };
        if !remotes.contains(&remote) {
            return Err(GitError::InvalidArgument(format!(
                "remote {remote:?} is not configured"
            )));
        }
        let mut get_url = vec!["remote", "get-url"];
        if op == RemoteOp::Push {
            get_url.push("--push");
        }
        get_url.push(&remote);
        let url = String::from_utf8_lossy(&self.run(&get_url, Access::ReadOnly)?)
            .trim()
            .to_string();
        Ok(RemoteCommand {
            op,
            remote,
            url,
            args,
            branch,
        })
    }

    /// Runs a command from [`Repository::remote_command`], exactly as
    /// built. Refuses any forcing or deleting argument. Unless the user
    /// configured an SSH command, SSH runs in batch mode so it never asks
    /// for a password on the terminal the editor is drawing on. Returns
    /// git's messages (it writes progress and results to stderr).
    pub fn run_remote(&self, cmd: &RemoteCommand) -> Result<String> {
        if let Some(bad) = cmd.args.iter().skip(1).find(|a| forbidden(a)) {
            return Err(GitError::InvalidArgument(format!(
                "refusing to run git with {bad}"
            )));
        }
        let first = cmd.args.first().map(String::as_str);
        let expected = match cmd.op {
            RemoteOp::Fetch => "fetch",
            RemoteOp::Pull => "pull",
            RemoteOp::Push => "push",
        };
        if first != Some(expected) {
            return Err(GitError::InvalidArgument("not a remote command".into()));
        }
        let now = self.head()?.branch().map(str::to_string);
        if now != cmd.branch {
            return Err(GitError::InvalidArgument(format!(
                "{} is checked out now, not {}; nothing was run",
                now.as_deref().unwrap_or("a detached HEAD"),
                cmd.branch.as_deref().unwrap_or("a detached HEAD")
            )));
        }
        let mut git = self.git().clone();
        let ssh_set = std::env::var_os("GIT_SSH_COMMAND").is_some()
            || std::env::var_os("GIT_SSH").is_some()
            || self.config_get("core.sshCommand")?.is_some();
        if !ssh_set {
            git = git.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        let repo = self.with_git(git.env("GIT_EDITOR", "true"));
        let (out, shown) = repo.run_status(&cmd.args, Access::Write)?;
        if !out.status.success() {
            return Err(command_error(shown, &out));
        }
        let mut msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !stdout.is_empty() {
            if !msg.is_empty() {
                msg.push('\n');
            }
            msg.push_str(&stdout);
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forcing_arguments_are_recognized() {
        for bad in [
            "-f",
            "--force",
            "--force-with-lease",
            "--force-if-includes",
            "+refs/heads/a:refs/heads/a",
            "--mirror",
            "--delete",
            "-uf",
        ] {
            assert!(forbidden(bad), "{bad}");
        }
        for ok in [
            "origin",
            "--set-upstream",
            "--ff-only",
            "--no-rebase",
            "refs/heads/main:refs/heads/main",
        ] {
            assert!(!forbidden(ok), "{ok}");
        }
    }

    #[test]
    fn parses_branches() {
        let b = parse_branches(b"*\0main\0origin/main\0subject\n \0topic\0\0wip: a\0b\n");
        assert_eq!(b.len(), 2);
        assert!(b[0].current);
        assert_eq!(b[0].upstream.as_deref(), Some("origin/main"));
        assert!(!b[1].current);
        assert_eq!(b[1].upstream, None);
        assert_eq!(b[1].subject, "wip: a\0b");
    }
}
