# 0006: Agent runtime, terminals and review (M2 interfaces)

Date: October 30, 2026. Status: accepted.

This records the crate boundaries and public interfaces for M2 before parallel work starts. Names here are the contract between workstreams; a workstream that needs to change one says so in its report instead of changing it silently.

## Crates

| Crate | Owns | Depends on |
|---|---|---|
| `maestro-term` | Pseudo-terminals and terminal emulation: spawn a program in a PTY, feed its output through a VT parser, expose a screen snapshot, encode keys, resize, kill | `portable-pty`, `vt100` |
| `maestro-agent` | Agent records and status, provider configuration, the Agent Client Protocol (ACP) client, file access rules for agents | `agent-client-protocol-schema` for message types, `serde_json` |
| `maestro-tasks` | Running a verification command (tests, build) in a directory, capturing output and exit status, summarizing `cargo test` output | std only |
| `maestro-git` (extended) | The review diff of a worktree against its base (committed, uncommitted and untracked), intraline ranges, committing an agent's work, integrating a branch safely, archiving a worktree | unchanged |

None of these depend on `maestro-editor` or `maestro-tui`. The editor owns the glue: it starts agents, holds their terminals, turns their events into jobs with `JobSender`, and draws them.

## Threads, not async

The ACP client works like the LSP client (`maestro-lsp`): a child process, one reader thread, one writer, JSON-RPC 2.0 framed as newline-delimited JSON on stdio, and callbacks from the reader thread. The official Rust SDK (`agent-client-protocol`) is async and expects a local executor. Bringing an async runtime into an editor whose every other subsystem is threads and channels would cost more than the framing it saves. The SDK's schema crate (`agent-client-protocol-schema`) gives us the message types, so we follow the published protocol without hand-copying it. This is the only reason the workspace `rust-version` moved from 1.85 to 1.88.

Every producer that changes state off the main thread wakes the event loop through `Editor::wake_handle()` or a `JobSender` (which wakes on send).

## `maestro-term`

```rust
pub struct CommandSpec { pub program: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)> }
#[derive(Clone, Copy)] pub struct TermSize { pub rows: u16, pub cols: u16 }

pub struct Terminal; // Send + Sync, cheap to share behind Arc
impl Terminal {
    /// Spawns `spec` in a new PTY. `on_output` runs on the reader thread after each chunk is parsed.
    pub fn spawn(spec: &CommandSpec, size: TermSize, on_output: impl Fn() + Send + Sync + 'static) -> io::Result<Terminal>;
    pub fn write(&self, bytes: &[u8]) -> io::Result<()>;
    pub fn resize(&self, size: TermSize) -> io::Result<()>;
    pub fn snapshot(&self) -> Screen;          // the visible screen, or a scrolled-back view
    pub fn scroll(&self, lines: isize);        // into scrollback; 0 returns to the live screen
    pub fn exit_status(&self) -> Option<ExitInfo>; // None while running
    pub fn kill(&self) -> io::Result<()>;
    pub fn application_cursor(&self) -> bool; // DECCKM, for key encoding
    pub fn title(&self) -> String;
}

pub struct Screen { pub rows: u16, pub cols: u16, pub cells: Vec<Cell> /* row-major */, pub cursor: Option<(u16, u16)>, pub scrolled: bool }
pub struct Cell { pub text: String /* empty for the right half of a wide char */, pub fg: TermColor, pub bg: TermColor, pub bold: bool, pub italic: bool, pub underline: bool, pub inverse: bool, pub wide: bool }
pub enum TermColor { Default, Indexed(u8), Rgb(u8, u8, u8) }
pub struct ExitInfo { pub code: Option<i32>, pub description: String }

pub enum TermKey { Char(char), Enter, Esc, Backspace, Delete, Tab, BackTab, Left, Right, Up, Down, Home, End, PageUp, PageDown, Insert, F(u8) }
pub struct TermMods { pub ctrl: bool, pub alt: bool, pub shift: bool }
/// xterm encoding of a key press.
pub fn encode_key(key: TermKey, mods: TermMods, application_cursor: bool) -> Vec<u8>;
/// Bracketed paste when the program enabled it, else raw text.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8>;
```

## `maestro-agent`

