//! Merge safety checks that never touch the working tree or the index.

use std::path::PathBuf;

use crate::repo::check_rev;
use crate::{
    Access, GitError, GitVersion, Repository, Result, bytes_to_path, command_error, split_nul,
};

/// The result of a trial merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeCheck {
    /// The merge would succeed; `tree` is the merged tree's id.
    Clean { tree: String },
    /// The merge would stop with conflicts in these files (relative to the
    /// repository root). `tree` holds the result with conflict markers.
    Conflicts { tree: String, files: Vec<PathBuf> },
}

impl MergeCheck {
    pub fn is_clean(&self) -> bool {
        matches!(self, MergeCheck::Clean { .. })
    }
}

/// `git merge-tree --write-tree` arrived in 2.38.
pub const MERGE_TREE_WRITE: GitVersion = GitVersion::new(2, 38, 0);

impl Repository {
    /// True when `target` can be fast-forwarded to `source`, that is when
    /// `target` is an ancestor of (or the same commit as) `source`.
    pub fn can_fast_forward(&self, target: &str, source: &str) -> Result<bool> {
        check_rev(target)?;
        check_rev(source)?;
        let (out, shown) = self.run_status(
            &["merge-base", "--is-ancestor", target, source],
            Access::ReadOnly,
        )?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(command_error(shown, &out)),
        }
    }

    /// Merges `theirs` into `ours` in memory and reports conflicts, without
    /// changing any branch, the index or the working tree. It does write
    /// the resulting tree objects to the object store (unreferenced, so `gc`
    /// drops them). Needs git 2.38 or newer.
    pub fn merge_check(&self, ours: &str, theirs: &str) -> Result<MergeCheck> {
        check_rev(ours)?;
        check_rev(theirs)?;
        let have = self.git().version()?;
        if have < MERGE_TREE_WRITE {
            return Err(GitError::Unsupported {
                feature: "a dry-run merge (merge-tree --write-tree)",
                need: MERGE_TREE_WRITE,
                have,
            });
        }
        let (out, shown) = self.run_status(
            &[
                "merge-tree",
                "--write-tree",
                "-z",
                "--name-only",
                "--no-messages",
                ours,
                theirs,
            ],
            Access::ReadOnly,
        )?;
        let conflicted = match out.status.code() {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(command_error(shown, &out)),
        };
        let fields = split_nul(&out.stdout);
        let Some((tree, files)) = fields.split_first() else {
            return Err(GitError::Parse {
                args: shown,
                detail: "no tree id".into(),
            });
        };
        let tree = String::from_utf8_lossy(tree).trim().to_string();
        if !conflicted {
            return Ok(MergeCheck::Clean { tree });
        }
        let mut files: Vec<PathBuf> = files.iter().map(|f| bytes_to_path(f)).collect();
        files.dedup();
        Ok(MergeCheck::Conflicts { tree, files })
    }
}
