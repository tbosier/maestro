//! The commands that change something: reviews, comments and merges on
//! GitHub, opening a browser, and checking a pull request out locally.
//! Each is built as a [`GhCommand`] first, so the editor can show exactly
//! what will run (and what goes to its standard input) before it runs.

use std::path::Path;

use crate::model::PrRef;
use crate::{ForgeError, Gh, GhCommand, Result};

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

/// A change made on GitHub as the logged-in user. `head` is the commit the
/// user looked at: reviews are submitted on that commit, and a merge is
/// refused by GitHub if the pull request's head moved since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteAction {
    Approve { head: String },
    Comment { body: String },
    RequestChanges { body: String, head: String },
    Merge { method: MergeMethod, head: String },
}

/// What a write did, as far as GitHub says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// The merge command succeeded and the pull request's state was read
    /// afterward: `MERGED`, or `OPEN` when a merge queue or auto-merge
    /// took it instead.
    MergeState(Result<String>),
}

impl WriteAction {
    /// What the action does, as a verb phrase: `approve`, `comment on`...
    pub fn verb(&self) -> &'static str {
        match self {
            WriteAction::Approve { .. } => "approve",
            WriteAction::Comment { .. } => "comment on",
            WriteAction::RequestChanges { .. } => "request changes on",
            WriteAction::Merge { .. } => "merge",
        }
    }

    /// Past tense for the status line: `approved`, `commented on`... For a
    /// merge, see [`Outcome::MergeState`] for what actually happened.
    pub fn done(&self) -> &'static str {
        match self {
            WriteAction::Approve { .. } => "approved",
            WriteAction::Comment { .. } => "commented on",
            WriteAction::RequestChanges { .. } => "requested changes on",
            WriteAction::Merge { .. } => "merged",
        }
    }

    /// The text the user wrote, if any.
    pub fn body(&self) -> Option<&str> {
        match self {
            WriteAction::Comment { body } | WriteAction::RequestChanges { body, .. } => Some(body),
            _ => None,
        }
    }

    /// The commit the action is pinned to, if any.
    pub fn head(&self) -> Option<&str> {
        match self {
            WriteAction::Approve { head }
            | WriteAction::RequestChanges { head, .. }
            | WriteAction::Merge { head, .. } => Some(head),
            WriteAction::Comment { .. } => None,
        }
    }

    /// The exact gh command. Text goes through standard input, never the
    /// command line. Reviews go through the REST API (`gh api`), because
    /// `gh pr review` cannot name the commit being reviewed.
    pub fn command(&self, pr: &PrRef) -> GhCommand {
        let n = pr.number.to_string();
        let repo = pr.repo.clone();
        match self {
            WriteAction::Approve { head } => review_command(pr, "APPROVE", head, None),
            WriteAction::RequestChanges { body, head } => {
                review_command(pr, "REQUEST_CHANGES", head, Some(body))
            }
            WriteAction::Comment { body } => {
                GhCommand::new(["pr", "comment", &n, "--repo", &repo, "--body-file", "-"])
                    .stdin(body.clone())
            }
            WriteAction::Merge { method, head } => GhCommand::new([
                "pr",
                "merge",
                &n,
                "--repo",
                &repo,
                method.flag(),
                "--match-head-commit",
                head,
            ]),
        }
    }

    /// Runs the action. A review first checks that the pull request's head
    /// is still the commit shown, and sends nothing if it moved. After a
    /// merge, the pull request's state is read back.
    pub fn run(&self, gh: &Gh, pr: &PrRef) -> Result<Outcome> {
        if let WriteAction::Approve { head } | WriteAction::RequestChanges { head, .. } = self {
            let now = crate::pr_head(gh, pr)?;
            if now != *head {
                return Err(ForgeError::HeadMoved {
                    shown: head.clone(),
                    now,
                });
            }
        }
        gh.run(&self.command(pr))?;
        Ok(match self {
            WriteAction::Merge { .. } => Outcome::MergeState(crate::pr_state(gh, pr)),
            _ => Outcome::Done,
        })
    }
}

/// `POST repos/<owner>/<name>/pulls/<n>/reviews` with the event, the
/// commit and the text as JSON on standard input.
fn review_command(pr: &PrRef, event: &str, head: &str, body: Option<&str>) -> GhCommand {
    let (host, name) = pr.host_and_name();
    // Inserted in key order, so the text is the same whether or not
    // serde_json keeps insertion order.
    let mut payload = serde_json::Map::new();
    if let Some(b) = body {
        payload.insert("body".into(), b.into());
    }
    payload.insert("commit_id".into(), head.into());
    payload.insert("event".into(), event.into());
    let mut args: Vec<String> = vec!["api".into()];
    if let Some(h) = host {
        args.push("--hostname".into());
        args.push(h.into());
    }
    args.extend([
        "--method".into(),
        "POST".into(),
        format!("repos/{name}/pulls/{}/reviews", pr.number),
        "--input".into(),
        "-".into(),
    ]);
    GhCommand::new(args).stdin(serde_json::Value::Object(payload).to_string())
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
        let c = WriteAction::Approve { head: "abc".into() }.command(&pr);
        assert_eq!(
            c.display(),
            "gh api --method POST repos/o/r/pulls/7/reviews --input -"
        );
        assert_eq!(
            c.stdin.as_deref(),
            Some(r#"{"commit_id":"abc","event":"APPROVE"}"#)
        );
        let c = WriteAction::Comment {
            body: "-rf hi".into(),
        }
        .command(&pr);
        assert_eq!(c.display(), "gh pr comment 7 --repo o/r --body-file -");
        assert_eq!(c.stdin.as_deref(), Some("-rf hi"));
        let c = WriteAction::RequestChanges {
            body: "no\n\"x\"".into(),
            head: "abc".into(),
        }
        .command(&PrRef::new("ghe.example.com/o/r", 7));
        assert_eq!(
            c.display(),
            "gh api --hostname ghe.example.com --method POST repos/o/r/pulls/7/reviews --input -"
        );
        assert_eq!(
            c.stdin.as_deref(),
            Some(r#"{"body":"no\n\"x\"","commit_id":"abc","event":"REQUEST_CHANGES"}"#)
        );
        let c = WriteAction::Merge {
            method: MergeMethod::Squash,
            head: "abc123".into(),
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
