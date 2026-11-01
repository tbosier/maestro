//! Git for Maestro: repository discovery, status, line-level diffs for gutter
//! signs, a unified diff model, worktree lifecycle and merge checks.
//!
//! Everything goes through the `git` command-line program with argument
//! vectors; nothing is ever passed through a shell. ADR 0005 explains why the
//! CLI and not a library. This crate has no editor or terminal dependency, so
//! the editor, the automation API and agent orchestration can all use it.
//!
//! Every call blocks while `git` runs. Callers on an interactive thread
//! should run them on a background thread.

mod diff;
mod intraline;
mod linediff;
mod merge;
mod repo;
mod review;
mod status;
mod worktree;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub use diff::{DiffFile, DiffLine, DiffLineKind, DiffSpec, FileChange, Hunk};
pub use intraline::{INTRALINE_MAX_LINE, intraline};
pub use linediff::{LineHunk, LineHunkKind, Sign, SignMap, diff_lines};
pub use merge::MergeCheck;
pub use repo::{Base, Head, Repository};
pub use review::{
    CommitShape, IntegrateError, IntegrateOutcome, Integration, MERGE_TREE_TREES, WorkSnapshot,
};
pub use status::{BranchInfo, ChangeKind, ConflictKind, EntryState, Status, StatusEntry};
pub use worktree::{RemoveError, RemoveRefusal, Worktree};

/// Errors from running `git`.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    NotInstalled,
    #[error("{0} is not inside a Git working tree")]
    NotARepository(PathBuf),
    #[error("git {args} failed ({status}): {stderr}")]
    Command {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("{feature} needs git {need} or newer; this is git {have}")]
    Unsupported {
        feature: &'static str,
        need: GitVersion,
        have: GitVersion,
    },
    #[error("unexpected output from git {args}: {detail}")]
    Parse { args: String, detail: String },
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// Git has no user name and email to commit with. Holds git's message.
    #[error("git has no identity to commit with; set user.name and user.email ({0})")]
    NoIdentity(String),
    /// Files are in conflict; committing now would commit conflict markers.
    #[error("{} file(s) are in conflict; resolve them first", .0.len())]
    Unmerged(Vec<PathBuf>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// A `git --version` number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl GitVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses `git version 2.39.2`, `git version 2.37.1 (Apple Git-137.1)`
    /// or `git version 2.41.0.windows.1`.
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.trim().strip_prefix("git version ")?;
        let mut nums = rest
            .split(|c: char| !c.is_ascii_digit())
            .take(3)
            .map(|n| n.parse::<u32>().ok());
        let major = nums.next()??;
        let minor = nums.next()??;
        let patch = nums.next().flatten().unwrap_or(0);
        Some(Self::new(major, minor, patch))
    }
}

