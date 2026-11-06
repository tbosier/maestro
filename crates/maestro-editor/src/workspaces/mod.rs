//! Projects, worktrees and agent attention.
//!
//! * The sidebar (`space s`, [`sidebar`]) lists every agent by what it
//!   needs from you, then the projects and their worktrees with branch,
//!   changed files and ahead/behind counts.
//! * The jump palette (`space j`, [`jump`]) is one fuzzy list of projects,
//!   worktrees, agents and open buffers.
//! * Choosing a worktree in either switches the editor to it ([`switch`]):
//!   the working directory moves, the current files and layout are saved as
//!   that worktree's session and the target's session is restored.
//! * When an agent starts needing you (a permission request, work ready
//!   for review, failed tests, a failure), the status line says so once
//!   and the front end rings the bell or sends an OSC 9 notification
//!   ([`check_attention`]).
//!
//! * The file tree (`space t`, [`tree`]) shows the current worktree's
//!   files right of the views, with the files that differ from the base
//!   branch marked.
//!
//! Git runs only in the background scan ([`scan`]); the sidebar and the
//! palette draw what the last scan found plus the live agent records.

pub mod jump;
pub mod scan;
pub mod sidebar;
pub mod switch;
pub mod tree;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use maestro_config::NotifyMethod;

pub use scan::{Project, RecordAgent, Upstream, WorktreeRow};

use crate::agents::{AgentId, Origin, Status};
use crate::editor::{Editor, Severity};

crate::commands! {
    toggle_sidebar => "Show and focus the projects and worktrees sidebar, focus it, or hide it",
    jump_palette => "Jump to a project, worktree, agent or open buffer",
    toggle_file_tree => "Show and focus the file tree with changes vs the base branch, focus it, or hide it",
}

/// Default key bindings, added to the `space` group.
pub const DEFAULT_KEYS: &[(&str, &str)] = &[
    ("space s", "toggle_sidebar"),
    ("space j", "jump_palette"),
    ("space t", "toggle_file_tree"),
];

/// Notifications kept for the front end before the oldest are dropped.
const MAX_NOTIFICATIONS: usize = 32;

/// Workspace state kept on the editor.
#[derive(Default)]
pub struct Workspaces {
    /// Scan in the background on the first tick, so the status line can
    /// name the project and worktree. Off by default so editors built in
    /// tests or for headless use run no Git on their own; the terminal
    /// front end turns it on. Opening the sidebar or the palette scans
    /// either way.
    pub enabled: bool,
    pub sidebar: sidebar::Sidebar,
    /// The file tree right of the views.
    pub tree: tree::FileTree,
    /// What the last scan found, in display order.
    pub projects: Vec<Project>,
    /// A scan has finished at least once.
    pub scanned: bool,
    /// Why the last scan failed, if it did.
    pub scan_error: Option<String>,
    pub(crate) scan: scan::ScanState,
    /// The last attention state seen per agent, so each change notifies
    /// once.
    seen: HashMap<String, Attention>,
    /// Notifications for the front end to deliver (bell or OSC 9). Taken
    /// with [`Workspaces::take_notifications`].
    pub notifications: Vec<Notification>,
    /// Sessions of worktrees left in this run, when sessions are not saved
    /// to disk (`--clean`, tests).
    pub(crate) memory_sessions: HashMap<PathBuf, crate::navigation::session::SessionFile>,
    /// The worktree last switched to.
    pub active_root: Option<PathBuf>,
}

impl Workspaces {
    /// The notifications waiting for the front end, oldest first.
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.notifications)
    }
}

/// A message the front end delivers outside the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub method: NotifyMethod,
    pub text: String,
}

/// Why an agent needs you.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeedsYou {
    /// An ACP agent asked for permission and waits for the answer.
    Permission,
    /// The agent finished and its work waits for review.
    Review,
    /// The last verification run did not pass: it exited with an error,
    /// was killed, or could not start.
    TestsFailed,
    /// The agent could not start, its turn failed, or it exited with an
    /// error.
    Failed,
    /// The ACP turn ended without finishing (refused, cancelled, out of
    /// turns): the agent waits for you to say more.
    Blocked,
}

impl NeedsYou {
    pub fn label(self) -> &'static str {
        match self {
            NeedsYou::Permission => "permission request",
            NeedsYou::Review => "ready for review",
            NeedsYou::TestsFailed => "tests failed",
            NeedsYou::Failed => "failed",
            NeedsYou::Blocked => "blocked",
        }
    }
}

