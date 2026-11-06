//! What a worktree changed against a base revision, by file: the facts the
//! file tree marks each file with. [`Repository::default_branch`] names the
//! branch work forks from, [`Repository::merge_base`] finds the fork point,
//! [`Repository::changes_since`] lists every file that differs between that
//! point and the files on disk (untracked files included), and
//! [`Repository::diff_against`] and [`Repository::diff_untracked`] give one
//! file's diff.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::diff::diff_args;
use crate::repo::check_rev;
use crate::{Access, DiffFile, FileChange, GitError, Repository, Result, bytes_to_path, split_nul};

/// How a file differs from the base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathChangeKind {
    /// New in the index or a commit since the base.
    Added,
    Modified,
    Deleted,
    /// Moved from `from` (relative to the root), possibly also edited.
    Renamed {
        from: PathBuf,
    },
    /// On disk, not tracked and not ignored.
    Untracked,
}

impl PathChangeKind {
    /// One letter for the mark: `A`, `M`, `D`, `R`, `?`.
    pub fn letter(&self) -> char {
        match self {
            PathChangeKind::Added => 'A',
            PathChangeKind::Modified => 'M',
            PathChangeKind::Deleted => 'D',
            PathChangeKind::Renamed { .. } => 'R',
            PathChangeKind::Untracked => '?',
        }
    }

    /// The word for it: `added`, `modified`...
    pub fn word(&self) -> &'static str {
        match self {
            PathChangeKind::Added => "added",
            PathChangeKind::Modified => "modified",
            PathChangeKind::Deleted => "deleted",
            PathChangeKind::Renamed { .. } => "renamed",
            PathChangeKind::Untracked => "untracked",
        }
    }
}

/// One changed file, relative to the working tree root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    pub path: PathBuf,
    pub kind: PathChangeKind,
}

impl Repository {
    /// The branch work forks from, as a full ref name so that a tag or
    /// another ref with the same short name cannot stand in for it:
    /// `origin/HEAD`'s target (`refs/remotes/origin/main`), else
    /// `refs/heads/main`, else `refs/heads/master`. `None` when none of
    /// them exists. [`short_ref`] gives the name to show.
    pub fn default_branch(&self) -> Result<Option<String>> {
        let (out, _) = self.run_status(
            &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
            Access::ReadOnly,
        )?;
        if out.status.success() {
            let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if full.starts_with("refs/remotes/") && self.is_commit(&full)? {
                return Ok(Some(full));
            }
        }
        for name in ["refs/heads/main", "refs/heads/master"] {
            if self.is_commit(name)? {
                return Ok(Some(name.to_string()));
            }
        }
        Ok(None)
    }

