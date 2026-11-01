//! The background scan behind the sidebar and the jump palette: which
//! projects there are, their worktrees, each worktree's branch, changed
//! files and ahead/behind counts, and the agents their saved records name.
//!
//! Projects come from `[projects] paths`, the working directory, the
//! directories of saved sessions (`<state>/sessions/*.toml`) and the
//! repositories of this editor's agents. A configured path that is missing
//! or not in a repository is listed with the reason; other candidates that
//! are not repositories are left out. Everything here runs `git` and reads
//! files, so it only runs on a background thread started by [`request`].
//! One scan runs at a time; a request while one runs queues one more.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use maestro_git::Git;

use crate::agents::Status;
use crate::agents::persist::{self, ReadOutcome};
use crate::editor::{Editor, expand_tilde, normalize_path};

/// How often the open sidebar rescans.
pub const REFRESH_EVERY: Duration = Duration::from_secs(5);

/// Session files larger than this are not read for their directory.
const MAX_SESSION_BYTES: u64 = 1 << 20;

/// A Git repository, keyed by its main worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The main worktree, canonical. For a configured path that is not a
    /// usable repository, the path as configured.
    pub root: PathBuf,
    /// The main worktree's directory name.
    pub name: String,
    /// The main worktree first, then the others in `git worktree list`
    /// order. Bare entries are left out.
    pub worktrees: Vec<WorktreeRow>,
    /// Agents in this repository's saved records.
    pub records: Vec<RecordAgent>,
    /// Why the project could not be listed (missing, not a repository,
    /// `git worktree list` failed).
    pub error: Option<String>,
    /// The saved agent records could not be used (see `agents::persist`).
    pub records_note: Option<String>,
}

/// One worktree of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRow {
    /// Canonical when the directory exists.
    pub path: PathBuf,
    /// The directory name.
    pub name: String,
    /// `None` when `HEAD` is detached or there are no commits yet.
    pub branch: Option<String>,
    /// The checked-out commit.
    pub head: Option<String>,
    pub is_main: bool,
    /// Its directory is gone (`git worktree prune` would drop it).
    pub missing: bool,
    pub locked: bool,
    /// Changed, staged, conflicted and untracked files (ignored files do
    /// not count). `None` when status could not be read.
    pub dirty: Option<usize>,
    /// Ahead and behind the upstream. `None` without an upstream, or when
    /// the upstream branch is gone.
    pub upstream: Option<Upstream>,
    /// Why status could not be read.
    pub error: Option<String>,
}

impl WorktreeRow {
    /// On a `maestro/` branch: a worktree made for an agent.
    pub fn is_agent_branch(&self) -> bool {
        self.branch
            .as_deref()
            .is_some_and(|b| b.starts_with("maestro/"))
    }

