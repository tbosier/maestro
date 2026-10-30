//! Repository discovery, HEAD and base texts for line diffs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::{Access, Git, GitError, Result, bytes_to_path, command_error, split_nul};

/// A Git working tree (the main one or a linked worktree).
#[derive(Debug, Clone)]
pub struct Repository {
    git: Git,
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

/// What `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// On a branch (possibly one with no commits yet).
    Branch(String),
    /// Detached at a commit; holds the abbreviated id.
    Detached(String),
}

impl Head {
    /// Text for a status line: the branch name, or `(detached abc1234)`.
    pub fn label(&self) -> String {
        match self {
            Head::Branch(b) => b.clone(),
            Head::Detached(id) => format!("(detached {id})"),
        }
    }

    pub fn branch(&self) -> Option<&str> {
        match self {
            Head::Branch(b) => Some(b),
            Head::Detached(_) => None,
        }
    }
}

/// What a working file is compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Base {
    /// The staged version (`git diff` semantics).
    #[default]
    Index,
    /// The last commit (`git diff HEAD` semantics).
    Head,
}

pub(crate) fn discover(git: &Git, path: &Path) -> Result<Option<Repository>> {
    // Run from the nearest existing directory: the file may be new.
    let mut dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().map(Path::to_path_buf).unwrap_or_default()
    };
    while !dir.as_os_str().is_empty() && !dir.is_dir() {
        dir = dir.parent().map(Path::to_path_buf).unwrap_or_default();
    }
    if dir.as_os_str().is_empty() {
        dir = PathBuf::from(".");
    }
    let args = [
        "rev-parse",
        "--show-toplevel",
        "--absolute-git-dir",
        "--git-common-dir",
    ];
    let (out, shown) = git.run_status(Some(&dir), &args, Access::ReadOnly)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Outside any repository, in a bare repository, or inside `.git`.
        if err.contains("not a git repository") || err.contains("must be run in a work tree") {
            return Ok(None);
        }
        return Err(command_error(shown, &out));
    }
    // One path per line, as raw bytes. rev-parse has no NUL-separated
    // form, so a path containing a newline is reported as a parse error.
    let stdout = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
    let lines: Vec<&[u8]> = stdout.split(|c| *c == b'\n').collect();
    let [root, git_dir, common] = lines[..] else {
        return Err(GitError::Parse {
            args: shown,
            detail: String::from_utf8_lossy(&out.stdout).into_owned(),
        });
    };
    let common = bytes_to_path(common);
    let common_dir = if common.is_absolute() {
        common
    } else {
        dir.join(common)
    };
    let canon = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    Ok(Some(Repository {
        git: git.clone(),
        root: canon(bytes_to_path(root)),
        git_dir: canon(bytes_to_path(git_dir)),
        common_dir: canon(common_dir),
    }))
}

impl Repository {
    /// The top directory of this working tree.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// This working tree's private Git directory (`.git`, or
    /// `.git/worktrees/<name>` for a linked worktree).
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// The Git directory shared by all worktrees of the repository.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    /// True for the main working tree, false for a linked worktree.
    pub fn is_main_worktree(&self) -> bool {
        self.git_dir == self.common_dir
    }

    /// Files whose modification time changes when the index or `HEAD`
    /// changes: the index, `HEAD` (branch switches) and `HEAD`'s reflog
    /// (commits, resets, and anything else that moves `HEAD`, when reflogs
    /// are enabled, which is the default in working trees). Watching them is
    /// a cheap way to know when to refresh.
    pub fn watch_files(&self) -> [PathBuf; 3] {
        [
            self.git_dir.join("index"),
            self.git_dir.join("HEAD"),
            self.git_dir.join("logs").join("HEAD"),
        ]
    }

    /// `path` relative to the working tree root, or `None` if it is outside.
    pub fn relative(&self, path: &Path) -> Option<PathBuf> {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        // Resolve `..` and directory symlinks through the filesystem, but
        // not the file name itself: Git tracks a symlink by its own path.
        let resolved = match (abs.parent(), abs.file_name()) {
            (Some(parent), Some(name)) => std::fs::canonicalize(parent).ok().map(|p| p.join(name)),
            _ => None,
        };
        let candidate = resolved.unwrap_or_else(|| lexical_normalize(&abs));
        candidate
            .strip_prefix(&self.root)
            .ok()
            .map(Path::to_path_buf)
    }

    pub(crate) fn run<S: AsRef<std::ffi::OsStr>>(
        &self,
        args: &[S],
        access: Access,
    ) -> Result<Vec<u8>> {
        self.git.run(Some(&self.root), args, access)
    }

