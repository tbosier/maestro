//! Staging, unstaging and discarding whole files or patches, and
//! committing what is staged.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::diff::diff_args;
use crate::{
    Access, DiffSpec, FilePatch, GitError, Repository, Result, bytes_to_path, parse_patches,
    split_nul,
};

/// Where `git apply` puts a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTo {
    /// The index only (`--cached`): the working tree file is not touched.
    Index,
    /// The working tree only.
    Worktree,
    /// Both (`--index`); git refuses when the file's working tree copy
    /// differs from the index.
    Both,
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn with_paths(mut args: Vec<OsString>, paths: &[&Path]) -> Vec<OsString> {
    args.push("--".into());
    args.extend(paths.iter().map(|p| p.as_os_str().to_os_string()));
    args
}

/// Refuses absolute paths, `..` and empty paths: callers pass paths
/// relative to the working tree root, from git's own output.
fn check_paths(paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Err(GitError::InvalidArgument("no path given".into()));
    }
    for p in paths {
        let plain = p.components().all(|c| matches!(c, Component::Normal(_)));
        if p.as_os_str().is_empty() || !plain {
            return Err(GitError::InvalidArgument(format!(
                "{} is not a path inside the working tree",
                p.display()
            )));
        }
    }
    Ok(())
}

impl Repository {
    /// Like [`Repository::diff`], with the exact bytes of every line kept
    /// for building partial patches.
    pub fn file_patches(&self, spec: &DiffSpec, paths: &[PathBuf]) -> Result<Vec<FilePatch>> {
        let mut args = diff_args(spec)?;
        args.push("--".into());
        args.extend(paths.iter().map(|p| p.clone().into_os_string()));
        let out = self.run(&args, Access::ReadOnly)?;
        parse_patches(&out).map_err(|detail| GitError::Parse {
            args: "diff".into(),
            detail,
        })
    }

    /// Applies `patch` (from [`FilePatch::build`]) to the index, the
    /// working tree or both, undoing it when `reverse` is set. Git checks
    /// the whole patch first (`--check`) and changes nothing when any part
    /// does not apply; every context line must match (no fuzz), though git
    /// may find a hunk a few lines from where it says, so callers that need
    /// the exact place check that the diff is still the one they built the
    /// patch from. The user's `apply.whitespace` setting is overridden so
    /// whitespace never changes what is applied.
    pub fn apply_patch(&self, patch: &[u8], to: ApplyTo, reverse: bool) -> Result<()> {
        let mut args = os(&["apply", "--whitespace=nowarn"]);
        match to {
            ApplyTo::Index => args.push("--cached".into()),
            ApplyTo::Worktree => {}
            ApplyTo::Both => args.push("--index".into()),
        }
        if reverse {
            args.push("-R".into());
        }
        let mut check = args.clone();
        check.push("--check".into());
        check.push("-".into());
        self.run_input(&check, patch, Access::ReadOnly)?;
        args.push("-".into());
        self.run_input(&args, patch, Access::Write)?;
        Ok(())
    }

    /// Stages whole files, including deletions (`git add --all`).
    pub fn stage_paths(&self, paths: &[&Path]) -> Result<()> {
        check_paths(paths)?;
        self.run(&with_paths(os(&["add", "--all"]), paths), Access::Write)?;
        Ok(())
    }

    /// Takes whole files out of the index back to `HEAD` (or out of the
    /// index entirely before the first commit). The working tree is not
    /// touched.
    pub fn unstage_paths(&self, paths: &[&Path]) -> Result<()> {
        check_paths(paths)?;
        let args = if self.has_head()? {
            os(&["reset", "-q", "HEAD"])
        } else {
            os(&["rm", "-q", "-r", "--cached", "--ignore-unmatch"])
        };
        self.run(&with_paths(args, paths), Access::Write)?;
        Ok(())
    }

    /// Puts working tree files back to their staged version, discarding
    /// unstaged changes to them.
    pub fn discard_paths(&self, paths: &[&Path]) -> Result<()> {
        check_paths(paths)?;
        self.run(
            &with_paths(os(&["restore", "--worktree"]), paths),
            Access::Write,
        )?;
        Ok(())
    }

