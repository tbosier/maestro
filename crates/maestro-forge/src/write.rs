//! The commands that change something: reviews, comments and merges on
//! GitHub, opening a browser, and checking a pull request out locally.
//! Each is built as a [`GhCommand`] first, so the editor can show exactly
//! what will run (and what goes to its standard input) before it runs.

use std::path::Path;

use crate::model::PrRef;
use crate::{Gh, GhCommand, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub const ALL: [MergeMethod; 3] =
        [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase];

    pub fn flag(self) -> &'static str {
        match self {
            MergeMethod::Merge => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MergeMethod::Merge => "merge commit",
            MergeMethod::Squash => "squash and merge",
            MergeMethod::Rebase => "rebase and merge",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            MergeMethod::Merge => "keeps every commit and adds a merge commit",
            MergeMethod::Squash => "combines the commits into one on the base branch",
            MergeMethod::Rebase => "replays each commit onto the base branch",
        }
    }
}

/// A change made on GitHub as the logged-in user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteAction {
    Approve,
    Comment {
        body: String,
    },
    RequestChanges {
        body: String,
    },
    /// `head` is the commit the user looked at; GitHub refuses the merge
    /// if the pull request's head moved since.
    Merge {
        method: MergeMethod,
        head: Option<String>,
    },
}

impl WriteAction {
    /// What the action does, as a verb phrase: `approve`, `comment on`...
    pub fn verb(&self) -> &'static str {
        match self {
            WriteAction::Approve => "approve",
            WriteAction::Comment { .. } => "comment on",
            WriteAction::RequestChanges { .. } => "request changes on",
            WriteAction::Merge { .. } => "merge",
        }
    }

    /// Past tense for the status line: `approved`, `commented on`...
    pub fn done(&self) -> &'static str {
        match self {
            WriteAction::Approve => "approved",
            WriteAction::Comment { .. } => "commented on",
            WriteAction::RequestChanges { .. } => "requested changes on",
            WriteAction::Merge { .. } => "merged",
        }
    }

    /// The body sent on standard input, if any.
    pub fn body(&self) -> Option<&str> {
        match self {
            WriteAction::Comment { body } | WriteAction::RequestChanges { body } => Some(body),
            _ => None,
        }
    }

    /// The exact gh command. Bodies go through standard input
    /// (`--body-file -`), never the command line.
    pub fn command(&self, pr: &PrRef) -> GhCommand {
        let n = pr.number.to_string();
        let repo = pr.repo.clone();
        match self {
            WriteAction::Approve => {
                GhCommand::new(["pr", "review", &n, "--repo", &repo, "--approve"])
            }
            WriteAction::Comment { body } => {
                GhCommand::new(["pr", "comment", &n, "--repo", &repo, "--body-file", "-"])
                    .stdin(body.clone())
            }
            WriteAction::RequestChanges { body } => GhCommand::new([
                "pr",
                "review",
                &n,
                "--repo",
                &repo,
                "--request-changes",
                "--body-file",
                "-",
            ])
            .stdin(body.clone()),
            WriteAction::Merge { method, head } => {
                let mut args: Vec<String> = ["pr", "merge", &n, "--repo", &repo, method.flag()]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                if let Some(h) = head.as_ref().filter(|h| !h.is_empty()) {
                    args.push("--match-head-commit".into());
                    args.push(h.clone());
                }
                GhCommand::new(args)
            }
        }
    }

    /// Runs the action. Returns gh's own words on success, if any.
    pub fn run(&self, gh: &Gh, pr: &PrRef) -> Result<String> {
        let out = gh.run(&self.command(pr))?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }
}

/// `gh pr view --web` (or `gh issue view --web`): opens the page in the
/// user's browser.
pub fn browse_command(pr: &PrRef, is_pr: bool) -> GhCommand {
    let what = if is_pr { "pr" } else { "issue" };
    GhCommand::new([
        what.to_string(),
        "view".into(),
        pr.number.to_string(),
        "--repo".into(),
        pr.repo.clone(),
        "--web".into(),
    ])
}

/// `gh pr checkout <n> --branch <branch> --worktree <path>`, run in the
/// repository `repo_dir`: fetches the pull request's head and checks it
/// out on a new local branch in a new worktree. Nothing is pushed.
pub fn checkout_command(pr: &PrRef, branch: &str, path: &Path, repo_dir: &Path) -> GhCommand {
    GhCommand::new([
        "pr".to_string(),
        "checkout".into(),
        pr.number.to_string(),
        "--repo".into(),
        pr.repo.clone(),
        "--branch".into(),
        branch.into(),
        "--worktree".into(),
        path.display().to_string(),
    ])
    .cwd(repo_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_exact() {
        let pr = PrRef::new("o/r", 7);
        assert_eq!(
            WriteAction::Approve.command(&pr).display(),
            "gh pr review 7 --repo o/r --approve"
        );
        let c = WriteAction::Comment {
            body: "-rf hi".into(),
        }
        .command(&pr);
        assert_eq!(c.display(), "gh pr comment 7 --repo o/r --body-file -");
        assert_eq!(c.stdin.as_deref(), Some("-rf hi"));
        let c = WriteAction::RequestChanges { body: "no".into() }.command(&pr);
        assert_eq!(
            c.display(),
            "gh pr review 7 --repo o/r --request-changes --body-file -"
        );
        let c = WriteAction::Merge {
            method: MergeMethod::Squash,
            head: Some("abc123".into()),
        }
        .command(&pr);
        assert_eq!(
            c.display(),
            "gh pr merge 7 --repo o/r --squash --match-head-commit abc123"
        );
        assert_eq!(
            browse_command(&pr, false).display(),
            "gh issue view 7 --repo o/r --web"
        );
    }
}
