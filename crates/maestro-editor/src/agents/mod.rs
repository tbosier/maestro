//! Coding agents in the editor: their records, the agent panel and the
//! review screen. See `docs/agent-model.md` and ADR 0006.
//!
//! An agent is a task, a provider, a Git worktree on a branch of its own,
//! and a runtime: an [`AcpClient`] for providers that speak the Agent Client
//! Protocol, or a [`Terminal`] for any other command-line agent. Everything
//! the record says about the agent comes from a real signal: ACP events,
//! the terminal program's exit, Maestro's own test runs and the user's
//! review decisions. A terminal agent's activity cannot be seen, so its
//! status is only [`Status::Running`] until its program exits.
//!
//! Events arrive on other threads. ACP events and test output are
//! forwarded as jobs through [`Editor::idle_job_sender`]; terminal output
//! only bumps a counter and wakes the loop through [`Editor::wake_handle`],
//! and [`poll`] (run from [`Editor::poll_background`]) notices it. Git work
//! (creating worktrees, review diffs, integration) runs on background
//! threads started with [`Editor::spawn_background`].

pub mod actions;
pub mod compose;
pub mod conflicts;
pub mod contract;
pub mod create;
pub mod marks;
pub mod panel;
pub mod parts;
pub mod persist;
pub mod rebase;
pub mod review;
pub mod test_outputs;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

pub use maestro_agent::{
    AcpClient, AgentId, PermissionChoice, PermissionRequest, ProviderKind, ProviderSpec,
    TranscriptEntry,
};
pub use maestro_git::{DiffFile, DiffLine, DiffLineKind, FileChange, Hunk, Integration};
pub use maestro_tasks::TestSummary;
pub use maestro_term::{Cell as TermCell, Screen, TermColor, TermSize, Terminal};

use crate::editor::Editor;

crate::commands! {
    agent_panel => "Show or hide the agent panel",
}

/// Default key bindings, added to the `space` group.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[("space a", "agent_panel")];

/// Where an agent is. Set only from real signals (see the module docs).
///
/// This is the editor's own status type. `maestro_agent::AgentStatus` has
/// no state for "a turn is running and the agent has not said what it is
/// doing" ([`Status::Working`]) or for a terminal program that is running
/// ([`Status::Running`]), so the editor keeps its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    /// The worktree is being created or the agent process is starting.
    Starting,
    /// An ACP turn is running; the agent has sent no plan or edit yet.
    Working,
    /// The ACP agent sent a plan during the current turn.
    Planning,
    /// The ACP agent reported an edit, delete or move tool call (in any
    /// state) or wrote a file through Maestro during the turn.
    Editing,
    /// The ACP agent asked for permission and waits for an answer.
    AwaitingApproval,
    /// A terminal agent's program is running. Maestro cannot see what it is
    /// doing.
    Running,
    /// Maestro's verification command is running in the worktree.
    RunningTests,
    /// The ACP turn ended normally, or the terminal program exited with
    /// code 0.
    ReadyForReview,
    /// Finished like `ReadyForReview`, but the worktree has no changes
    /// against its starting commit. Goes back to `ReadyForReview` if a
    /// later review diff finds changes.
    Completed,
    /// The ACP turn ended without finishing (refused, cancelled, out of
    /// tokens or turns).
    Blocked,
    /// The agent could not start, its turn failed, or it exited with an
    /// error.
    Failed,
    /// The user rejected the work.
    Rejected,
    /// The work was integrated into the main worktree's branch.
    Integrated,
    /// A relaunched ACP agent's session is up and no turn runs: it waits
    /// for a prompt (`f`).
    Idle,
    /// No process runs for the agent and Maestro did not see it finish: a
    /// record from an earlier session whose agent was still working when it
    /// was saved, an adopted found worktree, or an idle ACP agent whose
    /// process exited.
    Stopped,
}

impl Status {
    /// The word for this status. Every view uses it through
    /// [`state_word`]: the panel, the sidebar, the jump palette and
    /// notifications (see "State words" in `docs/agent-model.md`).
    pub fn label(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Working => "working",
            Status::Planning => "planning",
            Status::Editing => "editing",
            Status::AwaitingApproval => "awaiting approval",
            Status::Running => "running",
            Status::RunningTests => "running tests",
            Status::ReadyForReview => "ready for review",
            Status::Completed => "finished, no changes",
            Status::Blocked => "blocked",
            Status::Failed => "failed",
            Status::Rejected => "rejected",
            Status::Integrated => "integrated",
            Status::Idle => "waiting for a prompt",
            Status::Stopped => "stopped",
        }
    }

    /// The stable name saved in agent records.
    pub fn key(self) -> &'static str {
        match self {
            Status::Starting => "starting",
            Status::Working => "working",
            Status::Planning => "planning",
            Status::Editing => "editing",
            Status::AwaitingApproval => "awaiting-approval",
            Status::Running => "running",
            Status::RunningTests => "running-tests",
            Status::ReadyForReview => "ready-for-review",
            Status::Completed => "completed",
            Status::Blocked => "blocked",
            Status::Failed => "failed",
            Status::Rejected => "rejected",
            Status::Integrated => "integrated",
            Status::Idle => "idle",
            Status::Stopped => "stopped",
        }
    }

    pub fn from_key(key: &str) -> Option<Status> {
        use Status::*;
        [
            Starting,
            Working,
            Planning,
            Editing,
            AwaitingApproval,
            Running,
            RunningTests,
            ReadyForReview,
            Completed,
            Blocked,
            Failed,
            Rejected,
            Integrated,
            Idle,
            Stopped,
        ]
        .into_iter()
        .find(|s| s.key() == key)
    }

    /// The status says a process is doing something. A record loaded from
    /// an earlier session never shows one of these.
    pub fn needs_process(self) -> bool {
        matches!(
            self,
            Status::Starting
                | Status::Working
                | Status::Planning
                | Status::Editing
                | Status::AwaitingApproval
                | Status::Running
                | Status::RunningTests
                | Status::Idle
        )
    }

    /// The user decided; agent events no longer change the status.
    pub fn is_decided(self) -> bool {
        matches!(self, Status::Rejected | Status::Integrated)
    }
}