/// The herdr-style grouping of agents: what needs you first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Attention {
    NeedsYou(NeedsYou),
    /// A process or a verification run is doing something.
    Working,
    /// No process runs and nothing waits for you: a stopped agent, or an
    /// ACP agent waiting for a prompt.
    Idle,
    /// Decided (integrated or rejected), archived, or finished with no
    /// changes.
    Done,
}

impl Attention {
    /// Sort order: needs you, working, idle, done.
    pub fn rank(self) -> u8 {
        match self {
            Attention::NeedsYou(_) => 0,
            Attention::Working => 1,
            Attention::Idle => 2,
            Attention::Done => 3,
        }
    }

    /// The mark drawn before the agent's name, so the group never relies
    /// on color alone.
    pub fn symbol(self) -> &'static str {
        match self {
            Attention::NeedsYou(_) => "!",
            Attention::Working => "~",
            Attention::Idle => "-",
            Attention::Done => "=",
        }
    }

    /// The group heading.
    pub fn group(self) -> &'static str {
        match self {
            Attention::NeedsYou(_) => "needs you",
            Attention::Working => "working",
            Attention::Idle => "idle",
            Attention::Done => "done",
        }
    }

    /// The word for this agent's state.
    pub fn word(self) -> &'static str {
        match self {
            Attention::NeedsYou(n) => n.label(),
            Attention::Working => "working",
            Attention::Idle => "idle",
            Attention::Done => "done",
        }
    }
}

/// The attention state for an agent's status. `tests_failed`: the last
/// verification run did not pass (nonzero exit, killed, or not started).
pub fn attention_of(status: Status, archived: bool, tests_failed: bool) -> Attention {
    if archived {
        return Attention::Done;
    }
    match status {
        Status::AwaitingApproval => Attention::NeedsYou(NeedsYou::Permission),
        Status::Failed => Attention::NeedsYou(NeedsYou::Failed),
        Status::Starting
        | Status::Working
        | Status::Planning
        | Status::Editing
        | Status::Running
        | Status::RunningTests => Attention::Working,
        Status::Rejected | Status::Integrated => Attention::Done,
        _ if tests_failed => Attention::NeedsYou(NeedsYou::TestsFailed),
        Status::ReadyForReview => Attention::NeedsYou(NeedsYou::Review),
        Status::Blocked => Attention::NeedsYou(NeedsYou::Blocked),
        Status::Completed => Attention::Done,
        Status::Idle | Status::Stopped => Attention::Idle,
    }
}

/// One agent as the sidebar and the palette show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    /// Stable across refreshes: `worktree:<path>` once the agent has a
    /// worktree (the same for its saved record and the loaded agent),
    /// `agent:<id>` before.
    pub key: String,
    pub name: String,
    pub task: String,
    pub status: Status,
    pub attention: Attention,
    pub worktree: Option<PathBuf>,
    /// The main worktree of the agent's repository.
    pub project: Option<PathBuf>,
    /// The editor's own record (`None` for saved records not loaded here).
    pub id: Option<AgentId>,
    /// Started in this session (its first state is a change worth telling).
    pub new: bool,
    /// From saved records, with no process in this editor.
    pub from_records: bool,
}

/// Every agent: this editor's, then saved records of scanned projects
/// that this editor has not loaded. Sorted by attention, then name.
pub fn agent_entries(ed: &Editor) -> Vec<AgentEntry> {
    let mut out: Vec<AgentEntry> = Vec::new();
    for a in &ed.agents.agents {
        // A verification run that finished with an error, was killed or
        // could not start (code `None`) did not pass.
        let failed = a
            .latest_test()
            .and_then(|t| t.finished.as_ref())
            .is_some_and(|f| f.code != Some(0));
        // A permission request waits even while a verification run shows
        // as the agent's status.
        let status = match a.base_status() {
            Status::AwaitingApproval => Status::AwaitingApproval,
            _ => a.status(),
        };
        out.push(AgentEntry {
            key: agent_key(a.worktree.as_ref().map(|w| w.path.as_path()), a.id.0),
            name: a.name.clone(),
            task: a.task.clone(),
            status,
            attention: attention_of(status, a.archived, failed),
            worktree: a.worktree.as_ref().map(|w| w.path.clone()),
            project: a.worktree.as_ref().map(|w| w.main_root.clone()),
            id: Some(a.id),
            new: a.origin == Origin::New,
            from_records: a.origin != Origin::New && !a.is_running(),
        });
    }
    for p in &ed.workspaces.projects {
        for r in &p.records {
            let loaded = out
                .iter()
                .any(|e| e.worktree.as_deref() == Some(r.worktree.as_path()));
            if loaded {
                continue;
            }
            out.push(AgentEntry {
                key: agent_key(Some(&r.worktree), r.id),
                name: r.name.clone(),
                task: r.task.clone(),
                status: r.status,
                attention: attention_of(r.status, r.archived, r.tests_failed),
                worktree: Some(r.worktree.clone()),
                project: Some(p.root.clone()),
                id: None,
                new: false,
                from_records: true,
            });
        }
    }
    out.sort_by(|a, b| {
        (a.attention.rank(), &a.name, &a.key).cmp(&(b.attention.rank(), &b.name, &b.key))
    });
    out
}

