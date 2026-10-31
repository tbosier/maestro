//! Agent providers and the Agent Client Protocol client. See
//! `docs/decisions/0006-agent-runtime.md`.
//!
//! The crate has no editor or terminal dependency. [`AcpClient`] runs an
//! agent process and speaks ACP (newline-delimited JSON-RPC 2.0) to it on
//! plain threads, reporting everything through a callback as [`AcpEvent`]s.
//! File reads and writes the agent asks for go through [`FileAccess`], which
//! confines them to the agent's root.
//!
//! The confinement only covers what the agent asks the client to do. An
//! agent that runs its own tools (a shell, its own file writes) is not
//! sandboxed by this crate.

pub mod client;
pub mod files;

mod convert;
mod jsonrpc;

#[cfg(feature = "fake-agent")]
pub mod fake;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub use client::AcpClient;
pub use files::{AccessError, FileAccess};

/// Identifies an agent within one editor session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentId(pub u64);

/// Where an agent is in its life. The editor decides the transitions; this
/// crate only names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    Starting,
    Planning,
    Editing,
    RunningTests,
    AwaitingApproval,
    Blocked,
    ReadyForReview,
    Completed,
    Failed,
    Rejected,
    Integrated,
}

impl AgentStatus {
    /// A short lowercase label for lists and status lines.
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Starting => "starting",
            AgentStatus::Planning => "planning",
            AgentStatus::Editing => "editing",
            AgentStatus::RunningTests => "running tests",
            AgentStatus::AwaitingApproval => "awaiting approval",
            AgentStatus::Blocked => "blocked",
            AgentStatus::ReadyForReview => "ready for review",
            AgentStatus::Completed => "completed",
            AgentStatus::Failed => "failed",
            AgentStatus::Rejected => "rejected",
            AgentStatus::Integrated => "integrated",
        }
    }
}

/// How the editor talks to an agent program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderKind {
    /// The program speaks the Agent Client Protocol on stdio.
    Acp,
    /// The program is an interactive terminal application.
    Terminal,
}

/// From config. `Terminal` providers get the task text as `{task}` in args,
/// or typed in after start when no arg names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpec {
    pub name: String,
    pub kind: ProviderKind,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The placeholder a `Terminal` provider's args use for the task text.
pub const TASK_PLACEHOLDER: &str = "{task}";

/// The built-in providers whose programs are on `PATH`.
pub fn builtin_providers() -> Vec<ProviderSpec> {
    builtin_providers_in(std::env::var_os("PATH").as_deref())
}

/// [`builtin_providers`] with an explicit `PATH` value.
///
/// The flags were checked against the installed programs' `--help`:
/// `gemini --acp` (0.39; `--experimental-acp` is the deprecated spelling)
/// and `opencode acp` (1.17) speak ACP. `claude` and `codex` take an
/// initial prompt as a positional argument and are terminal programs; ACP
/// adapters for them exist as separate programs and are not listed here.
pub fn builtin_providers_in(path_var: Option<&OsStr>) -> Vec<ProviderSpec> {
    let table: [(&str, ProviderKind, &[&str]); 4] = [
        ("gemini", ProviderKind::Acp, &["--acp"]),
        ("opencode", ProviderKind::Acp, &["acp"]),
        ("claude", ProviderKind::Terminal, &[TASK_PLACEHOLDER]),
        ("codex", ProviderKind::Terminal, &[TASK_PLACEHOLDER]),
    ];
    table
        .iter()
        .filter(|(program, _, _)| find_executable(program, path_var).is_some())
        .map(|(program, kind, args)| ProviderSpec {
            name: (*program).to_string(),
            kind: *kind,
            program: (*program).to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            env: Vec::new(),
        })
        .collect()
}

/// Messages and tool activity from an ACP agent, in arrival order.
///
/// `User`, `Agent` and `Thought` carry one streamed chunk each; consecutive
/// chunks of the same kind belong to one message. The prompt text the editor
/// sends is not echoed back as a `User` entry; the editor records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntry {
    User(String),
    Agent(String),
    Thought(String),
    Tool {
        id: String,
        title: String,
        /// `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`,
        /// `fetch`, `switch_mode` or `other`.
        kind: String,
        /// `pending`, `in_progress`, `completed` or `failed`.
        status: String,
        locations: Vec<PathBuf>,
    },
    /// Each entry's text and status (`pending`, `in_progress`, `completed`).
    /// A new plan replaces the previous one.
    Plan(Vec<(String, String)>),
    System(String),
}