    pub(crate) fn run_status<S: AsRef<std::ffi::OsStr>>(
        &self,
        args: &[S],
        access: Access,
    ) -> Result<(std::process::Output, String)> {
        self.git.run_status(Some(&self.root), args, access)
    }

    /// The current branch, or the commit when `HEAD` is detached.
    pub fn head(&self) -> Result<Head> {
        let (out, shown) = self.run_status(
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            Access::ReadOnly,
        )?;
        if out.status.success() {
            return Ok(Head::Branch(
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
            ));
        }
        if out.status.code() != Some(1) {
            return Err(command_error(shown, &out));
        }
        let id = self.run(&["rev-parse", "--short", "HEAD"], Access::ReadOnly)?;
        Ok(Head::Detached(
            String::from_utf8_lossy(&id).trim().to_string(),
        ))
    }

    /// The contents of `path` in the index or in `HEAD`. `Ok(None)` when the
    /// file is not there (new, untracked, or in conflict in the index), or is
    /// a submodule.
    pub fn base_text(&self, path: &Path, base: Base) -> Result<Option<Vec<u8>>> {
        let rel = self.relative(path).ok_or_else(|| {
            GitError::InvalidArgument(format!(
                "{} is outside {}",
                path.display(),
                self.root.display()
            ))
        })?;
        let rel_os: OsString = rel.clone().into_os_string();
        let listing = match base {
            Base::Index => self.run(
                &[
                    OsString::from("ls-files"),
                    "--stage".into(),
                    "-z".into(),
                    "--".into(),
                    rel_os,
                ],
                Access::ReadOnly,
            )?,
            Base::Head => {
                let (out, shown) = self.run_status(
                    &[
                        OsString::from("ls-tree"),
                        "-z".into(),
                        "HEAD".into(),
                        "--".into(),
                        rel_os,
                    ],
                    Access::ReadOnly,
                )?;
                if !out.status.success() {
                    // No commits yet.
                    if self
                        .run_status(
                            &["rev-parse", "--verify", "--quiet", "HEAD"],
                            Access::ReadOnly,
                        )?
                        .0
                        .status
                        .code()
                        == Some(1)
                    {
                        return Ok(None);
                    }
                    return Err(command_error(shown, &out));
                }
                out.stdout
            }
        };
        // ls-files --stage: "<mode> <oid> <stage>\t<path>"
        // ls-tree:          "<mode> <type> <oid>\t<path>"
        let mut oid = None;
        for rec in split_nul(&listing) {
            let Some(tab) = rec.iter().position(|c| *c == b'\t') else {
                continue;
            };
            if bytes_to_path(&rec[tab + 1..]) != rel {
                continue;
            }
            let meta = String::from_utf8_lossy(&rec[..tab]);
            let fields: Vec<&str> = meta.split(' ').collect();
            let (mode, id, ok) = match base {
                Base::Index => (
                    fields[0],
                    fields.get(1).copied(),
                    fields.get(2) == Some(&"0"),
                ),
                Base::Head => (
                    fields[0],
                    fields.get(2).copied(),
                    fields.get(1) == Some(&"blob"),
                ),
            };
            if ok && mode != "160000" {
                oid = id.map(str::to_string);
            }
        }
        let Some(oid) = oid else {
            return Ok(None);
        };
        Ok(Some(
            self.run(&["cat-file", "blob", &oid], Access::ReadOnly)?,
        ))
    }

    /// True when `path` is untracked and not ignored.
    pub fn is_untracked(&self, path: &Path) -> Result<bool> {
        let Some(rel) = self.relative(path) else {
            return Ok(false);
        };
        let out = self.run(
            &[
                OsString::from("ls-files"),
                "-z".into(),
                "--others".into(),
                "--exclude-standard".into(),
                "--".into(),
                rel.clone().into_os_string(),
            ],
            Access::ReadOnly,
        )?;
        Ok(split_nul(&out).iter().any(|p| bytes_to_path(p) == rel))
    }

    /// Resolves a revision to a full commit id.
    pub fn resolve(&self, rev: &str) -> Result<String> {
        check_rev(rev)?;
        let out = self.run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{rev}^{{commit}}"),
            ],
            Access::ReadOnly,
        )?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }
}

/// Removes `.` and `..` components without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Refuses revisions that `git` could read as options.
pub(crate) fn check_rev(rev: &str) -> Result<()> {
    if rev.is_empty() || rev.starts_with('-') || rev.contains('\0') {
        return Err(GitError::InvalidArgument(format!("bad revision {rev:?}")));
    }
    Ok(())
}
