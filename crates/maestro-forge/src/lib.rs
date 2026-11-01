//! GitHub for Maestro through the GitHub CLI (`gh`): the pull requests and
//! issues waiting on the user, a pull request's details, checks and diff,
//! and the few write actions the inbox offers (approve, comment, request
//! changes, merge).
//!
//! Every call runs the `gh` program with an argument vector (never through
//! a shell), asks for `--json` fields where gh has them, and parses the
//! answer. Each run has a timeout. Failures come back as a [`ForgeError`]
//! in plain words: gh missing, not logged in, network, rate limit.
//!
//! Every call blocks while `gh` runs. The editor calls this crate only from
//! background threads. This crate has no editor or terminal dependency.

mod inbox;
mod model;
mod parse;
mod time;
mod write;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub use inbox::{
    DEFAULT_LIMIT, Inbox, Section, SectionKind, current_repo, load_inbox, pr_detail, pr_diff,
    pr_status, repo_prs, search_issues, search_prs,
};
pub use model::{
    Check, CheckState, ChecksSummary, FileStat, Item, ItemKind, Mergeable, Overall, PrDetail,
    PrRef, PrStatus, Review, ReviewDecision,
};
pub use parse::{
    parse_detail, parse_pr_list, parse_repo_view, parse_search_issues, parse_search_prs,
    parse_status, plain_text,
};
pub use time::{age_label, parse_time, utc_clock};
pub use write::{MergeMethod, WriteAction, browse_command, checkout_command};

/// How long a `gh` call may take before it is stopped.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors from running `gh`, in words a user can act on.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForgeError {
    #[error(
        "the GitHub CLI was not found ({program}); install it from https://cli.github.com, \
         or set [github] gh in the config to its path"
    )]
    NotInstalled { program: String },
    #[error("gh is not logged in to GitHub; run `gh auth login` in a terminal, then refresh")]
    NotLoggedIn,
    #[error("could not reach GitHub: {0}")]
    Network(String),
    #[error("GitHub's rate limit was reached; wait a few minutes, then refresh ({0})")]
    RateLimited(String),
    #[error("gh did not answer within {secs} s and was stopped")]
    Timeout { secs: u64 },
    #[error("{command} failed: {message}")]
    Command { command: String, message: String },
    #[error("unexpected output from {command}: {detail}")]
    Parse { command: String, detail: String },
    #[error("could not run gh: {0}")]
    Io(String),
}

impl ForgeError {
    /// True for errors that hold for every call (gh missing or logged
    /// out), as opposed to one call failing.
    pub fn is_setup(&self) -> bool {
        matches!(self, ForgeError::NotInstalled { .. } | ForgeError::NotLoggedIn)
    }
}

pub type Result<T, E = ForgeError> = std::result::Result<T, E>;

/// One `gh` invocation: its arguments, what goes to its standard input,
/// and the directory it runs in. [`GhCommand::display`] is what the user
/// is shown before a write action runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhCommand {
    pub args: Vec<String>,
    /// Sent to standard input, then closed. `None` gives gh no input.
    pub stdin: Option<String>,
    pub cwd: Option<PathBuf>,
}

impl GhCommand {
    pub fn new<S: Into<String>>(args: impl IntoIterator<Item = S>) -> Self {
        Self {
            args: args.into_iter().map(Into::into).collect(),
            stdin: None,
            cwd: None,
        }
    }

    pub fn stdin(mut self, input: impl Into<String>) -> Self {
        self.stdin = Some(input.into());
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// The command line as a shell would need it typed, starting with `gh`.
    pub fn display(&self) -> String {
        let mut out = String::from("gh");
        for a in &self.args {
            out.push(' ');
            out.push_str(&shell_quote(a));
        }
        out
    }

    /// The first two words (`gh pr view`), for error messages.
    fn short(&self) -> String {
        let words: Vec<&str> = self
            .args
            .iter()
            .take_while(|a| !a.starts_with('-'))
            .take(2)
            .map(String::as_str)
            .collect();
        format!("gh {}", words.join(" "))
    }
}

/// Quotes `s` for a POSIX shell when it needs it.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | '@' | ',' | '+')
        });
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The `gh` program and how long a call may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gh {
    program: PathBuf,
    timeout: Duration,
}

impl Default for Gh {
    fn default() -> Self {
        Self::new("gh")
    }
}