/// The word shown for an agent's state, the same in the agent panel, the
/// sidebar, the jump palette and notifications: `archived` once the
/// worktree was removed (whatever the status was), `found worktree` for a
/// worktree with no record, else [`Status::label`].
pub fn state_word(status: Status, archived: bool, origin: Origin) -> &'static str {
    if archived {
        ARCHIVED
    } else if origin == Origin::Found {
        FOUND
    } else {
        status.label()
    }
}

/// The state word of an archived agent.
pub const ARCHIVED: &str = "archived";
/// The state word of a found worktree.
pub const FOUND: &str = "found worktree";

/// Where an agent's record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Origin {
    /// Started (or relaunched) in this session.
    #[default]
    New,
    /// Loaded from the saved records of an earlier session, or adopted
    /// from a found worktree. No process runs for it until it is
    /// relaunched.
    Loaded,
    /// A worktree on a `maestro/` branch under the agents' worktree
    /// directory that no record names. Its task and provider are unknown;
    /// it can be reviewed, archived or adopted, nothing else. Not saved.
    Found,
    /// A result composed from hunks of several agents of one task, in a
    /// worktree of its own. No agent runs in it: it is reviewed, tested,
    /// integrated, rejected or archived like an agent's work.
    Composed,
}

/// The worktree an agent works in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Canonical path.
    pub path: PathBuf,
    pub branch: String,
    /// The commit the branch started from.
    pub base: String,
    /// The repository's main worktree, where work is integrated.
    pub main_root: PathBuf,
}

/// How the editor runs the agent.
pub enum Runtime {
    /// Nothing runs: not started yet, failed to start, or stopped.
    None,
    Acp(AcpClient),
    Terminal(Arc<Terminal>),
}

impl Runtime {
    pub fn terminal(&self) -> Option<&Arc<Terminal>> {
        match self {
            Runtime::Terminal(t) => Some(t),
            _ => None,
        }
    }

    pub fn acp(&self) -> Option<&AcpClient> {
        match self {
            Runtime::Acp(c) => Some(c),
            _ => None,
        }
    }
}

/// One timestamped event in an agent's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub at: SystemTime,
    pub text: String,
}

/// A review comment on one line of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Relative to the worktree root.
    pub path: PathBuf,
    /// One-based line number in the new file, or in the old file for a
    /// removed line (`old` is then true).
    pub line: usize,
    pub old: bool,
    pub text: String,
    /// Sent to the agent with the next message (`m`).
    pub sent: bool,
}

/// One run of the verification command.
#[derive(Debug, Clone)]
pub struct TestRun {
    pub command: String,
    pub started: Instant,
    /// Output lines, oldest first. At most [`MAX_TEST_LINES`]; older lines
    /// are dropped and counted in `dropped`.
    pub output: VecDeque<String>,
    pub dropped: usize,
    pub finished: Option<TestFinish>,
    /// The tree of the worktree's files (tracked and untracked, not
    /// ignored) as they were just before the command started, without
    /// files earlier runs created and with tracked files they changed as
    /// they were before those runs ([`TestOutput`]), from
    /// `maestro_git::Repository::work_snapshot_leaving_out`. Integrate
    /// compares it with the files then, and the run's changes to tracked
    /// files are found against it; `None` until computed, or when it could
    /// not be.
    pub tree: Option<String>,
    /// The agent's process was running when the run started or finished,
    /// so it may have changed files the run did not see.
    pub agent_was_running: bool,
}

#[derive(Debug, Clone)]
pub struct TestFinish {
    /// `None` when a signal ended the command.
    pub code: Option<i32>,
    pub duration: Duration,
    /// Parsed `cargo test` totals, when the output had any.
    pub summary: Option<TestSummary>,
}

impl TestRun {
    /// `passed`, `failed (3 of 40)`, `exit 2`, `running`...
    pub fn describe(&self) -> String {
        let Some(f) = &self.finished else {
            return "tests running".to_string();
        };
        let outcome = match f.code {
            Some(0) => "passed".to_string(),
            Some(c) => format!("failed (exit {c})"),
            None => "killed".to_string(),
        };
        match &f.summary {
            Some(s) => format!(
                "{outcome}: {} passed, {} failed, {} ignored",
                s.passed, s.failed, s.ignored
            ),
            None => outcome,
        }
    }

    /// A short form for the agent list: `ok 40`, `FAIL 3/40`, `exit 2`.
    pub fn short(&self) -> String {
        let Some(f) = &self.finished else {
            return "testing".to_string();
        };
        match (&f.summary, f.code) {
            (Some(s), Some(0)) => format!("ok {}", s.passed),
            (Some(s), _) if s.failed > 0 => {
                format!("FAIL {}/{}", s.failed, s.passed + s.failed)
            }
            (_, Some(0)) => "ok".to_string(),
            (_, Some(c)) => format!("exit {c}"),
            (_, None) => "killed".to_string(),
        }
    }