/// An agent's key: its worktree once it has one, so a saved record and the
/// same agent loaded into this editor keep one key; else its id.
fn agent_key(worktree: Option<&Path>, id: u64) -> String {
    match worktree {
        Some(w) => format!("worktree:{}", w.display()),
        None => format!("agent:{id}"),
    }
}

/// Compares every agent's attention state with the last one seen and
/// announces the agents that just started needing you: once in the status
/// line, and once to the front end unless `[notify] method = "off"`. An
/// agent seen for the first time is a baseline (no announcement) unless
/// it was started in this session. Returns true when it announced
/// something. Called after background work is applied and on tick.
pub fn check_attention(ed: &mut Editor) -> bool {
    if ed.agents.agents.is_empty() && ed.workspaces.projects.is_empty() {
        return false;
    }
    sidebar::pin_selection(ed);
    let entries = agent_entries(ed);
    let mut fresh: Vec<(String, NeedsYou)> = Vec::new();
    for e in &entries {
        let prev = match ed.workspaces.seen.get(&e.key) {
            Some(p) => Some(*p),
            None if e.new => Some(Attention::Working),
            None => None,
        };
        ed.workspaces.seen.insert(e.key.clone(), e.attention);
        if let (Some(prev), Attention::NeedsYou(why)) = (prev, e.attention)
            && prev != e.attention
        {
            fresh.push((e.name.clone(), why));
        }
    }
    if fresh.is_empty() {
        return false;
    }
    let text = fresh
        .iter()
        .map(|(name, why)| format!("{name} needs you: {}", why.label()))
        .collect::<Vec<_>>()
        .join("; ");
    ed.set_status(Severity::Warning, format!("{text} (space s lists agents)"));
    let method = ed.config.notify.method;
    if method != NotifyMethod::Off {
        let list = &mut ed.workspaces.notifications;
        list.push(Notification {
            method,
            text: format!("Maestro: {text}"),
        });
        if list.len() > MAX_NOTIFICATIONS {
            let extra = list.len() - MAX_NOTIFICATIONS;
            list.drain(..extra);
        }
    }
    true
}

/// Periodic work: the first scan when enabled, and a fresh scan every few
/// seconds while the sidebar is open. Called from [`Editor::tick`].
pub fn tick(ed: &mut Editor, now: Instant) {
    let ws = &ed.workspaces;
    let due = ws
        .scan
        .last
        .is_none_or(|t| now.saturating_duration_since(t) >= scan::REFRESH_EVERY);
    if (ws.enabled && !ws.scanned && ws.scan.last.is_none()) || (ws.sidebar.open && due) {
        scan::request(ed);
    }
    tree::tick(ed, now);
    check_attention(ed);
}

/// The worktree the working directory is in (see [`root_of`]).
pub fn current_root(ed: &Editor) -> PathBuf {
    root_of(ed, &ed.cwd)
}

/// The worktree whose session is open: the one last switched to, else the
/// worktree of the startup directory. `:cd` does not move it, so a switch
/// after `:cd` saves the files under the session they came from.
pub fn session_root(ed: &Editor) -> PathBuf {
    let dir = ed.navigation.session_cwd.as_deref().unwrap_or(&ed.cwd);
    root_of(ed, dir)
}

/// The worktree roots the editor knows: every scanned worktree, plus the
/// one last switched to (a scan may not list it yet).
fn known_roots(ed: &Editor) -> impl Iterator<Item = &Path> {
    ed.workspaces
        .projects
        .iter()
        .flat_map(|p| &p.worktrees)
        .map(|w| w.path.as_path())
        .chain(ed.workspaces.active_root.as_deref())
}

/// The deepest known worktree holding `dir`, else `dir` itself.
pub fn root_of(ed: &Editor, dir: &Path) -> PathBuf {
    let dir = lexical(dir);
    known_roots(ed)
        .filter(|w| dir.starts_with(w))
        .max_by_key(|w| w.as_os_str().len())
        .map_or(dir.clone(), Path::to_path_buf)
}