/// The agent asks before running a tool. Answer with
/// [`AcpClient::answer_permission`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// The JSON-RPC id of the agent's request, echoed back in the answer.
    pub request_id: serde_json::Value,
    /// The tool call this is about, as in [`TranscriptEntry::Tool`].
    pub tool_id: String,
    pub tool_title: String,
    pub options: Vec<PermissionChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionChoice {
    pub id: String,
    pub label: String,
    /// `allow_once`, `allow_always`, `reject_once` or `reject_always`.
    pub kind: String,
}

/// Something an ACP agent did, delivered to the callback given to
/// [`AcpClient::start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpEvent {
    Initialized {
        agent_name: Option<String>,
        load_session: bool,
    },
    SessionStarted {
        session_id: String,
    },
    Update(TranscriptEntry),
    /// A change to an earlier [`TranscriptEntry::Tool`]. `None` fields are
    /// unchanged.
    ToolUpdate {
        id: String,
        status: Option<String>,
        title: Option<String>,
        locations: Option<Vec<PathBuf>>,
    },
    PermissionRequested(PermissionRequest),
    /// The answer to a prompt: `end_turn`, `max_tokens`,
    /// `max_turn_requests`, `refusal` or `cancelled` from the agent, or
    /// `error` when the agent answered the prompt with an error (reported
    /// as an [`AcpEvent::Error`] first) or the session could not start.
    TurnEnded {
        stop_reason: String,
    },
    /// The agent wrote this file (an absolute path inside the root) through
    /// `fs/write_text_file`.
    FileWritten(PathBuf),
    /// One line the agent wrote to stderr.
    Stderr(String),
    /// The agent process ended. `None` when it was killed by a signal or
    /// its status is unknown. Always the last event: anything the agent's
    /// leftover processes write after it is dropped.
    Exited(Option<i32>),
    /// A protocol problem or a failed request. The session may still work.
    Error(String),
}

/// Finds `name` the way a shell would: a name with a path separator is
/// used as given (made absolute against the current directory), otherwise
/// each absolute `PATH` entry is tried. Only executable regular files count.
pub fn find_executable(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        // The agent runs with its root as working directory, so a relative
        // path must be resolved here or a file in the root could run.
        let absolute = std::path::absolute(candidate).ok()?;
        return is_executable(&absolute).then_some(absolute);
    }
    std::env::split_paths(path_var?)
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| {
            let plain = dir.join(name);
            let exe = cfg!(windows).then(|| dir.join(format!("{name}.exe")));
            std::iter::once(plain).chain(exe)
        })
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_distinct() {
        use AgentStatus::*;
        let all = [
            Starting,
            Planning,
            Editing,
            RunningTests,
            AwaitingApproval,
            Blocked,
            ReadyForReview,
            Completed,
            Failed,
            Rejected,
            Integrated,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), all.len());
        assert_eq!(AwaitingApproval.label(), "awaiting approval");
    }

    #[cfg(unix)]
    #[test]
    fn builtin_providers_lists_only_programs_on_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("maestro-agent-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["gemini", "codex"] {
            let p = dir.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Present but not executable: not listed.
        std::fs::write(dir.join("opencode"), "").unwrap();
        let found = builtin_providers_in(Some(dir.as_os_str()));
        let names: Vec<_> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["gemini", "codex"]);
        assert_eq!(found[0].kind, ProviderKind::Acp);
        assert_eq!(found[0].args, ["--acp"]);
        assert_eq!(found[1].kind, ProviderKind::Terminal);
        assert_eq!(found[1].args, [TASK_PLACEHOLDER]);
        assert!(builtin_providers_in(None).is_empty());
        // Relative PATH entries are ignored.
        assert!(builtin_providers_in(Some(OsStr::new("relative/bin"))).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