    pub fn passed(&self) -> Option<bool> {
        self.finished.as_ref().map(|f| f.code == Some(0))
    }
}

/// Output lines kept per test run.
pub const MAX_TEST_LINES: usize = 20_000;
/// Stderr lines kept per ACP agent.
const MAX_STDERR_LINES: usize = 500;
/// Transcript entries kept per ACP agent.
const MAX_TRANSCRIPT: usize = 5_000;

/// A file Maestro's own verification run (`t`) created, or a tracked file
/// it changed, in the agent's worktree, as the run left it. See
/// [`test_outputs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestOutput {
    /// Relative to the worktree root.
    pub path: PathBuf,
    /// Its blob id when the run ended. Once the file's contents differ
    /// from this, it counts as the agent's again.
    pub blob: String,
    /// For a tracked file the run changed: the blob id of its contents just
    /// before the run (the agent's version, or the commit's when the agent
    /// had not changed it), which is what counts as the agent's and is
    /// committed. `None` for a file the run created.
    pub before: Option<String>,
    /// For a tracked file the run changed: the blob id its index entry held
    /// when the run ended. Once the entry holds anything else (it was
    /// staged or committed) but `before`, which Maestro's own commit puts
    /// there, the file counts as the agent's again. `None` for a created
    /// file.
    pub index: Option<String>,
    /// The command that created it, as the run showed it.
    pub command: String,
    /// The agent's process was running during that run, so it may have
    /// written the file itself.
    pub agent_was_running: bool,
}

/// The last review diff of an agent's worktree.
#[derive(Debug, Default)]
pub struct ReviewData {
    /// Every changed file, the agent's first and then the test run's (the
    /// last `test_made.len()` entries).
    pub files: Vec<DiffFile>,
    /// The paths of the test run's entries at the end of `files`, in
    /// order: files Maestro's verification run created, and its changes to
    /// tracked files, that are still as it left them. Shown apart, left out
    /// of the counts, of overlaps, marks, compose and integrate. A tracked
    /// file the agent had changed before the run is also in the agent's
    /// part of `files`, with its contents from before the run.
    pub test_made: Vec<PathBuf>,
    /// The worktree's `HEAD` commit when `files` was computed.
    pub head: Option<String>,
    /// When `files` was computed. `None` before the first diff.
    pub computed: Option<Instant>,
    pub error: Option<String>,
    pub running: bool,
    /// Another refresh was asked for while one ran.
    pub again: bool,
}

impl ReviewData {
    /// The agent's files changed, lines added, lines removed. Files the
    /// verification run created ([`ReviewData::test_made`]) do not count.
    pub fn totals(&self) -> (usize, usize, usize) {
        let (mut n, mut a, mut r) = (0, 0, 0);
        for f in self.agent_files() {
            let (fa, fr) = f.counts();
            n += 1;
            a += fa;
            r += fr;
        }
        (n, a, r)
    }

    /// How many entries of `files`, from the start, are the agent's.
    pub fn agent_count(&self) -> usize {
        self.files.len().saturating_sub(self.test_made.len())
    }

    /// Entry `i` of `files` is Maestro's verification run's (a file it
    /// created, or its change to a tracked file), as it left it.
    pub fn is_test_made(&self, i: usize) -> bool {
        i < self.files.len() && i >= self.agent_count()
    }

    /// The files that count as the agent's work: every entry but the
    /// verification run's.
    pub fn agent_files(&self) -> impl Iterator<Item = &DiffFile> {
        self.files[..self.agent_count()].iter()
    }

    /// `K` on `path`: its entry is no longer the test run's. A created
    /// file's entry joins the agent's files at once; a changed file's entry
    /// (the run's change only) is dropped, and the refresh `K` asks for
    /// shows the whole file as the agent's.
    pub fn release_test_made(&mut self, path: &std::path::Path) {
        let Some(k) = self.test_made.iter().position(|p| p == path) else {
            return;
        };
        let i = self.agent_count() + k;
        self.test_made.remove(k);
        if i >= self.files.len() {
            return;
        }
        let f = self.files.remove(i);
        if f.change == FileChange::Added && f.old_path.is_none() {
            let at = self.agent_count();
            self.files.insert(at, f);
        }
    }
}

/// The paths a diff file names (old and new, once each), relative to the
/// worktree root.
pub fn file_paths(f: &DiffFile) -> Vec<&PathBuf> {
    let mut v: Vec<&PathBuf> = f.old_path.iter().chain(f.new_path.iter()).collect();
    v.dedup();
    v
}