impl std::fmt::Display for GitVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Environment variables that would make `git` ignore the directory it is
/// run in. Maestro finds repositories from paths, so they are cleared.
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// How to run `git`: the program and any extra environment or `-c` config.
/// The default runs `git` from `PATH` with the user's own configuration.
#[derive(Debug, Clone)]
pub struct Git {
    program: PathBuf,
    /// `None` removes the variable.
    env: Vec<(OsString, Option<OsString>)>,
    config: Vec<String>,
    /// Filled by the first successful [`Git::version`] call.
    version: std::sync::Arc<std::sync::OnceLock<GitVersion>>,
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

impl Git {
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("git"),
            env: Vec::new(),
            config: Vec::new(),
            version: Default::default(),
        }
    }

    /// Uses a specific `git` executable.
    pub fn program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self.version = Default::default();
        self
    }

    /// Sets an environment variable for every `git` this runs.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), Some(value.into())));
        self
    }

    /// Removes an inherited environment variable for every `git` this runs.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env.push((key.into(), None));
        self
    }

    /// Adds `-c name=value` to every `git` this runs.
    pub fn config(mut self, name_value: impl Into<String>) -> Self {
        self.config.push(name_value.into());
        self
    }

    /// The installed `git` version (asked once, then remembered).
    pub fn version(&self) -> Result<GitVersion> {
        if let Some(v) = self.version.get() {
            return Ok(*v);
        }
        let out = self.run(None, &["--version"], Access::ReadOnly)?;
        let text = String::from_utf8_lossy(&out);
        let v = GitVersion::parse(&text).ok_or_else(|| GitError::Parse {
            args: "--version".into(),
            detail: text.trim().to_string(),
        })?;
        Ok(*self.version.get_or_init(|| v))
    }

    /// Finds the working tree containing `path` (a file or directory, which
    /// need not exist yet). `Ok(None)` when it is not in a working tree.
    pub fn discover(&self, path: &Path) -> Result<Option<Repository>> {
        repo::discover(self, path)
    }

    fn command(&self, dir: Option<&Path>, args: &[&OsStr], access: Access) -> Command {
        let mut cmd = Command::new(&self.program);
        for c in &self.config {
            cmd.arg("-c").arg(c);
        }
        cmd.arg("--no-pager");
        cmd.args(args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        for k in SCRUBBED_ENV {
            cmd.env_remove(k);
        }
        // No terminal prompt for credentials. A configured askpass helper
        // can still run; nothing here enforces a timeout.
        cmd.env("GIT_TERMINAL_PROMPT", "0");
        cmd.env("GIT_PAGER", "cat");
        cmd.env("LC_ALL", "C");
        // Path arguments are file names, never pathspec magic or globs.
        cmd.env("GIT_LITERAL_PATHSPECS", "1");
        if access == Access::ReadOnly {
            // Do not take the index lock to refresh stat data, so a status
            // call never makes the user's own git command fail.
            cmd.env("GIT_OPTIONAL_LOCKS", "0");
            // In a partial clone, a missing blob is an error, not a network
            // fetch (honored by git 2.44 and newer).
            cmd.env("GIT_NO_LAZY_FETCH", "1");
        }
        for (k, v) in &self.env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        cmd.stdin(Stdio::null());
        cmd
    }

    fn output(
        &self,
        dir: Option<&Path>,
        args: &[&OsStr],
        access: Access,
    ) -> Result<(Output, String)> {
        let shown = args
            .iter()
            .map(|a| a.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let out = self
            .command(dir, args, access)
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => GitError::NotInstalled,
                _ => GitError::Io(e),
            })?;
        Ok((out, shown))
    }

    /// Runs `git` and returns stdout, or an error carrying stderr.
    fn run<S: AsRef<OsStr>>(
        &self,
        dir: Option<&Path>,
        args: &[S],
        access: Access,
    ) -> Result<Vec<u8>> {
        let args: Vec<&OsStr> = args.iter().map(AsRef::as_ref).collect();
        let (out, shown) = self.output(dir, &args, access)?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(command_error(shown, &out))
        }
    }

    /// Runs `git` and returns the raw output whatever the exit status.
    fn run_status<S: AsRef<OsStr>>(
        &self,
        dir: Option<&Path>,
        args: &[S],
        access: Access,
    ) -> Result<(Output, String)> {
        let args: Vec<&OsStr> = args.iter().map(AsRef::as_ref).collect();
        self.output(dir, &args, access)
    }
}

fn command_error(args: String, out: &Output) -> GitError {
    GitError::Command {
        args,
        status: out
            .status
            .code()
            .map_or_else(|| "killed".to_string(), |c| format!("exit {c}")),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    }
}

/// Whether a command may write to the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    ReadOnly,
    Write,
}

/// Converts raw path bytes from git output to a path.
fn bytes_to_path(b: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

/// Splits NUL-terminated output into fields, dropping the final empty one.
fn split_nul(b: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = b.split(|c| *c == 0).collect();
    if v.last().is_some_and(|l| l.is_empty()) {
        v.pop();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(
            GitVersion::parse("git version 2.56.0\n"),
            Some(GitVersion::new(2, 56, 0))
        );
        assert_eq!(
            GitVersion::parse("git version 2.37.1 (Apple Git-137.1)"),
            Some(GitVersion::new(2, 37, 1))
        );
        assert_eq!(
            GitVersion::parse("git version 2.41.0.windows.1"),
            Some(GitVersion::new(2, 41, 0))
        );
        assert_eq!(
            GitVersion::parse("git version 3.0"),
            Some(GitVersion::new(3, 0, 0))
        );
        assert_eq!(GitVersion::parse("hg 6.1"), None);
        assert!(GitVersion::new(2, 38, 0) > GitVersion::new(2, 37, 9));
    }
}
