//! Creating an agent: a worktree on a new branch, then the agent process.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use maestro_git::Git;

use super::{Agent, AgentId, ProviderKind, ProviderSpec, Runtime, Status, WorktreeInfo};
use crate::editor::Editor;

/// What a new agent starts from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAgent {
    pub task: String,
    pub provider: ProviderSpec,
    /// Join this group (a competing implementation of the same task).
    pub group: Option<u64>,
    /// Start from this commit instead of the repository's current `HEAD`.
    pub base: Option<String>,
    /// The task contract, sent with the task and checked by Maestro.
    pub contract: Option<super::contract::Contract>,
}

/// Creates the agent record and starts making its worktree in the
/// background. The agent launches once the worktree exists.
pub fn create(ed: &mut Editor, new: NewAgent) -> AgentId {
    let id = ed.agents.fresh_id();
    let group = match new.group {
        Some(g) => g,
        None => ed.agents.fresh_group(),
    };
    let mut agent = Agent::new(id, new.task.clone(), group, new.provider);
    if let Some(c) = &new.contract {
        agent.log(format!("task contract: {}", c.brief()));
    }
    agent.contract = new.contract;
    agent.log("creating a worktree");
    agent.git_busy = Some(super::CREATING);
    ed.agents.agents.push(agent);
    ed.agents.panel.selected = ed.agents.agents.len() - 1;

    let git = ed.git.git().clone();
    let cwd = ed.cwd.clone();
    let dir = ed.config.agents.worktree_dir.clone();
    let slug = super::slug(&new.task);
    let base = new.base;
    let spawned = ed.spawn_background("agent-worktree", move |tx| {
        let result = prepare(&git, &cwd, dir.as_deref(), id.0, &slug, base);
        tx.send(move |ed| on_worktree(ed, id, result));
    });
    if let Err(e) = spawned {
        fail(
            ed,
            id,
            format!("could not start creating the worktree: {e}"),
        );
    }
    id
}

fn fail(ed: &mut Editor, id: AgentId, msg: String) {
    if let Some(a) = ed.agents.get_mut(id) {
        a.git_busy = None;
        a.log(msg.clone());
        a.errors.push(msg.clone());
        a.set_status(Status::Failed);
    }
    ed.error(msg);
}

/// Finds the repository, picks a free branch and directory, and adds the
/// worktree. Runs on a background thread.
pub fn prepare(
    git: &Git,
    cwd: &Path,
    worktree_dir: Option<&str>,
    id: u64,
    slug: &str,
    base: Option<String>,
) -> Result<(WorktreeInfo, String), String> {
    let repo = match git.discover(cwd) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Err(format!(
                "{} is not in a Git repository; agents work in worktrees of one",
                cwd.display()
            ));
        }
        Err(e) => return Err(e.to_string()),
    };
    let worktrees = repo.worktrees().map_err(|e| e.to_string())?;
    let main_root = worktrees
        .iter()
        .find(|w| w.is_main)
        .map(|w| w.path.clone())
        .unwrap_or_else(|| repo.root().to_path_buf());
    let (base, from) = match base {
        Some(b) => {
            let from = short(&b).to_string();
            (b, from)
        }
        None => match task_base(&repo, &worktrees) {
            TaskBase::Main { branch, commit } => {
                let from = format!("{branch} at {}", short(&commit));
                (commit, from)
            }
            TaskBase::Here { commit, why } => {
                let commit = commit
                    .map_err(|e| format!("the repository has no commit to start from: {e}"))?;
                let from = format!("{} ({why})", short(&commit));
                (commit, from)
            }
        },
    };
    let dir = worktree_root(&main_root, worktree_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    for n in 0..100 {
        let name = if n == 0 {
            format!("{slug}-{id}")
        } else {
            format!("{slug}-{id}-{n}")
        };
        let branch = format!("maestro/{name}");
        let path = dir.join(&name);
        if path.exists() || repo.resolve(&format!("refs/heads/{branch}")).is_ok() {
            continue;
        }
        repo.add_worktree(&path, &branch, &base)
            .map_err(|e| format!("could not create the worktree: {e}"))?;
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        let info = WorktreeInfo {
            path,
            branch,
            base,
            main_root,
        };
        return Ok((info, from));
    }
    Err(format!(
        "no free branch name for maestro/{slug}-{id} in {}",
        dir.display()
    ))
}