```rust
pub struct AgentId(pub u64);
pub enum AgentStatus { Starting, Planning, Editing, RunningTests, AwaitingApproval, Blocked, ReadyForReview, Completed, Failed, Rejected, Integrated }
impl AgentStatus { pub fn label(self) -> &'static str; }

pub enum ProviderKind { Acp, Terminal }
/// From config. `Terminal` providers get the task text as `{task}` in args, or typed in after start when no arg names it.
pub struct ProviderSpec { pub name: String, pub kind: ProviderKind, pub program: String, pub args: Vec<String>, pub env: Vec<(String, String)> }
pub fn builtin_providers() -> Vec<ProviderSpec>; // only programs found on PATH

/// Messages and tool activity from an ACP agent, in arrival order.
pub enum TranscriptEntry { User(String), Agent(String), Thought(String), Tool { id: String, title: String, kind: String, status: String, locations: Vec<PathBuf> }, Plan(Vec<(String, String)>), System(String) }

pub struct PermissionRequest { pub request_id: serde_json::Value, pub tool_title: String, pub options: Vec<PermissionChoice> }
pub struct PermissionChoice { pub id: String, pub label: String, pub kind: String /* allow_once, allow_always, reject_once, reject_always */ }

pub enum AcpEvent { Initialized { agent_name: Option<String>, load_session: bool }, SessionStarted { session_id: String }, Update(TranscriptEntry), ToolUpdate { id: String, status: String }, PermissionRequested(PermissionRequest), TurnEnded { stop_reason: String }, FileWritten(PathBuf), Stderr(String), Exited(Option<i32>), Error(String) }

pub struct AcpClient;
impl AcpClient {
    /// Starts the agent process, runs `initialize` and `session/new` with `root` as cwd.
    /// File reads and writes the agent asks the client for are confined to `root`:
    /// paths outside it, symlinks out of it and the `.git` directory are refused.
    pub fn start(spec: &ProviderSpec, root: &Path, on_event: impl Fn(AcpEvent) + Send + Sync + 'static) -> io::Result<AcpClient>;
    pub fn prompt(&self, text: &str) -> io::Result<()>;
    pub fn cancel(&self) -> io::Result<()>;
    pub fn answer_permission(&self, request_id: serde_json::Value, choice: Option<&str> /* None = cancelled */) -> io::Result<()>;
    pub fn shutdown(&self);
}
```

The editor keeps the rest of an agent's record (task text, worktree, history, test runs, files changed) in its own state; `maestro-agent` holds only what the protocol needs.

## `maestro-tasks`

```rust
pub struct TaskSpec { pub program: String, pub args: Vec<String>, pub cwd: PathBuf }
pub enum TaskEvent { Output(String), Finished { code: Option<i32>, duration: Duration } }
pub struct TaskHandle; impl TaskHandle { pub fn kill(&self); pub fn id(&self) -> u32; }
pub fn start(spec: &TaskSpec, on_event: impl Fn(TaskEvent) + Send + 'static) -> io::Result<TaskHandle>;
pub struct TestSummary { pub passed: u64, pub failed: u64, pub ignored: u64, pub failures: Vec<String> }
/// Sums every `test result:` line of `cargo test` output. None if there is none.
pub fn parse_cargo_test(output: &str) -> Option<TestSummary>;
```

## `maestro-git` additions

```rust
impl Repository {
    /// Everything a worktree changed since `base`: commits on its branch, staged, unstaged and untracked files.
    pub fn review_diff(&self, base: &str) -> Result<Vec<DiffFile>>;
    /// Stages everything (untracked included) and commits it. Returns the new commit, or None when there was nothing to commit.
    pub fn commit_all(&self, message: &str) -> Result<Option<String>>;
    /// Integrates `branch` into the checked-out branch of this (target) worktree. Refuses, changing nothing, when this worktree has uncommitted changes, when `branch` has uncommitted changes in its own worktree, or when the merge dry run reports conflicts.
    pub fn integrate(&self, branch: &str, how: Integration) -> Result<IntegrateOutcome, IntegrateError>;
}
pub enum Integration { FastForwardOnly, Merge, CherryPick }
pub struct IntegrateOutcome { pub new_head: String, pub commits: Vec<String> }
pub enum IntegrateError { Dirty(Vec<PathBuf>), SourceDirty(Vec<PathBuf>), Conflicts(Vec<PathBuf>), NotFastForward, NothingToIntegrate, Git(GitError) }
/// Changed character ranges for a removed line and the added line paired with it.
pub fn intraline(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>);
```

Archiving is the existing `remove_worktree`, which refuses on uncommitted or unmerged work unless forced; the UI asks before forcing.

## Editor side (built after the crates)

- One full-screen agent panel (`space a`) over the editor area: the agent list on the left, the selected agent on the right (its terminal, or its ACP transcript and pending permission). Keys go to the panel while it is open; `C-\` hands keys to an agent's terminal and `C-\` again takes them back. Terminals are not shown in splits in this milestone.
- A review screen per agent: file list, unified or side-by-side diff with intraline marks and Duskfox diff colors, comments stored per line, `Enter` opens the worktree file in a normal buffer at that line.
- Actions with consequences (integrate, reject and archive, force-removing a worktree, granting a permission) need an explicit confirmation key in the panel. The backend checks again; the dialog is not the enforcement.
- Feedback goes back to the agent that made the change: an ACP agent gets a new prompt; a terminal agent gets the text typed into its terminal, which the UI says is all it can do.
