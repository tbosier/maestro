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
/// `env` is added to the inherited environment after TERM=xterm-256color and COLORTERM=truecolor, so it wins.
/// `cwd` must exist: spawn fails with NotFound instead of starting the program somewhere else.
pub struct CommandSpec { pub program: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)> }
#[derive(Clone, Copy)] pub struct TermSize { pub rows: u16, pub cols: u16 } // raised to at least 1 row, 2 columns
pub const SCROLLBACK_LINES: usize = 3000;
pub const MAX_QUEUED_INPUT: usize = 16 << 20; // bytes waiting for a program that is not reading

pub struct Terminal; // Send + Sync, cheap to share behind Arc
impl Terminal {
    /// Spawns `spec` in a new PTY. `on_output` runs on the reader thread after each chunk is parsed,
    /// and once more after the program exits. No new call starts after drop returns (one already starting may finish).
    pub fn spawn(spec: &CommandSpec, size: TermSize, on_output: impl Fn() + Send + Sync + 'static) -> io::Result<Terminal>;
    pub fn write(&self, bytes: &[u8]) -> io::Result<()>; // queued for a writer thread, never blocks; BrokenPipe after exit, WouldBlock past MAX_QUEUED_INPUT
    pub fn resize(&self, size: TermSize) -> io::Result<()>; // PTY and parser; a no-op when unchanged
    pub fn snapshot(&self) -> Screen;          // the visible screen, or a scrolled-back view
    pub fn scroll(&self, lines: isize);        // positive moves back into scrollback, negative toward live; 0 returns to the live screen
    pub fn exit_status(&self) -> Option<ExitInfo>; // None while running
    pub fn kill(&self) -> io::Result<()>;      // returns at once; the waiter thread sends SIGHUP, then SIGKILL after 200 ms
    pub fn application_cursor(&self) -> bool; // DECCKM, for key encoding
    pub fn bracketed_paste(&self) -> bool;    // for encode_paste
    pub fn title(&self) -> String;            // OSC 0 or 2, control characters removed, at most 256 chars
}
// Dropping a Terminal kills the program without waiting; the threads end once nothing holds the PTY open.

pub struct Screen { pub rows: u16, pub cols: u16, pub cells: Vec<Cell> /* row-major */, pub cursor: Option<(u16, u16)> /* None when hidden or scrolled back */, pub scrolled: bool }
impl Screen { pub fn cell(&self, row: u16, col: u16) -> Option<&Cell>; pub fn row_text(&self, row: u16) -> String /* trailing blanks trimmed */; }
pub struct Cell { pub text: String /* " " when blank, empty for the right half of a wide char */, pub fg: TermColor, pub bg: TermColor, pub bold: bool, pub dim: bool, pub italic: bool, pub underline: bool, pub inverse: bool, pub wide: bool /* left half only */ }
pub enum TermColor { Default, Indexed(u8), Rgb(u8, u8, u8) }
pub struct ExitInfo { pub code: Option<i32> /* None when a signal ended the program */, pub description: String }

pub enum TermKey { Char(char), Enter, Esc, Backspace, Delete, Tab, BackTab, Left, Right, Up, Down, Home, End, PageUp, PageDown, Insert, F(u8) }
#[derive(Default)] pub struct TermMods { pub ctrl: bool, pub alt: bool, pub shift: bool }
/// xterm encoding of a key press.
pub fn encode_key(key: TermKey, mods: TermMods, application_cursor: bool) -> Vec<u8>;
/// Bracketed paste (with ESC bytes removed from the text) when the program enabled it, else raw text with newlines sent as CR.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8>;
```

Each terminal runs three threads: a reader (PTY to parser), a writer (input queue to PTY) and a waiter that owns the child. The emulator answers cursor position (`CSI 6 n`), device status (`CSI 5 n`) and device attribute (`CSI c`, `CSI > c`) queries itself, because some programs wait seconds for those answers before they draw anything.

Kill and drop signal the program's process group through kill(1): SIGHUP, 200 ms, then SIGKILL. Known gaps: a descendant that left the group (a daemon, `setsid`) is not reached. portable-pty starts the program in `$HOME` if the working directory vanishes between the check and the spawn. The cursor position reply ignores origin mode (DECOM), because vt100 does not expose the scroll region.

## `maestro-agent`

```rust
pub struct AgentId(pub u64);
pub enum AgentStatus { Starting, Planning, Editing, RunningTests, AwaitingApproval, Blocked, ReadyForReview, Completed, Failed, Rejected, Integrated }
impl AgentStatus { pub fn label(self) -> &'static str; }

pub enum ProviderKind { Acp, Terminal }
/// From config. `Terminal` providers get the task text as `{task}` in args, or typed in after start when no arg names it.
pub struct ProviderSpec { pub name: String, pub kind: ProviderKind, pub program: String, pub args: Vec<String>, pub env: Vec<(String, String)> }
pub fn builtin_providers() -> Vec<ProviderSpec>; // only programs found on PATH
/// Built in: `gemini --acp` and `opencode acp` (ACP), `claude {task}` and `codex {task}` (Terminal).
/// Checked against the installed programs' `--help`; `--experimental-acp` is gemini's deprecated spelling.
pub fn builtin_providers_in(path_var: Option<&OsStr>) -> Vec<ProviderSpec>;
pub const TASK_PLACEHOLDER: &str = "{task}";
pub fn find_executable(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf>;

/// Messages and tool activity from an ACP agent, in arrival order.
/// `User`, `Agent` and `Thought` are streamed chunks; the editor's own prompt text is not echoed as `User`.
pub enum TranscriptEntry { User(String), Agent(String), Thought(String), Tool { id: String, title: String, kind: String, status: String, locations: Vec<PathBuf> }, Plan(Vec<(String, String)>), System(String) }

pub struct PermissionRequest { pub request_id: serde_json::Value, pub tool_id: String, pub tool_title: String, pub options: Vec<PermissionChoice> }
pub struct PermissionChoice { pub id: String, pub label: String, pub kind: String /* allow_once, allow_always, reject_once, reject_always */ }

pub enum AcpEvent {
    Initialized { agent_name: Option<String>, load_session: bool },
    SessionStarted { session_id: String },
    /// After `load_session`, once `session/load` was answered.
    SessionLoaded { session_id: String },
    Update(TranscriptEntry),
    /// A change to an earlier `Tool` entry; `None` fields are unchanged.
    ToolUpdate { id: String, status: Option<String>, title: Option<String>, locations: Option<Vec<PathBuf>> },
    PermissionRequested(PermissionRequest),
    /// The agent's stop reason, or `error` when the prompt failed or the session could not start.
    TurnEnded { stop_reason: String },
    FileWritten(PathBuf),
    Stderr(String),
    /// Always the last event.
    Exited(Option<i32>),
    Error(String),
}

pub struct AcpClient;
impl AcpClient {
    /// Starts the agent process (in its own process group on Unix), runs `initialize` and `session/new` with `root` as cwd.
    /// Returns once the process runs; the handshake is reported as `Initialized` and `SessionStarted`.
    /// File reads and writes the agent asks the client for are confined to `root`:
    /// paths outside it, symlinks out of it and the `.git` directory are refused.
    /// `on_event` runs on a dispatcher thread, one event at a time, and may call back into the client.
    pub fn start(spec: &ProviderSpec, root: &Path, on_event: impl Fn(AcpEvent) + Send + Sync + 'static) -> io::Result<AcpClient>;
    /// Like `start`, but resumes `session_id` with `session/load` when the agent advertises `loadSession`
    /// (replayed updates arrive as `Update`, then `SessionLoaded`). Without the capability, or when the
    /// load fails (reported as `Error`), a new session starts and `SessionStarted` is reported.
    pub fn load_session(spec: &ProviderSpec, root: &Path, session_id: &str, on_event: impl Fn(AcpEvent) + Send + Sync + 'static) -> io::Result<AcpClient>;
    /// `start` and `load_session` with extra rules for the agent's requests (added for task contracts).
    pub fn start_with_rules(spec: &ProviderSpec, root: &Path, rules: ClientRules, on_event: impl Fn(AcpEvent) + Send + Sync + 'static) -> io::Result<AcpClient>;
    pub fn load_session_with_rules(spec: &ProviderSpec, root: &Path, session_id: &str, rules: ClientRules, on_event: impl Fn(AcpEvent) + Send + Sync + 'static) -> io::Result<AcpClient>;
    /// Fails when a turn is already running or the agent is gone. Before `SessionStarted` it is queued.
    pub fn prompt(&self, text: &str) -> io::Result<()>;
    /// Sends `session/cancel` and answers pending permission requests as cancelled.
    pub fn cancel(&self) -> io::Result<()>;
    /// Fails, sending nothing, unless the request is pending and the choice was offered.
    pub fn answer_permission(&self, request_id: serde_json::Value, choice: Option<&str> /* None = cancelled */) -> io::Result<()>;
    /// Kills the agent's process group. Dropping the client does the same.
    pub fn shutdown(&self);
    pub fn root(&self) -> &Path; // canonical
    pub fn session_id(&self) -> Option<String>;
}

/// The confinement itself, usable on its own. Paths must be absolute.
pub struct FileAccess;
impl FileAccess {
    pub fn new(root: &Path) -> io::Result<FileAccess>;
    pub fn root(&self) -> &Path;
    /// On Unix, refuses a file with more than one hard link and a file that changed between the check and the open.
    pub fn read(&self, path: &Path, line: Option<u32> /* 1-based */, limit: Option<u32>) -> Result<String, AccessError>;
    /// Temp file in the same directory, then rename. Creates missing parent directories inside the root.
    pub fn write(&self, path: &Path, content: &str) -> Result<PathBuf, AccessError>;
    /// `write`, after `allow` agrees to the resolved path relative to the root (`Refused` otherwise).
    pub fn write_with(&self, path: &Path, content: &str, allow: &dyn Fn(&Path) -> Result<(), String>) -> Result<PathBuf, AccessError>;
}
/// Extra rules for an agent's requests: `write` is applied to every `fs/write_text_file` after the confinement.
pub type WriteRule = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;
#[derive(Clone, Default)] pub struct ClientRules { pub write: Option<WriteRule> }
pub enum AccessError { OutsideRoot(PathBuf), GitDir(PathBuf), NotRegularFile(PathBuf), NotFound(PathBuf), NotAbsolute(PathBuf), BadPath(PathBuf), Refused(String), Io(io::Error) }

// Behind the `fake-agent` feature: `maestro_agent::fake` (a scriptable fake ACP agent,
// command language documented in its source) and the `fake-acp-agent` binary.
```

The file confinement checks paths and then acts on them. It refuses what the agent asks the client to do; it does not sandbox the agent. A process changing the tree at the same moment (swapping a checked directory for a symlink between the check and the rename) can still redirect a write, and the agent's own tools (a shell, its own file writes) are not affected by it at all. Workstreams that need a real boundary need an OS sandbox around the agent process.

The editor keeps the rest of an agent's record (task text, worktree, history, test runs, files changed) in its own state; `maestro-agent` holds only what the protocol needs.

## `maestro-tasks`

```rust
pub struct TaskSpec { pub program: String, pub args: Vec<String>, pub cwd: PathBuf }
/// Output lines (stdout and stderr, in arrival order), then exactly one Finished.
/// `code` is None when a signal ended the program.
pub enum TaskEvent { Output(String), Finished { code: Option<i32>, duration: Duration } }
/// Longer output lines arrive in pieces of at most this many bytes of raw output, cut on char
/// boundaries. Invalid UTF-8 becomes U+FFFD, so a piece can be longer as text.
pub const MAX_LINE: usize = 64 * 1024;
/// Dropping the handle does not stop the task.
pub struct TaskHandle; impl TaskHandle {
    /// SIGKILL to the task's process group (Unix), then to the child. No-op once finished.
    pub fn kill(&self);
    pub fn id(&self) -> u32;
}
/// stdin is null; on Unix the child leads a new process group. `on_event` runs on a thread of the task's own.
pub fn start(spec: &TaskSpec, on_event: impl Fn(TaskEvent) + Send + 'static) -> io::Result<TaskHandle>;
pub struct TestSummary { pub passed: u64, pub failed: u64, pub ignored: u64, pub failures: Vec<String> }
/// Sums every `test result:` line of `cargo test` output. None if there is none.
/// Failure names come from `test x ... FAILED`, `x --- FAILED` (-q) and the closing `failures:` list.
pub fn parse_cargo_test(output: &str) -> Option<TestSummary>;
```

Killing a process group needs `killpg`, which the standard library does not offer safely, and the workspace forbids `unsafe`. `TaskHandle::kill` runs `kill -KILL -- -<pgid>` as a program, only while the child is still unreaped (so the id cannot have been reused), and falls back to killing the direct child. Lines go through a queue of 1,024; when `on_event` is slower than the output, the program blocks on its writes instead of the queue growing. After the program exits, `Finished` waits for its output pipes to close, but no longer than half a second with no new line and five seconds in all, so a background process that keeps them open does not hold it back; lines already queued are delivered first. The reader threads of such a task stay blocked until that process closes the pipes.

## `maestro-git` additions

```rust
impl Repository {
    /// Everything a worktree changed since it forked from `base` (the merge base of `base` and HEAD):
    /// commits on its branch, staged, unstaged and untracked files (as added; ignored files skipped),
    /// with rename detection. Untracked files get intent-to-add entries in a temporary copy of the
    /// index, written as a whole (not split) index; the real index and its shared index files are
    /// not touched. The only object it can write is the empty blob.
    pub fn review_diff(&self, base: &str) -> Result<Vec<DiffFile>>;
    /// Stages everything (untracked included) and commits it. Returns the new commit, or None when there was nothing to commit.
    /// Refuses with GitError::Unmerged while files are in conflict, and with GitError::NoIdentity
    /// (before staging) when git has no user name and email. Hooks run; if one rejects the commit, files stay staged.
    pub fn commit_all(&self, message: &str) -> Result<Option<String>>;
    /// Takes one file out of the work under review: `old_path` is restored to its version at the fork point
    /// of `base` and HEAD (index and working tree), and `new_path`, when it differs, is removed. Paths are
    /// relative to the worktree; absolute paths, `..` and a directory at `new_path` are refused.
    pub fn drop_change(&self, base: &str, old_path: Option<&Path>, new_path: Option<&Path>) -> Result<()>;
    /// Integrates the local branch `branch` into the checked-out branch of this (target) worktree.
    /// Refuses, changing nothing, when this worktree is detached, mid-operation or has staged,
    /// unstaged or conflicted files (untracked files do not block), when the worktree that has
    /// `branch` checked out has uncommitted or untracked files, when there is nothing to integrate,
    /// when fast-forward only cannot fast-forward, or when the merge-tree dry run reports conflicts.
    /// The target is checked again just before git runs. `branch.<target>.mergeOptions` is ignored
    /// and `--commit --no-squash` passed, so user defaults cannot leave a merge open.
    /// If git fails part way, the merge or cherry-pick is aborted (only if this call started it),
    /// then HEAD, open operations and changed files are checked. After a success, the result is
    /// checked too (nothing open, same branch, HEAD moved forward).
    pub fn integrate(&self, branch: &str, how: Integration) -> Result<IntegrateOutcome, IntegrateError>;
    /// Added for conflict detection: the worktree's files (HEAD plus staged, unstaged and untracked,
    /// not ignored) as an unreferenced commit with HEAD as parent, built in a temporary index copy
    /// that keeps the real index's modification time (so Git's racy-file check still works).
    /// Fixed author and dates: the same files on the same HEAD give the same commit id. Refused if HEAD moves meanwhile.
    pub fn work_snapshot(&self) -> Result<WorkSnapshot>;
    /// An unreferenced commit of `tree` with `parents`, fixed author and dates. For dry runs.
    pub fn commit_tree(&self, tree: &str, parents: &[&str], message: &str) -> Result<String>;
    /// Every path any commit in `from..to` changes (renames as both paths, merges against each parent).
    pub fn paths_touched(&self, from: &str, to: &str) -> Result<Vec<PathBuf>>;
}
pub struct WorkSnapshot { pub commit: String, pub tree: String }
/// Merge always creates a merge commit (--no-ff). CherryPick copies the branch's commits not on
/// the target, oldest first, skipping ones whose changes the target already has.
pub enum Integration { FastForwardOnly, Merge, CherryPick }
/// `commits`: commits now on the target that were not before, oldest first.
pub struct IntegrateOutcome { pub new_head: String, pub commits: Vec<String> }
pub enum IntegrateError {
    Dirty(Vec<PathBuf>), SourceDirty(Vec<PathBuf>), Conflicts(Vec<PathBuf>), NotFastForward, NothingToIntegrate,
    DetachedHead, InProgress(&'static str), MergeCommits(Vec<String>) /* cherry-pick refuses merges */,
    Git(GitError) /* git failed; any operation was aborted */,
    AbortFailed { cause: GitError, detail: String } /* git failed and the worktree could not be restored */,
    TargetChanged /* the target moved during the checks; nothing was integrated */,
    Unexpected(String) /* git reported success but the result is wrong; says what was done */,
}
/// Cherry-pick dry run needs merge-tree with trees (git 2.45+); older git gets GitError::Unsupported.
pub const MERGE_TREE_TREES: GitVersion;
/// GitError gains NoIdentity(String) and Unmerged(Vec<PathBuf>).
///
/// Changed byte ranges, on char boundaries, for a removed line and the added line paired with it.
/// Tokens are word runs, whitespace runs and single other chars. Lines over INTRALINE_MAX_LINE bytes,
/// or with under 40% matching non-whitespace text, are marked whole.
pub fn intraline(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>);
pub const INTRALINE_MAX_LINE: usize = 1024;
```

Archiving is the existing `remove_worktree`, which refuses on uncommitted or unmerged work unless forced; the UI asks before forcing.

## Editor side (built on `feat/panel`)

The glue lives in `crates/maestro-editor/src/agents/` (state, creation, panel keys, review rows, actions) and `crates/maestro-tui/src/agents.rs` (drawing). `docs/agent-model.md` describes the behavior; this records how it uses the crates above.

- The `Editor` gained one field, `agents: AgentState`, and small hooks: key dispatch and paste go to the panel while it is open (after pickers, prompts and pending-character requests), the mouse is ignored then, `poll_background` calls `agents::poll`, and `tick` calls `agents::tick`. The renderer draws the panel over the editor area and adds `agent <name> worktree` to the status line of a buffer inside an agent's worktree.
- ACP events and test output arrive on other threads and are forwarded as jobs through `idle_job_sender`. Terminal output bumps a per-agent counter and calls `wake_handle`; `agents::poll` compares the counter and reports a change only for the terminal on screen, so the front end redraws without a change to its loop. `poll` also notices terminal exits and resizes every agent PTY to the panel's content area.
- Git work runs on `spawn_background` threads: creating the worktree (`add_worktree` from the current `HEAD` of the repository the editor's working directory is in, under `[agents] worktree-dir` or `<repo>-worktrees` next to the main worktree), review diffs (`review_diff` against the base commit), the integration check (the main worktree's branch and status), `commit_all` plus `integrate`, and `remove_worktree`.
- The editor keeps its own status type. `AgentStatus` has no state for an ACP turn that has not reported a plan or edit yet, or for a running terminal program, so the editor adds `Working` and `Running`. `maestro-agent` was not changed.
- One full-screen agent panel (`space a`, `:agents`) over the editor area: the agent list on the left, the selected agent on the right (its terminal, or its ACP transcript and pending permission; `v` switches to its history or test output). Keys go to the panel while it is open; `C-\` hands keys to the selected agent's terminal and `C-\` again takes them back. Terminals send `C-\` as the byte 0x1c, which crossterm reports as `C-4`, so both are accepted. Terminals are not shown in splits in this milestone.
- A review screen per agent: file list with change kind and counts, unified or side-by-side diff with `intraline` marks and the theme's diff colors, comments stored per line on the agent record, `Enter` opens the worktree file in a normal buffer at that line.
- Actions with consequences (integrate, reject and archive, force-removing a worktree, granting a permission) open a dialog that says what will happen and needs a key to go ahead (`y`, or `F` for forcing). The backend checks again; the dialog is not the enforcement. Integration also refuses, changing nothing, when either worktree is gone or on another branch than the dialog named, the main worktree is dirty or detached, the agent's starting commit is not on the target branch, or a buffer in either worktree has unsaved edits; these are checked when the dialog opens and again just before the commit. A dialog that does not fit the window cannot be accepted.
- Feedback goes back to the agent that made the change: an ACP agent gets a new prompt; a terminal agent gets the text typed into its terminal, which the UI says is all it can do.
- Agent records are saved by `agents/persist.rs` per repository (keyed by the canonical main worktree path) to `<state>/agents/<hash>.json` with `navigation::session::write_private`, only when the navigation state has a state directory and sessions on. `agents::tick` saves when a cheap fingerprint of the records changed, and quitting (`finish_quit`, `stop_all`) saves with sync. The panel's first open starts a background scan (`git worktree list`, the main worktree's `HEAD`, the records file); its result adds loaded agents (`Origin::Loaded`, no runtime, active statuses mapped to the new `Status::Stopped`), notes worktrees that changed, and adds found worktrees (`Origin::Found`). Creating an agent waits for the scan. An unusable file blocks saving for that repository for the session.
- `R` relaunches through `create::relaunch`: `AcpClient::load_session` when the record has a session id and the agent advertised `loadSession`, else `AcpClient::start`, with no prompt (status `Idle` once the session is up); a terminal agent's spec without the arguments that held `{task}`. The editor gained `Status::Idle` and `Status::Stopped`, `Origin`, and the agent fields `origin`, `session_id`, `load_session` and `worktree_note`. `maestro-agent` gained `AcpClient::load_session`, `AcpEvent::SessionLoaded` and the fake agent's `--fake-load-session`.
- Archive checks, on its background thread, that Git lists the worktree on the recorded branch before removing it, and marks the record archived without removing anything when the directory is gone and Git no longer lists it. The review diff is computed only while the worktree is on the recorded branch.
- Task contracts (`agents/contract.rs`, added November 1, 2026): `Agent::contract` holds a `Contract` (allowed and forbidden patterns, verification command, acceptance criteria, source file), set from `NewAgent::contract` and saved in the record. The contract check for ACP writes is a closure the editor passes to `maestro-agent` as `ClientRules::write` (`AcpClient::start_with_rules`, `load_session_with_rules`). The check has to run on the client's reader thread, which answers `fs/write_text_file` synchronously; routing every write through the editor thread and back would add a round trip and a new failure mode to the client. A rule closure keeps `maestro-agent` free of any notion of contracts or globs: it only learns "a caller-supplied predicate on the resolved relative path", applied through `FileAccess::write_with` after the existing confinement. The integrate check builds the out-of-contract list from a fresh `review_diff` plus `paths_touched` over the branch, and records the snapshot tree it judged; `run_integration` after `commit_all` requires the same tree and checks the list again, and after a successful integrate compares the target before and after against the contract. `ConfirmAction::Integrate` accepts `O` instead of `y` when that list is not empty. `t` prefers `contract.verify`; its background thread takes a `work_snapshot` before starting the command and records `TestRun::tree`, compared with a fresh snapshot in the integrate check. `Agent::stop` sets a flag that keeps a run still being started from running.
- Conflicts between agents (`agents/conflicts.rs`): `AgentState::conflicts` holds the overlapping pairs. `review::applied` calls `conflicts::refresh` when an agent's diff changed, and reject, integrate and archive call it too. Overlap is computed in memory from the review diffs; one background job at a time snapshots the agents in overlapping pairs and runs `merge_check` per pair, cached by repository and the ordered pair of snapshot commit ids (at most 256 entries). Overlap also counts a path that is a directory of the other agent's path. `ReviewData::head` records the worktree's `HEAD`, and a change to it, or a binary file in the diff, triggers a recomputation. The integrate check merges the target's `HEAD` with the agent's snapshot in memory (`merge_check`, then `commit_tree`) and dry-runs that against each overlapping agent's snapshot. The agent panel's summary grew from four rows to six (`panel::agent_layout`); `panel::layout` keeps four rows for the proposal review.