    /// The branch, or `(detached abc1234)`, or `(no commits)`.
    pub fn branch_label(&self) -> String {
        match (&self.branch, &self.head) {
            (Some(b), _) => b.clone(),
            (None, Some(h)) => format!("(detached {})", &h[..h.len().min(7)]),
            (None, None) => "(no commits)".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub name: String,
    pub ahead: u32,
    pub behind: u32,
}

/// An agent named by a project's saved records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordAgent {
    pub id: u64,
    pub name: String,
    pub task: String,
    /// The saved status; a status that needs a process (the agent was
    /// running when it was saved) reads as stopped, as when the agent
    /// panel loads the record.
    pub status: Status,
    pub archived: bool,
    /// The last saved verification run finished with a nonzero code.
    pub tests_failed: bool,
    pub worktree: PathBuf,
}

/// Whether a scan runs and when the last one started.
#[derive(Debug, Default)]
pub(crate) struct ScanState {
    running: bool,
    again: bool,
    pub(crate) last: Option<Instant>,
}

/// What a scan needs from the editor, copied so it can run elsewhere.
struct Input {
    git: Git,
    configured: Vec<PathBuf>,
    cwd: PathBuf,
    /// Projects found by earlier scans in this run, so the project you
    /// switched away from stays listed without a saved session.
    known: Vec<PathBuf>,
    agent_roots: Vec<PathBuf>,
    state: Option<PathBuf>,
    records: bool,
}

/// Starts a scan, or queues one if a scan is running.
pub fn request(ed: &mut Editor) {
    if ed.workspaces.scan.running {
        ed.workspaces.scan.again = true;
        return;
    }
    ed.workspaces.scan.running = true;
    ed.workspaces.scan.again = false;
    ed.workspaces.scan.last = Some(Instant::now());
    let input = Input {
        git: ed.git.git().clone(),
        configured: ed
            .config
            .projects
            .paths
            .iter()
            .map(|p| ed.resolve_path(&expand_tilde(Path::new(p))))
            .collect(),
        cwd: ed.cwd.clone(),
        known: ed
            .workspaces
            .projects
            .iter()
            .filter(|p| p.error.is_none())
            .map(|p| p.root.clone())
            .collect(),
        agent_roots: ed
            .agents
            .agents
            .iter()
            .filter_map(|a| a.worktree.as_ref().map(|w| w.main_root.clone()))
            .collect(),
        state: ed.navigation.state_dir.clone(),
        records: crate::agents::persist::enabled(ed).is_some(),
    };
    let spawned = ed.spawn_background("workspace-scan", move |tx| {
        let projects = run(&input);
        tx.send(move |ed| apply(ed, projects));
    });
    if let Err(e) = spawned {
        ed.workspaces.scan.running = false;
        ed.workspaces.scan_error = Some(format!("could not scan projects: {e}"));
    }
}

fn apply(ed: &mut Editor, projects: Vec<Project>) {
    ed.workspaces.scan.running = false;
    ed.workspaces.scan_error = None;
    ed.workspaces.projects = projects;
    let first = !ed.workspaces.scanned;
    ed.workspaces.scanned = true;
    if first {
        // The current project starts unfolded.
        let current = super::current_root(ed);
        let project = ed
            .workspaces
            .projects
            .iter()
            .find(|p| p.worktrees.iter().any(|w| w.path == current))
            .map(|p| p.root.clone());
        if let Some(root) = project {
            ed.workspaces.sidebar.expanded.insert(root);
        }
    }
    super::jump::scan_finished(ed);
    if std::mem::take(&mut ed.workspaces.scan.again) {
        request(ed);
    }
}

/// Runs on a background thread.
fn run(input: &Input) -> Vec<Project> {
    let mut configured: Vec<Project> = Vec::new();
    let mut others: Vec<Project> = Vec::new();
    let mut seen_dirs: Vec<PathBuf> = Vec::new();
    let mut candidates: Vec<(PathBuf, bool)> =
        input.configured.iter().map(|p| (p.clone(), true)).collect();
    candidates.push((input.cwd.clone(), false));
    candidates.extend(input.known.iter().map(|p| (p.clone(), false)));
    candidates.extend(input.agent_roots.iter().map(|p| (p.clone(), false)));
    if let Some(state) = &input.state {
        candidates.extend(session_dirs(state).into_iter().map(|p| (p, false)));
    }
    for (dir, is_configured) in candidates {
        let dir = normalize_path(&dir);
        if seen_dirs.contains(&dir) {
            continue;
        }
        seen_dirs.push(dir.clone());
        let failed = |why: String| Project {
            name: dir_name(&dir),
            root: dir.clone(),
            worktrees: Vec::new(),
            records: Vec::new(),
            error: Some(why),
            records_note: None,
        };
        if !dir.is_dir() {
            if is_configured {
                configured.push(failed(format!("{} is not a directory", dir.display())));
            }
            continue;
        }
        let repo = match input.git.discover(&dir) {
            Ok(Some(r)) => r,
            Ok(None) => {
                if is_configured {
                    configured.push(failed(format!(
                        "{} is not in a Git repository",
                        dir.display()
                    )));
                }
                continue;
            }
            Err(e) => {
                if is_configured {
                    configured.push(failed(e.to_string()));
                }
                continue;
            }
        };
        let list = match repo.worktrees() {
            Ok(l) => l,
            Err(e) => {
                if is_configured {
                    configured.push(failed(format!("git worktree list failed: {e}")));
                }
                continue;
            }
        };
        let main = list
            .iter()
            .find(|w| w.is_main)
            .map(|w| w.path.clone())
            .unwrap_or_else(|| repo.root().to_path_buf());
        let main = normalize_path(&main);
        let known = configured
            .iter()
            .chain(others.iter())
            .any(|p| p.root == main);
        if known {
            continue;
        }
        let worktrees = list
            .iter()
            .filter(|w| !w.bare)
            .map(|w| worktree_row(&input.git, w))
            .collect();
        let (records, records_note) = match (&input.state, input.records) {
            (Some(state), true) => read_records(state, &main),
            _ => (Vec::new(), None),
        };
        let project = Project {
            name: dir_name(&main),
            root: main,
            worktrees,
            records,
            error: None,
            records_note,
        };
        if is_configured {
            configured.push(project);
        } else {
            others.push(project);
        }
    }
    others.sort_by(|a, b| (&a.name, &a.root).cmp(&(&b.name, &b.root)));
    configured.extend(others);
    configured
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn worktree_row(git: &Git, w: &maestro_git::Worktree) -> WorktreeRow {
    let missing = w.prunable.is_some() || !w.path.is_dir();
    let path = if missing {
        w.path.clone()
    } else {
        normalize_path(&w.path)
    };
    let mut row = WorktreeRow {
        name: dir_name(&path),
        path,
        branch: w.branch.clone(),
        head: w.head.clone(),
        is_main: w.is_main,
        missing,
        locked: w.locked.is_some(),
        dirty: None,
        upstream: None,
        error: None,
    };
    if missing {
        return row;
    }
    let status = git
        .discover(&row.path)
        .and_then(|r| r.map(|r| r.status()).transpose());
    match status {
        Ok(Some(s)) => {
            row.dirty = Some(
                s.entries
                    .iter()
                    .filter(|e| e.state != maestro_git::EntryState::Ignored)
                    .count(),
            );
            if let Some(name) = s.branch.upstream.clone()
                && !s.branch.upstream_gone
            {
                row.upstream = Some(Upstream {
                    name,
                    ahead: s.branch.ahead,
                    behind: s.branch.behind,
                });
            }
        }
        Ok(None) => row.error = Some("not a working tree".into()),
        Err(e) => row.error = Some(format!("status failed: {e}")),
    }
    row
}

/// The directories saved sessions belong to. Unreadable or invalid files
/// are skipped.
fn session_dirs(state: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(state.join("sessions")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "toml") {
            continue;
        }
        // Only small regular files: a FIFO here must not block the scan.
        let regular = std::fs::symlink_metadata(&path)
            .is_ok_and(|m| m.is_file() && m.len() <= MAX_SESSION_BYTES);
        if !regular {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(s) = toml::from_str::<crate::navigation::session::SessionFile>(&text) {
            out.push(PathBuf::from(s.cwd));
        }
    }
    out.sort();
    out
}

fn read_records(state: &Path, main: &Path) -> (Vec<RecordAgent>, Option<String>) {
    match persist::read(&persist::records_path(state, main), main) {
        ReadOutcome::Missing => (Vec::new(), None),
        ReadOutcome::Unusable(why) => (Vec::new(), Some(why)),
        ReadOutcome::Records(file) => {
            let agents = file
                .agents
                .iter()
                .map(|r| {
                    let status = match Status::from_key(&r.status) {
                        Some(s) if !s.needs_process() => s,
                        _ => Status::Stopped,
                    };
                    RecordAgent {
                        id: r.id,
                        name: r.name.clone(),
                        task: r.task.clone(),
                        status,
                        archived: r.archived,
                        tests_failed: r
                            .tests
                            .last()
                            .is_some_and(|t| t.finished && matches!(t.code, Some(c) if c != 0)),
                        worktree: normalize_path(Path::new(&r.worktree.path)),
                    }
                })
                .collect();
            (agents, None)
        }
    }
}