impl Gh {
    /// `program` is a path, or a name looked up on `PATH`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Runs `cmd` and returns its standard output. A nonzero exit is an
    /// error classified from its standard error. Past the timeout, gh is
    /// killed (its own child processes are not).
    pub fn run(&self, cmd: &GhCommand) -> Result<Vec<u8>> {
        let mut c = Command::new(&self.program);
        c.args(&cmd.args)
            // Never wait on a question nobody can answer, never page, and
            // never print colors or update notices into parsed output.
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
            .env("GH_SPINNER_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(d) = &cmd.cwd {
            c.current_dir(d);
        }
        // ETXTBSY: another thread of this process was forking while the
        // program file was open for writing (a script just written). It
        // clears in a moment.
        let mut spawned = c.spawn();
        for _ in 0..10 {
            match &spawned {
                Err(e) if e.raw_os_error() == Some(26) => {
                    std::thread::sleep(Duration::from_millis(20));
                    spawned = c.spawn();
                }
                _ => break,
            }
        }
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ForgeError::NotInstalled {
                    program: self.program.display().to_string(),
                });
            }
            Err(e) => return Err(ForgeError::Io(format!("{}: {e}", self.program.display()))),
        };
        if let (Some(input), Some(mut w)) = (cmd.stdin.clone(), child.stdin.take()) {
            // A writer thread, so a gh that never reads cannot block us.
            std::thread::spawn(move || {
                let _ = w.write_all(input.as_bytes());
            });
        }
        let out_rx = drain(child.stdout.take());
        let err_rx = drain(child.stderr.take());
        let deadline = Instant::now() + self.timeout;
        let mut pause = Duration::from_millis(2);
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ForgeError::Timeout {
                        secs: self.timeout.as_secs().max(1),
                    });
                }
                Ok(None) => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(Duration::from_millis(25));
                }
                Err(e) => return Err(ForgeError::Io(e.to_string())),
            }
        };
        // A process gh started (a browser) can hold the pipes open after gh
        // exits; read what arrived and do not wait for it.
        let grace = Duration::from_secs(2);
        let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
        let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
        if status.success() {
            Ok(stdout)
        } else {
            Err(classify(
                &cmd.short(),
                status.code(),
                &String::from_utf8_lossy(&stderr),
            ))
        }
    }

    /// Runs `cmd` and parses its output with `parse`.
    pub(crate) fn json<T>(
        &self,
        cmd: &GhCommand,
        parse: impl FnOnce(&[u8]) -> std::result::Result<T, String>,
    ) -> Result<T> {
        let out = self.run(cmd)?;
        parse(&out).map_err(|detail| ForgeError::Parse {
            command: cmd.short(),
            detail,
        })
    }
}

fn drain<R: Read + Send + 'static>(r: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut r) = r {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = r.read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
    } else {
        let _ = tx.send(Vec::new());
    }
    rx
}

/// Phrases in gh's error output that mean the network failed.
const NETWORK: &[&str] = &[
    "dial tcp",
    "connection refused",
    "no such host",
    "could not resolve host",
    "network is unreachable",
    "i/o timeout",
    "tls handshake",
    "error connecting to",
    "connection reset",
    "check your internet connection",
    "proxyconnect",
];

/// Turns a failed gh run into a [`ForgeError`]. gh exits with 4 when it
/// needs a login (see `gh help exit-codes`).
pub fn classify(command: &str, code: Option<i32>, stderr: &str) -> ForgeError {
    let text = stderr.trim();
    let lower = text.to_lowercase();
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let first = clip(first, 200);
    if code == Some(4)
        || lower.contains("to get started with github cli")
        || lower.contains("not logged into any github hosts")
        || lower.contains("http 401")
        || lower.contains("bad credentials")
    {
        ForgeError::NotLoggedIn
    } else if lower.contains("rate limit") {
        ForgeError::RateLimited(first)
    } else if NETWORK.iter().any(|p| lower.contains(p)) {
        ForgeError::Network(first)
    } else {
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let message = if lines.is_empty() {
            match code {
                Some(c) => format!("exit status {c}, no message"),
                None => "stopped by a signal".into(),
            }
        } else {
            clip(&lines[..lines.len().min(3)].join("; "), 300)
        };
        ForgeError::Command {
            command: command.to_string(),
            message,
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_only_what_needs_it() {
        let c = GhCommand::new(["pr", "review", "41", "--repo", "a/b", "--approve"]);
        assert_eq!(c.display(), "gh pr review 41 --repo a/b --approve");
        let c = GhCommand::new(["x", "it's here", ""]);
        assert_eq!(c.display(), r"gh x 'it'\''s here' ''");
        assert_eq!(c.short(), "gh x it's here");
    }

    #[test]
    fn classifies_real_gh_messages() {
        // Recorded from gh 2.102 with an empty config directory.
        let logged_out = "To get started with GitHub CLI, please run:  gh auth login\n\
            Alternatively, populate the GH_TOKEN environment variable with a GitHub API \
            authentication token.\n";
        assert_eq!(
            classify("gh search prs", Some(4), logged_out),
            ForgeError::NotLoggedIn
        );
        // Recorded with HTTPS_PROXY pointing at a closed port.
        let net = "Post \"https://api.github.com/graphql\": proxyconnect tcp: dial tcp \
            127.0.0.1:9: connect: connection refused\n";
        assert!(matches!(
            classify("gh search prs", Some(1), net),
            ForgeError::Network(m) if m.contains("connection refused")
        ));
        let limited = "GraphQL: API rate limit exceeded for user ID 1.\n";
        assert!(matches!(
            classify("gh pr view", Some(1), limited),
            ForgeError::RateLimited(_)
        ));
        // A "gh auth login" hint alone (gh repo view outside a GitHub
        // repository says it) is not a missing login.
        let not_found = "GraphQL: Could not resolve to a PullRequest with the number of \
            999999999. (repository.pullRequest)\n";
        assert_eq!(
            classify("gh pr view", Some(1), not_found),
            ForgeError::Command {
                command: "gh pr view".into(),
                message: not_found.trim().into()
            }
        );
        assert!(matches!(
            classify("gh pr view", Some(1), ""),
            ForgeError::Command { message, .. } if message == "exit status 1, no message"
        ));
    }

    #[test]
    fn a_missing_program_is_not_installed() {
        let gh = Gh::new("/nonexistent/maestro-test/gh");
        assert_eq!(
            gh.run(&GhCommand::new(["--version"])),
            Err(ForgeError::NotInstalled {
                program: "/nonexistent/maestro-test/gh".into()
            })
        );
    }
}