/// Files of `files` that `contract` does not allow, with the reason, in
/// diff order. A rename or deletion counts when either of its paths is
/// outside the contract.
pub fn out_of_scope<'a>(
    contract: &contract::Contract,
    files: impl IntoIterator<Item = &'a DiffFile>,
) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for f in files {
        for p in file_paths(f) {
            if let Some(why) = contract.violation(p) {
                out.push((p.clone(), why));
            }
        }
    }
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// Everything the editor knows about one agent.
pub struct Agent {
    pub id: AgentId,
    pub name: String,
    pub task: String,
    /// The task contract attached when the task was created: what it may
    /// change, its verification command, its acceptance criteria.
    pub contract: Option<contract::Contract>,
    /// Agents started on the same task (competing implementations) share
    /// a group.
    pub group: u64,
    pub provider: ProviderSpec,
    /// `None` until the worktree exists.
    pub worktree: Option<WorktreeInfo>,
    status: Status,
    pub runtime: Runtime,
    /// Merged ACP transcript: consecutive chunks of one message are joined.
    pub transcript: Vec<TranscriptEntry>,
    /// Permission requests not answered yet, oldest first. The panel
    /// answers the first.
    pub permissions: Vec<PermissionRequest>,
    pub history: Vec<HistoryEntry>,
    pub created: SystemTime,
    pub tests: Vec<TestRun>,
    test_handle: Option<maestro_tasks::TaskHandle>,
    /// Set by [`Agent::stop`]: a run whose process is still being started
    /// on a background thread is not started, or is killed on arrival.
    test_cancel: Arc<AtomicBool>,
    pub errors: Vec<String>,
    pub stderr: VecDeque<String>,
    pub review: ReviewData,
    pub comments: Vec<Comment>,
    /// The worktree was removed.
    pub archived: bool,
    /// How the process ended, once it has.
    pub exited: Option<String>,
    /// An ACP turn is running.
    pub turn_running: bool,
    /// Git work for this agent runs in the background: [`CREATING`],
    /// `"integrating"` or `"archiving"`.
    pub git_busy: Option<&'static str>,
    /// Terminal output counter, bumped on the terminal's reader thread.
    output: Arc<AtomicU64>,
    seen_output: u64,
    /// Task text still to be typed into a terminal agent.
    pending_input: Option<String>,
    spawned: Instant,
    term_size: Option<TermSize>,
    /// Where the record came from.
    pub origin: Origin,
    /// The ACP session, once one started.
    pub session_id: Option<String>,
    /// The ACP agent advertised `loadSession` when it last started.
    pub load_session: bool,
    /// What changed about a loaded record's worktree since it was saved
    /// (gone, or on another branch). Relaunch, tests and archive refuse
    /// while it is set.
    pub worktree_note: Option<String>,
    /// `session/load` is replaying the saved conversation: updates go to
    /// the transcript only.
    replaying: bool,
    /// Hunks marked in the review, for integrating only them.
    pub marks: Vec<marks::Mark>,
    /// Parts of the work integrated on their own, oldest first.
    pub parts: Vec<marks::Part>,
    /// For a composed result: where its hunks came from.
    pub composition: Option<compose::Composition>,
    /// Files Maestro's verification runs created in the worktree, from
    /// the last run that could tell. See [`test_outputs`].
    pub test_outputs: Vec<TestOutput>,
}

impl Agent {
    fn new(id: AgentId, task: String, group: u64, provider: ProviderSpec) -> Self {
        Self {
            id,
            name: format!("{}-{}", provider.name, id.0),
            task,
            contract: None,
            group,
            provider,
            worktree: None,
            status: Status::Starting,
            runtime: Runtime::None,
            transcript: Vec::new(),
            permissions: Vec::new(),
            history: Vec::new(),
            created: SystemTime::now(),
            tests: Vec::new(),
            test_handle: None,
            test_cancel: Arc::default(),
            errors: Vec::new(),
            stderr: VecDeque::new(),
            review: ReviewData::default(),
            comments: Vec::new(),
            archived: false,
            exited: None,
            turn_running: false,
            git_busy: None,
            output: Arc::new(AtomicU64::new(0)),
            seen_output: 0,
            pending_input: None,
            spawned: Instant::now(),
            term_size: None,
            origin: Origin::New,
            session_id: None,
            load_session: false,
            worktree_note: None,
            replaying: false,
            marks: Vec::new(),
            parts: Vec::new(),
            composition: None,
            test_outputs: Vec::new(),
        }
    }

    /// The record is from an earlier session (or adopted, or found) and no
    /// process runs for it: the panel says "not running (from an earlier
    /// session)" next to its status.
    pub fn from_earlier_session(&self) -> bool {
        !matches!(self.origin, Origin::New | Origin::Composed) && !self.is_running()
    }

    /// `R` would start the agent again: it has a record, no process, a
    /// worktree that is still its own, and no decision yet. (The provider
    /// and the worktree on disk are checked when `R` is pressed.)
    pub fn can_relaunch(&self) -> bool {
        self.relaunch_refusal().is_none()
    }

    /// Why `R` refuses for this agent, judged from the record alone. `R`
    /// checks this first, and the panel's hints use it, so the hint line
    /// never offers a relaunch that this refuses.
    pub fn relaunch_refusal(&self) -> Option<String> {
        if self.origin == Origin::Found {
            Some("this worktree has no record; a adopts it, then R can start an agent in it".into())
        } else if self.origin == Origin::Composed {
            Some(format!(
                "{} is a composed result; no agent runs in it to start again",
                self.name
            ))
        } else if self.is_running() {
            Some(format!("{} is already running", self.name))
        } else if self.archived {
            Some("the agent's worktree was archived".into())
        } else if self.base_status().is_decided() {
            Some(format!(
                "{} is {}; it is not relaunched",
                self.name,
                self.base_status().label()
            ))
        } else if let Some(what) = self.git_busy {
            Some(format!(
                "{} is busy ({what}); wait for it to finish",
                self.name
            ))
        } else if self.tests_running() {
            Some("tests are running for this agent; wait for them to finish".into())
        } else if let Some(note) = &self.worktree_note {
            Some(format!("not relaunched: {note}"))
        } else if self.worktree.is_none() {
            Some("the agent has no worktree".into())
        } else {
            None
        }
    }

