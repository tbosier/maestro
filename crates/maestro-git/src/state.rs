//! Operations a working tree can be in the middle of.

use crate::{Access, Repository, Result, command_error};

/// A multi-step Git operation that has stopped part way, waiting for the
/// user (conflicts to resolve, a commit to test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    /// `git am`, applying mailed patches.
    ApplyMailbox,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Operation::Merge => "merge",
            Operation::Rebase => "rebase",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
            Operation::Bisect => "bisect",
            Operation::ApplyMailbox => "am",
        }
    }

    /// The commands that finish or abandon it, for a refusal message.
    pub fn way_out(self) -> &'static str {
        match self {
            Operation::Merge => "git merge --continue or git merge --abort",
            Operation::Rebase => "git rebase --continue or git rebase --abort",
            Operation::CherryPick => "git cherry-pick --continue or git cherry-pick --abort",
            Operation::Revert => "git revert --continue or git revert --abort",
            Operation::Bisect => "git bisect reset",
            Operation::ApplyMailbox => "git am --continue or git am --abort",
        }
    }
}

impl Repository {
    /// The operation in progress in this working tree, if any.
    pub fn operation(&self) -> Result<Option<Operation>> {
        let d = self.git_dir();
        // Rebase state comes first: a rebase that replays a merge sets
        // MERGE_HEAD too, and the rebase is what has to be finished.
        if d.join("rebase-merge").is_dir() {
            return Ok(Some(Operation::Rebase));
        }
        if d.join("rebase-apply").is_dir() {
            return Ok(Some(if d.join("rebase-apply").join("applying").exists() {
                Operation::ApplyMailbox
            } else {
                Operation::Rebase
            }));
        }
        for (name, op) in [
            ("MERGE_HEAD", Operation::Merge),
            ("CHERRY_PICK_HEAD", Operation::CherryPick),
            ("REVERT_HEAD", Operation::Revert),
        ] {
            if self.has_pseudo_ref(name)? {
                return Ok(Some(op));
            }
        }
        if d.join("sequencer").is_dir() {
            return Ok(Some(Operation::CherryPick));
        }
        // Bisect state lives in the common directory's per-worktree part.
        if d.join("BISECT_START").exists() || d.join("BISECT_LOG").exists() {
            return Ok(Some(Operation::Bisect));
        }
        Ok(None)
    }

    /// True when a pseudo-ref such as `MERGE_HEAD` is set. Asked through
    /// git, since the reftable backend does not keep them as files.
    pub(crate) fn has_pseudo_ref(&self, name: &str) -> Result<bool> {
        let (out, shown) = self.run_status(
            &["rev-parse", "--verify", "--quiet", name],
            Access::ReadOnly,
        )?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(command_error(shown, &out)),
        }
    }
}