/// The commit a new task starts from when no base is given.
enum TaskBase {
    /// The `HEAD` of the branch checked out in the repository's main
    /// worktree: the branch `I` integrates into.
    Main { branch: String, commit: String },
    /// The main worktree has no branch with a commit to start from, so the
    /// task starts from the `HEAD` of the worktree the editor is in, as
    /// before; `why` says why, and `commit` is what `HEAD` resolved to.
    Here {
        commit: Result<String, String>,
        why: String,
    },
}

/// Where a new task starts, from `git worktree list` and the worktree
/// `repo` the editor is in.
fn task_base(repo: &maestro_git::Repository, worktrees: &[maestro_git::Worktree]) -> TaskBase {
    let main = worktrees.iter().find(|w| w.is_main);
    let why = match main {
        Some(w) if w.bare => "the repository's main worktree is bare".to_string(),
        Some(w) if w.detached => "the main worktree has a detached HEAD".to_string(),
        Some(w) => match (&w.branch, &w.head) {
            (Some(branch), Some(commit)) => {
                return TaskBase::Main {
                    branch: branch.clone(),
                    commit: commit.clone(),
                };
            }
            (Some(branch), None) => format!("{branch} in the main worktree has no commits yet"),
            (None, _) => "the main worktree is not on a branch".to_string(),
        },
        None => "Git listed no main worktree".to_string(),
    };
    TaskBase::Here {
        commit: repo.resolve("HEAD").map_err(|e| e.to_string()),
        why,
    }
}

/// What the provider list says about where a new task starts: the commit
/// and branch, whether the editor is in another worktree, and a warning,
/// marked `!`, for uncommitted files the agent will not get. Runs Git, so
/// it runs on a background thread.
pub fn start_notes(git: &Git, cwd: &Path) -> Vec<String> {
    let repo = match git.discover(cwd) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return vec![format!(
                "! {} is not in a Git repository; agents work in worktrees of one",
                cwd.display()
            )];
        }
        Err(e) => return vec![format!("! could not find the repository: {e}")],
    };
    let worktrees = match repo.worktrees() {
        Ok(w) => w,
        Err(e) => return vec![format!("! could not list the worktrees: {e}")],
    };
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let here = canon(repo.root());
    let main_root = worktrees
        .iter()
        .find(|w| w.is_main)
        .map(|w| canon(&w.path))
        .unwrap_or_else(|| here.clone());
    let mut notes = Vec::new();
    // The worktree whose commit the agent starts from, and its name in the
    // warning below.
    let (source, name) = match task_base(&repo, &worktrees) {
        TaskBase::Main { branch, commit } => {
            notes.push(format!(
                "starts from {branch} at {} (the branch I integrates into)",
                short(&commit)
            ));
            if here != main_root {
                let label = repo
                    .head()
                    .map(|h| h.label())
                    .unwrap_or_else(|_| here.display().to_string());
                notes.push(format!(
                    "you are in {label}; the new agent starts from {branch}, not from here"
                ));
            }
            (main_root, branch)
        }
        TaskBase::Here {
            commit: Ok(commit),
            why,
        } => {
            notes.push(format!(
                "! {why}, so the agent starts from the commit checked out here, {}; \
                 I integrates only into the branch checked out in the main worktree",
                short(&commit)
            ));
            (here, "this worktree".to_string())
        }
        TaskBase::Here {
            commit: Err(e),
            why,
        } => {
            notes.push(format!(
                "! {why}, and there is no commit here to start from either ({e}); \
                 the agent will not start"
            ));
            return notes;
        }
    };
    let status = git
        .discover(&source)
        .map_err(|e| e.to_string())
        .and_then(|r| r.ok_or_else(|| "it is not a worktree".to_string()))
        .and_then(|r| r.status().map_err(|e| e.to_string()));
    match status {
        Ok(status) => {
            let dirty: Vec<PathBuf> = status
                .entries
                .iter()
                .filter(|e| e.state != maestro_git::EntryState::Ignored)
                .map(|e| e.path.clone())
                .collect();
            if !dirty.is_empty() {
                let (files, them) = if dirty.len() == 1 {
                    ("file", "it")
                } else {
                    ("files", "them")
                };
                notes.push(format!(
                    "! {name} has {} uncommitted {files} ({}): the agent will not see {them}; \
                     commit {them} first if it should",
                    dirty.len(),
                    super::actions::list_paths(&dirty)
                ));
            }
        }
        Err(e) => notes.push(format!(
            "! could not check {} for uncommitted files: {e}",
            source.display()
        )),
    }
    notes
}