    /// Puts files back to `HEAD` in both the index and the working tree;
    /// a path that is not in `HEAD` is removed from both.
    pub fn discard_staged_paths(&self, paths: &[&Path]) -> Result<()> {
        check_paths(paths)?;
        if !self.has_head()? {
            return Err(GitError::InvalidArgument(
                "there is no commit yet to go back to".into(),
            ));
        }
        self.run(
            &with_paths(
                os(&["restore", "--source=HEAD", "--staged", "--worktree"]),
                paths,
            ),
            Access::Write,
        )?;
        Ok(())
    }

    /// Deletes an untracked file. Refuses a path git does not list as
    /// untracked (tracked or ignored), and a directory.
    pub fn remove_untracked(&self, path: &Path) -> Result<()> {
        check_paths(&[path])?;
        let listed = self.run(
            &with_paths(
                os(&["ls-files", "-z", "--others", "--exclude-standard"]),
                &[path],
            ),
            Access::ReadOnly,
        )?;
        if !split_nul(&listed).iter().any(|p| bytes_to_path(p) == path) {
            return Err(GitError::InvalidArgument(format!(
                "{} is not an untracked file",
                path.display()
            )));
        }
        let full = self.root().join(path);
        let meta = std::fs::symlink_metadata(&full)?;
        if meta.is_dir() {
            return Err(GitError::InvalidArgument(format!(
                "{} is a directory; not touching it",
                path.display()
            )));
        }
        std::fs::remove_file(full)?;
        Ok(())
    }

    /// The bytes `git add` would stage for the working tree file `path`:
    /// its contents after line-ending conversion and clean filters. The
    /// blob is written to the object store (unreferenced until staged).
    pub fn staged_form(&self, path: &Path) -> Result<Vec<u8>> {
        let rel = self.relative(path).ok_or_else(|| {
            GitError::InvalidArgument(format!(
                "{} is outside {}",
                path.display(),
                self.root().display()
            ))
        })?;
        let out = self.run(
            &with_paths(os(&["hash-object", "-w"]), &[rel.as_path()]),
            Access::Write,
        )?;
        let oid = String::from_utf8_lossy(&out).trim().to_string();
        crate::repo::check_rev(&oid)?;
        self.run(&["cat-file", "blob", &oid], Access::ReadOnly)
    }

    /// True once the branch has a commit.
    pub fn has_head(&self) -> Result<bool> {
        self.has_pseudo_ref("HEAD")
    }

    /// Commits what is staged with exactly `message` (no cleanup, no
    /// editor), amending `HEAD` when `amend` is set. The user's hooks run.
    /// Returns the new commit's id.
    pub fn commit(&self, message: &str, amend: bool) -> Result<String> {
        if message.trim().is_empty() {
            return Err(GitError::InvalidArgument("empty commit message".into()));
        }
        let mut args = os(&["commit", "--quiet", "--cleanup=verbatim", "-F", "-"]);
        if amend {
            args.push("--amend".into());
        }
        let writer = self.with_git(
            self.git()
                .clone()
                .env("GIT_EDITOR", "true")
                .env("GIT_SEQUENCE_EDITOR", "true"),
        );
        writer.run_input(&args, message.as_bytes(), Access::Write)?;
        self.resolve("HEAD")
    }

    /// The full message of `HEAD`, read from the commit object.
    pub fn head_message(&self) -> Result<String> {
        let out = self.run(&["cat-file", "commit", "HEAD"], Access::ReadOnly)?;
        let text = String::from_utf8_lossy(&out);
        Ok(text
            .split_once("\n\n")
            .map(|(_, m)| m.to_string())
            .unwrap_or_default())
    }

    /// Remote-tracking branches that already contain `HEAD`: amending it
    /// would rewrite a commit others may have.
    pub fn head_on_remotes(&self) -> Result<Vec<String>> {
        let out = self.run(
            &[
                "for-each-ref",
                "--contains",
                "HEAD",
                "--format=%(refname:short)",
                "refs/remotes",
            ],
            Access::ReadOnly,
        )?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter(|l| !l.is_empty() && !l.ends_with("/HEAD"))
            .map(str::to_string)
            .collect())
    }

    /// `git diff --cached --stat`: what a commit would contain.
    pub fn staged_stat(&self) -> Result<String> {
        let out = self.run(
            &[
                "diff",
                "--cached",
                "--stat",
                "--no-color",
                "--no-ext-diff",
                "--find-renames",
            ],
            Access::ReadOnly,
        )?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}