    /// The status to show: tests running take precedence over the agent's
    /// own state until the user has decided, except a permission request,
    /// which waits for the user whatever else runs.
    pub fn status(&self) -> Status {
        if self.tests_running()
            && !self.status.is_decided()
            && self.status != Status::AwaitingApproval
        {
            Status::RunningTests
        } else {
            self.status
        }
    }

    /// The word for the agent's state, as every view shows it (see
    /// [`state_word`]).
    pub fn word(&self) -> &'static str {
        state_word(self.status(), self.archived, self.origin)
    }

    /// The last verification run finished and did not pass: it exited
    /// with an error, was killed, or could not start (code `None`).
    pub fn tests_failed(&self) -> bool {
        self.latest_test()
            .and_then(|t| t.finished.as_ref())
            .is_some_and(|f| f.code != Some(0))
    }

    /// The agent's own status, without the test overlay.
    pub fn base_status(&self) -> Status {
        self.status
    }

    pub fn tests_running(&self) -> bool {
        self.tests.last().is_some_and(|t| t.finished.is_none())
    }

    pub fn kind(&self) -> ProviderKind {
        self.provider.kind
    }

    /// The process is still running.
    pub fn is_running(&self) -> bool {
        match &self.runtime {
            Runtime::None => false,
            Runtime::Acp(_) => self.exited.is_none(),
            Runtime::Terminal(t) => t.exit_status().is_none(),
        }
    }

    pub fn latest_test(&self) -> Option<&TestRun> {
        self.tests.last()
    }

    /// Files in the last review diff that the contract does not allow.
    pub fn out_of_scope(&self) -> Vec<(PathBuf, String)> {
        match &self.contract {
            Some(c) => out_of_scope(c, self.review.agent_files()),
            None => Vec::new(),
        }
    }

    /// The task as sent to the agent: the task text, then the contract.
    pub fn task_for_agent(&self) -> String {
        contract::task_with_contract(&self.task, self.contract.as_ref())
    }

    /// The rule ACP file writes through Maestro must pass: inside the
    /// contract's allowed paths and outside its forbidden ones. `None`
    /// without a contract that limits paths.
    pub fn client_rules(&self) -> maestro_agent::ClientRules {
        let Some(c) = self
            .contract
            .clone()
            .filter(|c| !c.allowed.is_empty() || !c.forbidden.is_empty())
        else {
            return maestro_agent::ClientRules::default();
        };
        maestro_agent::ClientRules {
            write: Some(Arc::new(move |p: &Path| match c.violation(p) {
                Some(why) => Err(format!("{why}: outside the task contract")),
                None => Ok(()),
            })),
        }
    }

    /// Records an event in the history.
    pub fn log(&mut self, text: impl Into<String>) {
        self.history.push(HistoryEntry {
            at: SystemTime::now(),
            text: text.into(),
        });
    }

    /// Changes the status unless the user already decided.
    pub(crate) fn set_status(&mut self, s: Status) {
        if !self.status.is_decided() {
            self.status = s;
        }
    }

    /// Changes the status even after a decision (only decisions do this).
    pub(crate) fn decide(&mut self, s: Status) {
        self.status = s;
    }

    /// Stops the agent's process and any test run. The record stays.
    pub(crate) fn stop(&mut self) {
        match &self.runtime {
            Runtime::Acp(c) => c.shutdown(),
            Runtime::Terminal(t) => {
                let _ = t.kill();
            }
            Runtime::None => {}
        }
        if let Some(h) = &self.test_handle {
            h.kill();
        }
        self.test_cancel.store(true, Ordering::SeqCst);
        self.pending_input = None;
    }

    fn push_transcript(&mut self, entry: TranscriptEntry) {
        use TranscriptEntry as T;
        match (self.transcript.last_mut(), entry) {
            (Some(T::Agent(a)), T::Agent(b)) => a.push_str(&b),
            (Some(T::Thought(a)), T::Thought(b)) => a.push_str(&b),
            (Some(T::User(a)), T::User(b)) => a.push_str(&b),
            (_, e) => self.transcript.push(e),
        }
        if self.transcript.len() > MAX_TRANSCRIPT {
            let extra = self.transcript.len() - MAX_TRANSCRIPT;
            self.transcript.drain(..extra);
        }
    }
}

/// The agents and the panel's state, kept on the editor.
#[derive(Default)]
pub struct AgentState {
    pub agents: Vec<Agent>,
    pub panel: panel::Panel,
    next_id: u64,
    next_group: u64,
    last_refresh: Option<Instant>,
    /// Saved records: which repositories were read, and what was written.
    pub persist: persist::PersistState,
    /// Which agents' changes overlap, and whether they would conflict.
    pub conflicts: conflicts::ConflictState,
}

impl AgentState {
    pub fn panel_open(&self) -> bool {
        self.panel.open
    }