/// `[agents] worktree-dir`, or `<repository>-worktrees` next to the main
/// worktree. A relative setting is taken from the main worktree.
pub fn worktree_root(main_root: &Path, setting: Option<&str>) -> PathBuf {
    match setting.filter(|s| !s.trim().is_empty()) {
        Some(s) => {
            let p = crate::editor::expand_tilde(Path::new(s));
            if p.is_absolute() {
                p
            } else {
                main_root.join(p)
            }
        }
        None => {
            let name = main_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repository".into());
            main_root
                .parent()
                .unwrap_or(main_root)
                .join(format!("{name}-worktrees"))
        }
    }
}

fn on_worktree(ed: &mut Editor, id: AgentId, result: Result<(WorktreeInfo, String), String>) {
    if let Some(a) = ed.agents.get_mut(id) {
        a.git_busy = None;
    }
    match result {
        Ok((info, from)) => {
            let Some(a) = ed.agents.get_mut(id) else {
                return;
            };
            a.log(format!(
                "worktree {} on branch {} from {from}",
                info.path.display(),
                info.branch,
            ));
            let main_root = info.main_root.clone();
            a.worktree = Some(info);
            // Normally read when the panel opened; after `:cd` into
            // another repository, read now so the next save keeps them.
            super::persist::ensure_loaded(ed, &main_root);
            let Some(a) = ed.agents.get_mut(id) else {
                return;
            };
            if a.base_status().is_decided() {
                a.log("rejected before it started; not launched");
                return;
            }
            launch(ed, id, Launch::New);
        }
        Err(e) => fail(ed, id, e),
    }
}

pub(crate) fn short(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

/// How an agent's process starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    /// A new agent: the task goes to it.
    New,
    /// An agent that ran before: an ACP agent gets its saved session back
    /// when it can, or a new one, and no prompt; a terminal agent gets its
    /// provider command without the task.
    Relaunch,
}

/// `R`: starts an agent that is not running again in its worktree. For an
/// ACP agent with a saved session that advertised `loadSession`, the
/// session is loaded; otherwise a new session starts, and the transcript
/// says so. Nothing is prompted: `f` sends a prompt. A terminal agent runs
/// its provider command with the arguments that held `{task}` left out,
/// and nothing is typed in.
pub fn relaunch(ed: &mut Editor, id: AgentId) {
    let providers = super::providers(ed);
    let Some(a) = ed.agents.get(id) else {
        return;
    };
    // What the record says first (the hint line uses the same check),
    // then the worktree on disk.
    let refusal = if let Some(why) = a.relaunch_refusal() {
        Some(why)
    } else {
        match &a.worktree {
            None => Some("the agent has no worktree".into()),
            Some(w) if !w.path.is_dir() => Some(format!(
                "not relaunched: the worktree {} is gone",
                w.path.display()
            )),
            // Checked again now: the worktree may have changed since the
            // records were loaded.
            Some(w) => super::worktree_mismatch(w).map(|why| format!("not relaunched: {why}")),
        }
    };
    if let Some(why) = refusal {
        ed.error(why);
        return;
    }
    let Some(spec) = providers
        .into_iter()
        .find(|p| p.name == a.provider.name && p.kind == a.provider.kind)
    else {
        ed.error(format!(
            "not relaunched: no {} provider named {} is configured now",
            super::kind_label(a.provider.kind),
            a.provider.name
        ));
        return;
    };
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    a.provider = spec;
    a.origin = super::Origin::New;
    a.runtime = Runtime::None;
    a.exited = None;
    a.turn_running = false;
    a.permissions.clear();
    a.pending_input = None;
    a.output = Arc::new(std::sync::atomic::AtomicU64::new(0));
    a.seen_output = 0;
    a.set_status(Status::Starting);
    launch(ed, id, Launch::Relaunch);
    if let Some(a) = ed.agents.get(id)
        && a.is_running()
    {
        let msg = match (a.kind(), a.replaying) {
            (ProviderKind::Terminal, _) => format!(
                "relaunched {} without its task: i gives it the keys to type what it should do",
                a.name
            ),
            (ProviderKind::Acp, true) => format!(
                "relaunched {}: loading its saved session; m sends it a message",
                a.name
            ),
            (ProviderKind::Acp, false) => format!(
                "relaunched {} in a new session without the earlier conversation; m sends it a message",
                a.name
            ),
        };
        ed.info(msg);
    }
}

