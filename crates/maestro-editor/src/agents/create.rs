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
    agent.log("creating a worktree");
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
) -> Result<WorktreeInfo, String> {
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
    let main_root = repo
        .worktrees()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|w| w.is_main)
        .map(|w| w.path)
        .unwrap_or_else(|| repo.root().to_path_buf());
    let base = match base {
        Some(b) => b,
        None => repo
            .resolve("HEAD")
            .map_err(|e| format!("the repository has no commit to start from: {e}"))?,
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
        return Ok(WorktreeInfo {
            path,
            branch,
            base,
            main_root,
        });
    }
    Err(format!(
        "no free branch name for maestro/{slug}-{id} in {}",
        dir.display()
    ))
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

fn on_worktree(ed: &mut Editor, id: AgentId, result: Result<WorktreeInfo, String>) {
    match result {
        Ok(info) => {
            let Some(a) = ed.agents.get_mut(id) else {
                return;
            };
            a.log(format!(
                "worktree {} on branch {} from {}",
                info.path.display(),
                info.branch,
                short(&info.base)
            ));
            a.worktree = Some(info);
            launch(ed, id);
        }
        Err(e) => fail(ed, id, e),
    }
}

pub(crate) fn short(commit: &str) -> &str {
    &commit[..commit.len().min(8)]
}

/// Starts the agent's process in its worktree.
fn launch(ed: &mut Editor, id: AgentId) {
    let size = super::panel::terminal_size(ed.area);
    let tx = ed.idle_job_sender();
    let wake = ed.wake_handle();
    let Some(a) = ed.agents.get_mut(id) else {
        return;
    };
    let Some(root) = a.worktree.as_ref().map(|w| w.path.clone()) else {
        return;
    };
    let result = match a.provider.kind {
        ProviderKind::Acp => {
            let started = maestro_agent::AcpClient::start(&a.provider, &root, move |event| {
                tx.send(move |ed| super::on_acp_event(ed, id, event));
            });
            started.and_then(|client| {
                client.prompt(&a.task)?;
                a.turn_running = true;
                a.transcript
                    .push(maestro_agent::TranscriptEntry::User(a.task.clone()));
                a.log(format!("started {}; sent the task", a.provider.program));
                Ok(Runtime::Acp(client))
            })
        }
        ProviderKind::Terminal => {
            let has_placeholder = a
                .provider
                .args
                .iter()
                .any(|x| x.contains(maestro_agent::TASK_PLACEHOLDER));
            let args = a
                .provider
                .args
                .iter()
                .map(|x| x.replace(maestro_agent::TASK_PLACEHOLDER, &a.task))
                .collect();
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
                if has_placeholder {
                    a.log(format!(
                        "started {} in a terminal with the task as an argument",
                        a.provider.program
                    ));
                } else {
                    a.pending_input = Some(a.task.clone());
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
            let msg = format!("could not start {}: {e}", a.provider.program);
            fail(ed, id, msg);
        }
    }
}