    pub fn get(&self, id: AgentId) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn get_mut(&mut self, id: AgentId) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.id == id)
    }

    /// The agent selected in the panel.
    pub fn selected(&self) -> Option<&Agent> {
        self.agents.get(self.panel.selected)
    }

    /// Agents whose process or tests are still running.
    /// Agents with Git work in flight (creating, integrating, archiving),
    /// as `name (what)`. Quitting waits for them.
    pub fn git_busy(&self) -> Vec<String> {
        self.agents
            .iter()
            .filter_map(|a| a.git_busy.map(|w| format!("{} ({w})", a.name)))
            .collect()
    }

    pub fn running(&self) -> Vec<&Agent> {
        self.agents
            .iter()
            .filter(|a| a.is_running() || a.tests_running())
            .collect()
    }

    /// The agent whose worktree holds `path`, for the status line.
    pub fn owner_of(&self, path: &Path) -> Option<&Agent> {
        let live = || self.agents.iter().filter(|a| !a.archived);
        let within =
            |p: &Path, a: &&Agent| a.worktree.as_ref().is_some_and(|w| p.starts_with(&w.path));
        if self.agents.is_empty() {
            return None;
        }
        live().find(|a| within(path, a)).or_else(|| {
            // Worktree paths are canonical; the buffer's may not be.
            let canonical = std::fs::canonicalize(path).ok()?;
            live().find(|a| within(&canonical, a))
        })
    }

    fn fresh_id(&mut self) -> AgentId {
        self.next_id += 1;
        AgentId(self.next_id)
    }

    fn fresh_group(&mut self) -> u64 {
        self.next_group += 1;
        self.next_group
    }
}

impl Drop for AgentState {
    /// Stops every agent process and test run, whatever ends the editor
    /// (`:qa`, the automation API's quit, a test). A test run's handle does
    /// not stop it on drop, so this does.
    fn drop(&mut self) {
        for a in &mut self.agents {
            a.stop();
        }
        // Give terminal programs their SIGKILL before the process ends.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline
            && self.agents.iter().any(|a| {
                a.runtime
                    .terminal()
                    .is_some_and(|t| t.exit_status().is_none())
            })
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// The providers to choose from: enabled `[agents.provider.*]` entries,
/// then the built-in ones found on `PATH` that the config does not name.
/// A config entry with `enabled = false` hides the built-in of that name.
pub fn providers(ed: &Editor) -> Vec<ProviderSpec> {
    providers_with(&ed.config.agents, maestro_agent::builtin_providers())
}

/// [`providers`] with an explicit built-in list.
pub fn providers_with(
    config: &maestro_config::AgentsConfig,
    builtin: Vec<ProviderSpec>,
) -> Vec<ProviderSpec> {
    let mut out: Vec<ProviderSpec> = config
        .provider
        .iter()
        .filter(|(_, p)| p.enabled)
        .map(|(name, p)| ProviderSpec {
            name: name.clone(),
            kind: match p.kind {
                maestro_config::AgentProviderKind::Acp => ProviderKind::Acp,
                maestro_config::AgentProviderKind::Terminal => ProviderKind::Terminal,
            },
            program: crate::editor::expand_tilde(Path::new(&p.command))
                .to_string_lossy()
                .into_owned(),
            args: p.args.clone(),
            env: p.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        })
        .collect();
    out.extend(
        builtin
            .into_iter()
            .filter(|b| !config.provider.contains_key(&b.name)),
    );
    out
}

/// `acp` or `terminal`, for lists.
pub fn kind_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Acp => "acp",
        ProviderKind::Terminal => "terminal",
    }
}

fn agent_panel(ed: &mut Editor) {
    if ed.agents.panel.open {
        panel::close(ed);
    } else {
        panel::open(ed);
    }
}

/// Handles one ACP event for agent `id`, on the editor thread.
pub(crate) fn on_acp_event(ed: &mut Editor, id: AgentId, event: maestro_agent::AcpEvent) {
    use maestro_agent::AcpEvent as E;
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let mut refresh = false;
    match event {
        E::Initialized {
            agent_name,
            load_session,
        } => {
            a.load_session = load_session;
            a.log(match agent_name {
                Some(n) => format!("initialized: {n}"),
                None => "initialized".to_string(),
            });
            if a.replaying && !load_session {
                a.log("the agent does not advertise session loading; starting a new session");
            }
        }
        E::SessionStarted { session_id } => {
            if a.replaying {
                a.replaying = false;
                a.transcript.push(TranscriptEntry::System(format!(
                    "the saved session was not loaded; this is a new session ({session_id}) \
                     without the earlier conversation"
                )));
            }
            a.log(format!("session {session_id} started"));
            a.session_id = Some(session_id);
            if a.turn_running {
                a.set_status(Status::Working);
            } else if a.status == Status::Starting {
                a.set_status(Status::Idle);
            }
        }
        E::SessionLoaded { session_id } => {
            a.replaying = false;
            a.log(format!(
                "loaded the saved session {session_id}; the agent replayed its conversation"
            ));
            a.session_id = Some(session_id);
            if a.turn_running {
                a.set_status(Status::Working);
            } else if a.status == Status::Starting {
                a.set_status(Status::Idle);
            }
        }
        // The saved conversation coming back: transcript only, so the
        // history and status are not rewritten by old events.
        E::Update(entry) if a.replaying => a.push_transcript(entry),
        E::Update(entry) => {
            match &entry {
                TranscriptEntry::Plan(_) if a.turn_running && a.permissions.is_empty() => {
                    a.set_status(Status::Planning);
                    a.log("sent a plan");
                }
                TranscriptEntry::Tool {
                    kind, title, id: t, ..
                } => {
                    if a.turn_running
                        && a.permissions.is_empty()
                        && matches!(kind.as_str(), "edit" | "delete" | "move")
                    {
                        a.set_status(Status::Editing);
                    }
                    a.log(format!("tool {t} ({kind}): {title}"));
                }
                _ => {}
            }
            a.push_transcript(entry);
        }
        E::ToolUpdate {
            id: tool,
            status,
            title,
            locations,
        } => {
            for e in a.transcript.iter_mut().rev() {
                if let TranscriptEntry::Tool {
                    id: t,
                    status: s,
                    title: ti,
                    locations: l,
                    ..
                } = e
                    && *t == tool
                {
                    if let Some(v) = status.clone() {
                        *s = v;
                    }
                    if let Some(v) = title.clone() {
                        *ti = v;
                    }
                    if let Some(v) = locations.clone() {
                        *l = v;
                    }
                    break;
                }
            }
            if let Some(s) = status.filter(|_| !a.replaying) {
                a.log(format!("tool {tool}: {s}"));
            }
        }
        E::PermissionRequested(req) => {
            a.log(format!("asks permission: {}", req.tool_title));
            a.permissions.push(req);
            a.set_status(Status::AwaitingApproval);
        }
        E::TurnEnded { stop_reason } => {
            a.turn_running = false;
            a.permissions.clear();
            a.log(format!("turn ended: {stop_reason}"));
            a.set_status(match stop_reason.as_str() {
                "end_turn" => Status::ReadyForReview,
                "error" => Status::Failed,
                _ => Status::Blocked,
            });
            refresh = true;
        }
        E::FileWritten(path) => {
            let shown = a
                .worktree
                .as_ref()
                .and_then(|w| path.strip_prefix(&w.path).ok())
                .unwrap_or(&path)
                .display()
                .to_string();
            a.log(format!("wrote {shown}"));
            if a.turn_running && a.permissions.is_empty() {
                a.set_status(Status::Editing);
            }
            refresh = true;
        }
        E::Stderr(line) => {
            a.stderr.push_back(line);
            if a.stderr.len() > MAX_STDERR_LINES {
                a.stderr.pop_front();
            }
        }
        E::Exited(code) => {
            let desc = match code {
                Some(c) => format!("exited with code {c}"),
                None => "was stopped".to_string(),
            };
            a.log(format!("agent process {desc}"));
            let unfinished = a.turn_running || a.status == Status::Starting;
            a.exited = Some(desc);
            a.turn_running = false;
            a.replaying = false;
            a.permissions.clear();
            if unfinished {
                a.set_status(Status::Failed);
            } else if a.status == Status::Idle {
                a.set_status(Status::Stopped);
            }
            refresh = true;
        }
        E::Error(msg) => {
            a.log(format!("error: {msg}"));
            a.errors.push(msg);
        }
    }
    if refresh {
        review::request(ed, id);
    }
}