/// The worktree a file belongs to: the deepest known worktree (or the
/// session's root) that holds it. `None` for files outside all of them.
/// Paths are compared after removing `.` and `..`; a file reached through
/// a symlink belongs to the worktree its spelled path is in.
pub fn owner_of(ed: &Editor, path: &Path) -> Option<PathBuf> {
    let path = lexical(path);
    let session = session_root(ed);
    known_roots(ed)
        .chain(std::iter::once(session.as_path()))
        .filter(|w| path.starts_with(w))
        .max_by_key(|w| w.as_os_str().len())
        .map(Path::to_path_buf)
}

/// `path` belongs to the worktree at `root` (see [`owner_of`]).
pub fn owned_by(ed: &Editor, path: &Path, root: &Path) -> bool {
    owner_of(ed, path).as_deref() == Some(root)
}

/// `path` with `.` dropped and `..` taken off the part before it, without
/// touching the filesystem.
pub fn lexical(path: &Path) -> PathBuf {
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

/// The project and worktree names for a worktree path, from the last
/// scan: `("maestro", "main")` for a main worktree, the directory name for
/// the others.
pub fn names_for(ed: &Editor, root: &Path) -> Option<(String, String)> {
    for p in &ed.workspaces.projects {
        if let Some(w) = p.worktrees.iter().find(|w| w.path == root) {
            let wt = if w.is_main {
                "main".to_string()
            } else {
                w.name.clone()
            };
            return Some((p.name.clone(), wt));
        }
    }
    None
}

/// `project:worktree` for the status line, once a scan knows the current
/// worktree.
pub fn status_label(ed: &Editor) -> Option<String> {
    let (p, w) = names_for(ed, &current_root(ed))?;
    Some(format!("{p}:{w}"))
}

/// Number of modified buffers whose files belong to the worktree at
/// `root`.
pub fn unsaved_in(ed: &Editor, root: &Path) -> usize {
    ed.buffers
        .values()
        .filter(|b| b.doc.is_modified())
        .filter(|b| b.doc.path().is_some_and(|p| owned_by(ed, p, root)))
        .count()
}

fn toggle_sidebar(ed: &mut Editor) {
    let s = &mut ed.workspaces.sidebar;
    if !s.open {
        s.open = true;
        s.focused = true;
        if s.too_narrow {
            ed.set_status(
                Severity::Warning,
                "the window is too narrow to show the sidebar ([sidebar] width, or widen it)",
            );
        }
        // One column takes the keys at a time.
        tree::blur(ed);
        scan::request(ed);
        sidebar::pin_selection(ed);
    } else if !s.focused {
        s.focused = true;
        tree::blur(ed);
    } else {
        s.open = false;
        s.focused = false;
    }
}

fn toggle_file_tree(ed: &mut Editor) {
    tree::toggle(ed);
}

fn jump_palette(ed: &mut Editor) {
    jump::open(ed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_orders_what_needs_you_first() {
        use Status::*;
        let a = |s| attention_of(s, false, false);
        assert_eq!(
            a(AwaitingApproval),
            Attention::NeedsYou(NeedsYou::Permission)
        );
        assert_eq!(a(ReadyForReview), Attention::NeedsYou(NeedsYou::Review));
        assert_eq!(a(Failed), Attention::NeedsYou(NeedsYou::Failed));
        assert_eq!(a(Blocked), Attention::NeedsYou(NeedsYou::Blocked));
        assert_eq!(a(Running), Attention::Working);
        assert_eq!(a(RunningTests), Attention::Working);
        assert_eq!(a(Stopped), Attention::Idle);
        assert_eq!(a(Idle), Attention::Idle);
        assert_eq!(a(Integrated), Attention::Done);
        assert_eq!(a(Completed), Attention::Done);
        // Failed tests outrank a finished agent, never a running one or a
        // decision.
        assert_eq!(
            attention_of(ReadyForReview, false, true),
            Attention::NeedsYou(NeedsYou::TestsFailed)
        );
        assert_eq!(attention_of(Running, false, true), Attention::Working);
        assert_eq!(attention_of(Rejected, false, true), Attention::Done);
        assert_eq!(attention_of(ReadyForReview, true, false), Attention::Done);
        let mut ranks: Vec<u8> = [Integrated, Idle, Running, ReadyForReview]
            .into_iter()
            .map(|s| a(s).rank())
            .collect();
        ranks.sort();
        assert_eq!(ranks, [0, 1, 2, 3]);
    }
}