    /// True when `rev` names a commit.
    fn is_commit(&self, rev: &str) -> Result<bool> {
        check_rev(rev)?;
        let (out, _) = self.run_status(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{rev}^{{commit}}"),
            ],
            Access::ReadOnly,
        )?;
        Ok(out.status.success())
    }

    /// The best common ancestor of `a` and `b`, as a full commit id. `None`
    /// when they share no history.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        check_rev(a)?;
        check_rev(b)?;
        let (out, shown) =
            self.run_status(&["merge-base", "--end-of-options", a, b], Access::ReadOnly)?;
        match out.status.code() {
            Some(0) => {
                let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
                check_rev(&id)?;
                Ok(Some(id))
            }
            Some(1) => Ok(None),
            _ => Err(crate::command_error(shown, &out)),
        }
    }

    /// Every file on disk (through the index) that differs from `rev`, with
    /// renames detected among tracked files, then every untracked file that
    /// is not ignored. With `rev` `None` (a branch with no commits yet),
    /// every file in the index counts as added. Sorted by path.
    pub fn changes_since(&self, rev: Option<&str>) -> Result<Vec<PathChange>> {
        let mut out = Vec::new();
        match rev {
            Some(rev) => {
                check_rev(rev)?;
                let raw = self.run(
                    &[
                        "diff",
                        "--name-status",
                        "-z",
                        "--no-color",
                        "--no-ext-diff",
                        "--find-renames",
                        rev,
                        "--",
                    ],
                    Access::ReadOnly,
                )?;
                out = parse_name_status(&raw).map_err(|detail| GitError::Parse {
                    args: "diff --name-status".into(),
                    detail,
                })?;
            }
            None => {
                let raw = self.run(&["ls-files", "-z", "--cached"], Access::ReadOnly)?;
                let mut seen = std::collections::HashSet::new();
                for p in split_nul(&raw) {
                    // A file in conflict is listed once per stage.
                    if seen.insert(p) {
                        out.push(PathChange {
                            path: bytes_to_path(p),
                            kind: PathChangeKind::Added,
                        });
                    }
                }
            }
        }
        let untracked = self.run(
            &["ls-files", "-z", "--others", "--exclude-standard"],
            Access::ReadOnly,
        )?;
        out.extend(split_nul(&untracked).into_iter().map(|p| PathChange {
            path: bytes_to_path(p),
            kind: PathChangeKind::Untracked,
        }));
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// The diff of `paths` (relative to the root) between `rev` and the
    /// files on disk, through the index, with renames detected when both
    /// names are given. Untracked files are not in it; see
    /// [`Repository::diff_untracked`].
    pub fn diff_against(&self, rev: &str, paths: &[PathBuf]) -> Result<Vec<DiffFile>> {
        check_rev(rev)?;
        let mut args = diff_args(&crate::DiffSpec::Unstaged)?;
        args.push(rev.into());
        args.push("--".into());
        args.extend(paths.iter().map(|p| p.clone().into_os_string()));
        let out = self.run(&args, Access::ReadOnly)?;
        crate::diff::parse_unified(&out).map_err(|detail| GitError::Parse {
            args: "diff".into(),
            detail,
        })
    }

    /// An untracked file (relative to the root) as a diff that adds every
    /// line. Nothing is written to the repository.
    pub fn diff_untracked(&self, path: &Path) -> Result<DiffFile> {
        let mut args = diff_args(&crate::DiffSpec::Unstaged)?;
        args.push("--no-index".into());
        args.push("--".into());
        args.push("/dev/null".into());
        args.push(OsString::from(path.as_os_str()));
        let (out, shown) = self.run_status(&args, Access::ReadOnly)?;
        // `--no-index` exits 1 when the files differ.
        if !matches!(out.status.code(), Some(0 | 1)) {
            return Err(crate::command_error(shown, &out));
        }
        let mut files =
            crate::diff::parse_unified(&out.stdout).map_err(|detail| GitError::Parse {
                args: "diff --no-index".into(),
                detail,
            })?;
        let mut file = if files.is_empty() {
            // An empty file: git prints nothing.
            DiffFile {
                old_path: None,
                new_path: Some(path.to_path_buf()),
                change: FileChange::Added,
                old_mode: None,
                new_mode: None,
                binary: false,
                unmerged: false,
                hunks: Vec::new(),
            }
        } else {
            files.swap_remove(0)
        };
        file.old_path = None;
        file.new_path = Some(path.to_path_buf());
        file.change = FileChange::Added;
        Ok(file)
    }
}

/// A full ref name as people write it: `refs/heads/main` is `main`,
/// `refs/remotes/origin/main` is `origin/main`.
pub fn short_ref(full: &str) -> &str {
    full.strip_prefix("refs/heads/")
        .or_else(|| full.strip_prefix("refs/remotes/"))
        .unwrap_or(full)
}

/// Parses `git diff --name-status -z`: a status field, then one path, or
/// two for renames and copies.
fn parse_name_status(raw: &[u8]) -> std::result::Result<Vec<PathChange>, String> {
    let fields = split_nul(raw);
    let mut out = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let status = String::from_utf8_lossy(fields[i]).into_owned();
        let letter = status.chars().next().ok_or("empty status field")?;
        let path = |n: usize| {
            fields
                .get(i + n)
                .map(|p| bytes_to_path(p))
                .ok_or_else(|| format!("no path after status {status:?}"))
        };
        match letter {
            'R' | 'C' => {
                let from = path(1)?;
                let to = path(2)?;
                let kind = if letter == 'R' {
                    PathChangeKind::Renamed { from }
                } else {
                    PathChangeKind::Added
                };
                out.push(PathChange { path: to, kind });
                i += 3;
            }
            _ => {
                let kind = match letter {
                    'A' => PathChangeKind::Added,
                    'D' => PathChangeKind::Deleted,
                    // Modified, type changed, unmerged, unknown.
                    _ => PathChangeKind::Modified,
                };
                out.push(PathChange {
                    path: path(1)?,
                    kind,
                });
                i += 2;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_status_reads_renames_and_plain_entries() {
        let raw = b"M\0a.txt\0R087\0old name.rs\0new.rs\0D\0gone\0A\0dir/new\0T\0link\0";
        let got = parse_name_status(raw).unwrap();
        let want = vec![
            PathChange {
                path: "a.txt".into(),
                kind: PathChangeKind::Modified,
            },
            PathChange {
                path: "new.rs".into(),
                kind: PathChangeKind::Renamed {
                    from: "old name.rs".into(),
                },
            },
            PathChange {
                path: "gone".into(),
                kind: PathChangeKind::Deleted,
            },
            PathChange {
                path: "dir/new".into(),
                kind: PathChangeKind::Added,
            },
            PathChange {
                path: "link".into(),
                kind: PathChangeKind::Modified,
            },
        ];
        assert_eq!(got, want);
        assert!(parse_name_status(b"R100\0only-one\0").is_err());
        assert_eq!(parse_name_status(b"").unwrap(), Vec::new());
    }
}