/// Starts the agent's process in its worktree.
fn launch(ed: &mut Editor, id: AgentId, how: Launch) {
    let size = super::panel::terminal_size(ed.area);
    let tx = ed.idle_job_sender();
    let wake = ed.wake_handle();
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let Some(root) = a.worktree.as_ref().map(|w| w.path.clone()) else {
        return;
    };
    // File writes the agent asks Maestro to make must stay inside the
    // contract's paths, on a relaunch too.
    let rules = a.client_rules();
    let task = a.task_for_agent();
    let result = match a.provider.kind {
        ProviderKind::Acp if how == Launch::Relaunch => {
            let on_event = move |event| {
                tx.send(move |ed| super::on_acp_event(ed, id, event));
            };
            let started = match (&a.session_id, a.load_session) {
                (Some(session), true) => maestro_agent::AcpClient::load_session_with_rules(
                    &a.provider,
                    &root,
                    session,
                    rules,
                    on_event,
                )
                .map(|c| (c, Some(session.clone()))),
                _ => {
                    maestro_agent::AcpClient::start_with_rules(&a.provider, &root, rules, on_event)
                        .map(|c| (c, None))
                }
            };
            started.map(|(client, loading)| {
                let why_new = match (&a.session_id, a.load_session) {
                    (None, _) => "there is no saved session",
                    (Some(_), _) => "the agent did not advertise session loading when it last ran",
                };
                let note = match &loading {
                    Some(s) => format!("relaunched; loading the saved session {s}"),
                    None => format!("relaunched in a new session: {why_new}"),
                };
                a.replaying = loading.is_some();
                a.transcript
                    .push(maestro_agent::TranscriptEntry::System(note.clone()));
                a.log(format!("{note}; no prompt sent"));
                Runtime::Acp(client)
            })
        }
        ProviderKind::Acp => {
            let started = maestro_agent::AcpClient::start_with_rules(
                &a.provider,
                &root,
                rules,
                move |event| {
                    tx.send(move |ed| super::on_acp_event(ed, id, event));
                },
            );
            started.and_then(|client| {
                client.prompt(&task)?;
                a.turn_running = true;
                a.transcript
                    .push(maestro_agent::TranscriptEntry::User(task.clone()));
                let with = if a.contract.is_some() {
                    " with its contract"
                } else {
                    ""
                };
                a.log(format!(
                    "started {}; sent the task{with}",
                    a.provider.program
                ));
                Ok(Runtime::Acp(client))
            })
        }
        ProviderKind::Terminal => {
            let has_placeholder = a
                .provider
                .args
                .iter()
                .any(|x| x.contains(maestro_agent::TASK_PLACEHOLDER));
            let args = if how == Launch::Relaunch {
                // No task: arguments that would carry it are left out.
                a.provider
                    .args
                    .iter()
                    .filter(|x| !x.contains(maestro_agent::TASK_PLACEHOLDER))
                    .cloned()
                    .collect()
            } else {
                a.provider
                    .args
                    .iter()
                    .map(|x| x.replace(maestro_agent::TASK_PLACEHOLDER, &task))
                    .collect()
            };
            let spec = maestro_term::CommandSpec {
                program: a.provider.program.clone(),
                args,
                cwd: root,
                env: a.provider.env.clone(),
            };
            let counter = a.output.clone();
            let started = maestro_term::Terminal::spawn(&spec, size, move || {
                counter.fetch_add(1, Ordering::SeqCst);
                wake.wake();
            });
            started.map(|t| {
                a.term_size = Some(size);
                a.spawned = Instant::now();
                if how == Launch::Relaunch {
                    let left_out = if has_placeholder {
                        "; the arguments that held the task were left out"
                    } else {
                        ""
                    };
                    a.log(format!(
                        "relaunched {} in a terminal without the task{left_out}; the user types",
                        a.provider.program
                    ));
                } else if has_placeholder {
                    a.log(format!(
                        "started {} in a terminal with the task as an argument",
                        a.provider.program
                    ));
                } else {
                    a.pending_input = Some(task.clone());
                    a.log(format!(
                        "started {} in a terminal; the task will be typed in",
                        a.provider.program
                    ));
                }
                a.set_status(Status::Running);
                Runtime::Terminal(Arc::new(t))
            })
        }
    };
    match result {
        Ok(rt) => a.runtime = rt,
        Err(e) => {
            a.replaying = false;
            let msg = format!("could not start {}: {e}", a.provider.program);
            fail(ed, id, msg);
        }
    }
}