/// Notices terminal output, exits and size changes, and types a terminal
/// agent's task in once its program is up. Returns true when something
/// the panel shows changed. Called from [`Editor::poll_background`].
pub fn poll(ed: &mut Editor) -> bool {
    if ed.agents.agents.is_empty() {
        return false;
    }
    let size = panel::terminal_size(ed.area);
    let shown = ed
        .agents
        .panel
        .open
        .then(|| ed.agents.selected().map(|a| a.id))
        .flatten();
    let mut changed = false;
    let mut exited = Vec::new();
    for a in &mut ed.agents.agents {
        let Runtime::Terminal(t) = &a.runtime else {
            continue;
        };
        let t = t.clone();
        if a.term_size != Some(size) {
            let _ = t.resize(size);
            a.term_size = Some(size);
        }
        let n = a.output.load(Ordering::SeqCst);
        if n != a.seen_output {
            a.seen_output = n;
            changed |= shown == Some(a.id);
        }
        if a.pending_input.is_some() && (n > 0 || a.spawned.elapsed() > Duration::from_secs(1)) {
            let text = a.pending_input.take().unwrap_or_default();
            let mut bytes = maestro_term::encode_paste(&text, t.bracketed_paste());
            bytes.push(b'\r');
            match t.write(&bytes) {
                Ok(()) => a.log("typed the task into the terminal"),
                Err(e) => {
                    let msg = format!("could not type the task into the terminal: {e}");
                    a.log(msg.clone());
                    a.errors.push(msg);
                }
            }
        }
        if a.exited.is_none()
            && let Some(info) = t.exit_status()
        {
            a.log(format!("program {}", info.description));
            a.exited = Some(info.description.clone());
            a.set_status(if info.code == Some(0) {
                Status::ReadyForReview
            } else {
                Status::Failed
            });
            exited.push(a.id);
            changed = true;
        }
    }
    for id in exited {
        if ed.agents.panel.terminal_focus == Some(id) {
            ed.agents.panel.terminal_focus = None;
            ed.info("the agent's program exited; keys are back with the panel");
        }
        review::request(ed, id);
    }
    changed
}

/// Refreshes the review diff of running agents every few seconds while the
/// panel is open, so file counts follow terminal agents, whose edits send
/// no event. Called from [`Editor::tick`].
pub fn tick(ed: &mut Editor, now: Instant) {
    // Saved records follow changes within about a second; unchanged
    // records are not written.
    persist::save(ed, false);
    if !ed.agents.panel.open {
        return;
    }
    if ed
        .agents
        .last_refresh
        .is_some_and(|t| now.duration_since(t) < Duration::from_secs(3))
    {
        return;
    }
    ed.agents.last_refresh = Some(now);
    let ids: Vec<AgentId> = ed
        .agents
        .agents
        .iter()
        .filter(|a| a.is_running() && a.worktree.is_some() && !a.archived)
        .map(|a| a.id)
        .collect();
    for id in ids {
        review::request(ed, id);
    }
}

/// Stops every agent and test run, for quitting, and waits up to a second
/// for terminal programs to exit: their SIGKILL comes 200 ms after SIGHUP
/// on another thread, which would die with the editor's process.
pub fn stop_all(ed: &mut Editor) {
    for a in &mut ed.agents.agents {
        if a.is_running() || a.tests_running() {
            a.stop();
            a.log("stopped: the editor quit");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline
        && ed.agents.agents.iter().any(|a| {
            a.runtime
                .terminal()
                .is_some_and(|t| t.exit_status().is_none())
        })
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    persist::save(ed, true);
}

/// Why the worktree on disk is not the one the record names, if it is not:
/// checked from the worktree's `.git` file and `HEAD` without running Git,
/// so it can run on the editor thread before tests or a relaunch. The
/// worktree must be a linked worktree of `main_root`'s repository with the
/// recorded branch checked out. Only regular files are read: the agent
/// controls its worktree, and a FIFO there must not block the editor.
pub(crate) fn worktree_mismatch(w: &WorktreeInfo) -> Option<String> {
    fn read_small(p: &Path) -> Result<String, String> {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_file() && m.len() < 64 * 1024 => {
                std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
            }
            Ok(_) => Err(format!("{} is not a small regular file", p.display())),
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }
    let check = || -> Result<(), String> {
        let dot_git = read_small(&w.path.join(".git"))
            .map_err(|e| format!("{} is not a linked worktree ({e})", w.path.display()))?;
        let gitdir = dot_git
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .ok_or_else(|| format!("{} has an unexpected .git file", w.path.display()))?;
        let gitdir = w.path.join(gitdir);
        let head = read_small(&gitdir.join("HEAD"))?;
        let branch = head.trim().strip_prefix("ref: refs/heads/");
        if branch != Some(w.branch.as_str()) {
            return Err(format!(
                "the worktree {} is on {} now, not {}",
                w.path.display(),
                branch.map_or("a detached HEAD".to_string(), |b| format!("branch {b}")),
                w.branch
            ));
        }
        // The worktree's Git directory belongs to the main worktree's
        // repository (when that keeps its Git directory in `.git`).
        let main_git = w.main_root.join(".git");
        if main_git.is_dir() {
            let common = read_small(&gitdir.join("commondir"))?;
            let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
            if canon(&gitdir.join(common.trim())) != canon(&main_git) {
                return Err(format!(
                    "{} is not a worktree of the repository at {}",
                    w.path.display(),
                    w.main_root.display()
                ));
            }
        }
        Ok(())
    };
    check().err()
}

/// The Git work for this agent that runs in the background while its
/// worktree is being made.
pub const CREATING: &str = "creating its worktree";

/// Lowercase ASCII words joined by `-`, at most 24 characters; `task` when
/// nothing is left.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        if out.len() >= 24 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "task".into() } else { out }
}

/// A commit subject from the task: its first sentence when that fits in
/// 72 characters, else its first line cut at the last word that fits.
/// No ellipsis; the whole task goes in the commit body.
pub fn task_summary(task: &str) -> String {
    const MAX: usize = 72;
    let first = task
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if first.is_empty() {
        return "Agent task".to_string();
    }
    let sentence = first
        .find(". ")
        .map_or(first, |i| &first[..i])
        .trim_end_matches('.');
    if sentence.chars().count() <= MAX {
        return sentence.to_string();
    }
    let cut: String = first.chars().take(MAX + 1).collect();
    let cut = match cut.rfind(char::is_whitespace) {
        Some(i) if i > 0 => &cut[..i],
        _ => &cut[..cut.char_indices().nth(MAX).map_or(cut.len(), |(i, _)| i)],
    };
    cut.trim_end_matches([',', ';', ':', ' ']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_short_and_safe() {
        assert_eq!(
            slug("Fix the parser: handle CRLF!"),
            "fix-the-parser-handle-cr"
        );
        assert_eq!(slug("  ***  "), "task");
        assert_eq!(slug("ünïcode only"), "n-code-only");
    }

    #[test]
    fn config_providers_come_first_and_can_hide_builtins() {
        let mut cfg = maestro_config::AgentsConfig::default();
        let entry = |kind, enabled| maestro_config::AgentProviderConfig {
            kind,
            command: "x".into(),
            args: vec![],
            env: Default::default(),
            enabled,
        };
        cfg.provider.insert(
            "mine".into(),
            entry(maestro_config::AgentProviderKind::Terminal, true),
        );
        cfg.provider.insert(
            "gemini".into(),
            entry(maestro_config::AgentProviderKind::Acp, false),
        );
        let builtin = |name: &str| ProviderSpec {
            name: name.into(),
            kind: ProviderKind::Acp,
            program: name.into(),
            args: vec![],
            env: vec![],
        };
        let list = providers_with(&cfg, vec![builtin("gemini"), builtin("opencode")]);
        let names: Vec<_> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["mine", "opencode"]);
        assert_eq!(list[0].kind, ProviderKind::Terminal);
    }

    #[test]
    fn task_summary_takes_the_first_line() {
        assert_eq!(task_summary("\n  Add a flag  \nmore"), "Add a flag");
        assert_eq!(task_summary(""), "Agent task");
        assert_eq!(task_summary(&"x".repeat(100)).chars().count(), 72);
        assert_eq!(
            task_summary(
                "Add a function add(a, b) to calc.py that returns a + b, and an assertion \
                 for it in test_calc.py. Keep the change minimal."
            ),
            "Add a function add(a, b) to calc.py that returns a + b, and an assertion"
        );
        assert_eq!(
            task_summary("Fix the parser. Then add tests for it."),
            "Fix the parser"
        );
        assert!(!task_summary(&"word ".repeat(30)).contains('…'));
    }
}
